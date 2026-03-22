use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use regex::Regex;

use crate::config::{ExecToolConfig, WebSearchConfig};
use crate::engine::{ContextBuilder, SkillsLoader};
use crate::providers::SharedProvider;
use crate::storage::{ChatMessage, InboundMessage, MessageBus};
use crate::tools::{
    EditFileTool, ExecTool, ListDirTool, ReadFileTool, ToolRegistry, WebFetchTool, WebSearchTool,
    WriteFileTool,
};

#[derive(Clone)]
pub struct SubagentManager {
    provider: SharedProvider,
    workspace: PathBuf,
    bus: Arc<Mutex<MessageBus>>,
    model: String,
    web_search_config: WebSearchConfig,
    web_proxy: Option<String>,
    exec_config: ExecToolConfig,
    restrict_to_workspace: bool,
    running_tasks: Arc<Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>>,
    session_tasks: Arc<Mutex<BTreeMap<String, HashSet<String>>>>,
}

impl SubagentManager {
    pub fn new(
        provider: SharedProvider,
        workspace: PathBuf,
        bus: MessageBus,
        model: String,
        web_search_config: WebSearchConfig,
        web_proxy: Option<String>,
        exec_config: ExecToolConfig,
        restrict_to_workspace: bool,
    ) -> Self {
        Self {
            provider,
            workspace,
            bus: Arc::new(Mutex::new(bus)),
            model,
            web_search_config,
            web_proxy,
            exec_config,
            restrict_to_workspace,
            running_tasks: Arc::new(Mutex::new(BTreeMap::new())),
            session_tasks: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub async fn spawn(
        &self,
        task: String,
        label: Option<String>,
        origin_channel: String,
        origin_chat_id: String,
        session_key: Option<String>,
    ) -> String {
        let task_id = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();
        let display_label = label.unwrap_or_else(|| {
            let trimmed = task.chars().take(30).collect::<String>();
            if task.chars().count() > 30 {
                format!("{trimmed}...")
            } else {
                trimmed
            }
        });
        let manager = self.clone();
        let task_id_for_spawn = task_id.clone();
        let display_label_for_spawn = display_label.clone();
        let session_key_for_cleanup = session_key.clone();
        let handle = tokio::spawn(async move {
            let _ = manager
                .run_subagent(
                    task_id_for_spawn.clone(),
                    task,
                    display_label_for_spawn,
                    origin_channel,
                    origin_chat_id,
                )
                .await;
            manager.cleanup_task(&task_id_for_spawn, session_key_for_cleanup.as_deref());
        });
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .insert(task_id.clone(), handle);
        if let Some(session_key) = session_key {
            self.session_tasks
                .lock()
                .expect("subagent session lock poisoned")
                .entry(session_key)
                .or_default()
                .insert(task_id.clone());
        }
        format!(
            "Subagent [{display_label}] started (id: {task_id}). I'll notify you when it completes."
        )
    }

    pub async fn cancel_by_session(&self, session_key: &str) -> usize {
        let task_ids = self
            .session_tasks
            .lock()
            .expect("subagent session lock poisoned")
            .get(session_key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for task_id in task_ids {
            if let Some(handle) = self
                .running_tasks
                .lock()
                .expect("subagent running lock poisoned")
                .remove(&task_id)
            {
                handle.abort();
                cancelled += 1;
            }
        }
        self.session_tasks
            .lock()
            .expect("subagent session lock poisoned")
            .remove(session_key);
        cancelled
    }

    pub fn get_running_count(&self) -> usize {
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .len()
    }

    pub fn set_bus(&self, bus: MessageBus) {
        *self.bus.lock().expect("subagent bus lock poisoned") = bus;
    }

    async fn run_subagent(
        &self,
        task_id: String,
        task: String,
        label: String,
        origin_channel: String,
        origin_chat_id: String,
    ) -> Result<()> {
        let tools = self.build_tools();
        let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
        let mut messages = vec![
            ChatMessage::text("system", self.build_subagent_prompt()),
            ChatMessage::text("user", task.clone()),
        ];

        let mut final_result = None;
        for _ in 0..15 {
            let response = self
                .provider
                .chat_with_retry(
                    &messages,
                    Some(&tools.definitions()),
                    Some(&self.model),
                    None,
                    None,
                )
                .await?;
            if response.has_tool_calls() {
                let tool_calls = response
                    .tool_calls
                    .iter()
                    .map(|call| call.to_openai_tool_call())
                    .collect::<Vec<_>>();
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response.content.clone().map(serde_json::Value::String),
                    tool_calls: Some(tool_calls),
                    tool_call_id: None,
                    name: None,
                    timestamp: None,
                    reasoning_content: response.reasoning_content.clone(),
                    thinking_blocks: response.thinking_blocks.clone(),
                    metadata: None,
                });
                for tool_call in response.tool_calls {
                    let output = tools.execute(&tool_call.name, tool_call.arguments).await;
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(output.into_value()),
                        tool_calls: None,
                        tool_call_id: Some(tool_call.id),
                        name: Some(tool_call.name),
                        timestamp: None,
                        reasoning_content: None,
                        thinking_blocks: None,
                        metadata: None,
                    });
                }
            } else {
                final_result = response
                    .content
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                break;
            }
        }

        let result = final_result
            .unwrap_or_else(|| "Task completed but no final response was generated.".to_string());
        let bus = self.bus.lock().expect("subagent bus lock poisoned").clone();
        bus
            .publish_inbound(InboundMessage {
                channel: "system".to_string(),
                sender_id: "subagent".to_string(),
                chat_id: format!("{origin_channel}:{origin_chat_id}"),
                content: format!(
                    "[Subagent '{label}' completed]\n\nTask: {task}\n\nResult:\n{result}\n\nSummarize this naturally for the user. Keep it brief and do not mention technical implementation details."
                ),
                timestamp: chrono::Utc::now(),
                media: Vec::new(),
                metadata: BTreeMap::from([(
                    "task_id".to_string(),
                    serde_json::Value::String(task_id),
                )]),
                session_key_override: None,
            })
            .await?;
        Ok(())
    }

    fn build_tools(&self) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        let allowed_dir = self.restrict_to_workspace.then(|| self.workspace.clone());
        tools.register(Arc::new(ReadFileTool::new(
            Some(self.workspace.clone()),
            allowed_dir.clone(),
            vec![],
        )));
        tools.register(Arc::new(WriteFileTool::new(
            Some(self.workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(EditFileTool::new(
            Some(self.workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(ListDirTool::new(
            Some(self.workspace.clone()),
            allowed_dir.clone(),
        )));
        if self.exec_config.enable {
            tools.register(Arc::new(ExecTool::new(
                self.exec_config.timeout,
                Some(self.workspace.clone()),
                self.restrict_to_workspace,
                self.exec_config.path_append.clone(),
            )));
        }
        tools.register(Arc::new(WebSearchTool::new(
            self.web_search_config.clone(),
            self.web_proxy.clone(),
        )));
        tools.register(Arc::new(WebFetchTool::new(50_000, self.web_proxy.clone())));
        tools
    }

    fn build_subagent_prompt(&self) -> String {
        let runtime_ctx = ContextBuilder::RUNTIME_CONTEXT_TAG;
        let skills_summary = SkillsLoader::new(&self.workspace, None).build_skills_summary();
        if skills_summary.is_empty() {
            format!(
                "# Subagent\n\n{runtime_ctx}\n\nYou are a focused background subagent. Stay on the assigned task and return a concise final result.\n\n## Workspace\n{}",
                self.workspace.display()
            )
        } else {
            format!(
                "# Subagent\n\n{runtime_ctx}\n\nYou are a focused background subagent. Stay on the assigned task and return a concise final result.\n\n## Workspace\n{}\n\n## Skills\n{}\n",
                self.workspace.display(),
                skills_summary
            )
        }
    }

    fn cleanup_task(&self, task_id: &str, session_key: Option<&str>) {
        self.running_tasks
            .lock()
            .expect("subagent running lock poisoned")
            .remove(task_id);
        if let Some(session_key) = session_key {
            let mut session_tasks = self
                .session_tasks
                .lock()
                .expect("subagent session lock poisoned");
            if let Some(ids) = session_tasks.get_mut(session_key) {
                ids.remove(task_id);
                if ids.is_empty() {
                    session_tasks.remove(session_key);
                }
            }
        }
    }
}
