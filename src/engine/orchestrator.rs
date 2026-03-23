use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::{ExecToolConfig, WebSearchConfig};
use crate::cron::CronService;
use crate::engine::{ContextBuilder, MemoryConsolidator, SubagentManager};
use crate::integrations::mcp::register_mcp_tools;
use crate::providers::{LlmResponse, SharedProvider, TextStreamCallback};
use crate::storage::{
    ChatMessage, InboundMessage, MessageBus, OutboundMessage, Session, SessionManager,
};
use crate::tools::{
    CronTool, EditFileTool, ExecTool, ListDirTool, MessageSendCallback, MessageTool, ReadFileTool,
    SpawnTool, ToolOutput, ToolRegistry, WebFetchTool, WebSearchTool, WriteFileTool,
};
use crate::util::build_status_content;

#[derive(Debug, Clone, Serialize)]
pub struct AgentSnapshot {
    pub model: String,
    pub workspace: String,
    pub uptime_seconds: u64,
    pub max_iterations: usize,
    pub context_window_tokens: usize,
    pub session_count: usize,
    pub running_subagents: usize,
    pub last_prompt_tokens: usize,
    pub last_completion_tokens: usize,
}

pub struct AgentLoop {
    provider: SharedProvider,
    workspace: PathBuf,
    model: String,
    max_iterations: usize,
    context_window_tokens: usize,
    context: ContextBuilder,
    sessions: Mutex<SessionManager>,
    tools: ToolRegistry,
    memory: MemoryConsolidator,
    subagents: SubagentManager,
    message_tool: Arc<MessageTool>,
    progress_sender: Arc<Mutex<Option<MessageSendCallback>>>,
    spawn_tool: Arc<SpawnTool>,
    cron_tool: Option<Arc<CronTool>>,
    start_time: Instant,
    last_usage: Mutex<(usize, usize)>,
}

impl AgentLoop {
    pub async fn new(
        provider: SharedProvider,
        workspace: impl AsRef<Path>,
        model: Option<String>,
        max_iterations: usize,
        context_window_tokens: usize,
        web_search: WebSearchConfig,
        web_proxy: Option<String>,
        exec: ExecToolConfig,
        restrict_to_workspace: bool,
        cron_service: Option<CronService>,
        mcp_servers: &BTreeMap<String, crate::config::McpServerConfig>,
    ) -> Result<Self> {
        let workspace = workspace.as_ref().to_path_buf();
        let context = ContextBuilder::new(&workspace)?;
        let sessions = SessionManager::new(&workspace)?;
        let memory = MemoryConsolidator::new(&workspace, context_window_tokens)?;
        let resolved_model = model
            .clone()
            .unwrap_or_else(|| provider.default_model().to_string());
        let subagents = SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            MessageBus::new(64),
            resolved_model.clone(),
            web_search.clone(),
            web_proxy.clone(),
            exec.clone(),
            restrict_to_workspace,
        );
        let mut tools = ToolRegistry::new();
        let allowed_dir = restrict_to_workspace.then(|| workspace.clone());
        tools.register(Arc::new(ReadFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
            vec![],
        )));
        tools.register(Arc::new(WriteFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(EditFileTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        tools.register(Arc::new(ListDirTool::new(
            Some(workspace.clone()),
            allowed_dir.clone(),
        )));
        if exec.enable {
            tools.register(Arc::new(ExecTool::new(
                exec.timeout,
                Some(workspace.clone()),
                restrict_to_workspace,
                exec.path_append.clone(),
            )));
        }
        tools.register(Arc::new(WebSearchTool::new(web_search, web_proxy.clone())));
        tools.register(Arc::new(WebFetchTool::new(50_000, web_proxy)));
        let message_tool = Arc::new(MessageTool::new(None));
        tools.register(message_tool.clone());
        let spawn_tool = Arc::new(SpawnTool::new(subagents.clone()));
        tools.register(spawn_tool.clone());
        let cron_tool = cron_service.map(|service| Arc::new(CronTool::new(service)));
        if let Some(cron_tool) = &cron_tool {
            tools.register(cron_tool.clone());
        }
        register_mcp_tools(&mut tools, mcp_servers).await?;

        Ok(Self {
            provider: provider.clone(),
            workspace,
            model: model.unwrap_or_else(|| provider.default_model().to_string()),
            max_iterations,
            context_window_tokens,
            context,
            sessions: Mutex::new(sessions),
            tools,
            memory,
            subagents,
            message_tool,
            progress_sender: Arc::new(Mutex::new(None)),
            spawn_tool,
            cron_tool,
            start_time: Instant::now(),
            last_usage: Mutex::new((0, 0)),
        })
    }

    pub fn set_message_sender(&self, callback: Option<MessageSendCallback>) {
        self.message_tool.set_send_callback(callback);
    }

    pub fn set_progress_sender(&self, callback: Option<MessageSendCallback>) {
        *self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned") = callback;
    }

    pub fn set_runtime_bus(&self, bus: MessageBus) {
        self.subagents.set_bus(bus);
    }

    pub async fn process_direct(
        &self,
        content: &str,
        session_key: &str,
        channel: &str,
        chat_id: &str,
    ) -> Result<Option<OutboundMessage>> {
        self.process_direct_stream(content, session_key, channel, chat_id, None)
            .await
    }

    pub async fn process_direct_stream(
        &self,
        content: &str,
        session_key: &str,
        channel: &str,
        chat_id: &str,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(
            InboundMessage {
                channel: channel.to_string(),
                sender_id: "user".to_string(),
                chat_id: chat_id.to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: Vec::new(),
                metadata: BTreeMap::new(),
                session_key_override: Some(session_key.to_string()),
            },
            text_stream,
        )
        .await
    }

    pub async fn process_inbound(&self, msg: InboundMessage) -> Result<Option<OutboundMessage>> {
        self.process_inbound_with_stream(msg, None).await
    }

    async fn process_inbound_with_stream(
        &self,
        msg: InboundMessage,
        text_stream: Option<TextStreamCallback>,
    ) -> Result<Option<OutboundMessage>> {
        if msg.channel == "system" {
            return self.process_system_inbound(msg).await;
        }
        if msg.content.trim() == "/stop" {
            let cancelled = self.subagents.cancel_by_session(&msg.session_key()).await;
            return Ok(Some(OutboundMessage {
                channel: msg.channel,
                chat_id: msg.chat_id,
                content: if cancelled == 0 {
                    "No active task to stop.".to_string()
                } else {
                    format!("Stopped {cancelled} task(s).")
                },
                reply_to: None,
                media: Vec::new(),
                metadata: BTreeMap::new(),
            }));
        }
        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&msg.session_key())?;

            match msg.content.trim() {
                "/new" => {
                    let snapshot = session.messages[session.last_consolidated..].to_vec();
                    session.clear();
                    sessions.save(&session)?;
                    if !snapshot.is_empty() {
                        self.memory.archive_messages(&snapshot)?;
                    }
                    return Ok(Some(OutboundMessage {
                        channel: msg.channel,
                        chat_id: msg.chat_id,
                        content: "New session started.".to_string(),
                        reply_to: None,
                        media: Vec::new(),
                        metadata: BTreeMap::new(),
                    }));
                }
                "/status" => {
                    return Ok(Some(self.status_response(&msg, &session)));
                }
                "/help" => {
                    return Ok(Some(OutboundMessage {
                        channel: msg.channel,
                        chat_id: msg.chat_id,
                        content: "/new\n/stop\n/restart\n/status\n/help".to_string(),
                        reply_to: None,
                        media: Vec::new(),
                        metadata: BTreeMap::new(),
                    }));
                }
                _ => {}
            }

            self.memory.maybe_consolidate_by_tokens(&mut session)?;
            sessions.put(session);
        }

        self.message_tool.set_context(
            &msg.channel,
            &msg.chat_id,
            msg.metadata
                .get("message_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        );
        self.message_tool.start_turn();
        self.spawn_tool.set_context(&msg.channel, &msg.chat_id);
        if let Some(cron_tool) = &self.cron_tool {
            cron_tool.set_context(&msg.channel, &msg.chat_id);
        }

        let history = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            sessions.get_or_create(&msg.session_key())?.get_history(0)
        };
        let initial_messages = self.context.build_messages(
            history,
            &msg.content,
            Some(&msg.media),
            Some(&msg.channel),
            Some(&msg.chat_id),
            "user",
        )?;
        let (final_content, all_messages) = match self
            .run_agent_loop(
                initial_messages.clone(),
                text_stream,
                Some(ProgressTarget::from_inbound(&msg)),
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.persist_session_messages(&msg.session_key(), &initial_messages)?;
                return Err(err);
            }
        };

        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(&msg.session_key())?;
        self.save_turn(&mut session, &all_messages)?;
        self.memory.maybe_consolidate_by_tokens(&mut session)?;
        sessions.save(&session)?;

        if self.message_tool.sent_in_turn() {
            return Ok(None);
        }
        Ok(Some(OutboundMessage {
            channel: msg.channel,
            chat_id: msg.chat_id,
            content: final_content.unwrap_or_else(|| {
                "I've completed processing but have no response to give.".to_string()
            }),
            reply_to: None,
            media: Vec::new(),
            metadata: msg.metadata,
        }))
    }

    async fn process_system_inbound(&self, msg: InboundMessage) -> Result<Option<OutboundMessage>> {
        let (channel, chat_id) = msg
            .chat_id
            .split_once(':')
            .map(|(channel, chat_id)| (channel.to_string(), chat_id.to_string()))
            .unwrap_or_else(|| ("cli".to_string(), msg.chat_id.clone()));
        let session_key = format!("{channel}:{chat_id}");

        {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            let mut session = sessions.get_or_create(&session_key)?;
            self.memory.maybe_consolidate_by_tokens(&mut session)?;
            sessions.put(session);
        }

        self.message_tool.set_context(&channel, &chat_id, None);
        self.message_tool.start_turn();
        let history = {
            let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
            sessions.get_or_create(&session_key)?.get_history(0)
        };
        let initial_messages = self.context.build_messages(
            history,
            &msg.content,
            None,
            Some(&channel),
            Some(&chat_id),
            if msg.sender_id == "subagent" {
                "assistant"
            } else {
                "user"
            },
        )?;
        let (final_content, all_messages) = match self
            .run_agent_loop(
                initial_messages.clone(),
                None,
                Some(ProgressTarget {
                    channel: channel.clone(),
                    chat_id: chat_id.clone(),
                    metadata: BTreeMap::new(),
                }),
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.persist_session_messages(&session_key, &initial_messages)?;
                return Err(err);
            }
        };

        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(&session_key)?;
        self.save_turn(&mut session, &all_messages)?;
        self.memory.maybe_consolidate_by_tokens(&mut session)?;
        sessions.save(&session)?;

        Ok(Some(OutboundMessage {
            channel,
            chat_id,
            content: final_content.unwrap_or_else(|| "Background task completed.".to_string()),
            reply_to: None,
            media: Vec::new(),
            metadata: BTreeMap::new(),
        }))
    }

    async fn run_agent_loop(
        &self,
        mut messages: Vec<ChatMessage>,
        text_stream: Option<TextStreamCallback>,
        progress_target: Option<ProgressTarget>,
    ) -> Result<(Option<String>, Vec<ChatMessage>)> {
        *self.last_usage.lock().expect("usage lock poisoned") = (0, 0);
        let mut final_content = None;
        let think_re = Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex");
        let mut last_tool_fingerprint: Option<String> = None;
        let mut repeated_tool_batches = 0_usize;
        let mut last_assistant_content: Option<String> = None;

        let mut iteration = 0_usize;
        loop {
            if self.max_iterations > 0 && iteration >= self.max_iterations {
                break;
            }
            iteration += 1;
            let defs = self.tools.definitions();
            let response = self
                .provider
                .chat_with_retry_stream(
                    &messages,
                    Some(&defs),
                    Some(&self.model),
                    None,
                    None,
                    text_stream.clone(),
                )
                .await?;
            self.record_usage(&response);

            if response.has_tool_calls() {
                let tool_call_fingerprint = serde_json::to_string(&response.tool_calls)
                    .unwrap_or_else(|_| format!("{:?}", response.tool_calls));
                if last_tool_fingerprint.as_deref() == Some(tool_call_fingerprint.as_str()) {
                    repeated_tool_batches += 1;
                } else {
                    repeated_tool_batches = 1;
                    last_tool_fingerprint = Some(tool_call_fingerprint);
                }
                let tool_calls = response
                    .tool_calls
                    .iter()
                    .map(|call| call.to_openai_tool_call())
                    .collect::<Vec<_>>();
                let assistant_content = response
                    .content
                    .clone()
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                if let Some(content) = &assistant_content {
                    last_assistant_content = Some(content.clone());
                }
                self.context.add_assistant_message(
                    &mut messages,
                    assistant_content,
                    Some(tool_calls),
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );
                for tool_call in response.tool_calls {
                    self.send_tool_hint(progress_target.as_ref(), &tool_call)
                        .await;
                    let output = self
                        .tools
                        .execute(&tool_call.name, tool_call.arguments)
                        .await;
                    self.context.add_tool_result(
                        &mut messages,
                        &tool_call.id,
                        &tool_call.name,
                        output.into_value(),
                    );
                }
                if repeated_tool_batches >= 3 {
                    final_content = Some(build_repeated_tool_loop_message(
                        repeated_tool_batches,
                        &messages,
                        last_assistant_content.as_deref(),
                    ));
                    break;
                }
            } else {
                let content = response
                    .content
                    .clone()
                    .map(|text| think_re.replace_all(&text, "").trim().to_string())
                    .filter(|text| !text.is_empty());
                if let Some(content) = &content {
                    last_assistant_content = Some(content.clone());
                }
                self.context.add_assistant_message(
                    &mut messages,
                    content.clone(),
                    None,
                    response.reasoning_content.clone(),
                    response.thinking_blocks.clone(),
                );
                final_content = content;
                break;
            }
        }

        if final_content.is_none() && self.max_iterations > 0 {
            final_content = Some(build_iteration_limit_message(
                self.max_iterations,
                &messages,
                last_assistant_content.as_deref(),
            ));
        }

        Ok((final_content, messages))
    }

    fn recent_tool_diagnostics(messages: &[ChatMessage]) -> Vec<String> {
        messages
            .iter()
            .rev()
            .filter(|message| message.role == "tool")
            .filter_map(|message| {
                let name = message.name.as_deref().unwrap_or("tool");
                let text = message.content_as_text()?;
                let text = text.trim();
                if text.is_empty() {
                    return None;
                }
                Some(format!("{name}: {}", truncate_for_diagnostic(text, 220)))
            })
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    async fn send_tool_hint(
        &self,
        target: Option<&ProgressTarget>,
        tool_call: &crate::providers::ToolCallRequest,
    ) {
        if matches!(tool_call.name.as_str(), "message" | "spawn") {
            return;
        }
        let Some(target) = target else {
            return;
        };
        let callback = self
            .progress_sender
            .lock()
            .expect("progress callback lock poisoned")
            .clone();
        let Some(callback) = callback else {
            return;
        };
        let mut metadata = target.metadata.clone();
        metadata.insert("_progress".to_string(), Value::Bool(true));
        metadata.insert("_tool_hint".to_string(), Value::Bool(true));
        let outbound = OutboundMessage {
            channel: target.channel.clone(),
            chat_id: target.chat_id.clone(),
            content: format_tool_hint(tool_call),
            reply_to: None,
            media: Vec::new(),
            metadata,
        };
        let _ = callback(outbound).await;
    }

    fn status_response(&self, msg: &InboundMessage, session: &Session) -> OutboundMessage {
        let (prompt_tokens, completion_tokens) =
            *self.last_usage.lock().expect("usage lock poisoned");
        let context_tokens = self.memory.estimate_session_prompt_tokens(session);
        OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: build_status_content(
                env!("CARGO_PKG_VERSION"),
                &self.model,
                self.start_time.elapsed().as_secs(),
                prompt_tokens,
                completion_tokens,
                self.context_window_tokens,
                session.get_history(0).len(),
                context_tokens,
            ),
            reply_to: None,
            media: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    fn record_usage(&self, response: &LlmResponse) {
        let mut usage = self.last_usage.lock().expect("usage lock poisoned");
        usage.0 = usage.0.max(response.usage.prompt_tokens);
        usage.1 += response.usage.completion_tokens;
    }

    fn save_turn(&self, session: &mut Session, messages: &[ChatMessage]) -> Result<()> {
        let skip = 1 + session.get_history(0).len();
        for message in messages.iter().skip(skip) {
            if message.role == "assistant"
                && message.content.is_none()
                && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
            {
                continue;
            }
            let mut stored = message.clone();
            if stored.timestamp.is_none() {
                stored.timestamp = Some(crate::util::now_iso());
            }
            let Some(stored) = sanitize_message_for_storage(stored) else {
                continue;
            };
            if let Some(Value::String(text)) = &stored.content {
                if text.trim().is_empty() {
                    continue;
                }
            }
            session.messages.push(stored);
        }
        session.updated_at = crate::util::now_iso();
        Ok(())
    }

    fn persist_session_messages(&self, session_key: &str, messages: &[ChatMessage]) -> Result<()> {
        let mut sessions = self.sessions.lock().expect("session manager lock poisoned");
        let mut session = sessions.get_or_create(session_key)?;
        self.save_turn(&mut session, messages)?;
        self.memory.maybe_consolidate_by_tokens(&mut session)?;
        sessions.save(&session)?;
        Ok(())
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn snapshot(&self) -> Result<AgentSnapshot> {
        let sessions = self.sessions.lock().expect("session manager lock poisoned");
        let session_count = sessions.list_session_summaries()?.len();
        let (last_prompt_tokens, last_completion_tokens) =
            *self.last_usage.lock().expect("usage lock poisoned");
        Ok(AgentSnapshot {
            model: self.model.clone(),
            workspace: self.workspace.display().to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            max_iterations: self.max_iterations,
            context_window_tokens: self.context_window_tokens,
            session_count,
            running_subagents: self.subagents.get_running_count(),
            last_prompt_tokens,
            last_completion_tokens,
        })
    }

    pub fn session_summaries(&self) -> Result<Vec<crate::storage::SessionSummary>> {
        let sessions = self.sessions.lock().expect("session manager lock poisoned");
        sessions.list_session_summaries()
    }

    pub fn tool_output_to_string(output: ToolOutput) -> Result<String> {
        match output {
            ToolOutput::Text(text) => Ok(text),
            ToolOutput::Blocks(blocks) => Ok(serde_json::to_string(&blocks)?),
        }
    }
}

#[derive(Clone)]
struct ProgressTarget {
    channel: String,
    chat_id: String,
    metadata: BTreeMap<String, Value>,
}

impl ProgressTarget {
    fn from_inbound(msg: &InboundMessage) -> Self {
        Self {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            metadata: msg.metadata.clone(),
        }
    }
}

fn format_tool_hint(tool_call: &crate::providers::ToolCallRequest) -> String {
    let preview = summarize_tool_arguments(&tool_call.arguments);
    if preview.is_empty() {
        tool_call.name.clone()
    } else {
        format!("{} · {}", tool_call.name, preview)
    }
}

fn summarize_tool_arguments(arguments: &Value) -> String {
    match arguments {
        Value::Object(map) => {
            let preferred_keys = [
                "path",
                "target_file",
                "file",
                "command",
                "cmd",
                "url",
                "query",
                "pattern",
                "task",
                "label",
            ];
            let mut parts = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for key in preferred_keys {
                let Some(value) = map.get(key) else {
                    continue;
                };
                seen.insert(key.to_string());
                let summary = summarize_tool_argument_value(value);
                if !summary.is_empty() {
                    parts.push(format!("{key}={summary}"));
                }
            }
            for (key, value) in map.iter() {
                if parts.len() >= 5 {
                    break;
                }
                if seen.contains(key) {
                    continue;
                }
                let summary = summarize_tool_argument_value(value);
                if !summary.is_empty() {
                    parts.push(format!("{key}={summary}"));
                }
            }
            if map.len() > parts.len() {
                parts.push("...".to_string());
            }
            if parts.is_empty() {
                String::new()
            } else {
                parts.join(" · ")
            }
        }
        Value::Null => String::new(),
        other => truncate_for_diagnostic(&other.to_string(), 72),
    }
}

fn summarize_tool_argument_value(value: &Value) -> String {
    match value {
        Value::String(text) => truncate_for_diagnostic(text, 64),
        Value::Array(items) => format!(
            "[{} item{}]",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ),
        Value::Object(_) => "{...}".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => String::new(),
    }
}

fn build_iteration_limit_message(
    max_iterations: usize,
    messages: &[ChatMessage],
    last_assistant_content: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "Stopped after reaching the tool-call limit ({max_iterations}) before the task completed."
    )];
    if let Some(content) = last_assistant_content.filter(|content| !content.trim().is_empty()) {
        lines.push(format!(
            "Last assistant intent: {}",
            truncate_for_diagnostic(content.trim(), 220)
        ));
    }
    let tool_diagnostics = AgentLoop::recent_tool_diagnostics(messages);
    if !tool_diagnostics.is_empty() {
        lines.push("Recent tool results:".to_string());
        lines.extend(tool_diagnostics.into_iter().map(|line| format!("- {line}")));
    }
    lines.push(
        "If this task legitimately needs more steps, increase `agents.defaults.maxToolIterations`."
            .to_string(),
    );
    lines.join("\n")
}

fn build_repeated_tool_loop_message(
    repeated_batches: usize,
    messages: &[ChatMessage],
    last_assistant_content: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "Stopped because the same tool-call pattern repeated {repeated_batches} times without reaching a final answer."
    )];
    if let Some(content) = last_assistant_content.filter(|content| !content.trim().is_empty()) {
        lines.push(format!(
            "Last assistant intent: {}",
            truncate_for_diagnostic(content.trim(), 220)
        ));
    }
    let tool_diagnostics = AgentLoop::recent_tool_diagnostics(messages);
    if !tool_diagnostics.is_empty() {
        lines.push("Recent tool results:".to_string());
        lines.extend(tool_diagnostics.into_iter().map(|line| format!("- {line}")));
    }
    lines.push(
        "The agent appears stuck. Adjust the task, inspect the tool errors above, or raise `agents.defaults.maxToolIterations` if the workflow is valid but long."
            .to_string(),
    );
    lines.join("\n")
}

fn truncate_for_diagnostic(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated = text.chars().take(max_chars).collect::<String>();
    format!("{}...", truncated.trim_end())
}

fn sanitize_message_for_storage(mut message: ChatMessage) -> Option<ChatMessage> {
    match &mut message.content {
        Some(Value::String(text)) => {
            if text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG) {
                if message.role == "user" {
                    *text = strip_runtime_context_text(text);
                } else {
                    return None;
                }
            }
        }
        Some(Value::Array(blocks)) => {
            if message.role == "user"
                && blocks
                    .first()
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG))
            {
                blocks.remove(0);
            }
            if blocks.is_empty() {
                return None;
            }
        }
        _ => {}
    }
    Some(message)
}

fn strip_runtime_context_text(text: &str) -> String {
    if !text.starts_with(ContextBuilder::RUNTIME_CONTEXT_TAG) {
        return text.to_string();
    }
    text.split_once("\n\n")
        .map(|(_, remainder)| remainder.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        build_iteration_limit_message, build_repeated_tool_loop_message,
        sanitize_message_for_storage, strip_runtime_context_text, truncate_for_diagnostic,
    };
    use crate::engine::ContextBuilder;
    use crate::storage::ChatMessage;
    use serde_json::json;

    #[test]
    fn iteration_limit_message_includes_recent_tool_output() {
        let messages = vec![
            ChatMessage::text("assistant", "Plan the edit"),
            ChatMessage {
                role: "tool".to_string(),
                content: Some(serde_json::Value::String(
                    "Error: file not found".to_string(),
                )),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                timestamp: None,
                reasoning_content: None,
                thinking_blocks: None,
                metadata: None,
            },
        ];

        let message = build_iteration_limit_message(64, &messages, Some("Inspect workspace"));
        assert!(message.contains("tool-call limit (64)"));
        assert!(message.contains("Last assistant intent: Inspect workspace"));
        assert!(message.contains("read_file: Error: file not found"));
    }

    #[test]
    fn repeated_tool_loop_message_mentions_stuck_state() {
        let messages = vec![ChatMessage {
            role: "tool".to_string(),
            content: Some(serde_json::Value::String("Permission denied".to_string())),
            tool_calls: None,
            tool_call_id: Some("call_2".to_string()),
            name: Some("exec".to_string()),
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        }];

        let message = build_repeated_tool_loop_message(3, &messages, None);
        assert!(message.contains("repeated 3 times"));
        assert!(message.contains("The agent appears stuck"));
        assert!(message.contains("exec: Permission denied"));
    }

    #[test]
    fn diagnostic_truncation_adds_ellipsis() {
        let text = "a".repeat(300);
        let truncated = truncate_for_diagnostic(&text, 32);
        assert_eq!(truncated.len(), 35);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn strips_runtime_context_from_persisted_user_text() {
        let message = ChatMessage::text(
            "user",
            format!(
                "{}\nCurrent Time: now\nChannel: cli\nChat ID: direct\n\ncontinue investigating",
                crate::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
            ),
        );
        let stored = sanitize_message_for_storage(message).expect("message should persist");
        assert_eq!(
            stored.content_as_text().as_deref(),
            Some("continue investigating")
        );
    }

    #[test]
    fn strips_runtime_context_block_from_persisted_user_media_messages() {
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "text",
                    "text": format!(
                        "{}\nCurrent Time: now\nChannel: cli\nChat ID: direct",
                        crate::engine::ContextBuilder::RUNTIME_CONTEXT_TAG
                    )
                },
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}},
                {"type": "text", "text": "look at this diagram"}
            ])),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        };
        let stored = sanitize_message_for_storage(message).expect("message should persist");
        assert_eq!(
            stored
                .content
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn strip_runtime_context_text_returns_empty_without_user_content() {
        let stripped = strip_runtime_context_text(ContextBuilder::RUNTIME_CONTEXT_TAG);
        assert!(stripped.is_empty());
    }
}
