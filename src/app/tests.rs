use tempfile::TempDir;

use super::*;

use crate::session::{MAX_THINKING_LINE_BYTES, estimate_context_tokens};
use std::time::Duration;

fn handle_event_for_test(app: &mut App, event: AgentEvent) -> crate::session::SessionOutcome {
    let ctx = crate::session::EventCtx {
        storage: &app.storage,
        workspace: &app.workspace,
    };
    app.current.handle_event(&ctx, event)
}

#[test]
fn submit_keeps_first_prompt_when_provider_is_unavailable() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.current.runner = None;
    app.input.set("keep this prompt");

    submit_input(&mut app).unwrap();

    assert_eq!(app.input.as_str(), "keep this prompt");
    assert_eq!(app.current.status, "请打开提供商设置配置 API Key");
}

#[tokio::test]
async fn delete_last_session_creates_replacement_and_removes_old_runtime() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let deleted = app.active_session.clone();

    execute_command(&mut app, Command::Delete).unwrap();

    assert_ne!(app.active_session, deleted);
    assert_eq!(app.current.session_id, app.active_session);
    assert!(!app.background.contains_key(&deleted));
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.sessions[0].id, app.active_session);
    assert_eq!(app.current.mode, AgentMode::Build);
    assert_eq!(app.current.status, "会话已删除");
    let sessions = app.storage.list_sessions(&app.workspace).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, app.active_session);
}

#[tokio::test]
async fn delete_session_switches_to_most_recent_remaining_session() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let deleted = app.active_session.clone();
    let replacement = app.storage.create_session(&app.workspace).unwrap();

    execute_command(&mut app, Command::Delete).unwrap();

    assert_eq!(app.active_session, replacement);
    assert_eq!(app.current.session_id, replacement);
    assert!(!app.background.contains_key(&deleted));
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.sessions[0].id, replacement);
    assert_eq!(app.current.status, "会话已删除");
}

#[test]
fn export_session_defaults_to_a_visible_workspace_file() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.current.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: "export me".into(),
    });
    let session_id = app.current.session_id.clone();

    export_session(&mut app, None).unwrap();

    let target = app.workspace.join(format!("1h-agent-{session_id}.md"));
    assert!(target.is_file());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "## You\n\nexport me\n\n"
    );
    assert!(app.current.status.contains("工作区"));
    assert!(
        app.current
            .status
            .contains(&format!("1h-agent-{session_id}.md"))
    );
}

#[test]
fn export_session_accepts_a_workspace_relative_path() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);

    export_session(&mut app, Some("conversation.md".into())).unwrap();

    let target = app.workspace.join("conversation.md");
    assert!(target.is_file());
    assert!(app.current.status.contains("conversation.md"));
}

#[tokio::test]
async fn undo_and_redo_reload_the_active_session_history() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let session_id = app.current.session_id.clone();
    app.storage
        .append_message(&session_id, Role::User, "hello")
        .unwrap();
    app.storage
        .append_message(&session_id, Role::Assistant, "hi")
        .unwrap();
    app.storage
        .save_response_id(&session_id, "response")
        .unwrap();

    execute_command(&mut app, Command::Undo).unwrap();

    assert_eq!(app.current.status, "已撤销上一轮");
    assert!(app.current.conversation.is_empty());
    assert_eq!(app.current.entries.len(), 1);
    assert!(app.storage.response_id(&session_id).unwrap().is_none());

    execute_command(&mut app, Command::Redo).unwrap();

    assert_eq!(app.current.status, "已重做上一轮");
    assert_eq!(app.current.conversation.len(), 2);
    assert_eq!(app.current.entries.len(), 2);
}

#[test]
fn rebuild_runner_clears_the_usage_anchor() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    // A provider/model switch rebuilds the runner under a new tokenizer
    // and routing contract; the old anchor is no longer comparable.
    app.current.usage_anchor = Some(crate::session::UsageAnchor {
        real_input: 500,
        at_len: 4,
    });

    rebuild_runner(&mut app).unwrap();

    assert!(app.current.usage_anchor.is_none());
}

#[tokio::test]
async fn undo_and_redo_clear_the_usage_anchor() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let session_id = app.current.session_id.clone();
    app.storage
        .append_message(&session_id, Role::User, "hello")
        .unwrap();
    app.storage
        .append_message(&session_id, Role::Assistant, "hi")
        .unwrap();
    app.current.usage_anchor = Some(crate::session::UsageAnchor {
        real_input: 500,
        at_len: 2,
    });

    execute_command(&mut app, Command::Undo).unwrap();
    // reload_current_session builds a fresh runtime: no stale anchor.
    assert!(app.current.usage_anchor.is_none());

    execute_command(&mut app, Command::Redo).unwrap();
    assert!(app.current.usage_anchor.is_none());
}

#[tokio::test]
async fn undo_rolls_back_snapshotted_file_and_redo_restores_it() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let session_id = app.current.session_id.clone();
    // New head turn that undo will detach.
    app.storage
        .append_message(&session_id, Role::User, "write")
        .unwrap();
    let turn = app.storage.head_turn_id(&session_id).unwrap().unwrap();

    let file = temp.path().join("a.txt");
    std::fs::write(&file, b"after").unwrap();
    app.storage
        .snapshot_file(
            &session_id,
            &turn,
            "call_1",
            "a.txt",
            Some(b"before"),
            true,
            1024 * 1024,
            16 * 1024 * 1024,
        )
        .unwrap();
    app.storage
        .save_post_image("call_1", Some(b"after"))
        .unwrap();

    execute_command(&mut app, Command::Undo).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"before");

    execute_command(&mut app, Command::Redo).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"after");
}

#[tokio::test]
async fn undo_without_snapshot_keeps_file_untouched() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let session_id = app.current.session_id.clone();
    app.storage
        .append_message(&session_id, Role::User, "write")
        .unwrap();
    let file = temp.path().join("a.txt");
    std::fs::write(&file, b"content").unwrap();

    execute_command(&mut app, Command::Undo).unwrap();
    // No snapshot was recorded; the file must be left exactly as it was.
    assert_eq!(std::fs::read(&file).unwrap(), b"content");
    assert_eq!(app.current.status, "已撤销上一轮");
}

#[test]
fn switch_mode_updates_registry_storage_and_status() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.storage
        .save_response_id(&app.current.session_id, "stale-response")
        .unwrap();
    switch_mode(&mut app, AgentMode::Explore).unwrap();
    assert_eq!(app.current.mode, AgentMode::Explore);
    assert!(app.current.status.contains("EXPLORE"));
    assert_eq!(
        app.storage.session_mode(&app.current.session_id).unwrap(),
        AgentMode::Explore.as_str()
    );
    assert!(
        app.storage
            .response_id(&app.current.session_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn cluster_command_switches_to_cluster_mode() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    assert_eq!(app.current.mode, AgentMode::Build);
    execute_command(&mut app, Command::Mode(AgentMode::Cluster)).unwrap();
    assert_eq!(app.current.mode, AgentMode::Cluster);
}

#[tokio::test]
async fn activate_session_keeps_global_model_when_session_model_is_empty() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.config.provider.preset = ProviderPreset::DeepSeek;
    app.config.provider.model = "deepseek-v4-flash".into();
    let new_session = app.storage.create_session(&app.workspace).unwrap();
    activate_session(&mut app, new_session).unwrap();
    // A regular session stores an empty model; it must fall back to the
    // global DeepSeek model (deepseek-v4-flash window) rather than "".
    assert_eq!(app.current.context_limit_tokens, Some(1_000_000));
}

#[tokio::test]
async fn handle_routed_event_records_child_session_status() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let parent = app.active_session.clone();
    let child_id = app
        .storage
        .create_child_session(
            &app.workspace,
            &parent,
            "openai",
            "gpt-5-mini",
            "child",
            "explore",
            "reviewer",
        )
        .unwrap();
    let redraw = handle_routed_event(
        &mut app,
        RoutedEvent {
            session_id: parent,
            event: AgentEvent::ChildSessionProgress {
                session_id: child_id.clone(),
                progress: ChildSessionProgress {
                    status: ChildSessionStatus::WaitingModel,
                    turn: 1,
                    max_turns: 3,
                    tool: None,
                    updated_at: Instant::now(),
                },
            },
        },
    );
    assert!(redraw);
    assert_eq!(
        app.child_status
            .get(&child_id)
            .map(|progress| progress.status),
        Some(ChildSessionStatus::WaitingModel)
    );
}

#[tokio::test]
async fn cluster_batch_status_tracks_queued_running_and_completed_children() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let parent = app.active_session.clone();
    let child_a = app
        .storage
        .create_child_session(
            &app.workspace,
            &parent,
            "openai",
            "gpt-5-mini",
            "child-a",
            "explore",
            "reviewer",
        )
        .unwrap();
    let child_b = app
        .storage
        .create_child_session(
            &app.workspace,
            &parent,
            "openai",
            "gpt-5-mini",
            "child-b",
            "explore",
            "reviewer",
        )
        .unwrap();
    let route = |app: &mut App, child: &str, status| {
        handle_routed_event(
            app,
            RoutedEvent {
                session_id: parent.clone(),
                event: AgentEvent::ChildSessionProgress {
                    session_id: child.into(),
                    progress: ChildSessionProgress {
                        status,
                        turn: 1,
                        max_turns: 3,
                        tool: None,
                        updated_at: Instant::now(),
                    },
                },
            },
        )
    };

    assert!(route(&mut app, &child_a, ChildSessionStatus::Queued));
    assert!(route(&mut app, &child_b, ChildSessionStatus::Queued));
    assert!(route(&mut app, &child_a, ChildSessionStatus::WaitingModel));
    assert_eq!(app.current.status, "集群 0/2 完成 · 1 运行 · 1 排队");
    assert!(route(&mut app, &child_a, ChildSessionStatus::Completed));
    assert_eq!(app.current.status, "集群 1/2 完成 · 0 运行 · 1 排队");
}

#[tokio::test]
async fn switching_session_parks_runtime_and_switches_back() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let old_session = app.active_session.clone();
    let new_session = app.storage.create_session(&app.workspace).unwrap();

    activate_session(&mut app, new_session.clone()).unwrap();
    assert_eq!(app.active_session, new_session);
    assert!(app.background.contains_key(&old_session));

    activate_session(&mut app, old_session.clone()).unwrap();
    assert_eq!(app.active_session, old_session);
    assert!(app.background.contains_key(&new_session));
}

#[tokio::test]
async fn delete_running_session_aborts_task_and_rejects_approval() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let deleted = app.active_session.clone();
    let (task_finished, task_result) = oneshot::channel();
    app.current.busy = true;
    app.current.active_task = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let _ = task_finished.send(());
    }));
    let (approval_reply, approval_result) = oneshot::channel();
    app.current.pending_approval = Some(PendingApproval {
        call: ToolCall {
            id: "delete-running".into(),
            name: "file_write".into(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        },
        reason: "test deletion shutdown".into(),
        source_session_id: None,
        source_title: None,
        action: ApprovalAction::Agent(approval_reply),
        created_at: Instant::now(),
    });

    execute_command(&mut app, Command::Delete).unwrap();

    assert!(!app.background.contains_key(&deleted));
    assert!(!approval_result.await.unwrap());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), task_result)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn background_capacity_evicts_least_recently_parked_idle_runtime() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.config.runtime.max_background_sessions = 2;
    let first = app.active_session.clone();
    let second = app.storage.create_session(&app.workspace).unwrap();
    let third = app.storage.create_session(&app.workspace).unwrap();
    let fourth = app.storage.create_session(&app.workspace).unwrap();

    activate_session(&mut app, second.clone()).unwrap();
    app.background.get_mut(&first).unwrap().parked_at = Instant::now();
    activate_session(&mut app, third.clone()).unwrap();
    app.background.get_mut(&second).unwrap().parked_at = Instant::now() + Duration::from_secs(1);
    activate_session(&mut app, fourth).unwrap();

    assert_eq!(app.background.len(), 2);
    assert!(!app.background.contains_key(&first));
    assert!(app.background.contains_key(&second));
    assert!(app.background.contains_key(&third));
}

#[tokio::test]
async fn background_capacity_rejects_switch_when_all_runtimes_are_busy() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.config.runtime.max_background_sessions = 2;
    let busy = app.active_session.clone();
    let waiting = app.storage.create_session(&app.workspace).unwrap();
    let evicted = app.storage.create_session(&app.workspace).unwrap();
    let active = app.storage.create_session(&app.workspace).unwrap();

    activate_session(&mut app, waiting.clone()).unwrap();
    app.background.get_mut(&busy).unwrap().busy = true;
    activate_session(&mut app, evicted.clone()).unwrap();
    let (approval_reply, approval_result) = oneshot::channel();
    app.background.get_mut(&waiting).unwrap().pending_approval = Some(PendingApproval {
        call: ToolCall {
            id: "capacity-approval".into(),
            name: "file_write".into(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        },
        reason: "test protected approval".into(),
        source_session_id: None,
        source_title: None,
        action: ApprovalAction::Agent(approval_reply),
        created_at: Instant::now(),
    });
    assert!(activate_session(&mut app, active).is_err());

    assert_eq!(app.background.len(), 2);
    assert!(app.background.contains_key(&busy));
    assert!(app.background.contains_key(&waiting));
    assert_eq!(app.active_session, evicted);
    app.background.get_mut(&waiting).unwrap().shutdown();
    assert!(!approval_result.await.unwrap());
}

#[tokio::test]
async fn background_capacity_never_interrupts_busy_or_approval_runtime() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    app.config.runtime.max_background_sessions = 2;
    let oldest = app.active_session.clone();
    let second = app.storage.create_session(&app.workspace).unwrap();
    let third = app.storage.create_session(&app.workspace).unwrap();
    let active = app.storage.create_session(&app.workspace).unwrap();

    activate_session(&mut app, second.clone()).unwrap();
    let (approval_reply, approval_result) = oneshot::channel();
    app.background.get_mut(&oldest).unwrap().pending_approval = Some(PendingApproval {
        call: ToolCall {
            id: "forced-capacity-approval".into(),
            name: "file_write".into(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        },
        reason: "test strict capacity".into(),
        source_session_id: None,
        source_title: None,
        action: ApprovalAction::Agent(approval_reply),
        created_at: Instant::now(),
    });
    activate_session(&mut app, third.clone()).unwrap();
    app.background.get_mut(&second).unwrap().busy = true;
    app.current.busy = true;

    assert!(activate_session(&mut app, active).is_err());

    assert_eq!(app.background.len(), 2);
    assert!(app.background.contains_key(&oldest));
    assert!(app.background.contains_key(&second));
    assert!(!app.background.contains_key(&third));
    assert_eq!(app.active_session, third);
    app.background.get_mut(&oldest).unwrap().shutdown();
    assert!(!approval_result.await.unwrap());
}

fn thinking_summary_count(app: &App) -> usize {
    app.current
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, DisplayKind::Thinking))
        .count()
}

fn last_thinking_summary(app: &App) -> Option<&str> {
    app.current
        .entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.kind, DisplayKind::Thinking))
        .and_then(|entry| match &entry.content {
            DisplayContent::Thinking(thinking) => Some(thinking.content.as_str()),
            _ => None,
        })
}

fn test_app(temp: &TempDir) -> App {
    let workspace = temp.path().to_path_buf();
    let config = Config::default();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(&workspace).unwrap();
    let sessions = storage.list_sessions(&workspace).unwrap();
    let registry = Arc::new(ToolRegistry::new(
        Workspace::new(&workspace).unwrap(),
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
    let (agent_tx, _agent_rx) = mpsc::channel(8);
    let (router_tx, router_rx) = mpsc::channel(16);
    let approval_lock = Arc::new(Mutex::new(()));
    let runtime = SessionRuntime {
        session_id: session_id.clone(),
        status: String::new(),
        entries: vec![DisplayEntry {
            kind: DisplayKind::Assistant,
            content: DisplayContent::Markdown("first line\n\n中文 🙂 long output".into()),
        }],
        todos: Vec::new(),
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
        context_used_tokens: 1,
        context_limit_tokens: None,
        token_calibration: 1.0,
        usage_anchor: None,
        metadata_fetch_attempted: false,
        pending_approval: None,
        mode: AgentMode::default(),
        child_role: None,
        conversation: Vec::new(),
        runner: None,
        agent_tx,
        active_task: None,
        request_seq: 0,
        parked_at: Instant::now(),
    };
    App {
        workspace,
        input: InputBuffer::new(),
        context_meter_enabled: false,
        sessions,
        child_status: HashMap::new(),
        child_batches: HashMap::new(),
        storage,
        config,
        registry,
        approval_lock,
        active_secret: None,
        active_session: session_id,
        current: runtime,
        background: HashMap::new(),
        router_tx,
        router_rx,
        should_quit: false,
    }
}

#[test]
fn display_restore_keeps_agent_tool_agent_order() {
    let call = ToolCall {
        id: "call_1".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"src/lib.rs"}),
    };
    let entries = display_entries(&[
        ConversationItem::Message {
            role: Role::Assistant,
            content: "before".into(),
        },
        ConversationItem::AssistantToolCalls { calls: vec![call] },
        ConversationItem::ToolOutput {
            call_id: "call_1".into(),
            output: "ok".into(),
        },
        ConversationItem::Message {
            role: Role::Assistant,
            content: "after".into(),
        },
    ]);
    assert!(matches!(entries[0].kind, DisplayKind::Assistant));
    assert!(matches!(entries[1].kind, DisplayKind::Tool));
    assert!(matches!(entries[2].kind, DisplayKind::Assistant));
    assert_eq!(entries.len(), 3);
    assert!(matches!(&entries[1].content, DisplayContent::Tool(tool)
        if tool.call_id == "call_1" && tool.result.as_deref() == Some("ok")));
}

#[test]
fn context_estimate_is_bounded_and_nonzero() {
    assert_eq!(estimate_context_tokens(&[]), 1);
    assert_eq!(
        estimate_context_tokens(&[ConversationItem::Message {
            role: Role::User,
            content: "12345678".into(),
        }]),
        2
    );
}

#[test]
fn cancel_saves_streaming_text_as_partial_and_marks_it_incomplete() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let session_id = app.active_session.clone();
    // The fixture pre-seeds an assistant entry; a live session starts empty.
    app.current.entries.clear();

    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(&mut app, AgentEvent::TextDelta("half an ".into()));
    handle_event_for_test(&mut app, AgentEvent::TextDelta("answer".into()));

    // The runtime tracks the streaming text as a regular Assistant entry.
    assert!(matches!(
        app.current.entries.last().map(|e| &e.kind),
        Some(DisplayKind::Assistant)
    ));

    cancel_active_request(&mut app);

    // The interrupted answer is persisted as assistant_partial.
    let partial = app.storage.load_partial(&session_id).unwrap().unwrap();
    assert_eq!(partial.content, "half an answer");

    // And the live entry is now marked incomplete.
    assert!(matches!(
        app.current.entries.iter().rev().find(|e| matches!(e.kind, DisplayKind::Assistant | DisplayKind::AssistantPartial)),
        Some(entry) if matches!(entry.kind, DisplayKind::AssistantPartial)
    ));
}

#[test]
fn terminal_events_mark_streaming_assistant_as_partial() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    handle_event_for_test(&mut app, AgentEvent::TextDelta("interrupted".into()));
    handle_event_for_test(&mut app, AgentEvent::Failed("boom".into()));
    assert!(
        app.current
            .entries
            .iter()
            .any(|e| matches!(e.kind, DisplayKind::AssistantPartial))
    );

    // A fresh streaming round after completion stays a normal assistant.
    let mut app2 = test_app(&temp);
    handle_event_for_test(&mut app2, AgentEvent::TextDelta("complete".into()));
    handle_event_for_test(&mut app2, AgentEvent::Completed { items: Vec::new() });
    assert!(
        app2.current
            .entries
            .iter()
            .all(|e| !matches!(e.kind, DisplayKind::AssistantPartial))
    );
}

#[test]
fn submit_while_busy_is_rejected_without_overwriting_task() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    crate::secrets::test_seed_key(app.config.provider.preset, "test-key");
    app.active_secret = Some((app.config.provider.preset, "test-key".into()));
    rebuild_runner(&mut app).unwrap();
    assert!(app.current.runner.is_some());

    app.current.busy = true;
    app.input.set("second message");
    let result = submit_input(&mut app);
    assert!(result.is_err(), "busy submit must be rejected");
    assert!(app.current.active_task.is_none());
}

#[test]
fn finish_thinking_skips_empty_buffer() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(
        &mut app,
        AgentEvent::ToolStarted(ToolCall {
            id: "call-empty".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"Cargo.toml"}),
        }),
    );
    assert_eq!(thinking_summary_count(&app), 0);
}

#[test]
fn reasoning_without_newlines_keeps_utf8_safe_tail() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    let delta = format!("{}👩‍💻e\u{301}尾", "中文🙂".repeat(400));
    handle_event_for_test(&mut app, AgentEvent::ReasoningDelta(delta));

    assert!(app.current.thinking_last_line.len() <= MAX_THINKING_LINE_BYTES);
    assert!(app.current.thinking_last_line.ends_with("👩‍💻e\u{301}尾"));
    assert!(!app.current.thinking_last_line.contains('\u{fffd}'));
}

#[test]
fn reasoning_terminal_events_set_fixed_statuses_and_persist() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);

    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(
        &mut app,
        AgentEvent::ReasoningDelta("正在分析工具结果".into()),
    );
    handle_event_for_test(&mut app, AgentEvent::TextDelta("answer".into()));
    assert!(!app.current.thinking_active);
    assert_eq!(app.current.thinking_result, ThinkingResult::Completed);
    assert_eq!(thinking_summary_count(&app), 1);
    assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("正在分析工具结果")));

    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("最后失败位置".into()));
    handle_event_for_test(&mut app, AgentEvent::Failed("failed".into()));
    assert!(!app.current.thinking_active);
    assert_eq!(app.current.thinking_result, ThinkingResult::Failed);
    assert_eq!(thinking_summary_count(&app), 2);
    assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("最后失败位置")));

    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("取消前内容".into()));
    handle_event_for_test(&mut app, AgentEvent::Cancelled("cancelled".into()));
    assert!(!app.current.thinking_active);
    assert_eq!(app.current.thinking_result, ThinkingResult::Cancelled);
    assert_eq!(thinking_summary_count(&app), 3);
    assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("取消前内容")));
}

#[test]
fn reasoning_completed_persists_summary_and_switches_to_body_phase() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
    handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("思考内容".into()));
    handle_event_for_test(&mut app, AgentEvent::ReasoningCompleted);
    assert!(
        !app.current.thinking_active,
        "live thinking retires at the barrier"
    );
    assert_eq!(app.current.thinking_result, ThinkingResult::Completed);
    assert_eq!(app.current.agent_phase, AgentPhase::StreamingText);
    assert_eq!(thinking_summary_count(&app), 1);
    assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("思考内容")));

    // The body delta only appends to the answer; it must not re-persist a
    // second summary for the same reasoning phase.
    handle_event_for_test(&mut app, AgentEvent::TextDelta("正文".into()));
    assert_eq!(thinking_summary_count(&app), 1);
    assert!(
        matches!(app.current.entries.last().map(|entry| &entry.content),
        Some(DisplayContent::Markdown(text)) if text == "正文")
    );
}

#[test]
fn tool_started_and_finished_update_one_display_entry() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    let call = ToolCall {
        id: "merged-call".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"src/app.rs"}),
    };
    let initial_len = app.current.entries.len();
    handle_event_for_test(&mut app, AgentEvent::ToolStarted(call.clone()));
    handle_event_for_test(
        &mut app,
        AgentEvent::ToolFinished {
            call,
            result: "contents".into(),
        },
    );
    assert_eq!(app.current.entries.len(), initial_len + 1);
    assert!(
        matches!(app.current.entries.last().map(|entry| &entry.content),
        Some(DisplayContent::Tool(tool))
            if tool.status == ToolDisplayStatus::Completed
                && tool.result.as_deref() == Some("contents"))
    );
}

#[test]
fn export_includes_todo_checklist() {
    let temp = TempDir::new().unwrap();
    let mut app = test_app(&temp);
    execute_command(
        &mut app,
        Command::Todo(TodoCommand::Add("pending task".into())),
    )
    .unwrap();
    execute_command(
        &mut app,
        Command::Todo(TodoCommand::Add("done task".into())),
    )
    .unwrap();
    execute_command(&mut app, Command::Todo(TodoCommand::Done(2))).unwrap();

    export_session(&mut app, None).unwrap();
    let filename = format!("1h-agent-{}.md", app.current.session_id);
    let output = std::fs::read_to_string(app.workspace.join(filename)).unwrap();
    assert!(output.contains("## 任务清单（1/2）"));
    assert!(output.contains("- [ ] pending task"));
    assert!(output.contains("- [x] done task"));
}
