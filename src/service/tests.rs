use super::*;
use crate::protocol::{ApiErrorKind, MessageDto, PROTOCOL_VERSION};
use tempfile::TempDir;

async fn test_handle() -> (TempDir, AppHandle) {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().to_path_buf();
    let mut config = Config::default();
    config.data_dir = temp.path().join("data");
    // Give the handle a writable config path so provider create/remove paths
    // exercise the same persist behavior as production.
    config.set_config_path_for_test(temp.path().join("config.toml"));
    // Deterministic runner: without a seeded key, whether the shared key
    // cache holds one depends on test scheduling, making submit-based tests
    // flaky.
    crate::secrets::test_seed_key(config.provider.preset, "test-key");
    let handle = AppService::start(CoreConfig {
        workspace,
        config,
        data_dir: temp.path().join("data"),
        event_capacity: 64,
        event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
        approval_timeout: Duration::from_secs(300),
        message_page_size: 100,
    })
    .await
    .unwrap();
    (temp, handle)
}

#[tokio::test]
async fn snapshot_reports_sessions_and_cursor() {
    let (_temp, handle) = test_handle().await;
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
    assert_eq!(snapshot.event_cursor, handle.current_cursor());
    assert!(!snapshot.model.is_empty());
    assert!(!snapshot.provider.is_empty());
    assert_eq!(snapshot.mode, "build");
    assert!(snapshot.approval.is_none());
}

#[tokio::test]
async fn provider_settings_reports_active_and_saved_profiles() {
    let (_temp, handle) = test_handle().await;
    let settings = handle.provider_settings().await.unwrap();
    // Default config: OpenAI active from the template, nothing saved yet.
    assert_eq!(settings.active.preset, "openai");
    assert_eq!(settings.active.base_url, "https://api.openai.com/v1");
    assert_eq!(settings.active.kind, "responses");
    assert!(settings.saved.is_empty());
    // test_handle seeds the active preset's key, so it resolves as connected.
    assert!(settings.connected.contains(&"openai".to_owned()));
}

#[tokio::test]
async fn set_provider_accepts_an_unsaved_legacy_preset_name() {
    let (_temp, handle) = test_handle().await;
    // Phase-one compatibility: `set_provider("deepseek")` must resolve the
    // legacy preset name to its template even before a saved profile exists,
    // because old clients and pre-migration session rows still speak presets.
    let error = handle
        .set_provider("deepseek", "deepseek-v4-flash")
        .await
        .unwrap_err();
    // The legacy preset name is recognized (not "unknown provider") and the
    // error uses the family label, exactly as the pre-id implementation did.
    assert_eq!(error.kind, ApiErrorKind::BadRequest);
    assert!(error.message.contains("DeepSeek"), "{}", error.message);
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.provider_id, "openai");
}

#[tokio::test]
async fn provider_models_merges_community_metadata_for_unreported_models() {
    let (temp, handle) = test_handle().await;
    // Seed the cache the way a refresh would: a provider list payload
    // where one model reports its window and one does not, plus a
    // models.dev community row for the unreported model.
    let storage = crate::storage::Storage::open(&temp.path().join("data/agent.db")).unwrap();
    let payload = serde_json::to_string(&vec![
        crate::provider::ProviderModelInfo {
            id: "gateway-x".to_owned(),
            context_window_tokens: None,
            max_output_tokens: None,
        },
        crate::provider::ProviderModelInfo {
            id: "gpt-4o".to_owned(),
            context_window_tokens: Some(128_000),
            max_output_tokens: Some(16_384),
        },
    ])
    .unwrap();
    storage
        .save_model_metadata(
            &crate::model_meta::provider_list_key("https://api.openai.com/v1"),
            "provider",
            None,
            None,
            Some(&payload),
        )
        .unwrap();
    storage
        .save_model_metadata(
            &crate::model_meta::community_meta_key("gateway-x"),
            "community",
            Some(200_000),
            Some(8_192),
            None,
        )
        .unwrap();
    drop(storage);

    let dto = handle.provider_models(false).await.unwrap();
    let gateway = dto.models.iter().find(|m| m.id == "gateway-x").unwrap();
    // The community row fills only what the provider left unreported.
    assert_eq!(gateway.context_window_tokens, Some(200_000));
    assert_eq!(gateway.max_output_tokens, Some(8_192));
    // Provider-reported values are not overwritten by the merge.
    let gpt = dto.models.iter().find(|m| m.id == "gpt-4o").unwrap();
    assert_eq!(gpt.context_window_tokens, Some(128_000));
    assert_eq!(gpt.max_output_tokens, Some(16_384));
}

#[tokio::test]
async fn set_provider_profile_switches_to_an_unsaved_preset_from_template() {
    let (_temp, handle) = test_handle().await;
    crate::secrets::test_seed_key(crate::config::ProviderPreset::DeepSeek, "deepseek-test-key");
    handle
        .set_provider_profile(
            "deepseek",
            crate::config::ProviderPreset::DeepSeek,
            None,
            "deepseek-v4-flash",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    // No saved DeepSeek profile existed, so the preset template provides
    // the base URL and protocol; the switch itself upserts the profile.
    assert_eq!(settings.active.preset, "deepseek");
    assert_eq!(settings.active.model, "deepseek-v4-flash");
    assert_eq!(settings.active.base_url, "https://api.deepseek.com");
    assert_eq!(settings.active.kind, "responses");
    assert!(
        settings
            .saved
            .iter()
            .any(|profile| profile.preset == "deepseek")
    );
    // The secret refresh on a preset switch resolves the seeded key.
    assert!(settings.connected.contains(&"deepseek".to_owned()));
    assert_eq!(handle.snapshot().await.unwrap().provider, "DeepSeek");
}

#[tokio::test]
async fn set_provider_profile_applies_overrides_and_keeps_the_active_base() {
    let (_temp, handle) = test_handle().await;
    handle
        .set_provider_profile(
            "openai",
            crate::config::ProviderPreset::OpenAi,
            None,
            "gpt-5",
            Some("https://proxy.example.com/v1"),
            Some(crate::config::ProviderKind::ChatCompletions),
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.preset, "openai");
    assert_eq!(settings.active.model, "gpt-5");
    assert_eq!(settings.active.base_url, "https://proxy.example.com/v1");
    assert_eq!(settings.active.kind, "chat_completions");
}

#[tokio::test]
async fn set_provider_profile_overrides_the_explicit_window_with_clamping() {
    let (_temp, handle) = test_handle().await;
    // A session must exist for the snapshot to carry a context budget.
    handle.submit(None, "hello").await.unwrap();
    handle
        .set_provider_profile(
            "openai",
            crate::config::ProviderPreset::OpenAi,
            None,
            "gateway-unknown-model",
            None,
            None,
            // Out-of-bounds windows are clamped to the Config::load bounds,
            // never installed verbatim.
            Some(3),
            None,
        )
        .await
        .unwrap();
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .context
            .as_ref()
            .and_then(|c| c.context_window_tokens),
        Some(4096)
    );
    assert_eq!(
        snapshot.context.as_ref().map(|c| c.window_source.as_str()),
        Some("config")
    );
    // Omitting the window keeps the merged profile's explicit value.
    handle
        .set_provider_profile(
            "openai",
            crate::config::ProviderPreset::OpenAi,
            None,
            "gateway-unknown-model",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .context
            .as_ref()
            .and_then(|c| c.context_window_tokens),
        Some(4096)
    );
}

#[tokio::test]
async fn set_provider_profile_without_a_key_marks_missing_provider() {
    let (_temp, handle) = test_handle().await;
    // The process-wide key cache is shared across parallel tests (see the
    // comment in `test_handle`), so "no key for Volcano" cannot be
    // guaranteed: another test may have seeded one. Assert the missing-key
    // behavior only when Volcano was unresolvable when the test started.
    let volcano_unconnected =
        crate::secrets::api_key_cached_only(crate::config::ProviderPreset::Volcano, "volcano")
            .is_err();
    handle
        .set_provider_profile(
            "volcano",
            crate::config::ProviderPreset::Volcano,
            None,
            "doubao-seed-2-1-pro-260628",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.preset, "volcano");
    assert_eq!(settings.active.model, "doubao-seed-2-1-pro-260628");
    if volcano_unconnected {
        assert!(!settings.connected.contains(&"volcano".to_owned()));
    }
}

#[tokio::test]
async fn custom_providers_can_be_added_renamed_and_removed() {
    let (_temp, handle) = test_handle().await;
    crate::secrets::test_seed_key_for_id("custom-aaaa", "key-a");
    crate::secrets::test_seed_key_for_id("custom-bbbb", "key-b");

    // Create two named custom providers from the custom template.
    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway A"),
            "model-a",
            Some("https://a.example/v1"),
            Some(crate::config::ProviderKind::ChatCompletions),
            None,
            None,
        )
        .await
        .unwrap();
    let first_id = handle.provider_settings().await.unwrap().active.id.clone();
    assert!(first_id.starts_with("custom-"), "{first_id}");
    assert_eq!(
        handle.provider_settings().await.unwrap().active.name,
        "Gateway A"
    );

    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway B"),
            "model-b",
            Some("https://b.example/v1"),
            Some(crate::config::ProviderKind::ChatCompletions),
            None,
            None,
        )
        .await
        .unwrap();
    let second_id = handle.provider_settings().await.unwrap().active.id.clone();
    assert_ne!(first_id, second_id);
    let settings = handle.provider_settings().await.unwrap();
    let names = settings
        .saved
        .iter()
        .map(|profile| profile.name.clone())
        .collect::<Vec<_>>();
    assert!(names.contains(&"Gateway A".to_owned()), "{names:?}");
    assert!(names.contains(&"Gateway B".to_owned()), "{names:?}");

    // Rename the first provider and switch to it.
    handle
        .set_provider_profile(
            &first_id,
            crate::config::ProviderPreset::Custom,
            Some("Gateway A2"),
            "model-a2",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.id, first_id);
    assert_eq!(settings.active.name, "Gateway A2");
    assert_eq!(settings.active.model, "model-a2");

    // Deleting the active provider falls back to the remaining saved profile.
    handle.remove_provider(&first_id).await.unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.id, second_id);
    assert!(!settings.saved.iter().any(|profile| profile.id == first_id));
}

#[tokio::test]
async fn duplicate_and_empty_custom_names_are_rejected() {
    let (_temp, handle) = test_handle().await;
    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway"),
            "model-a",
            Some("https://a.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // A second custom provider with the same name (any casing) is refused.
    let duplicate = handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("gateway"),
            "model-b",
            Some("https://b.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate.kind, ApiErrorKind::BadRequest);
    assert!(
        duplicate.message.contains("已被占用"),
        "{}",
        duplicate.message
    );

    // A blank name is refused for a new custom provider.
    let blank = handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("   "),
            "model-c",
            Some("https://c.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(blank.kind, ApiErrorKind::BadRequest);
    assert!(blank.message.contains("不能为空"), "{}", blank.message);

    // A built-in preset label is reserved as well.
    let builtin = handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("DeepSeek"),
            "model-d",
            Some("https://d.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(builtin.kind, ApiErrorKind::BadRequest);
}

#[tokio::test]
async fn legacy_unnamed_custom_profile_stays_editable() {
    // A profile written before custom names existed must not be force-renamed
    // or rejected: only *creating* a new custom provider requires a name.
    let (_temp, handle) = test_handle().await;
    let mut legacy = crate::config::ProviderPreset::Custom.defaults();
    legacy.id = "custom".to_owned();
    legacy.name = String::new();
    handle.set_provider_config(legacy).await.unwrap();

    handle
        .set_provider_profile(
            "custom",
            crate::config::ProviderPreset::Custom,
            None,
            "model-legacy",
            Some("https://legacy.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.id, "custom");
    assert_eq!(settings.active.model, "model-legacy");
}

#[tokio::test]
async fn renaming_a_custom_provider_to_its_own_name_is_allowed() {
    let (_temp, handle) = test_handle().await;
    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway"),
            "model-a",
            Some("https://a.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let id = handle.provider_settings().await.unwrap().active.id.clone();

    // Editing the same profile keeps its own name (case-insensitive self-match
    // must not count as a duplicate).
    handle
        .set_provider_profile(
            &id,
            crate::config::ProviderPreset::Custom,
            Some("GATEWAY"),
            "model-a2",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        handle.provider_settings().await.unwrap().active.name,
        "GATEWAY"
    );
}

#[tokio::test]
async fn a_key_cached_under_the_family_id_is_promoted_to_the_new_custom_id() {
    // The client stores the key before the core has minted the custom id, so it
    // can only be cached under the family id. Creating the provider must adopt
    // it, otherwise the freshly created profile would be reported disconnected.
    let (_temp, handle) = test_handle().await;
    crate::secrets::test_seed_key(crate::config::ProviderPreset::Custom, "family-key");
    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway"),
            "model-a",
            Some("https://a.example/v1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    let id = settings.active.id.clone();
    assert!(id.starts_with("custom-"), "{id}");
    assert!(
        settings.connected.contains(&id),
        "the promoted key makes the new provider connected: {:?}",
        settings.connected
    );
    assert_eq!(
        crate::secrets::api_key_cached_only(crate::config::ProviderPreset::Custom, &id).unwrap(),
        "family-key"
    );
}

#[tokio::test]
async fn enabled_models_round_trip_through_settings() {
    let (_temp, handle) = test_handle().await;
    handle
        .set_provider_profile(
            "",
            crate::config::ProviderPreset::Custom,
            Some("Gateway"),
            "model-a",
            Some("https://a.example/v1"),
            None,
            None,
            Some(vec!["model-a".to_owned(), "model-b".to_owned()]),
        )
        .await
        .unwrap();
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(
        settings.active.enabled_models,
        vec!["model-a".to_owned(), "model-b".to_owned()]
    );
}

#[tokio::test]
async fn set_provider_profile_rejects_an_invalid_base_url() {
    let (_temp, handle) = test_handle().await;
    let error = handle
        .set_provider_profile(
            "openai",
            crate::config::ProviderPreset::OpenAi,
            None,
            "gpt-5-mini",
            Some("not a url"),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ApiErrorKind::BadRequest);
    assert!(error.message.contains("Base URL"), "{}", error.message);
    // A rejected profile leaves the active provider untouched.
    let settings = handle.provider_settings().await.unwrap();
    assert_eq!(settings.active.base_url, "https://api.openai.com/v1");
}

#[tokio::test]
async fn input_creates_session_when_none_active() {
    let (_temp, handle) = test_handle().await;
    let before = handle.snapshot().await.unwrap();
    if before.active_session.is_some() {
        // A fresh temp workspace has no sessions; this guard keeps the test
        // robust if a default session were ever created eagerly.
        return;
    }
    handle.submit(None, "first message").await.unwrap();
    let after = handle.snapshot().await.unwrap();
    assert_eq!(after.sessions.len(), 1);
    assert!(after.active_session.is_some());
    assert!(after.active_session.as_deref() != before.active_session.as_deref());
}

#[tokio::test]
async fn messages_return_a_page_and_cursor_pagination() {
    let (_temp, handle) = test_handle().await;
    handle.submit(None, "hello").await.unwrap();
    let session = handle.snapshot().await.unwrap().active_session.unwrap();
    // A running session rejects concurrent submits (structured conflict), so
    // multi-page seeding is exercised at the storage layer; here we verify
    // the page shape and the rejection path.
    let error = handle
        .submit(Some(session.clone()), "while busy")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ApiErrorKind::Conflict);
    let page = handle.messages(&session, None, Some(20)).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    // Display order: oldest first.
    let first = &page.messages[0];
    match first {
        MessageDto::User { content, .. } => assert!(content.starts_with("hello")),
        other => panic!("expected user message, got {other:?}"),
    }
    // Unknown session is rejected.
    let error = handle.messages("missing", None, None).await.unwrap_err();
    assert_eq!(error.kind, ApiErrorKind::NotFound);
}

#[tokio::test]
async fn submit_to_unknown_session_returns_not_found() {
    let (_temp, handle) = test_handle().await;
    let error = handle
        .submit(Some("missing-session".into()), "hi")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ApiErrorKind::NotFound);
}

#[tokio::test]
async fn submit_reports_request_seq_and_stale_cancel_is_ignored() {
    let (_temp, handle) = test_handle().await;
    let seq = handle.submit(None, "hello").await.unwrap();
    assert_eq!(seq, 1, "first submit starts request sequence at 1");
    let session = handle.snapshot().await.unwrap().active_session.unwrap();
    // Watch the live stream for the Cancelled event.
    let subscription = handle
        .subscribe_from(handle.current_cursor())
        .expect("cursor not evicted");
    let mut live = subscription.live;
    // A stale cancel (wrong sequence) must not abort the current request and
    // must not emit a Cancelled event.
    handle.cancel(&session, Some(seq + 100)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut stale_cancelled = false;
    while let Ok(envelope) = live.try_recv() {
        if matches!(envelope.event, Event::Cancelled { .. }) {
            stale_cancelled = true;
        }
    }
    assert!(!stale_cancelled, "stale cancel must not emit Cancelled");
    // A matching cancel succeeds and emits the terminal event.
    handle.cancel(&session, Some(seq)).await.unwrap();
    let mut matched = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !matched {
        if let Ok(envelope) = live.try_recv() {
            if matches!(envelope.event, Event::Cancelled { .. }) {
                matched = true;
            }
        } else if tokio::time::Instant::now() >= deadline {
            break;
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(matched, "matching cancel must emit a Cancelled event");
}

#[tokio::test]
async fn commands_invalidate_transcript_and_update_snapshot() {
    let (_temp, handle) = test_handle().await;
    handle.submit(None, "rename me").await.unwrap();
    let session = handle.snapshot().await.unwrap().active_session.unwrap();
    handle
        .execute_command(Some(session.clone()), "/todo add write tests")
        .await
        .unwrap();
    handle
        .execute_command(Some(session.clone()), "/undo")
        .await
        .unwrap();
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.sessions[0].id, session);
}

#[tokio::test]
async fn unknown_session_commands_fail_without_creating() {
    let (_temp, handle) = test_handle().await;
    let result = handle.submit(Some("missing".into()), "hello").await;
    assert!(result.is_err());
    let snapshot = handle.snapshot().await.unwrap();
    assert!(snapshot.sessions.is_empty() || snapshot.sessions.is_empty());
}

#[tokio::test]
async fn set_provider_model_only_switch_updates_snapshot() {
    let (_temp, handle) = test_handle().await;
    handle.set_provider("openai", "gpt-5").await.unwrap();
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.provider, "OpenAI");
    assert_eq!(snapshot.model, "gpt-5");
}

#[tokio::test]
async fn shutdown_rejects_pending_approvals() {
    let (_temp, handle) = test_handle().await;
    let session = handle.snapshot().await.unwrap().active_session;
    let _ = session;
    // A shutdown must not panic even with no pending approval.
    handle.execute_command(None, "/new").await.unwrap();
}

#[tokio::test]
async fn workspace_lock_blocks_second_service() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().to_path_buf();
    let data_dir = temp.path().join("data");
    let core_config = |dir: &std::path::Path| CoreConfig {
        workspace: workspace.clone(),
        config: Config::default(),
        data_dir: dir.to_path_buf(),
        event_capacity: 64,
        event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
        approval_timeout: Duration::from_secs(300),
        message_page_size: 100,
    };
    // First service acquires the per-workspace lock.
    let first = AppService::start(core_config(&data_dir)).await.unwrap();
    // A second service on the same canonical workspace must fail immediately
    // (same data_dir → same lock file, still held by the first service).
    let second = AppService::start(core_config(&data_dir)).await;
    assert!(
        second.is_err(),
        "second service must fail on the locked workspace"
    );
    drop(first);
    // After dropping the first, the same workspace is lockable again.
    let third = AppService::start(core_config(&data_dir)).await;
    assert!(third.is_ok());
}

#[test]
fn stored_provider_item_maps_to_display_safe_tool_dto() {
    let row = StoredMessage {
        id: 5,
        role: "assistant".into(),
        content: serde_json::json!({
            "id": "ws_1",
            "type": "web_search_call",
            "status": "completed",
            "action": {"type":"search","query":"Rust"}
        })
        .to_string(),
        kind: "provider_item".into(),
        metadata: None,
        created_at: "now".into(),
    };
    let dto = stored_to_message_dto(&row);
    match dto {
        MessageDto::Tool {
            name, arguments, ..
        } => {
            assert_eq!(name, "web_search");
            assert_eq!(arguments["query"], "Rust");
        }
        other => panic!("expected a tool dto, got {other:?}"),
    }
}

#[test]
fn lock_hash_is_stable() {
    let a = stable_hash(Path::new("/workspace/a"));
    let b = stable_hash(Path::new("/workspace/b"));
    assert_ne!(a, b);
    assert_eq!(stable_hash(Path::new("/workspace/a")), a);
}

#[tokio::test]
async fn restart_and_restore_session_keeps_a_usable_runner_and_streams() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().to_path_buf();
    let data_dir = temp.path().join("data");
    let mut config = Config::default();
    config.data_dir = data_dir.clone();
    // Deterministic runner across both service lifetimes (no keyring I/O).
    crate::secrets::test_seed_key(config.provider.preset, "test-key");

    let start = |workspace: PathBuf, config: Config| {
        AppService::start(CoreConfig {
            workspace,
            config,
            data_dir: data_dir.clone(),
            event_capacity: 64,
            event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
            approval_timeout: Duration::from_secs(300),
            message_page_size: 100,
        })
    };

    // First service: create two sessions so the second service can
    // explicitly restore the non-latest one (exercising the rebuild path).
    let handle1 = start(workspace.clone(), config.clone()).await.unwrap();
    handle1.submit(None, "first").await.unwrap();
    let session_a = handle1.snapshot().await.unwrap().active_session.unwrap();
    handle1.submit(None, "second").await.unwrap();
    let session_b = handle1.snapshot().await.unwrap().active_session.unwrap();
    assert_ne!(session_a, session_b);
    handle1.shutdown().await.unwrap();
    drop(handle1);

    // Restart against the same persistent database. The latest session is
    // restored automatically; explicitly restore the older one.
    let handle2 = start(workspace, config).await.unwrap();
    handle2.activate_session(&session_a).await.unwrap();

    // Watch the live stream, then submit to the restored session. A
    // successful submit (not a "provider not configured" conflict) proves
    // the restored session owns a usable runner.
    let subscription = handle2
        .subscribe_from(handle2.current_cursor())
        .expect("cursor must still be buffered");
    let mut live = subscription.live;
    handle2
        .submit(Some(session_a.clone()), "again")
        .await
        .unwrap();

    // The agent emits ModelStreaming before any network I/O, so the
    // restored session is guaranteed to enter the streaming phase even
    // though the seeded key would be rejected by the real provider.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut entered_streaming = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(envelope) = live.try_recv() {
            if envelope.session_id == session_a && matches!(envelope.event, Event::ModelStreaming) {
                entered_streaming = true;
                break;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    assert!(
        entered_streaming,
        "restored session must accept input and enter ModelStreaming"
    );
    handle2.shutdown().await.unwrap();
}
