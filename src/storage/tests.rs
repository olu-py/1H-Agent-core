use super::*;
use tempfile::tempdir;

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
fn empty_partial_clears_existing_partial() {
    let storage = Storage::in_memory().unwrap();
    let root = tempdir().unwrap();
    let session = storage.create_session(root.path()).unwrap();
    storage.save_partial(&session, "some text").unwrap();
    storage.save_partial(&session, "   ").unwrap();
    assert!(storage.load_partial(&session).unwrap().is_none());
}
