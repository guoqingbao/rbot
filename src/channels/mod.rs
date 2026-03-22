pub mod email;
pub mod feishu;
pub mod slack;
pub mod telegram;

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::config::ChannelsConfig;
use crate::storage::{InboundMessage, MessageBus, OutboundMessage};

pub use email::{
    EmailBackend, EmailBackendError, EmailBackendErrorKind, EmailChannel, EmailConfig,
    EmailSearchCriteria, OutgoingEmail, ParsedInboundEmail, RawEmail,
};
pub use feishu::{
    FeishuApi, FeishuChannel, FeishuConfig, FeishuMessageDetails, FeishuResource,
    extract_post_content,
};
pub use slack::{SlackApi, SlackChannel, SlackConfig, SlackDmConfig};
pub use telegram::{
    ReplyParameters, TELEGRAM_MAX_MESSAGE_LEN, TELEGRAM_REPLY_CONTEXT_MAX_LEN, TelegramApi,
    TelegramBotIdentity, TelegramChannel, TelegramConfig,
};

#[derive(Debug, Clone)]
pub struct ChannelBase {
    pub config: Value,
    pub bus: MessageBus,
    running: Arc<Mutex<bool>>,
}

impl ChannelBase {
    pub fn new(config: Value, bus: MessageBus) -> Self {
        Self {
            config,
            bus,
            running: Arc::new(Mutex::new(false)),
        }
    }

    pub fn allow_from(&self) -> Vec<String> {
        self.config
            .get("allowFrom")
            .or_else(|| self.config.get("allow_from"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn is_allowed(&self, sender_id: &str) -> bool {
        let allow = self.allow_from();
        if allow.is_empty() {
            return false;
        }
        if allow.iter().any(|item| item == "*") {
            return true;
        }
        allow.iter().any(|item| item == sender_id)
    }

    pub async fn handle_message(
        &self,
        channel_name: &str,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        media: Option<Vec<String>>,
        metadata: Option<BTreeMap<String, Value>>,
        session_key_override: Option<String>,
    ) -> Result<()> {
        if !self.is_allowed(sender_id) {
            return Ok(());
        }
        self.bus
            .publish_inbound(InboundMessage {
                channel: channel_name.to_string(),
                sender_id: sender_id.to_string(),
                chat_id: chat_id.to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: media.unwrap_or_default(),
                metadata: metadata.unwrap_or_default(),
                session_key_override,
            })
            .await?;
        Ok(())
    }

    pub fn set_running(&self, value: bool) {
        *self.running.lock().expect("channel running lock poisoned") = value;
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().expect("channel running lock poisoned")
    }
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn base(&self) -> &ChannelBase;
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str {
        self.name()
    }
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}

type ChannelFactory = Arc<dyn Fn(Value, MessageBus) -> Result<Arc<dyn Channel>> + Send + Sync>;

#[derive(Clone)]
pub struct ChannelDescriptor {
    pub name: String,
    pub display_name: String,
    pub default_config: Value,
    pub factory: ChannelFactory,
}

impl ChannelDescriptor {
    pub fn new(
        name: impl Into<String>,
        display_name: impl Into<String>,
        default_config: Value,
        factory: ChannelFactory,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            default_config,
            factory,
        }
    }
}

fn plugin_registry() -> &'static Mutex<BTreeMap<String, ChannelDescriptor>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, ChannelDescriptor>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone)]
pub struct LocalChannel {
    base: ChannelBase,
    sent: Arc<Mutex<Vec<OutboundMessage>>>,
}

impl LocalChannel {
    pub fn new(config: Value, bus: MessageBus) -> Self {
        Self {
            base: ChannelBase::new(config, bus),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn inject_message(
        &self,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        session_key: Option<String>,
    ) -> Result<()> {
        self.base
            .handle_message(
                self.name(),
                sender_id,
                chat_id,
                content,
                None,
                None,
                session_key,
            )
            .await
    }

    pub fn sent_messages(&self) -> Vec<OutboundMessage> {
        self.sent
            .lock()
            .expect("local channel sent lock poisoned")
            .clone()
    }

    pub fn default_config() -> Value {
        json!({"enabled": false, "allowFrom": ["*"]})
    }
}

#[async_trait]
impl Channel for LocalChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "local"
    }

    fn display_name(&self) -> &'static str {
        "Local"
    }

    async fn start(&self) -> Result<()> {
        self.base.set_running(true);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.sent
            .lock()
            .expect("local channel sent lock poisoned")
            .push(msg);
        Ok(())
    }
}

pub fn discover_channel_names() -> Vec<String> {
    vec![
        "local".to_string(),
        "email".to_string(),
        "feishu".to_string(),
        "slack".to_string(),
        "telegram".to_string(),
    ]
}

pub fn register_plugin(descriptor: ChannelDescriptor) {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .insert(descriptor.name.clone(), descriptor);
}

pub fn clear_plugins() {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .clear();
}

pub fn discover_plugins() -> BTreeMap<String, ChannelDescriptor> {
    plugin_registry()
        .lock()
        .expect("plugin registry lock poisoned")
        .clone()
}

pub fn discover_all() -> BTreeMap<String, ChannelDescriptor> {
    let mut builtin = BTreeMap::new();
    builtin.insert(
        "local".to_string(),
        ChannelDescriptor::new(
            "local",
            "Local",
            LocalChannel::default_config(),
            Arc::new(|config, bus| Ok(Arc::new(LocalChannel::new(config, bus)))),
        ),
    );
    builtin.insert(
        "email".to_string(),
        ChannelDescriptor::new(
            "email",
            "Email",
            EmailChannel::default_config(),
            Arc::new(|config, bus| Ok(Arc::new(EmailChannel::new(config, bus)?))),
        ),
    );
    builtin.insert(
        "feishu".to_string(),
        ChannelDescriptor::new(
            "feishu",
            "Feishu",
            FeishuChannel::default_config(),
            Arc::new(|config, bus| Ok(Arc::new(FeishuChannel::new(config, bus)?))),
        ),
    );
    builtin.insert(
        "slack".to_string(),
        ChannelDescriptor::new(
            "slack",
            "Slack",
            SlackChannel::default_config(),
            Arc::new(|config, bus| Ok(Arc::new(SlackChannel::new(config, bus)?))),
        ),
    );
    builtin.insert(
        "telegram".to_string(),
        ChannelDescriptor::new(
            "telegram",
            "Telegram",
            TelegramChannel::default_config(),
            Arc::new(|config, bus| Ok(Arc::new(TelegramChannel::new(config, bus)?))),
        ),
    );
    let external = discover_plugins();
    let mut merged = external;
    for (name, desc) in builtin {
        merged.insert(name, desc);
    }
    merged
}

pub struct ChannelManager {
    pub bus: MessageBus,
    pub channels: BTreeMap<String, Arc<dyn Channel>>,
    config: ChannelsConfig,
    dispatch_task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl ChannelManager {
    pub fn new(config: ChannelsConfig, bus: MessageBus) -> Result<Self> {
        let mut manager = Self {
            bus,
            channels: BTreeMap::new(),
            config,
            dispatch_task: AsyncMutex::new(None),
        };
        manager.init_channels()?;
        Ok(manager)
    }

    fn init_channels(&mut self) -> Result<()> {
        for (name, descriptor) in discover_all() {
            let Some(section) = self.config.section(&name).cloned() else {
                continue;
            };
            let enabled = section
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            let channel = (descriptor.factory)(section.clone(), self.bus.clone())?;
            if channel.base().allow_from().is_empty() {
                return Err(anyhow!(
                    "\"{name}\" has empty allowFrom (denies all). Set [\"*\"] or explicit IDs."
                ));
            }
            self.channels.insert(name, channel);
        }
        Ok(())
    }

    pub async fn start_all(&self) -> Result<()> {
        if self.channels.is_empty() {
            return Ok(());
        }
        let mut dispatch = self.dispatch_task.lock().await;
        if dispatch.is_none() {
            let channels = self.channels.clone();
            let bus = self.bus.clone();
            let send_progress = self.config.send_progress;
            let send_tool_hints = self.config.send_tool_hints;
            *dispatch = Some(tokio::spawn(async move {
                loop {
                    let Some(msg) = bus.consume_outbound().await else {
                        break;
                    };
                    let channel_name = msg.channel.clone();
                    let chat_id = msg.chat_id.clone();
                    let content_preview = msg.content.chars().take(200).collect::<String>();
                    if msg.metadata.get("_progress").is_some() {
                        let is_tool_hint = msg
                            .metadata
                            .get("_tool_hint")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if is_tool_hint && !send_tool_hints {
                            continue;
                        }
                        if !is_tool_hint && !send_progress {
                            continue;
                        }
                    }
                    if let Some(channel) = channels.get(&msg.channel) {
                        if let Err(err) = channel.send(msg).await {
                            eprintln!(
                                "failed to send outbound message via channel '{channel_name}' to '{chat_id}': {err}"
                            );
                        }
                    } else if channel_name == "system" {
                        eprintln!("{content_preview}");
                    } else {
                        eprintln!(
                            "dropping outbound message for unknown or disabled channel '{channel_name}' to '{chat_id}'"
                        );
                    }
                }
            }));
        }
        drop(dispatch);
        for channel in self.channels.values() {
            channel.start().await?;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        if let Some(task) = self.dispatch_task.lock().await.take() {
            task.abort();
        }
        for channel in self.channels.values() {
            channel.stop().await?;
        }
        Ok(())
    }

    pub async fn start_channel(&self, name: &str) -> Result<()> {
        let Some(channel) = self.channels.get(name) else {
            return Err(anyhow!("unknown channel '{name}'"));
        };
        channel.start().await
    }

    pub async fn stop_channel(&self, name: &str) -> Result<()> {
        let Some(channel) = self.channels.get(name) else {
            return Err(anyhow!("unknown channel '{name}'"));
        };
        channel.stop().await
    }

    pub fn get_channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels.get(name).cloned()
    }

    pub fn enabled_channels(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }

    pub fn status(&self) -> BTreeMap<String, BTreeMap<String, bool>> {
        self.channels
            .iter()
            .map(|(name, channel)| {
                (
                    name.clone(),
                    BTreeMap::from([
                        ("enabled".to_string(), true),
                        ("running".to_string(), channel.base().is_running()),
                    ]),
                )
            })
            .collect()
    }
}
