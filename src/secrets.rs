use std::{
    collections::HashMap,
    env,
    sync::{Mutex, OnceLock},
};

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
