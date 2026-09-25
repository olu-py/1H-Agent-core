use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot};

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
    security::PolicyDecision,
    session::UsageAnchor,
    storage::Storage,
    tools::SharedToolRegistry,
};

mod child;
mod events;
mod runner;
mod tool_loop;

use child::{child_tool_name_allowed, is_implement_role};
pub use events::{AgentEvent, ChildSessionProgress, ChildSessionStatus};

#[cfg(test)]
use child::{
    ChildArgs, ChildCancellationGuard, child_title, infer_child_provider, validate_child_model,
};

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
    child_allowed_tools: Vec<String>,
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
    partial_output_sink: Option<Arc<std::sync::Mutex<String>>>,
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
            partial_output_sink: None,
        }
    }

    fn with_memory_limits(mut self, memory: MemoryConfig) -> Self {
        self.max_tool_call_bytes = memory.max_tool_call_bytes;
        self.max_tool_call_total_bytes = memory.max_tool_call_total_bytes;
        self.max_tool_calls = memory.max_tool_calls;
        self
    }

    fn with_partial_output_sink(mut self, sink: Arc<std::sync::Mutex<String>>) -> Self {
        self.partial_output_sink = Some(sink);
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
                        if let Some(sink) = &self.partial_output_sink {
                            if let Ok(mut output) = sink.lock() {
                                *output = self.assistant_text.clone();
                            }
                        }
                        return Err(format!(
                            "model response exceeded the {} byte limit",
                            max_bytes
                        ));
                    }
                    append_text_bounded(&mut self.assistant_text, &delta, max_bytes);
                } else {
                    self.assistant_text.push_str(&delta);
                }
                if let Some(sink) = &self.partial_output_sink {
                    if let Ok(mut output) = sink.lock() {
                        *output = self.assistant_text.clone();
                    }
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

#[cfg(test)]
mod tests;
