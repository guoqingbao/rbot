mod cli;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use cli::{CliOutput, CliShell, InputEvent, TurnSummary, run_config_channel, run_config_provider};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use rbot::channels::ChannelManager;
use rbot::config::Config;
use rbot::cron::CronService;
use rbot::engine::AgentLoop;
use rbot::observability::{InstrumentedProvider, RuntimeTelemetry};
use rbot::providers::SharedProvider;
use rbot::runtime::{
    AgentRuntime, HeartbeatService, build_gateway_router, build_provider_client,
    validate_run_config,
};
use rbot::storage::{InboundMessage, MessageBus, OutboundMessage};
use rbot::tools::MessageSendCallback;
use rbot::util::{sync_workspace_templates, workspace_state_dir};

#[derive(Parser)]
#[command(name = "rbot", about = "Rust-native autonomous bot runtime")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Onboard {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Chat {
        #[arg(long)]
        model: Option<String>,
        prompt: Vec<String>,
    },
    Repl {
        #[arg(long)]
        model: Option<String>,
    },
    Run {
        #[arg(long)]
        model: Option<String>,
    },
    Status {
        #[arg(long)]
        model: Option<String>,
    },
    Sessions,
    Jobs,
    PrintConfig,
    Config {
        #[arg(long)]
        provider: bool,
        #[arg(long)]
        channel: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Onboard { workspace } => onboard(cli.config.as_deref(), workspace.as_deref()),
        Command::Chat { model, prompt } => chat(cli.config.as_deref(), model, prompt).await,
        Command::Repl { model } => repl(cli.config.as_deref(), model).await,
        Command::Run { model } => run(cli.config.as_deref(), model).await,
        Command::Status { model } => status(cli.config.as_deref(), model).await,
        Command::Sessions => sessions(cli.config.as_deref()),
        Command::Jobs => jobs(cli.config.as_deref()),
        Command::PrintConfig => {
            let config = Config::load(cli.config.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        Command::Config { provider, channel } => {
            config_cmd(cli.config.as_deref(), provider, channel).await
        }
    }
}

async fn config_cmd(config_path: Option<&Path>, provider: bool, channel: bool) -> Result<()> {
    if provider {
        run_config_provider(config_path).await?;
    } else if channel {
        run_config_channel(config_path).await?;
    } else {
        println!("Please specify either --provider or --channel");
    }
    Ok(())
}

fn onboard(config_path: Option<&Path>, workspace_override: Option<&Path>) -> Result<()> {
    let mut config = Config::load(config_path)?;
    if let Some(workspace) = workspace_override {
        config.agents.defaults.workspace = workspace.display().to_string();
    }
    let path = config.save(config_path)?;
    let workspace = config.workspace_path();
    let created = sync_workspace_templates(&workspace)?;
    println!("Config: {}", path.display());
    println!("Workspace: {}", workspace.display());
    if created.is_empty() {
        println!("Workspace templates already present.");
    } else {
        for path in created {
            println!("Created {}", path.display());
        }
    }
    Ok(())
}

async fn chat(
    config_path: Option<&Path>,
    model: Option<String>,
    prompt: Vec<String>,
) -> Result<()> {
    let prompt = if prompt.is_empty() {
        return Err(anyhow!("chat requires a prompt"));
    } else {
        prompt.join(" ")
    };
    let built = build_agent(config_path, model).await?;
    let shell = CliShell::new(
        &built.workspace,
        &built.cwd,
        &built.model,
        &built.provider_name,
    )?;
    let stream = shell.stream_renderer();
    built
        .agent
        .set_progress_sender(Some(cli_progress_callback(stream.clone())));
    let started = Instant::now();
    stream.start_waiting();
    if let Some(response) = built
        .agent
        .process_direct_stream(
            &prompt,
            &built.session_key,
            "cli",
            &built.chat_id,
            Some(stream.callback()),
        )
        .await?
    {
        let summary = turn_summary(&built.agent, started.elapsed())?;
        stream.finish(&response.content, &summary);
    }
    Ok(())
}

async fn repl(config_path: Option<&Path>, model: Option<String>) -> Result<()> {
    let built = build_agent(config_path, model).await?;
    let BuiltAgent {
        agent,
        model,
        provider_name,
        workspace,
        cwd,
        session_key,
        chat_id,
    } = built;
    let agent = Arc::new(agent);
    let mut shell = CliShell::new(&workspace, &cwd, &model, &provider_name)?;
    shell.print_welcome();
    let output = shell.create_output()?;
    output.show_idle_footer();
    let (tx, mut rx) = unbounded_channel::<ReplEvent>();
    let input_tx = tx.clone();
    let busy_flag = Arc::new(AtomicBool::new(false));
    let input_busy_flag = Arc::clone(&busy_flag);
    std::thread::spawn(move || {
        let mut shell = shell;
        loop {
            match shell.read_event() {
                Ok(InputEvent::Prompt(prompt)) => {
                    shell.hide_footer();
                    if input_tx
                        .send(ReplEvent::Input(InputEvent::Prompt(prompt)))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(InputEvent::Exit) => {
                    shell.hide_footer();
                    let _ = input_tx.send(ReplEvent::Input(InputEvent::Exit));
                    break;
                }
                Ok(InputEvent::Interrupt) => {
                    if input_busy_flag.load(Ordering::SeqCst) {
                        if input_tx
                            .send(ReplEvent::Input(InputEvent::Interrupt))
                            .is_err()
                        {
                            break;
                        }
                    } else {
                        shell.hide_footer();
                        let _ = input_tx.send(ReplEvent::Input(InputEvent::Exit));
                        break;
                    }
                }
                Ok(InputEvent::Stop) => {
                    if input_tx.send(ReplEvent::Input(InputEvent::Stop)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    let _ = input_tx.send(ReplEvent::Input(InputEvent::Exit));
                    break;
                }
            }
        }
    });

    let mut pending = VecDeque::new();
    let mut busy = false;
    let mut exit_requested = false;
    let mut active_task = None::<tokio::task::JoinHandle<()>>;
    while let Some(event) = rx.recv().await {
        match event {
            ReplEvent::Input(InputEvent::Prompt(prompt)) => {
                if busy {
                    pending.push_back(prompt.clone());
                    output.print_queue_notice(pending.len(), &prompt);
                } else {
                    output.set_queue_depth(0);
                    active_task = Some(spawn_repl_turn(
                        agent.clone(),
                        session_key.clone(),
                        chat_id.clone(),
                        prompt,
                        output.clone(),
                        tx.clone(),
                    ));
                    busy = true;
                    busy_flag.store(true, Ordering::SeqCst);
                }
            }
            ReplEvent::Input(InputEvent::Interrupt) => {
                if busy {
                    if let Some(handle) = active_task.take() {
                        handle.abort();
                        agent.set_progress_sender(None);
                        output.print_interrupt_notice(pending.len());
                        busy = false;
                        busy_flag.store(false, Ordering::SeqCst);
                        if exit_requested {
                            break;
                        }
                        if let Some(next) = pending.pop_front() {
                            output.print_dequeue_notice(pending.len(), &next);
                            active_task = Some(spawn_repl_turn(
                                agent.clone(),
                                session_key.clone(),
                                chat_id.clone(),
                                next,
                                output.clone(),
                                tx.clone(),
                            ));
                            busy = true;
                            busy_flag.store(true, Ordering::SeqCst);
                        } else {
                            output.show_idle_footer();
                        }
                    }
                } else {
                    break;
                }
            }
            ReplEvent::Input(InputEvent::Stop) => {
                if busy {
                    if let Some(handle) = active_task.take() {
                        handle.abort();
                        agent.set_progress_sender(None);
                        output.print_interrupt_notice(pending.len());
                        busy = false;
                        busy_flag.store(false, Ordering::SeqCst);
                        if exit_requested {
                            break;
                        }
                        if let Some(next) = pending.pop_front() {
                            output.print_dequeue_notice(pending.len(), &next);
                            active_task = Some(spawn_repl_turn(
                                agent.clone(),
                                session_key.clone(),
                                chat_id.clone(),
                                next,
                                output.clone(),
                                tx.clone(),
                            ));
                            busy = true;
                            busy_flag.store(true, Ordering::SeqCst);
                        } else {
                            output.show_idle_footer();
                        }
                    }
                } else {
                    output.print_interrupt_notice(0);
                }
            }
            ReplEvent::Input(InputEvent::Exit) => {
                if busy {
                    exit_requested = true;
                    pending.clear();
                    output.print_exit_notice();
                } else {
                    break;
                }
            }
            ReplEvent::TurnFinished => {
                busy = false;
                busy_flag.store(false, Ordering::SeqCst);
                if exit_requested {
                    break;
                }
                if let Some(next) = pending.pop_front() {
                    output.print_dequeue_notice(pending.len(), &next);
                    active_task = Some(spawn_repl_turn(
                        agent.clone(),
                        session_key.clone(),
                        chat_id.clone(),
                        next,
                        output.clone(),
                        tx.clone(),
                    ));
                    busy = true;
                    busy_flag.store(true, Ordering::SeqCst);
                } else {
                    output.show_idle_footer();
                }
            }
        }
    }
    Ok(())
}

fn spawn_repl_turn(
    agent: Arc<AgentLoop>,
    session_key: String,
    chat_id: String,
    prompt: String,
    output: CliOutput,
    tx: UnboundedSender<ReplEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stream = output.stream_renderer();
        agent.set_progress_sender(Some(cli_progress_callback(stream.clone())));
        let started = Instant::now();
        stream.start_waiting();
        match agent
            .process_direct_stream(
                &prompt,
                &session_key,
                "cli",
                &chat_id,
                Some(stream.callback()),
            )
            .await
        {
            Ok(Some(response)) => match turn_summary(&agent, started.elapsed()) {
                Ok(summary) => stream.finish(&response.content, &summary),
                Err(err) => stream.finish_error(&err.to_string()),
            },
            Ok(None) => match turn_summary(&agent, started.elapsed()) {
                Ok(summary) => stream.finish_empty("no direct reply", &summary),
                Err(err) => stream.finish_error(&err.to_string()),
            },
            Err(err) => stream.finish_error(&err.to_string()),
        }
        let _ = tx.send(ReplEvent::TurnFinished);
    })
}

async fn run(config_path: Option<&Path>, model_override: Option<String>) -> Result<()> {
    let config = Config::load(config_path)?;
    let workspace = config.workspace_path();
    sync_workspace_templates(&workspace)?;
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    validate_run_config(&config, &model)?;
    let (provider_name, provider_cfg) = config
        .provider_for_model(Some(&model))
        .ok_or_else(|| anyhow!("no configured provider matched model '{model}'"))?;
    let provider = build_provider_client(
        &provider_name,
        &provider_cfg,
        &model,
        config.provider_api_base_for_model(Some(&model)),
        config.tools.web.proxy.as_deref(),
    )?;
    let telemetry = RuntimeTelemetry::new(
        provider_name.clone(),
        model.clone(),
        config.provider_api_base_for_model(Some(&model)),
    );
    let provider: SharedProvider = Arc::new(InstrumentedProvider::new(provider, telemetry.clone()));
    let bus = MessageBus::with_telemetry(256, Some(telemetry.clone()));
    let agent_slot: Arc<std::sync::Mutex<Option<Arc<AgentLoop>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let cron_bus = bus.clone();
    let cron_agent_slot = agent_slot.clone();
    let cron_service = CronService::with_callback(
        workspace_state_dir(&workspace)
            .join("cron")
            .join("jobs.json"),
        move |job| {
            let cron_bus = cron_bus.clone();
            let cron_agent_slot = cron_agent_slot.clone();
            async move {
                if job.payload.kind != "agent_turn" {
                    return Ok(());
                }
                let Some(agent) = cron_agent_slot
                    .lock()
                    .expect("cron agent slot lock poisoned")
                    .clone()
                else {
                    return Ok(());
                };
                let channel = job
                    .payload
                    .channel
                    .clone()
                    .unwrap_or_else(|| "system".to_string());
                let chat_id = job.payload.to.clone().unwrap_or_else(|| "cron".to_string());
                let session_key = format!("cron:{channel}:{chat_id}");
                if let Some(outbound) = agent
                    .process_direct(&job.payload.message, &session_key, &channel, &chat_id)
                    .await?
                {
                    if job.payload.deliver {
                        cron_bus.publish_outbound(outbound).await?;
                    }
                }
                Ok(())
            }
        },
    );

    let agent = Arc::new(
        build_agent_from_config(
            &config,
            provider,
            Some(model.clone()),
            Some(cron_service.clone()),
        )
        .await?,
    );
    *agent_slot.lock().expect("agent slot lock poisoned") = Some(agent.clone());

    let runtime = AgentRuntime::new(agent.clone(), bus.clone());
    let heartbeat_service = build_heartbeat_service(
        &config,
        workspace.as_path(),
        bus.clone(),
        agent_slot.clone(),
        model.clone(),
        provider_name.clone(),
    );
    runtime.start().await?;
    cron_service.start().await?;

    let manager = Arc::new(ChannelManager::new(config.channels.clone(), bus.clone())?);
    manager.start_all().await?;
    if let Some(heartbeat_service) = &heartbeat_service {
        heartbeat_service.start().await?;
    }

    let router = build_gateway_router(
        &manager,
        &config,
        Some(agent.clone()),
        Some(cron_service.clone()),
        heartbeat_service.clone(),
        Some(telemetry.clone()),
    )?
    .expect("gateway router is always available");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let host = config.gateway.host.clone();
    let port = config.gateway.port;
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    println!("Gateway listening on http://{host}:{port}");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
    });

    let enabled = manager.enabled_channels();
    if enabled.is_empty() {
        println!("Enabled channels: none");
        println!(
            "Runtime started without inbound channels; admin UI and gateway are still available."
        );
    } else {
        println!("Enabled channels: {}", enabled.join(", "));
    }
    println!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;
    let _ = shutdown_tx.send(true);
    let _ = server_task.await;
    manager.stop_all().await?;
    if let Some(heartbeat_service) = &heartbeat_service {
        heartbeat_service.stop().await;
    }
    cron_service.stop();
    runtime.stop().await;
    Ok(())
}

async fn status(config_path: Option<&Path>, model_override: Option<String>) -> Result<()> {
    let config = Config::load(config_path)?;
    let workspace = config.workspace_path();
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    let provider_name = config
        .provider_name_for_model(Some(&model))
        .unwrap_or_else(|| "unknown".to_string());
    let api_base = config.provider_api_base_for_model(Some(&model));
    let system = rbot::observability::collect_system_snapshot().await;
    let provider = rbot::observability::collect_provider_model_snapshot(
        &provider_name,
        &model,
        api_base.as_deref(),
    )
    .await;
    let session_manager = rbot::storage::SessionManager::new(&workspace)?;
    let sessions = session_manager.list_session_summaries()?;
    let cron = CronService::new(
        workspace_state_dir(&workspace)
            .join("cron")
            .join("jobs.json"),
    );
    let (cron_running, cron_jobs, next_run) = cron.status()?;

    println!("Workspace: {}", workspace.display());
    println!("Model: {model}");
    println!("Provider: {provider_name}");
    println!(
        "API Base: {}",
        api_base.unwrap_or_else(|| "(default)".to_string())
    );
    println!(
        "Admin UI: http://{}:{}{}",
        config.gateway.host, config.gateway.port, config.gateway.admin.path
    );
    println!(
        "Metrics: http://{}:{}{}",
        config.gateway.host, config.gateway.port, config.gateway.metrics.path
    );
    println!("Sessions: {}", sessions.len());
    println!("Cron Jobs: {cron_jobs}");
    println!("Cron Running: {cron_running}");
    println!(
        "Next Cron Run (ms): {}",
        next_run
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!("CPU Usage: {:.2}%", system.cpu_usage_pct);
    println!(
        "Memory: {} / {}",
        system.used_memory_bytes, system.total_memory_bytes
    );
    println!("Resolved Model ID: {}", provider.model_id);
    println!(
        "Resolved Model Size: {}",
        provider
            .model_size_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );
    if !provider.available_models.is_empty() {
        println!("Available Models: {}", provider.available_models.join(", "));
    }
    Ok(())
}

fn sessions(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let manager = rbot::storage::SessionManager::new(&config.workspace_path())?;
    for session in manager.list_session_summaries()? {
        println!(
            "{} | updated={} | messages={} | consolidated={}",
            session.key, session.updated_at, session.message_count, session.last_consolidated
        );
    }
    Ok(())
}

fn jobs(config_path: Option<&Path>) -> Result<()> {
    let config = Config::load(config_path)?;
    let cron = CronService::new(
        workspace_state_dir(&config.workspace_path())
            .join("cron")
            .join("jobs.json"),
    );
    for job in cron.list_jobs(true)? {
        println!(
            "{} | enabled={} | next_run={:?} | last_status={:?}",
            job.name, job.enabled, job.state.next_run_at_ms, job.state.last_status
        );
    }
    Ok(())
}

struct BuiltAgent {
    agent: AgentLoop,
    model: String,
    provider_name: String,
    workspace: PathBuf,
    cwd: PathBuf,
    session_key: String,
    chat_id: String,
}

enum ReplEvent {
    Input(InputEvent),
    TurnFinished,
}

async fn build_agent(
    config_path: Option<&Path>,
    model_override: Option<String>,
) -> Result<BuiltAgent> {
    let config = Config::load(config_path)?;
    let cwd = std::env::current_dir()?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let model = model_override.unwrap_or_else(|| config.agents.defaults.model.clone());
    let (provider_name, provider_cfg) = config
        .provider_for_model(Some(&model))
        .ok_or_else(|| anyhow!("no configured provider matched model '{model}'"))?;
    let provider = build_provider_client(
        &provider_name,
        &provider_cfg,
        &model,
        config.provider_api_base_for_model(Some(&model)),
        config.tools.web.proxy.as_deref(),
    )?;
    let workspace = resolve_cli_workspace(&config, &cwd);
    sync_workspace_templates(&workspace)?;
    let agent =
        build_agent_for_workspace(&config, &workspace, provider, Some(model.clone()), None).await?;
    let session_key = cli_session_key(&cwd);
    let chat_id = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "direct".to_string());
    Ok(BuiltAgent {
        agent,
        model,
        provider_name,
        workspace,
        cwd,
        session_key,
        chat_id,
    })
}

async fn build_agent_from_config(
    config: &Config,
    provider: SharedProvider,
    model: Option<String>,
    cron_service: Option<CronService>,
) -> Result<AgentLoop> {
    build_agent_for_workspace(
        config,
        &config.workspace_path(),
        provider,
        model,
        cron_service,
    )
    .await
}

async fn build_agent_for_workspace(
    config: &Config,
    workspace: &Path,
    provider: SharedProvider,
    model: Option<String>,
    cron_service: Option<CronService>,
) -> Result<AgentLoop> {
    AgentLoop::new(
        provider,
        workspace,
        model,
        config.agents.defaults.max_tool_iterations,
        config.agents.defaults.context_window_tokens,
        config.agents.defaults.memory_max_bytes,
        config.tools.web.search.clone(),
        config.tools.web.proxy.clone(),
        config.tools.exec.clone(),
        config.tools.restrict_to_workspace,
        cron_service,
        &config.tools.mcp_servers,
    )
    .await
}

fn turn_summary(agent: &AgentLoop, elapsed: std::time::Duration) -> Result<TurnSummary> {
    let snapshot = agent.snapshot()?;
    Ok(TurnSummary {
        prompt_tokens: snapshot.last_prompt_tokens,
        completion_tokens: snapshot.last_completion_tokens,
        elapsed,
    })
}

fn resolve_cli_workspace(config: &Config, cwd: &Path) -> PathBuf {
    let configured = config.workspace_path();
    let default_workspace = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rbot")
        .join("workspace");
    if configured == default_workspace {
        cwd.to_path_buf()
    } else {
        configured
    }
}

fn cli_progress_callback(stream: cli::StreamRenderer) -> MessageSendCallback {
    Arc::new(move |msg: OutboundMessage| {
        let stream = stream.clone();
        Box::pin(async move {
            if msg
                .metadata
                .get("_tool_hint")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                stream.tool_hint(
                    msg.content.trim(),
                    msg.metadata
                        .get("_tool_name")
                        .and_then(serde_json::Value::as_str),
                    msg.metadata.get("_tool_args"),
                );
            }
            Ok(())
        })
    })
}

fn cli_session_key(cwd: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    cwd.to_string_lossy().hash(&mut hasher);
    let project = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    format!("cli:{project}:{:016x}", hasher.finish())
}

fn build_heartbeat_service(
    config: &Config,
    workspace: &Path,
    bus: MessageBus,
    agent_slot: Arc<std::sync::Mutex<Option<Arc<AgentLoop>>>>,
    model: String,
    provider_name: String,
) -> Option<HeartbeatService> {
    if !config.gateway.heartbeat.enabled {
        return None;
    }

    let execute_agent_slot = agent_slot.clone();
    let execute =
        Arc::new(
            move |tasks: String| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String>> + Send>,
            > {
                let execute_agent_slot = execute_agent_slot.clone();
                Box::pin(async move {
                    let Some(agent) = execute_agent_slot
                        .lock()
                        .expect("heartbeat agent slot lock poisoned")
                        .clone()
                    else {
                        return Ok(String::new());
                    };
                    let response = agent
                        .process_inbound(InboundMessage {
                            channel: "system".to_string(),
                            sender_id: "heartbeat".to_string(),
                            chat_id: "heartbeat:heartbeat".to_string(),
                            content: tasks,
                            timestamp: chrono::Utc::now(),
                            media: Vec::new(),
                            metadata: Default::default(),
                            session_key_override: None,
                        })
                        .await?;
                    Ok(response.map(|msg| msg.content).unwrap_or_default())
                })
            },
        );

    let notify_bus = bus.clone();
    let notify = Arc::new(
        move |response: String| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send>,
        > {
            let notify_bus = notify_bus.clone();
            Box::pin(async move {
                if response.trim().is_empty() {
                return Ok(());
            }
            notify_bus
                .publish_outbound(OutboundMessage {
                    channel: "system".to_string(),
                    chat_id: "heartbeat".to_string(),
                    content: response,
                    reply_to: None,
                    media: Vec::new(),
                    metadata: Default::default(),
                })
                .await?;
                Ok(())
            })
        },
    );

    Some(HeartbeatService::new(
        workspace,
        build_provider_client(
            &provider_name,
            config.providers.get(&provider_name)?,
            &model,
            config.provider_api_base_for_model(Some(&model)),
            config.tools.web.proxy.as_deref(),
        )
        .ok()?,
        model,
        Some(execute),
        Some(notify),
        None,
        config.gateway.heartbeat.interval_s,
        true,
    ))
}
