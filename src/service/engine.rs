use super::*;

/// Commands serialized from consumers into the single state-machine task.
pub(super) enum CoreCommand {
    GetState {
        reply: oneshot::Sender<Result<AppSnapshotV2, ApiError>>,
    },
    GetMessages {
        session_id: String,
        before: Option<i64>,
        limit: usize,
        reply: oneshot::Sender<Result<MessagePage, ApiError>>,
    },
    GetMemories {
        query: Option<String>,
        include_deleted: bool,
        reply: oneshot::Sender<Result<Vec<MemoryDto>, ApiError>>,
    },
    SaveMemory {
        title: String,
        content: String,
        candidate: bool,
        reply: oneshot::Sender<Result<MemoryDto, ApiError>>,
    },
    ConfirmMemory {
        id: i64,
        reply: oneshot::Sender<Result<MemoryDto, ApiError>>,
    },
    UpdateMemory {
        id: i64,
        title: String,
        content: String,
        reply: oneshot::Sender<Result<MemoryDto, ApiError>>,
    },
    DeleteMemory {
        id: i64,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    SubmitInput {
        session_id: Option<String>,
        text: String,
        /// Replies with the request sequence assigned to the new request
        /// (or the current sequence when the input was a command/approval).
        reply: oneshot::Sender<Result<u64, ApiError>>,
    },
    ExecuteCommand {
        session_id: Option<String>,
        text: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Approve {
        approval_id: String,
        accept: bool,
        /// When set, register a session-scoped always-allow rule for the
        /// approved tool before resuming the agent (the TUI "A" key).
        allow_session: bool,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Cancel {
        session_id: String,
        /// The request sequence the caller believes is active. `Some(seq)` that
        /// no longer matches the session's current request is a stale cancel and
        /// is ignored (never aborts a newer request); `None` cancels whatever is
        /// active.
        request_seq: Option<u64>,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    SetProvider {
        provider_id: String,
        model: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    /// Applies a full non-secret provider profile (protocol, base_url, model,
    /// thinking level/budget, context window, retry policy). API keys never
    /// enter the core; the caller stores them in the OS keyring.
    SetProviderConfig {
        provider: crate::config::ProviderConfig,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    /// Applies the settings-screen edit: merges `model` / optional `base_url`
    /// / optional protocol onto the current or saved profile of `preset`
    /// (falling back to the preset template when nothing is saved), then
    /// commits it through the `SetProviderConfig` path.
    SetProviderProfile {
        provider_id: String,
        template: crate::config::ProviderPreset,
        /// New display name for a custom provider; `None` keeps the merged one.
        name: Option<String>,
        model: String,
        base_url: Option<String>,
        kind: Option<crate::config::ProviderKind>,
        /// Optional explicit window override (clamped 4096..=10_000_000);
        /// `None` keeps the merged profile's value.
        context_window_tokens: Option<u64>,
        /// Reserved selectable-model list; `None` keeps the merged value.
        enabled_models: Option<Vec<String>>,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    /// Reads the provider settings view (active + saved profiles, connected
    /// presets). Never includes API keys.
    GetProviderSettings {
        reply: oneshot::Sender<Result<crate::protocol::ProviderSettingsDto, ApiError>>,
    },
    /// Lists the provider's models from the `GET {base_url}/models` cache.
    /// `refresh = true` refetches first (provider endpoint, then models.dev)
    /// when metadata fetching is enabled.
    GetProviderModels {
        refresh: bool,
        reply: oneshot::Sender<Result<crate::protocol::ProviderModelsDto, ApiError>>,
    },
    /// A background metadata fetch finished; the engine re-stamps the active
    /// provider and pushes a fresh context budget. No reply.
    ModelMetadataRefreshed,
    /// Removes a saved provider profile, switching the active provider when it
    /// was the one removed.
    RemoveProvider {
        provider_id: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    ActivateSession {
        session_id: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Shutdown,
}

/// A pending approval: the consumer-facing id plus the deadline after which the
/// engine auto-rejects it. The oneshot sender lives in the runtime's
/// `pending_approval`; only the id crosses the boundary.
pub(super) struct PendingRecord {
    session_id: String,
    deadline: Instant,
}

/// Owns the `App` state machine: runs the router event loop and services
/// serialized commands.
pub(super) struct Engine {
    pub(super) app: App,
    pub(super) bridge: Arc<crate::bridge::EventBridge>,
    pub(super) pending: HashMap<String, PendingRecord>,
    pub(super) approval_timeout: Duration,
    /// Engine-bound clone of the command channel, used by background tasks
    /// (model metadata fetches) to re-enter the state machine.
    pub(super) command_tx: mpsc::Sender<CoreCommand>,
}

impl Engine {
    /// The pending approval with the earliest creation time across all
    /// sessions, along with its engine-side `approval_id`.
    fn oldest_pending(&self) -> Option<(String, String, &PendingApproval)> {
        let mut best: Option<(String, String, &PendingApproval)> = None;
        for (approval_id, record) in &self.pending {
            let runtime = self.app.runtime(&record.session_id)?;
            let Some(approval) = &runtime.pending_approval else {
                continue;
            };
            if approval.approval_id.as_deref() != Some(approval_id) {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, _, current)| approval.created_at < current.created_at)
            {
                best = Some((approval_id.clone(), record.session_id.clone(), approval));
            }
        }
        best
    }

    /// Applies a routed agent event to its owning session, then forwards an
    /// envelope to the bridge. Approval events get a fresh `approval_id` and are
    /// registered in the pending table before the sender is stored in the
    /// runtime.
    fn handle_routed(&mut self, routed: crate::app::RoutedEvent) {
        let crate::app::RoutedEvent { session_id, event } = routed;

        let approval_id = if let AgentEvent::Approval {
            call,
            reason,
            source_session_id,
            source_title,
            ..
        } = &event
        {
            let approval_id = uuid::Uuid::new_v4().to_string();
            self.pending.insert(
                approval_id.clone(),
                PendingRecord {
                    session_id: session_id.clone(),
                    deadline: Instant::now() + self.approval_timeout,
                },
            );
            self.bridge.push(
                session_id.clone(),
                Event::Approval {
                    approval_id: approval_id.clone(),
                    call: call.clone(),
                    reason: reason.clone(),
                    source_session_id: source_session_id.clone(),
                    source_title: source_title.clone(),
                },
            );
            Some(approval_id)
        } else if let Some(event) = routed_to_event(&event) {
            self.bridge.push(session_id.clone(), event);
            None
        } else {
            None
        };

        // The session's context budget changes whenever usage or a terminal
        // turn outcome updates the estimated used tokens; keep the TUI meter
        // authoritative without a history refetch. Computed before the event is
        // moved into the app state machine.
        let context_dirty = matches!(
            &event,
            AgentEvent::Usage { .. }
                | AgentEvent::Completed { .. }
                | AgentEvent::CompactionCompleted { .. }
                | AgentEvent::CompactionFailed(_)
        );
        app::handle_routed_event(
            &mut self.app,
            crate::app::RoutedEvent {
                session_id: session_id.clone(),
                event,
            },
        );
        if let Some(approval_id) = approval_id {
            if let Some(approval) = self
                .app
                .runtime_mut(&session_id)
                .and_then(|runtime| runtime.pending_approval.as_mut())
            {
                approval.approval_id = Some(approval_id);
            }
        }
        if context_dirty {
            push_context_updated(self, &session_id);
        }
    }

    /// Rejects every approval whose deadline has passed, sending `false` to the
    /// agent so it never hangs waiting on an unanswerable prompt.
    fn sweep_expired_approvals(&mut self) {
        let now = Instant::now();
        let expired = self
            .pending
            .iter()
            .filter(|(_, record)| record.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for approval_id in expired {
            let _ = self.resolve_approval(&approval_id, false, false);
        }
    }

    fn clear_session_approvals(&mut self, session_id: &str) {
        let ids = self
            .pending
            .iter()
            .filter(|(_, record)| record.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for approval_id in ids {
            self.pending.remove(&approval_id);
            self.bridge.push(
                session_id.to_owned(),
                Event::ApprovalResolved {
                    approval_id,
                    approved: false,
                },
            );
        }
    }

    /// Resolves an approval: extracts the oneshot sender from the owning
    /// session's runtime and sends the decision. Shell (`!`) approvals reuse
    /// the same flow; an accepted shell command is executed through the
    /// registry.
    fn resolve_approval(
        &mut self,
        approval_id: &str,
        accept: bool,
        allow_session: bool,
    ) -> Result<(), ApiError> {
        let record = self
            .pending
            .get(approval_id)
            .ok_or_else(|| ApiError::not_found(format!("unknown approval {approval_id}")))?;
        let owner = record.session_id.clone();
        let matches = self
            .app
            .runtime(&owner)
            .and_then(|runtime| runtime.pending_approval.as_ref())
            .is_some_and(|approval| approval.approval_id.as_deref() == Some(approval_id));
        if !matches {
            return Err(ApiError::conflict("approval already resolved"));
        }
        let approval = self
            .app
            .take_pending_approval(&owner, approval_id)
            .ok_or_else(|| ApiError::conflict("approval already resolved"))?;
        self.pending.remove(approval_id);

        if accept && allow_session {
            let (tool, prefix, label) = match &approval.action {
                ApprovalAction::Agent(_) => session_allow_for_call(&approval.call),
                ApprovalAction::Shell(command) => (
                    "terminal_shell".to_owned(),
                    Some(command.clone()),
                    command.clone(),
                ),
            };
            self.app
                .registry
                .allow_for_session(&tool, prefix.as_deref());
            if let Some(runtime) = self.app.runtime_mut(&owner) {
                runtime.push_entry(crate::model::DisplayEntry {
                    kind: crate::model::DisplayKind::System,
                    content: crate::model::DisplayContent::Markdown(format!("本会话放行：{label}")),
                });
                runtime.status = format!("本会话已放行 {label}");
            }
        }

        let is_agent = match approval.action {
            ApprovalAction::Agent(reply) => {
                let _ = reply.send(accept);
                if let Some(runtime) = self.app.runtime_mut(&owner) {
                    runtime.agent_phase = if accept {
                        AgentPhase::Thinking
                    } else {
                        AgentPhase::Idle
                    };
                    runtime.model_phase = crate::model::ModelPhase::Idle;
                    runtime.status = if accept {
                        "已批准，开始执行工具……".into()
                    } else {
                        "已拒绝，将结果返回模型……".into()
                    };
                }
                true
            }
            ApprovalAction::Shell(command) => {
                if !accept {
                    if let Some(runtime) = self.app.runtime_mut(&owner) {
                        runtime.agent_phase = AgentPhase::Idle;
                        runtime.status = "Shell 命令已拒绝".into();
                    }
                    true
                } else {
                    let registry = self.app.registry.clone();
                    let Some(runtime) = self.app.runtime_mut(&owner) else {
                        return Ok(());
                    };
                    let events = runtime.agent_tx.clone();
                    runtime.busy = true;
                    runtime.agent_phase = AgentPhase::ToolRunning;
                    runtime.model_phase = crate::model::ModelPhase::Idle;
                    runtime.status = "正在执行 Shell 命令……".into();
                    runtime.active_task = Some(tokio::spawn(async move {
                        let result = registry
                            .execute_shell(&command)
                            .await
                            .unwrap_or_else(|error| error.to_string());
                        let _ = events
                            .send(AgentEvent::LocalCommandFinished { command, result })
                            .await;
                    }));
                    true
                }
            }
        };

        if is_agent {
            self.bridge.push(
                owner.clone(),
                Event::ApprovalResolved {
                    approval_id: approval_id.to_owned(),
                    approved: accept,
                },
            );
        }
        Ok(())
    }

    /// Fully stops every runtime (rejecting approvals and aborting agent tasks),
    /// then resolves any leftover pending approvals.
    fn shutdown(&mut self) {
        let ids = self
            .app
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in ids {
            if let Some(runtime) = self.app.runtime_mut(&session_id) {
                runtime.shutdown();
            }
        }
        let pending = self.pending.keys().cloned().collect::<Vec<_>>();
        for approval_id in pending {
            let _ = self.resolve_approval(&approval_id, false, false);
        }
        self.app.should_quit = true;
    }
}

/// The state-machine event loop: consumes routed agent events and serialized
/// commands, plus a periodic sweep for expired approvals.
pub(super) async fn run_engine(mut engine: Engine, mut command_rx: mpsc::Receiver<CoreCommand>) {
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            routed = engine.app.router_rx.recv() => {
                match routed {
                    Some(routed) => engine.handle_routed(routed),
                    None => break,
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    // All handles were dropped without an explicit shutdown:
                    // clean up so no approvals or agents are left dangling.
                    engine.shutdown();
                    break;
                };
                match command {
                    CoreCommand::Shutdown => {
                        engine.shutdown();
                        break;
                    }
                    command => handle_command(&mut engine, command).await,
                }
            }
            _ = sweep.tick() => engine.sweep_expired_approvals(),
        }
    }
}

async fn handle_command(engine: &mut Engine, command: CoreCommand) {
    match command {
        CoreCommand::GetState { reply } => {
            let _ = reply.send(state_snapshot(engine));
        }
        CoreCommand::GetMessages {
            session_id,
            before,
            limit,
            reply,
        } => {
            let _ = reply.send(message_page(engine, &session_id, before, limit));
        }
        CoreCommand::GetMemories {
            query,
            include_deleted,
            reply,
        } => {
            let workspace = engine.app.workspace.to_string_lossy().into_owned();
            let result = engine
                .app
                .storage
                .list_memories(&workspace, query.as_deref(), include_deleted)
                .map(|records| records.iter().map(memory_dto).collect())
                .map_err(storage_api_error);
            let _ = reply.send(result);
        }
        CoreCommand::SaveMemory {
            title,
            content,
            candidate,
            reply,
        } => {
            let result = save_memory(engine, &title, &content, candidate);
            let _ = reply.send(result);
        }
        CoreCommand::ConfirmMemory { id, reply } => {
            let result = mutate_memory(engine, |storage, workspace, config| {
                storage
                    .confirm_memory(workspace, id)
                    .map(|record| memory_dto(&record))
                    .map_err(|error| memory_api_error(error, config))
            });
            let _ = reply.send(result);
        }
        CoreCommand::UpdateMemory {
            id,
            title,
            content,
            reply,
        } => {
            let result = mutate_memory(engine, |storage, workspace, config| {
                storage
                    .update_memory(
                        workspace,
                        id,
                        &title,
                        &content,
                        config.memory.max_entry_bytes,
                    )
                    .map(|record| memory_dto(&record))
                    .map_err(|error| memory_api_error(error, config))
            });
            let _ = reply.send(result);
        }
        CoreCommand::DeleteMemory { id, reply } => {
            let result = mutate_memory(engine, |storage, workspace, config| {
                storage
                    .delete_memory(workspace, id)
                    .map_err(|error| memory_api_error(error, config))
            });
            let _ = reply.send(result);
        }
        CoreCommand::SubmitInput {
            session_id,
            text,
            reply,
        } => {
            let result = submit_input(engine, session_id.as_deref(), &text);
            let _ = reply.send(result);
        }
        CoreCommand::ExecuteCommand {
            session_id,
            text,
            reply,
        } => {
            let result = execute_command(engine, session_id.as_deref(), &text);
            let _ = reply.send(result);
        }
        CoreCommand::Approve {
            approval_id,
            accept,
            allow_session,
            reply,
        } => {
            let result = engine.resolve_approval(&approval_id, accept, allow_session);
            let _ = reply.send(result);
        }
        CoreCommand::Cancel {
            session_id,
            request_seq,
            reply,
        } => {
            let result = cancel_session(engine, &session_id, request_seq);
            let _ = reply.send(result);
        }
        CoreCommand::SetProvider {
            provider_id,
            model,
            reply,
        } => {
            let result = set_provider(engine, &provider_id, &model);
            let _ = reply.send(result);
        }
        CoreCommand::SetProviderConfig { provider, reply } => {
            let result = set_provider_config(engine, provider);
            let _ = reply.send(result);
        }
        CoreCommand::SetProviderProfile {
            provider_id,
            template,
            name,
            model,
            base_url,
            kind,
            context_window_tokens,
            enabled_models,
            reply,
        } => {
            let result = set_provider_profile(
                engine,
                &provider_id,
                template,
                name,
                &model,
                base_url,
                kind,
                context_window_tokens,
                enabled_models,
            );
            let _ = reply.send(result);
        }
        CoreCommand::GetProviderSettings { reply } => {
            let result = Ok(provider_settings(engine));
            let _ = reply.send(result);
        }
        CoreCommand::GetProviderModels { refresh, reply } => {
            let result = provider_models(engine, refresh).await;
            let _ = reply.send(result);
        }
        CoreCommand::ModelMetadataRefreshed => {
            model_metadata_refreshed(engine);
        }
        CoreCommand::RemoveProvider { provider_id, reply } => {
            let result = remove_provider(engine, &provider_id);
            let _ = reply.send(result);
        }
        CoreCommand::ActivateSession { session_id, reply } => {
            let result = activate_session(engine, &session_id);
            let _ = reply.send(result);
        }
        CoreCommand::Shutdown => unreachable!("Shutdown is handled by the engine loop"),
    }
}

/// Builds the v2 application snapshot.
fn state_snapshot(engine: &Engine) -> Result<AppSnapshotV2, ApiError> {
    let app = &engine.app;
    let sessions = app
        .sessions
        .iter()
        .map(|session| {
            let runtime = app.runtime(&session.id);
            SessionStateDto {
                id: session.id.clone(),
                title: session.title.clone(),
                parent_id: session.parent_id.clone(),
                busy: runtime.map(|r| r.busy).unwrap_or(false),
                phase: runtime
                    .map(|r| r.agent_phase.label().to_owned())
                    .unwrap_or_else(|| AgentPhase::Idle.label().to_owned()),
                status: runtime.map(|r| r.status.clone()).unwrap_or_default(),
                child_status: app
                    .child_status
                    .get(&session.id)
                    .map(|progress| progress.status.wire_name().to_owned())
                    .or_else(|| session.child_status.clone()),
                child_phase: app
                    .child_status
                    .get(&session.id)
                    .and_then(|progress| progress.status.phase_name())
                    .map(str::to_owned),
                child_turn: app
                    .child_status
                    .get(&session.id)
                    .map(|progress| progress.turn),
                child_max_turns: app
                    .child_status
                    .get(&session.id)
                    .map(|progress| progress.max_turns),
                child_tool: app
                    .child_status
                    .get(&session.id)
                    .and_then(|progress| progress.tool.clone()),
            }
        })
        .collect::<Vec<_>>();
    let active_session = if app.sessions.is_empty() || app.active_session.is_empty() {
        None
    } else {
        Some(app.active_session.clone())
    };
    let approval = engine
        .oldest_pending()
        .map(|(approval_id, session_id, approval)| ApprovalDto {
            approval_id,
            session_id,
            call: approval.call.clone(),
            reason: approval.reason.clone(),
            source_session_id: approval.source_session_id.clone(),
            source_title: approval.source_title.clone(),
            created_at_ms: approval.created_at.elapsed().as_millis() as u64,
        });
    let todos = app
        .current
        .todos
        .iter()
        .map(TodoDto::from)
        .collect::<Vec<_>>();
    let context = active_session
        .as_deref()
        .and_then(|session_id| context_budget(engine, session_id));
    let assistant_partial = active_session
        .as_deref()
        .and_then(|session_id| engine.app.storage.load_partial(session_id).ok())
        .flatten()
        .map(|row| protocol::PartialDto {
            content: row.content,
            created_at: row.created_at,
        });
    Ok(AppSnapshotV2 {
        protocol_version: protocol::PROTOCOL_VERSION,
        event_cursor: engine.bridge.current_cursor(),
        active_session,
        sessions,
        provider: app.config.provider.display_label().to_owned(),
        provider_id: app.config.provider.id().to_owned(),
        model: app.config.provider.model.clone(),
        mode: app.current.mode.as_str().to_owned(),
        approval,
        todos,
        context,
        assistant_partial,
    })
}

/// Computes the context budget for a session: window, current usage, output
/// reservation and the resulting safe input budget. The core is the single
/// authority for context capacity. `window_source` reports which metadata
/// tier the window came from (config / provider / community / registry /
/// unknown); `estimated` is true whenever it is not an explicit config
/// value, because discovered tiers can change as fetches land.
fn context_budget(engine: &Engine, session_id: &str) -> Option<ContextBudgetDto> {
    let app = &engine.app;
    let runtime = app.runtime(session_id)?;
    let provider = &app.config.provider;
    let resolved = crate::model_meta::resolve(provider);
    let window = runtime.context_limit_tokens;
    let used = crate::session::estimate_used_tokens(
        runtime.usage_anchor.as_ref(),
        &runtime.conversation,
        runtime.token_calibration,
    )
    .max(runtime.context_used_tokens);
    let reserve = u64::from(resolved.max_output_tokens.unwrap_or(0));
    let window_source = resolved.window_source_tag().to_owned();
    Some(ContextBudgetDto {
        context_window_tokens: window,
        used_tokens: used,
        output_reserve_tokens: reserve,
        safe_input_tokens: window.map(|w| w.saturating_sub(reserve).saturating_sub(used)),
        window_source,
        estimated: resolved.window_source != crate::model_meta::MetaSource::Config,
    })
}

/// Broadcasts a `ContextUpdated` envelope for a session (when it has a
/// runtime), so consumers refresh their safe-input budget.
fn push_context_updated(engine: &Engine, session_id: &str) {
    if let Some(budget) = context_budget(engine, session_id) {
        engine
            .bridge
            .push(session_id.to_owned(), Event::ContextUpdated { budget });
    }
}

/// Fetches a page of a session's transcript along the current head chain.
fn message_page(
    engine: &Engine,
    session_id: &str,
    before: Option<i64>,
    limit: usize,
) -> Result<MessagePage, ApiError> {
    if engine.app.runtime(session_id).is_none()
        && !engine
            .app
            .sessions
            .iter()
            .any(|session| session.id == session_id)
    {
        return Err(ApiError::not_found(format!("unknown session {session_id}")));
    }
    let rows = engine
        .app
        .storage
        .load_message_page(session_id, before, limit + 1)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let has_more = rows.len() > limit;
    let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
    // Rows come back newest-first; restore display (oldest→newest) order. The
    // opaque `next_before` cursor is the id of the oldest message in the page
    // (the last row before the reversal), so the previous page is id < that.
    let messages = rows
        .iter()
        .rev()
        .map(stored_to_message_dto)
        .collect::<Vec<_>>();
    let next_before = rows.last().map(|row| row.id);
    Ok(MessagePage {
        messages,
        next_before,
        has_more,
    })
}

/// Maps a stored message row to its display-safe v2 DTO. Provider-private
/// payloads (`provider_item`) are translated to a display-safe shape and never
/// leaked as raw JSON.
pub(super) fn stored_to_message_dto(row: &StoredMessage) -> MessageDto {
    let id = row.id;
    let created_at = row.created_at.clone();
    match row.kind.as_str() {
        "message" => match row.role.as_str() {
            "user" => MessageDto::User {
                id,
                content: row.content.clone(),
                created_at,
            },
            "assistant" => MessageDto::Assistant {
                id,
                content: row.content.clone(),
                created_at,
            },
            _ => MessageDto::System {
                id,
                content: row.content.clone(),
                created_at,
            },
        },
        "context" => MessageDto::Context {
            id,
            label: row.metadata.clone().unwrap_or_else(|| "context".into()),
            content: row.content.clone(),
            created_at,
        },
        "thinking_summary" => MessageDto::Thinking {
            id,
            content: row.content.clone(),
            created_at,
        },
        "compaction_summary" => MessageDto::CompactionSummary {
            id,
            content: row.content.clone(),
            created_at,
        },
        "provider_item" => match serde_json::from_str::<serde_json::Value>(&row.content) {
            Ok(item)
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("web_search_call") =>
            {
                MessageDto::Tool {
                    id,
                    call_id: item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("web-search-{id}")),
                    name: "web_search".into(),
                    arguments: item
                        .get("action")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    status: "completed".into(),
                    result: None,
                    created_at,
                }
            }
            _ => MessageDto::System {
                id,
                content: "（已归档的模型内部调用）".into(),
                created_at,
            },
        },
        "tool_calls" => {
            let calls = serde_json::from_str::<Vec<ToolCall>>(&row.content).unwrap_or_default();
            MessageDto::ToolCalls {
                id,
                calls,
                created_at,
            }
        }
        "tool_output" => MessageDto::ToolOutput {
            id,
            call_id: row.metadata.clone().unwrap_or_default(),
            output: row.content.clone(),
            created_at,
        },
        _ => MessageDto::System {
            id,
            content: row.content.clone(),
            created_at,
        },
    }
}

/// Converts a routed agent event into its protocol shape. Approval events are
/// handled separately (they need an engine-side `approval_id`).
///
/// `pub(crate)` so the `test-util` conformance module can drive the real
/// `AgentEvent -> Event` mapping in its contract tests; still invisible to
/// external consumers.
pub(crate) fn routed_to_event(event: &AgentEvent) -> Option<Event> {
    Some(match event {
        AgentEvent::ReasoningDelta(delta) => Event::ReasoningDelta {
            delta: delta.clone(),
        },
        AgentEvent::ReasoningCompleted => Event::ReasoningCompleted,
        AgentEvent::ProviderRetry {
            attempt,
            reason,
            delay_ms,
        } => Event::ProviderRetry {
            attempt: *attempt,
            reason: reason.clone(),
            delay_ms: *delay_ms,
        },
        AgentEvent::ModelStreaming => Event::ModelStreaming,
        AgentEvent::WebSearchStarted { query } => Event::WebSearchStarted {
            query: query.clone(),
        },
        AgentEvent::WebSearchResult {
            title,
            url,
            snippet,
        } => Event::WebSearchResult {
            title: title.clone(),
            url: url.clone(),
            snippet: snippet.clone(),
        },
        AgentEvent::WebSearchCompleted { count } => Event::WebSearchCompleted { count: *count },
        AgentEvent::Cancelled(reason) => Event::Cancelled {
            reason: reason.clone(),
        },
        AgentEvent::TextDelta(delta) => Event::TextDelta {
            delta: delta.clone(),
        },
        AgentEvent::ToolCallStreaming {
            name,
            received_bytes,
        } => Event::ToolCallStreaming {
            name: name.clone(),
            received_bytes: *received_bytes,
        },
        AgentEvent::Approval { .. } => return None,
        AgentEvent::ToolStarted(call) => Event::ToolStarted { call: call.clone() },
        AgentEvent::ToolFinished { call, result } => Event::ToolFinished {
            call: call.clone(),
            result: result.clone(),
        },
        AgentEvent::Usage { usage, .. } => Event::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        },
        AgentEvent::Completed { .. } => Event::Completed,
        AgentEvent::Failed(error) => Event::Failed {
            error: error.clone(),
        },
        AgentEvent::SessionsChanged => Event::SessionsChanged,
        AgentEvent::ChildSessionProgress {
            session_id: child_id,
            progress,
        } => Event::ChildSessionProgress {
            child_session_id: child_id.clone(),
            status: progress.status.wire_name().to_owned(),
            phase: progress.status.phase_name().map(str::to_owned),
            turn: progress.turn,
            max_turns: progress.max_turns,
            tool: progress.tool.clone(),
        },
        AgentEvent::LocalCommandFinished { command, result } => Event::LocalCommandFinished {
            command: command.clone(),
            result: result.clone(),
        },
        AgentEvent::CompactionStarted => Event::CompactionStarted,
        AgentEvent::CompactionCompleted { hidden } => {
            Event::CompactionCompleted { hidden: *hidden }
        }
        AgentEvent::CompactionFailed(error) => Event::CompactionFailed {
            error: error.clone(),
        },
        AgentEvent::TodoUpdated { tasks } => Event::TodoUpdated {
            tasks: tasks.clone(),
        },
    })
}

/// Submits user input to a session, creating it first when `None` (the home
/// screen "first message creates a session" semantic). Returns the request
/// sequence of the new request (the current sequence for command/approval
/// inputs, which do not start an agent task).
fn submit_input(
    engine: &mut Engine,
    session_id: Option<&str>,
    text: &str,
) -> Result<u64, ApiError> {
    let session_id = ensure_session(engine, session_id)?;
    // The target session is authoritative: when a caller submits to a session
    // that is not the engine's active one, route to it instead of mutating the
    // engine's current session.
    if session_id != engine.app.active_session {
        activate_session(engine, &session_id)?;
    }
    if text.starts_with('/') {
        let app = &mut engine.app;
        if let Some(command) = commands::parse(text) {
            let outcome = CommandOutcome::from_command(&command);
            app::execute_command(app, command).map_err(api_error)?;
            let request_seq = app.current.request_seq;
            sync_state_after_command(engine, &session_id, outcome);
            return Ok(request_seq);
        }
        let request_seq = app.current.request_seq;
        app.current.push_entry(crate::model::DisplayEntry {
            kind: crate::model::DisplayKind::Error,
            content: crate::model::DisplayContent::Markdown(format!(
                "未知命令，请使用 /help 查看命令：{text}"
            )),
        });
        return Ok(request_seq);
    }
    if text.starts_with('!') {
        let command = text.strip_prefix('!').unwrap_or(text).trim().to_owned();
        let app = &mut engine.app;
        app::request_shell_approval(app, command.clone()).map_err(api_error)?;
        let request_seq = app.current.request_seq;
        // The shell approval is created directly on the runtime (no AgentEvent
        // round trip), so register an id and broadcast it ourselves.
        register_shell_approval(engine, &command);
        return Ok(request_seq);
    }
    // Normal conversation: strict pre-submit checks against the target session.
    // A concurrent submit must never overwrite `active_task`.
    let app = &mut engine.app;
    {
        let runtime = app
            .runtime(&session_id)
            .ok_or_else(|| ApiError::conflict("session runtime is unavailable"))?;
        if runtime.busy || runtime.active_task.is_some() {
            return Err(ApiError::conflict("session is busy with another request"));
        }
        if runtime.pending_approval.is_some() {
            return Err(ApiError::conflict("session is waiting for an approval"));
        }
        if runtime.runner.is_none() {
            return Err(ApiError::conflict(
                "provider/API key is not configured; open the Provider settings",
            ));
        }
    }
    // The target session is the active one here; stamp the new request before
    // `submit` starts its task.
    app.current.request_seq = app.current.request_seq.wrapping_add(1);
    let request_seq = app.current.request_seq;
    app.input.set(text.to_owned());
    app::submit_input(app).map_err(api_error)?;
    // Lazy one-shot metadata fetch: the first submit with an unknown window
    // triggers exactly one background refresh per session (explicit config
    // and fresh cache rows short-circuit it inside the spawner).
    if !app.current.metadata_fetch_attempted && app.current.context_limit_tokens.is_none() {
        spawn_model_metadata_refresh(engine);
    }
    // Submitting a message changes the used-token estimate; refresh the
    // safe-input budget for the target session.
    push_context_updated(engine, &session_id);
    Ok(request_seq)
}

/// Registers a shell (`!`) approval that was created directly on the runtime:
/// assigns an id, records the deadline, and broadcasts the approval envelope.
fn register_shell_approval(engine: &mut Engine, _command: &str) {
    let Some(approval) = &engine.app.current.pending_approval else {
        return;
    };
    let call = approval.call.clone();
    let reason = approval.reason.clone();
    let source_session_id = approval.source_session_id.clone();
    let source_title = approval.source_title.clone();
    let session_id = engine.app.active_session.clone();
    let approval_id = uuid::Uuid::new_v4().to_string();
    if let Some(approval) = engine.app.current.pending_approval.as_mut() {
        approval.approval_id = Some(approval_id.clone());
    }
    engine.pending.insert(
        approval_id.clone(),
        PendingRecord {
            session_id: session_id.clone(),
            deadline: Instant::now() + engine.approval_timeout,
        },
    );
    engine.bridge.push(
        session_id.clone(),
        Event::Approval {
            approval_id,
            call,
            reason,
            source_session_id,
            source_title,
        },
    );
}

/// Executes a slash command against a session.
fn execute_command(
    engine: &mut Engine,
    session_id: Option<&str>,
    text: &str,
) -> Result<(), ApiError> {
    let session_id = ensure_session(engine, session_id)?;
    let app = &mut engine.app;
    let Some(command) = commands::parse(text) else {
        return Err(ApiError::bad_request(format!("unknown command: {text}")));
    };
    let outcome = CommandOutcome::from_command(&command);
    app::execute_command(app, command).map_err(api_error)?;
    sync_state_after_command(engine, &session_id, outcome);
    Ok(())
}

/// Which post-command sync events to broadcast.
#[derive(Clone, Copy, Default)]
struct CommandOutcome {
    todo_changed: bool,
    transcript_invalidated: bool,
    deleted_session: bool,
}

impl CommandOutcome {
    fn from_command(command: &Command) -> Self {
        Self {
            todo_changed: matches!(command, Command::Todo(_)),
            deleted_session: matches!(command, Command::Delete),
            // History-modifying commands invalidate the cached transcript.
            transcript_invalidated: matches!(
                command,
                Command::NewSession
                    | Command::Rename(_)
                    | Command::Delete
                    | Command::Fork
                    | Command::Undo
                    | Command::Redo
                    | Command::Compact(_)
                    | Command::Uncompact
            ),
        }
    }
}

/// Pushes the DTOs the frontend needs to stay in sync after a command that the
/// core logic handled entirely in-process (no `AgentEvent` round trip):
/// a `TodoUpdated` for todo mutations, a `TranscriptInvalidated` for
/// history-modifying commands, and a `SessionsChanged` so the consumer
/// refreshes its sidebar and clears transient "发送中…" state.
fn sync_state_after_command(engine: &mut Engine, session_id: &str, outcome: CommandOutcome) {
    if outcome.deleted_session {
        engine.clear_session_approvals(session_id);
    }
    if outcome.todo_changed {
        let tasks = engine
            .app
            .runtime(session_id)
            .map(|runtime| runtime.todos.clone())
            .unwrap_or_default();
        engine
            .bridge
            .push(session_id.to_owned(), Event::TodoUpdated { tasks });
    }
    if outcome.transcript_invalidated {
        engine
            .bridge
            .push(session_id.to_owned(), Event::TranscriptInvalidated);
    }
    engine
        .bridge
        .push(session_id.to_owned(), Event::SessionsChanged);
}

/// Ensures the target session exists, creating and activating it when no id is
/// given (or when the special token `"new"` is used).
fn ensure_session(engine: &mut Engine, session_id: Option<&str>) -> Result<String, ApiError> {
    match session_id {
        Some(id) if !id.is_empty() && id != "new" => {
            if engine.app.runtime(id).is_none()
                && !engine.app.sessions.iter().any(|session| session.id == id)
            {
                return Err(ApiError::not_found(format!("unknown session {id}")));
            }
            Ok(id.to_owned())
        }
        _ => {
            app::create_session(&mut engine.app).map_err(api_error)?;
            Ok(engine.app.active_session.clone())
        }
    }
}

/// Switches the engine-side active session (the runtime whose events are routed
/// to the current view).
fn activate_session(engine: &mut Engine, session_id: &str) -> Result<(), ApiError> {
    if !engine
        .app
        .sessions
        .iter()
        .any(|session| session.id == session_id)
    {
        return Err(ApiError::not_found(format!("unknown session {session_id}")));
    }
    app::activate_session(&mut engine.app, session_id.to_owned()).map_err(api_error)?;
    engine
        .bridge
        .push(session_id.to_owned(), Event::SessionsChanged);
    Ok(())
}

fn cancel_session(
    engine: &mut Engine,
    session_id: &str,
    request_seq: Option<u64>,
) -> Result<(), ApiError> {
    if session_id != engine.app.active_session {
        return Err(ApiError::conflict(
            "only the active session can be cancelled",
        ));
    }
    // A stale cancel (a sequence that no longer matches the session's current
    // request) must not abort a newer request; ignore it silently.
    if let Some(seq) = request_seq {
        let Some(runtime) = engine.app.runtime(session_id) else {
            return Err(ApiError::not_found("session not found"));
        };
        if runtime.request_seq != seq {
            return Ok(());
        }
    }
    app::cancel_active_request(&mut engine.app);
    engine.clear_session_approvals(session_id);
    // `cancel_active_request` mutates the runtime in place without an
    // `AgentEvent` round trip, so broadcast the terminal event ourselves.
    engine.bridge.push(
        session_id.to_owned(),
        Event::Cancelled {
            reason: "user".into(),
        },
    );
    Ok(())
}

/// Applies non-secret provider settings: switches the provider (by stable id)
/// when given and sets the model. API keys are never handled here (they stay in
/// the keyring). A bare built-in preset name resolves to its own id, so legacy
/// callers keep working.
fn set_provider(engine: &mut Engine, provider_id: &str, model: &str) -> Result<(), ApiError> {
    // Resolve the target profile first so an unknown id is rejected up front.
    let target = engine
        .app
        .config
        .provider_for_id(provider_id)
        .or_else(|| {
            crate::config::ProviderPreset::parse(provider_id)
                .and_then(|preset| engine.app.config.provider_for(preset))
        })
        .ok_or_else(|| ApiError::bad_request(format!("unknown provider {provider_id}")))?;
    let target_id = target.id().to_owned();
    let target_label = target.display_label().to_owned();
    let current_id = engine.app.config.provider.id().to_owned();
    if target_id != current_id {
        app::apply_provider_choice_by_id(&mut engine.app, &target_id).map_err(api_error)?;
        // `apply_provider_choice_by_id` reports an unavailable key as a status,
        // not an error. Detect an unchanged provider and refuse to apply the
        // model so we never leave an inconsistent provider/model pair.
        if engine.app.config.provider.id() != target_id {
            return Err(ApiError::bad_request(format!(
                "{target_label} 的 API Key 不可用"
            )));
        }
    }
    if !model.is_empty() && model != engine.app.config.provider.model {
        app::apply_model_choice(&mut engine.app, model.to_owned()).map_err(api_error)?;
    }
    // Model switches and provider switches are both metadata fetch triggers.
    spawn_model_metadata_refresh(engine);
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Applies a full non-secret provider profile: switches the preset, protocol,
/// base_url, model and thinking settings, upserts the saved profile, rebuilds
/// the runner and persists the config. API keys are never handled here (the
/// caller stores them in the OS keyring before calling).
fn set_provider_config(
    engine: &mut Engine,
    mut provider: crate::config::ProviderConfig,
) -> Result<(), ApiError> {
    provider.ensure_id();
    let preset = provider.preset;
    let provider_id = provider.id().to_owned();
    provider
        .validate()
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    // New custom providers must carry a non-empty, unique name; built-ins and
    // legacy unnamed custom profiles may keep an empty name (label fallback).
    if preset == crate::config::ProviderPreset::Custom && provider.name.trim().is_empty() {
        return Err(ApiError::bad_request("自定义供应商名称不能为空"));
    }
    if engine
        .app
        .config
        .provider_name_taken(&provider.name, Some(&provider_id))
    {
        return Err(ApiError::bad_request(format!(
            "供应商名称 \"{}\" 已被占用",
            provider.name.trim()
        )));
    }
    let label = provider.display_label().to_owned();
    let model = provider.model.clone();
    provider.normalize_thinking();
    // Refresh the active secret whenever it does not match the incoming
    // provider id - a provider switch, or the first key arriving for the
    // current provider - so the rebuilt runner never pairs one provider's key
    // with another provider's base URL, and a newly stored key takes effect
    // without a restart. `api_key_cached` resolves environment variables first
    // (by family) and reads the keyring at most once per process and id.
    if engine.app.config.provider.id() != provider_id
        || !matches!(&engine.app.active_secret, Some((active, _)) if *active == provider_id)
    {
        engine.app.active_secret = crate::secrets::api_key_cached(preset, &provider_id)
            .ok()
            .map(|key| (provider_id.clone(), key));
    }
    let has_key = engine.app.active_secret.is_some();
    engine.app.config.provider = provider.clone();
    // The incoming profile never carries runtime metadata (the field does not
    // serialize); re-stamp from the cache before resolving the window so a
    // previous run's fetch still applies to the new base URL/model.
    app::stamp_discovered_meta(&mut engine.app.config, &engine.app.storage);
    engine.app.config.upsert_provider(provider);
    engine.app.current.context_limit_tokens =
        engine.app.config.provider.resolved_context_window_tokens();
    // Force a fresh provider context so the next request uses the new
    // provider's contract instead of resuming a foreign response id.
    engine
        .app
        .storage
        .clear_response_id(&engine.app.current.session_id)
        .map_err(|error| api_error(error.into()))?;
    app::rebuild_runner(&mut engine.app).map_err(api_error)?;
    let status = match engine.app.config.save() {
        Ok(()) if has_key => format!("就绪 | {label} | {model}"),
        Ok(()) => "需要配置提供商".into(),
        Err(error) => {
            let suffix = crate::secrets::redact(&error.to_string());
            if has_key {
                format!("配置已应用，但保存失败：{suffix}")
            } else {
                format!("配置已应用（缺 API Key），但保存失败：{suffix}")
            }
        }
    };
    engine.app.current.status = status;
    // Event-driven metadata refresh: a provider switch is a fetch trigger.
    // Best effort, fully async — the reply never waits on the network.
    spawn_model_metadata_refresh(engine);
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Applies a settings-screen provider edit: merges the model and the optional
/// base URL / protocol / explicit context window onto the base profile for
/// `provider_id` - the current profile when it is already active (keeping
/// thinking, retry and context customizations), otherwise the saved profile
/// or a fresh template - then commits it via [`set_provider_config`].
/// `template` supplies the family defaults for a brand-new profile. A provided
/// window is clamped to the same bounds `Config::load` enforces, so the
/// settings screen can never install an out-of-bounds window.
#[allow(clippy::too_many_arguments)]
fn set_provider_profile(
    engine: &mut Engine,
    provider_id: &str,
    template: crate::config::ProviderPreset,
    name: Option<String>,
    model: &str,
    base_url: Option<String>,
    kind: Option<crate::config::ProviderKind>,
    context_window_tokens: Option<u64>,
    enabled_models: Option<Vec<String>>,
) -> Result<(), ApiError> {
    // Empty id + the `custom` template is the create path: mint a fresh id so
    // several custom providers can coexist. Built-ins keep their preset key.
    let creating = provider_id.trim().is_empty();
    let mut profile = if creating {
        template.defaults()
    } else if engine.app.config.provider.id() == provider_id {
        engine.app.config.provider.clone()
    } else {
        engine
            .app
            .config
            .provider_for_id(provider_id)
            .unwrap_or_else(|| template.defaults())
    };
    if creating {
        profile.id = if template == crate::config::ProviderPreset::Custom {
            crate::config::ProviderConfig::new_custom_id()
        } else {
            template.key_id().to_owned()
        };
    } else {
        profile.id = provider_id.to_owned();
    }
    profile.preset = template;
    // `name` is only meaningful for named custom providers; built-ins fall back
    // to the preset label and must not overwrite it with a stale name.
    if let Some(name) = name {
        profile.name = name;
    }
    if let Some(models) = enabled_models {
        profile.enabled_models = models;
    }
    profile.model = model.trim().to_owned();
    if let Some(base_url) = base_url {
        if !base_url.trim().is_empty() {
            profile.base_url = base_url.trim().to_owned();
        }
    }
    if let Some(kind) = kind {
        profile.kind = kind;
    }
    if let Some(window) = context_window_tokens {
        profile.context_window_tokens = Some(window.clamp(
            crate::model_meta::MIN_CONTEXT_WINDOW_TOKENS,
            crate::model_meta::MAX_CONTEXT_WINDOW_TOKENS,
        ));
    }
    set_provider_config(engine, profile)
}

/// Builds the provider settings view. `connected` uses cache-only key lookups
/// by provider id (startup unlock, environment preload, keys stored this run)
/// so answering a settings read never touches the OS keyring.
fn provider_settings(engine: &Engine) -> crate::protocol::ProviderSettingsDto {
    let app = &engine.app;
    let mut profiles: Vec<&crate::config::ProviderConfig> = app.config.providers.iter().collect();
    if !profiles.iter().any(|p| p.id() == app.config.provider.id()) {
        profiles.push(&app.config.provider);
    }
    let connected = profiles
        .iter()
        .filter(|provider| {
            crate::secrets::api_key_cached_only(provider.preset, provider.id()).is_ok()
        })
        .map(|provider| provider.id().to_owned())
        .collect::<Vec<_>>();
    crate::protocol::ProviderSettingsDto {
        active: provider_profile_dto(&app.config.provider),
        saved: app
            .config
            .providers
            .iter()
            .map(provider_profile_dto)
            .collect(),
        connected,
    }
}

/// Maps a [`crate::config::ProviderConfig`] onto its non-secret wire form.
fn provider_profile_dto(
    provider: &crate::config::ProviderConfig,
) -> crate::protocol::ProviderProfileDto {
    crate::protocol::ProviderProfileDto {
        id: provider.id().to_owned(),
        preset: provider.preset.key_id().to_owned(),
        name: provider.name.clone(),
        kind: provider.kind.wire_tag().to_owned(),
        model: provider.model.clone(),
        base_url: provider.base_url.clone(),
        enabled_models: provider.enabled_models.clone(),
    }
}

/// Lists the provider's models for pickers. `refresh = false` answers from
/// the cached `provider-list|{base_url}` row (instant, offline-safe);
/// `true` performs a bounded inline fetch first (explicit user action, so
/// waiting up to the metadata timeout is acceptable) when
/// `[model_metadata] fetch` is enabled and a key is cached. The DTO carries
/// metadata only — never the API key.
async fn provider_models(
    engine: &mut Engine,
    refresh: bool,
) -> Result<crate::protocol::ProviderModelsDto, ApiError> {
    if refresh {
        run_model_metadata_refresh(engine).await;
    }
    let base_url = engine.app.config.provider.base_url.clone();
    let row = engine
        .app
        .storage
        .model_metadata(&crate::model_meta::provider_list_key(&base_url))
        .ok()
        .flatten();
    let models = row
        .as_ref()
        .and_then(|row| row.payload.as_deref())
        .and_then(|payload| {
            serde_json::from_str::<Vec<crate::provider::ProviderModelInfo>>(payload).ok()
        })
        .unwrap_or_default();
    // Community fallback: entries the provider endpoint does not describe pick
    // up their models.dev row (the same exact-key semantics the active
    // model's L3 lookup uses), so pickers and fetch buttons see one merged
    // view instead of a list that hides known community windows.
    let models = models
        .into_iter()
        .map(|mut model| {
            if model.context_window_tokens.is_none() || model.max_output_tokens.is_none() {
                if let Ok(Some(community)) = engine
                    .app
                    .storage
                    .model_metadata(&crate::model_meta::community_meta_key(&model.id))
                {
                    if model.context_window_tokens.is_none() {
                        model.context_window_tokens = community.context_window_tokens;
                    }
                    if model.max_output_tokens.is_none() {
                        model.max_output_tokens = community.max_output_tokens;
                    }
                }
            }
            crate::protocol::ProviderModelDto {
                id: model.id,
                context_window_tokens: model.context_window_tokens,
                max_output_tokens: model.max_output_tokens,
            }
        })
        .collect();
    Ok(crate::protocol::ProviderModelsDto {
        models,
        fetched_at: row.map(|row| row.fetched_at),
    })
}

/// Performs the metadata fetches for one provider endpoint — `GET
/// {base_url}/models` (cached as the model list plus per-model rows) and
/// models.dev (community rows) — and persists them. Every failure is
/// swallowed: metadata is best effort and the snapshot/registry tiers
/// remain the fallback. Returns whether any rows were written.
async fn fetch_and_cache_metadata(
    client: &crate::provider::OpenAiClient,
    base_url: &str,
    timeout_ms: u64,
    storage: &Storage,
) -> bool {
    let mut written = false;
    if let Ok(models) = client.list_models(timeout_ms).await {
        for model in &models {
            if model.context_window_tokens.is_some() || model.max_output_tokens.is_some() {
                let _ = storage.save_model_metadata(
                    &crate::model_meta::provider_meta_key(base_url, &model.id),
                    "provider",
                    model.context_window_tokens,
                    model.max_output_tokens,
                    None,
                );
            }
        }
        if let Ok(payload) = serde_json::to_string(&models) {
            let _ = storage.save_model_metadata(
                &crate::model_meta::provider_list_key(base_url),
                "provider",
                None,
                None,
                Some(&payload),
            );
        }
        written = true;
    }
    if let Ok(table) = crate::model_meta::fetch_community_models(timeout_ms).await {
        let rows = table
            .into_iter()
            .map(|(model, meta)| {
                (
                    crate::model_meta::community_meta_key(&model),
                    meta.context_window_tokens,
                    meta.max_output_tokens,
                )
            })
            .collect::<Vec<_>>();
        if storage
            .save_model_metadata_batch("community", &rows)
            .is_ok()
        {
            written = written || !rows.is_empty();
        }
    }
    written
}

/// Whether a cached row is newer than the configured TTL.
fn metadata_row_is_fresh(engine: &Engine, key: &str) -> bool {
    // `ttl_hours` is clamped to 1..=168 at load, so the cast cannot overflow.
    let ttl_secs = engine.app.config.model_metadata.ttl_hours as i64 * 3600;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    engine
        .app
        .storage
        .model_metadata(key)
        .ok()
        .flatten()
        .is_some_and(|row| row.fetched_at.saturating_add(ttl_secs) >= now)
}

/// Spawns the background metadata fetch for the active provider (provider
/// switch / model switch triggers). Skipped silently when fetching is
/// disabled, no API key is cached, or a fresh cached row already covers the
/// active model (TTL gates the *attempt* only — cached values stay usable).
/// The reply path never waits on this task.
fn spawn_model_metadata_refresh(engine: &Engine) {
    if !engine.app.config.model_metadata.fetch {
        return;
    }
    let Some((_, api_key)) = engine.app.active_secret.as_ref() else {
        return;
    };
    let base_url = engine.app.config.provider.base_url.clone();
    let model = engine.app.config.provider.model.clone();
    if metadata_row_is_fresh(
        engine,
        &crate::model_meta::provider_meta_key(&base_url, &model),
    ) || metadata_row_is_fresh(engine, &crate::model_meta::community_meta_key(&model))
    {
        return;
    }
    let Ok(client) = crate::provider::OpenAiClient::new(base_url.clone(), api_key.clone()) else {
        return;
    };
    let timeout_ms = engine.app.config.model_metadata.timeout_ms;
    let storage = engine.app.storage.clone();
    let command_tx = engine.command_tx.clone();
    tokio::spawn(async move {
        fetch_and_cache_metadata(&client, &base_url, timeout_ms, &storage).await;
        let _ = command_tx.try_send(CoreCommand::ModelMetadataRefreshed);
    });
}

/// Explicit refresh (settings screen): fetches inline — bounded by the
/// metadata timeout — then re-stamps the active provider. Disabled or
/// keyless setups fall through to the cache read.
async fn run_model_metadata_refresh(engine: &mut Engine) {
    if !engine.app.config.model_metadata.fetch {
        return;
    }
    let Some((_, api_key)) = engine.app.active_secret.clone() else {
        return;
    };
    let base_url = engine.app.config.provider.base_url.clone();
    let Ok(client) = crate::provider::OpenAiClient::new(base_url.clone(), api_key) else {
        return;
    };
    let timeout_ms = engine.app.config.model_metadata.timeout_ms;
    let storage = engine.app.storage.clone();
    fetch_and_cache_metadata(&client, &base_url, timeout_ms, &storage).await;
    model_metadata_refreshed(engine);
}

/// A metadata fetch (background or inline) finished: re-stamp the active
/// provider from the cache, propagate the stamp into live runners so
/// in-flight compaction sees the fresh window, refresh the context budget,
/// and mark the one-shot lazy attempt as done.
fn model_metadata_refreshed(engine: &mut Engine) {
    let base_url = engine.app.config.provider.base_url.clone();
    let model = engine.app.config.provider.model.clone();
    let discovered = app::stamp_discovered_meta(&mut engine.app.config, &engine.app.storage);
    // `discovered` is `#[serde(skip)]`, so saved profiles stay clean; only
    // the in-memory runner copies are patched in place.
    for runtime in engine.app.background.values_mut() {
        if let Some(runner) = runtime.runner.as_mut() {
            runner.set_discovered_meta(&base_url, &model, discovered);
        }
    }
    if let Some(runner) = engine.app.current.runner.as_mut() {
        runner.set_discovered_meta(&base_url, &model, discovered);
    }
    engine.app.current.context_limit_tokens =
        engine.app.config.provider.resolved_context_window_tokens();
    engine.app.current.metadata_fetch_attempted = true;
    push_context_updated(engine, &engine.app.current.session_id);
}

/// Removes a saved provider profile by id, switching the active provider (and
/// its runner) when it was the one removed. The API key stays in the OS
/// keyring.
fn remove_provider(engine: &mut Engine, provider_id: &str) -> Result<(), ApiError> {
    engine.app.config.remove_provider_by_id(provider_id);
    if engine.app.config.provider.id() == provider_id {
        engine.app.config.provider = engine
            .app
            .config
            .providers
            .first()
            .cloned()
            .unwrap_or_else(|| crate::config::ProviderPreset::OpenAi.defaults());
        let fallback_id = engine.app.config.provider.id().to_owned();
        engine.app.active_secret =
            crate::secrets::api_key_cached(engine.app.config.provider.preset, &fallback_id)
                .ok()
                .map(|key| (fallback_id, key));
        app::rebuild_runner(&mut engine.app).map_err(api_error)?;
    }
    // The in-memory removal (and any switch) has already taken effect, so a
    // failed persist degrades to a status warning instead of undoing the
    // user's action, mirroring the provider-apply path.
    if let Err(error) = engine.app.config.save() {
        let suffix = crate::secrets::redact(&error.to_string());
        engine.app.current.status = format!("供应商已删除，但保存失败：{suffix}");
    }
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Computes the session always-allow key (tool + optional command prefix) for
/// an agent approval prompt, plus a human-readable label for the audit entry.
fn session_allow_for_call(call: &crate::provider::ToolCall) -> (String, Option<String>, String) {
    let prefix = crate::tools::ToolRegistry::command_prefix_for(call);
    match prefix {
        Some(command) => {
            let label = format!("{} {}", call.name, command);
            (call.name.clone(), Some(command), label)
        }
        None => (call.name.clone(), None, call.name.clone()),
    }
}

fn api_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    if message.contains("后台会话容量已满") {
        ApiError::conflict(message)
    } else {
        ApiError::internal(message)
    }
}

fn memory_dto(record: &MemoryRecord) -> MemoryDto {
    MemoryDto {
        id: record.id,
        kind: record.kind.clone(),
        title: record.title.clone(),
        content: record.content.clone(),
        topic: record.topic.clone(),
        status: record.status.clone(),
        source_session_id: record.source_session_id.clone(),
        source_turn_id: record.source_turn_id.clone(),
        source_message_id: record.source_message_id,
        evidence: record.evidence.clone(),
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        confirmed_at: record.confirmed_at.clone(),
        recallable: record.recallable,
    }
}

fn storage_api_error(error: crate::storage::StorageError) -> ApiError {
    match error {
        crate::storage::StorageError::MemoryLimit(message) => ApiError::conflict(message),
        other => ApiError::internal(other.to_string()),
    }
}

fn memory_api_error(error: crate::storage::StorageError, _config: &Config) -> ApiError {
    storage_api_error(error)
}

fn mutate_memory<T>(
    engine: &mut Engine,
    operation: impl FnOnce(&Storage, &str, &Config) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let workspace = engine.app.workspace.to_string_lossy().into_owned();
    operation(&engine.app.storage, &workspace, &engine.app.config)
}

fn save_memory(
    engine: &mut Engine,
    title: &str,
    content: &str,
    candidate: bool,
) -> Result<MemoryDto, ApiError> {
    let workspace = engine.app.workspace.to_string_lossy().into_owned();
    let session_id = engine.app.active_session.clone();
    let turn_id = engine.app.storage.head_turn_id(&session_id).ok().flatten();
    engine
        .app
        .storage
        .create_memory(
            &workspace,
            if candidate { "candidate" } else { "explicit" },
            title,
            content,
            None,
            if candidate { "candidate" } else { "active" },
            Some(&session_id),
            turn_id.as_deref(),
            None,
            Some("user-managed"),
            engine.app.config.memory.max_entries,
            engine.app.config.memory.max_candidates,
            engine.app.config.memory.max_entry_bytes,
            engine.app.config.memory.max_total_bytes,
        )
        .map(|record| memory_dto(&record))
        .map_err(storage_api_error)
}

#[cfg(test)]
mod approval_routing_tests {
    use super::*;
    use crate::{
        app::{ApprovalAction, PendingApproval},
        provider::ToolCall,
    };
    use tempfile::TempDir;

    #[tokio::test]
    async fn resolves_two_session_approvals_by_id_in_reverse_order() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let mut config = crate::config::Config::default();
        config.data_dir = temp.path().join("data");
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let storage = crate::storage::Storage::open(&temp.path().join("data/agent.db")).unwrap();
        let first = storage.create_session(&workspace).unwrap();
        let app = crate::app::build_app(workspace.clone(), config, storage.clone(), first.clone())
            .await
            .unwrap();
        let mut app = app;
        let second = storage.create_session(&workspace).unwrap();
        crate::app::activate_session(&mut app, second.clone()).unwrap();

        let (first_tx, first_rx) = oneshot::channel();
        app.background.get_mut(&first).unwrap().pending_approval = Some(PendingApproval {
            approval_id: Some("approval-first".into()),
            call: ToolCall {
                id: "first-call".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"first.txt"}),
            },
            reason: "first".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(first_tx),
            created_at: Instant::now(),
        });
        let (second_tx, second_rx) = oneshot::channel();
        app.current.pending_approval = Some(PendingApproval {
            approval_id: Some("approval-second".into()),
            call: ToolCall {
                id: "second-call".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"second.txt"}),
            },
            reason: "second".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(second_tx),
            created_at: Instant::now(),
        });
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut pending = HashMap::new();
        pending.insert(
            "approval-first".into(),
            PendingRecord {
                session_id: first.clone(),
                deadline,
            },
        );
        pending.insert(
            "approval-second".into(),
            PendingRecord {
                session_id: second.clone(),
                deadline,
            },
        );
        let (command_tx, _command_rx) = mpsc::channel(1);
        let mut engine = Engine {
            app,
            bridge: Arc::new(crate::bridge::EventBridge::new(64, 1024 * 1024)),
            pending,
            approval_timeout: Duration::from_secs(300),
            command_tx,
        };

        engine
            .resolve_approval("approval-second", false, false)
            .unwrap();
        assert!(!second_rx.await.unwrap());
        assert!(
            engine
                .resolve_approval("approval-second", true, false)
                .is_err()
        );
        assert!(engine.app.background[&first].pending_approval.is_some());
        engine
            .resolve_approval("approval-first", true, false)
            .unwrap();
        assert!(first_rx.await.unwrap());
        assert!(engine.pending.is_empty());
        assert!(engine.app.background[&first].pending_approval.is_none());
        assert!(engine.app.current.pending_approval.is_none());
        engine.shutdown();
    }
}
