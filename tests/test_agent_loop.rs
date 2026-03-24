use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rbot::config::ExecToolConfig;
use rbot::engine::AgentLoop;
use rbot::providers::{LlmProvider, LlmResponse, LlmUsage, QueuedProvider, ToolCallRequest};
use rbot::runtime::AgentRuntime;
use rbot::storage::{ChatMessage, InboundMessage, MessageBus, SessionManager};
use rbot::util::{DEFAULT_HISTORY_TEMPLATE, workspace_state_dir};
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
        32 * 1024,
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
        32 * 1024,
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
        32 * 1024,
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
    let repeated_response = LlmResponse {
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
    };
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![repeated_response; 30],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
        8_000,
        32 * 1024,
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

#[tokio::test]
async fn agent_loop_stops_on_repeated_tool_calls_even_with_new_ids() {
    let dir = tempdir().unwrap();
    let responses = (1..=30)
        .map(|idx| LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{idx}"),
                name: "exec".to_string(),
                arguments: json!({"command": "find /root -maxdepth 3 -name Cargo.toml"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        })
        .collect();
    let provider = Arc::new(QueuedProvider::new("test-model", responses));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
        8_000,
        32 * 1024,
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
        .process_direct("find cargo manifests", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("pattern repeated"));
    assert!(response.content.contains("30 times"));
}

#[tokio::test]
async fn agent_loop_does_not_stop_when_tool_arguments_change() {
    let dir = tempdir().unwrap();
    let mut responses = (1..=12)
        .map(|idx| LlmResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: format!("call_{idx}"),
                name: "exec".to_string(),
                arguments: json!({"command": format!("find /root -maxdepth {idx} -name Cargo.toml")}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        })
        .collect::<Vec<_>>();
    responses.push(LlmResponse {
        content: Some("Finished reviewing varying search attempts.".to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LlmUsage::default(),
        reasoning_content: None,
        thinking_blocks: None,
    });
    let provider = Arc::new(QueuedProvider::new("test-model", responses));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        40,
        8_000,
        32 * 1024,
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
        .process_direct("find cargo manifests", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.content,
        "Finished reviewing varying search attempts."
    );
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
            32 * 1024,
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

#[tokio::test]
async fn stop_command_does_not_block_the_next_prompt() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new(
            "test-model",
            vec![
                LlmResponse {
                    content: Some("first response".to_string()),
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: LlmUsage::default(),
                    reasoning_content: None,
                    thinking_blocks: None,
                },
                LlmResponse {
                    content: Some("second response".to_string()),
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: LlmUsage::default(),
                    reasoning_content: None,
                    thinking_blocks: None,
                },
            ],
        ),
        delay: Duration::from_millis(50),
    });
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            8,
            8_000,
            32 * 1024,
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
            .process_direct("first prompt", "cli:direct", "cli", "direct")
            .await
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    let stop_response = agent
        .process_direct("/stop", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop_response.content, "Stopping current turn...");
    assert!(handle.await.unwrap().unwrap().is_none());

    let next = agent
        .process_direct("second prompt", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.content, "second response");
}

#[tokio::test]
async fn runtime_stop_command_sends_threaded_ack_and_completion() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(SlowQueuedProvider {
        inner: QueuedProvider::new(
            "test-model",
            vec![LlmResponse {
                content: Some("should not be delivered".to_string()),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            }],
        ),
        delay: Duration::from_millis(75),
    });
    let agent = Arc::new(
        AgentLoop::new(
            provider,
            dir.path(),
            Some("test-model".to_string()),
            8,
            8_000,
            32 * 1024,
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
    let bus = MessageBus::new(16);
    let runtime = AgentRuntime::new(agent, bus.clone());
    runtime.start().await.unwrap();

    let session_key = "slack:C123:1700000000.000100".to_string();
    let slack_metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "work on this".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: slack_metadata.clone(),
        session_key_override: Some(session_key.clone()),
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    bus.publish_inbound(InboundMessage {
        channel: "slack".to_string(),
        sender_id: "u1".to_string(),
        chat_id: "C123".to_string(),
        content: "/stop".to_string(),
        timestamp: chrono::Utc::now(),
        media: Vec::new(),
        metadata: slack_metadata.clone(),
        session_key_override: Some(session_key),
    })
    .await
    .unwrap();

    let ack = tokio::time::timeout(Duration::from_secs(1), bus.consume_outbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ack.content, "Stopping current turn...");
    assert_eq!(ack.metadata, slack_metadata);

    let completion = tokio::time::timeout(Duration::from_secs(1), bus.consume_outbound())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completion.content, "Task stopped by user.");
    assert_eq!(completion.metadata, ack.metadata);

    runtime.stop().await;
}

#[tokio::test]
async fn clear_command_resets_history_template() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![LlmResponse {
            content: Some("Task finished.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: LlmUsage::default(),
            reasoning_content: None,
            thinking_blocks: None,
        }],
    ));
    let agent = AgentLoop::new(
        provider,
        dir.path(),
        Some("test-model".to_string()),
        8,
        8_000,
        32 * 1024,
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

    agent
        .process_direct("finish something", "cli:direct", "cli", "direct")
        .await
        .unwrap();

    let history_path = workspace_state_dir(dir.path())
        .join("memory")
        .join("HISTORY.md");
    std::fs::write(&history_path, "junk history").unwrap();

    let response = agent
        .process_direct("/clear", "cli:direct", "cli", "direct")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.content,
        "New session started. Session cleared and history reset."
    );
    assert_eq!(
        std::fs::read_to_string(history_path).unwrap(),
        DEFAULT_HISTORY_TEMPLATE
    );
}

#[tokio::test]
async fn completed_tasks_append_task_summary_to_memory() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(QueuedProvider::new(
        "test-model",
        vec![
            LlmResponse {
                content: Some(
                    "Implemented the stop acknowledgement flow. Added runtime replies, wired session cancellation cleanup, and covered the path with regression tests.".to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            },
            LlmResponse {
                content: Some(
                    r#"{"title":"Stop acknowledgement flow","summary":"Added immediate stop acknowledgement and completion replies, with cancellation cleanup and regression coverage.","attention_points":["Keep threaded channel metadata on both replies."]}"#
                        .to_string(),
                ),
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
        32 * 1024,
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

    agent
        .process_direct(
            "Implement stop acknowledgement flow",
            "cli:direct",
            "cli",
            "direct",
        )
        .await
        .unwrap();

    let memory =
        std::fs::read_to_string(workspace_state_dir(dir.path()).join("memory/MEMORY.md")).unwrap();
    assert!(memory.contains("Task Summary"));
    assert!(memory.contains("Stop acknowledgement flow"));
    assert!(memory.contains("Added immediate stop acknowledgement and completion replies"));
    assert!(memory.contains("Keep threaded channel metadata on both replies."));
}

#[tokio::test]
async fn memorize_command_appends_user_memory_entry() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(QueuedProvider::new(
            "test-model",
            vec![LlmResponse {
                content: Some(
                    r#"{"title":"User prefers concise delivery","summary":"Keep responses concise and include documentation updates when feature work changes behavior.","attention_points":["Docs should stay in sync with shipped changes."]}"#
                        .to_string(),
                ),
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: LlmUsage::default(),
                reasoning_content: None,
                thinking_blocks: None,
            }],
        )),
        dir.path(),
        Some("test-model".to_string()),
        8,
        8_000,
        32 * 1024,
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
        .process_direct(
            "/memorize User prefers concise delivery and wants docs updated with feature work.",
            "cli:direct",
            "cli",
            "direct",
        )
        .await
        .unwrap()
        .unwrap();
    assert!(response.content.contains("Memorized into permanent memory"));

    let memory =
        std::fs::read_to_string(workspace_state_dir(dir.path()).join("memory/MEMORY.md")).unwrap();
    assert!(memory.contains("User Instructed Memory"));
    assert!(memory.contains("User prefers concise delivery"));
    assert!(memory.contains("Docs should stay in sync with shipped changes."));
}

#[tokio::test]
async fn help_command_preserves_inbound_metadata() {
    let dir = tempdir().unwrap();
    let agent = AgentLoop::new(
        Arc::new(QueuedProvider::new("test-model", Vec::new())),
        dir.path(),
        Some("test-model".to_string()),
        8,
        8_000,
        32 * 1024,
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
    let metadata = BTreeMap::from([(
        "slack".to_string(),
        json!({
            "thread_ts": "1700000000.000100",
            "channel_type": "channel",
        }),
    )]);

    let response = agent
        .process_inbound(InboundMessage {
            channel: "slack".to_string(),
            sender_id: "u1".to_string(),
            chat_id: "C123".to_string(),
            content: "/help".to_string(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: metadata.clone(),
            session_key_override: Some("slack:C123:1700000000.000100".to_string()),
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.metadata, metadata);
    assert_eq!(
        response.content,
        "/new (or clear)\n/stop\n/memorize <text>\n/status\n/help"
    );
}
