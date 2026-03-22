use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::providers::{LlmResponse, SharedProvider};
use crate::storage::ChatMessage;
use crate::util::{current_time_str, workspace_state_dir};

type ExecuteCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>;
type NotifyCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;
type EvaluateCallback =
    Arc<dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> + Send + Sync>;

fn heartbeat_tool() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "heartbeat",
            "description": "Report heartbeat decision after reviewing tasks.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["skip", "run"]
                    },
                    "tasks": {
                        "type": "string"
                    }
                },
                "required": ["action"]
            }
        }
    })]
}

fn is_transient_error(response: &LlmResponse) -> bool {
    if response.finish_reason != "error" {
        return false;
    }
    let text = response
        .content
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "429",
        "rate limit",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "server error",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

#[derive(Clone)]
pub struct HeartbeatService {
    workspace: PathBuf,
    provider: SharedProvider,
    model: String,
    on_execute: Option<ExecuteCallback>,
    on_notify: Option<NotifyCallback>,
    evaluator: Option<EvaluateCallback>,
    interval_s: u64,
    enabled: bool,
    running: Arc<AtomicBool>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl HeartbeatService {
    pub fn new(
        workspace: impl AsRef<Path>,
        provider: SharedProvider,
        model: impl Into<String>,
        on_execute: Option<ExecuteCallback>,
        on_notify: Option<NotifyCallback>,
        evaluator: Option<EvaluateCallback>,
        interval_s: u64,
        enabled: bool,
    ) -> Self {
        Self {
            workspace: workspace.as_ref().to_path_buf(),
            provider,
            model: model.into(),
            on_execute,
            on_notify,
            evaluator,
            interval_s,
            enabled,
            running: Arc::new(AtomicBool::new(false)),
            task: Arc::new(Mutex::new(None)),
        }
    }

    pub fn heartbeat_file(&self) -> PathBuf {
        workspace_state_dir(&self.workspace).join("HEARTBEAT.md")
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn read_heartbeat_file(&self) -> Option<String> {
        let path = self.heartbeat_file();
        if !path.exists() {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        (!text.trim().is_empty()).then_some(text)
    }

    pub async fn decide(&self, content: &str) -> Result<(String, String)> {
        let tools = heartbeat_tool();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let response = self
                .provider
                .chat(
                    &[
                        ChatMessage::text(
                            "system",
                            "You are a heartbeat agent. Call the heartbeat tool to report your decision.",
                        ),
                        ChatMessage::text(
                            "user",
                            format!(
                                "Current Time: {}\n\nReview the following HEARTBEAT.md and decide whether there are active tasks.\n\n{}",
                                current_time_str(),
                                content
                            ),
                        ),
                    ],
                    Some(&tools),
                    Some(&self.model),
                    None,
                    None,
                )
                .await?;
            if is_transient_error(&response) && attempts < 2 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            if !response.has_tool_calls() {
                return Ok(("skip".to_string(), String::new()));
            }
            let args = response.tool_calls[0].arguments.clone();
            return Ok((
                args.get("action")
                    .and_then(|value| value.as_str())
                    .unwrap_or("skip")
                    .to_string(),
                args.get("tasks")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ));
        }
    }

    pub async fn start(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let this = self.clone();
        let handle = tokio::spawn(async move {
            while this.running.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_secs(this.interval_s)).await;
                if !this.running.load(Ordering::SeqCst) {
                    break;
                }
                let _ = this.tick().await;
            }
        });
        *self.task.lock().await = Some(handle);
        Ok(())
    }

    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().await.take() {
            handle.abort();
        }
    }

    pub async fn tick(&self) -> Result<()> {
        let Some(content) = self.read_heartbeat_file() else {
            return Ok(());
        };
        let (action, tasks) = self.decide(&content).await?;
        if action != "run" {
            return Ok(());
        }
        let Some(on_execute) = &self.on_execute else {
            return Ok(());
        };
        let response = on_execute(tasks.clone()).await?;
        let should_notify = if let Some(evaluator) = &self.evaluator {
            evaluator(response.clone(), tasks.clone()).await?
        } else {
            !response.trim().is_empty()
        };
        if should_notify {
            if let Some(on_notify) = &self.on_notify {
                on_notify(response).await?;
            }
        }
        Ok(())
    }

    pub async fn trigger_now(&self) -> Result<Option<String>> {
        let Some(content) = self.read_heartbeat_file() else {
            return Ok(None);
        };
        let (action, tasks) = self.decide(&content).await?;
        if action != "run" {
            return Ok(None);
        }
        let Some(on_execute) = &self.on_execute else {
            return Ok(None);
        };
        Ok(Some(on_execute(tasks).await?))
    }
}
