use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, Semaphore, mpsc, oneshot},
    time::timeout,
};

use crate::{
    config::{
        AgentConfig, ClusterConfig, CompactionConfig, MemoryConfig, NativeWebSearch,
        ProviderConfig, ProviderKind, ProviderPreset, ThinkingCapability, thinking_profile,
    },
    model::{TodoStatus, TodoTask},
    prompt,
    provider::{
        ConversationItem, ModelEvent, ModelRequest, OpenAiClient, ProviderError, Role,
        ThinkingMode, ToolCall, ToolDefinition, is_context_overflow,
    },
    secrets,
    security::PolicyDecision,
    session::{UsageAnchor, trim_conversation_bounded},
    storage::Storage,
    tools::SharedToolRegistry,
};

mod events;

pub use events::{AgentEvent, ChildSessionProgress, ChildSessionStatus};

pub(crate) type ChildProviderResolver =
    dyn Fn(ProviderPreset) -> Result<ProviderConfig, String> + Send + Sync;

/// Upper bound for a single persisted thinking summary. Matches the UI's live
/// thinking buffer limit so the stored summary and the displayed summary stay
/// consistent even when the model streams an unusually long reasoning block.
const MAX_REASONING_BYTES: usize = 64 * 1024;

/// Appends a reasoning delta while keeping the buffer within
/// `MAX_REASONING_BYTES`, retaining the tail (like the live UI buffer) on
/// overflow.
fn append_reasoning_bounded(buffer: &mut String, delta: &str) {
    buffer.push_str(delta);
    if buffer.len() <= MAX_REASONING_BYTES {
        return;
    }
    let minimum = buffer.len() - MAX_REASONING_BYTES;
    let start = buffer
        .char_indices()
        .map(|(offset, _)| offset)
        .find(|offset| *offset >= minimum)
        .unwrap_or(buffer.len());
    buffer.drain(..start);
}

/// Appends `delta` to `buffer` without exceeding `max_bytes`, truncating at a
/// UTF-8 character boundary. Used for bounded child-agent output.
fn append_text_bounded(buffer: &mut String, delta: &str, max_bytes: usize) {
    let remaining = max_bytes.saturating_sub(buffer.len());
    if remaining == 0 {
        return;
    }
    let mut end = delta.len().min(remaining);
    while end > 0 && !delta.is_char_boundary(end) {
        end -= 1;
    }
    buffer.push_str(&delta[..end]);
}

/// Tools a child agent may ever receive. This is intentionally smaller than the
/// role-based filter: no terminal, shell, git mutation, browser, MCP, spawn, or
/// delete tools are ever delegated to a child.
fn child_tool_name_allowed(tool: &str, role: Option<&str>, allowed_tools: &[String]) -> bool {
    const READ_TOOLS: &[&str] = &[
        "file_list",
        "file_stat",
        "file_read",
        "file_search",
        "file_glob",
        "repo_map",
        "web_search",
        "web_fetch",
        "git_diff",
    ];
    const WRITE_TOOLS: &[&str] = &[
        "file_write",
        "file_edit",
        "file_mkdir",
        "file_copy",
        "file_move",
    ];

    if READ_TOOLS.contains(&tool) {
        return true;
    }
    if !allowed_tools.is_empty() {
        return WRITE_TOOLS.contains(&tool) && allowed_tools.iter().any(|name| name == tool);
    }
    WRITE_TOOLS.contains(&tool) && is_implement_role(role)
}

/// Truncates `value` to at most `max_bytes` at a UTF-8 character boundary,
/// appending a marker when bytes were dropped.
fn truncate_utf8_bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[child output truncated]", &value[..end])
}

/// Returns the workspace-relative target path of a mutating file tool call, or
/// `None` for tools that do not target a single file (file_mkdir) or non-file
/// tools.
fn snapshot_target_path(name: &str, arguments: &Value) -> Option<String> {
    match name {
        "file_write" | "file_edit" | "file_delete" => arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned),
        "file_copy" | "file_move" => arguments
            .get("destination")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Maps a model id prefix to its canonical provider preset. Used both to infer
/// an omitted `provider` in `agent_spawn` and to catch an explicit provider
/// that contradicts the requested model (for example `provider=qwen` with
/// `model=deepseek-v4-flash`).
fn model_prefix_preset(model: &str) -> Option<ProviderPreset> {
    let model = model.trim().to_ascii_lowercase();
    if model.starts_with("deepseek") {
        Some(ProviderPreset::DeepSeek)
    } else if model.starts_with("qwen") {
        Some(ProviderPreset::Qwen)
    } else if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some(ProviderPreset::OpenAi)
    } else if model.starts_with("doubao") || model.starts_with("glm") {
        Some(ProviderPreset::Volcano)
    } else {
        None
    }
}

/// Chooses the child provider when `agent_spawn` omits the `provider`
/// argument. Current provider wins when it already lists the model; otherwise
/// an explicit family prefix (deepseek/qwen/gpt/o*/doubao/glm) infers the
/// matching provider, and finally any preset whose selectable list contains
/// the model is tried.
fn infer_child_provider(model: &str, current: ProviderPreset) -> ProviderPreset {
    if current == ProviderPreset::Custom {
        return current;
    }
    let model = model.trim().to_ascii_lowercase();
    if current.selectable_models().contains(&model.as_str()) {
        return current;
    }
    if let Some(preset) = model_prefix_preset(&model) {
        return preset;
    }
    for preset in ProviderPreset::ALL {
        if preset != current && preset.selectable_models().contains(&model.as_str()) {
            return preset;
        }
    }
    current
}

/// Validates a child model id without rejecting new provider models that are
/// absent from the built-in picker list (for example `qwen3.5-flash`). It only
/// rejects empty ids, obvious shorthands such as `v4pro`, and explicit
/// provider/model prefix contradictions.
fn validate_child_model(provider_config: &ProviderConfig) -> Result<(), String> {
    let model = provider_config.model.trim();
    if model.is_empty() {
        return Err("child agent model must not be empty".into());
    }
    if provider_config.preset == ProviderPreset::Custom {
        return Ok(());
    }
    let normalized = model.to_ascii_lowercase();
    let selectable = provider_config.preset.selectable_models();
    if selectable.contains(&normalized.as_str()) {
        return Ok(());
    }
    if let Some(prefix_preset) = model_prefix_preset(&normalized) {
        if prefix_preset != provider_config.preset {
            return Err(format!(
                "model \"{model}\" belongs to {}; set provider={} or omit provider to infer it",
                prefix_preset.label(),
                prefix_preset.key_id()
            ));
        }
    }
    let looks_like_full_id =
        normalized.contains('-') || normalized.contains('.') || normalized.contains(':');
    if !looks_like_full_id {
        return Err(format!(
            "unknown model \"{model}\" for {}; use a full model name such as {}",
            provider_config.preset.label(),
            selectable.join(", ")
        ));
    }
    Ok(())
}

fn child_title(
    arguments: &ChildArgs,
    configured_agent: Option<&AgentConfig>,
    role: Option<&str>,
) -> String {
    if let Some(title) = arguments
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return title.chars().take(80).collect();
    }
    if let Some(agent) = configured_agent {
        return agent.name.clone();
    }
    if let Some(role) = role.map(str::trim).filter(|r| !r.is_empty()) {
        let prompt = arguments.prompt.trim();
        let suffix = if prompt.chars().count() > 18 {
            prompt.chars().take(18).collect::<String>() + "…"
        } else {
            prompt.to_owned()
        };
        return format!("{role}·{suffix}");
    }
    "子 Agent".into()
}

/// Whether a child role implies write access. Planning/review roles stay
/// read-only; implementation/coding roles may write files (subject to the
/// normal approval policy).
fn is_implement_role(role: Option<&str>) -> bool {
    role.is_some_and(|role| {
        let role = role.to_ascii_lowercase();
        [
            "implement",
            "implementation",
            "code",
            "coder",
            "write",
            "build",
            "实施",
            "编码",
        ]
        .iter()
        .any(|keyword| role.contains(keyword))
    })
}

/// Summarizes a child agent's completed tool results so a turn-limited child
/// does not lose all its intermediate work. Returns the last `max_items`
/// results, each truncated to `max_bytes`.
fn summarize_child_trail(items: &[ConversationItem], max_items: usize, max_bytes: usize) -> String {
    let mut summary = String::new();
    let mut count = 0usize;
    for item in items.iter().rev() {
        let ConversationItem::ToolOutput { output, .. } = item else {
            continue;
        };
        if count >= max_items {
            break;
        }
        let output = output.trim();
        if output.is_empty() {
            continue;
        }
        count += 1;
        let end = output.len().min(max_bytes);
        let mut end = end;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        summary.insert_str(0, &format!("\n[tool result]: {}\n", &output[..end]));
    }
    summary
}

fn incremental_request_cursor(items: &[ConversationItem]) -> usize {
    items
        .iter()
        .rposition(|item| {
            matches!(
                item,
                ConversationItem::Message {
                    role: Role::User,
                    ..
                }
            )
        })
        .unwrap_or_else(|| items.len().saturating_sub(1))
}

/// Produces protocol-valid local history for stateless replay. Context
/// trimming or cancellation can leave one half of a tool call/output pair;
/// Responses endpoints reject either an orphan output or an unanswered call.
fn replay_safe_items(items: &[ConversationItem]) -> Vec<ConversationItem> {
    let output_ids = items
        .iter()
        .filter_map(|item| match item {
            ConversationItem::ToolOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut known_calls = HashSet::<String>::new();
    let mut replay = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ConversationItem::AssistantToolCalls { calls } => {
                let calls = calls
                    .iter()
                    .filter(|call| output_ids.contains(call.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !calls.is_empty() {
                    known_calls.extend(calls.iter().map(|call| call.id.clone()));
                    replay.push(ConversationItem::AssistantToolCalls { calls });
                }
            }
            ConversationItem::ToolOutput { call_id, .. } if !known_calls.contains(call_id) => {}
            _ => replay.push(item.clone()),
        }
    }
    replay
}

fn child_progress(
    status: ChildSessionStatus,
    turn: usize,
    max_turns: usize,
    tool: Option<String>,
) -> ChildSessionProgress {
    ChildSessionProgress {
        status,
        turn,
        max_turns,
        tool,
        updated_at: Instant::now(),
    }
}

async fn emit_child_progress(
    ui_events: &mpsc::Sender<AgentEvent>,
    session_id: &str,
    progress: ChildSessionProgress,
) {
    let _ = ui_events
        .send(AgentEvent::ChildSessionProgress {
            session_id: session_id.to_owned(),
            progress,
        })
        .await;
}

struct ChildCancellationGuard {
    ui_events: mpsc::Sender<AgentEvent>,
    session_id: String,
    max_turns: usize,
    finished: bool,
}

struct ChildToolContext<'a> {
    child_id: &'a str,
    child_title: &'a str,
    ui_events: &'a mpsc::Sender<AgentEvent>,
    turn: usize,
    max_turns: usize,
}

impl ChildCancellationGuard {
    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for ChildCancellationGuard {
    fn drop(&mut self) {
        if !self.finished {
            let event = AgentEvent::ChildSessionProgress {
                session_id: self.session_id.clone(),
                progress: child_progress(ChildSessionStatus::Cancelled, 0, self.max_turns, None),
            };
            if let Err(mpsc::error::TrySendError::Full(event)) = self.ui_events.try_send(event) {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let ui_events = self.ui_events.clone();
                    runtime.spawn(async move {
                        let _ = ui_events.send(event).await;
                    });
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct TodoWriteArguments {
    tasks: Vec<TodoWriteTask>,
}

#[derive(Deserialize)]
struct TodoWriteTask {
    #[serde(default)]
    id: Option<String>,
    title: String,
    status: TodoStatus,
}

#[derive(Serialize)]
struct TodoToolTask<'a> {
    id: &'a str,
    title: &'a str,
    status: TodoStatus,
}

#[derive(Serialize)]
struct TodoToolResponse<'a> {
    tasks: Vec<TodoToolTask<'a>>,
}

fn todo_tool_response(tasks: &[TodoTask]) -> Result<String, String> {
    serde_json::to_string(&TodoToolResponse {
        tasks: tasks
            .iter()
            .map(|task| TodoToolTask {
                id: task.id.as_str(),
                title: task.title.as_str(),
                status: task.status,
            })
            .collect(),
    })
    .map_err(|error| error.to_string())
}

#[derive(Clone)]
pub struct AgentRunner {
    provider: OpenAiClient,
    provider_config: ProviderConfig,
    tools: SharedToolRegistry,
    storage: Storage,
    session_id: String,
    approval_lock: Arc<Mutex<()>>,
    child_slots: Arc<Semaphore>,
    child_role: Option<String>,
    cluster: ClusterConfig,
    configured_agents: Arc<Vec<AgentConfig>>,
    child_provider_resolver: Option<Arc<ChildProviderResolver>>,
    compaction: CompactionConfig,
    /// Session token-estimate calibration (real usage / local estimate,
    /// clamped 0.5..=2.0), snapshotted when the runner was built. Scales the
    /// heuristic estimate so the compaction threshold tracks the provider's
    /// real tokenizer (byte/4 underestimates CJK-heavy context).
    token_calibration: f64,
    /// Session usage anchor snapshot (see `session::UsageAnchor`), stamped
    /// when the runner is built/submitted. Lets `compact_if_needed` compare
    /// the anchored meter against the compaction threshold instead of the
    /// full calibrated estimate. Never re-read after an in-run compaction
    /// rewrote the prefix — the recovery gate compares calibrated estimates.
    usage_anchor: Option<UsageAnchor>,
    memory: MemoryConfig,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates the common per-round streaming state: assistant/reasoning text,
/// partial tool calls, and completed tool calls. The streaming loop and partial
/// convergence are shared by the main agent and child agents.
struct StreamCollector {
    assistant_text: String,
    reasoning_text: String,
    partials: HashMap<String, PartialToolCall>,
    completed_calls: Vec<ToolCall>,
    completed_ids: HashSet<String>,
    saw_done: bool,
    max_text_bytes: Option<usize>,
    max_tool_call_bytes: usize,
    max_tool_call_total_bytes: usize,
    max_tool_calls: usize,
    tool_call_bytes: usize,
}

impl StreamCollector {
    fn new(max_text_bytes: Option<usize>) -> Self {
        Self {
            assistant_text: String::new(),
            reasoning_text: String::new(),
            partials: HashMap::new(),
            completed_calls: Vec::new(),
            completed_ids: HashSet::new(),
            saw_done: false,
            max_text_bytes,
            max_tool_call_bytes: 1024 * 1024,
            max_tool_call_total_bytes: 4 * 1024 * 1024,
            max_tool_calls: 32,
            tool_call_bytes: 0,
        }
    }

    fn with_memory_limits(mut self, memory: MemoryConfig) -> Self {
        self.max_tool_call_bytes = memory.max_tool_call_bytes;
        self.max_tool_call_total_bytes = memory.max_tool_call_total_bytes;
        self.max_tool_calls = memory.max_tool_calls;
        self
    }

    /// Accumulates text/tool-call state. Returns the event unchanged when it
    /// needs caller-level side effects (web search, provider items, usage,
    /// response id, or reasoning forwarding); returns None when fully handled.
    fn on_event(&mut self, event: ModelEvent) -> Result<Option<ModelEvent>, String> {
        match event {
            ModelEvent::TextDelta(delta) => {
                if let Some(max_bytes) = self.max_text_bytes {
                    if self.assistant_text.len().saturating_add(delta.len()) > max_bytes {
                        append_text_bounded(&mut self.assistant_text, &delta, max_bytes);
                        return Err(format!(
                            "model response exceeded the {} byte limit",
                            max_bytes
                        ));
                    }
                    append_text_bounded(&mut self.assistant_text, &delta, max_bytes);
                } else {
                    self.assistant_text.push_str(&delta);
                }
                Ok(Some(ModelEvent::TextDelta(delta)))
            }
            ModelEvent::ReasoningDelta(delta) => {
                append_reasoning_bounded(&mut self.reasoning_text, &delta);
                Ok(Some(ModelEvent::ReasoningDelta(delta)))
            }
            ModelEvent::ToolCallDelta {
                slot,
                id,
                name,
                arguments_delta,
            } => {
                let delta_bytes = arguments_delta.len();
                let forwarded_name = name.clone();
                let forwarded_slot = slot.clone();
                if !self.partials.contains_key(&slot)
                    && self
                        .partials
                        .len()
                        .saturating_add(self.completed_calls.len())
                        >= self.max_tool_calls
                {
                    return Err(format!(
                        "model returned more than {} tool calls",
                        self.max_tool_calls
                    ));
                }
                {
                    let partial = self.partials.entry(slot).or_default();
                    if partial.arguments.len().saturating_add(delta_bytes)
                        > self.max_tool_call_bytes
                        || self.tool_call_bytes.saturating_add(delta_bytes)
                            > self.max_tool_call_total_bytes
                    {
                        return Err("tool call arguments exceeded the configured byte limit".into());
                    }
                    if let Some(id) = id {
                        partial.id = id;
                    }
                    if let Some(name) = name {
                        partial.name = name;
                    }
                    partial.arguments.push_str(&arguments_delta);
                }
                self.tool_call_bytes = self.tool_call_bytes.saturating_add(delta_bytes);
                let forwarded = ModelEvent::ToolCallDelta {
                    slot: forwarded_slot,
                    id: None,
                    name: forwarded_name,
                    arguments_delta,
                };
                // Forward the raw delta so the caller's closure can merge it
                // into streaming progress events (TextDelta/ReasoningDelta
                // follow the same pass-through pattern). `id` is internal to
                // partial accumulation; the closure only needs name + bytes.
                Ok(Some(forwarded))
            }
            ModelEvent::ToolCallComplete(call) => {
                let completed_partial = self
                    .partials
                    .iter()
                    .find(|(_, partial)| partial.id == call.id)
                    .map(|(slot, partial)| (slot.clone(), partial.arguments.len()));
                if let Some((slot, partial_bytes)) = completed_partial {
                    self.partials.remove(&slot);
                    self.tool_call_bytes = self.tool_call_bytes.saturating_sub(partial_bytes);
                }
                let call_bytes = call
                    .name
                    .len()
                    .saturating_add(call.arguments.to_string().len());
                if self
                    .completed_calls
                    .len()
                    .saturating_add(self.partials.len())
                    >= self.max_tool_calls
                {
                    return Err(format!(
                        "model returned more than {} tool calls",
                        self.max_tool_calls
                    ));
                }
                if call_bytes > self.max_tool_call_bytes
                    || self.tool_call_bytes.saturating_add(call_bytes)
                        > self.max_tool_call_total_bytes
                {
                    return Err("tool call arguments exceeded the configured byte limit".into());
                }
                self.tool_call_bytes = self.tool_call_bytes.saturating_add(call_bytes);
                self.completed_ids.insert(call.id.clone());
                self.completed_calls.push(call);
                Ok(None)
            }
            ModelEvent::Done => {
                self.saw_done = true;
                Ok(None)
            }
            other => Ok(Some(other)),
        }
    }

    fn finish_partials(&mut self, error_kind: &str) -> Result<(), String> {
        for partial in std::mem::take(&mut self.partials).into_values() {
            if self.completed_ids.contains(&partial.id) || partial.name.is_empty() {
                continue;
            }
            let arguments: Value = serde_json::from_str(if partial.arguments.is_empty() {
                "{}"
            } else {
                &partial.arguments
            })
            .map_err(|error| format!("invalid {error_kind} arguments: {error}"))?;
            self.completed_calls.push(ToolCall {
                id: if partial.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    partial.id
                },
                name: partial.name,
                arguments,
            });
        }
        Ok(())
    }
}

/// What a forwarded stream event should do on the UI channel.
enum Forwarded {
    /// Send this agent event, propagating send failures.
    Send(AgentEvent),
    /// Send multiple agent events in order, propagating send failures. One
    /// model event may expand into a fixed ordered sequence (e.g. the
    /// `ReasoningCompleted` barrier immediately before the first `TextDelta`).
    SendMany(Vec<AgentEvent>),
    /// Send this agent event, ignoring send failures.
    SendIgnore(AgentEvent),
    /// The event was handled locally and needs no UI forwarding.
    Ignore,
}

/// Why a single model stream round failed.
enum StreamFailure {
    /// A caller-side event handler failed (fatal).
    Handler(String),
    /// The provider returned an error for this request (replayable when the
    /// round produced no output). The structured error lets the agent layer
    /// recognize context overflows for the compact→retry recovery.
    Provider(ProviderError),
    /// The spawned provider task failed to join (fatal).
    Join(String),
    /// The local hard limit stopped the provider round. The collector retains
    /// the bounded partial answer, but incomplete tool arguments are never
    /// passed to execution.
    Limit(String),
    /// The stream ended without a Done marker (fatal).
    EndedWithoutCompletion,
}

/// Streams one model request into `collector`, forwarding events the collector
/// does not own to `forward`, then sending the resulting agent events to the UI.
async fn stream_once(
    provider: &OpenAiClient,
    request: ModelRequest,
    collector: &mut StreamCollector,
    channel_capacity: usize,
    max_agent_event_bytes: usize,
    ui_events: &mpsc::Sender<AgentEvent>,
    mut forward: impl FnMut(ModelEvent) -> Result<Forwarded, String>,
) -> Result<(), StreamFailure> {
    let (model_tx, mut model_rx) = mpsc::channel(channel_capacity);
    let provider = provider.clone();
    let provider_task = tokio::spawn(async move { provider.stream(request, model_tx).await });
    while let Some(event) = model_rx.recv().await {
        let forwarded = match collector.on_event(event) {
            Ok(forwarded) => forwarded,
            Err(error) => {
                // Dropping a JoinHandle does not cancel the provider task. An
                // explicit abort is required here or a rejected oversized
                // stream would keep reading the network in the background.
                provider_task.abort();
                let _ = provider_task.await;
                return Err(StreamFailure::Limit(error));
            }
        };
        if let Some(event) = forwarded {
            match forward(event).map_err(StreamFailure::Handler)? {
                Forwarded::Send(agent_event) => {
                    send_agent_event_bounded(ui_events, agent_event, max_agent_event_bytes)
                        .await
                        .map_err(|_| {
                            StreamFailure::Handler("UI event receiver closed".to_owned())
                        })?
                }
                Forwarded::SendMany(agent_events) => {
                    for agent_event in agent_events {
                        send_agent_event_bounded(ui_events, agent_event, max_agent_event_bytes)
                            .await
                            .map_err(|_| {
                                StreamFailure::Handler("UI event receiver closed".to_owned())
                            })?;
                    }
                }
                Forwarded::SendIgnore(agent_event) => {
                    let _ = send_agent_event_bounded(ui_events, agent_event, max_agent_event_bytes)
                        .await;
                }
                Forwarded::Ignore => {}
            }
        }
        if collector.saw_done {
            break;
        }
    }
    let provider_result = provider_task
        .await
        .map_err(|error| StreamFailure::Join(error.to_string()))?;
    if let Err(error) = provider_result {
        return Err(StreamFailure::Provider(error));
    }
    if !collector.saw_done {
        return Err(StreamFailure::EndedWithoutCompletion);
    }
    Ok(())
}

/// Keeps individual queue payloads bounded even when a provider sends one
/// unusually large delta. The collector's total response limit still decides
/// whether the round may continue; this helper only controls queue pressure.
async fn send_agent_event_bounded(
    sender: &mpsc::Sender<AgentEvent>,
    event: AgentEvent,
    max_bytes: usize,
) -> Result<(), Box<mpsc::error::SendError<AgentEvent>>> {
    let max_bytes = max_bytes.max(1);
    match event {
        AgentEvent::TextDelta(mut text) => {
            while !text.is_empty() {
                let mut end = text.len().min(max_bytes);
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    end = text.chars().next().map(char::len_utf8).unwrap_or(0);
                }
                let chunk = text[..end].to_owned();
                text.drain(..end);
                sender
                    .send(AgentEvent::TextDelta(chunk))
                    .await
                    .map_err(Box::new)?;
            }
            Ok(())
        }
        AgentEvent::ReasoningDelta(mut text) => {
            while !text.is_empty() {
                let mut end = text.len().min(max_bytes);
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    end = text.chars().next().map(char::len_utf8).unwrap_or(0);
                }
                let chunk = text[..end].to_owned();
                text.drain(..end);
                sender
                    .send(AgentEvent::ReasoningDelta(chunk))
                    .await
                    .map_err(Box::new)?;
            }
            Ok(())
        }
        event => sender.send(event).await.map_err(Box::new),
    }
}

impl AgentRunner {
    pub fn new(
        provider: OpenAiClient,
        provider_config: ProviderConfig,
        tools: SharedToolRegistry,
        storage: Storage,
        session_id: String,
    ) -> Self {
        Self {
            provider,
            provider_config,
            tools,
            storage,
            session_id,
            approval_lock: Arc::new(Mutex::new(())),
            child_slots: Arc::new(Semaphore::new(4)),
            child_role: None,
            cluster: ClusterConfig::default(),
            configured_agents: Arc::new(Vec::new()),
            child_provider_resolver: None,
            compaction: CompactionConfig::default(),
            token_calibration: 1.0,
            usage_anchor: None,
            memory: MemoryConfig::default(),
        }
    }

    pub fn with_cluster_config(mut self, cluster: ClusterConfig) -> Self {
        self.child_slots = Arc::new(Semaphore::new(
            cluster.max_parallel_children.unwrap_or(4).clamp(1, 32),
        ));
        self.cluster = cluster;
        self
    }

    pub fn with_approval_lock(mut self, approval_lock: Arc<Mutex<()>>) -> Self {
        self.approval_lock = approval_lock;
        self
    }

    pub fn with_configured_agents(mut self, agents: Vec<AgentConfig>) -> Self {
        self.configured_agents = Arc::new(agents);
        self
    }

    pub fn with_child_role(mut self, child_role: Option<String>) -> Self {
        self.child_role = child_role;
        self
    }

    pub fn with_child_provider_resolver(mut self, resolver: Arc<ChildProviderResolver>) -> Self {
        self.child_provider_resolver = Some(resolver);
        self
    }

    pub fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        self.compaction = config;
        self.compaction.normalize();
        self
    }

    pub fn with_token_calibration(mut self, calibration: f64) -> Self {
        self.token_calibration = calibration.clamp(0.5, 2.0);
        self
    }

    /// Stamps the session's current usage anchor onto this runner snapshot.
    /// `None` (or a stale anchor the meter guards against) falls back to the
    /// calibrated whole-conversation estimate.
    pub fn with_usage_anchor(mut self, anchor: Option<UsageAnchor>) -> Self {
        self.usage_anchor = anchor;
        self
    }

    pub fn with_memory_config(mut self, memory: MemoryConfig) -> Self {
        self.memory = memory;
        self.memory.normalize();
        self
    }

    /// Applies a runtime-discovered metadata stamp (provider `/models` or
    /// models.dev) so in-flight compaction decisions see the fresh window.
    /// No-op unless this runner targets the same base URL and model.
    pub(crate) fn set_discovered_meta(
        &mut self,
        base_url: &str,
        model: &str,
        discovered: Option<crate::model_meta::DiscoveredMeta>,
    ) {
        if self.provider_config.base_url == base_url && self.provider_config.model == model {
            self.provider_config.discovered = discovered;
        }
    }

    /// Local estimate scaled by the session calibration factor.
    fn calibrated_estimate(&self, tokens: u64) -> u64 {
        (tokens as f64 * self.token_calibration).ceil() as u64
    }

    fn tools_for_request(&self) -> Vec<ToolDefinition> {
        match &self.child_role {
            Some(role) => self
                .tools
                .definitions()
                .into_iter()
                .filter(|tool| child_tool_name_allowed(&tool.name, Some(role), &[]))
                .collect(),
            None => self.tools.definitions(),
        }
    }

    fn child_tool_definitions(
        &self,
        role: Option<&str>,
        allowed_tools: &[String],
    ) -> Vec<ToolDefinition> {
        self.tools
            .definitions()
            .into_iter()
            .filter(|tool| child_tool_name_allowed(&tool.name, role, allowed_tools))
            .collect()
    }

    fn configured_agent(&self, name: &str) -> Option<AgentConfig> {
        self.configured_agents
            .iter()
            .find(|agent| agent.name == name)
            .cloned()
    }

    fn resolve_child_provider(
        &self,
        requested_preset: Option<ProviderPreset>,
        requested_model: Option<&str>,
    ) -> Result<(OpenAiClient, ProviderConfig), String> {
        let model = requested_model
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        let preset = match requested_preset {
            Some(preset) => preset,
            None => match &model {
                Some(model) => infer_child_provider(model, self.provider_config.preset),
                None => self.provider_config.preset,
            },
        };
        let (provider, mut provider_config) = if preset == self.provider_config.preset {
            (self.provider.clone(), self.provider_config.clone())
        } else {
            let resolver = self.child_provider_resolver.as_ref().ok_or_else(|| {
                format!(
                    "cross-provider child agents are not configured; use {} models only",
                    self.provider_config.preset.label()
                )
            })?;
            let mut provider_config = resolver(preset)?;
            provider_config
                .validate()
                .map_err(|error| format!("invalid child provider configuration: {error}"))?;
            provider_config.normalize_thinking();
            let api_key =
                secrets::api_key_cached_only(preset).map_err(|error| error.to_string())?;
            let provider = OpenAiClient::new_with_retry(
                provider_config.base_url.clone(),
                api_key,
                provider_config.retry_max_attempts,
                provider_config.retry_initial_backoff_ms,
                provider_config.retry_max_backoff_ms,
            )
            .map(|provider| {
                provider.with_sse_limits(
                    self.memory.max_sse_frame_bytes,
                    self.memory.max_sse_buffer_bytes,
                )
            })
            .map_err(|error| error.to_string())?;
            (provider, provider_config)
        };
        if let Some(model) = model {
            provider_config.model = model;
        }
        validate_child_model(&provider_config)?;
        provider_config.normalize_thinking();
        provider_config.use_previous_response_id = false;
        Ok((provider, provider_config))
    }

    async fn compact_if_needed(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) {
        if !self.compaction.enabled {
            return;
        }
        let Some(window) = self.provider_config.resolved_context_window_tokens() else {
            return;
        };
        // The anchored meter (real usage prefix + calibrated increment) when
        // a valid anchor was stamped; otherwise the calibrated estimate.
        let estimated = crate::session::estimate_used_tokens(
            self.usage_anchor.as_ref(),
            items,
            self.token_calibration,
        );
        if (estimated as f64) < (window as f64 * f64::from(self.compaction.auto_threshold)) {
            return;
        }
        if let Err(error) = self.compact_context(items, None, ui_events).await {
            let _ = ui_events.send(AgentEvent::CompactionFailed(error)).await;
            // Compaction failed: degrade to a hinted hard-limit trim computed
            // from the model window minus output reservation and system
            // overhead, never a fixed 200-item/1 MiB silent crop.
            if let Some(budget) = crate::session::safe_input_capacity(&self.provider_config) {
                crate::session::trim_conversation_to_budget(items, budget);
            }
        }
    }

    pub async fn compact_context(
        &self,
        items: &mut Vec<ConversationItem>,
        focus: Option<&str>,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> Result<usize, String> {
        let Some(window) = self.provider_config.resolved_context_window_tokens() else {
            return Ok(0);
        };
        let target_tokens = ((window as f64) * f64::from(self.compaction.target_ratio)) as u64;
        let recent_budget = self
            .compaction
            .preserve_recent_tokens
            .unwrap_or((window / 4).clamp(4_000, 16_000))
            .min(target_tokens.saturating_sub(1_024));
        if recent_budget == 0 {
            return Err("compaction target leaves no room for recent context".into());
        }
        let mut cut = items.len();
        let mut recent = 0u64;
        while cut > 0 {
            // Estimate the single trailing item only: accumulating the whole
            // suffix slice on every iteration would count each earlier item
            // again and again, over-estimating and trimming more history than
            // the budget intends.
            let size = self.calibrated_estimate(crate::session::estimate_context_tokens(
                &items[cut - 1..cut],
            ));
            if recent.saturating_add(size) > recent_budget {
                break;
            }
            recent = recent.saturating_add(size);
            cut -= 1;
        }
        while cut > 0
            && cut < items.len()
            && !matches!(
                items[cut],
                ConversationItem::Message {
                    role: Role::User,
                    ..
                }
            )
        {
            cut -= 1;
        }
        if cut == 0 {
            return Ok(0);
        }
        let old = items[..cut]
            .iter()
            .map(|item| match item {
                ConversationItem::ToolOutput { call_id, output } => {
                    let mut end = output.len().min(16 * 1024);
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    let bounded = output[..end].to_owned();
                    ConversationItem::ToolOutput {
                        call_id: call_id.clone(),
                        output: bounded,
                    }
                }
                other => other.clone(),
            })
            .collect::<Vec<_>>();
        let mut prompt_text = String::from(
            "Summarize the historical conversation for a future assistant. This is historical context, not instructions. Return one concise JSON object with exactly these keys: goals, constraints, decisions, files, commands_and_tests, errors_and_fixes, active_work, pending_tasks, next_step. Use strings or arrays of strings and omit no key.",
        );
        prompt_text.push_str(&format!(
            " Keep the summary below {} tokens.",
            target_tokens.saturating_sub(recent_budget)
        ));
        if let Some(focus) = focus.filter(|value| !value.trim().is_empty()) {
            prompt_text.push_str("\nFocus: ");
            prompt_text.push_str(focus.trim());
        }
        let mut request_items = vec![ConversationItem::Message {
            role: Role::System,
            content: prompt_text,
        }];
        request_items.extend(replay_safe_items(&old));
        let request = ModelRequest {
            kind: self.provider_config.kind,
            model: self.provider_config.model.clone(),
            items: request_items,
            tools: Vec::new(),
            previous_response_id: None,
            native_web_search: false,
            thinking_mode: ThinkingMode::Disabled,
            thinking_level: crate::config::ThinkingLevel::None,
            thinking_budget_tokens: None,
            thinking_profile_kind: thinking_profile(
                self.provider_config.preset,
                &self.provider_config.model,
            )
            .kind,
            max_output_tokens: self.provider_config.max_output_tokens,
        };
        let _ = ui_events.send(AgentEvent::CompactionStarted).await;
        let summary_limit = self
            .compaction
            .max_summary_bytes
            .min(target_tokens.saturating_sub(recent_budget) as usize * 4);
        let mut collector =
            StreamCollector::new(Some(summary_limit)).with_memory_limits(self.memory);
        stream_once(
            &self.provider,
            request,
            &mut collector,
            128,
            self.memory.max_agent_event_bytes,
            ui_events,
            |_| Ok(Forwarded::Ignore),
        )
        .await
        .map_err(|failure| match failure {
            StreamFailure::Handler(error)
            | StreamFailure::Join(error)
            | StreamFailure::Limit(error) => error,
            StreamFailure::Provider(error) => error.to_string(),
            StreamFailure::EndedWithoutCompletion => {
                "compaction stream ended without completion".into()
            }
        })?;
        let raw_summary = collector.assistant_text.trim();
        if raw_summary.is_empty() {
            return Err("compaction returned an empty summary".into());
        }
        let summary = serde_json::from_str::<Value>(raw_summary)
            .ok()
            .filter(Value::is_object)
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| raw_summary.to_owned());
        let hidden = self
            .storage
            .compact_with_summary(
                &self.session_id,
                &summary,
                items.len().saturating_sub(cut).max(1),
            )
            .map_err(|error| error.to_string())?;
        let mut canonical = vec![ConversationItem::CompactionSummary { content: summary }];
        canonical.extend_from_slice(&items[cut..]);
        *items = canonical;
        let _ = ui_events
            .send(AgentEvent::CompactionCompleted { hidden })
            .await;
        Ok(hidden)
    }

    pub async fn run(&self, mut items: Vec<ConversationItem>, ui_events: mpsc::Sender<AgentEvent>) {
        self.compact_if_needed(&mut items, &ui_events).await;
        if let Err(error) = self.run_at_depth(&mut items, &ui_events, 0).await {
            if error.starts_with("cancelled:") {
                let _ = ui_events.send(AgentEvent::Cancelled(error)).await;
            } else {
                let _ = ui_events.send(AgentEvent::Failed(error)).await;
            }
        }
    }

    async fn run_at_depth(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
        depth: usize,
    ) -> Result<(), String> {
        self.run_inner(items, ui_events, depth).await
    }

    /// Persists the half-generated answer as `assistant_partial` and clears the
    /// provider response id so the next request never resumes an interrupted
    /// stream. Best-effort: a storage failure must not mask the stream failure.
    fn save_partial(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let _ = self.storage.save_partial(&self.session_id, text);
        let _ = self.storage.clear_response_id(&self.session_id);
    }

    async fn run_inner(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
        depth: usize,
    ) -> Result<(), String> {
        let mut previous_response_id = if self.provider_config.use_previous_response_id {
            self.storage
                .response_id(&self.session_id)
                .map_err(|error| error.to_string())?
        } else {
            None
        };
        // A persisted response already contains all but the newly appended user item.
        let mut request_cursor = if previous_response_id.is_some() {
            incremental_request_cursor(items)
        } else {
            0
        };
        let native_web_search = self.provider_config.preset == ProviderPreset::DeepSeek
            && self.provider_config.kind == ProviderKind::Responses
            && self.provider_config.native_web_search != NativeWebSearch::Disabled;
        let mut executed_tool_calls = HashSet::<String>::new();
        // Recovery attempts for provider-confirmed context overflows within
        // this run. Bounded by `compaction.max_overflow_retries` (0 disables
        // the recovery entirely).
        let mut overflow_retries: u32 = 0;
        loop {
            let request_items = if previous_response_id.is_some() {
                items[request_cursor..].to_vec()
            } else {
                replay_safe_items(items)
            };
            let mut request_items = request_items;
            if previous_response_id.is_none() {
                request_items.insert(
                    0,
                    ConversationItem::Message {
                        role: Role::System,
                        content: match &self.child_role {
                            Some(role) => prompt::child_system_prompt(Some(role), &[]),
                            None => prompt::system_prompt(
                                self.provider_config.preset,
                                self.tools.mode(),
                            ),
                        },
                    },
                );
            }
            let request = ModelRequest {
                kind: self.provider_config.kind,
                model: self.provider_config.model.clone(),
                items: request_items,
                tools: self.tools_for_request(),
                previous_response_id: previous_response_id.clone(),
                native_web_search,
                thinking_mode: thinking_mode_for(&self.provider_config),
                thinking_level: self.provider_config.thinking_level,
                thinking_budget_tokens: self.provider_config.thinking_budget_tokens,
                thinking_profile_kind: thinking_profile(
                    self.provider_config.preset,
                    &self.provider_config.model,
                )
                .kind,
                max_output_tokens: self.provider_config.max_output_tokens,
            };
            // Full-replay rounds (no previous_response_id) carry the entire
            // conversation, so their real usage anchors the token-estimate
            // calibration. Incremental rounds resume server-side state and
            // are not comparable — report 0 and skip recalibration.
            let input_estimate = if previous_response_id.is_none() {
                crate::session::estimate_context_tokens(&request.items)
            } else {
                0
            };
            ui_events
                .send(AgentEvent::ModelStreaming)
                .await
                .map_err(|_| "UI event receiver closed".to_owned())?;
            let mut collector = StreamCollector::new(Some(self.memory.max_response_bytes))
                .with_memory_limits(self.memory);
            let mut search_results = 0usize;
            let mut search_bytes = 0usize;
            // Per-round reasoning phase tracking: this closure is rebuilt on every
            // outer-loop round, so the phase resets between tool/model rounds and
            // a completion barrier is never repeated across rounds.
            let mut reasoning_seen = false;
            let mut reasoning_emitted = false;
            // Per-round tool-call streaming progress. The closure is rebuilt on
            // every round, so the merge state resets between rounds; a 9190-byte
            // `file_write` argument stream becomes ~10 bounded events instead of
            // hundreds of deltas.
            let mut tool_stream_name: Option<String> = None;
            let mut tool_stream_bytes: u64 = 0;
            let mut tool_stream_next_emit: u64 = 0;
            match stream_once(
                &self.provider,
                request,
                &mut collector,
                128,
                self.memory.max_agent_event_bytes,
                ui_events,
                |event| match event {
                    ModelEvent::WebSearchStarted { query } => {
                        Ok(Forwarded::Send(AgentEvent::WebSearchStarted { query }))
                    }
                    ModelEvent::WebSearchResult {
                        title,
                        url,
                        snippet,
                    } => {
                        let item_bytes = title.len() + url.len() + snippet.len();
                        if search_results < 10 && search_bytes + item_bytes <= 64 * 1024 {
                            search_results += 1;
                            search_bytes += item_bytes;
                            let label = format!("搜索来源：{title}");
                            let content = format!("{url}\n{snippet}");
                            items.push(ConversationItem::Context {
                                label: label.clone(),
                                content: content.clone(),
                            });
                            self.storage
                                .append_context(&self.session_id, &label, &content)
                                .map_err(|error| error.to_string())?;
                            Ok(Forwarded::Send(AgentEvent::WebSearchResult {
                                title,
                                url,
                                snippet,
                            }))
                        } else {
                            Ok(Forwarded::Ignore)
                        }
                    }
                    ModelEvent::WebSearchCompleted { count } => {
                        Ok(Forwarded::Send(AgentEvent::WebSearchCompleted {
                            count: if count == 0 {
                                search_results
                            } else {
                                count.min(10)
                            },
                        }))
                    }
                    ModelEvent::ProviderItem(item) => {
                        let encoded = serde_json::to_vec(&item)
                            .map_err(|error| format!("invalid provider item: {error}"))?;
                        if encoded.len() <= 64 * 1024 {
                            items.push(ConversationItem::ProviderItem { item: item.clone() });
                            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                                self.storage
                                    .append_provider_item(&self.session_id, &item)
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                        Ok(Forwarded::Ignore)
                    }
                    ModelEvent::ReasoningDelta(delta) => {
                        reasoning_seen = true;
                        Ok(Forwarded::Send(AgentEvent::ReasoningDelta(delta)))
                    }
                    ModelEvent::Retrying {
                        attempt,
                        reason,
                        delay_ms,
                    } => Ok(Forwarded::SendIgnore(AgentEvent::ProviderRetry {
                        attempt,
                        reason,
                        delay_ms,
                    })),
                    ModelEvent::TextDelta(delta) => {
                        // The reasoning phase ends at the first body delta: emit
                        // the ReasoningCompleted barrier immediately before it so
                        // consumers can draw the finished thinking view on its
                        // own frame before the answer starts streaming below it.
                        if reasoning_seen && !reasoning_emitted {
                            reasoning_emitted = true;
                            Ok(Forwarded::SendMany(vec![
                                AgentEvent::ReasoningCompleted,
                                AgentEvent::TextDelta(delta),
                            ]))
                        } else {
                            Ok(Forwarded::Send(AgentEvent::TextDelta(delta)))
                        }
                    }
                    ModelEvent::Usage(usage) => Ok(Forwarded::SendIgnore(AgentEvent::Usage {
                        usage,
                        input_estimate,
                    })),
                    ModelEvent::ResponseId(id) => {
                        if self.provider_config.use_previous_response_id {
                            previous_response_id = Some(id.clone());
                            self.storage
                                .save_response_id(&self.session_id, &id)
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(Forwarded::Ignore)
                    }
                    ModelEvent::ToolCallDelta {
                        name,
                        arguments_delta,
                        ..
                    } => {
                        // Merge into ~1 KiB progress reports: emit on the first
                        // delta, on the first known tool name, and whenever the
                        // cumulative byte count crosses the next threshold. The
                        // UI animates "generating tool call" instead of freezing
                        // while a large argument payload streams.
                        let first = tool_stream_bytes == 0;
                        tool_stream_bytes =
                            tool_stream_bytes.saturating_add(arguments_delta.len() as u64);
                        let name_first_known = name.is_some() && tool_stream_name.is_none();
                        if let Some(name) = name {
                            tool_stream_name = Some(name);
                        }
                        if first || name_first_known || tool_stream_bytes >= tool_stream_next_emit {
                            if tool_stream_next_emit == 0 {
                                tool_stream_next_emit = 1024;
                            }
                            // A single giant delta may cross several thresholds
                            // at once; skip past all of them so the next emit
                            // still waits a full 1 KiB.
                            while tool_stream_bytes >= tool_stream_next_emit {
                                tool_stream_next_emit = tool_stream_next_emit.saturating_add(1024);
                            }
                            Ok(Forwarded::SendIgnore(AgentEvent::ToolCallStreaming {
                                name: tool_stream_name.clone(),
                                received_bytes: tool_stream_bytes,
                            }))
                        } else {
                            Ok(Forwarded::Ignore)
                        }
                    }
                    ModelEvent::ToolCallComplete(_) | ModelEvent::Done => Ok(Forwarded::Ignore),
                },
            )
            .await
            {
                Ok(()) => {}
                Err(StreamFailure::Provider(error)) => {
                    // Provider-confirmed context overflow: run the compact →
                    // (trim) → retry recovery while attempts remain. A retry
                    // is only granted when the request actually shrank, so a
                    // failing compaction cannot loop; the original error stays
                    // authoritative otherwise.
                    if is_context_overflow(&error)
                        && overflow_retries < self.compaction.max_overflow_retries
                    {
                        let before = self
                            .calibrated_estimate(crate::session::estimate_context_tokens(items));
                        if let Err(compact_error) =
                            self.compact_context(items, None, ui_events).await
                        {
                            let _ = ui_events
                                .send(AgentEvent::CompactionFailed(compact_error))
                                .await;
                            // Compaction failed: degrade to the hinted
                            // hard-limit trim. It needs a resolved window to
                            // compute a budget; without one there is no safe
                            // trim and the progress gate below refuses the
                            // retry.
                            if let Some(budget) =
                                crate::session::safe_input_capacity(&self.provider_config)
                            {
                                crate::session::trim_conversation_to_budget(items, budget);
                            }
                        }
                        // The gate compares the same calibrated estimate on
                        // both sides, so it stays exact even though a
                        // compaction invalidated the usage anchor's item
                        // index (the anchor is deliberately not consulted
                        // here). Compaction (prefix → summary) and trim
                        // (front removal) both strictly reduce it, and
                        // Ok(0)/removed == 0 leave it unchanged.
                        let after = self
                            .calibrated_estimate(crate::session::estimate_context_tokens(items));
                        if after < before {
                            overflow_retries += 1;
                            // The compacted history matches no server-side
                            // state: the retry is a full replay.
                            self.storage
                                .clear_response_id(&self.session_id)
                                .map_err(|error| error.to_string())?;
                            previous_response_id = None;
                            request_cursor = 0;
                            let _ = ui_events
                                .send(AgentEvent::ProviderRetry {
                                    attempt: overflow_retries,
                                    reason: "context overflow".to_owned(),
                                    delay_ms: 0,
                                })
                                .await;
                            continue;
                        }
                        // No measurable progress (nothing compactable, no
                        // usable window): fail the round with the original
                        // error instead of retrying the same request.
                        self.save_partial(&collector.assistant_text);
                        return Err(error.to_string());
                    }
                    if previous_response_id.is_some()
                        && collector.assistant_text.is_empty()
                        && collector.partials.is_empty()
                        && collector.completed_calls.is_empty()
                    {
                        // Compatible endpoints can expire or reject server-side state.
                        // Replay the canonical local history once instead.
                        self.storage
                            .clear_response_id(&self.session_id)
                            .map_err(|error| error.to_string())?;
                        previous_response_id = None;
                        request_cursor = 0;
                        continue;
                    }
                    // Keep the half-generated answer so the user can review and
                    // resume it instead of losing the stream.
                    self.save_partial(&collector.assistant_text);
                    return Err(error.to_string());
                }
                Err(StreamFailure::Handler(error))
                | Err(StreamFailure::Join(error))
                | Err(StreamFailure::Limit(error)) => {
                    self.save_partial(&collector.assistant_text);
                    return Err(error);
                }
                Err(StreamFailure::EndedWithoutCompletion) => {
                    self.save_partial(&collector.assistant_text);
                    return Err("model stream ended without completion".into());
                }
            }
            collector.finish_partials("tool")?;
            let reasoning_text = collector.reasoning_text.trim();
            if !reasoning_text.is_empty() {
                items.push(ConversationItem::ThinkingSummary {
                    content: reasoning_text.to_owned(),
                });
                self.storage
                    .append_thinking_summary(&self.session_id, reasoning_text)
                    .map_err(|error| error.to_string())?;
            }
            if !collector.assistant_text.is_empty() {
                items.push(ConversationItem::Message {
                    role: Role::Assistant,
                    content: collector.assistant_text.clone(),
                });
                self.storage
                    .append_message(&self.session_id, Role::Assistant, &collector.assistant_text)
                    .map_err(|error| error.to_string())?;
                // The formal assistant message replaces any earlier partial.
                self.storage
                    .clear_partial(&self.session_id)
                    .map_err(|error| error.to_string())?;
            }
            if collector.completed_calls.is_empty() {
                ui_events
                    .send(AgentEvent::Completed {
                        items: items.clone(),
                    })
                    .await
                    .map_err(|_| "UI event receiver closed".to_owned())?;
                return Ok(());
            }

            items.push(ConversationItem::AssistantToolCalls {
                calls: collector.completed_calls.clone(),
            });
            self.storage
                .append_tool_calls(&self.session_id, &collector.completed_calls)
                .map_err(|error| error.to_string())?;
            // When Responses server state is enabled, the response already owns the
            // assistant text and tool calls. Only subsequent tool outputs are new.
            request_cursor = items.len();
            let mut spawn_tasks: Vec<ToolCall> = Vec::new();
            for call in collector.completed_calls {
                let signature = tool_call_signature(&call);
                if executed_tool_calls.contains(&signature) {
                    let result = "Duplicate tool call was not executed. Reuse the previous result or choose a different action.".to_owned();
                    ui_events
                        .send(AgentEvent::ToolFinished {
                            call: call.clone(),
                            result: result.clone(),
                        })
                        .await
                        .map_err(|_| "UI event receiver closed".to_owned())?;
                    items.push(ConversationItem::ToolOutput {
                        call_id: call.id.clone(),
                        output: result.clone(),
                    });
                    self.storage
                        .append_tool_output(&self.session_id, &call.id, &result)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if let Some(role) = &self.child_role {
                    if !child_tool_name_allowed(&call.name, Some(role), &[]) {
                        let result =
                            format!("denied by policy: child role does not allow {}", call.name);
                        self.storage
                            .begin_tool(&self.session_id, &call, "denied")
                            .map_err(|error| error.to_string())?;
                        self.complete_tool(
                            &call,
                            &result,
                            ui_events,
                            items,
                            &mut executed_tool_calls,
                        )
                        .await?;
                        continue;
                    }
                }
                if matches!(call.name.as_str(), "todo_read" | "todo_write") {
                    self.storage
                        .begin_tool(&self.session_id, &call, "allowed")
                        .map_err(|error| error.to_string())?;
                    ui_events
                        .send(AgentEvent::ToolStarted(call.clone()))
                        .await
                        .map_err(|_| "UI event receiver closed".to_owned())?;
                    let (result, updated_tasks) = self.execute_todo_tool(&call);
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                    if let Some(tasks) = updated_tasks {
                        ui_events
                            .send(AgentEvent::TodoUpdated { tasks })
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
                    }
                    continue;
                }
                let decision = self.tools.policy(&call);
                let session_allowed = matches!(decision, PolicyDecision::Allow)
                    && self.tools.is_session_allowed(&call);
                let approved = match decision {
                    PolicyDecision::Allow => true,
                    PolicyDecision::Deny(reason) => {
                        self.storage
                            .begin_tool(&self.session_id, &call, "denied")
                            .map_err(|error| error.to_string())?;
                        let result = format!("denied by policy: {reason}");
                        self.complete_tool(
                            &call,
                            &result,
                            ui_events,
                            items,
                            &mut executed_tool_calls,
                        )
                        .await?;
                        continue;
                    }
                    PolicyDecision::RequireApproval(reason) => {
                        let (reply, answer) = oneshot::channel();
                        ui_events
                            .send(AgentEvent::Approval {
                                call: call.clone(),
                                reason,
                                source_session_id: None,
                                source_title: None,
                                reply,
                            })
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
                        answer
                            .await
                            .map_err(|_| "cancelled: approval channel closed".to_owned())?
                    }
                };
                let decision_name = if session_allowed {
                    "session-allowed"
                } else if approved {
                    "approved"
                } else {
                    "rejected"
                };
                self.storage
                    .begin_tool(&self.session_id, &call, decision_name)
                    .map_err(|error| error.to_string())?;
                if !approved {
                    self.complete_tool(
                        &call,
                        "rejected by user",
                        ui_events,
                        items,
                        &mut executed_tool_calls,
                    )
                    .await?;
                    continue;
                }
                ui_events
                    .send(AgentEvent::ToolStarted(call.clone()))
                    .await
                    .map_err(|_| "UI event receiver closed".to_owned())?;
                if call.name == "agent_spawn" {
                    if depth >= 1 {
                        self.complete_tool(
                            &call,
                            "child agents cannot recursively spawn another child",
                            ui_events,
                            items,
                            &mut executed_tool_calls,
                        )
                        .await?;
                    } else {
                        spawn_tasks.push(call);
                    }
                } else {
                    let _ = self.snapshot_tool_pre(&call);
                    let result = self
                        .tools
                        .execute(&call)
                        .await
                        .unwrap_or_else(|error| error.to_string());
                    let _ = self.snapshot_tool_post(&call);
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                }
            }

            if !spawn_tasks.is_empty() {
                let mut futures = FuturesUnordered::new();
                for call in spawn_tasks {
                    let runner = self.clone();
                    let ui_events = ui_events.clone();
                    futures.push(async move {
                        let result = runner
                            .run_child(&call, &ui_events)
                            .await
                            .unwrap_or_else(|error| error);
                        (call, result)
                    });
                }
                while let Some((call, result)) = futures.next().await {
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                }
            }
        }
    }

    fn execute_todo_tool(&self, call: &ToolCall) -> (String, Option<Vec<TodoTask>>) {
        let result: Result<(String, Option<Vec<TodoTask>>), String> = match call.name.as_str() {
            "todo_read" => self
                .storage
                .list_tasks(&self.session_id)
                .map_err(|error| error.to_string())
                .and_then(|tasks| todo_tool_response(&tasks).map(|output| (output, None))),
            "todo_write" => self
                .execute_todo_write(&call.arguments)
                .map(|(output, tasks)| (output, Some(tasks))),
            _ => Err(format!("unknown todo tool: {}", call.name)),
        };
        match result {
            Ok((output, updated_tasks)) => (output, updated_tasks),
            Err(error) => (format!("todo tool error: {error}"), None),
        }
    }

    fn execute_todo_write(&self, arguments: &Value) -> Result<(String, Vec<TodoTask>), String> {
        let arguments: TodoWriteArguments =
            serde_json::from_value(arguments.clone()).map_err(|error| error.to_string())?;
        let current = self
            .storage
            .list_tasks(&self.session_id)
            .map_err(|error| error.to_string())?;
        let existing: std::collections::HashMap<String, TodoTask> = current
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        let now = chrono::Utc::now().to_rfc3339();
        let tasks = arguments
            .tasks
            .into_iter()
            .map(|input| {
                let title = input.title.trim().to_owned();
                if let Some(existing) = input.id.as_deref().and_then(|id| existing.get(id)) {
                    TodoTask {
                        id: existing.id.clone(),
                        title,
                        status: input.status,
                        created_at: existing.created_at.clone(),
                        updated_at: now.clone(),
                    }
                } else {
                    TodoTask {
                        id: uuid::Uuid::new_v4().to_string(),
                        title,
                        status: input.status,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    }
                }
            })
            .collect::<Vec<_>>();
        self.storage
            .replace_tasks(&self.session_id, &tasks)
            .map_err(|error| error.to_string())?;
        Ok((todo_tool_response(&tasks)?, tasks))
    }

    /// Captures the pre-execution image of the file a mutating tool is about to
    /// write, so undo/redo can roll it back. Returns `true` when a snapshot was
    /// recorded for the current head turn.
    fn snapshot_tool_pre(&self, call: &ToolCall) -> Result<bool, String> {
        let Some(path) = snapshot_target_path(&call.name, &call.arguments) else {
            return Ok(false);
        };
        let Some(turn_id) = self
            .storage
            .head_turn_id(&self.session_id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        let path_buf = self
            .tools
            .workspace()
            .resolve_existing(&path)
            .map_err(|error| error.to_string())?;
        let (pre_image, existed) = match std::fs::read(&path_buf) {
            Ok(bytes) => (Some(bytes), true),
            Err(_) => (None, false),
        };
        let (max_file, max_session) = self.tools.checkpoint_limits();
        self.storage
            .snapshot_file(
                &self.session_id,
                &turn_id,
                &call.id,
                &path,
                pre_image.as_deref(),
                existed,
                max_file,
                max_session,
            )
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// Backfills the post-execution image for a snapshotted tool call.
    fn snapshot_tool_post(&self, call: &ToolCall) -> Result<(), String> {
        let Some(path) = snapshot_target_path(&call.name, &call.arguments) else {
            return Ok(());
        };
        let path_buf = self
            .tools
            .workspace()
            .resolve_existing(&path)
            .map_err(|error| error.to_string())?;
        let post_image = std::fs::read(&path_buf).ok();
        self.storage
            .save_post_image(&call.id, post_image.as_deref())
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Finishes an already-started tool call: persists the result, emits the
    /// `ToolFinished` event, and appends the tool output to the conversation.
    async fn complete_tool(
        &self,
        call: &ToolCall,
        result: &str,
        ui_events: &mpsc::Sender<AgentEvent>,
        items: &mut Vec<ConversationItem>,
        executed_tool_calls: &mut HashSet<String>,
    ) -> Result<(), String> {
        self.storage
            .finish_tool(&call.id, result)
            .map_err(|error| error.to_string())?;
        ui_events
            .send(AgentEvent::ToolFinished {
                call: call.clone(),
                result: result.to_owned(),
            })
            .await
            .map_err(|_| "UI event receiver closed".to_owned())?;
        items.push(ConversationItem::ToolOutput {
            call_id: call.id.clone(),
            output: result.to_owned(),
        });
        self.storage
            .append_tool_output(&self.session_id, &call.id, result)
            .map_err(|error| error.to_string())?;
        executed_tool_calls.insert(tool_call_signature(call));
        Ok(())
    }

    /// Executes one child-agent tool call, honouring the shared policy. Write
    /// tools request approval (serialized through `approval_lock` so concurrent
    /// children cannot interleave approval prompts).
    async fn execute_child_tool(
        &self,
        context: &ChildToolContext<'_>,
        call: &ToolCall,
        active_budget: &mut Duration,
    ) -> Option<String> {
        match self.tools.policy(call) {
            PolicyDecision::Allow => {
                let decision = if self.tools.is_session_allowed(call) {
                    "session-allowed"
                } else {
                    "allowed"
                };
                let _ = self.storage.begin_tool(context.child_id, call, decision);
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::RunningTool,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let result = self
                    .execute_child_tool_with_budget(call, active_budget)
                    .await?;
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
            PolicyDecision::Deny(reason) => {
                let result = format!("denied by policy: {reason}");
                let _ = self.storage.begin_tool(context.child_id, call, "denied");
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
            PolicyDecision::RequireApproval(reason) => {
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::WaitingApprovalSlot,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let _guard = self.approval_lock.lock().await;
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::WaitingApproval,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let (reply, answer) = oneshot::channel();
                if context
                    .ui_events
                    .send(AgentEvent::Approval {
                        call: call.clone(),
                        reason,
                        source_session_id: Some(context.child_id.to_owned()),
                        source_title: Some(context.child_title.to_owned()),
                        reply,
                    })
                    .await
                    .is_err()
                {
                    return Some("approval channel closed".into());
                }
                let approved = match answer.await {
                    Ok(approved) => approved,
                    Err(_) => return Some("approval cancelled".into()),
                };
                let _ = self.storage.begin_tool(
                    context.child_id,
                    call,
                    if approved { "approved" } else { "rejected" },
                );
                let result = if approved {
                    emit_child_progress(
                        context.ui_events,
                        context.child_id,
                        child_progress(
                            ChildSessionStatus::RunningTool,
                            context.turn,
                            context.max_turns,
                            Some(call.name.clone()),
                        ),
                    )
                    .await;
                    self.execute_child_tool_with_budget(call, active_budget)
                        .await?
                } else {
                    "rejected by user".to_owned()
                };
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
        }
    }

    async fn execute_child_tool_with_budget(
        &self,
        call: &ToolCall,
        active_budget: &mut Duration,
    ) -> Option<String> {
        if active_budget.is_zero() {
            return None;
        }
        let started = Instant::now();
        let result = timeout(*active_budget, self.tools.execute(call)).await;
        *active_budget = active_budget.saturating_sub(started.elapsed());
        match result {
            Ok(result) => Some(result.unwrap_or_else(|error| error.to_string())),
            Err(_) => None,
        }
    }

    async fn run_child(
        &self,
        call: &ToolCall,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> Result<String, String> {
        let arguments: ChildArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| format!("invalid child agent arguments: {error}"))?;
        if arguments.prompt.trim().is_empty() {
            return Err("child agent prompt must not be empty".into());
        }
        let configured_agent = match arguments.agent.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(name) => match self.configured_agent(name) {
                Some(agent) => Some(agent),
                None => return Err(format!("unknown configured agent \"{name}\"")),
            },
        };
        let role = arguments
            .role
            .clone()
            .or_else(|| configured_agent.as_ref().map(|agent| agent.name.clone()));
        let allowed_tools = configured_agent
            .as_ref()
            .map(|agent| agent.allowed_tools.clone())
            .unwrap_or_default();
        let role = match role {
            Some(role) => Some(role),
            None if allowed_tools.iter().any(|tool| {
                tool == "file_write"
                    || tool == "file_edit"
                    || tool == "file_mkdir"
                    || tool == "file_copy"
                    || tool == "file_move"
            }) =>
            {
                Some("implement".to_owned())
            }
            None => None,
        };
        let max_turns = arguments
            .max_turns
            .or_else(|| configured_agent.as_ref().map(|agent| agent.max_turns))
            .unwrap_or(0);

        let requested_preset = match arguments.provider.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(name) => Some(
                ProviderPreset::parse(name)
                    .ok_or_else(|| format!("unknown provider preset \"{name}\" for agent_spawn"))?,
            ),
        };
        let (provider, provider_config) =
            self.resolve_child_provider(requested_preset, arguments.model.as_deref())?;

        // Create a nested session so the child's work is inspectable from the
        // session panel, using its own provider/model when one was requested.
        let workspace = self
            .storage
            .session_workspace(&self.session_id)
            .map_err(|error| error.to_string())?;
        let title = child_title(&arguments, configured_agent.as_ref(), role.as_deref());
        let child_mode = if is_implement_role(role.as_deref()) {
            "build"
        } else {
            "explore"
        };
        let child_role = role.as_deref().unwrap_or("");
        let child_id = self
            .storage
            .create_child_session(
                Path::new(&workspace),
                &self.session_id,
                provider_config.preset.key_id(),
                &provider_config.model,
                &title,
                child_mode,
                child_role,
            )
            .map_err(|error| error.to_string())?;
        self.storage
            .append_message(&child_id, Role::User, &arguments.prompt)
            .map_err(|error| error.to_string())?;
        let mut cancellation_guard = ChildCancellationGuard {
            ui_events: ui_events.clone(),
            session_id: child_id.clone(),
            max_turns,
            finished: false,
        };
        // Let the UI refresh the session tree as soon as the child exists,
        // rather than waiting for the whole turn to complete.
        let _ = ui_events.send(AgentEvent::SessionsChanged).await;
        emit_child_progress(
            ui_events,
            &child_id,
            child_progress(ChildSessionStatus::Queued, 0, max_turns, None),
        )
        .await;
        let _child_slot = self
            .child_slots
            .acquire()
            .await
            .map_err(|_| "child concurrency limiter closed".to_owned())?;

        let tools = self.child_tool_definitions(role.as_deref(), &allowed_tools);
        let mut child_system = prompt::child_system_prompt(role.as_deref(), &allowed_tools);
        if let Some(agent) = &configured_agent {
            if !agent.system_prompt.trim().is_empty() {
                child_system.push_str("\n\nADDITIONAL AGENT INSTRUCTIONS\n");
                child_system.push_str(agent.system_prompt.trim());
            }
        }

        // Multi-turn loop: execute the child's role-filtered tools, keep its
        // context bounded, and return only the final deliverable.
        let thinking_profile_kind =
            thinking_profile(provider_config.preset, &provider_config.model).kind;
        let native_web_search = provider_config.preset == ProviderPreset::DeepSeek
            && provider_config.kind == ProviderKind::Responses
            && provider_config.native_web_search != NativeWebSearch::Disabled;
        let child_max_output_bytes = self.cluster.child_max_output_bytes;

        let mut items = vec![ConversationItem::Message {
            role: Role::User,
            content: arguments.prompt.clone(),
        }];
        let mut final_answer = String::new();
        let mut tool_call_count = 0usize;
        let mut remaining_turns = max_turns;
        let mut completed_turns = 0usize;
        let mut active_budget =
            Duration::from_secs(self.cluster.child_active_timeout_seconds.max(1));
        let mut failure: Option<String> = None;
        let mut status = ChildSessionStatus::Completed;
        'turns: loop {
            if max_turns > 0 && remaining_turns == 0 {
                status = ChildSessionStatus::TurnLimit;
                let trail = summarize_child_trail(&items, 3, 512);
                append_text_bounded(
                    &mut final_answer,
                    &format!("\n[child agent reached its turn limit]{trail}"),
                    child_max_output_bytes,
                );
                break;
            }
            if max_turns > 0 {
                remaining_turns -= 1;
            }
            completed_turns = completed_turns.saturating_add(1);
            let turn = completed_turns;

            let mut request_items = vec![ConversationItem::Message {
                role: Role::System,
                content: child_system.clone(),
            }];
            request_items.extend(items.clone());
            let request = ModelRequest {
                kind: provider_config.kind,
                model: provider_config.model.clone(),
                items: request_items,
                tools: tools.clone(),
                previous_response_id: None,
                native_web_search,
                thinking_mode: thinking_mode_for(&provider_config),
                thinking_level: provider_config.thinking_level,
                thinking_budget_tokens: provider_config.thinking_budget_tokens,
                thinking_profile_kind,
                max_output_tokens: provider_config.max_output_tokens,
            };
            let mut collector = StreamCollector::new(Some(
                child_max_output_bytes.min(self.memory.max_response_bytes),
            ))
            .with_memory_limits(self.memory);
            emit_child_progress(
                ui_events,
                &child_id,
                child_progress(ChildSessionStatus::WaitingModel, turn, max_turns, None),
            )
            .await;
            if active_budget.is_zero() {
                status = ChildSessionStatus::TimedOut;
                break;
            }
            let stream_started = Instant::now();
            let mut streaming_reported = false;
            let stream_result = timeout(
                active_budget,
                stream_once(
                    &provider,
                    request,
                    &mut collector,
                    512,
                    self.memory.max_agent_event_bytes,
                    ui_events,
                    |_| {
                        if streaming_reported {
                            Ok(Forwarded::Ignore)
                        } else {
                            streaming_reported = true;
                            Ok(Forwarded::SendIgnore(AgentEvent::ChildSessionProgress {
                                session_id: child_id.clone(),
                                progress: child_progress(
                                    ChildSessionStatus::Streaming,
                                    turn,
                                    max_turns,
                                    None,
                                ),
                            }))
                        }
                    },
                ),
            )
            .await;
            active_budget = active_budget.saturating_sub(stream_started.elapsed());
            let stream_result = match stream_result {
                Ok(result) => result,
                Err(_) => {
                    status = ChildSessionStatus::TimedOut;
                    if !collector.assistant_text.is_empty() {
                        append_text_bounded(
                            &mut final_answer,
                            &collector.assistant_text,
                            child_max_output_bytes,
                        );
                    }
                    break;
                }
            };
            match stream_result {
                Ok(()) => {}
                Err(StreamFailure::Provider(error)) => {
                    status = ChildSessionStatus::Failed;
                    failure = Some(error.to_string());
                    break;
                }
                Err(StreamFailure::Handler(error))
                | Err(StreamFailure::Join(error))
                | Err(StreamFailure::Limit(error)) => {
                    return Err(error);
                }
                Err(StreamFailure::EndedWithoutCompletion) => {
                    status = ChildSessionStatus::Failed;
                    failure = Some("child stream ended without completion".into());
                    break;
                }
            }
            collector.finish_partials("child tool")?;
            if collector.completed_calls.is_empty() {
                final_answer = collector.assistant_text.clone();
                break;
            }

            if !collector.assistant_text.is_empty() {
                items.push(ConversationItem::Message {
                    role: Role::Assistant,
                    content: std::mem::take(&mut collector.assistant_text),
                });
            }
            items.push(ConversationItem::AssistantToolCalls {
                calls: collector.completed_calls.clone(),
            });
            self.storage
                .append_tool_calls(&child_id, &collector.completed_calls)
                .map_err(|error| error.to_string())?;
            for tool_call in std::mem::take(&mut collector.completed_calls) {
                tool_call_count += 1;
                let context = ChildToolContext {
                    child_id: &child_id,
                    child_title: &title,
                    ui_events,
                    turn,
                    max_turns,
                };
                let Some(result) = self
                    .execute_child_tool(&context, &tool_call, &mut active_budget)
                    .await
                else {
                    let result = "child active execution budget exceeded".to_owned();
                    let _ = self.storage.finish_tool(&tool_call.id, &result);
                    let _ = self
                        .storage
                        .append_tool_output(&child_id, &tool_call.id, &result);
                    items.push(ConversationItem::ToolOutput {
                        call_id: tool_call.id,
                        output: result,
                    });
                    status = ChildSessionStatus::TimedOut;
                    break 'turns;
                };
                let result =
                    truncate_utf8_bounded(&result, self.cluster.child_max_tool_output_bytes);
                self.storage
                    .append_tool_output(&child_id, &tool_call.id, &result)
                    .map_err(|error| error.to_string())?;
                items.push(ConversationItem::ToolOutput {
                    call_id: tool_call.id.clone(),
                    output: result,
                });
                trim_conversation_bounded(
                    &mut items,
                    self.cluster.child_max_context_items,
                    self.cluster.child_max_context_bytes,
                );
            }
        }

        if let Some(error) = failure {
            append_text_bounded(
                &mut final_answer,
                &format!("\n[child failed: {error}]"),
                child_max_output_bytes,
            );
        }
        if status == ChildSessionStatus::TimedOut {
            let trail = summarize_child_trail(&items, 3, 512);
            append_text_bounded(
                &mut final_answer,
                &format!("\n[child agent exceeded its active execution budget]{trail}"),
                child_max_output_bytes,
            );
        }
        if final_answer.trim().is_empty() {
            if tool_call_count > 0 {
                final_answer.push_str(&format!(
                    "[child agent issued {tool_call_count} tool call(s) but returned no text]"
                ));
            } else {
                final_answer.push_str("[child agent returned no text]");
            }
        }
        let final_answer = truncate_utf8_bounded(&final_answer, child_max_output_bytes);
        self.storage
            .append_message(&child_id, Role::Assistant, &final_answer)
            .map_err(|error| error.to_string())?;
        emit_child_progress(
            ui_events,
            &child_id,
            child_progress(status, completed_turns, max_turns, None),
        )
        .await;
        cancellation_guard.finish();
        Ok(serde_json::to_string(&json!({
            "session_id": child_id,
            "title": title,
            "status": status.wire_name(),
            "output": final_answer,
        }))
        .unwrap_or_else(|_| final_answer.clone()))
    }
}

fn tool_call_signature(call: &ToolCall) -> String {
    format!("{}:{}", call.name, canonical_json(&call.arguments))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut fields = object.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| *key);
            let fields = fields
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON object keys are serializable"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => serde_json::to_string(value).expect("JSON values are serializable"),
    }
}

fn qwen_thinking_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("qwen-plus")
        || model.contains("qwen-max")
        || model.contains("qwen-turbo")
        || model.contains("qwen3")
        || model.contains("qwq")
        || model.contains("qwen-flash")
}

fn volcano_thinking_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("thinking") || model.contains("reason") || model.contains("seed")
}

fn thinking_mode_for(config: &ProviderConfig) -> ThinkingMode {
    let capability = match config.thinking {
        ThinkingCapability::Auto => None,
        ThinkingCapability::OpenAi => Some(if config.kind == ProviderKind::Responses {
            ThinkingMode::OpenAiResponsesSummary
        } else {
            ThinkingMode::CompatibleAuto
        }),
        ThinkingCapability::DeepSeek => Some(match config.kind {
            ProviderKind::Responses => ThinkingMode::DeepSeekResponses,
            ProviderKind::ChatCompletions => ThinkingMode::DeepSeekChat,
        }),
        ThinkingCapability::Qwen => Some(if config.kind == ProviderKind::ChatCompletions {
            ThinkingMode::QwenChat
        } else {
            ThinkingMode::QwenResponses
        }),
        ThinkingCapability::Volcano => Some(if config.kind == ProviderKind::ChatCompletions {
            ThinkingMode::VolcanoChat
        } else {
            ThinkingMode::CompatibleAuto
        }),
        ThinkingCapability::Compatible => Some(ThinkingMode::CompatibleAuto),
        ThinkingCapability::Disabled => Some(ThinkingMode::Disabled),
    };
    if let Some(mode) = capability {
        return mode;
    }
    match (config.preset, config.kind) {
        (ProviderPreset::OpenAi, ProviderKind::Responses) => ThinkingMode::OpenAiResponsesSummary,
        (ProviderPreset::DeepSeek, ProviderKind::Responses) => ThinkingMode::DeepSeekResponses,
        (ProviderPreset::DeepSeek, ProviderKind::ChatCompletions) => ThinkingMode::DeepSeekChat,
        (ProviderPreset::Qwen, ProviderKind::Responses) if qwen_thinking_model(&config.model) => {
            ThinkingMode::QwenResponses
        }
        (ProviderPreset::Qwen, ProviderKind::ChatCompletions)
            if qwen_thinking_model(&config.model) =>
        {
            ThinkingMode::QwenChat
        }
        (ProviderPreset::Volcano, ProviderKind::ChatCompletions)
            if volcano_thinking_model(&config.model) =>
        {
            ThinkingMode::VolcanoChat
        }
        (ProviderPreset::Custom, ProviderKind::ChatCompletions) => ThinkingMode::CompatibleAuto,
        // A custom Responses endpoint (OpenAI-compatible gateways, Volcano Ark
        // coding, ...) streams reasoning with any of the known delta shapes;
        // `Disabled` here would silently drop every incremental reasoning
        // event and leave only the completed item's summary replay.
        (ProviderPreset::Custom, ProviderKind::Responses) => ThinkingMode::CompatibleAuto,
        _ => ThinkingMode::Disabled,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildArgs {
    prompt: String,
    max_turns: Option<usize>,
    role: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    title: Option<String>,
}

#[cfg(test)]
mod tests;
