use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod limits;
pub use limits::{
    ClusterConfig, CompactionConfig, MemoryConfig, RuntimeConfig, SearchBackend, ServerConfig,
};

mod provider;
pub use provider::{
    ModelMetadataConfig, NativeWebSearch, ProviderConfig, ProviderKind, ProviderPreset,
    ThinkingCapability, ThinkingLevel, ThinkingProfile, ThinkingProfileKind, thinking_profile,
};
pub(crate) use provider::{known_context_window, known_max_output};

use crate::commands::AgentMode;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderConfig,
    /// Saved connection profiles. Each preset has at most one profile; API
    /// keys deliberately remain in the system keyring rather than TOML.
    pub providers: Vec<ProviderConfig>,
    /// Distinguishes an old config with no `providers` field from a user who
    /// intentionally removed every saved connection in the new UI.
    pub provider_profiles_initialized: bool,
    pub ui: UiConfig,
    pub server: ServerConfig,
    pub runtime: RuntimeConfig,
    /// Bounded allocations shared by the provider, runtime queues and history
    /// recovery paths. These are safety limits, not token-window settings.
    pub memory: MemoryConfig,
    pub compaction: CompactionConfig,
    pub model_metadata: ModelMetadataConfig,
    pub security: SecurityConfig,
    pub permissions: PermissionConfig,
    pub browser: BrowserConfig,
    pub cluster: ClusterConfig,
    pub commands: Vec<CustomCommandConfig>,
    pub agents: Vec<AgentConfig>,
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(skip)]
    pub data_dir: PathBuf,
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

/// Hard byte/count limits for data that can otherwise grow faster than the
/// normal transcript/page limits. Every value is normalized at config load so
/// a malformed TOML cannot disable a safety boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    pub context_meter: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            context_meter: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub allow_private_networks: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PermissionConfig {
    pub tools: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
    pub keep_alive_seconds: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            args: Vec::new(),
            timeout_seconds: 30,
            max_output_bytes: 2 * 1024 * 1024,
            keep_alive_seconds: 30,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CustomCommandConfig {
    pub name: String,
    pub description: String,
    pub template: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub name: String,
    pub mode: AgentMode,
    /// Optional hard turn limit. Zero means unlimited; the child active
    /// execution budget remains the production safety bound.
    pub max_turns: usize,
    pub allowed_tools: Vec<String>,
    pub system_prompt: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: AgentMode::Explore,
            max_turns: 0,
            allowed_tools: Vec::new(),
            system_prompt: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            providers: Vec::new(),
            provider_profiles_initialized: false,
            ui: UiConfig::default(),
            server: ServerConfig::default(),
            runtime: RuntimeConfig::default(),
            memory: MemoryConfig::default(),
            compaction: CompactionConfig::default(),
            model_metadata: ModelMetadataConfig::default(),
            security: SecurityConfig::default(),
            permissions: PermissionConfig::default(),
            browser: BrowserConfig::default(),
            cluster: ClusterConfig::default(),
            commands: Vec::new(),
            agents: Vec::new(),
            mcp_servers: Vec::new(),
            data_dir: PathBuf::new(),
            config_path: None,
        }
    }
}

impl Config {
    pub fn load(explicit_path: Option<&Path>, workspace: &Path) -> Result<Self> {
        let path = explicit_path
            .map(Path::to_path_buf)
            .or_else(default_config_path);
        let mut config = if let Some(path) = path.as_ref().filter(|path| path.exists()) {
            let value = fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            toml::from_str(&value).with_context(|| format!("invalid config {}", path.display()))?
        } else {
            Self::default()
        };

        // A config written by older versions has only `[provider]`. Preserve
        // that complete profile before applying process-only environment
        // overrides, so migration never loses the user's connection details.
        config.ensure_provider_profiles();
        config.memory.normalize();
        config.compaction.normalize();

        if let Ok(value) = env::var("AGENT_API_BASE") {
            if !value.trim().is_empty() {
                config.provider.base_url = value;
            }
        }
        if let Ok(value) = env::var("AGENT_MODEL") {
            if !value.trim().is_empty() {
                config.provider.model = value;
            }
        }
        if let Ok(value) = env::var("AGENT_PROVIDER") {
            config.provider.kind = match value.to_ascii_lowercase().as_str() {
                "chat" | "chat_completions" => ProviderKind::ChatCompletions,
                "responses" => ProviderKind::Responses,
                _ => anyhow::bail!("AGENT_PROVIDER must be 'chat' or 'responses'"),
            };
        }

        config.provider.validate()?;
        config.provider.normalize_thinking();
        config.provider.retry_max_attempts = config.provider.retry_max_attempts.clamp(0, 5);
        config.provider.retry_initial_backoff_ms =
            config.provider.retry_initial_backoff_ms.clamp(100, 2000);
        config.provider.retry_max_backoff_ms =
            config.provider.retry_max_backoff_ms.clamp(1000, 30000);
        if let Some(limit) = config.provider.context_window_tokens {
            if limit < 4096 {
                anyhow::bail!("provider.context_window_tokens must be at least 4096");
            }
            config.provider.context_window_tokens = Some(limit.min(10_000_000));
        }
        if let Some(limit) = config.provider.max_output_tokens {
            // 0 disables the cap (provider default); otherwise bounded 64..=64_000.
            config.provider.max_output_tokens = if limit == 0 {
                None
            } else {
                Some(limit.clamp(64, 64_000))
            };
        }
        config.model_metadata.ttl_hours = config.model_metadata.ttl_hours.clamp(1, 168);
        config.model_metadata.timeout_ms = config.model_metadata.timeout_ms.clamp(1000, 15_000);
        if config.browser.timeout_seconds == 0 || config.browser.timeout_seconds > 3600 {
            anyhow::bail!("browser timeout must be between 1 and 3600 seconds");
        }
        config.browser.max_output_bytes = config.browser.max_output_bytes.min(8 * 1024 * 1024);
        config.browser.keep_alive_seconds = config.browser.keep_alive_seconds.min(300);
        config.server.port = config.server.port.clamp(1024, 65535);
        config.server.event_buffer = config.server.event_buffer.clamp(16, 4096);
        config.server.event_max_bytes = config
            .server
            .event_max_bytes
            .clamp(1024 * 1024, 16 * 1024 * 1024);
        config.server.approval_timeout_seconds =
            config.server.approval_timeout_seconds.clamp(10, 3600);
        config.runtime.max_background_sessions =
            config.runtime.max_background_sessions.clamp(2, 64);
        config.runtime.checkpoint_max_file_bytes = config
            .runtime
            .checkpoint_max_file_bytes
            .clamp(4 * 1024, 8 * 1024 * 1024);
        config.runtime.checkpoint_max_session_bytes = config
            .runtime
            .checkpoint_max_session_bytes
            .clamp(1024 * 1024, 256 * 1024 * 1024);
        config.cluster.child_max_output_bytes = config
            .cluster
            .child_max_output_bytes
            .clamp(16 * 1024, 1024 * 1024);
        config.cluster.max_parallel_children = Some(
            config
                .cluster
                .max_parallel_children
                .unwrap_or(4)
                .clamp(1, 32),
        );
        config.cluster.child_active_timeout_seconds =
            config.cluster.child_active_timeout_seconds.clamp(30, 3600);
        config.cluster.child_max_tool_output_bytes = config
            .cluster
            .child_max_tool_output_bytes
            .clamp(4 * 1024, 1024 * 1024);
        config.cluster.child_max_context_items =
            config.cluster.child_max_context_items.clamp(4, 200);
        config.cluster.child_max_context_bytes = config
            .cluster
            .child_max_context_bytes
            .clamp(16 * 1024, 1024 * 1024);
        for (tool, permission) in &config.permissions.tools {
            if !matches!(permission.as_str(), "allow" | "ask" | "deny") {
                anyhow::bail!("permission for {tool} must be allow, ask, or deny");
            }
        }
        for server in &mut config.mcp_servers {
            server.timeout_seconds = server.timeout_seconds.clamp(1, 3600);
            server.max_output_bytes = server.max_output_bytes.clamp(1024, 8 * 1024 * 1024);
        }

        config.data_dir = env::var_os("AGENT_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|path| path.join("1h-agent")))
            .unwrap_or_else(|| workspace.join(".1h-agent"));
        config.config_path = path;
        Ok(config)
    }

    /// Test-only hook so service tests can exercise the persist path without
    /// depending on the host's real configuration directory.
    #[cfg(test)]
    pub(crate) fn set_config_path_for_test(&mut self, path: PathBuf) {
        self.config_path = Some(path);
    }

    pub fn save(&self) -> Result<()> {
        let path = self
            .config_path
            .as_ref()
            .context("no writable configuration directory is available")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let value = toml::to_string_pretty(self).context("failed to serialize configuration")?;
        fs::write(path, value).with_context(|| format!("failed to save config {}", path.display()))
    }

    /// Returns a saved connection profile by provider id, falling back to the
    /// active profile for compatibility with callers during the old-config
    /// migration. Ids are the profile identity; `preset` is only the template.
    pub fn provider_for_id(&self, id: &str) -> Option<ProviderConfig> {
        self.providers
            .iter()
            .find(|provider| provider.id() == id)
            .cloned()
            .or_else(|| (self.provider.id() == id).then(|| self.provider.clone()))
    }

    /// Returns a saved connection profile by preset template. Built-in presets
    /// map one-to-one onto ids, so this is the legacy form of
    /// [`Self::provider_for_id`] retained for template-driven callers.
    pub fn provider_for(&self, preset: ProviderPreset) -> Option<ProviderConfig> {
        self.provider_for_id(preset.key_id())
    }

    /// Inserts or replaces a profile by its stable id. Keeping this centralized
    /// also prevents duplicate additions from reaching the config file. An
    /// empty id is stamped from the preset first, so template defaults always
    /// land on a stable identity.
    pub fn upsert_provider(&mut self, mut provider: ProviderConfig) {
        provider.ensure_id();
        let id = provider.id().to_owned();
        if let Some(existing) = self
            .providers
            .iter_mut()
            .find(|existing| existing.id() == id)
        {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    /// Removes a profile by id. Callers that only have a preset template should
    /// pass `preset.key_id()`.
    pub fn remove_provider_by_id(&mut self, id: &str) -> Option<ProviderConfig> {
        let index = self
            .providers
            .iter()
            .position(|provider| provider.id() == id)?;
        Some(self.providers.remove(index))
    }

    /// Whether `name` is already taken by a different saved profile or by a
    /// built-in preset label. Comparison is trimmed and case-insensitive so the
    /// provider picker can never show two indistinguishable rows. `exclude_id`
    /// lets an existing profile keep its own name while being edited.
    pub fn provider_name_taken(&self, name: &str, exclude_id: Option<&str>) -> bool {
        let wanted = name.trim().to_lowercase();
        if wanted.is_empty() {
            return false;
        }
        if ProviderPreset::ALL
            .iter()
            .any(|preset| preset.label().to_lowercase() == wanted)
        {
            return true;
        }
        self.providers.iter().any(|provider| {
            provider.id() != exclude_id.unwrap_or("")
                && provider.name.trim().to_lowercase() == wanted
        }) || (self.provider.id() != exclude_id.unwrap_or("")
            && self.provider.name.trim().to_lowercase() == wanted)
    }

    pub fn remove_provider(&mut self, preset: ProviderPreset) -> Option<ProviderConfig> {
        self.remove_provider_by_id(preset.key_id())
    }

    fn ensure_provider_profiles(&mut self) {
        // Stamp ids before de-duplicating so profiles written by older versions
        // (no id) keep their preset key and cannot collide.
        self.provider.ensure_id();
        let mut unique = Vec::with_capacity(self.providers.len() + 1);
        for mut provider in std::mem::take(&mut self.providers) {
            provider.ensure_id();
            let id = provider.id().to_owned();
            if let Some(existing) = unique
                .iter_mut()
                .find(|existing: &&mut ProviderConfig| existing.id() == id)
            {
                *existing = provider;
            } else {
                unique.push(provider);
            }
        }
        self.providers = unique;
        if !self.provider_profiles_initialized {
            self.upsert_provider(self.provider.clone());
            self.provider_profiles_initialized = true;
        }
    }
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|path| path.join("1h-agent").join("config.toml"))
}

#[cfg(test)]
mod tests;
