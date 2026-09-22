use std::time::Instant;

use tokio::sync::oneshot;

use crate::{
    model::TodoTask,
    provider::{ConversationItem, ToolCall, Usage},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildSessionStatus {
    Queued,
    WaitingModel,
    Streaming,
    RunningTool,
    WaitingApprovalSlot,
    WaitingApproval,
    Completed,
    Failed,
    TurnLimit,
    TimedOut,
    Cancelled,
}
impl ChildSessionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::TurnLimit | Self::TimedOut | Self::Cancelled
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::WaitingModel => "等待模型",
            Self::Streaming => "模型响应中",
            Self::RunningTool => "执行工具",
            Self::WaitingApprovalSlot => "等待审批槽",
            Self::WaitingApproval => "等待审批",
            Self::Completed => "完成",
            Self::Failed => "失败",
            Self::TurnLimit => "达到轮次上限",
            Self::TimedOut => "执行超时",
            Self::Cancelled => "已取消",
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TurnLimit => "turn_limit",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Queued
            | Self::WaitingModel
            | Self::Streaming
            | Self::RunningTool
            | Self::WaitingApprovalSlot
            | Self::WaitingApproval => "running",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSessionProgress {
    pub status: ChildSessionStatus,
    pub turn: usize,
    pub max_turns: usize,
    pub tool: Option<String>,
    pub updated_at: Instant,
}

impl ChildSessionProgress {
    pub fn label(&self) -> String {
        let turn = (self.turn > 0).then(|| {
            if self.max_turns == 0 {
                format!(" 第{}轮", self.turn)
            } else {
                format!(" {}/{}", self.turn, self.max_turns)
            }
        });
        let tool = self.tool.as_deref().map(|name| format!(" ·{name}"));
        format!(
            "{}{}{}",
            self.status.label(),
            turn.unwrap_or_default(),
            tool.unwrap_or_default()
        )
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    ReasoningDelta(String),
    /// Marks the end of the current reasoning phase. Guaranteed to be emitted
    /// exactly once per model round between the last `ReasoningDelta` and the
    /// first `TextDelta`; never emitted for rounds without reasoning. Consumers
    /// treat it as the render barrier from the thinking view to the body view.
    ReasoningCompleted,
    ProviderRetry {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
    ModelStreaming,
    WebSearchStarted {
        query: String,
    },
    WebSearchResult {
        title: String,
        url: String,
        snippet: String,
    },
    WebSearchCompleted {
        count: usize,
    },
    Cancelled(String),
    TextDelta(String),
    /// Reports streaming progress while the model generates a tool call's
    /// arguments (a `file_write` payload can take seconds). `received_bytes` is
    /// monotonic within one model round; the event is emitted at ~1 KiB
    /// thresholds so the UI can animate a "generating tool call" row instead of
    /// freezing silently. Ordering within a round: after the last `TextDelta`
    /// and before `Approval`/`ToolStarted`.
    ToolCallStreaming {
        name: Option<String>,
        received_bytes: u64,
    },
    Approval {
        call: ToolCall,
        reason: String,
        source_session_id: Option<String>,
        source_title: Option<String>,
        reply: oneshot::Sender<bool>,
    },
    ToolStarted(ToolCall),
    ToolFinished {
        call: ToolCall,
        result: String,
    },
    /// Real usage reported by the provider for one request round.
    /// `input_estimate` is the runner's local estimate of that request's
    /// input (0 when the round was incremental and therefore not
    /// comparable); consumers use the pair to calibrate future estimates.
    Usage {
        usage: Usage,
        input_estimate: u64,
    },
    Completed {
        items: Vec<ConversationItem>,
    },
    Failed(String),
    SessionsChanged,
    ChildSessionProgress {
        session_id: String,
        progress: ChildSessionProgress,
    },
    LocalCommandFinished {
        command: String,
        result: String,
    },
    CompactionStarted,
    CompactionCompleted {
        hidden: usize,
    },
    CompactionFailed(String),
    TodoUpdated {
        tasks: Vec<TodoTask>,
    },
}
