use serde::{Deserialize, Serialize};

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
