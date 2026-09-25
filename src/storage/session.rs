use std::path::Path;

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::{
    model::TodoTask,
    provider::{ConversationItem, Role, ToolCall},
};

use super::{
    SessionSummary, Storage, StorageError, StoredMessage, append_message_on_turn,
    conversation_item_bytes, decode_conversation_item, parse_todo_status, validate_todo_tasks,
};

impl Storage {
    pub fn create_session(&self, workspace: &Path) -> Result<String, StorageError> {
        let id = Uuid::new_v4().to_string();
        let turn_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, head_turn_id) VALUES (?1, ?2, ?3, ?4, ?4, 'build', 'openai', '', ?5)",
            params![id, workspace.display().to_string(), "New session", now, turn_id],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![turn_id, id, now],
        )?;
        Ok(id)
    }

    /// Creates a child session nested under `parent_id`. The child owns its own
    /// provider/model so a cluster can run different roles on different models.
    /// `mode` is the session mode used when the child is opened later, and
    /// `child_role` preserves the role-based tool restrictions for that later
    /// interaction (implement roles may write files but still never receive
    /// terminal or spawn tools).
    #[allow(clippy::too_many_arguments)]
    pub fn create_child_session(
        &self,
        workspace: &Path,
        parent_id: &str,
        provider: &str,
        model: &str,
        title: &str,
        mode: &str,
        child_role: &str,
    ) -> Result<String, StorageError> {
        let id = Uuid::new_v4().to_string();
        let turn_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, parent_id, head_turn_id, child_role, child_status) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'running')",
            params![id, workspace.display().to_string(), title, now, mode, provider, model, parent_id, turn_id, child_role],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![turn_id, id, now],
        )?;
        Ok(id)
    }

    /// Returns the stored provider preset id and model for a session.
    pub fn session_provider_model(
        &self,
        session_id: &str,
    ) -> Result<(String, String), StorageError> {
        self.lock()?
            .query_row(
                "SELECT provider, model FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StorageError::from)
    }

    /// Returns the child role captured at spawn time, if this is a child session.
    pub fn session_child_role(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT child_role FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn session_parent_id(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT parent_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Returns the workspace path this session belongs to.
    pub fn session_workspace(&self, session_id: &str) -> Result<String, StorageError> {
        self.lock()?
            .query_row(
                "SELECT workspace FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    /// Returns the current head turn id for a session, if any.
    pub fn head_turn_id(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn latest_session(&self, workspace: &Path) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT id FROM sessions WHERE workspace = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC LIMIT 1",
                [workspace.display().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn list_sessions(&self, workspace: &Path) -> Result<Vec<SessionSummary>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, title, parent_id, child_status FROM sessions WHERE workspace = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC, created_at DESC",
        )?;
        let rows = statement.query_map([workspace.display().to_string()], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                parent_id: row.get(2)?,
                child_status: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn set_child_status(&self, session_id: &str, status: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE sessions SET child_status = ?2, updated_at = ?3 WHERE id = ?1 AND parent_id IS NOT NULL",
            params![session_id, status, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_child_allowed_tools(
        &self,
        session_id: &str,
        tools: &[String],
    ) -> Result<(), StorageError> {
        let encoded = serde_json::to_string(tools).map_err(StorageError::from)?;
        self.lock()?.execute(
            "UPDATE sessions SET child_allowed_tools = ?2 WHERE id = ?1 AND parent_id IS NOT NULL",
            params![session_id, encoded],
        )?;
        Ok(())
    }

    pub fn session_child_allowed_tools(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<String>>, StorageError> {
        let encoded = self
            .lock()?
            .query_row(
                "SELECT child_allowed_tools FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        encoded
            .map(|value| serde_json::from_str(&value).map_err(StorageError::from))
            .transpose()
    }

    /// Marks children left in the active state by a previous process as failed
    /// and leaves a message explaining the interruption in their transcript.
    pub fn mark_running_children_interrupted(&self) -> Result<(), StorageError> {
        let ids = {
            let connection = self.lock()?;
            let mut statement = connection.prepare(
                "SELECT id FROM sessions WHERE parent_id IS NOT NULL AND child_status = 'running'",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            connection.execute(
                "UPDATE sessions SET child_status = 'failed', updated_at = ?1 WHERE parent_id IS NOT NULL AND child_status = 'running'",
                [Utc::now().to_rfc3339()],
            )?;
            ids
        };
        for id in ids {
            self.append_message(
                &id,
                Role::Assistant,
                "[child interrupted by process restart]",
            )?;
        }
        Ok(())
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let current_turn: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let turn_id = current_turn
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if current_turn.is_none() {
            let now = Utc::now().to_rfc3339();
            connection.execute(
                "INSERT OR IGNORE INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
                params![turn_id, session_id, now],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, turn_id],
            )?;
        }
        if role == Role::User {
            let child = Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();
            connection.execute(
                "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![child, session_id, turn_id, now],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, child],
            )?;
            return append_message_on_turn(&connection, session_id, &child, role, content);
        }
        append_message_on_turn(&connection, session_id, &turn_id, role, content)
    }

    /// Persists the incomplete assistant answer for a session, replacing any
    /// previous partial. Written on stream interruption, provider failure or
    /// user cancellation; cleared on normal completion.
    pub fn save_partial(&self, session_id: &str, content: &str) -> Result<(), StorageError> {
        let content = content.trim();
        if content.is_empty() {
            return self.clear_partial(session_id);
        }
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND partial = 1",
            [session_id],
        )?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, partial) VALUES (?1, 'assistant', ?2, ?3, NULL, 'message', 0, 1)",
            params![session_id, content, now],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Loads the persisted incomplete assistant answer for a session, if any.
    pub fn load_partial(&self, session_id: &str) -> Result<Option<StoredMessage>, StorageError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT id, role, content, kind, metadata, created_at FROM messages WHERE session_id = ?1 AND partial = 1 ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| {
                    Ok(StoredMessage {
                        id: row.get(0)?,
                        role: row.get(1)?,
                        content: row.get(2)?,
                        kind: row.get(3)?,
                        metadata: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Removes the persisted partial answer for a session (used when a formal
    /// assistant message replaces it or when the session is cleared).
    pub fn clear_partial(&self, session_id: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND partial = 1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn append_context(
        &self,
        session_id: &str,
        label: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let turn_id: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, 'context', ?2, ?3, ?4, 'context', 0, ?5)",
            params![session_id, content, now, turn_id, label],
        )?;
        connection.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn append_thinking_summary(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "thinking", "thinking_summary", content, None)
    }

    pub fn append_compaction_summary(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "user", "compaction_summary", content, None)
    }

    pub fn compact_with_summary(
        &self,
        session_id: &str,
        summary: &str,
        keep: usize,
    ) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        let ids = {
            let mut stmt = tx.prepare(
                "SELECT id FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id DESC",
            )?;
            stmt.query_map([session_id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let hidden: Vec<i64> = ids.into_iter().skip(keep).collect();
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO compactions(session_id, hidden_ids, summary, created_at) VALUES (?1, ?2, ?3, ?4)", params![session_id, serde_json::to_string(&hidden)?, summary, now])?;
        for id in &hidden {
            tx.execute("UPDATE messages SET hidden = 1 WHERE id = ?1", [id])?;
        }
        let turn: Option<String> = tx
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(turn_id) = turn {
            tx.execute("INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden) VALUES (?1, 'user', ?2, ?3, ?4, 'compaction_summary', 0)", params![session_id, summary, now, turn_id])?;
        }
        tx.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(hidden.len())
    }

    pub fn restore_latest_compaction(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        let row: Option<(i64, String)> = tx.query_row("SELECT id, hidden_ids FROM compactions WHERE session_id = ?1 ORDER BY id DESC LIMIT 1", [session_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((id, encoded)) = row else {
            return Ok(false);
        };
        let ids: Vec<i64> = serde_json::from_str(&encoded)?;
        for msg_id in ids {
            tx.execute("UPDATE messages SET hidden = 0 WHERE id = ?1", [msg_id])?;
        }
        tx.execute("UPDATE messages SET hidden = 1 WHERE session_id = ?1 AND kind = 'compaction_summary' AND id = (SELECT max(id) FROM messages WHERE session_id = ?1 AND kind = 'compaction_summary')", [session_id])?;
        tx.execute("DELETE FROM compactions WHERE id = ?1", [id])?;
        tx.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn append_provider_item(
        &self,
        session_id: &str,
        item: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.append_typed_item(
            session_id,
            "assistant",
            "provider_item",
            &serde_json::to_string(item)?,
            None,
        )
    }

    pub fn append_tool_calls(
        &self,
        session_id: &str,
        calls: &[ToolCall],
    ) -> Result<(), StorageError> {
        let content = serde_json::to_string(calls)?;
        self.append_typed_item(session_id, "assistant", "tool_calls", &content, None)
    }

    pub fn append_tool_output(
        &self,
        session_id: &str,
        call_id: &str,
        output: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "tool", "tool_output", output, Some(call_id))
    }

    fn append_typed_item(
        &self,
        session_id: &str,
        role: &str,
        kind: &str,
        content: &str,
        metadata: Option<&str>,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let turn_id: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![session_id, role, content, now, turn_id, kind, metadata],
        )?;
        connection.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), StorageError> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let title = title.chars().take(120).collect::<String>();
        self.lock()?.execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1 AND deleted_at IS NULL",
            params![session_id, title, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Soft-deletes the session and all of its descendants, returning every
    /// deleted id (root first) so callers can tear down their in-memory
    /// runtimes and tracking state for the whole subtree.
    pub fn delete_session(&self, session_id: &str) -> Result<Vec<String>, StorageError> {
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "WITH RECURSIVE descendants(id, depth) AS (
                 SELECT id, 0 FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT s.id, d.depth + 1 FROM sessions s JOIN descendants d ON s.parent_id = d.id
             )
             SELECT id FROM descendants ORDER BY depth",
        )?;
        let deleted = statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        connection.execute(
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT s.id FROM sessions s JOIN descendants d ON s.parent_id = d.id
             )
             UPDATE sessions SET deleted_at = ?2 WHERE id IN (SELECT id FROM descendants)",
            params![session_id, now],
        )?;
        Ok(deleted)
    }

    pub fn fork_session(&self, session_id: &str) -> Result<String, StorageError> {
        let connection = self.lock()?;
        let (workspace, title, mode, provider, model): (String, String, String, String, String) =
            connection.query_row(
                "SELECT workspace, title, mode, provider, model FROM sessions WHERE id = ?1",
                [session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        let new_id = Uuid::new_v4().to_string();
        let root_turn = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, parent_id, head_turn_id) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![new_id, workspace, format!("{title} (fork)"), now, mode, provider, model, session_id, root_turn],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![root_turn, new_id, now],
        )?;
        let rows = {
            let mut statement = connection.prepare(
                "SELECT role, content, kind, hidden, metadata FROM messages WHERE session_id = ?1 ORDER BY id ASC",
            )?;
            statement
                .query_map([session_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (role, content, kind, hidden, metadata) in rows {
            connection.execute(
                "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![new_id, role, content, now, root_turn, kind, hidden, metadata],
            )?;
        }
        let tasks = {
            let mut statement = connection.prepare(
                "SELECT title, status, created_at, updated_at FROM session_tasks WHERE session_id = ?1 ORDER BY position ASC",
            )?;
            statement
                .query_map([session_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (position, (title, status, created_at, updated_at)) in tasks.into_iter().enumerate() {
            connection.execute(
                "INSERT INTO session_tasks(id, session_id, position, title, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    new_id,
                    position as i64,
                    title,
                    status,
                    created_at,
                    updated_at
                ],
            )?;
        }
        Ok(new_id)
    }

    pub fn list_tasks(&self, session_id: &str) -> Result<Vec<TodoTask>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, title, status, created_at, updated_at FROM session_tasks \
             WHERE session_id = ?1 ORDER BY position ASC",
        )?;
        let rows = statement
            .query_map([session_id], |row| {
                Ok(TodoTask {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    status: parse_todo_status(row.get::<_, String>(2)?)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn replace_tasks(&self, session_id: &str, tasks: &[TodoTask]) -> Result<(), StorageError> {
        validate_todo_tasks(tasks)?;
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM session_tasks WHERE session_id = ?1",
            [session_id],
        )?;
        for (position, task) in tasks.iter().enumerate() {
            transaction.execute(
                "INSERT INTO session_tasks(id, session_id, position, title, status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    task.id,
                    session_id,
                    position as i64,
                    task.title,
                    task.status.as_str(),
                    task.created_at,
                    task.updated_at
                ],
            )?;
        }
        transaction.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![session_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn compact_session(&self, session_id: &str, keep: usize) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        let ids = {
            let mut statement = connection.prepare(
                "SELECT id FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id DESC",
            )?;
            statement
                .query_map([session_id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let hidden = ids.into_iter().skip(keep).collect::<Vec<_>>();
        for id in &hidden {
            connection.execute("UPDATE messages SET hidden = 1 WHERE id = ?1", [id])?;
        }
        Ok(hidden.len())
    }

    pub fn set_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE sessions SET mode = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, mode, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn session_mode(&self, session_id: &str) -> Result<String, StorageError> {
        self.lock()?
            .query_row(
                "SELECT mode FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<ConversationItem>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "WITH RECURSIVE chain(id) AS (
                 SELECT head_turn_id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                 WHERE turns.parent_id IS NOT NULL
             )
             SELECT role, content, kind, metadata FROM messages
             WHERE session_id = ?1 AND hidden = 0 AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
             ORDER BY id ASC",
        )?;
        let rows = statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(role, content, kind, metadata)| match kind.as_str() {
                "context" => Ok(ConversationItem::Context {
                    label: metadata.unwrap_or_else(|| "context".into()),
                    content,
                }),
                "thinking_summary" => Ok(ConversationItem::ThinkingSummary { content }),
                "compaction_summary" => Ok(ConversationItem::CompactionSummary { content }),
                "provider_item" => Ok(ConversationItem::ProviderItem {
                    item: serde_json::from_str(&content)?,
                }),
                "tool_calls" => Ok(ConversationItem::AssistantToolCalls {
                    calls: serde_json::from_str(&content)?,
                }),
                "tool_output" => Ok(ConversationItem::ToolOutput {
                    call_id: metadata.unwrap_or_default(),
                    output: content,
                }),
                _ if role == "context" => Ok(ConversationItem::Context {
                    label: metadata.unwrap_or_else(|| "context".into()),
                    content,
                }),
                _ => Ok(ConversationItem::Message {
                    role: match role.as_str() {
                        "system" => Role::System,
                        "assistant" => Role::Assistant,
                        _ => Role::User,
                    },
                    content,
                }),
            })
            .collect()
    }

    /// Loads only the newest bounded working set from the active turn chain.
    /// Unlike `load_messages`, this query applies the row and item limits in
    /// SQLite before collecting strings, so restoring a session does not
    /// materialize the entire historical transcript just to discard it.
    pub fn load_messages_bounded(
        &self,
        session_id: &str,
        max_items: usize,
        max_bytes: usize,
        max_item_bytes: usize,
    ) -> Result<Vec<ConversationItem>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "WITH RECURSIVE chain(id) AS (
                 SELECT head_turn_id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                 WHERE turns.parent_id IS NOT NULL
             )
             SELECT role, substr(content, 1, ?3), kind, metadata, length(content)
             FROM messages
             WHERE session_id = ?1 AND hidden = 0 AND partial = 0
               AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![
                    session_id,
                    max_items.max(1) as i64,
                    max_item_bytes.max(1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)? as usize,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        let mut items = Vec::with_capacity(rows.len());
        let mut used_bytes = 0usize;
        let mut omitted = 0usize;
        for (role, content, kind, metadata, actual_bytes) in rows {
            if actual_bytes > max_item_bytes || content.len() > max_item_bytes {
                omitted += 1;
                continue;
            }
            let item = decode_conversation_item(&role, content, &kind, metadata)?;
            let item_bytes = conversation_item_bytes(&item);
            if used_bytes.saturating_add(item_bytes) > max_bytes {
                omitted += 1;
                continue;
            }
            used_bytes = used_bytes.saturating_add(item_bytes);
            items.push(item);
        }
        items.reverse();
        if omitted > 0 {
            items.insert(
                0,
                ConversationItem::Message {
                    role: Role::System,
                    content: format!(
                        "Earlier history was omitted during bounded recovery ({omitted} items)."
                    ),
                },
            );
        }
        Ok(items)
    }

    /// Returns one page of raw message rows along the current head chain,
    /// newest-first (the caller reverses for display order).
    ///
    /// `before` is an opaque cursor (a message `id`): only rows strictly older
    /// than it are returned. Pass `limit + 1` to detect `has_more` without a
    /// separate count query. The `session_id + hidden + id` index makes this
    /// index-only.
    pub fn load_message_page(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "WITH RECURSIVE chain(id) AS (
                 SELECT head_turn_id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                 WHERE turns.parent_id IS NOT NULL
             )
             SELECT id, role, content, kind, metadata, created_at FROM messages
             WHERE session_id = ?1 AND hidden = 0 AND partial = 0
               AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
               AND (?2 IS NULL OR id < ?2)
             ORDER BY id DESC
             LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![session_id, before, limit as i64], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    kind: row.get(3)?,
                    metadata: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn begin_tool(
        &self,
        session_id: &str,
        call: &ToolCall,
        decision: &str,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "INSERT OR REPLACE INTO tool_calls(id, session_id, name, arguments, decision, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                call.id,
                session_id,
                call.name,
                call.arguments.to_string(),
                decision,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn finish_tool(&self, call_id: &str, result: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE tool_calls SET result = ?2, finished_at = ?3 WHERE id = ?1",
            params![call_id, result, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Returns the recorded policy decision for a tool call, if any.
    pub fn tool_decision(&self, call_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT decision FROM tool_calls WHERE id = ?1",
                [call_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn save_response_id(
        &self,
        session_id: &str,
        response_id: &str,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "INSERT INTO provider_state(session_id, response_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET response_id = excluded.response_id, updated_at = excluded.updated_at",
            params![session_id, response_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn response_id(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT response_id FROM provider_state WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn clear_response_id(&self, session_id: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }
}
