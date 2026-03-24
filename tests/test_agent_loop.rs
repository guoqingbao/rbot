use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rbot::config::ExecToolConfig;
use rbot::engine::AgentLoop;
use rbot::providers::{LlmProvider, LlmResponse, LlmUsage, QueuedProvider, ToolCallRequest};
use rbot::storage::{ChatMessage, SessionManager};
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn agent_loop_executes_tool_then_returns_final_answer() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hello from rust").unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Inspecting file".to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": file.display().to_string()}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("File inspection complete.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        8,
        8_000,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();
    let response = agent
        .process_direct("please inspect note.txt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "File inspection complete.");
    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert!(
        session
            .messages
            .iter()
            .any(|message| message.role == "tool" && message.name.as_deref() == Some("read_file"))
    );
}

#[tokio::test]
async fn zero_max_tool_iterations_means_unbounded_until_completion() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hello from rust").unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some("Inspecting file".to_string()),
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": file.display().to_string()}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some("File inspection complete.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
        ],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        0,
        8_000,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();
    let response = agent
        .process_direct("please inspect note.txt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.content, "File inspection complete.");
}

struct AlwaysFailProvider;

#[async_trait]
impl LlmProvider for AlwaysFailProvider {
    fn default_model(&self) -> &str {
        "test-model"
    }

    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Value]>,
        _model: Option<&str>,
        _max_tokens: Option<usize>,
        _temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        Err(anyhow!("error decoding response body"))
    }
}

#[tokio::test]
async fn provider_errors_still_persist_the_user_turn() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(AlwaysFailProvider),
        dir.path(),
        Some("test-model".to_string()),
        0,
        8_000,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();

    let error = agent
        .process_direct("continue investigating", "cli:direct", "cli", "direct")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("error decoding response body"));

    let mut sessions = SessionManager::new(dir.path()).unwrap();
    let session = sessions.get_or_create("cli:direct").unwrap();
    assert!(session.messages.iter().any(|message| {
        message.role == "user"
            && message
                .content_as_text()
                .as_deref()
                .is_some_and(|text| text.contains("continue investigating"))
    }));
}

#[tokio::test]
async fn agent_loop_stops_on_repeated_tool_calls() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".to_string(),
                    name: "list_dir".to_string(),
                    arguments: json!({"path": "."}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            }; 4 // Repeat 4 times
        ],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        10,
        8_000,
        Default::default(),
        None,
        ExecToolConfig {
            enable: false,
            timeout: 60,
            path_append: String::new(),
        },
        false,
        None,
        &Default::default(),
    )
    .await
    .unwrap();
    let response = agent
        .process_direct("list files", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("pattern repeated"));
}

struct SlowQueuedProvider {
    inner: QueuedProvider,
    delay: std::time::Duration,
}

#[async_trait]
impl LlmProvider for SlowQueuedProvider {
    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        model: Option<&str>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
    ) -> Result<LlmResponse> {
        tokio::time::sleep(self.delay).await;
        self.inner
            .chat(messages, tools, model, max_tokens, temperature)
            .await
    }
}

#[tokio::test]
async fn agent_loop_honors_stop_command() {
    let dir = tempdir().unwrap();
    // A provider that keeps requesting tools with UNIQUE arguments to avoid repetition detection
    let mut responses = Vec::new();
    for i in 0..100 {
        responses.push(LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{i}"),
                name: "list_dir".to_string(),
                arguments: json!({"path": format!("./{i}")}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        });
    }

    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new("test-model", responses),
        delay: std::time::Duration::from_millis(10),
    });
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            200, // Many iterations
            8_000,
            Default::default(),
            None,
            ExecToolConfig {
                enable: false,
                timeout: 60,
                path_append: String::new(),
            },
            false,
            None,
            &Default::default(),
        )
        .await
        .unwrap(),
    );

    let agent_clone = agent.clone();
    let handle = tokio::spawn(async move {
        agent_clone
            .process_direct("loop forever", "cli:direct", "cli", "direct")
            .await
    });

    // Wait a bit then send stop
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let stop_response = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop_response.content, "Stopping current turn...");

    let result = handle.await.unwrap().unwrap();
    assert!(result.is_none());
}
