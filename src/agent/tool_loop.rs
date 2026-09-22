use std::collections::HashSet;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    model::TodoTask,
    provider::{ConversationItem, ToolCall},
};

use super::{
    AgentEvent, AgentRunner, TodoWriteArguments, snapshot_target_path, todo_tool_response,
    tool_call_signature,
};

impl AgentRunner {
    pub(super) fn execute_todo_tool(&self, call: &ToolCall) -> (String, Option<Vec<TodoTask>>) {
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

    pub(super) fn execute_todo_write(
        &self,
        arguments: &Value,
    ) -> Result<(String, Vec<TodoTask>), String> {
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
    pub(super) fn snapshot_tool_pre(&self, call: &ToolCall) -> Result<bool, String> {
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
    pub(super) fn snapshot_tool_post(&self, call: &ToolCall) -> Result<(), String> {
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
    pub(super) async fn complete_tool(
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
}
