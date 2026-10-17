use super::*;
use crate::provider::ToolCall;
use rusqlite::{Connection, params};
use tempfile::tempdir;

#[test]
fn cancellation_interrupts_running_sql_and_removes_progress_callback() {
    let cancellation = crate::service::RequestCancellation::default();
    let token = cancellation.clone();
    let worker = std::thread::spawn(move || {
        let connection = Connection::open_in_memory().unwrap();
        let progress = ReadProgress::install(&connection, &token);
        let result=connection.query_row(
            "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n",[],|row|row.get::<_,i64>(0));
        assert!(
            matches!(result,Err(rusqlite::Error::SqliteFailure(error,_)) if error.code==rusqlite::ErrorCode::OperationInterrupted)
        );
        drop(progress);
        // The still-cancelled token must not affect the next independent query.
        assert_eq!(
            connection
                .query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            50005000
        );
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while cancellation.sql_steps() == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        cancellation.sql_steps() > 0,
        "must cancel after SQLite is actually executing"
    );
    cancellation.cancel();
    worker.join().unwrap();
}

fn assert_history_pages_match_recursive_oracle(storage: &Storage, session_id: &str) {
    let connection = storage.lock().unwrap();
    let mut statement = connection
        .prepare(
            "WITH RECURSIVE chain(id) AS (
                 SELECT head_turn_id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                 WHERE turns.parent_id IS NOT NULL
             )
             SELECT id, role, content, kind, metadata, created_at FROM messages
             WHERE session_id = ?1 AND hidden = 0 AND partial = 0
               AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
             ORDER BY id DESC",
        )
        .unwrap();
    let oracle = statement
        .query_map([session_id], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                kind: row.get(3)?,
                metadata: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(statement);
    drop(connection);

    for limit in [1, 4, 100] {
        let mut cursors = vec![None, Some(i64::MIN), Some(i64::MAX)];
        cursors.extend(oracle.iter().map(|message| Some(message.id)));
        for cursor in cursors {
            let expected = oracle
                .iter()
                .filter(|message| cursor.is_none_or(|cursor| message.id < cursor))
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                storage
                    .load_message_page(session_id, cursor, limit)
                    .unwrap(),
                expected,
                "page mismatch for cursor {cursor:?} and limit {limit}"
            );
        }
    }
    assert!(
        storage
            .load_message_page(session_id, None, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn child_session_nests_under_parent_and_keeps_provider_model() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let parent = storage.create_session(root.path()).unwrap();
    let child = storage
        .create_child_session(
            root.path(),
            &parent,
            "deepseek",
            "deepseek-v4-pro",
            "计划",
            "explore",
            "planner",
        )
        .unwrap();

    let sessions = storage.list_sessions(root.path()).unwrap();
    let child_summary = sessions.iter().find(|session| session.id == child).unwrap();
    assert_eq!(child_summary.parent_id.as_deref(), Some(parent.as_str()));
    assert_eq!(child_summary.title, "计划");

    let (provider, model) = storage.session_provider_model(&child).unwrap();
    assert_eq!(provider, "deepseek");
    assert_eq!(model, "deepseek-v4-pro");
    assert_eq!(storage.session_mode(&child).unwrap(), "explore");
    assert_eq!(
        storage.session_child_role(&child).unwrap().as_deref(),
        Some("planner")
    );
    assert_eq!(child_summary.child_status.as_deref(), Some("running"));
}

#[test]
fn running_child_sessions_restore_as_failed_after_restart() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let parent = storage.create_session(root.path()).unwrap();
    let child = storage
        .create_child_session(
            root.path(),
            &parent,
            "openai",
            "gpt-5-mini",
            "child",
            "explore",
            "read_only",
        )
        .unwrap();

    storage.mark_running_children_interrupted().unwrap();

    let summary = storage
        .list_sessions(root.path())
        .unwrap()
        .into_iter()
        .find(|session| session.id == child)
        .unwrap();
    assert_eq!(summary.child_status.as_deref(), Some("failed"));
    assert!(storage
        .load_messages(&child)
        .unwrap()
        .iter()
        .any(|item| matches!(item, ConversationItem::Message { content, .. } if content == "[child interrupted by process restart]")));
}

#[test]
fn delete_session_soft_deletes_descendants() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let parent = storage.create_session(root.path()).unwrap();
    let child = storage
        .create_child_session(
            root.path(),
            &parent,
            "openai",
            "gpt-5-mini",
            "child",
            "explore",
            "reviewer",
        )
        .unwrap();
    let grandchild = storage
        .create_child_session(
            root.path(),
            &child,
            "openai",
            "gpt-5-mini",
            "grandchild",
            "explore",
            "reviewer",
        )
        .unwrap();

    let deleted = storage.delete_session(&parent).unwrap();
    assert_eq!(
        deleted,
        vec![parent.clone(), child.clone(), grandchild.clone()]
    );
    let sessions = storage.list_sessions(root.path()).unwrap();
    assert!(sessions.is_empty());

    // Directly deleting a child leaves other branches alone.
    let parent2 = storage.create_session(root.path()).unwrap();
    let child2 = storage
        .create_child_session(
            root.path(),
            &parent2,
            "openai",
            "gpt-5-mini",
            "child2",
            "explore",
            "reviewer",
        )
        .unwrap();
    storage.delete_session(&child2).unwrap();
    let sessions = storage.list_sessions(root.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, parent2);
    let _ = grandchild;
}

#[test]
fn stores_and_loads_messages_and_provider_state() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "hello")
        .unwrap();
    assert_eq!(storage.load_messages(&session).unwrap().len(), 1);
    storage.save_response_id(&session, "resp_1").unwrap();
    assert_eq!(
        storage.response_id(&session).unwrap().as_deref(),
        Some("resp_1")
    );
    assert_eq!(
        storage.latest_session(root.path()).unwrap().as_deref(),
        Some(session.as_str())
    );
    let sessions = storage.list_sessions(root.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
    assert_eq!(sessions[0].title, "hello");
}

#[test]
fn model_metadata_rows_roundtrip_and_batch_skips_empty_entries() {
    let storage = Storage::in_memory().unwrap();
    storage
        .save_model_metadata(
            "provider|https://gw/v1|big-model",
            "provider",
            Some(200_000),
            Some(32_768),
            None,
        )
        .unwrap();
    let row = storage
        .model_metadata("provider|https://gw/v1|big-model")
        .unwrap()
        .expect("row present");
    assert_eq!(row.source, "provider");
    assert_eq!(row.context_window_tokens, Some(200_000));
    assert_eq!(row.max_output_tokens, Some(32_768));
    assert!(row.fetched_at > 0);

    // A list row carries the payload and no per-model limits.
    storage
        .save_model_metadata(
            "provider-list|https://gw/v1",
            "provider",
            None,
            None,
            Some(r#"[{"id":"big-model"}]"#),
        )
        .unwrap();
    let list = storage
        .model_metadata("provider-list|https://gw/v1")
        .unwrap()
        .expect("list row present");
    assert_eq!(list.payload.as_deref(), Some(r#"[{"id":"big-model"}]"#));
    assert!(list.context_window_tokens.is_none());

    // Batch upserts skip empty entries and overwrite existing rows.
    let written = storage
        .save_model_metadata_batch(
            "community",
            &[
                ("community|big-model".to_owned(), Some(256_000), Some(8_192)),
                ("community|empty".to_owned(), None, None),
            ],
        )
        .unwrap();
    assert_eq!(written, 1);
    let community = storage
        .model_metadata("community|big-model")
        .unwrap()
        .expect("community row present");
    assert_eq!(community.context_window_tokens, Some(256_000));
    assert!(storage.model_metadata("community|empty").unwrap().is_none());

    // Missing keys are a plain miss, not an error.
    assert!(
        storage
            .model_metadata("provider|https://gw/v1|other-model")
            .unwrap()
            .is_none()
    );
}

#[test]
fn compaction_checkpoint_restores_raw_messages_and_clears_response_state() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage.append_message(&session, Role::User, "old").unwrap();
    storage
        .append_message(&session, Role::Assistant, "answer")
        .unwrap();
    storage
        .append_message(&session, Role::User, "latest")
        .unwrap();
    storage.save_response_id(&session, "resp").unwrap();
    assert_eq!(
        storage
            .compact_with_summary(&session, "goals and next step", 1)
            .unwrap(),
        2
    );
    assert!(storage.response_id(&session).unwrap().is_none());
    let compacted = storage.load_messages(&session).unwrap();
    assert!(
        compacted
            .iter()
            .any(|item| matches!(item, ConversationItem::CompactionSummary { .. }))
    );
    assert!(storage.restore_latest_compaction(&session).unwrap());
    assert!(
        storage.load_messages(&session).unwrap().iter().any(
            |item| matches!(item, ConversationItem::Message { content, .. } if content == "old")
        )
    );
    assert!(!storage.restore_latest_compaction(&session).unwrap());
}

#[test]
fn supports_fork_undo_redo_and_compaction() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage.append_message(&session, Role::User, "one").unwrap();
    storage
        .append_message(&session, Role::Assistant, "answer")
        .unwrap();
    storage.append_message(&session, Role::User, "two").unwrap();
    assert!(storage.undo(&session).unwrap());
    assert_eq!(storage.load_messages(&session).unwrap().len(), 2);
    assert!(storage.redo(&session).unwrap());
    assert_eq!(storage.load_messages(&session).unwrap().len(), 3);
    assert!(storage.compact_session(&session, 1).unwrap() >= 1);
    let fork = storage.fork_session(&session).unwrap();
    assert_eq!(storage.load_messages(&fork).unwrap().len(), 1);
}

#[test]
fn preserves_thinking_and_tool_order_for_display_restore() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "inspect")
        .unwrap();
    storage
        .append_thinking_summary(&session, "Checking the workspace")
        .unwrap();
    storage
        .append_message(&session, Role::Assistant, "I will inspect it.")
        .unwrap();
    let call = ToolCall {
        id: "call_1".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"src/lib.rs"}),
    };
    storage.append_tool_calls(&session, &[call]).unwrap();
    storage
        .append_tool_output(&session, "call_1", "contents")
        .unwrap();
    let items = storage.load_messages(&session).unwrap();
    assert!(matches!(items[1], ConversationItem::ThinkingSummary { .. }));
    assert!(matches!(
        items[3],
        ConversationItem::AssistantToolCalls { .. }
    ));
    assert!(matches!(items[4], ConversationItem::ToolOutput { .. }));
}

#[test]
fn persists_provider_items_for_stateless_responses_replay() {
    let root = tempfile::tempdir().unwrap();
    let storage = Storage::open(&root.path().join("agent.db")).unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "search")
        .unwrap();
    storage
        .append_provider_item(
            &session,
            &serde_json::json!({
                "id": "ws_1",
                "type": "web_search_call",
                "status": "completed",
                "action": {"type":"search", "query":"Rust"}
            }),
        )
        .unwrap();
    let items = storage.load_messages(&session).unwrap();
    assert!(matches!(
        &items[1],
        ConversationItem::ProviderItem { item } if item["id"] == "ws_1"
    ));
}

#[test]
fn todo_tasks_replace_list_and_are_copied_on_fork() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let first = TodoTask::new("first", TodoStatus::Pending);
    let second = TodoTask::new("second", TodoStatus::InProgress);

    storage
        .replace_tasks(&session, &[first.clone(), second])
        .unwrap();
    assert_eq!(storage.list_tasks(&session).unwrap().len(), 2);

    let first_updated = TodoTask {
        status: TodoStatus::Done,
        ..first
    };
    let third = TodoTask::new("third", TodoStatus::Pending);
    storage
        .replace_tasks(&session, &[first_updated.clone(), third])
        .unwrap();
    let tasks = storage.list_tasks(&session).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].id, first_updated.id);
    assert_eq!(tasks[0].status, TodoStatus::Done);
    assert_eq!(tasks[0].title, "first");
    assert_eq!(tasks[1].title, "third");

    let fork = storage.fork_session(&session).unwrap();
    let forked = storage.list_tasks(&fork).unwrap();
    assert_eq!(forked.len(), 2);
    assert_ne!(forked[0].id, tasks[0].id);
    assert_ne!(forked[1].id, tasks[1].id);
    assert_eq!(forked[1].title, tasks[1].title);
}

#[test]
fn todo_task_bounds_are_enforced() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let tasks = (0..51)
        .map(|index| TodoTask::new(format!("task {index}"), TodoStatus::Pending))
        .collect::<Vec<_>>();
    let error = storage.replace_tasks(&session, &tasks).unwrap_err();
    assert!(error.to_string().contains("at most 50 tasks"));

    let long = "x".repeat(241);
    let error = storage
        .replace_tasks(&session, &[TodoTask::new(long, TodoStatus::Pending)])
        .unwrap_err();
    assert!(error.to_string().contains("1 to 240 characters"));
}

#[test]
fn file_snapshots_round_trip_and_restore_by_turn() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let turn = storage.head_turn_id(&session).unwrap().unwrap();
    storage
        .snapshot_file(
            &session,
            &turn,
            "call_1",
            "src/a.txt",
            Some(b"before"),
            true,
            1024 * 1024,
            16 * 1024 * 1024,
        )
        .unwrap();
    storage.save_post_image("call_1", Some(b"after")).unwrap();

    let snapshots = storage.restore_turn_files(&session, &turn).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].path, "src/a.txt");
    assert_eq!(
        snapshots[0].pre_image.as_deref(),
        Some(b"before".as_slice())
    );
    assert_eq!(
        snapshots[0].post_image.as_deref(),
        Some(b"after".as_slice())
    );
    assert!(snapshots[0].existed);
}

#[test]
fn file_snapshot_over_limit_is_stored_as_marker() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let turn = storage.head_turn_id(&session).unwrap().unwrap();
    storage
        .snapshot_file(
            &session,
            &turn,
            "call_big",
            "big.bin",
            Some(&vec![0u8; 1000]),
            true,
            100, // tiny per-file cap
            16 * 1024 * 1024,
        )
        .unwrap();
    let snapshots = storage.restore_turn_files(&session, &turn).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert!(!snapshots[0].existed);
    assert!(snapshots[0].pre_image.is_none());
}

#[test]
fn file_snapshot_session_total_drops_oldest() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let turn = storage.head_turn_id(&session).unwrap().unwrap();
    storage
        .snapshot_file(
            &session,
            &turn,
            "call_1",
            "a.txt",
            Some(&[0u8; 60]),
            true,
            1024 * 1024,
            100, // tiny session cap: only one 60-byte row fits
        )
        .unwrap();
    storage
        .snapshot_file(
            &session,
            &turn,
            "call_2",
            "b.txt",
            Some(&[0u8; 60]),
            true,
            1024 * 1024,
            100,
        )
        .unwrap();
    let snapshots = storage.restore_turn_files(&session, &turn).unwrap();
    // First snapshot dropped by the session cap, second remains.
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].path, "b.txt");
}

#[test]
fn turns_between_returns_ordered_chain() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let root_turn = storage.head_turn_id(&session).unwrap().unwrap();
    storage.append_message(&session, Role::User, "one").unwrap();
    let turn1 = storage.head_turn_id(&session).unwrap().unwrap();
    storage.append_message(&session, Role::User, "two").unwrap();
    let turn2 = storage.head_turn_id(&session).unwrap().unwrap();
    let chain = storage.turns_between(&session, &root_turn, &turn2).unwrap();
    assert_eq!(chain, vec![turn1, turn2]);
}

#[test]
fn purge_soft_deleted_snapshots_cleans_sessions() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let turn = storage.head_turn_id(&session).unwrap().unwrap();
    storage
        .snapshot_file(
            &session,
            &turn,
            "call_1",
            "a.txt",
            Some(b"x"),
            true,
            1024 * 1024,
            16 * 1024 * 1024,
        )
        .unwrap();
    storage.delete_session(&session).unwrap();
    storage.purge_soft_deleted_snapshots().unwrap();
    let count: i64 = storage
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM file_snapshots WHERE session_id = ?1",
            [&session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn memory_lifecycle_is_bounded_and_reviewable() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let workspace = root.path().display().to_string();

    let candidate = storage
        .create_memory(
            &workspace,
            "preference",
            "Candidate title",
            "Candidate content",
            Some("preferences"),
            "candidate",
            Some(&session),
            None,
            None,
            Some("user said this"),
            8,
            4,
            1024,
            4096,
        )
        .unwrap();
    assert_eq!(candidate.status, "candidate");
    assert!(!candidate.recallable);

    let listed = storage
        .list_memories(&workspace, Some("Candidate"), false)
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].source_session_id.as_deref(),
        Some(session.as_str())
    );

    let confirmed = storage.confirm_memory(&workspace, candidate.id).unwrap();
    assert_eq!(confirmed.status, "active");
    assert!(confirmed.recallable);

    let updated = storage
        .update_memory(
            &workspace,
            candidate.id,
            "Updated title",
            "Updated content",
            1024,
        )
        .unwrap();
    assert_eq!(updated.title, "Updated title");
    assert_eq!(
        storage
            .recall_memories(&workspace, Some("Updated"), 8, 4096)
            .unwrap()
            .len(),
        1
    );

    storage.delete_memory(&workspace, candidate.id).unwrap();
    assert!(
        storage
            .list_memories(&workspace, None, false)
            .unwrap()
            .is_empty()
    );
    let deleted = storage.list_memories(&workspace, None, true).unwrap();
    assert_eq!(deleted[0].status, "deleted");
    assert!(!deleted[0].recallable);
}

#[test]
fn memory_source_invalidation_disables_recall_without_erasing_record() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let workspace = root.path().display().to_string();
    let memory = storage
        .create_memory(
            &workspace,
            "fact",
            "Source-bound fact",
            "Keep this for audit",
            None,
            "active",
            Some(&session),
            None,
            None,
            None,
            8,
            4,
            1024,
            4096,
        )
        .unwrap();
    assert!(memory.recallable);
    storage.delete_session(&session).unwrap();

    let listed = storage.list_memories(&workspace, None, false).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].recallable);
    assert!(
        storage
            .recall_memories(&workspace, None, 8, 4096)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn undo_invalidates_memory_supported_only_by_detached_turn() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    let workspace = root.path().display().to_string();
    storage
        .append_message(&session, Role::User, "first turn")
        .unwrap();
    storage
        .append_message(&session, Role::User, "turn to undo")
        .unwrap();
    let detached_turn = storage.head_turn_id(&session).unwrap().unwrap();
    let memory = storage
        .create_memory(
            &workspace,
            "decision",
            "Detached decision",
            "This only came from the second turn",
            None,
            "active",
            Some(&session),
            Some(&detached_turn),
            None,
            None,
            8,
            4,
            1024,
            4096,
        )
        .unwrap();
    assert!(memory.recallable);

    assert!(storage.undo(&session).unwrap());
    let listed = storage.list_memories(&workspace, None, false).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].recallable);
}

#[test]
fn bounded_history_drops_oversized_rows_with_notice() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage.append_message(&session, Role::User, "old").unwrap();
    storage
        .append_message(&session, Role::User, "this message is too long")
        .unwrap();
    storage.append_message(&session, Role::User, "new").unwrap();

    let items = storage.load_messages_bounded(&session, 2, 1024, 8).unwrap();
    assert!(items.iter().any(|item| matches!(
        item,
        ConversationItem::Message { content, .. }
            if content.contains("Earlier history was omitted")
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        ConversationItem::Message { content, .. } if content == "new"
    )));
    assert!(!items.iter().any(|item| matches!(
        item,
        ConversationItem::Message { content, .. } if content == "this message is too long"
    )));
}

#[test]
fn partial_is_saved_replaced_loaded_and_cleared() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();

    assert!(storage.load_partial(&session).unwrap().is_none());

    storage.save_partial(&session, "half an answer").unwrap();
    let partial = storage.load_partial(&session).unwrap().unwrap();
    assert_eq!(partial.content, "half an answer");
    assert_eq!(partial.role, "assistant");

    // Saving again replaces the previous partial.
    storage
        .save_partial(&session, "half an answer, continued")
        .unwrap();
    let partial = storage.load_partial(&session).unwrap().unwrap();
    assert_eq!(partial.content, "half an answer, continued");

    // Clearing removes it.
    storage.clear_partial(&session).unwrap();
    assert!(storage.load_partial(&session).unwrap().is_none());
}

#[test]
fn partial_index_migrates_existing_database_and_bounds_snapshot_lookup() {
    let root = tempdir().unwrap();
    let database = root.path().join("partial-index.db");
    let storage = Storage::open(&database).unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "formal history")
        .unwrap();
    storage.save_partial(&session, "retained partial").unwrap();
    {
        let connection = storage.lock().unwrap();
        connection
            .execute_batch(
                "DROP INDEX idx_messages_partial_session_id;
             DELETE FROM schema_migrations WHERE version = 10;",
            )
            .unwrap();
    }
    drop(storage);
    for _ in 0..2 {
        let storage = Storage::open(&database).unwrap();
        assert_eq!(
            storage.load_partial(&session).unwrap().unwrap().content,
            "retained partial"
        );
        let connection = storage.lock().unwrap();
        let mut query = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id, role, content, kind, metadata, created_at
             FROM messages WHERE session_id = ?1 AND partial = 1 ORDER BY id DESC LIMIT 1",
            )
            .unwrap();
        let plan = query
            .query_map([&session], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" ");
        assert!(plan.contains("idx_messages_partial_session_id"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
    let storage = Storage::open(&database).unwrap();
    storage.clear_partial(&session).unwrap();
    assert!(storage.load_partial(&session).unwrap().is_none());
    assert_eq!(storage.load_messages(&session).unwrap().len(), 1);
}

#[test]
fn partial_is_filtered_from_history_page_and_does_not_trip_cursor() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();

    storage
        .append_message(&session, Role::User, "first")
        .unwrap();
    storage
        .append_message(&session, Role::User, "second")
        .unwrap();
    storage.save_partial(&session, "incomplete answer").unwrap();

    // The normal history page excludes the partial row.
    let page = storage.load_message_page(&session, None, 100).unwrap();
    assert_eq!(page.len(), 2);
    assert!(page.iter().all(|row| row.content != "incomplete answer"));
}

#[test]
fn cached_history_pages_follow_cursor_and_active_branch_changes() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "first question")
        .unwrap();
    storage
        .append_message(&session, Role::Assistant, "first answer")
        .unwrap();
    storage
        .append_message(&session, Role::User, "second question")
        .unwrap();
    storage
        .append_message(&session, Role::Assistant, "second answer")
        .unwrap();

    let newest = storage.load_message_page(&session, None, 2).unwrap();
    assert_eq!(
        newest
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["second answer", "second question"]
    );
    let older = storage
        .load_message_page(&session, newest.last().map(|message| message.id), 2)
        .unwrap();
    assert_eq!(
        older
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["first answer", "first question"]
    );

    assert!(storage.undo(&session).unwrap());
    let after_undo = storage.load_message_page(&session, None, 10).unwrap();
    assert_eq!(
        after_undo
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["first answer", "first question"]
    );
    storage
        .append_message(&session, Role::User, "alternate question")
        .unwrap();
    storage
        .append_message(&session, Role::Assistant, "alternate answer")
        .unwrap();
    let alternate = storage.load_message_page(&session, None, 10).unwrap();
    assert_eq!(
        alternate
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        [
            "alternate answer",
            "alternate question",
            "first answer",
            "first question"
        ]
    );
}

#[test]
fn visible_history_matches_recursive_oracle_across_mutations_and_fork() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    for text in ["u1", "a1", "u2", "a2", "u3", "a3"] {
        storage
            .append_message(
                &session,
                if text.starts_with('u') {
                    Role::User
                } else {
                    Role::Assistant
                },
                text,
            )
            .unwrap();
        assert_history_pages_match_recursive_oracle(&storage, &session);
    }

    {
        let connection = storage.lock().unwrap();
        let turn = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [&session],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, partial)
                 VALUES (?1, 'context', 'hidden row', '2026-01-01', ?2, 'context', 0, 0)",
                params![session, turn],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE messages SET hidden = 1 WHERE content = 'hidden row'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, partial)
                 VALUES (?1, 'context', 'null turn row', '2026-01-02', NULL, 'context', 0, 0)",
                [&session],
            )
            .unwrap();
    }
    storage.save_partial(&session, "partial row").unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &session);
    storage
        .append_context(&session, "test-context", "typed active message")
        .unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &session);

    assert!(storage.undo(&session).unwrap());
    assert_history_pages_match_recursive_oracle(&storage, &session);
    assert!(storage.undo(&session).unwrap());
    assert_history_pages_match_recursive_oracle(&storage, &session);
    assert!(storage.redo(&session).unwrap());
    assert_history_pages_match_recursive_oracle(&storage, &session);
    assert!(storage.redo(&session).unwrap());
    assert_history_pages_match_recursive_oracle(&storage, &session);
    assert!(storage.undo(&session).unwrap());
    storage
        .append_message(&session, Role::User, "new branch")
        .unwrap();
    storage
        .append_message(&session, Role::Assistant, "new branch answer")
        .unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &session);

    storage
        .compact_with_summary(&session, "summary", 4)
        .unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &session);
    assert!(storage.restore_latest_compaction(&session).unwrap());
    assert_history_pages_match_recursive_oracle(&storage, &session);
    storage.compact_session(&session, 2).unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &session);

    let fork = storage.fork_session(&session).unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &fork);
    let other = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&other, Role::User, "isolated")
        .unwrap();
    assert_history_pages_match_recursive_oracle(&storage, &other);
    storage.delete_session(&other).unwrap();
    // Soft deletion historically leaves direct message paging readable.
    assert_history_pages_match_recursive_oracle(&storage, &other);
}

#[test]
fn visible_history_migration_rolls_back_and_retries_once() {
    let root = tempdir().unwrap();
    let database = root.path().join("migration.db");
    let storage = Storage::open(&database).unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage
        .append_message(&session, Role::User, "before migration")
        .unwrap();
    drop(storage);

    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("DELETE FROM schema_migrations WHERE version = 9", [])
            .unwrap();
        connection
            .execute("UPDATE turns SET on_active_path = 0", [])
            .unwrap();
        connection
            .execute("DELETE FROM visible_history", [])
            .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_visible_history_migration
                 BEFORE INSERT ON visible_history
                 BEGIN SELECT RAISE(ABORT, 'injected migration interruption'); END;",
            )
            .unwrap();
    }
    assert!(Storage::open(&database).is_err());
    {
        let connection = Connection::open(&database).unwrap();
        let migration_done: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = 9)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let active_turns: i64 = connection
            .query_row(
                "SELECT count(*) FROM turns WHERE on_active_path = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let visible_rows: i64 = connection
            .query_row("SELECT count(*) FROM visible_history", [], |row| row.get(0))
            .unwrap();
        assert!(!migration_done);
        assert_eq!(active_turns, 0);
        assert_eq!(visible_rows, 0);
        connection
            .execute("DROP TRIGGER fail_visible_history_migration", [])
            .unwrap();
    }

    let migrated = Storage::open(&database).unwrap();
    assert_history_pages_match_recursive_oracle(&migrated, &session);
    let connection = migrated.lock().unwrap();
    let active_turns: i64 = connection
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id = ?1 AND on_active_path = 1",
            [&session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_turns, 2);
    drop(connection);
    drop(migrated);

    // Once version 9 is committed, open preserves the version marker.
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE schema_migrations SET applied_at = 'migration-complete' WHERE version = 9",
            [],
        )
        .unwrap();
    drop(connection);
    let reopened = Storage::open(&database).unwrap();
    assert_eq!(
        reopened
            .load_message_page(&session, None, 10)
            .unwrap()
            .len(),
        1
    );
    let connection = reopened.lock().unwrap();
    let marker: String = connection
        .query_row(
            "SELECT applied_at FROM schema_migrations WHERE version = 9",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(marker, "migration-complete");
}

#[test]
fn visible_history_migrates_an_unmodified_legacy_schema() {
    let root = tempdir().unwrap();
    let database = root.path().join("legacy-v8.db");
    let session = "legacy-session";
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
                 INSERT INTO schema_migrations VALUES (8, 'legacy-v8');
                 CREATE TABLE sessions(
                     id TEXT PRIMARY KEY, workspace TEXT NOT NULL, title TEXT NOT NULL,
                     created_at TEXT NOT NULL, updated_at TEXT NOT NULL, head_turn_id TEXT
                 );
                 CREATE TABLE turns(
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                     parent_id TEXT, created_at TEXT NOT NULL
                 );
                 CREATE TABLE messages(
                     id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                     role TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL,
                     turn_id TEXT, kind TEXT NOT NULL DEFAULT 'message',
                     hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions(id, workspace, title, created_at, updated_at, head_turn_id)
                 VALUES (?1, 'legacy', 'legacy', '2026-01-01', '2026-01-01', 'turn-2')",
                [session],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turns(id, session_id, parent_id, created_at)
                 VALUES ('turn-1', ?1, NULL, '2026-01-01'),
                        ('turn-2', ?1, 'turn-1', '2026-01-02'),
                        ('off-branch', ?1, 'turn-1', '2026-01-03')",
                [session],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(session_id, role, content, created_at, turn_id)
                 VALUES (?1, 'user', 'ancestor', '2026-01-01', 'turn-1'),
                        (?1, 'assistant', 'head', '2026-01-02', 'turn-2'),
                        (?1, 'assistant', 'other branch', '2026-01-03', 'off-branch'),
                        (?1, 'context', 'legacy null turn', '2026-01-04', NULL)",
                [session],
            )
            .unwrap();
    }

    let storage = Storage::open(&database).unwrap();
    assert_history_pages_match_recursive_oracle(&storage, session);
    let page = storage.load_message_page(session, None, 10).unwrap();
    let content = page
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(content, ["legacy null turn", "head", "ancestor"]);
    let connection = storage.lock().unwrap();
    let active_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id = ?1 AND on_active_path = 1",
            [session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_count, 2);
    let partial_column_exists = connection
        .prepare("PRAGMA table_info(messages)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .iter()
        .any(|column| column == "partial");
    assert!(partial_column_exists);
}

#[test]
fn visible_history_latest_and_cursor_queries_use_primary_key_ranges() {
    let storage = Storage::in_memory().unwrap();
    let connection = storage.lock().unwrap();
    for sql in [
        "EXPLAIN QUERY PLAN
         SELECT m.id, m.role, m.content, m.kind, m.metadata, m.created_at
         FROM visible_history AS v JOIN messages AS m ON m.id = v.message_id
         WHERE v.session_id = 's' ORDER BY v.message_id DESC LIMIT 101",
        "EXPLAIN QUERY PLAN
         SELECT m.id, m.role, m.content, m.kind, m.metadata, m.created_at
         FROM visible_history AS v JOIN messages AS m ON m.id = v.message_id
         WHERE v.session_id = 's' AND v.message_id < 101
         ORDER BY v.message_id DESC LIMIT 101",
    ] {
        let details = connection
            .prepare(sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let plan = details.join(" | ");
        assert!(plan.contains("SEARCH v USING PRIMARY KEY"), "{plan}");
        assert!(
            plan.contains("SEARCH m USING INTEGER PRIMARY KEY"),
            "{plan}"
        );
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
}

#[test]
fn redo_child_lookup_uses_the_turn_parent_index() {
    let storage = Storage::in_memory().unwrap();
    let connection = storage.lock().unwrap();
    let details = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT id FROM turns
             WHERE session_id = 's' AND parent_id = 't'
             ORDER BY created_at DESC LIMIT 1",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let plan = details.join(" | ");
    assert!(
        plan.contains("SEARCH turns USING INDEX idx_turns_session_parent_created"),
        "{plan}"
    );
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
}

#[test]
fn empty_partial_clears_existing_partial() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage.save_partial(&session, "some text").unwrap();
    storage.save_partial(&session, "   ").unwrap();
    assert!(storage.load_partial(&session).unwrap().is_none());
}
