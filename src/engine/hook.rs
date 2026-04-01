use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::storage::OutboundMessage;
use crate::tools::MessageSendCallback;

/// Lifecycle hooks for the agent loop. Implementations can observe and react to
/// iteration boundaries, streaming events, and tool execution phases without
/// modifying the core loop logic.
#[async_trait]
pub trait AgentHook: Send + Sync {
    /// Called before each LLM iteration begins.
    async fn before_iteration(&self, _iteration: usize) -> Result<()> {
        Ok(())
    }

    /// Whether this hook wants to receive streaming tokens.
    fn wants_streaming(&self) -> bool {
        false
    }

    /// Called for each streaming token delta.
    async fn on_stream(&self, _delta: &str) -> Result<()> {
        Ok(())
    }

    /// Called when a streaming segment ends.
    async fn on_stream_end(&self, _has_tool_calls: bool) -> Result<()> {
        Ok(())
    }

    /// Called before tools are executed in a given iteration.
    async fn before_execute_tools(&self, _tool_calls: &[Value]) -> Result<()> {
        Ok(())
    }

    /// Called after each iteration completes.
    async fn after_iteration(&self, _iteration: usize, _had_tool_calls: bool) -> Result<()> {
        Ok(())
    }

    /// Called to post-process the final assistant content before it is returned.
    async fn finalize_content(&self, content: Option<String>) -> Result<Option<String>> {
        Ok(content)
    }
}

/// Default hook that bridges streaming and progress to the existing callback system.
pub struct CallbackHook {
    channel: String,
    chat_id: String,
    session_key: String,
    metadata: BTreeMap<String, Value>,
    progress_sender: Option<MessageSendCallback>,
    stream_segment: std::sync::atomic::AtomicU64,
}

impl CallbackHook {
    pub fn new(
        channel: String,
        chat_id: String,
        session_key: String,
        metadata: BTreeMap<String, Value>,
        progress_sender: Option<MessageSendCallback>,
    ) -> Self {
        Self {
            channel,
            chat_id,
            session_key,
            metadata,
            progress_sender,
            stream_segment: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn build_stream_id(&self) -> String {
        let seg = self
            .stream_segment
            .load(std::sync::atomic::Ordering::Relaxed);
        format!(
            "{}:{}:{}",
            self.session_key,
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            seg
        )
    }
}

#[async_trait]
impl AgentHook for CallbackHook {
    fn wants_streaming(&self) -> bool {
        self.metadata
            .get("_wants_stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    async fn on_stream(&self, delta: &str) -> Result<()> {
        if let Some(sender) = &self.progress_sender {
            let mut meta = self.metadata.clone();
            meta.insert("_stream_delta".to_string(), Value::Bool(true));
            meta.insert(
                "_stream_id".to_string(),
                Value::String(self.build_stream_id()),
            );
            let msg = OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: delta.to_string(),
                reply_to: None,
                media: Vec::new(),
                metadata: meta,
            };
            sender(msg).await?;
        }
        Ok(())
    }

    async fn on_stream_end(&self, has_tool_calls: bool) -> Result<()> {
        if let Some(sender) = &self.progress_sender {
            let mut meta = self.metadata.clone();
            meta.insert("_stream_end".to_string(), Value::Bool(true));
            meta.insert(
                "_stream_id".to_string(),
                Value::String(self.build_stream_id()),
            );
            if has_tool_calls {
                meta.insert("_resuming".to_string(), Value::Bool(true));
            }
            let msg = OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: String::new(),
                reply_to: None,
                media: Vec::new(),
                metadata: meta,
            };
            sender(msg).await?;
        }
        self.stream_segment
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// No-op hook for contexts that don't need lifecycle callbacks.
pub struct NoOpHook;

#[async_trait]
impl AgentHook for NoOpHook {}
