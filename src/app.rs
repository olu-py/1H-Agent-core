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

mod runtime;
use runtime::{
    SnapshotDirection, build_runtime, provider_config_resolver, restore_snapshots,
    session_provider_config,
};
pub(crate) use runtime::{
    activate_session, evict_background_overflow, refresh_sessions, reload_current_session,
};

#[path = "app/commands.rs"]
mod command_ops;
#[cfg(test)]
use command_ops::export_session;
pub(crate) use command_ops::{execute_command, submit_input};

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
    /// The active provider's resolved key, tagged by provider id (not preset:
    /// several custom providers share the `custom` template).
    pub(crate) active_secret: Option<(String, String)>,
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
        if provider_config.id() != config.provider.id() {
            let _ = secrets::api_key_cached(provider_config.preset, provider_config.id());
        }
    }
    let (active_secret, initial_status) =
        match secrets::api_key_cached(config.provider.preset, config.provider.id()) {
            Ok(api_key) => (
                Some((config.provider.id().to_owned(), api_key)),
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

/// Switches the active provider to the saved profile with `id`. Legacy callers
/// that only hold a preset template should pass `preset.key_id()`.
pub(crate) fn apply_provider_choice_by_id(app: &mut App, id: &str) -> Result<()> {
    if id == app.config.provider.id() {
        return Ok(());
    }
    let Some(provider) = app.config.provider_for_id(id) else {
        app.current.status = "供应商连接不存在".into();
        return Ok(());
    };
    let provider_id = provider.id().to_owned();
    let preset = provider.preset;
    let label = provider.display_label().to_owned();
    let api_key = app
        .active_secret
        .as_ref()
        .filter(|(active, _)| active == &provider_id)
        .map(|(_, key)| key.clone())
        .or_else(|| secrets::api_key_cached(preset, &provider_id).ok());
    let Some(api_key) = api_key else {
        app.current.status = format!("{label} 的 API Key 不可用，请在供应商设置中补充");
        return Ok(());
    };

    app.storage.clear_response_id(&app.current.session_id)?;
    app.config.provider = provider;
    app.active_secret = Some((provider_id, api_key));
    stamp_discovered_meta(&mut app.config, &app.storage);
    app.current.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    rebuild_runner(app)?;
    app.current.status = match app.config.save() {
        Ok(()) => format!("已切换到 {label} · {}", app.config.provider.model),
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
        if progress.status.is_terminal() {
            let _ = app
                .storage
                .set_child_status(child_id, progress.status.wire_name());
        }
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
        approval_id: None,
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

    pub(crate) fn take_pending_approval(
        &mut self,
        owner: &str,
        approval_id: &str,
    ) -> Option<PendingApproval> {
        let runtime = self.runtime_mut(owner)?;
        if !runtime
            .pending_approval
            .as_ref()
            .is_some_and(|approval| approval.approval_id.as_deref() == Some(approval_id))
        {
            return None;
        }
        runtime.take_pending_approval()
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
