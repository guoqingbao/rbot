use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::engine::AgentLoop;
use crate::storage::{MessageBus, OutboundMessage};
use crate::tools::MessageSendCallback;

#[derive(Clone)]
pub struct AgentRuntime {
    agent: Arc<AgentLoop>,
    bus: MessageBus,
    running: Arc<AtomicBool>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AgentRuntime {
    pub fn new(agent: Arc<AgentLoop>, bus: MessageBus) -> Self {
        let publish_bus = bus.clone();
        let callback: MessageSendCallback = Arc::new(move |msg| {
            let bus = publish_bus.clone();
            Box::pin(async move {
                bus.publish_outbound(msg).await?;
                Ok(())
            })
        });
        agent.set_message_sender(Some(callback));
        let progress_bus = bus.clone();
        let progress_callback: MessageSendCallback = Arc::new(move |msg| {
            let bus = progress_bus.clone();
            Box::pin(async move {
                bus.publish_outbound(msg).await?;
                Ok(())
            })
        });
        agent.set_progress_sender(Some(progress_callback));
        agent.set_runtime_bus(bus.clone());
        Self {
            agent,
            bus,
            running: Arc::new(AtomicBool::new(false)),
            task: Arc::new(Mutex::new(None)),
        }
    }

    pub fn bus(&self) -> MessageBus {
        self.bus.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn start(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let this = self.clone();
        let handle = tokio::spawn(async move {
            while this.running.load(Ordering::SeqCst) {
                let Some(msg) = this.bus.consume_inbound().await else {
                    break;
                };
                match this.agent.process_inbound(msg).await {
                    Ok(Some(outbound)) => {
                        let _ = this.bus.publish_outbound(outbound).await;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = this
                            .bus
                            .publish_outbound(OutboundMessage {
                                channel: "system".to_string(),
                                chat_id: "runtime".to_string(),
                                content: format!("Error processing inbound message: {err}"),
                                reply_to: None,
                                media: Vec::new(),
                                metadata: Default::default(),
                            })
                            .await;
                    }
                }
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
        self.agent.set_message_sender(None);
        self.agent.set_progress_sender(None);
    }
}
