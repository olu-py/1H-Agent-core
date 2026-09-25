use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

use crate::{
    config::{
        AgentConfig, NativeWebSearch, ProviderConfig, ProviderKind, ProviderPreset,
        thinking_profile,
    },
    prompt,
    provider::{ConversationItem, ModelRequest, OpenAiClient, Role, ToolCall, ToolDefinition},
    secrets,
    security::PolicyDecision,
    session::trim_conversation_bounded,
};

use super::{
    AgentEvent, AgentRunner, ChildSessionProgress, ChildSessionStatus, Forwarded, StreamCollector,
    StreamFailure, append_text_bounded, stream_once, thinking_mode_for,
};

/// Tools a child agent may ever receive. This is intentionally smaller than the
/// role-based filter: no terminal, shell, git mutation, browser, MCP, spawn, or
/// delete tools are ever delegated to a child.
pub(super) fn child_tool_name_allowed(
    tool: &str,
    can_write: bool,
    allowed_tools: &[String],
) -> bool {
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

    let role_allows = READ_TOOLS.contains(&tool) || (can_write && WRITE_TOOLS.contains(&tool));
    role_allows && (allowed_tools.is_empty() || allowed_tools.iter().any(|name| name == tool))
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
pub(super) fn infer_child_provider(model: &str, current: ProviderPreset) -> ProviderPreset {
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
pub(super) fn validate_child_model(provider_config: &ProviderConfig) -> Result<(), String> {
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

pub(super) fn child_title(
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
pub(super) fn is_implement_role(role: Option<&str>) -> bool {
    role.is_some_and(|role| {
        matches!(
            role.trim().to_ascii_lowercase().as_str(),
            "implement" | "implementation" | "code" | "coder" | "build" | "实施" | "编码"
        )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildCapability {
    ReadOnly,
    Implement,
}

impl ChildCapability {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read_only" | "readonly" => Some(Self::ReadOnly),
            "implementation" | "implement" => Some(Self::Implement),
            _ => None,
        }
    }

    fn can_write(self) -> bool {
        self == Self::Implement
    }
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

pub(super) struct ChildCancellationGuard {
    pub(super) ui_events: mpsc::Sender<AgentEvent>,
    pub(super) storage: crate::storage::Storage,
    pub(super) partial_output: Arc<std::sync::Mutex<String>>,
    pub(super) session_id: String,
    pub(super) max_turns: usize,
    pub(super) finished: bool,
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
            let partial = self
                .partial_output
                .lock()
                .map(|output| output.clone())
                .unwrap_or_default();
            if !partial.trim().is_empty() {
                let _ = self
                    .storage
                    .append_message(&self.session_id, Role::Assistant, &partial);
            }
            let _ = self.storage.set_child_status(&self.session_id, "cancelled");
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

impl AgentRunner {
    fn child_tool_definitions(
        &self,
        can_write: bool,
        allowed_tools: &[String],
    ) -> Vec<ToolDefinition> {
        self.tools
            .definitions()
            .into_iter()
            .filter(|tool| child_tool_name_allowed(&tool.name, can_write, allowed_tools))
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

    pub(super) async fn execute_child_tool_with_budget(
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

    pub(super) async fn run_child(
        &self,
        call: &ToolCall,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> Result<String, String> {
        match self.run_child_inner(call, ui_events).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => Ok(child_outcome(
                None,
                "子 Agent".into(),
                ChildSessionStatus::Failed,
                String::new(),
                Some(error),
            )),
        }
    }

    async fn run_child_inner(
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
        let capability = match arguments.capability.as_deref() {
            Some(value) => ChildCapability::parse(value).unwrap_or(ChildCapability::ReadOnly),
            None if is_implement_role(role.as_deref()) => ChildCapability::Implement,
            None => ChildCapability::ReadOnly,
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
        let child_mode = if capability.can_write() {
            "build"
        } else {
            "explore"
        };
        let child_role = if capability.can_write() {
            "implement"
        } else {
            "read_only"
        };
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
        let partial_output = Arc::new(std::sync::Mutex::new(String::new()));
        let mut cancellation_guard = ChildCancellationGuard {
            ui_events: ui_events.clone(),
            storage: self.storage.clone(),
            partial_output: partial_output.clone(),
            session_id: child_id.clone(),
            max_turns,
            finished: false,
        };
        let mut final_answer = String::new();
        let child_result: Result<String, String> = async {
            self.storage
                .set_child_allowed_tools(&child_id, &allowed_tools)
                .map_err(|error| error.to_string())?;
            self.storage
                .append_message(&child_id, Role::User, &arguments.prompt)
                .map_err(|error| error.to_string())?;
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

            let tools = self.child_tool_definitions(capability.can_write(), &allowed_tools);
            let mut child_system = prompt::child_system_prompt(
                role.as_deref(),
                capability.can_write(),
                &allowed_tools,
            );
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
            let mut tool_call_count = 0usize;
            let mut remaining_turns = max_turns;
            let mut completed_turns = 0usize;
            let mut active_budget =
                Duration::from_secs(self.cluster.child_active_timeout_seconds.max(1));
            let mut failure: Option<String> = None;
            let mut outcome_error: Option<String> = None;
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
                .with_memory_limits(self.memory)
                .with_partial_output_sink(partial_output.clone());
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
                outcome_error = Some(error.clone());
                append_text_bounded(
                    &mut final_answer,
                    &format!("\n[child failed: {error}]"),
                    child_max_output_bytes,
                );
            }
            if status == ChildSessionStatus::TimedOut {
                outcome_error
                    .get_or_insert_with(|| "child active execution budget exceeded".to_owned());
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
            self.storage
                .set_child_status(&child_id, status.wire_name())
                .map_err(|error| error.to_string())?;
            emit_child_progress(
                ui_events,
                &child_id,
                child_progress(status, completed_turns, max_turns, None),
            )
            .await;
            Ok(serde_json::to_string(&json!({
                "session_id": child_id,
                "title": title,
                "status": status.wire_name(),
                "output": final_answer,
                "error": outcome_error,
            }))
            .unwrap_or_else(|_| final_answer.clone()))
        }
        .await;
        match child_result {
            Ok(outcome) => {
                cancellation_guard.finish();
                Ok(outcome)
            }
            Err(error) => {
                if let Ok(partial) = partial_output.lock()
                    && !partial.trim().is_empty()
                {
                    append_text_bounded(
                        &mut final_answer,
                        &partial,
                        self.cluster.child_max_output_bytes,
                    );
                }
                let message = format!("[child failed: {error}]");
                append_text_bounded(
                    &mut final_answer,
                    &message,
                    self.cluster.child_max_output_bytes,
                );
                let _ = self
                    .storage
                    .append_message(&child_id, Role::Assistant, &final_answer);
                let _ = self.storage.set_child_status(&child_id, "failed");
                emit_child_progress(
                    ui_events,
                    &child_id,
                    child_progress(ChildSessionStatus::Failed, 0, max_turns, None),
                )
                .await;
                cancellation_guard.finish();
                Ok(child_outcome(
                    Some(child_id),
                    title,
                    ChildSessionStatus::Failed,
                    final_answer,
                    Some(error),
                ))
            }
        }
    }
}

fn child_outcome(
    session_id: Option<String>,
    title: String,
    status: ChildSessionStatus,
    output: String,
    error: Option<String>,
) -> String {
    serde_json::to_string(&json!({
        "session_id": session_id,
        "title": title,
        "status": status.wire_name(),
        "output": output,
        "error": error,
    }))
    .unwrap_or_else(|_| "{\"session_id\":null,\"title\":\"子 Agent\",\"status\":\"failed\",\"output\":\"\",\"error\":\"failed to serialize child outcome\"}".into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChildArgs {
    pub(super) prompt: String,
    pub(super) max_turns: Option<usize>,
    pub(super) role: Option<String>,
    pub(super) capability: Option<String>,
    pub(super) model: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) title: Option<String>,
}
