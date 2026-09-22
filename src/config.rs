use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Long-term memory is opt-in at the feature level but the storage schema
    /// is always migrated so it can be enabled without a later data move.
    pub enabled: bool,
    /// Explicit opt-in reserved for a future, user-authorized provider
    /// context integration. Storage and management remain available when
    /// this is false.
    pub auto_recall: bool,
    pub max_entries: usize,
    pub max_candidates: usize,
    pub max_entry_bytes: usize,
    pub max_total_bytes: usize,
    pub max_recall_entries: usize,
    pub max_recall_bytes: usize,
    pub max_sse_frame_bytes: usize,
    pub max_sse_buffer_bytes: usize,
    pub max_response_bytes: usize,
    pub max_tool_call_bytes: usize,
    pub max_tool_call_total_bytes: usize,
    pub max_tool_calls: usize,
    pub max_agent_event_bytes: usize,
    pub max_history_items: usize,
    pub max_history_bytes: usize,
    pub max_history_item_bytes: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_recall: false,
            max_entries: 512,
            max_candidates: 128,
            max_entry_bytes: 16 * 1024,
            max_total_bytes: 8 * 1024 * 1024,
            max_recall_entries: 8,
            max_recall_bytes: 16 * 1024,
            max_sse_frame_bytes: 2 * 1024 * 1024,
            max_sse_buffer_bytes: 4 * 1024 * 1024,
            max_response_bytes: 2 * 1024 * 1024,
            max_tool_call_bytes: 1024 * 1024,
            max_tool_call_total_bytes: 4 * 1024 * 1024,
            max_tool_calls: 32,
            max_agent_event_bytes: 64 * 1024,
            max_history_items: 200,
            max_history_bytes: 1024 * 1024,
            max_history_item_bytes: 256 * 1024,
        }
    }
}

impl MemoryConfig {
    pub fn normalize(&mut self) {
        self.max_entries = self.max_entries.clamp(1, 10_000);
        self.max_candidates = self.max_candidates.clamp(1, self.max_entries);
        self.max_entry_bytes = self.max_entry_bytes.clamp(256, 256 * 1024);
        self.max_total_bytes = self
            .max_total_bytes
            .clamp(self.max_entry_bytes, 256 * 1024 * 1024);
        self.max_recall_entries = self.max_recall_entries.clamp(1, 32);
        self.max_recall_bytes = self.max_recall_bytes.clamp(1024, 128 * 1024);
        self.max_sse_frame_bytes = self.max_sse_frame_bytes.clamp(64 * 1024, 8 * 1024 * 1024);
        self.max_sse_buffer_bytes = self
            .max_sse_buffer_bytes
            .clamp(self.max_sse_frame_bytes, 16 * 1024 * 1024);
        self.max_response_bytes = self.max_response_bytes.clamp(64 * 1024, 16 * 1024 * 1024);
        self.max_tool_call_bytes = self.max_tool_call_bytes.clamp(16 * 1024, 4 * 1024 * 1024);
        self.max_tool_call_total_bytes = self
            .max_tool_call_total_bytes
            .clamp(self.max_tool_call_bytes, 16 * 1024 * 1024);
        self.max_tool_calls = self.max_tool_calls.clamp(1, 256);
        self.max_agent_event_bytes = self.max_agent_event_bytes.clamp(4 * 1024, 256 * 1024);
        self.max_history_items = self.max_history_items.clamp(20, 2_000);
        self.max_history_bytes = self.max_history_bytes.clamp(128 * 1024, 16 * 1024 * 1024);
        self.max_history_item_bytes = self
            .max_history_item_bytes
            .clamp(4 * 1024, self.max_history_bytes.min(2 * 1024 * 1024));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub auto_threshold: f32,
    pub target_ratio: f32,
    pub preserve_recent_tokens: Option<u64>,
    pub max_summary_bytes: usize,
    /// Recovery attempts after a provider-confirmed context overflow. Each
    /// attempt compacts (or trims) the conversation and retries only when the
    /// request actually shrank. 0 disables the recovery; failures beyond the
    /// cap surface the original provider error.
    pub max_overflow_retries: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_threshold: 0.80,
            target_ratio: 0.55,
            preserve_recent_tokens: None,
            max_summary_bytes: 65_536,
            max_overflow_retries: 1,
        }
    }
}

impl CompactionConfig {
    pub fn normalize(&mut self) {
        self.auto_threshold = self.auto_threshold.clamp(0.60, 0.90);
        self.target_ratio = self.target_ratio.clamp(0.30, 0.70);
        self.max_summary_bytes = self.max_summary_bytes.clamp(4 * 1024, 256 * 1024);
        self.max_overflow_retries = self.max_overflow_retries.clamp(0, 3);
        if let Some(value) = self.preserve_recent_tokens.as_mut() {
            *value = (*value).clamp(4_000, 16_000);
        }
    }
}

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

/// HTTP/SSE service binding. The server deliberately defaults to a loopback
/// address so it is never exposed to the network without explicit opt-in; a
/// non-loopback `bind` additionally requires token auth (see
/// `server::auth`). `port` is clamped to the dynamic/registered range so a
/// hostile or accidental config cannot redirect the UI onto a privileged port.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address. Defaults to loopback.
    pub bind: String,
    /// TCP port. Clamped to `1024..=65535` at load.
    pub port: u32,
    /// Maximum number of events retained in the SSE replay ring (clamped
    /// `16..=4096` at load).
    pub event_buffer: usize,
    /// Maximum total bytes retained in the SSE replay ring (clamped
    /// `1 MiB..=16 MiB` at load).
    pub event_max_bytes: usize,
    /// How long a pending approval waits before it is rejected automatically.
    pub approval_timeout_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 7788,
            event_buffer: 512,
            event_max_bytes: 4 * 1024 * 1024,
            approval_timeout_seconds: 300,
        }
    }
}

/// Web search backend used by the `web_search` tool. DuckDuckGo is the
/// default (best-effort public endpoint); Bing is offered for networks where
/// DuckDuckGo is unreliable or unreachable.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    #[default]
    DuckDuckGo,
    Bing,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub command_timeout_seconds: u64,
    pub max_tool_output_bytes: usize,
    pub max_fetch_bytes: usize,
    /// Web search backend for the `web_search` tool.
    pub search_backend: SearchBackend,
    /// Hard cap on runtimes parked in the background (the active session is
    /// not counted). Overflow prefers the least-recently-parked idle runtime,
    /// then shuts down the oldest busy runtime if necessary.
    pub max_background_sessions: usize,
    /// Per-file snapshot byte cap for undo/redo checkpointing. Files above
    /// this are recorded as skipped markers instead of snapshotted.
    pub checkpoint_max_file_bytes: usize,
    /// Per-session total snapshot byte cap; exceeding it drops the oldest
    /// snapshots for that session.
    pub checkpoint_max_session_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Maximum active children. Missing values use the safe default of four.
    pub max_parallel_children: Option<usize>,
    /// Reserved count limits; parallel execution is currently bounded by
    /// `max_parallel_children`.
    pub max_children_per_turn: Option<usize>,
    pub max_children_per_session: Option<usize>,
    /// Active model/tool time available to one child. Queueing and approval
    /// waits are excluded from this budget.
    pub child_active_timeout_seconds: u64,
    /// Bounds applied to each child agent's working context and tool output.
    /// These are enforced in `agent::run_child`.
    pub child_max_output_bytes: usize,
    pub child_max_tool_output_bytes: usize,
    pub child_max_context_items: usize,
    pub child_max_context_bytes: usize,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            max_parallel_children: Some(4),
            max_children_per_turn: None,
            max_children_per_session: None,
            child_active_timeout_seconds: 300,
            child_max_output_bytes: 256 * 1024,
            child_max_tool_output_bytes: 128 * 1024,
            child_max_context_items: 48,
            child_max_context_bytes: 512 * 1024,
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

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            command_timeout_seconds: 60,
            max_tool_output_bytes: 1024 * 1024,
            max_fetch_bytes: 10 * 1024 * 1024,
            search_backend: SearchBackend::default(),
            max_background_sessions: 8,
            checkpoint_max_file_bytes: 1024 * 1024,
            checkpoint_max_session_bytes: 16 * 1024 * 1024,
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

    /// Returns a saved connection profile, falling back to the active profile
    /// for compatibility with callers during the old-config migration.
    pub fn provider_for(&self, preset: ProviderPreset) -> Option<ProviderConfig> {
        self.providers
            .iter()
            .find(|provider| provider.preset == preset)
            .cloned()
            .or_else(|| (self.provider.preset == preset).then(|| self.provider.clone()))
    }

    /// Inserts or replaces a profile by preset. Keeping this centralized also
    /// prevents duplicate template additions from reaching the config file.
    pub fn upsert_provider(&mut self, provider: ProviderConfig) {
        if let Some(existing) = self
            .providers
            .iter_mut()
            .find(|existing| existing.preset == provider.preset)
        {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    pub fn remove_provider(&mut self, preset: ProviderPreset) -> Option<ProviderConfig> {
        let index = self
            .providers
            .iter()
            .position(|provider| provider.preset == preset)?;
        Some(self.providers.remove(index))
    }

    fn ensure_provider_profiles(&mut self) {
        let mut unique = Vec::with_capacity(self.providers.len() + 1);
        for provider in std::mem::take(&mut self.providers) {
            if let Some(existing) = unique
                .iter_mut()
                .find(|existing: &&mut ProviderConfig| existing.preset == provider.preset)
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
