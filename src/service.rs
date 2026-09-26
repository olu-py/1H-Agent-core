//! `AppService`/`AppHandle`: the in-process core entry point shared by every
//! interface (Web, TUI, Desktop).
//!
//! [`AppService::start`] builds the full application state machine (sessions,
//! runtimes, tool registry, router, approvals, storage) and returns an
//! [`AppHandle`]. The handle exposes only typed operations — `snapshot`,
//! `messages`, `submit`, `execute_command`, `approve`, `cancel`,
//! `activate_session`, `set_provider`, `subscribe`, `shutdown` — and never
//! leaks oneshot channels or the internal [`CoreCommand`] enum. All upward
//! commands are serialized through a bounded channel into the single
//! state-machine task, so concurrent consumers cannot interleave mutations.
//!
//! On shutdown the engine rejects pending approvals, cancels the agent tree,
//! closes subscriptions, and releases the database and the per-workspace
//! exclusive lock. A second program opening the same canonical workspace fails
//! immediately at [`AppService::start`].

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use tokio::sync::{mpsc, oneshot};

mod engine;
#[cfg(test)]
pub(crate) use engine::routed_to_event;
#[cfg(test)]
use engine::stored_to_message_dto;
use engine::{CoreCommand, Engine, run_engine};

mod handle;
pub use handle::AppHandle;

use crate::{
    agent::AgentEvent,
    app::{self, App},
    commands::{self, Command},
    config::Config,
    model::{AgentPhase, ApprovalAction, PendingApproval},
    protocol::{
        self, ApiError, AppSnapshotV2, ApprovalDto, ContextBudgetDto, Event, MemoryDto, MessageDto,
        MessagePage, SessionStateDto, TodoDto,
    },
    provider::ToolCall,
    secrets,
    storage::{MemoryRecord, Storage, StoredMessage},
};

/// Everything the core needs to start, decoupled from any UI-specific config.
/// The caller is responsible for deriving these from the shared config file and
/// CLI flags; the core never parses server bind/port/auth settings itself.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Canonicalized workspace path the agent is allowed to access.
    pub workspace: PathBuf,
    /// Shared configuration (provider, permissions, runtime, cluster, ...).
    /// UI-specific sections (e.g. `server`) are ignored by the core.
    pub config: Config,
    /// Directory holding `agent.db` (and the per-workspace lock files).
    pub data_dir: PathBuf,
    /// Maximum events retained in the bridge replay ring (clamped 16..=4096).
    pub event_capacity: usize,
    /// Maximum total bytes retained in the bridge replay ring (clamped
    /// 1 MiB..=16 MiB).
    pub event_max_bytes: usize,
    /// How long a pending approval waits before it is rejected automatically.
    pub approval_timeout: Duration,
    /// Message page size used by the messages endpoint (clamped 20..=200).
    pub message_page_size: usize,
}

/// The running application service. [`AppService::start`] builds the full state
/// machine, starts the engine loop, and returns an [`AppHandle`] that owns the
/// engine task and the per-workspace exclusive lock. Dropping the last handle
/// shuts the engine down.
pub struct AppService;

impl AppService {
    /// Builds the full state machine and starts the engine loop.
    ///
    /// Fails immediately if the canonical workspace is already locked by
    /// another process (Web/TUI print an error and exit; Desktop surfaces an
    /// "already in use" prompt).
    pub async fn start(config: CoreConfig) -> Result<AppHandle> {
        let lock = WorkspaceLock::acquire(&config.data_dir, &config.workspace)
            .context("workspace is already in use by another 1H-Agent instance")?;
        std::fs::create_dir_all(&config.data_dir).with_context(|| {
            format!("cannot create data directory {}", config.data_dir.display())
        })?;
        let storage = Storage::open(&config.data_dir.join("agent.db"))?;
        storage.mark_running_children_interrupted()?;
        secrets::preload_environment_keys();
        // Warm environment-backed keys for every saved provider id (including
        // generated custom ids) without touching the keyring.
        let saved_providers = config
            .config
            .providers
            .iter()
            .map(|provider| (provider.preset, provider.id().to_owned()))
            .collect::<Vec<_>>();
        secrets::preload_environment_keys_for_providers(&saved_providers);
        // Startup loads the default provider's key once (environment first,
        // then the system keychain) so the restored session owns a usable
        // runner without a settings round trip. Cached per provider id.
        let _ = secrets::api_key_cached(config.config.provider.preset, config.config.provider.id());

        let bridge = Arc::new(crate::bridge::EventBridge::new(
            config.event_capacity,
            config.event_max_bytes,
        ));
        let (command_tx, command_rx) = mpsc::channel::<CoreCommand>(64);

        let active_session = storage.latest_session(&config.workspace)?;
        let placeholder = uuid::Uuid::new_v4().to_string();
        let app = app::build_app(
            config.workspace.clone(),
            config.config.clone(),
            storage,
            active_session.clone().unwrap_or(placeholder),
        )
        .await?;

        let engine = Engine {
            app,
            bridge: bridge.clone(),
            pending: HashMap::new(),
            approval_timeout: config.approval_timeout,
            command_tx: command_tx.clone(),
        };
        let engine_task = tokio::spawn(run_engine(engine, command_rx));
        let event_capacity = bridge.max_events();
        let event_max_bytes = bridge.max_bytes();
        let default_page_size = protocol::clamp_page_size(Some(config.message_page_size));

        Ok(AppHandle::new(
            command_tx,
            bridge,
            engine_task,
            lock,
            event_capacity,
            event_max_bytes,
            default_page_size,
        ))
    }
}

/// Per-workspace exclusive lock. A second program opening the same canonical
/// workspace fails immediately at startup.
struct WorkspaceLock {
    _file: std::fs::File,
}

impl WorkspaceLock {
    fn acquire(data_dir: &Path, workspace: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let lock_dir = data_dir.join("workspace-locks");
        std::fs::create_dir_all(&lock_dir)?;
        let name = format!("{}.lock", stable_hash(workspace));
        let file = std::fs::File::create(lock_dir.join(name))?;
        file.try_lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

/// A stable, dependency-free 64-bit FNV-1a hash used to name the per-workspace
/// lock file (stable across Rust versions, unlike `DefaultHasher`).
fn stable_hash(value: &Path) -> u64 {
    let bytes = value.as_os_str().as_encoded_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests;
