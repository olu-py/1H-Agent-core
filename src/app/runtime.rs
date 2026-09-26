use super::*;

/// Resolves a saved provider profile for a cross-provider child agent by
/// provider id. Built-in ids still fall back to their preset template so an
/// unconfigured built-in remains usable; an unknown id (for example a deleted
/// custom provider) is rejected rather than silently mapped elsewhere.
pub(super) fn provider_config_resolver(
    config: &Config,
) -> Arc<crate::agent::ChildProviderResolver> {
    let providers = config.providers.clone();
    let default_provider = config.provider.clone();
    Arc::new(
        move |id: &str| -> Result<crate::config::ProviderConfig, String> {
            if let Some(provider) = providers
                .iter()
                .find(|provider| provider.id() == id)
                .cloned()
            {
                return Ok(provider);
            }
            if default_provider.id() == id {
                return Ok(default_provider.clone());
            }
            // `agent_spawn` may name a saved custom provider instead of its
            // opaque id; names are unique (case-insensitive) by construction.
            if let Some(provider) = providers
                .iter()
                .find(|provider| {
                    !provider.name.trim().is_empty()
                        && provider.name.trim().eq_ignore_ascii_case(id)
                })
                .cloned()
            {
                return Ok(provider);
            }
            if !default_provider.name.trim().is_empty()
                && default_provider.name.trim().eq_ignore_ascii_case(id)
            {
                return Ok(default_provider.clone());
            }
            let preset = ProviderPreset::parse(id)
                .ok_or_else(|| format!("unknown provider id \"{id}\" for agent_spawn"))?;
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
/// the saved profile (or the preset template when only the built-in id is
/// known) is used, and must be valid (e.g. Qwen needs a real workspace URL
/// configured via env or config). Legacy session rows that stored a bare preset
/// name keep working because built-in ids equal their preset key and the
/// fallback also parses the value as a preset.
pub(super) fn session_provider_config(
    config: &Config,
    provider_id: &str,
    model: &str,
) -> Option<crate::config::ProviderConfig> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let mut provider_config = config.provider_for_id(provider_id).or_else(|| {
        ProviderPreset::parse(provider_id).map(|preset| {
            config
                .provider_for(preset)
                .unwrap_or_else(|| preset.defaults())
        })
    })?;
    provider_config.ensure_id();
    provider_config.validate().ok()?;
    provider_config.model = model.to_owned();
    provider_config.normalize_thinking();
    Some(provider_config)
}

/// Builds a fresh `SessionRuntime` for the given session: loads its messages,
/// resolves provider/model (child sessions override the global default), and
/// spawns an event forwarder that routes its agent events to the router.
pub(super) fn build_runtime(
    storage: &Storage,
    config: &Config,
    registry: &Arc<ToolRegistry>,
    router_tx: &mpsc::Sender<RoutedEvent>,
    approval_lock: &Arc<Mutex<()>>,
    active_secret: Option<&(String, String)>,
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
    let mut child_role = storage.session_child_role(session_id).ok().flatten();
    let child_allowed_tools = storage
        .session_child_allowed_tools(session_id)
        .ok()
        .flatten();
    let is_child = storage
        .session_parent_id(session_id)
        .ok()
        .flatten()
        .is_some();
    if is_child && child_allowed_tools.is_none() {
        // Older child sessions did not persist template restrictions or an
        // explicit capability. Restore them read-only because the original
        // effective tool set cannot be reconstructed safely.
        child_role = Some("read_only".into());
    }
    let child_allowed_tools = child_allowed_tools.unwrap_or_default();
    let child_provider_resolver = provider_config_resolver(config);
    let runtime_key = active_secret
        .filter(|(id, _)| id == provider_config.id())
        .map(|(_, api_key)| api_key.clone())
        .or_else(|| {
            secrets::api_key_cached_only(provider_config.preset, provider_config.id()).ok()
        });
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
            .with_child_allowed_tools(child_allowed_tools.clone())
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
        child_allowed_tools,
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
pub(super) enum SnapshotDirection {
    Backward,
    Forward,
}

/// Rolls the file snapshots recorded on `turn_id` back to disk for undo or
/// forward for redo. Returns a human-readable summary of any files that could
/// not be restored, or `None` when every snapshot applied cleanly.
pub(super) fn restore_snapshots(
    app: &mut App,
    turn_id: &str,
    direction: SnapshotDirection,
) -> Option<String> {
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
        let _ = secrets::api_key_cached(provider_config.preset, provider_config.id());
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
