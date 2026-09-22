use std::sync::Arc;

use tempfile::TempDir;

use super::*;
use crate::{config::RuntimeConfig, security::Workspace, tools::ToolRegistry};

#[test]
fn child_status_has_stable_wire_names_and_localized_labels() {
    let terminal = [
        (ChildSessionStatus::Completed, "completed", "完成"),
        (ChildSessionStatus::Failed, "failed", "失败"),
        (ChildSessionStatus::TurnLimit, "turn_limit", "达到轮次上限"),
        (ChildSessionStatus::TimedOut, "timed_out", "执行超时"),
        (ChildSessionStatus::Cancelled, "cancelled", "已取消"),
    ];
    for (status, wire_name, label) in terminal {
        assert!(status.is_terminal());
        assert_eq!(status.wire_name(), wire_name);
        assert_eq!(status.label(), label);
    }
    assert_eq!(ChildSessionStatus::Queued.wire_name(), "running");
    assert_eq!(ChildSessionStatus::WaitingApproval.label(), "等待审批");
}

#[test]
fn enables_thinking_only_for_known_qwen_thinking_families() {
    assert!(qwen_thinking_model("qwen3.8-max"));
    assert!(qwen_thinking_model("QWQ-32B"));
    assert!(qwen_thinking_model("qwen-plus"));
    assert!(qwen_thinking_model("qwen-max"));
    assert!(qwen_thinking_model("qwen-turbo"));
    assert!(!qwen_thinking_model("qwen2.5-coder"));
    assert!(!qwen_thinking_model("custom-model"));
}

#[test]
fn selects_provider_specific_thinking_modes() {
    let mut config = ProviderPreset::OpenAi.defaults();
    assert_eq!(
        thinking_mode_for(&config),
        ThinkingMode::OpenAiResponsesSummary
    );

    config = ProviderPreset::DeepSeek.defaults();
    assert_eq!(thinking_mode_for(&config), ThinkingMode::DeepSeekResponses);
    config.kind = ProviderKind::ChatCompletions;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::DeepSeekChat);

    config = ProviderPreset::Qwen.defaults();
    for model in [
        "qwen-plus",
        "qwen-max",
        "qwen-turbo",
        "qwen3-max",
        "qwq-32b",
    ] {
        config.model = model.into();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenChat);
    }
    config.model = "unknown-qwen-model".into();
    assert_eq!(thinking_mode_for(&config), ThinkingMode::Disabled);
    config.model = "qwen3.8-max".into();
    config.kind = ProviderKind::Responses;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenResponses);

    config = ProviderPreset::Volcano.defaults();
    assert_eq!(thinking_mode_for(&config), ThinkingMode::VolcanoChat);

    config = ProviderPreset::Custom.defaults();
    assert_eq!(thinking_mode_for(&config), ThinkingMode::CompatibleAuto);
    // A custom Responses endpoint must keep compatible reasoning parsing
    // too; `Disabled` here dropped every incremental reasoning event and
    // left only the completed item's summary replay.
    config.kind = ProviderKind::Responses;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::CompatibleAuto);
    config.kind = ProviderKind::ChatCompletions;
    config.thinking = ThinkingCapability::Qwen;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenChat);
    config.kind = ProviderKind::Responses;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenResponses);
    config.thinking = ThinkingCapability::OpenAi;
    assert_eq!(
        thinking_mode_for(&config),
        ThinkingMode::OpenAiResponsesSummary
    );
    config.thinking = ThinkingCapability::Disabled;
    assert_eq!(thinking_mode_for(&config), ThinkingMode::Disabled);
}

#[test]
fn tool_signatures_normalize_object_keys_but_preserve_values() {
    let first = ToolCall {
        id: "call-1".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"src/lib.rs","options":{"end":20,"start":1}}),
    };
    let reordered = ToolCall {
        id: "call-2".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"options":{"start":1,"end":20},"path":"src/lib.rs"}),
    };
    let different = ToolCall {
        id: "call-3".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"src/lib.rs","options":{"start":2,"end":20}}),
    };

    assert_eq!(tool_call_signature(&first), tool_call_signature(&reordered));
    assert_ne!(tool_call_signature(&first), tool_call_signature(&different));
}

#[test]
fn incremental_cursor_keeps_latest_user_message_and_following_context() {
    let items = vec![
        ConversationItem::Message {
            role: Role::User,
            content: "old".into(),
        },
        ConversationItem::Message {
            role: Role::Assistant,
            content: "answer".into(),
        },
        ConversationItem::Message {
            role: Role::User,
            content: "new".into(),
        },
        ConversationItem::Context {
            label: "file".into(),
            content: "contents".into(),
        },
    ];
    assert_eq!(incremental_request_cursor(&items), 2);
    assert_eq!(items[incremental_request_cursor(&items)..].len(), 2);
}

#[test]
fn stateless_replay_keeps_only_complete_ordered_tool_pairs() {
    let complete = ToolCall {
        id: "complete".into(),
        name: "agent_spawn".into(),
        arguments: json!({}),
    };
    let unanswered = ToolCall {
        id: "unanswered".into(),
        name: "agent_spawn".into(),
        arguments: json!({}),
    };
    let items = vec![
        ConversationItem::ToolOutput {
            call_id: "orphan".into(),
            output: "bad".into(),
        },
        ConversationItem::AssistantToolCalls {
            calls: vec![complete.clone(), unanswered],
        },
        ConversationItem::ToolOutput {
            call_id: complete.id.clone(),
            output: "ok".into(),
        },
        ConversationItem::Message {
            role: Role::User,
            content: "continue".into(),
        },
    ];

    let replay = replay_safe_items(&items);
    assert!(matches!(
        &replay[0],
        ConversationItem::AssistantToolCalls { calls }
            if calls.len() == 1 && calls[0].id == "complete"
    ));
    assert!(matches!(
        &replay[1],
        ConversationItem::ToolOutput { call_id, .. } if call_id == "complete"
    ));
    assert_eq!(replay.len(), 3);
}

#[tokio::test]
async fn main_agent_completes_after_one_hundred_tool_rounds() {
    let mut responses = (0..100)
        .map(|round| {
            vec![
                ModelEvent::ToolCallComplete(ToolCall {
                    id: format!("call-{round}"),
                    name: "file_read".into(),
                    arguments: serde_json::json!({"path":format!("missing-{round}")}),
                }),
                ModelEvent::Done,
            ]
        })
        .collect::<Vec<_>>();
    responses.push(vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done]);

    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "run tools")
        .unwrap();
    let mut provider_config = ProviderPreset::Custom.defaults();
    provider_config.model = "fixture".into();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        OpenAiClient::scripted(responses).unwrap(),
        provider_config,
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "run tools".into(),
                }],
                events,
            )
            .await;
    });

    let mut completed = false;
    let mut failed = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Completed { .. } => completed = true,
            AgentEvent::Failed(error) => failed = Some(error),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(completed);
    assert!(failed.is_none(), "unexpected failure: {failed:?}");
}

#[tokio::test]
async fn provider_retry_event_reaches_the_ui_channel() {
    use crate::provider::OpenAiClient as ScriptedOpenAi;
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "retry")
        .unwrap();
    let mut provider_config = ProviderPreset::Custom.defaults();
    provider_config.model = "fixture".into();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = ScriptedOpenAi::scripted_with_failures(
        vec![vec![ModelEvent::TextDelta("ok".into()), ModelEvent::Done]],
        vec![crate::provider::ProviderError::Status {
            status: 429,
            message: "rate limited".into(),
            retry_after_ms: Some(1),
        }],
    )
    .unwrap();
    let runner = AgentRunner::new(provider, provider_config, tools, storage, session_id);
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "retry".into(),
                }],
                events,
            )
            .await;
    });

    let mut saw_retry = false;
    let mut completed = false;
    while let Some(event) = receiver.recv().await {
        if matches!(
            &event,
            AgentEvent::ProviderRetry { attempt: 1, reason, .. } if reason.contains("429")
        ) {
            saw_retry = true;
        }
        if matches!(event, AgentEvent::Completed { .. }) {
            completed = true;
        }
    }
    task.await.unwrap();
    assert!(
        saw_retry,
        "expected AgentEvent::ProviderRetry on the UI channel"
    );
    assert!(completed, "retry should recover and complete");
}

/// Builds a runner with a scripted provider for the overflow-recovery
/// tests: failures are served first (in request order), then responses.
fn overflow_test_runner(
    temp: &TempDir,
    responses: Vec<Vec<ModelEvent>>,
    failures: Vec<ProviderError>,
    context_window_tokens: Option<u64>,
) -> AgentRunner {
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "long task")
        .unwrap();
    let mut provider_config = ProviderPreset::Custom.defaults();
    provider_config.model = "fixture".into();
    provider_config.context_window_tokens = context_window_tokens;
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    // Keep the compaction recent window small so the 30-message fixture
    // has history worth summarizing.
    let compaction = CompactionConfig {
        preserve_recent_tokens: Some(4_000),
        ..CompactionConfig::default()
    };
    AgentRunner::new(
        OpenAiClient::scripted_with_failures(responses, failures).unwrap(),
        provider_config,
        tools,
        storage,
        session_id,
    )
    .with_compaction_config(compaction)
}

fn overflow_failure() -> ProviderError {
    ProviderError::Status {
        status: 400,
        message: "This model's maximum context length is 65536 tokens. However, you requested 70000 tokens."
            .into(),
        retry_after_ms: None,
    }
}

fn overflow_items(count: usize) -> Vec<ConversationItem> {
    (0..count)
        .map(|i| ConversationItem::Message {
            role: Role::User,
            content: format!("message {i} {}", "x".repeat(1000)),
        })
        .collect()
}

#[tokio::test]
async fn context_overflow_compacts_and_retries_as_full_replay() {
    let temp = TempDir::new().unwrap();
    let runner = overflow_test_runner(
        &temp,
        vec![
            // Consumed by the recovery compaction's summary request.
            vec![
                ModelEvent::TextDelta("{\"goals\":\"summary\"}".into()),
                ModelEvent::Done,
            ],
            // Consumed by the retry (a full replay after compaction).
            vec![ModelEvent::TextDelta("recovered".into()), ModelEvent::Done],
        ],
        vec![overflow_failure()],
        Some(100_000),
    );
    let (events, mut receiver) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        runner.run(overflow_items(30), events).await;
    });

    let mut saw_retry = false;
    let mut saw_compacted = false;
    let mut final_answer = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ProviderRetry {
                attempt,
                reason,
                delay_ms,
            } => {
                saw_retry = true;
                assert_eq!(attempt, 1);
                assert_eq!(reason, "context overflow");
                assert_eq!(delay_ms, 0);
            }
            AgentEvent::CompactionCompleted { .. } => saw_compacted = true,
            AgentEvent::Completed { items } => {
                final_answer = items.iter().rev().find_map(|item| match item {
                    ConversationItem::Message {
                        role: Role::Assistant,
                        content,
                    } => Some(content.clone()),
                    _ => None,
                });
                break;
            }
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(saw_retry, "the overflow must trigger a ProviderRetry event");
    assert!(
        saw_compacted,
        "the recovery must compact the conversation first"
    );
    assert_eq!(final_answer.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn context_overflow_without_progress_fails_with_the_original_error() {
    let temp = TempDir::new().unwrap();
    // No window: compaction is a no-op, the hinted trim has no budget,
    // so there is no progress and the retry must be refused.
    let runner = overflow_test_runner(&temp, Vec::new(), vec![overflow_failure()], None);
    let (events, mut receiver) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        runner.run(overflow_items(30), events).await;
    });

    let mut failure = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Failed(error) => {
                failure = Some(error);
                break;
            }
            AgentEvent::ProviderRetry { .. } | AgentEvent::CompactionCompleted { .. } => {
                panic!("no retry may be attempted without measurable progress")
            }
            _ => {}
        }
    }
    task.await.unwrap();
    let failure = failure.expect("the run must fail");
    assert!(
        failure.contains("maximum context length"),
        "the original provider error stays authoritative: {failure}"
    );
}

#[tokio::test]
async fn context_overflow_retries_are_bounded_by_max_overflow_retries() {
    let temp = TempDir::new().unwrap();
    // Steps are consumed in request order: the first request overflows;
    // the recovery's compaction request fails (500), so the recovery
    // degrades to the hinted trim, which does shrink the oversized
    // history; the retry overflows again and the default cap (1) forbids
    // a second recovery — the original error is surfaced.
    let server_error = ProviderError::Status {
        status: 500,
        message: "internal error".into(),
        retry_after_ms: None,
    };
    let runner = overflow_test_runner(
        &temp,
        Vec::new(),
        vec![overflow_failure(), server_error, overflow_failure()],
        Some(100_000),
    );
    let (events, mut receiver) = mpsc::channel(64);
    // ~100.8k estimated tokens: above the 95_904 safe input capacity, so
    // the trim fallback has something to remove.
    let task = tokio::spawn(async move {
        runner.run(overflow_items(400), events).await;
    });

    let mut recoveries = 0usize;
    let mut compaction_failed = false;
    let mut failure = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ProviderRetry { attempt: 1, .. } => recoveries += 1,
            AgentEvent::ProviderRetry { attempt, .. } => {
                panic!("attempt {attempt} must never be attempted beyond the cap")
            }
            AgentEvent::CompactionFailed(_) => compaction_failed = true,
            AgentEvent::CompactionCompleted { .. } => {}
            AgentEvent::Failed(error) => {
                failure = Some(error);
                break;
            }
            _ => {}
        }
    }
    task.await.unwrap();
    assert_eq!(recoveries, 1);
    assert!(
        compaction_failed,
        "the recovery must report the failed compaction"
    );
    let failure = failure.expect("the run must fail after exhausting the cap");
    assert!(
        failure.contains("maximum context length"),
        "the original provider error stays authoritative: {failure}"
    );
}

#[tokio::test]
async fn main_agent_snapshots_file_write_pre_and_post_images() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("a.txt"), "before").unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "edit file")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![
        vec![
            ModelEvent::ToolCallComplete(ToolCall {
                id: "c1".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"a.txt","content":"after"}),
            }),
            ModelEvent::Done,
        ],
        vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
    ])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "edit file".into(),
                }],
                events,
            )
            .await;
    });
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Approval { reply, .. } => {
                let _ = reply.send(true);
            }
            AgentEvent::Completed { .. } => break,
            _ => {}
        }
    }
    task.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "after"
    );

    let turn = storage.head_turn_id(&session_id).unwrap().unwrap();
    let snapshots = storage.restore_turn_files(&session_id, &turn).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].path, "a.txt");
    assert_eq!(
        snapshots[0].pre_image.as_deref(),
        Some(b"before".as_slice())
    );
    assert_eq!(
        snapshots[0].post_image.as_deref(),
        Some(b"after".as_slice())
    );
    assert!(snapshots[0].existed);
}

#[tokio::test]
async fn session_allowed_tool_is_audited_as_session_allowed() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "edit")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    // Pre-grant a session allow for file_edit before the agent runs.
    tools.allow_for_session("file_edit", None);
    std::fs::write(temp.path().join("a.txt"), "before").unwrap();
    let provider = OpenAiClient::scripted(vec![
        vec![
            ModelEvent::ToolCallComplete(ToolCall {
                id: "c1".into(),
                name: "file_edit".into(),
                arguments: serde_json::json!({"path":"a.txt","old_string":"before","new_string":"after"}),
            }),
            ModelEvent::Done,
        ],
        vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
    ])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "edit".into(),
                }],
                events,
            )
            .await;
    });
    while let Some(event) = receiver.recv().await {
        if matches!(event, AgentEvent::Completed { .. }) {
            break;
        }
    }
    task.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "after"
    );
    // No Approval event should have been raised; the call was audited as
    // session-allowed rather than approved.
    let decision = storage.tool_decision("c1").unwrap().unwrap();
    assert_eq!(decision, "session-allowed");
}

#[tokio::test]
async fn reasoning_deltas_are_persisted_as_thinking_summary() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "think")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![vec![
        ModelEvent::ReasoningDelta("第一段".into()),
        ModelEvent::ReasoningDelta("第二段".into()),
        ModelEvent::TextDelta("answer".into()),
        ModelEvent::Done,
    ]])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "think".into(),
                }],
                events,
            )
            .await;
    });

    let mut completed = false;
    let mut reasoning_seen = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Completed { items } => {
                completed = true;
                assert!(items.iter().any(|item| {
                    matches!(
                        item,
                        ConversationItem::ThinkingSummary { content }
                            if content == "第一段第二段"
                    )
                }));
            }
            AgentEvent::ReasoningDelta(_) => reasoning_seen = true,
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(completed);
    assert!(reasoning_seen);

    let loaded = storage.load_messages(&session_id).unwrap();
    assert!(loaded.iter().any(|item| {
        matches!(
            item,
            ConversationItem::ThinkingSummary { content } if content == "第一段第二段"
        )
    }));
}

#[tokio::test]
async fn reasoning_completed_sits_between_last_reasoning_and_first_text() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "think")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![vec![
        ModelEvent::ReasoningDelta("第一段".into()),
        ModelEvent::ReasoningDelta("第二段".into()),
        ModelEvent::TextDelta("answer".into()),
        ModelEvent::Done,
    ]])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "think".into(),
                }],
                events,
            )
            .await;
    });

    let mut sequence = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ReasoningDelta(delta) => sequence.push(("reasoning", delta)),
            AgentEvent::ReasoningCompleted => sequence.push(("completed", String::new())),
            AgentEvent::TextDelta(delta) => sequence.push(("text", delta)),
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert_eq!(
        sequence,
        vec![
            ("reasoning", "第一段".to_owned()),
            ("reasoning", "第二段".to_owned()),
            ("completed", String::new()),
            ("text", "answer".to_owned()),
        ]
    );
}

#[tokio::test]
async fn no_reasoning_emits_no_completion_event() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "plain")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![vec![
        ModelEvent::TextDelta("answer".into()),
        ModelEvent::Done,
    ]])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "plain".into(),
                }],
                events,
            )
            .await;
    });

    let mut completed = false;
    let mut completion_events = 0usize;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ReasoningCompleted => completion_events += 1,
            AgentEvent::Completed { .. } => completed = true,
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(completed);
    assert_eq!(
        completion_events, 0,
        "rounds without reasoning must not emit"
    );
}

#[tokio::test]
async fn multi_round_reasoning_emits_one_completion_per_phase() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "multi")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    // Round 1: reasoning + body text, then a tool call. Round 2: reasoning +
    // final body text. Each round with reasoning followed by text must emit
    // exactly one completion barrier; none may leak across rounds.
    let responses = vec![
        vec![
            ModelEvent::ReasoningDelta("r1".into()),
            ModelEvent::TextDelta("t1".into()),
            ModelEvent::ToolCallComplete(ToolCall {
                id: "call-1".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path": "missing-1"}),
            }),
            ModelEvent::Done,
        ],
        vec![
            ModelEvent::ReasoningDelta("r2".into()),
            ModelEvent::TextDelta("t2".into()),
            ModelEvent::Done,
        ],
    ];
    let runner = AgentRunner::new(
        OpenAiClient::scripted(responses).unwrap(),
        ProviderPreset::Custom.defaults(),
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "multi".into(),
                }],
                events,
            )
            .await;
    });

    let mut tags = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ReasoningDelta(_) => tags.push("reasoning"),
            AgentEvent::ReasoningCompleted => tags.push("completed"),
            AgentEvent::TextDelta(_) => tags.push("text"),
            AgentEvent::ToolFinished { .. } => tags.push("tool"),
            AgentEvent::Completed { .. } => tags.push("run_completed"),
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert_eq!(
        tags,
        vec![
            // Round 1: reasoning then its own completion barrier before text.
            "reasoning",
            "completed",
            "text",
            "tool",
            // Round 2: a fresh barrier for the second reasoning phase.
            "reasoning",
            "completed",
            "text",
            "run_completed",
        ]
    );
}

#[tokio::test]
async fn tool_call_streaming_reports_merged_progress_between_text_and_approval() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "write html")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    // Replays the reported scenario: reasoning -> body text -> a ~6 KiB
    // file_write argument stream (10 x 600 B) -> completion -> the approval
    // gate, then a final answer round.
    let mut tool_round = vec![
        ModelEvent::ReasoningDelta("推演".into()),
        ModelEvent::TextDelta("正文".into()),
    ];
    for _ in 0..10 {
        tool_round.push(ModelEvent::ToolCallDelta {
            slot: "s0".into(),
            id: Some("c1".into()),
            name: Some("file_write".into()),
            arguments_delta: "x".repeat(600),
        });
    }
    tool_round.push(ModelEvent::ToolCallComplete(ToolCall {
        id: "c1".into(),
        name: "file_write".into(),
        arguments: serde_json::json!({"path": "a.txt", "content": "x".repeat(6000)}),
    }));
    tool_round.push(ModelEvent::Done);
    let responses = vec![
        tool_round,
        vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
    ];
    let runner = AgentRunner::new(
        OpenAiClient::scripted(responses).unwrap(),
        ProviderPreset::Custom.defaults(),
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "write html".into(),
                }],
                events,
            )
            .await;
    });

    let mut streaming = Vec::new();
    let mut text_seen = false;
    let mut after_text = false;
    let mut saw_approval = false;
    let mut saw_tool_started = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolCallStreaming {
                name,
                received_bytes,
            } => {
                assert!(after_text, "streaming must follow the body text");
                assert!(
                    !saw_approval && !saw_tool_started,
                    "streaming must precede approval/tool start"
                );
                streaming.push((name, received_bytes));
            }
            AgentEvent::TextDelta(_) => {
                after_text = true;
                text_seen = true;
            }
            AgentEvent::Approval { reply, .. } => {
                saw_approval = true;
                let _ = reply.send(true);
            }
            AgentEvent::ToolStarted(_) => saw_tool_started = true,
            AgentEvent::Completed { .. } => break,
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(text_seen);
    assert!(
        !streaming.is_empty(),
        "tool call streaming must be reported"
    );
    assert_eq!(
        streaming[0].0.as_deref(),
        Some("file_write"),
        "the first event carries the tool name"
    );
    let mut previous = 0u64;
    for (_, bytes) in &streaming {
        assert!(
            *bytes > previous,
            "received_bytes must be monotonic; got {streaming:?}"
        );
        previous = *bytes;
    }
    assert!(
        streaming.len() < 10,
        "the 1 KiB merge must produce fewer events than deltas; got {}",
        streaming.len()
    );
    assert_eq!(
        streaming.last().unwrap().1,
        5400,
        "the last report carries the cumulative bytes at the final threshold"
    );
    assert!(
        saw_approval && saw_tool_started,
        "the tool round must end in the approval gate then tool execution"
    );
}

#[tokio::test]
async fn child_agent_creates_nested_session_with_model_and_result() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "parent")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![vec![
        ModelEvent::TextDelta("child result".into()),
        ModelEvent::Done,
    ]])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );

    let call = ToolCall {
        id: "call-1".into(),
        name: "agent_spawn".into(),
        arguments: serde_json::json!({"prompt":"do the plan","role":"plan","model":"gpt-5"}),
    };
    let (ui_events, mut receiver) = mpsc::channel(16);
    let result = runner.run_child(&call, &ui_events).await.unwrap();
    let payload: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["output"], "child result");
    assert_eq!(payload["title"], "plan·do the plan");

    let sessions = storage.list_sessions(temp.path()).unwrap();
    assert_eq!(sessions.len(), 2);
    let child = sessions.iter().find(|s| s.id != session_id).unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(child.title, "plan·do the plan");
    assert_eq!(
        storage.session_provider_model(&child.id).unwrap().1,
        "gpt-5"
    );
    assert_eq!(storage.load_messages(&child.id).unwrap().len(), 2);

    let mut statuses = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let AgentEvent::ChildSessionProgress { progress, .. } = event {
            statuses.push(progress.status);
        }
    }
    assert_eq!(
        statuses,
        vec![
            ChildSessionStatus::Queued,
            ChildSessionStatus::WaitingModel,
            ChildSessionStatus::Streaming,
            ChildSessionStatus::Completed,
        ]
    );
}

#[tokio::test]
async fn child_concurrency_slots_enforce_the_configured_limit() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        OpenAiClient::scripted(Vec::new()).unwrap(),
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage,
        session_id,
    )
    .with_cluster_config(ClusterConfig {
        max_parallel_children: Some(2),
        ..ClusterConfig::default()
    });

    let first = runner.child_slots.try_acquire().unwrap();
    let _second = runner.child_slots.try_acquire().unwrap();
    assert!(runner.child_slots.try_acquire().is_err());
    drop(first);
    assert!(runner.child_slots.try_acquire().is_ok());
}

#[tokio::test]
async fn zero_child_tool_budget_does_not_execute_the_tool() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        OpenAiClient::scripted(Vec::new()).unwrap(),
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage,
        session_id,
    );
    let call = ToolCall {
        id: "must-not-run".into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"missing"}),
    };
    let mut budget = Duration::ZERO;
    assert!(
        runner
            .execute_child_tool_with_budget(&call, &mut budget)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn cancellation_progress_survives_a_temporarily_full_channel() {
    let (events, mut receiver) = mpsc::channel(1);
    events.send(AgentEvent::SessionsChanged).await.unwrap();
    let guard = ChildCancellationGuard {
        ui_events: events,
        session_id: "cancelled-child".into(),
        max_turns: 3,
        finished: false,
    };
    drop(guard);

    assert!(matches!(
        receiver.recv().await,
        Some(AgentEvent::SessionsChanged)
    ));
    let cancelled = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        cancelled,
        AgentEvent::ChildSessionProgress {
            session_id,
            progress: ChildSessionProgress {
                status: ChildSessionStatus::Cancelled,
                ..
            },
        } if session_id == "cancelled-child"
    ));
}

#[test]
fn child_tool_filter_never_grants_terminal_or_spawn() {
    assert!(child_tool_name_allowed("file_read", Some("plan"), &[]));
    assert!(child_tool_name_allowed(
        "file_write",
        Some("implement"),
        &[]
    ));
    assert!(!child_tool_name_allowed("file_write", Some("plan"), &[]));
    assert!(!child_tool_name_allowed(
        "terminal_exec",
        Some("implement"),
        &[]
    ));
    assert!(!child_tool_name_allowed(
        "agent_spawn",
        Some("implement"),
        &[]
    ));
    assert!(!child_tool_name_allowed(
        "file_delete",
        Some("implement"),
        &[]
    ));
    assert!(child_tool_name_allowed(
        "file_read",
        None,
        &["file_read".into()]
    ));
}

#[test]
fn child_provider_is_inferred_from_model_prefix() {
    assert_eq!(
        infer_child_provider("deepseek-v4-flash", ProviderPreset::Qwen),
        ProviderPreset::DeepSeek
    );
    assert_eq!(
        infer_child_provider("qwen3.5-flash", ProviderPreset::DeepSeek),
        ProviderPreset::Qwen
    );
    assert_eq!(
        infer_child_provider("gpt-5-mini", ProviderPreset::DeepSeek),
        ProviderPreset::OpenAi
    );
    // A provider that already hosts the model keeps it.
    assert_eq!(
        infer_child_provider("deepseek-v4-flash", ProviderPreset::Volcano),
        ProviderPreset::Volcano
    );
    assert_eq!(
        infer_child_provider("deepseek-v4-flash", ProviderPreset::DeepSeek),
        ProviderPreset::DeepSeek
    );
}

#[test]
fn child_model_validation_allows_unknown_full_ids_and_catches_mismatches() {
    let mut qwen = ProviderPreset::Qwen.defaults();
    qwen.model = "qwen3.5-flash".into();
    assert!(validate_child_model(&qwen).is_ok());

    let mut qwen_wrong = ProviderPreset::Qwen.defaults();
    qwen_wrong.model = "deepseek-v4-flash".into();
    let error = validate_child_model(&qwen_wrong).unwrap_err();
    assert!(error.contains("belongs to DeepSeek"));
    assert!(error.contains("provider=deepseek"));

    let mut openai_shorthand = ProviderPreset::OpenAi.defaults();
    openai_shorthand.model = "v4pro".into();
    assert!(validate_child_model(&openai_shorthand).is_err());
}

#[test]
fn child_title_prefers_explicit_then_role_with_prompt_snippet() {
    let arguments = ChildArgs {
        prompt: "review the database schema carefully".into(),
        max_turns: None,
        role: Some("reviewer".into()),
        model: None,
        provider: None,
        agent: None,
        title: None,
    };
    assert_eq!(
        child_title(&arguments, None, Some("reviewer")),
        "reviewer·review the databas…"
    );

    let arguments = ChildArgs {
        title: Some("自定义标题".into()),
        ..arguments
    };
    assert_eq!(
        child_title(&arguments, None, Some("reviewer")),
        "自定义标题"
    );
}

#[tokio::test]
async fn child_agent_uses_configured_agent_template() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "parent")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![vec![
        ModelEvent::TextDelta("review done".into()),
        ModelEvent::Done,
    ]])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    )
    .with_configured_agents(vec![AgentConfig {
        name: "reviewer".into(),
        mode: crate::commands::AgentMode::Explore,
        max_turns: 2,
        allowed_tools: vec!["file_read".into()],
        system_prompt: "Review for correctness.".into(),
    }]);

    let call = ToolCall {
        id: "call-agent".into(),
        name: "agent_spawn".into(),
        arguments: serde_json::json!({"prompt":"review the code","agent":"reviewer"}),
    };
    let (ui_events, mut receiver) = mpsc::channel(16);
    let result = runner.run_child(&call, &ui_events).await.unwrap();
    let payload: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["output"], "review done");
    assert_eq!(payload["title"], "reviewer");

    let sessions = storage.list_sessions(temp.path()).unwrap();
    let child = sessions.iter().find(|s| s.id != session_id).unwrap();
    assert_eq!(storage.session_mode(&child.id).unwrap(), "explore");
    assert_eq!(
        storage.session_child_role(&child.id).unwrap().as_deref(),
        Some("reviewer")
    );
    // The child session must be created before the result is returned.
    assert!(receiver.recv().await.is_some());
}

#[tokio::test]
async fn child_agent_rejects_invalid_model_name() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        OpenAiClient::scripted(Vec::new()).unwrap(),
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage,
        session_id,
    );
    let call = ToolCall {
        id: "bad-model".into(),
        name: "agent_spawn".into(),
        arguments: serde_json::json!({"prompt":"x","model":"v4pro"}),
    };
    let (ui_events, _receiver) = mpsc::channel(16);
    let error = runner.run_child(&call, &ui_events).await.unwrap_err();
    assert!(error.contains("unknown model"));
    assert!(error.contains("gpt-5-mini"));
}

#[tokio::test]
async fn child_agent_executes_read_tools_across_turns() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("plan.txt"), "plan content").unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "parent")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![
        vec![
            ModelEvent::ToolCallComplete(ToolCall {
                id: "c1".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"plan.txt"}),
            }),
            ModelEvent::Done,
        ],
        vec![
            ModelEvent::TextDelta("read and planned".into()),
            ModelEvent::Done,
        ],
    ])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );
    let call = ToolCall {
        id: "spawn".into(),
        name: "agent_spawn".into(),
        arguments: serde_json::json!({"prompt":"read plan.txt then plan"}),
    };
    let (ui_events, _receiver) = mpsc::channel(16);
    let result = runner.run_child(&call, &ui_events).await.unwrap();
    assert!(result.contains("read and planned"));

    let sessions = storage.list_sessions(temp.path()).unwrap();
    let child = sessions.iter().find(|s| s.id != session_id).unwrap();
    let messages = storage.load_messages(&child.id).unwrap();
    assert!(
        messages
            .iter()
            .any(|item| matches!(item, ConversationItem::ToolOutput { .. }))
    );
}

#[tokio::test]
async fn child_agent_implement_role_writes_files_with_approval() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("plan.txt"), "plan").unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "parent")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let provider = OpenAiClient::scripted(vec![
        vec![
            ModelEvent::ToolCallComplete(ToolCall {
                id: "c1".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"plan.txt"}),
            }),
            ModelEvent::Done,
        ],
        vec![
            ModelEvent::ToolCallComplete(ToolCall {
                id: "c2".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"out.txt","content":"written"}),
            }),
            ModelEvent::Done,
        ],
        vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
    ])
    .unwrap();
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::OpenAi.defaults(),
        tools,
        storage.clone(),
        session_id.clone(),
    );

    let (ui_events, mut receiver) = mpsc::channel(16);
    let approver = tokio::spawn(async move {
        let mut statuses = Vec::new();
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::Approval { reply, .. } => {
                    let _ = reply.send(true);
                }
                AgentEvent::ChildSessionProgress { progress, .. } => {
                    statuses.push(progress.status);
                }
                _ => {}
            }
        }
        statuses
    });

    let call = ToolCall {
        id: "impl".into(),
        name: "agent_spawn".into(),
        arguments: serde_json::json!({"prompt":"read plan.txt then write out.txt","role":"implement"}),
    };
    let result = runner.run_child(&call, &ui_events).await.unwrap();
    assert!(result.contains("done"));
    assert!(temp.path().join("out.txt").exists());
    drop(ui_events);
    drop(runner);
    let statuses = approver.await.unwrap();
    let approval_slot = statuses
        .iter()
        .position(|status| *status == ChildSessionStatus::WaitingApprovalSlot)
        .unwrap();
    let user_approval = statuses
        .iter()
        .position(|status| *status == ChildSessionStatus::WaitingApproval)
        .unwrap();
    assert!(approval_slot < user_approval);
}

#[tokio::test]
async fn duplicate_tool_call_is_skipped_and_conversation_continues() {
    let call = |id: &str| ToolCall {
        id: id.into(),
        name: "file_read".into(),
        arguments: serde_json::json!({"path":"fixture.txt"}),
    };
    let provider = OpenAiClient::scripted(vec![
        vec![
            ModelEvent::ToolCallComplete(call("call-1")),
            ModelEvent::Done,
        ],
        vec![
            ModelEvent::ToolCallComplete(call("call-2")),
            ModelEvent::Done,
        ],
        vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
    ])
    .unwrap();
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("fixture.txt"), "fixture result").unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    storage
        .append_message(&session_id, Role::User, "read fixture")
        .unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        provider,
        ProviderPreset::Custom.defaults(),
        tools,
        storage,
        session_id,
    );
    let (events, mut receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        runner
            .run(
                vec![ConversationItem::Message {
                    role: Role::User,
                    content: "read fixture".into(),
                }],
                events,
            )
            .await;
    });

    let mut starts = 0;
    let mut duplicate_notice = false;
    let mut completed = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolStarted(_) => starts += 1,
            AgentEvent::ToolFinished { result, .. } => {
                duplicate_notice |= result.starts_with("Duplicate tool call was not executed");
            }
            AgentEvent::Completed { .. } => completed = true,
            AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert_eq!(starts, 1);
    assert!(duplicate_notice);
    assert!(completed);
}

#[test]
fn todo_tools_are_session_scoped_and_replace_the_whole_list() {
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
    let session_id = storage.create_session(temp.path()).unwrap();
    let tools = Arc::new(ToolRegistry::new(
        Workspace::new(temp.path()).unwrap(),
        RuntimeConfig::default(),
        false,
    ));
    let runner = AgentRunner::new(
        OpenAiClient::scripted(Vec::new()).unwrap(),
        ProviderPreset::OpenAi.defaults(),
        tools.clone(),
        storage.clone(),
        session_id.clone(),
    );

    assert!(matches!(
        tools.policy(&ToolCall {
            id: "policy".into(),
            name: "todo_write".into(),
            arguments: serde_json::json!({"tasks":[]})
        }),
        PolicyDecision::Allow
    ));
    assert!(!child_tool_name_allowed(
        "todo_write",
        Some("implement"),
        &[]
    ));
    assert!(
        tools
            .definitions()
            .iter()
            .any(|tool| tool.name == "todo_write")
    );

    let read = ToolCall {
        id: "todo-read".into(),
        name: "todo_read".into(),
        arguments: serde_json::json!({}),
    };
    let (output, updated) = runner.execute_todo_tool(&read);
    assert_eq!(output, r#"{"tasks":[]}"#);
    assert!(updated.is_none());

    let write = ToolCall {
        id: "todo-write".into(),
        name: "todo_write".into(),
        arguments: serde_json::json!({
            "tasks": [
                {"title":"inspect","status":"pending"},
                {"title":"implement","status":"in_progress"}
            ]
        }),
    };
    let (output, updated) = runner.execute_todo_tool(&write);
    let tasks = updated.expect("todo_write should return updated tasks");
    assert_eq!(tasks.len(), 2);
    let payload: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(payload["tasks"][1]["title"], "implement");
    assert_eq!(storage.list_tasks(&session_id).unwrap(), tasks);

    let first_id = tasks[0].id.clone();
    let write = ToolCall {
        id: "todo-write-2".into(),
        name: "todo_write".into(),
        arguments: serde_json::json!({
            "tasks": [
                {"id":first_id,"title":"inspect and test","status":"done"}
            ]
        }),
    };
    let (_, updated) = runner.execute_todo_tool(&write);
    let tasks = updated.unwrap();
    assert_eq!(tasks[0].id, first_id);
    assert_eq!(tasks[0].title, "inspect and test");
    assert_eq!(storage.list_tasks(&session_id).unwrap().len(), 1);
}
