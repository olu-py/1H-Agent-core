//! Model metadata resolution: where a model's context window and output cap
//! come from, in priority order.
//!
//! ```text
//! L1 explicit config   provider.context_window_tokens / max_output_tokens
//! L2 provider native   GET {base_url}/models, cached in `model_metadata`
//! L3 community         models.dev snapshot + runtime refresh, same cache
//! L4 built-in registry config.rs static tables
//! ```
//!
//! Unknown models resolve to `None` — the core never guesses a window. All
//! runtime fetching is event-driven (provider switch, first submit with an
//! unknown window, or an explicit refresh request); there is no polling and
//! the process never touches the network at startup.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::config::{self, ProviderConfig};
use crate::provider::ProviderModelInfo;

/// Community model database (models.dev). The request carries no API key and
/// no user data; `Config::model_metadata.fetch = false` disables it entirely.
pub const MODELS_DEV_URL: &str = "https://api.models.dev";
/// Body cap for `GET {base_url}/models` responses.
pub const PROVIDER_MODELS_BODY_CAP: usize = 1024 * 1024;
/// Body cap for the models.dev response.
pub const COMMUNITY_BODY_CAP: usize = 8 * 1024 * 1024;
/// Entries accepted from one provider `/models` response.
pub const MAX_PROVIDER_MODELS: usize = 512;
/// Entries accepted from one models.dev response.
pub const MAX_COMMUNITY_MODELS: usize = 4096;
/// Discovered values outside these bounds are rejected (not clamped) so a
/// poisoned or malformed metadata entry can never shape the context budget.
/// Mirrors the explicit `provider.context_window_tokens` clamp in `Config`.
pub const MIN_CONTEXT_WINDOW_TOKENS: u64 = 4096;
pub const MAX_CONTEXT_WINDOW_TOKENS: u64 = 10_000_000;
pub const MIN_DISCOVERED_MAX_OUTPUT_TOKENS: u32 = 1024;
pub const MAX_DISCOVERED_MAX_OUTPUT_TOKENS: u32 = 131_072;

/// models.dev provider slugs that win a per-model conflict, in order. Slugs
/// outside the list rank after it (alphabetically, via the JSON map order).
const COMMUNITY_SLUG_PRIORITY: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "qwen",
    "alibaba",
    "volcengine",
    "volcano",
    "moonshotai",
    "zhipu",
    "mistral",
    "xai",
];

/// Where a resolved value came from. Wire tags: `config` / `provider` /
/// `community` / `registry` (`unknown` when no window resolved at all).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaSource {
    Config,
    ProviderApi,
    Community,
    Registry,
}

impl MetaSource {
    pub fn wire_tag(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::ProviderApi => "provider",
            Self::Community => "community",
            Self::Registry => "registry",
        }
    }
}

/// A model's discovered limits. Empty fields mean "not reported".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelMeta {
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
}

impl ModelMeta {
    pub fn is_empty(self) -> bool {
        self.context_window_tokens.is_none() && self.max_output_tokens.is_none()
    }
}

/// Runtime-discovered metadata stamped onto the active provider profile.
/// Never serialized: it lives on `ProviderConfig` only in memory and is
/// rebuilt from the `model_metadata` cache after every restart or switch.
#[derive(Clone, Copy, Debug)]
pub struct DiscoveredMeta {
    pub source: MetaSource,
    pub meta: ModelMeta,
    pub fetched_at: i64,
}

/// The full resolution result for one provider profile.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedMeta {
    pub context_window_tokens: Option<u64>,
    /// Meaningful only when `context_window_tokens` is `Some`.
    pub window_source: MetaSource,
    /// Effective output cap: explicit config wins over any discovery. Used
    /// for budget reservation and display only — never injected into requests
    /// unless the user configured it explicitly.
    pub max_output_tokens: Option<u32>,
}

impl ResolvedMeta {
    /// Wire tag for the window's origin; `unknown` when no window resolved.
    pub fn window_source_tag(&self) -> &'static str {
        self.context_window_tokens
            .map_or("unknown", |_| self.window_source.wire_tag())
    }
}

/// Resolves the model metadata chain for a provider profile. Pure: the
/// runtime tiers (L2/L3) read the `discovered` field stamped by the engine;
/// the community snapshot and the built-in registry are compile-time data.
pub fn resolve(provider: &ProviderConfig) -> ResolvedMeta {
    let discovered = provider.discovered;
    let community = community_meta(&provider.model);
    let (context_window_tokens, window_source) = if provider.context_window_tokens.is_some() {
        (provider.context_window_tokens, MetaSource::Config)
    } else if let Some(found) =
        discovered.filter(|found| found.meta.context_window_tokens.is_some())
    {
        (found.meta.context_window_tokens, found.source)
    } else if let Some(meta) = community.filter(|meta| meta.context_window_tokens.is_some()) {
        (meta.context_window_tokens, MetaSource::Community)
    } else if let Some(window) = config::known_context_window(provider.preset, &provider.model) {
        (Some(window), MetaSource::Registry)
    } else {
        (None, MetaSource::Registry)
    };
    let max_output_tokens = provider
        .max_output_tokens
        .or(discovered.and_then(|found| found.meta.max_output_tokens))
        .or(community.and_then(|meta| meta.max_output_tokens))
        .or_else(|| config::known_max_output(provider.preset, &provider.model));
    ResolvedMeta {
        context_window_tokens,
        window_source,
        max_output_tokens,
    }
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

pub fn provider_list_key(base_url: &str) -> String {
    format!("provider-list|{base_url}")
}

pub fn provider_meta_key(base_url: &str, model: &str) -> String {
    format!("provider|{base_url}|{model}")
}

pub fn community_meta_key(model: &str) -> String {
    format!("community|{}", normalize_model_id(model))
}

pub fn normalize_model_id(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Community snapshot (release-time fallback; refreshed at runtime into storage)
// ---------------------------------------------------------------------------

static COMMUNITY_SNAPSHOT: OnceLock<HashMap<String, ModelMeta>> = OnceLock::new();

fn community_snapshot() -> &'static HashMap<String, ModelMeta> {
    COMMUNITY_SNAPSHOT
        .get_or_init(|| parse_community_snapshot(include_str!("community-snapshot.json")))
}

/// Static community lookup by model id (normalized). Runtime-refreshed rows
/// take precedence via the `discovered` stamp; this is the offline fallback.
pub fn community_meta(model: &str) -> Option<ModelMeta> {
    community_snapshot()
        .get(&normalize_model_id(model))
        .copied()
}

pub fn parse_community_snapshot(raw: &str) -> HashMap<String, ModelMeta> {
    serde_json::from_str::<Value>(raw)
        .map(|value| parse_community(&value))
        .unwrap_or_default()
}

/// Parses a models.dev API response:
/// `{"models": {provider_slug: {"models": {model_id: {context_length, …}}}}}`.
/// Tolerates a top-level provider map without the `models` wrapper. When the
/// same model id appears under several providers the highest-priority slug
/// wins, so resolution is deterministic.
pub fn parse_community(value: &Value) -> HashMap<String, ModelMeta> {
    let mut table = HashMap::new();
    let providers = value.get("models").and_then(Value::as_object).or_else(|| {
        value
            .as_object()
            .filter(|map| map.values().any(|entry| entry.get("models").is_some()))
    });
    let Some(providers) = providers else {
        return table;
    };
    let mut ordered: Vec<(&String, &Value)> = providers.iter().collect();
    ordered.sort_by_key(|(slug, _)| community_slug_priority(slug));
    for (_, provider) in ordered {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (id, model) in models {
            if table.len() >= MAX_COMMUNITY_MODELS {
                return table;
            }
            let meta = ModelMeta {
                context_window_tokens: bounded_u64(
                    model,
                    &["context_length"],
                    MIN_CONTEXT_WINDOW_TOKENS,
                    MAX_CONTEXT_WINDOW_TOKENS,
                ),
                max_output_tokens: bounded_u32(
                    model,
                    &["max_output_tokens", "max_tokens"],
                    MIN_DISCOVERED_MAX_OUTPUT_TOKENS,
                    MAX_DISCOVERED_MAX_OUTPUT_TOKENS,
                ),
            };
            if meta.is_empty() {
                continue;
            }
            table.entry(normalize_model_id(id)).or_insert(meta);
        }
    }
    table
}

fn community_slug_priority(slug: &str) -> usize {
    COMMUNITY_SLUG_PRIORITY
        .iter()
        .position(|candidate| *candidate == slug)
        .unwrap_or(COMMUNITY_SLUG_PRIORITY.len())
}

// ---------------------------------------------------------------------------
// Provider /models parsing (OpenAI-compatible, OpenRouter, LM Studio shapes)
// ---------------------------------------------------------------------------

/// Parses `GET {base_url}/models`: `{"data": [{id, …}]}`. Field shapes
/// accepted per entry: `context_length` (OpenRouter), `max_context_length`
/// (LM Studio), and `top_provider.max_completion_tokens` (OpenRouter).
/// Entries without any metadata are still returned — the id list feeds the
/// model picker.
pub fn parse_provider_models(value: &Value) -> Vec<ProviderModelInfo> {
    let mut models = Vec::new();
    let Some(entries) = value.get("data").and_then(Value::as_array) else {
        return models;
    };
    for entry in entries {
        if models.len() >= MAX_PROVIDER_MODELS {
            break;
        }
        let Some(id) = entry.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let max_output_tokens = bounded_u32(
            entry,
            &["max_completion_tokens", "max_tokens"],
            MIN_DISCOVERED_MAX_OUTPUT_TOKENS,
            MAX_DISCOVERED_MAX_OUTPUT_TOKENS,
        )
        .or_else(|| {
            entry.get("top_provider").and_then(|top| {
                bounded_u32(
                    top,
                    &["max_completion_tokens", "max_tokens"],
                    MIN_DISCOVERED_MAX_OUTPUT_TOKENS,
                    MAX_DISCOVERED_MAX_OUTPUT_TOKENS,
                )
            })
        });
        models.push(ProviderModelInfo {
            id: id.to_owned(),
            context_window_tokens: bounded_u64(
                entry,
                &["context_length", "max_context_length", "context_window"],
                MIN_CONTEXT_WINDOW_TOKENS,
                MAX_CONTEXT_WINDOW_TOKENS,
            ),
            max_output_tokens,
        });
    }
    models
}

// ---------------------------------------------------------------------------
// Number extraction
// ---------------------------------------------------------------------------

/// Reads a bounded positive integer from the first present field. Numeric
/// strings are accepted (some gateways emit them); negative, `-`, and
/// out-of-bounds values are rejected rather than clamped.
fn bounded_u64(value: &Value, fields: &[&str], min: u64, max: u64) -> Option<u64> {
    fields.iter().find_map(|field| {
        let number = json_u64(value.get(*field)?)?;
        (number >= min && number <= max).then_some(number)
    })
}

fn bounded_u32(value: &Value, fields: &[&str], min: u32, max: u32) -> Option<u32> {
    fields.iter().find_map(|field| {
        let number = json_u64(value.get(*field)?)?;
        let number = u32::try_from(number).ok()?;
        (number >= min && number <= max).then_some(number)
    })
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| {
                number
                    .as_i64()
                    .and_then(|signed| u64::try_from(signed).ok())
            })
            .or_else(|| {
                number.as_f64().and_then(|float| {
                    (float >= 0.0 && float.fract() == 0.0 && float <= u64::MAX as f64)
                        .then_some(float as u64)
                })
            }),
        Value::String(text) => text
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|signed| u64::try_from(signed).ok()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Network fetch (community tier only; provider tier lives on OpenAiClient)
// ---------------------------------------------------------------------------

/// Fetches and parses the models.dev database. No authentication header and
/// no user data leave the process. Failures are expected to be logged and
/// ignored by callers — the snapshot and registry remain the fallback.
pub async fn fetch_community_models(timeout_ms: u64) -> Result<HashMap<String, ModelMeta>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(MODELS_DEV_URL)
        .timeout(Duration::from_millis(timeout_ms.max(1)))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("models.dev returned HTTP {status}"));
    }
    let body = read_capped(response, COMMUNITY_BODY_CAP).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| format!("invalid models.dev JSON: {error}"))?;
    Ok(parse_community(&value))
}

/// Reads a response body incrementally, failing once it exceeds `cap` bytes
/// so a hostile or misbehaving endpoint cannot exhaust memory.
pub(crate) async fn read_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if body.len() + chunk.len() > cap {
            return Err("response exceeds the metadata size cap".to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderPreset;
    use serde_json::json;

    fn provider(preset: ProviderPreset, model: &str) -> ProviderConfig {
        let mut config = preset.defaults();
        config.model = model.to_owned();
        config
    }

    #[test]
    fn resolve_prefers_explicit_config() {
        let mut config = provider(ProviderPreset::OpenAi, "totally-unknown-model");
        config.context_window_tokens = Some(64_000);
        config.max_output_tokens = Some(4_096);
        let resolved = resolve(&config);
        assert_eq!(resolved.context_window_tokens, Some(64_000));
        assert_eq!(resolved.window_source, MetaSource::Config);
        assert_eq!(resolved.max_output_tokens, Some(4_096));
        assert_eq!(resolved.window_source_tag(), "config");
    }

    #[test]
    fn resolve_uses_discovered_before_snapshot_and_registry() {
        let mut config = provider(ProviderPreset::Custom, "some-gateway-model");
        config.discovered = Some(DiscoveredMeta {
            source: MetaSource::ProviderApi,
            meta: ModelMeta {
                context_window_tokens: Some(256_000),
                max_output_tokens: Some(8_192),
            },
            fetched_at: 0,
        });
        let resolved = resolve(&config);
        assert_eq!(resolved.context_window_tokens, Some(256_000));
        assert_eq!(resolved.window_source, MetaSource::ProviderApi);
        assert_eq!(resolved.max_output_tokens, Some(8_192));
        assert_eq!(resolved.window_source_tag(), "provider");

        // Explicit max_output still wins over a discovered value.
        config.max_output_tokens = Some(2_048);
        assert_eq!(resolve(&config).max_output_tokens, Some(2_048));
    }

    #[test]
    fn resolve_falls_back_to_registry_and_unknown() {
        let config = provider(ProviderPreset::OpenAi, "gpt-4o-mini");
        let resolved = resolve(&config);
        assert_eq!(resolved.context_window_tokens, Some(128_000));
        assert_eq!(resolved.window_source, MetaSource::Registry);

        let unknown = provider(ProviderPreset::Custom, "mystery-model");
        let resolved = resolve(&unknown);
        assert_eq!(resolved.context_window_tokens, None);
        assert_eq!(resolved.window_source_tag(), "unknown");
    }

    #[test]
    fn community_snapshot_ships_parseable_shape() {
        // The committed snapshot may be empty until the first release-time
        // refresh, but it must always parse into the expected shape.
        let table = community_snapshot();
        assert!(table.len() <= MAX_COMMUNITY_MODELS);
    }

    #[test]
    fn parse_community_prefers_priority_slugs_and_normalizes_ids() {
        let value = json!({
            "models": {
                "zzz-gateway": {
                    "models": {
                        "GPT-4o": { "context_length": 8_000_000, "max_output_tokens": 512 }
                    }
                },
                "openai": {
                    "models": {
                        "gpt-4o": { "context_length": 128_000, "max_output_tokens": 16_384 }
                    }
                }
            }
        });
        let table = parse_community(&value);
        // The poisoned zzz-gateway entry (out-of-bounds values) is rejected;
        // the openai entry wins and is reachable by normalized id.
        assert_eq!(
            table.get("gpt-4o"),
            Some(&ModelMeta {
                context_window_tokens: Some(128_000),
                max_output_tokens: Some(16_384),
            })
        );
    }

    #[test]
    fn parse_community_accepts_wrapperless_top_level_and_strings() {
        let value = json!({
            "deepseek": {
                "models": {
                    "deepseek-chat": {
                        "context_length": "131072",
                        "max_output_tokens": "8192"
                    }
                }
            }
        });
        let table = parse_community(&value);
        assert_eq!(
            table.get("deepseek-chat"),
            Some(&ModelMeta {
                context_window_tokens: Some(131_072),
                max_output_tokens: Some(8_192),
            })
        );
    }

    #[test]
    fn parse_community_rejects_negative_and_missing_values() {
        let value = json!({
            "models": {
                "openai": {
                    "models": {
                        "a": { "context_length": -1 },
                        "b": { "max_output_tokens": 64 },
                        "c": { "context_length": 131_072 }
                    }
                }
            }
        });
        let table = parse_community(&value);
        // "a" has no usable fields and "b" only carries an out-of-bounds
        // output cap; both are dropped. "c" survives with a window only.
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.get("c"),
            Some(&ModelMeta {
                context_window_tokens: Some(131_072),
                max_output_tokens: None,
            })
        );
    }

    #[test]
    fn parse_provider_models_handles_known_field_shapes() {
        let value = json!({
            "data": [
                { "id": "gpt-4o" },
                {
                    "id": "openrouter/big-model",
                    "context_length": 200_000,
                    "top_provider": { "max_completion_tokens": 32_768 }
                },
                { "id": "lmstudio-local", "max_context_length": 32_768, "max_tokens": 4_096 },
                { "id": "" },
                { "object": "model" },
                { "id": "poisoned", "context_length": 999_999_999 }
            ]
        });
        let models = parse_provider_models(&value);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-4o",
                "openrouter/big-model",
                "lmstudio-local",
                "poisoned"
            ]
        );
        assert_eq!(
            models[0],
            ProviderModelInfo {
                id: "gpt-4o".to_owned(),
                context_window_tokens: None,
                max_output_tokens: None,
            }
        );
        assert_eq!(models[1].context_window_tokens, Some(200_000));
        assert_eq!(models[1].max_output_tokens, Some(32_768));
        assert_eq!(models[2].context_window_tokens, Some(32_768));
        assert_eq!(models[2].max_output_tokens, Some(4_096));
        // Out-of-bounds values are rejected, not clamped.
        assert_eq!(models[3].context_window_tokens, None);
    }

    #[test]
    fn parse_provider_models_caps_entries() {
        let entries: Vec<Value> = (0..600)
            .map(|index| json!({ "id": format!("m{index}") }))
            .collect();
        let value = json!({ "data": entries });
        assert_eq!(parse_provider_models(&value).len(), MAX_PROVIDER_MODELS);
    }

    #[test]
    fn json_u64_accepts_numbers_strings_and_integral_floats() {
        assert_eq!(json_u64(&json!(128_000)), Some(128_000));
        assert_eq!(json_u64(&json!("131072")), Some(131_072));
        assert_eq!(json_u64(&json!(128_000.0)), Some(128_000));
        assert_eq!(json_u64(&json!(-1)), None);
        assert_eq!(json_u64(&json!("-")), None);
        assert_eq!(json_u64(&json!(null)), None);
        assert_eq!(json_u64(&json!(1.5)), None);
    }

    #[test]
    fn storage_keys_are_stable() {
        assert_eq!(
            provider_list_key("https://x/v1"),
            "provider-list|https://x/v1"
        );
        assert_eq!(
            provider_meta_key("https://x/v1", "m"),
            "provider|https://x/v1|m"
        );
        assert_eq!(community_meta_key("GPT-4o"), "community|gpt-4o");
    }
}
