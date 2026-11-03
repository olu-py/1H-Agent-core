use std::{
    collections::HashMap,
    env,
    sync::{Mutex, OnceLock},
};

use base64::Engine as _;
use thiserror::Error;

use crate::config::ProviderPreset;

const SERVICE: &str = "1h-agent";

#[derive(Clone, Debug, Error)]
pub enum SecretError {
    #[error("no API key is configured for {0}")]
    Missing(String),
    #[error("system keyring error: {0}")]
    Keyring(String),
}

/// Process-wide key cache keyed by provider id (`"openai"`, `"custom"`,
/// `"custom-<uuid>"`). Built-in ids equal their preset key, so single-provider
/// setups keep the same keys they always had.
type KeyCache = HashMap<String, Result<String, SecretError>>;

fn key_cache() -> &'static Mutex<KeyCache> {
    static CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_key(id: &str) -> Option<Result<String, SecretError>> {
    key_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(id).cloned())
}

fn remember_key_for_id(id: &str, result: Result<String, SecretError>) {
    if let Ok(mut cache) = key_cache().lock() {
        cache.insert(id.to_owned(), result);
    }
}

fn environment_key(preset: ProviderPreset) -> Option<String> {
    let variables: &[&str] = match preset {
        ProviderPreset::OpenAi => &["OPENAI_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::DeepSeek => &["DEEPSEEK_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Qwen => &["DASHSCOPE_API_KEY", "QWEN_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Volcano => &["ARK_API_KEY", "VOLCANO_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Custom => &["AGENT_API_KEY"],
    };
    variables
        .iter()
        .find_map(|variable| env::var(variable).ok().filter(|key| !key.trim().is_empty()))
}

/// Caches environment-backed keys without touching the OS keyring. This keeps
/// cross-provider agents available when their keys come from the environment,
/// while startup performs only one potentially interactive keyring read.
pub fn preload_environment_keys() {
    for preset in ProviderPreset::ALL {
        let id = preset.key_id();
        if cached_key(id).is_none() {
            if let Some(key) = environment_key(preset) {
                remember_key_for_id(id, Ok(key));
            }
        }
    }
}

/// Warm the cache from environment variables for explicitly named providers
/// (for example generated `custom-<uuid>` ids restored from config). Startup
/// preloads the built-in families via [`preload_environment_keys`]; this extends
/// it to saved custom profiles without ever touching the OS keyring, so the
/// one-interactive-read startup budget is preserved.
pub fn preload_environment_keys_for_providers(providers: &[(ProviderPreset, String)]) {
    for (preset, id) in providers {
        if cached_key(id).is_none() {
            if let Some(key) = environment_key(*preset) {
                remember_key_for_id(id, Ok(key));
            }
        }
    }
}

/// Reads a provider API key at most once per process: environment variables
/// first (keyed by the `preset` family), then the OS keyring under the provider
/// `id`. The result (including a missing-key error) is cached by id so repeated
/// settings opens and cross-provider child agents do not hit the keyring on
/// every access.
pub fn api_key_cached(preset: ProviderPreset, id: &str) -> Result<String, SecretError> {
    if let Some(cached) = cached_key(id) {
        return cached;
    }
    let result = api_key(preset, id);
    remember_key_for_id(id, result.clone());
    result
}

/// Reads only the process cache and never touches the OS keyring. Runtime UI,
/// session restoration, and child-agent paths use this after startup preloads
/// every connected provider.
pub fn api_key_cached_only(preset: ProviderPreset, id: &str) -> Result<String, SecretError> {
    cached_key(id).unwrap_or_else(|| Err(SecretError::Missing(preset.label().into())))
}

pub fn api_key(preset: ProviderPreset, id: &str) -> Result<String, SecretError> {
    if let Some(key) = environment_key(preset) {
        return Ok(key);
    }

    let entry = keyring::Entry::new(SERVICE, id)
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    match entry.get_password() {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        Ok(_) | Err(keyring::Error::NoEntry) => Err(SecretError::Missing(preset.label().into())),
        Err(error) => Err(SecretError::Keyring(error.to_string())),
    }
}

pub fn store_api_key(preset: ProviderPreset, id: &str, api_key: &str) -> Result<(), SecretError> {
    if api_key.trim().is_empty() {
        return Err(SecretError::Missing(preset.label().into()));
    }
    let entry = keyring::Entry::new(SERVICE, id)
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    entry
        .set_password(api_key)
        .map_err(|error| SecretError::Keyring(error.to_string()))
}

/// Stores a key in the OS keyring and keeps it in the process cache for the
/// rest of this run, even if the keyring write fails (the caller can then show
/// a "this run only" warning).
pub fn store_api_key_cached(
    preset: ProviderPreset,
    id: &str,
    api_key: &str,
) -> Result<(), SecretError> {
    let result = store_api_key(preset, id, api_key);
    if result.is_ok() || !api_key.trim().is_empty() {
        remember_key_for_id(id, Ok(api_key.to_owned()));
    }
    result
}

pub fn redact(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            if token.starts_with("sk-") && token.len() > 12 {
                "[REDACTED]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Redacts keys already present in the process cache without consulting the
/// environment or keyring. This is for complete output fields, not streams.
pub(crate) fn redact_cached(input: &str) -> String {
    redact_values(input, &cached_keys())
}

pub(crate) fn cached_keys() -> Vec<String> {
    key_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|value| value.as_ref().ok().filter(|key| !key.is_empty()).cloned())
        .collect()
}

pub(crate) fn redact_values(input: &str, keys: &[String]) -> String {
    let patterns = redaction_patterns(keys);
    if patterns.is_empty() {
        return input.to_owned();
    }
    let marker = redaction_marker(&patterns);
    let mut remaining = input;
    let mut result = String::with_capacity(input.len());
    while let Some((position, length)) = patterns
        .iter()
        .filter_map(|pattern| {
            remaining
                .find(pattern)
                .map(|position| (position, pattern.len()))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
    {
        result.push_str(&remaining[..position]);
        result.push_str(&marker);
        remaining = &remaining[position + length..];
    }
    result.push_str(remaining);
    result
}

pub(crate) fn redact_json(value: &mut serde_json::Value, keys: &[String]) {
    match value {
        serde_json::Value::String(text) => *text = redact_values(text, keys),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json(item, keys);
            }
        }
        serde_json::Value::Object(items) => {
            for (key, mut value) in std::mem::take(items) {
                redact_json(&mut value, keys);
                items.insert(redact_values(&key, keys), value);
            }
        }
        _ => {}
    }
}

pub(crate) fn redaction_patterns(keys: &[String]) -> Vec<String> {
    let mut patterns = Vec::new();
    for key in keys.iter().filter(|key| !key.is_empty()) {
        patterns.push(key.clone());
        let json = serde_json::to_string(key).expect("string serialization cannot fail");
        patterns.push(json[1..json.len() - 1].to_owned());
        let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        // Percent escapes are case-insensitive; keep the original characters
        // outside escapes unchanged.
        let mut lower_hex = encoded.clone().into_bytes();
        let mut index = 0;
        while index + 2 < lower_hex.len() {
            if lower_hex[index] == b'%' {
                lower_hex[index + 1].make_ascii_lowercase();
                lower_hex[index + 2].make_ascii_lowercase();
                index += 3;
            } else {
                index += 1;
            }
        }
        patterns.push(String::from_utf8(lower_hex).expect("URL encoding is UTF-8"));
        patterns.push(encoded);
        for engine in [
            base64::engine::general_purpose::STANDARD,
            base64::engine::general_purpose::STANDARD_NO_PAD,
            base64::engine::general_purpose::URL_SAFE,
            base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ] {
            patterns.push(engine.encode(key.as_bytes()));
        }
    }
    patterns.sort_unstable();
    patterns.dedup();
    patterns
}

pub(crate) fn redaction_marker(patterns: &[String]) -> String {
    const TEXT: &str = "[REDACTED]";
    if patterns
        .iter()
        .all(|pattern| !pattern.contains('[') && !pattern.contains(']') && !TEXT.contains(pattern))
    {
        TEXT.to_owned()
    } else {
        let ch = (0xE000..=0x10FFFD)
            .filter_map(char::from_u32)
            .find(|ch| patterns.iter().all(|pattern| !pattern.contains(*ch)))
            .unwrap_or('\u{FFFD}');
        if patterns.iter().any(|pattern| TEXT.contains(pattern)) {
            ch.to_string().repeat(3)
        } else {
            format!("{ch}{TEXT}{ch}")
        }
    }
}

/// Adopts a key the caller cached under the template's family id onto a
/// freshly minted provider id. A new custom provider is created inside the core,
/// so the client can only have stored its key before the id existed (under the
/// family key, e.g. `"custom"`). Copying it to the real id - keyring included -
/// keeps the first activation working and the key discoverable after restart.
/// Cache-only on the source: environment-provided keys are already resolved by
/// family at request time and must not be written into the keyring.
pub fn promote_cached_family_key(preset: ProviderPreset, to_id: &str) -> bool {
    let from_id = preset.key_id();
    if from_id == to_id || cached_key(to_id).is_some() {
        return false;
    }
    let Some(Ok(key)) = cached_key(from_id) else {
        return false;
    };
    let _ = store_api_key_cached(preset, to_id, &key);
    true
}

/// Seeds the process key cache with a fake key for a built-in preset id.
/// Test-only: makes `build_app` create a runner without touching the OS
/// keyring, so core tests are deterministic regardless of what other tests
/// cached.
#[cfg(test)]
pub fn test_seed_key(preset: ProviderPreset, api_key: &str) {
    remember_key_for_id(preset.key_id(), Ok(api_key.to_owned()));
}

/// Seeds the cache for an explicit provider id (including generated custom
/// ids). Test-only.
#[cfg(test)]
pub fn test_seed_key_for_id(id: &str, api_key: &str) {
    remember_key_for_id(id, Ok(api_key.to_owned()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_redaction_handles_multiple_overlapping_and_json_escaped_keys() {
        let keys = vec![
            "synthetic/alpha".to_owned(),
            "synthetic/alpha-long".to_owned(),
            "quoted\"\\key".to_owned(),
        ];
        let encoded = serde_json::to_string(&keys[2]).unwrap();
        let input = format!(
            "a={} b={} c={} d={}",
            keys[0],
            keys[1],
            keys[2],
            &encoded[1..encoded.len() - 1]
        );
        let output = redact_values(&input, &keys);
        for key in &keys {
            assert!(!output.contains(key));
        }
        assert!(!output.contains(&encoded[1..encoded.len() - 1]));
        assert_eq!(
            output,
            "a=[REDACTED] b=[REDACTED] c=[REDACTED] d=[REDACTED]"
        );
        assert_eq!(redact_values("ordinary\ntext", &keys), "ordinary\ntext");
        for key in ["RED", "[REDACTED]", "A"] {
            let output = redact_values(&format!("before {key} after"), &[key.to_owned()]);
            assert!(!output.contains(key));
        }
    }

    #[test]
    fn redacts_url_and_base64_encodings() {
        let key = "synthetic/key+密钥".to_owned();
        let keys = vec![key.clone()];
        let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.as_bytes());
        for value in [key, encoded, b64, url_safe] {
            let output = redact_values(&format!("before {value} after"), &keys);
            assert!(!output.contains(&value));
            assert!(output.starts_with("before "));
            assert!(output.ends_with(" after"));
        }
    }

    #[test]
    fn redacts_key_like_tokens() {
        assert_eq!(
            redact(&format!("Bearer {}{} end", "sk-", "example123456789")),
            "Bearer [REDACTED] end"
        );
        assert_eq!(redact("ordinary text"), "ordinary text");
    }

    #[test]
    fn key_cache_stores_success_and_error_results() {
        remember_key_for_id("openai", Ok("cached-openai".into()));
        match cached_key("openai") {
            Some(Ok(key)) => assert_eq!(key, "cached-openai"),
            other => panic!("expected cached key, got {other:?}"),
        }

        remember_key_for_id("deepseek", Err(SecretError::Missing("DeepSeek".into())));
        assert!(matches!(
            cached_key("deepseek"),
            Some(Err(SecretError::Missing(_)))
        ));
    }

    #[test]
    fn cache_only_lookup_returns_a_preloaded_key() {
        remember_key_for_id("custom", Ok("cached-custom".into()));
        assert_eq!(
            api_key_cached_only(ProviderPreset::Custom, "custom").unwrap(),
            "cached-custom"
        );
    }

    #[test]
    fn cached_lookup_never_invokes_another_backend_read() {
        remember_key_for_id("volcano", Ok("cached-volcano".into()));
        assert_eq!(
            api_key_cached(ProviderPreset::Volcano, "volcano").unwrap(),
            "cached-volcano"
        );
    }

    #[test]
    fn custom_provider_ids_cache_independently() {
        // Two custom providers share the `custom` family (so env fallback is
        // shared) but must never share a cached key.
        remember_key_for_id("custom-aaaa", Ok("key-a".into()));
        remember_key_for_id("custom-bbbb", Ok("key-b".into()));
        assert_eq!(
            api_key_cached_only(ProviderPreset::Custom, "custom-aaaa").unwrap(),
            "key-a"
        );
        assert_eq!(
            api_key_cached_only(ProviderPreset::Custom, "custom-bbbb").unwrap(),
            "key-b"
        );
    }
}
