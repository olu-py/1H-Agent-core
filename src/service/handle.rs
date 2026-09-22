use std::sync::Arc;

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::protocol::{self, ApiError, AppSnapshotV2, MemoryDto, MessagePage};

use super::{CoreCommand, WorkspaceLock};

/// Shared inner state of an [`AppHandle`]; reference-counted so handles can be
/// cheaply cloned while the engine task and workspace lock live exactly once.
struct AppHandleInner {
    command_tx: mpsc::Sender<CoreCommand>,
    bridge: Arc<crate::bridge::EventBridge>,
    engine_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    _lock: WorkspaceLock,
    event_capacity: usize,
    event_max_bytes: usize,
    default_page_size: usize,
}

impl Drop for AppHandleInner {
    fn drop(&mut self) {
        // Signal a clean shutdown; the engine loop rejects approvals, cancels
        // the agent tree, and closes subscriptions before it exits. The task is
        // not aborted here: aborting could cut the cleanup short.
        let _ = self.command_tx.try_send(CoreCommand::Shutdown);
    }
}

/// A handle to the running application. Cheap to clone; every method serializes
/// its command into the state-machine task and awaits the reply.
#[derive(Clone)]
pub struct AppHandle {
    inner: Arc<AppHandleInner>,
}

impl AppHandle {
    pub(super) fn new(
        command_tx: mpsc::Sender<CoreCommand>,
        bridge: Arc<crate::bridge::EventBridge>,
        engine_task: JoinHandle<()>,
        workspace_lock: WorkspaceLock,
        event_capacity: usize,
        event_max_bytes: usize,
        default_page_size: usize,
    ) -> Self {
        Self {
            inner: Arc::new(AppHandleInner {
                command_tx,
                bridge,
                engine_task: tokio::sync::Mutex::new(Some(engine_task)),
                _lock: workspace_lock,
                event_capacity,
                event_max_bytes,
                default_page_size,
            }),
        }
    }

    /// Fetches the full application snapshot. The returned `event_cursor` is the
    /// position from which the consumer should subscribe.
    pub async fn snapshot(&self) -> Result<AppSnapshotV2, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetState { reply: tx }).await?;
        rx.await
            .map_err(|_| ApiError::internal("state unavailable"))?
    }

    /// Fetches a page of a session's transcript. `before` is the opaque cursor
    /// returned by a previous page's `next_before`; `None` fetches the newest
    /// page. `limit` is clamped to 20..=200.
    pub async fn messages(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: Option<usize>,
    ) -> Result<MessagePage, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetMessages {
            session_id: session_id.to_owned(),
            before,
            limit: limit
                .map(|n| n.clamp(protocol::MIN_PAGE_SIZE, protocol::MAX_PAGE_SIZE))
                .unwrap_or(self.inner.default_page_size),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("messages unavailable"))?
    }

    /// Lists workspace memories. Candidates and source-invalid rows are
    /// returned for review; the core never lets consumers query another
    /// workspace because workspace ownership is held by this service.
    pub async fn memories(
        &self,
        query: Option<&str>,
        include_deleted: bool,
    ) -> Result<Vec<MemoryDto>, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetMemories {
            query: query.map(str::to_owned),
            include_deleted,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("memories unavailable"))?
    }

    pub async fn save_memory(
        &self,
        title: &str,
        content: &str,
        candidate: bool,
    ) -> Result<MemoryDto, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SaveMemory {
            title: title.to_owned(),
            content: content.to_owned(),
            candidate,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("memory save unavailable"))?
    }

    pub async fn confirm_memory(&self, id: i64) -> Result<MemoryDto, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::ConfirmMemory { id, reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("memory confirmation unavailable"))?
    }

    pub async fn update_memory(
        &self,
        id: i64,
        title: &str,
        content: &str,
    ) -> Result<MemoryDto, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::UpdateMemory {
            id,
            title: title.to_owned(),
            content: content.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("memory update unavailable"))?
    }

    pub async fn delete_memory(&self, id: i64) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::DeleteMemory { id, reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("memory deletion unavailable"))?
    }

    /// Submits user input, creating the session when `session_id` is `None`
    /// (the home-screen "first message creates a session" semantic). Returns the
    /// request sequence assigned to the new request; pass it to [`Self::cancel`]
    /// so a stale cancel never aborts a newer request.
    pub async fn submit(&self, session_id: Option<String>, text: &str) -> Result<u64, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SubmitInput {
            session_id,
            text: text.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Executes a slash command against a session.
    pub async fn execute_command(
        &self,
        session_id: Option<String>,
        text: &str,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::ExecuteCommand {
            session_id,
            text: text.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Resolves a pending approval. `allow_session` registers a session-scoped
    /// always-allow rule for the approved tool before resuming the agent (the
    /// TUI "A" key); the rule lives only in memory and config deny still wins.
    pub async fn approve(
        &self,
        approval_id: &str,
        accept: bool,
        allow_session: bool,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::Approve {
            approval_id: approval_id.to_owned(),
            accept,
            allow_session,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Cancels the active request of a session. `request_seq` is the sequence
    /// returned by the submit that started the request: when it no longer
    /// matches the session's current request the cancel is stale and ignored,
    /// so it can never abort a newer request.
    pub async fn cancel(&self, session_id: &str, request_seq: Option<u64>) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::Cancel {
            session_id: session_id.to_owned(),
            request_seq,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Switches the server-side active session.
    pub async fn activate_session(&self, session_id: &str) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::ActivateSession {
            session_id: session_id.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Applies non-secret provider settings (preset + model). API keys stay in
    /// the OS keyring.
    pub async fn set_provider(&self, preset: &str, model: &str) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SetProvider {
            preset: preset.to_owned(),
            model: model.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Applies a complete non-secret provider profile (protocol, base_url,
    /// model, thinking level/budget, context window, retry policy). API keys
    /// never enter the core; the caller stores them in the OS keyring. The
    /// profile is upserted into the saved provider list.
    pub async fn set_provider_config(
        &self,
        provider: crate::config::ProviderConfig,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SetProviderConfig {
            provider,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Applies a settings-screen provider edit: `model` plus optional
    /// `base_url` and protocol onto the current or saved profile of `preset`
    /// (a fresh preset template when nothing is saved). `context_window_tokens`
    /// optionally overrides the merged profile's explicit window (clamped to
    /// the same bounds as `Config::load`); `None` keeps the merged value.
    /// The caller stores any new API key in the OS keyring (e.g.
    /// `secrets::store_api_key_cached`) *before* calling this so the rebuilt
    /// runner picks it up.
    pub async fn set_provider_profile(
        &self,
        preset: crate::config::ProviderPreset,
        model: &str,
        base_url: Option<&str>,
        kind: Option<crate::config::ProviderKind>,
        context_window_tokens: Option<u64>,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SetProviderProfile {
            preset,
            model: model.to_owned(),
            base_url: base_url.map(str::to_owned),
            kind,
            context_window_tokens,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Lists the provider's models for pickers. `refresh = true` refetches
    /// the provider's `GET /models` and models.dev before answering (when
    /// metadata fetching is enabled); `false` answers from cache instantly.
    pub async fn provider_models(
        &self,
        refresh: bool,
    ) -> Result<crate::protocol::ProviderModelsDto, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetProviderModels { refresh, reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Reads the provider settings view: the active profile, the saved
    /// per-preset profiles, and the presets with a currently resolvable API
    /// key. Never includes the keys themselves.
    pub async fn provider_settings(
        &self,
    ) -> Result<crate::protocol::ProviderSettingsDto, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetProviderSettings { reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Removes a saved provider profile, switching the active provider when it
    /// was the one removed. The API key stays in the OS keyring.
    pub async fn remove_provider(
        &self,
        preset: crate::config::ProviderPreset,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::RemoveProvider { preset, reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Replays buffered events after `after`, then returns a live receiver.
    /// Call [`Self::replay_after`] *before* [`Self::subscribe`].
    pub fn replay_after(&self, after: u64) -> crate::bridge::ReplayResult {
        self.inner.bridge.replay_after(after)
    }

    /// Subscribes to live envelopes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<crate::protocol::Envelope>> {
        self.inner.bridge.subscribe()
    }

    /// Replays every event strictly after `after` and returns a live receiver
    /// atomically, so a fresh consumer misses nothing between its snapshot and
    /// the live stream. Returns [`crate::bridge::ResyncRequired`] when `after`
    /// has been evicted from the bridge ring; the caller must then refetch the
    /// snapshot and message page and subscribe again from the fresh cursor.
    ///
    /// Events pushed between the live subscription and the ring snapshot appear
    /// in both `replay` and `live`; consumers deduplicate by cursor (skip live
    /// events whose cursor is `<=` the last cursor processed from `replay`).
    pub fn subscribe_from(
        &self,
        after: u64,
    ) -> Result<crate::bridge::Subscription, crate::bridge::ResyncRequired> {
        self.inner.bridge.subscribe_from(after)
    }

    /// The current process-global cursor (matches the latest snapshot).
    pub fn current_cursor(&self) -> u64 {
        self.inner.bridge.current_cursor()
    }

    /// The clamped bridge ring capacity (events).
    pub fn event_capacity(&self) -> usize {
        self.inner.event_capacity
    }

    /// The clamped bridge ring byte cap.
    pub fn event_max_bytes(&self) -> usize {
        self.inner.event_max_bytes
    }

    /// Gracefully shuts the engine down: pending approvals are rejected, the
    /// agent tree is cancelled, subscriptions are closed, and the database and
    /// workspace lock are released. Returns once the engine task has finished.
    pub async fn shutdown(&self) -> Result<(), ApiError> {
        self.send(CoreCommand::Shutdown).await?;
        if let Some(task) = self.inner.engine_task.lock().await.take() {
            let _ = task.await;
        }
        Ok(())
    }

    async fn send(&self, command: CoreCommand) -> Result<(), ApiError> {
        self.inner
            .command_tx
            .send(command)
            .await
            .map_err(|_| ApiError::internal("server shutting down"))
    }
}
