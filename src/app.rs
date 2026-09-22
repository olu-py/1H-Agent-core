use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::sync::{Mutex, mpsc};

use crate::{
    agent::{AgentEvent, AgentRunner, ChildSessionProgress, ChildSessionStatus},
    commands::{self, AgentMode, Command, TodoCommand},
    config::{Config, ProviderPreset},
    input::InputBuffer,
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    session::{
        EventCtx, SessionRuntime, display_entries, estimate_context_tokens, estimate_used_tokens,
        trim_conversation, trim_conversation_bounded,
    },
    storage::{MemoryRecord, SessionSummary, Storage},
    tools::ToolRegistry,
};

#[path = "app/commands.rs"]
mod command_ops;
pub(crate) use command_ops::{execute_command, submit_input};
#[cfg(test)]
use command_ops::export_session;

pub(crate) use crate::model::ThinkingResult;
pub use crate::model::{
    AgentPhase, ApprovalAction, DisplayContent, DisplayEntry, DisplayKind, ModelPhase,
    PendingApproval, ThinkingDisplay, TodoDisplay, TodoStatus, TodoTask, ToolDisplay,
    ToolDisplayStatus,
};

pub struct App {
    pub workspace: PathBuf,
    pub input: InputBuffer,
    pub context_meter_enabled: bool,
    pub sessions: Vec<SessionSummary>,
    pub child_status: HashMap<String, ChildSessionProgress>,
    pub child_batches: HashMap<String, HashSet<String>>,
    pub(crate) storage: Storage,
    pub(crate) config: Config,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) approval_lock: Arc<Mutex<()>>,
    pub(crate) active_secret: Option<(ProviderPreset, String)>,
    pub(crate) active_session: String,
    pub current: SessionRuntime,
    pub(crate) background: HashMap<String, SessionRuntime>,
    pub(crate) router_tx: mpsc::Sender<RoutedEvent>,
    pub(crate) router_rx: mpsc::Receiver<RoutedEvent>,
    pub(crate) should_quit: bool,
}

/// An agent event tagged with the session it belongs to, so a single channel
/// can route events to any background session in O(1).
pub(crate) struct RoutedEvent {
    pub(crate) session_id: String,
    pub(crate) event: AgentEvent,
}

/// Builds the application state machine (sessions, runtimes, registry, router
/// channel) without touching the terminal. Shared by the TUI event loop and
/// the WebUI server, which only replaces the `router_rx` consumer.
pub(crate) async fn build_app(
    workspace_path: PathBuf,
    mut config: Config,
    storage: Storage,
    session_id: String,
) -> Result<App> {
    let sessions = storage.list_sessions(&workspace_path)?;
    let workspace = Workspace::new(&workspace_path)?;
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
    registry.set_permission_rules(config.permissions.tools.clone());
    registry.set_external_config(config.browser.clone(), config.mcp_servers.clone());
    let _ = registry.initialize_mcp().await;
    let (router_tx, router_rx) = mpsc::channel(256);
    let approval_lock = Arc::new(Mutex::new(()));
    // A restored child session may own a different provider. This is an
    // explicit resume action, so unlock only that one additional credential.
    if let Some(provider_config) = storage
        .session_provider_model(&session_id)
        .ok()
        .and_then(|(provider_id, model)| session_provider_config(&config, &provider_id, &model))
    {
        if provider_config.preset != config.provider.preset {
            let _ = secrets::api_key_cached(provider_config.preset);
        }
    }
    let (active_secret, initial_status) = match secrets::api_key_cached(config.provider.preset) {
        Ok(api_key) => (
            Some((config.provider.preset, api_key)),
            format!(
                "Ready | {} | {}",
                config.provider.preset.label(),
                config.provider.model
            ),
        ),
        Err(secrets::SecretError::Missing(_)) => (None, "需要配置提供商".into()),
        Err(error) => (
            None,
            format!(
                "系统密钥环读取失败：{}",
                secrets::redact(&error.to_string())
            ),
        ),
    };
    let initial_mode = storage
        .session_mode(&session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    registry.set_mode(initial_mode);
    // Rebuild the runtime-discovered metadata for the active provider from
    // the cache before any runtime exists — startup never touches the
    // network, but a previous run's fetch still seeds the window.
    stamp_discovered_meta(&mut config, &storage);
    let mut runtime = build_runtime(
        &storage,
        &config,
        &registry,
        &router_tx,
        &approval_lock,
        active_secret.as_ref(),
        &session_id,
    );
    runtime.status = initial_status;
    Ok(App {
        workspace: workspace_path,
        input: InputBuffer::new(),
        context_meter_enabled: config.ui.context_meter,
        sessions,
        child_status: HashMap::new(),
        child_batches: HashMap::new(),
        storage,
        config,
        registry,
        approval_lock,
        active_secret,
        active_session: session_id,
        current: runtime,
        background: HashMap::new(),
        router_tx,
        router_rx,
        should_quit: false,
    })
}

/// Rebuilds `config.provider.discovered` from the `model_metadata` cache:
/// the provider `/models` row for the active base URL + model first, then
/// the models.dev community row. Pure storage reads — never network. Returns
/// the stamped value so callers can propagate it into live runners.
pub(crate) fn stamp_discovered_meta(
    config: &mut Config,
    storage: &Storage,
) -> Option<crate::model_meta::DiscoveredMeta> {
    let base_url = config.provider.base_url.clone();
    let model = config.provider.model.clone();
    let mut discovered = storage
        .model_metadata(&crate::model_meta::provider_meta_key(&base_url, &model))
        .ok()
        .flatten()
        .filter(|row| row.context_window_tokens.is_some() || row.max_output_tokens.is_some())
        .map(|row| crate::model_meta::DiscoveredMeta {
            source: crate::model_meta::MetaSource::ProviderApi,
            meta: crate::model_meta::ModelMeta {
                context_window_tokens: row.context_window_tokens,
                max_output_tokens: row.max_output_tokens,
            },
            fetched_at: row.fetched_at,
        });
    if discovered.is_none() {
        discovered = storage
            .model_metadata(&crate::model_meta::community_meta_key(&model))
            .ok()
            .flatten()
            .filter(|row| row.context_window_tokens.is_some() || row.max_output_tokens.is_some())
            .map(|row| crate::model_meta::DiscoveredMeta {
                source: crate::model_meta::MetaSource::Community,
                meta: crate::model_meta::ModelMeta {
                    context_window_tokens: row.context_window_tokens,
                    max_output_tokens: row.max_output_tokens,
                },
                fetched_at: row.fetched_at,
            });
    }
    config.provider.discovered = discovered;
    discovered
}

pub(crate) fn apply_provider_choice(app: &mut App, preset: ProviderPreset) -> Result<()> {
    if preset == app.config.provider.preset {
        return Ok(());
    }
    let Some(provider) = app.config.provider_for(preset) else {
        app.current.status = "供应商连接不存在".into();
        return Ok(());
    };
    let api_key = app
        .active_secret
        .as_ref()
        .filter(|(active, _)| *active == preset)
        .map(|(_, key)| key.clone())
        .or_else(|| secrets::api_key_cached(preset).ok());
    let Some(api_key) = api_key else {
        app.current.status = format!("{} 的 API Key 不可用，请在供应商设置中补充", preset.label());
        return Ok(());
    };

    app.storage.clear_response_id(&app.current.session_id)?;
    app.config.provider = provider;
    app.active_secret = Some((preset, api_key));
    stamp_discovered_meta(&mut app.config, &app.storage);
    app.current.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    rebuild_runner(app)?;
    app.current.status = match app.config.save() {
        Ok(()) => format!(
            "已切换到 {} · {}",
            preset.label(),
            app.config.provider.model
        ),
        Err(error) => format!(
            "供应商已切换；配置保存失败：{}",
            secrets::redact(&error.to_string())
        ),
    };
    Ok(())
}

pub(crate) fn apply_model_choice(app: &mut App, model: String) -> Result<()> {
    if model.trim().is_empty() {
        return Ok(());
    }
    app.config.provider.model = model;
    app.config.provider.normalize_thinking();
    stamp_discovered_meta(&mut app.config, &app.storage);
    app.config.upsert_provider(app.config.provider.clone());
    app.current.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    app.storage.clear_response_id(&app.current.session_id)?;
    rebuild_runner(app)?;
    let status = match app.config.save() {
        Ok(()) => format!("模型已设置为 {}", app.config.provider.model),
        Err(error) => format!(
            "模型已更新；配置保存失败：{}",
            secrets::redact(&error.to_string())
        ),
    };
    app.current.status = status;
    Ok(())
}

pub(crate) fn cancel_active_request(app: &mut App) {
    if let Some(approval) = app.current.take_pending_approval() {
        if let ApprovalAction::Agent(reply) = approval.action {
            let _ = reply.send(false);
        }
    }
    // Capture the half-generated assistant text (accumulated in the live
    // entries by TextDelta) and persist it as `assistant_partial` before
    // aborting, so an interrupted stream survives for review.
    if let Some(text) = streaming_assistant_text(&app.current.entries) {
        if !text.trim().is_empty() {
            let _ = app.storage.save_partial(&app.current.session_id, text);
            let _ = app.storage.clear_response_id(&app.current.session_id);
        }
    }
    if let Some(task) = app.current.active_task.take() {
        task.abort();
    }
    app.current.finish_thinking("思考已取消");
    app.current.mark_partial_if_streaming();
    app.current.busy = false;
    app.current.agent_phase = AgentPhase::Idle;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "已取消当前请求".into();
    app.current.push_entry(DisplayEntry {
        kind: DisplayKind::System,
        content: DisplayContent::Markdown("当前请求已取消。".into()),
    });
}

/// The text of the trailing live assistant entry, if one is streaming.
fn streaming_assistant_text(entries: &[DisplayEntry]) -> Option<&str> {
    entries.iter().rev().find_map(|entry| match &entry.content {
        DisplayContent::Markdown(text) if matches!(entry.kind, DisplayKind::Assistant) => {
            Some(text.as_str())
        }
        _ => None,
    })
}

pub(crate) fn rebuild_runner(app: &mut App) -> Result<()> {
    // A rebuilt runner serves a different provider/model contract (new
    // tokenizer and routing), so the usage anchor recorded under the old one
    // is no longer comparable: drop it with the response id.
    app.current.usage_anchor = None;
    let Some((_, api_key)) = &app.active_secret else {
        app.current.runner = None;
        return Ok(());
    };
    let provider = OpenAiClient::new_with_retry(
        app.config.provider.base_url.clone(),
        api_key.clone(),
        app.config.provider.retry_max_attempts,
        app.config.provider.retry_initial_backoff_ms,
        app.config.provider.retry_max_backoff_ms,
    )?
    .with_sse_limits(
        app.config.memory.max_sse_frame_bytes,
        app.config.memory.max_sse_buffer_bytes,
    );
    let child_role = app.current.child_role.clone();
    let child_provider_resolver = provider_config_resolver(&app.config);
    app.current.runner = Some(
        AgentRunner::new(
            provider,
            app.config.provider.clone(),
            app.registry.clone(),
            app.storage.clone(),
            app.current.session_id.clone(),
        )
        .with_cluster_config(app.config.cluster.clone())
        .with_approval_lock(app.approval_lock.clone())
        .with_configured_agents(app.config.agents.clone())
        .with_compaction_config(app.config.compaction.clone())
        .with_memory_config(app.config.memory)
        .with_child_role(child_role)
        .with_child_provider_resolver(child_provider_resolver)
        .with_token_calibration(app.current.token_calibration)
        .with_usage_anchor(app.current.usage_anchor.clone()),
    );
    Ok(())
}

pub(crate) fn start_diff(app: &mut App) -> Result<()> {
    if app.current.busy {
        return Ok(());
    }
    let registry = app.registry.clone();
    let events = app.current.agent_tx.clone();
    app.current.busy = true;
    app.current.status = "正在收集 Git diff…… | Esc 取消".into();
    app.current.active_task = Some(tokio::spawn(async move {
        let call = ToolCall {
            id: format!("diff_{}", uuid::Uuid::new_v4()),
            name: "git".into(),
            arguments: serde_json::json!({"args":["diff","--no-ext-diff","--unified=3"]}),
        };
        let result = registry
            .execute(&call)
            .await
            .unwrap_or_else(|error| error.to_string());
        let _ = events
            .send(AgentEvent::LocalCommandFinished {
                command: "/diff".into(),
                result,
            })
            .await;
    }));
    Ok(())
}

pub(crate) fn handle_routed_event(app: &mut App, routed: RoutedEvent) -> bool {
    let RoutedEvent { session_id, event } = routed;
    let is_active = session_id == app.active_session;
    if let AgentEvent::ChildSessionProgress {
        session_id: child_id,
        progress,
    } = &event
    {
        let previous_batch_finished = app.child_batches.get(&session_id).is_some_and(|children| {
            !children.is_empty()
                && children.iter().all(|child| {
                    app.child_status
                        .get(child)
                        .is_some_and(|progress| progress.status.is_terminal())
                })
        });
        if progress.status == ChildSessionStatus::Queued && previous_batch_finished {
            app.child_batches.remove(&session_id);
        }
        app.child_batches
            .entry(session_id.clone())
            .or_default()
            .insert(child_id.clone());
        app.child_status.insert(child_id.clone(), progress.clone());
        let _ = refresh_sessions(app);
        update_cluster_batch_status(app, &session_id);
        return true;
    }
    let outcome = {
        let ctx = EventCtx {
            storage: &app.storage,
            workspace: &app.workspace,
        };
        if is_active {
            app.current.handle_event(&ctx, event)
        } else if let Some(rt) = app.background.get_mut(&session_id) {
            rt.handle_event(&ctx, event)
        } else {
            return false;
        }
    };
    if outcome.sessions_dirty && refresh_sessions(app).is_err() && is_active {
        app.current.status = "就绪，但刷新会话失败".into();
    }
    if !is_active {
        evict_background_overflow(app);
    }
    is_active || app.has_pending_approval()
}

fn update_cluster_batch_status(app: &mut App, parent_id: &str) {
    let Some(children) = app.child_batches.get(parent_id) else {
        return;
    };
    let total = children.len();
    let completed = children
        .iter()
        .filter(|child| {
            app.child_status
                .get(*child)
                .is_some_and(|progress| progress.status.is_terminal())
        })
        .count();
    let queued = children
        .iter()
        .filter(|child| {
            app.child_status
                .get(*child)
                .is_some_and(|progress| progress.status == ChildSessionStatus::Queued)
        })
        .count();
    let running = total.saturating_sub(completed + queued);
    let status = format!("集群 {completed}/{total} 完成 · {running} 运行 · {queued} 排队");
    if let Some(runtime) = app.runtime_mut(parent_id) {
        runtime.status = status;
    }
}

/// How a pending approval prompt was answered.
pub(crate) fn request_shell_approval(app: &mut App, command: String) -> Result<()> {
    let call = ToolCall {
        id: format!("shell_{}", uuid::Uuid::new_v4()),
        name: "terminal_shell".into(),
        arguments: serde_json::json!({ "command": command }),
    };
    app.current.pending_approval = Some(PendingApproval {
        call,
        reason: "! 命令将通过 workspace Shell 执行".into(),
        source_session_id: None,
        source_title: None,
        action: ApprovalAction::Shell(command),
        created_at: Instant::now(),
    });
    app.current.agent_phase = AgentPhase::WaitingApproval;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "Shell 命令需要确认".into();
    Ok(())
}

pub(crate) fn create_session(app: &mut App) -> Result<()> {
    let session_id = app.storage.create_session(&app.workspace)?;
    activate_session(app, session_id)?;
    refresh_sessions(app)?;
    app.current.status = "新会话已就绪".into();
    Ok(())
}

/// Shared mode-switch entry point for slash commands, palette actions, and
/// clicking the mode label in the input title. It updates UI state,
/// tool permissions, persistence, and clears the provider response id so the
/// next request uses the new mode contract.
pub(crate) fn switch_mode(app: &mut App, mode: AgentMode) -> Result<()> {
    app.current.mode = mode;
    app.registry.set_mode(mode);
    let _ = app
        .storage
        .set_session_mode(&app.current.session_id, mode.as_str());
    app.storage.clear_response_id(&app.current.session_id)?;
    app.current.status = format!("模式已切换为 {}", mode.as_str().to_ascii_uppercase());
    Ok(())
}

/// Resolves the provider configuration used by a spawned child agent, falling
/// back to the session's default provider when the child has none of its own.
fn provider_config_resolver(config: &Config) -> Arc<crate::agent::ChildProviderResolver> {
    let providers = config.providers.clone();
    let default_provider = config.provider.clone();
    Arc::new(
        move |preset: ProviderPreset| -> Result<crate::config::ProviderConfig, String> {
            if let Some(provider) = providers
                .iter()
                .find(|provider| provider.preset == preset)
                .cloned()
            {
                return Ok(provider);
            }
            if preset == default_provider.preset {
                return Ok(default_provider.clone());
            }
            let mut provider_config = preset.defaults();
            provider_config
                .validate()
                .map_err(|error| format!("invalid child provider configuration: {error}"))?;
            Ok(provider_config)
        },
    )
}

/// Resolves a stored provider id/model pair for a session. Child sessions may
/// reference a different provider than the current global setting; in that case
/// the preset defaults are used (and must be valid, e.g. Qwen needs a real
/// workspace URL configured via env or config).
fn session_provider_config(
    config: &Config,
    provider_id: &str,
    model: &str,
) -> Option<crate::config::ProviderConfig> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let preset = ProviderPreset::parse(provider_id)?;
    let mut provider_config = config
        .provider_for(preset)
        .unwrap_or_else(|| preset.defaults());
    provider_config.validate().ok()?;
    provider_config.model = model.to_owned();
    provider_config.normalize_thinking();
    Some(provider_config)
}

/// Builds a fresh `SessionRuntime` for the given session: loads its messages,
/// resolves provider/model (child sessions override the global default), and
/// spawns an event forwarder that routes its agent events to the router.
fn build_runtime(
    storage: &Storage,
    config: &Config,
    registry: &Arc<ToolRegistry>,
    router_tx: &mpsc::Sender<RoutedEvent>,
    approval_lock: &Arc<Mutex<()>>,
    active_secret: Option<&(ProviderPreset, String)>,
    session_id: &str,
) -> SessionRuntime {
    let mut conversation = storage
        .load_messages_bounded(
            session_id,
            config.memory.max_history_items,
            config.memory.max_history_bytes,
            config.memory.max_history_item_bytes,
        )
        .unwrap_or_default();
    trim_conversation_bounded(
        &mut conversation,
        config.memory.max_history_items,
        config.memory.max_history_bytes,
    );
    let todos = storage.list_tasks(session_id).unwrap_or_default();
    let entries = display_entries(&conversation);
    let mode = storage
        .session_mode(session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    let provider_config = storage
        .session_provider_model(session_id)
        .ok()
        .and_then(|(provider_id, model)| session_provider_config(config, &provider_id, &model))
        .unwrap_or_else(|| config.provider.clone());
    let child_role = storage.session_child_role(session_id).ok().flatten();
    let child_provider_resolver = provider_config_resolver(config);
    let runtime_key = active_secret
        .filter(|(preset, _)| *preset == provider_config.preset)
        .map(|(_, api_key)| api_key.clone())
        .or_else(|| secrets::api_key_cached_only(provider_config.preset).ok());
    let runner = runtime_key.as_ref().and_then(|api_key| {
        OpenAiClient::new_with_retry(
            provider_config.base_url.clone(),
            api_key.clone(),
            provider_config.retry_max_attempts,
            provider_config.retry_initial_backoff_ms,
            provider_config.retry_max_backoff_ms,
        )
        .map(|provider| {
            provider.with_sse_limits(
                config.memory.max_sse_frame_bytes,
                config.memory.max_sse_buffer_bytes,
            )
        })
        .ok()
        .map(|provider| {
            AgentRunner::new(
                provider,
                provider_config.clone(),
                registry.clone(),
                storage.clone(),
                session_id.to_owned(),
            )
            .with_cluster_config(config.cluster.clone())
            .with_approval_lock(approval_lock.clone())
            .with_configured_agents(config.agents.clone())
            .with_compaction_config(config.compaction.clone())
            .with_memory_config(config.memory)
            .with_child_role(child_role.clone())
            .with_child_provider_resolver(child_provider_resolver)
        })
    });
    let (agent_tx, agent_rx) = mpsc::channel(128);
    let router = router_tx.clone();
    let sid = session_id.to_owned();
    tokio::spawn(async move {
        let mut receiver = agent_rx;
        while let Some(event) = receiver.recv().await {
            if router
                .send(RoutedEvent {
                    session_id: sid.clone(),
                    event,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    SessionRuntime {
        session_id: session_id.to_owned(),
        status: String::new(),
        entries,
        todos,
        busy: false,
        agent_phase: AgentPhase::Idle,
        model_phase: ModelPhase::Idle,
        thinking_last_line: String::new(),
        thinking_active: false,
        thinking_buffer: String::new(),
        thinking_buffer_truncated: false,
        thinking_buffer_epoch: 0,
        thinking_result: ThinkingResult::Completed,
        usage: Usage::default(),
        context_used_tokens: estimate_context_tokens(&conversation),
        context_limit_tokens: provider_config.resolved_context_window_tokens(),
        token_calibration: 1.0,
        usage_anchor: None,
        metadata_fetch_attempted: false,
        pending_approval: None,
        mode,
        child_role,
        conversation,
        runner,
        agent_tx,
        active_task: None,
        request_seq: 0,
        parked_at: Instant::now(),
    }
}

pub(crate) fn reload_current_session(app: &mut App) -> Result<()> {
    let session_id = app.active_session.clone();
    let active_secret = app.active_secret.clone();
    let target = build_runtime(
        &app.storage,
        &app.config,
        &app.registry,
        &app.router_tx,
        &app.approval_lock,
        active_secret.as_ref(),
        &session_id,
    );
    let mut old = std::mem::replace(&mut app.current, target);
    old.shutdown();
    app.registry.set_mode(app.current.mode);
    app.input.clear();
    Ok(())
}

/// Direction for `restore_snapshots`: `Backward` (undo) writes each file's
/// pre-image (deleting files that did not exist), `Forward` (redo) writes the
/// post-image.
#[derive(Clone, Copy)]
enum SnapshotDirection {
    Backward,
    Forward,
}

/// Rolls the file snapshots recorded on `turn_id` back to disk for undo or
/// forward for redo. Returns a human-readable summary of any files that could
/// not be restored, or `None` when every snapshot applied cleanly.
fn restore_snapshots(app: &mut App, turn_id: &str, direction: SnapshotDirection) -> Option<String> {
    let snapshots = app
        .storage
        .restore_turn_files(&app.current.session_id, turn_id)
        .ok()?;
    let mut problems = Vec::new();
    let mut ordered = snapshots;
    match direction {
        SnapshotDirection::Backward => ordered.reverse(),
        SnapshotDirection::Forward => {}
    }
    for snapshot in ordered {
        let relative = PathBuf::from(&snapshot.path);
        let resolved = app.workspace.join(&relative);
        let (image, existed) = match direction {
            SnapshotDirection::Backward => (
                snapshot.pre_image.as_ref(),
                snapshot.existed && snapshot.pre_image.is_some(),
            ),
            SnapshotDirection::Forward => {
                (snapshot.post_image.as_ref(), snapshot.post_image.is_some())
            }
        };
        let Some(image) = image else {
            if !snapshot.existed {
                // Marker: file exceeded the snapshot limit and was skipped.
                problems.push(format!("{} 超出快照上限，未回滚", snapshot.path));
            }
            continue;
        };
        let write_result = if existed {
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).and_then(|_| std::fs::write(&resolved, image))
            } else {
                std::fs::write(&resolved, image)
            }
        } else {
            let _ = std::fs::remove_file(&resolved);
            Ok(())
        };
        if let Err(error) = write_result {
            problems.push(format!("{}: {error}", snapshot.path));
        }
    }
    if problems.is_empty() {
        None
    } else {
        Some(problems.join("；"))
    }
}

pub(crate) fn activate_session(app: &mut App, session_id: String) -> Result<()> {
    if session_id == app.active_session {
        return Ok(());
    }
    // Explicit resume: the target session may own a different provider than the
    // currently active one. Unlock that provider's key once (cached per
    // provider per process) before building its runtime, so the restored
    // session owns a usable runner even when its key lives only in the keychain.
    if let Some(provider_config) = app
        .storage
        .session_provider_model(&session_id)
        .ok()
        .and_then(|(provider_id, model)| session_provider_config(&app.config, &provider_id, &model))
    {
        let _ = secrets::api_key_cached(provider_config.preset);
    }
    // Pulling a fresh target adds the current runtime to the parked set. Make
    // room before changing active state: unload one idle runtime, but never
    // interrupt a busy background task merely because another session was
    // selected. The caller surfaces this as a conflict and can retry after a
    // task finishes.
    if !app.background.contains_key(&session_id)
        && app.background.len() >= app.config.runtime.max_background_sessions
    {
        let idle_id = app
            .background
            .iter()
            .filter(|(_, runtime)| runtime.idle())
            .min_by_key(|(_, runtime)| runtime.parked_at)
            .map(|(session_id, _)| session_id.clone());
        let Some(idle_id) = idle_id else {
            return Err(anyhow::anyhow!(
                "后台会话容量已满：现有任务均在运行，请等待任务完成后再切换"
            ));
        };
        if let Some(mut runtime) = app.background.remove(&idle_id) {
            runtime.shutdown();
        }
    }
    // Pull the target runtime from the background (preserving any in-flight
    // agent state) or build it fresh; the current runtime is parked so its
    // agent keeps running in the background.
    let active_secret = app.active_secret.clone();
    let target = app.background.remove(&session_id).unwrap_or_else(|| {
        build_runtime(
            &app.storage,
            &app.config,
            &app.registry,
            &app.router_tx,
            &app.approval_lock,
            active_secret.as_ref(),
            &session_id,
        )
    });
    let old_id = app.active_session.clone();
    let mut old = std::mem::replace(&mut app.current, target);
    old.parked_at = Instant::now();
    app.background.insert(old_id, old);
    evict_background_overflow(app);
    app.active_session = session_id;
    app.registry.set_mode(app.current.mode);
    app.input.clear();
    app.current.status = if app.current.runner.is_some() {
        "就绪".into()
    } else {
        "需要配置提供商".into()
    };
    Ok(())
}

pub(crate) fn evict_background_overflow(app: &mut App) {
    let capacity = app.config.runtime.max_background_sessions;
    while app.background.len() > capacity {
        let eviction_id = app
            .background
            .iter()
            .filter(|(_, runtime)| runtime.idle())
            .min_by_key(|(_, runtime)| runtime.parked_at)
            .map(|(session_id, _)| session_id.clone());
        let Some(eviction_id) = eviction_id else {
            break;
        };
        if let Some(mut runtime) = app.background.remove(&eviction_id) {
            runtime.shutdown();
        }
    }
}

pub(crate) fn refresh_sessions(app: &mut App) -> Result<()> {
    app.sessions = app.storage.list_sessions(&app.workspace)?;
    let live_ids = app
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();
    app.child_status
        .retain(|session_id, _| live_ids.contains(session_id.as_str()));
    app.child_batches.retain(|session_id, children| {
        if !live_ids.contains(session_id.as_str()) {
            return false;
        }
        children.retain(|child_id| live_ids.contains(child_id.as_str()));
        !children.is_empty()
    });
    Ok(())
}

impl App {
    pub(crate) fn has_pending_approval(&self) -> bool {
        self.pending_approval().is_some()
    }

    pub(crate) fn pending_approval(&self) -> Option<&PendingApproval> {
        let current = self
            .current
            .pending_approval
            .as_ref()
            .map(|approval| (approval.created_at, approval));
        self.background
            .values()
            .filter_map(|runtime| {
                runtime
                    .pending_approval
                    .as_ref()
                    .map(|approval| (approval.created_at, approval))
            })
            .chain(current)
            .min_by_key(|(created_at, _)| *created_at)
            .map(|(_, approval)| approval)
    }

    pub(crate) fn take_pending_approval_global(&mut self) -> Option<(String, PendingApproval)> {
        let mut owner = self
            .current
            .pending_approval
            .as_ref()
            .map(|approval| (approval.created_at, self.active_session.clone()));
        for (session_id, runtime) in &self.background {
            if let Some(approval) = &runtime.pending_approval {
                if owner
                    .as_ref()
                    .is_none_or(|(created_at, _)| approval.created_at < *created_at)
                {
                    owner = Some((approval.created_at, session_id.clone()));
                }
            }
        }
        let (_, owner) = owner?;
        let approval = if owner == self.active_session {
            self.current.take_pending_approval()
        } else {
            self.background
                .get_mut(&owner)
                .and_then(SessionRuntime::take_pending_approval)
        }?;
        Some((owner, approval))
    }

    pub(crate) fn runtime_mut(&mut self, session_id: &str) -> Option<&mut SessionRuntime> {
        if session_id == self.active_session {
            Some(&mut self.current)
        } else {
            self.background.get_mut(session_id)
        }
    }

    /// Immutable access to a runtime by id (current or background).
    pub(crate) fn runtime(&self, session_id: &str) -> Option<&SessionRuntime> {
        if session_id == self.active_session {
            Some(&self.current)
        } else {
            self.background.get(session_id)
        }
    }
}

#[cfg(test)]
mod tests;
