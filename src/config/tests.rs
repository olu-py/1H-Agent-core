use tempfile::TempDir;

use super::*;

#[test]
fn selectable_models_cover_defaults_and_custom_is_empty() {
    for preset in [
        ProviderPreset::OpenAi,
        ProviderPreset::DeepSeek,
        ProviderPreset::Qwen,
        ProviderPreset::Volcano,
    ] {
        let default_model = preset.defaults().model;
        assert!(preset.selectable_models().contains(&default_model.as_str()));
    }
    assert!(ProviderPreset::Custom.selectable_models().is_empty());
}

#[test]
fn defaults_are_bounded() {
    let config = Config::default();
    assert_eq!(config.provider.kind, ProviderKind::Responses);
    assert!(config.runtime.max_fetch_bytes >= config.runtime.max_tool_output_bytes);
    assert_eq!(config.runtime.max_background_sessions, 8);
    assert_eq!(config.cluster.max_parallel_children, Some(4));
    assert_eq!(config.cluster.child_active_timeout_seconds, 300);
    assert!(config.compaction.enabled);
    assert_eq!(config.compaction.auto_threshold, 0.80);
    assert_eq!(config.compaction.max_overflow_retries, 1);
    assert_eq!(config.provider.retry_max_attempts, 3);
    assert_eq!(config.provider.retry_initial_backoff_ms, 500);
    assert_eq!(config.provider.retry_max_backoff_ms, 8000);
    assert_eq!(config.runtime.checkpoint_max_file_bytes, 1024 * 1024);
    assert_eq!(
        config.runtime.checkpoint_max_session_bytes,
        16 * 1024 * 1024
    );
    assert_eq!(config.runtime.search_backend, SearchBackend::DuckDuckGo);
    assert!(config.model_metadata.fetch);
    assert_eq!(config.model_metadata.ttl_hours, 24);
    assert_eq!(config.model_metadata.timeout_ms, 5000);
}

#[test]
fn model_metadata_settings_are_clamped() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "[model_metadata]\nfetch = false\nttl_hours = 0\ntimeout_ms = 1\n",
    )
    .unwrap();
    let low = Config::load(Some(&path), temp.path()).unwrap();
    assert!(!low.model_metadata.fetch);
    assert_eq!(low.model_metadata.ttl_hours, 1);
    assert_eq!(low.model_metadata.timeout_ms, 1000);

    fs::write(
        &path,
        "[model_metadata]\nttl_hours = 9999\ntimeout_ms = 999999\n",
    )
    .unwrap();
    let high = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(high.model_metadata.ttl_hours, 168);
    assert_eq!(high.model_metadata.timeout_ms, 15_000);
}

#[test]
fn registry_reports_documented_output_caps() {
    assert_eq!(
        known_max_output(ProviderPreset::OpenAi, "gpt-4o"),
        Some(16_384)
    );
    assert_eq!(
        known_max_output(ProviderPreset::OpenAi, "gpt-4.1-mini"),
        Some(32_768)
    );
    assert_eq!(
        known_max_output(ProviderPreset::DeepSeek, "deepseek-chat"),
        Some(8_192)
    );
    // Unknown models and models without a documented cap stay `None`.
    assert_eq!(known_max_output(ProviderPreset::Custom, "mystery"), None);
    assert_eq!(
        known_max_output(ProviderPreset::Volcano, "doubao-seed-2-0-pro"),
        None
    );
}

#[test]
fn search_backend_is_parsed_from_toml() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "[runtime]\nsearch_backend = \"bing\"\n").unwrap();
    let config = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(config.runtime.search_backend, SearchBackend::Bing);

    fs::write(&path, "[runtime]\nsearch_backend = \"bogus\"\n").unwrap();
    assert!(Config::load(Some(&path), temp.path()).is_err());
}

#[test]
fn background_session_limit_is_normalized() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "[runtime]\nmax_background_sessions = 0\n").unwrap();
    let low = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(low.runtime.max_background_sessions, 2);

    fs::write(&path, "[runtime]\nmax_background_sessions = 1000\n").unwrap();
    let high = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(high.runtime.max_background_sessions, 64);
}

#[test]
fn checkpoint_limits_are_normalized() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "[runtime]\ncheckpoint_max_file_bytes = 1\ncheckpoint_max_session_bytes = 1\n",
    )
    .unwrap();
    let low = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(low.runtime.checkpoint_max_file_bytes, 4 * 1024);
    assert_eq!(low.runtime.checkpoint_max_session_bytes, 1024 * 1024);

    fs::write(
        &path,
        "[runtime]\ncheckpoint_max_file_bytes = 999999999\ncheckpoint_max_session_bytes = 999999999\n",
    )
    .unwrap();
    let high = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(high.runtime.checkpoint_max_file_bytes, 8 * 1024 * 1024);
    assert_eq!(high.runtime.checkpoint_max_session_bytes, 256 * 1024 * 1024);
}

#[test]
fn provider_retry_limits_are_normalized() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "[provider]\nretry_max_attempts = 99\nretry_initial_backoff_ms = 1\nretry_max_backoff_ms = 999999\n",
    )
    .unwrap();
    let config = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(config.provider.retry_max_attempts, 5);
    assert_eq!(config.provider.retry_initial_backoff_ms, 100);
    assert_eq!(config.provider.retry_max_backoff_ms, 30000);

    fs::write(&path, "[provider]\nretry_max_attempts = 0\n").unwrap();
    let disabled = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(disabled.provider.retry_max_attempts, 0);
}

#[test]
fn server_limits_are_normalized() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(
        &path,
        "[server]\nport = 1\nevent_buffer = 1\napproval_timeout_seconds = 1\n",
    )
    .unwrap();
    let low = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(low.server.port, 1024);
    assert_eq!(low.server.event_buffer, 16);
    assert_eq!(low.server.approval_timeout_seconds, 10);

    fs::write(
        &path,
        "[server]\nport = 99999\nevent_buffer = 99999\napproval_timeout_seconds = 99999\n",
    )
    .unwrap();
    let high = Config::load(Some(&path), temp.path()).unwrap();
    assert_eq!(high.server.port, 65535);
    assert_eq!(high.server.event_buffer, 4096);
    assert_eq!(high.server.approval_timeout_seconds, 3600);
}

#[test]
fn server_defaults_are_bounded_and_loopback() {
    let config = Config::default();
    assert_eq!(config.server.bind, "127.0.0.1");
    assert_eq!(config.server.port, 7788);
    assert_eq!(config.server.event_buffer, 512);
    assert_eq!(config.server.approval_timeout_seconds, 300);
}

#[test]
fn compaction_limits_are_normalized() {
    let mut config = CompactionConfig {
        enabled: true,
        auto_threshold: 1.0,
        target_ratio: 0.1,
        preserve_recent_tokens: Some(100_000),
        max_summary_bytes: 1,
        max_overflow_retries: 99,
    };
    config.normalize();
    assert_eq!(config.auto_threshold, 0.90);
    assert_eq!(config.target_ratio, 0.30);
    assert_eq!(config.preserve_recent_tokens, Some(16_000));
    assert_eq!(config.max_summary_bytes, 4 * 1024);
    assert_eq!(config.max_overflow_retries, 3);

    let mut config = CompactionConfig {
        max_overflow_retries: 0,
        ..CompactionConfig::default()
    };
    config.normalize();
    // 0 legitimately disables the overflow recovery and stays 0.
    assert_eq!(config.max_overflow_retries, 0);
}

#[test]
fn legacy_active_provider_is_migrated_without_losing_fields() {
    let mut config = Config {
        provider: ProviderPreset::DeepSeek.defaults(),
        ..Config::default()
    };
    config.provider.base_url = "https://gateway.example/deepseek".into();
    config.provider.model = "deepseek-private".into();
    config.ensure_provider_profiles();

    let saved = config.provider_for(ProviderPreset::DeepSeek).unwrap();
    assert_eq!(saved.base_url, "https://gateway.example/deepseek");
    assert_eq!(saved.model, "deepseek-private");
}

#[test]
fn provider_profiles_are_unique_and_serialized_without_secrets() {
    let mut config = Config::default();
    config.upsert_provider(ProviderPreset::Qwen.defaults());
    let mut replacement = ProviderPreset::Qwen.defaults();
    replacement.model = "qwen-custom-deployment".into();
    config.upsert_provider(replacement);

    assert_eq!(
        config
            .providers
            .iter()
            .filter(|provider| provider.preset == ProviderPreset::Qwen)
            .count(),
        1
    );
    assert_eq!(
        config.provider_for(ProviderPreset::Qwen).unwrap().model,
        "qwen-custom-deployment"
    );
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("providers"));
    assert!(!encoded.to_ascii_lowercase().contains("api_key"));
}

#[test]
fn initialized_empty_profile_list_is_not_repopulated() {
    let mut config = Config {
        provider_profiles_initialized: true,
        ..Config::default()
    };
    config.ensure_provider_profiles();
    assert!(config.providers.is_empty());
}

#[test]
fn legacy_config_without_ids_gets_preset_keys() {
    // A config written before ids existed has neither `id` nor `name`.
    let toml = r#"
[provider]
preset = "deep_seek"
kind = "responses"
base_url = "https://api.deepseek.com"
model = "deepseek-v4-flash"

[[providers]]
preset = "custom"
kind = "chat_completions"
base_url = "https://gateway.example/v1"
model = "gateway-model"

[[providers]]
preset = "custom"
kind = "chat_completions"
base_url = "https://other.example/v1"
model = "other-model"
"#;
    let mut config: Config = toml::from_str(toml).unwrap();
    config.ensure_provider_profiles();

    // The active `[provider]` profile is stamped from its parsed preset.
    assert_eq!(config.provider.id(), "deepseek");
    assert_eq!(config.provider.preset, ProviderPreset::DeepSeek);
    // Both legacy custom rows derive the same id "custom", so the list stays
    // de-duplicated (last-wins) and single-custom setups keep their key.
    let custom = config.provider_for_id("custom").unwrap();
    assert_eq!(custom.base_url, "https://other.example/v1");
    assert_eq!(custom.model, "other-model");
    assert_eq!(
        config
            .providers
            .iter()
            .filter(|provider| provider.id() == "custom")
            .count(),
        1
    );
    // The active DeepSeek profile was migrated into the saved list as well.
    assert!(
        config
            .providers
            .iter()
            .any(|provider| provider.id() == "deepseek")
    );
}

#[test]
fn custom_profiles_with_distinct_ids_coexist() {
    let mut config = Config::default();
    let mut first = ProviderPreset::Custom.defaults();
    first.id = "custom-aaaa".into();
    first.name = "Gateway A".into();
    first.model = "model-a".into();
    let mut second = ProviderPreset::Custom.defaults();
    second.id = "custom-bbbb".into();
    second.name = "Gateway B".into();
    second.model = "model-b".into();
    config.upsert_provider(first);
    config.upsert_provider(second);

    assert_eq!(config.providers.len(), 2);
    assert_eq!(
        config.provider_for_id("custom-aaaa").unwrap().name,
        "Gateway A"
    );
    assert_eq!(
        config.provider_for_id("custom-bbbb").unwrap().name,
        "Gateway B"
    );
}

#[test]
fn provider_name_taken_is_case_insensitive_and_ignores_self() {
    let mut config = Config::default();
    let mut gateway = ProviderPreset::Custom.defaults();
    gateway.id = "custom-aaaa".into();
    gateway.name = "My Gateway".into();
    config.upsert_provider(gateway);

    assert!(config.provider_name_taken("my gateway", None));
    assert!(config.provider_name_taken("MY GATEWAY", Some("custom-bbbb")));
    // The profile may keep its own name while being edited.
    assert!(!config.provider_name_taken("My Gateway", Some("custom-aaaa")));
    // Built-in labels are reserved too.
    assert!(config.provider_name_taken("DeepSeek", None));
    // Empty/whitespace names are never considered taken.
    assert!(!config.provider_name_taken("   ", None));
}

#[test]
fn provider_name_bounds_are_validated_and_trimmed() {
    let mut provider = ProviderPreset::Custom.defaults();
    provider.name = "  Padded  ".into();
    provider.validate().unwrap();
    assert_eq!(provider.name, "Padded");

    provider.name = "x".repeat(crate::config::provider::MAX_PROVIDER_NAME_CHARS + 1);
    assert!(provider.validate().is_err());

    provider.name = "line\nbreak".into();
    assert!(provider.validate().is_err());
}

#[test]
fn enabled_models_round_trip_through_toml() {
    let mut config = Config::default();
    let mut provider = ProviderPreset::Custom.defaults();
    provider.id = "custom-aaaa".into();
    provider.name = "Gateway".into();
    provider.enabled_models = vec!["model-a".into(), "model-b".into()];
    config.upsert_provider(provider);

    let encoded = toml::to_string(&config).unwrap();
    let decoded: Config = toml::from_str(&encoded).unwrap();
    assert_eq!(
        decoded
            .provider_for_id("custom-aaaa")
            .unwrap()
            .enabled_models,
        vec!["model-a".to_owned(), "model-b".to_owned()]
    );
}

#[test]
fn new_custom_id_is_well_formed_and_unique() {
    let first = ProviderConfig::new_custom_id();
    let second = ProviderConfig::new_custom_id();
    assert!(first.starts_with("custom-"));
    assert_eq!(first.len(), "custom-".len() + 32);
    assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    assert_ne!(first, second);
}

#[test]
fn deprecated_main_agent_turn_limit_is_ignored() {
    // RuntimeConfig intentionally accepts and ignores this removed key so
    // existing user configuration continues to load.
    let runtime: RuntimeConfig = toml::from_str(
        "max_agent_turns = 8\ncommand_timeout_seconds = 17\nmax_tool_output_bytes = 2048\nmax_fetch_bytes = 4096",
    )
    .unwrap();
    assert_eq!(runtime.command_timeout_seconds, 17);
    assert_eq!(runtime.max_tool_output_bytes, 2048);
    assert_eq!(runtime.max_fetch_bytes, 4096);
}

#[test]
fn presets_have_expected_protocols_and_current_models() {
    let deepseek = ProviderPreset::DeepSeek.defaults();
    assert_eq!(deepseek.model, "deepseek-v4-flash");
    assert_eq!(deepseek.base_url, "https://api.deepseek.com");
    assert_eq!(deepseek.kind, ProviderKind::Responses);
    let qwen = ProviderPreset::Qwen.defaults();
    assert!(qwen.base_url.contains("{WorkspaceId}"));
    assert_eq!(qwen.kind, ProviderKind::ChatCompletions);
    assert!(ProviderPreset::Qwen.supports_responses());
    let volcano = ProviderPreset::Volcano.defaults();
    assert!(volcano.base_url.ends_with("/api/v3"));
}

#[test]
fn thinking_profiles_match_provider_models_and_defaults() {
    let cases = [
        (
            ProviderPreset::OpenAi,
            "GPT-5.6-SOL-2026-08",
            ThinkingProfileKind::OpenAi,
            ThinkingLevel::Auto,
        ),
        (
            ProviderPreset::Qwen,
            "deployment-QWEN_3.8-MAX-v2",
            ThinkingProfileKind::Qwen38,
            ThinkingLevel::XHigh,
        ),
        (
            ProviderPreset::Qwen,
            "qwen-3.7-plus-latest",
            ThinkingProfileKind::Qwen37,
            ThinkingLevel::Enabled,
        ),
        (
            ProviderPreset::DeepSeek,
            "DEEPSEEK-V4-PRO-202608",
            ThinkingProfileKind::DeepSeekPro,
            ThinkingLevel::High,
        ),
        (
            ProviderPreset::DeepSeek,
            "tenant-deepseek_v4_flash",
            ThinkingProfileKind::DeepSeekFlash,
            ThinkingLevel::High,
        ),
        (
            ProviderPreset::Volcano,
            "deployment-id",
            ThinkingProfileKind::Volcano,
            ThinkingLevel::High,
        ),
        (
            ProviderPreset::Custom,
            "unknown-model",
            ThinkingProfileKind::Compatible,
            ThinkingLevel::Auto,
        ),
    ];
    for (preset, model, kind, default) in cases {
        let profile = thinking_profile(preset, model);
        assert_eq!(profile.kind, kind, "{model}");
        assert_eq!(profile.default, default, "{model}");
    }
    assert!(
        thinking_profile(ProviderPreset::DeepSeek, "deepseek-v4-flash")
            .options
            .contains(&ThinkingLevel::Max)
    );
    assert_eq!(
        thinking_profile(ProviderPreset::Volcano, "any").options,
        &[ThinkingLevel::High]
    );
}

#[test]
fn thinking_config_serializes_and_old_config_uses_model_default() {
    let mut provider = ProviderPreset::DeepSeek.defaults();
    provider.thinking_level = ThinkingLevel::Max;
    let encoded = toml::to_string(&provider).unwrap();
    assert!(encoded.contains("thinking_level = \"max\""));
    let decoded: ProviderConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded.thinking_level, ThinkingLevel::Max);

    let mut old: ProviderConfig = toml::from_str(
        r#"
preset = "qwen"
kind = "chat_completions"
base_url = "https://example.com/v1"
model = "qwen3.7-plus"
"#,
    )
    .unwrap();
    old.normalize_thinking();
    assert_eq!(old.thinking_level, ThinkingLevel::Enabled);
    assert_eq!(old.thinking_budget_tokens, None);
}

#[test]
fn qwen_requires_workspace_id_before_saving() {
    let mut qwen = ProviderPreset::Qwen.defaults();
    assert!(qwen.validate().is_err());
    qwen.base_url = qwen.base_url.replace("{WorkspaceId}", "ws-example");
    assert!(qwen.validate().is_ok());
}

#[test]
fn context_window_uses_provider_aware_registry_and_default() {
    let mut provider = ProviderPreset::DeepSeek.defaults();
    provider.model = "  DEEPSEEK-V3-0324  ".into();
    assert_eq!(provider.resolved_context_window_tokens(), Some(128_000));

    provider.model = "deepseek-v4-flash".into();
    assert_eq!(provider.resolved_context_window_tokens(), Some(1_000_000));
}

#[test]
fn context_window_registry_covers_each_provider() {
    let cases = [
        (ProviderPreset::OpenAi, "gpt-5-mini", 400_000),
        (ProviderPreset::OpenAi, "gpt-5.6-sol", 1_050_000),
        (ProviderPreset::OpenAi, "gpt-5.6-terra", 1_050_000),
        (ProviderPreset::OpenAi, "gpt-5.6-luna", 1_050_000),
        (ProviderPreset::OpenAi, "gpt-4.1-mini", 1_047_576),
        (ProviderPreset::OpenAi, "gpt-4o-mini", 128_000),
        (ProviderPreset::OpenAi, "o1-mini", 128_000),
        (ProviderPreset::OpenAi, "o1-mini-2024-09-12", 128_000),
        (ProviderPreset::OpenAi, "o1", 200_000),
        (ProviderPreset::OpenAi, "o3", 200_000),
        (ProviderPreset::OpenAi, "o3-2025-04-16", 200_000),
        (ProviderPreset::OpenAi, "o4-mini", 200_000),
        (ProviderPreset::DeepSeek, "deepseek-chat", 128_000),
        (ProviderPreset::DeepSeek, "deepseek-reasoner", 128_000),
        (ProviderPreset::DeepSeek, "deepseek-r1-0528", 128_000),
        (ProviderPreset::DeepSeek, "deepseek-v4-pro", 1_000_000),
        (ProviderPreset::DeepSeek, "deepseek-v4-flash", 1_000_000),
        (ProviderPreset::Qwen, "qwen-max", 32_768),
        (ProviderPreset::Qwen, "qwen-plus", 131_072),
        (ProviderPreset::Qwen, "qwen-plus-latest", 131_072),
        (ProviderPreset::Qwen, "qwen-turbo", 1_000_000),
        (ProviderPreset::Qwen, "qwen-turbo-2025-xx", 1_000_000),
        (ProviderPreset::Qwen, "qwen-long", 1_000_000),
        (ProviderPreset::Qwen, "qwen3-235b-a22b", 131_072),
        (ProviderPreset::Qwen, "qwen3.8-max", 1_000_000),
        (ProviderPreset::Qwen, "qwen3.7-max", 1_000_000),
        (ProviderPreset::Qwen, "qwen3.7-plus", 1_000_000),
        (ProviderPreset::Qwen, "qwen3.7-flash", 1_000_000),
        (
            ProviderPreset::Volcano,
            "doubao-seed-2-1-pro-260628",
            256_000,
        ),
        (ProviderPreset::Volcano, "doubao-pro-32k-250115", 32_000),
        (ProviderPreset::Volcano, "deepseek-v4-flash", 1_000_000),
        (ProviderPreset::Volcano, "glm-5.2", 1_000_000),
        (ProviderPreset::Volcano, "deepseek-v4-pro", 200_000),
        (ProviderPreset::Volcano, "glm-4.7", 200_000),
        (ProviderPreset::Volcano, "minimax-m2.7", 200_000),
        (ProviderPreset::Volcano, "minimax-m2.5", 200_000),
        (ProviderPreset::Volcano, "doubao-seed-2.0-pro", 256_000),
        (ProviderPreset::Volcano, "doubao-seed-2.0-code", 256_000),
        (ProviderPreset::Volcano, "doubao-seed-2.0-lite", 256_000),
        (ProviderPreset::Volcano, "kimi-k2.6", 256_000),
        (ProviderPreset::Volcano, "kimi-k2.5", 256_000),
        (ProviderPreset::Custom, "gpt-5-mini", 400_000),
        (ProviderPreset::Custom, "deepseek-chat", 128_000),
        (ProviderPreset::Custom, "qwen3-32b", 131_072),
        (
            ProviderPreset::Custom,
            "doubao-seed-2-1-pro-260628",
            256_000,
        ),
        (ProviderPreset::Custom, "deepseek-v4-flash", 1_000_000),
    ];
    for (preset, model, expected) in cases {
        let mut provider = preset.defaults();
        provider.model = model.into();
        assert_eq!(provider.resolved_context_window_tokens(), Some(expected));
    }
}

#[test]
fn unknown_models_return_no_window_without_explicit_override() {
    // An unrecognized model must never resolve to an uncertain default
    // window: requests must not be sized against a made-up capacity.
    for (preset, model) in [
        (ProviderPreset::Volcano, "other-model-256k"),
        (ProviderPreset::Custom, "vendor-model-128k"),
        (ProviderPreset::Custom, "unknown-32k"),
        (ProviderPreset::OpenAi, "o3foobar"),
        (ProviderPreset::Qwen, "qwen-plusfake"),
        (ProviderPreset::OpenAi, "gpt-5fake"),
    ] {
        let mut provider = preset.defaults();
        provider.model = model.into();
        assert_eq!(provider.resolved_context_window_tokens(), None);
    }
    // An explicit override still wins for the same unknown model.
    let mut provider = ProviderPreset::Custom.defaults();
    provider.model = "vendor-model-128k".into();
    provider.context_window_tokens = Some(128_000);
    assert_eq!(provider.resolved_context_window_tokens(), Some(128_000));
}

#[test]
fn exact_model_rules_win_and_prefixes_use_longest_match() {
    assert_eq!(
        known_context_window(ProviderPreset::OpenAi, "O1-MINI"),
        Some(128_000)
    );
    assert_eq!(
        known_context_window(ProviderPreset::OpenAi, "gpt-4.1-mini"),
        Some(1_047_576)
    );
    assert_eq!(
        known_context_window(ProviderPreset::DeepSeek, "deepseek-r1"),
        Some(128_000)
    );
    assert_eq!(
        known_context_window(ProviderPreset::Qwen, "qwen3"),
        Some(131_072)
    );
    assert_eq!(
        known_context_window(ProviderPreset::OpenAi, "o3foobar"),
        None
    );
    assert_eq!(
        known_context_window(ProviderPreset::Qwen, "qwen-plusfake"),
        None
    );
    assert_eq!(
        known_context_window(ProviderPreset::OpenAi, "gpt-5fake"),
        None
    );
}

#[test]
fn custom_only_uses_explicit_known_vendor_families() {
    assert_eq!(
        known_context_window(ProviderPreset::Custom, "gpt-5"),
        Some(400_000)
    );
    assert_eq!(
        known_context_window(ProviderPreset::Custom, "unknown-32k"),
        None
    );
}

#[test]
fn explicit_context_window_override_wins() {
    let mut provider = ProviderPreset::OpenAi.defaults();
    provider.context_window_tokens = Some(32_768);
    assert_eq!(provider.resolved_context_window_tokens(), Some(32_768));
}

#[test]
fn deepseek_responses_is_stateless_and_native_search_defaults_to_auto() {
    let mut provider = ProviderPreset::DeepSeek.defaults();
    provider.use_previous_response_id = true;
    provider.validate().unwrap();
    assert!(!provider.use_previous_response_id);
    assert_eq!(provider.native_web_search, NativeWebSearch::Auto);
}
