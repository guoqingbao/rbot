use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackDmConfig {
    pub enabled: bool,
    pub policy: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
}

impl Default for SlackDmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: "open".to_string(),
            allow_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackConfig {
    pub enabled: bool,
    pub mode: String,
    #[serde(alias = "webhookPath")]
    pub webhook_path: String,
    #[serde(alias = "signingSecret")]
    pub signing_secret: String,
    #[serde(alias = "botToken")]
    pub bot_token: String,
    #[serde(alias = "appToken")]
    pub app_token: String,
    #[serde(alias = "userTokenReadOnly")]
    pub user_token_read_only: bool,
    #[serde(alias = "replyInThread")]
    pub reply_in_thread: bool,
    #[serde(alias = "reactEmoji")]
    pub react_emoji: String,
    #[serde(alias = "doneEmoji")]
    pub done_emoji: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
    #[serde(alias = "groupPolicy")]
    pub group_policy: String,
    #[serde(alias = "groupAllowFrom")]
    pub group_allow_from: Vec<String>,
    pub dm: SlackDmConfig,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "webhook".to_string(),
            webhook_path: "/slack/events".to_string(),
            signing_secret: String::new(),
            bot_token: String::new(),
            app_token: String::new(),
            user_token_read_only: true,
            reply_in_thread: true,
            react_emoji: "eyes".to_string(),
            done_emoji: "white_check_mark".to_string(),
            allow_from: Vec::new(),
            group_policy: "mention".to_string(),
            group_allow_from: Vec::new(),
            dm: SlackDmConfig::default(),
        }
    }
}

#[async_trait]
pub trait SlackApi: Send + Sync {
    async fn auth_test(&self) -> Result<String>;
    async fn chat_post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()>;
    async fn files_upload(&self, channel: &str, file: &str, thread_ts: Option<&str>) -> Result<()>;
    async fn reactions_add(&self, channel: &str, name: &str, timestamp: &str) -> Result<()>;
    async fn reactions_remove(&self, channel: &str, name: &str, timestamp: &str) -> Result<()>;
}

pub struct ReqwestSlackApi {
    client: Client,
    bot_token: String,
}

impl ReqwestSlackApi {
    pub fn new(bot_token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder().build()?,
            bot_token,
        })
    }

    async fn post_json(&self, url: &str, body: Value) -> Result<()> {
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow!(
                "slack api error: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }
}

#[async_trait]
impl SlackApi for ReqwestSlackApi {
    async fn auth_test(&self) -> Result<String> {
        let response = self
            .client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(payload
                .get("user_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string())
        } else {
            Err(anyhow!(
                "slack auth.test error: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }

    async fn chat_post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        self.post_json(
            "https://slack.com/api/chat.postMessage",
            json!({
                "channel": channel,
                "text": text,
                "thread_ts": thread_ts,
            }),
        )
        .await
    }

    async fn files_upload(&self, channel: &str, file: &str, thread_ts: Option<&str>) -> Result<()> {
        let form = if file.starts_with("http://") || file.starts_with("https://") {
            reqwest::multipart::Form::new()
                .text("channels", channel.to_string())
                .text("content", file.to_string())
                .text(
                    "filename",
                    file.rsplit('/').next().unwrap_or("attachment").to_string(),
                )
        } else {
            let bytes = std::fs::read(file)?;
            reqwest::multipart::Form::new()
                .text("channels", channel.to_string())
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes)
                        .file_name(file.rsplit('/').next().unwrap_or("attachment").to_string()),
                )
        };
        let form = if let Some(thread_ts) = thread_ts {
            form.text("thread_ts", thread_ts.to_string())
        } else {
            form
        };
        let response = self
            .client
            .post("https://slack.com/api/files.upload")
            .bearer_auth(&self.bot_token)
            .multipart(form)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow!(
                "slack upload error: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }

    async fn reactions_add(&self, channel: &str, name: &str, timestamp: &str) -> Result<()> {
        self.post_json(
            "https://slack.com/api/reactions.add",
            json!({
                "channel": channel,
                "name": name,
                "timestamp": timestamp,
            }),
        )
        .await
    }

    async fn reactions_remove(&self, channel: &str, name: &str, timestamp: &str) -> Result<()> {
        self.post_json(
            "https://slack.com/api/reactions.remove",
            json!({
                "channel": channel,
                "name": name,
                "timestamp": timestamp,
            }),
        )
        .await
    }
}

#[derive(Clone)]
pub struct SlackChannel {
    base: ChannelBase,
    config: SlackConfig,
    api: Arc<AsyncMutex<Option<Arc<dyn SlackApi>>>>,
    bot_user_id: Arc<Mutex<Option<String>>>,
    socket_task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    socket_shutdown: Arc<AsyncMutex<Option<watch::Sender<bool>>>>,
}

impl SlackChannel {
    pub fn new(config: Value, bus: MessageBus) -> Result<Self> {
        let config: SlackConfig = serde_json::from_value(config)?;
        Ok(Self {
            base: ChannelBase::new(serde_json::to_value(&config)?, bus),
            config,
            api: Arc::new(AsyncMutex::new(None)),
            bot_user_id: Arc::new(Mutex::new(None)),
            socket_task: Arc::new(AsyncMutex::new(None)),
            socket_shutdown: Arc::new(AsyncMutex::new(None)),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(SlackConfig::default()).expect("serializable slack config")
    }

    pub async fn set_api(&self, api: Arc<dyn SlackApi>) {
        *self.api.lock().await = Some(api);
    }

    pub fn set_bot_user_id(&self, user_id: Option<String>) {
        *self
            .bot_user_id
            .lock()
            .expect("slack bot user id lock poisoned") = user_id;
    }

    async fn open_socket_url(&self) -> Result<String> {
        if self.config.app_token.trim().is_empty() {
            return Err(anyhow!("slack app token not configured for socket mode"));
        }
        let response = Client::builder()
            .build()?
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.config.app_token)
            .send()
            .await?;
        let payload: Value = response.json().await?;
        if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            payload
                .get("url")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| anyhow!("slack apps.connections.open returned no url"))
        } else {
            Err(anyhow!(
                "slack apps.connections.open error: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ))
        }
    }

    async fn run_socket_mode(self, mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            match self.run_socket_session(&mut shutdown_rx).await {
                Ok(()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    eprintln!("[slack] socket mode disconnected; reconnecting");
                }
                Err(err) => {
                    eprintln!("[slack] socket mode error: {err}");
                }
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }
        }
    }

    async fn run_socket_session(&self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
        let url = self.open_socket_url().await?;
        eprintln!("[slack] opening socket mode connection");
        let (mut socket, _) = connect_async(url).await?;
        eprintln!("[slack] socket mode connected");

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let _ = socket.close(None).await;
                        return Ok(());
                    }
                }
                frame = socket.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_socket_frame(&mut socket, text.as_str()).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            socket.send(Message::Pong(payload)).await?;
                        }
                        Some(Ok(Message::Close(_))) => return Ok(()),
                        Some(Ok(_)) => {}
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    async fn handle_socket_frame<S>(&self, socket: &mut S, text: &str) -> Result<()>
    where
        S: SinkExt<Message> + Unpin,
        <S as futures::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
    {
        let payload: Value = serde_json::from_str(text)?;
        if let Some(envelope_id) = payload.get("envelope_id").and_then(Value::as_str) {
            socket
                .send(Message::Text(
                    json!({ "envelope_id": envelope_id }).to_string().into(),
                ))
                .await?;
        }

        match payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "hello" => {
                eprintln!("[slack] socket mode hello");
            }
            "disconnect" => {
                eprintln!("[slack] socket mode disconnect requested by Slack");
            }
            "events_api" => {
                if let Some(event) = payload.get("payload").and_then(|inner| inner.get("event")) {
                    eprintln!(
                        "[slack] socket received event type '{}'",
                        event
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    );
                    self.handle_event(event).await?;
                }
            }
            kind => {
                eprintln!("[slack] socket envelope type '{kind}'");
            }
        }
        Ok(())
    }

    async fn api(&self) -> Result<Arc<dyn SlackApi>> {
        if let Some(api) = self.api.lock().await.clone() {
            return Ok(api);
        }
        if self.config.bot_token.trim().is_empty() {
            return Err(anyhow!("slack bot token not configured"));
        }
        let api: Arc<dyn SlackApi> = Arc::new(ReqwestSlackApi::new(self.config.bot_token.clone())?);
        *self.api.lock().await = Some(api.clone());
        Ok(api)
    }

    pub async fn handle_event(&self, event: &Value) -> Result<()> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(event_type, "message" | "app_mention") {
            eprintln!("[slack] ignoring event type '{event_type}'");
            return Ok(());
        }
        if event.get("subtype").is_some() {
            eprintln!(
                "[slack] ignoring subtype event '{}' ({})",
                event_type,
                event
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            );
            return Ok(());
        }
        let sender_id = event
            .get("user")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let chat_id = event
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if sender_id.is_empty() || chat_id.is_empty() {
            eprintln!("[slack] ignoring malformed event without user/channel");
            return Ok(());
        }
        if self
            .bot_user_id
            .lock()
            .expect("slack bot user id lock poisoned")
            .as_deref()
            == Some(sender_id)
        {
            eprintln!("[slack] ignoring self-authored event in channel {chat_id}");
            return Ok(());
        }

        let channel_type = event
            .get("channel_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !self.is_allowed_sender(sender_id, channel_type) {
            eprintln!("[slack] ignoring sender '{sender_id}' in {channel_type} due to allowFrom");
            return Ok(());
        }
        if channel_type != "im" && !self.should_respond_in_channel(event_type, &text) {
            eprintln!(
                "[slack] ignoring channel message in {chat_id}: mention/group policy did not match"
            );
            return Ok(());
        }
        let thread_ts = event.get("thread_ts").and_then(Value::as_str).or_else(|| {
            (self.config.reply_in_thread && channel_type != "im")
                .then(|| event.get("ts").and_then(Value::as_str))
                .flatten()
        });
        let cleaned = self.strip_bot_mention(&text);
        let session_key = thread_ts
            .filter(|_| channel_type != "im")
            .map(|thread_ts| format!("slack:{chat_id}:{thread_ts}"));
        let metadata = BTreeMap::from([(
            "slack".to_string(),
            json!({
                "event": event,
                "thread_ts": thread_ts,
                "channel_type": channel_type,
            }),
        )]);
        eprintln!("[slack] inbound {event_type} from {sender_id} in {chat_id} ({channel_type})");
        self.base
            .handle_message(
                self.name(),
                sender_id,
                chat_id,
                cleaned.trim(),
                None,
                Some(metadata),
                session_key,
            )
            .await
    }

    fn should_respond_in_channel(&self, event_type: &str, text: &str) -> bool {
        if self.config.group_policy == "open" {
            return true;
        }
        if event_type == "app_mention" {
            return true;
        }
        let Some(bot_user_id) = self
            .bot_user_id
            .lock()
            .expect("slack bot user id lock poisoned")
            .clone()
        else {
            return false;
        };
        text.contains(&format!("<@{bot_user_id}>"))
    }

    fn strip_bot_mention(&self, text: &str) -> String {
        let re = Regex::new(r"<@[A-Z0-9]+>").expect("valid slack mention regex");
        re.replace_all(text, "").trim().to_string()
    }

    fn is_allowed_sender(&self, sender_id: &str, channel_type: &str) -> bool {
        if channel_type == "im" {
            if !self.config.dm.enabled {
                return false;
            }
            if self.config.dm.policy == "open" {
                return true;
            }
            return self
                .config
                .dm
                .allow_from
                .iter()
                .any(|item| item == sender_id);
        }
        if !self.config.group_allow_from.is_empty() {
            return self
                .config
                .group_allow_from
                .iter()
                .any(|item| item == sender_id);
        }
        self.base.is_allowed(sender_id)
    }

    async fn update_react_emoji(&self, chat_id: &str, ts: Option<&str>) -> Result<()> {
        let Some(ts) = ts else {
            return Ok(());
        };
        let api = self.api().await?;
        let _ = api
            .reactions_remove(chat_id, &self.config.react_emoji, ts)
            .await;
        if !self.config.done_emoji.is_empty() {
            let _ = api
                .reactions_add(chat_id, &self.config.done_emoji, ts)
                .await;
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "slack"
    }

    fn display_name(&self) -> &'static str {
        "Slack"
    }

    async fn start(&self) -> Result<()> {
        if !self.config.bot_token.trim().is_empty() {
            let api = self.api().await?;
            let bot_user_id = api.auth_test().await?;
            self.set_bot_user_id(Some(bot_user_id.clone()));
            if self.config.mode.eq_ignore_ascii_case("socket") {
                if self.config.app_token.trim().is_empty() {
                    return Err(anyhow!("slack mode=socket requires appToken"));
                }
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                *self.socket_shutdown.lock().await = Some(shutdown_tx);
                let channel = self.clone();
                let handle = tokio::spawn(async move {
                    channel.run_socket_mode(shutdown_rx).await;
                });
                *self.socket_task.lock().await = Some(handle);
                eprintln!(
                    "[slack] connected as {bot_user_id}; socket mode enabled; groupPolicy={}",
                    self.config.group_policy
                );
            } else {
                eprintln!(
                    "[slack] connected as {bot_user_id}; webhook path {}; groupPolicy={}",
                    self.config.webhook_path, self.config.group_policy
                );
            }
        }
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(shutdown) = self.socket_shutdown.lock().await.take() {
            let _ = shutdown.send(true);
        }
        if let Some(task) = self.socket_task.lock().await.take() {
            let _ = task.await;
        }
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let api = self.api().await?;
        let slack_meta = msg.metadata.get("slack").cloned().unwrap_or(Value::Null);
        let thread_ts = slack_meta
            .get("thread_ts")
            .and_then(Value::as_str)
            .filter(|_| {
                slack_meta
                    .get("channel_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    != "im"
            });
        if !msg.content.is_empty() || msg.media.is_empty() {
            let text = if msg.content.is_empty() {
                " ".to_string()
            } else if msg.content.ends_with('\n') {
                msg.content.clone()
            } else {
                format!("{}\n", msg.content)
            };
            api.chat_post_message(&msg.chat_id, &text, thread_ts)
                .await?;
            eprintln!(
                "[slack] sent message to {}{}",
                msg.chat_id,
                thread_ts
                    .map(|ts| format!(" (thread {ts})"))
                    .unwrap_or_default()
            );
        }
        for media_path in &msg.media {
            let _ = api.files_upload(&msg.chat_id, media_path, thread_ts).await;
            eprintln!(
                "[slack] uploaded attachment to {}: {}",
                msg.chat_id, media_path
            );
        }
        if !msg.metadata.get("_progress").is_some() {
            let event_ts = slack_meta
                .get("event")
                .and_then(|event| event.get("ts"))
                .and_then(Value::as_str);
            self.update_react_emoji(&msg.chat_id, event_ts).await?;
        }
        Ok(())
    }
}
