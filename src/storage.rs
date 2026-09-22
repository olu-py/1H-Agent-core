use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

mod schema;

use crate::{
    model::{TodoStatus, TodoTask},
    provider::{ConversationItem, Role, ToolCall},
};

#[derive(Clone)]
pub struct Storage {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
}

/// A single file snapshot captured around a mutating file tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    pub path: String,
    pub pre_image: Option<Vec<u8>>,
    pub post_image: Option<Vec<u8>>,
    /// Whether the file existed before the tool ran. `false` also marks a
    /// snapshot that exceeded the per-file limit and was skipped.
    pub existed: bool,
}

/// A raw message row returned by cursor pagination. `id` is the opaque cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub kind: String,
    pub metadata: Option<String>,
    pub created_at: String,
}

/// A user-visible long-term memory row. `recallable` is derived from the
/// current session/source graph; deleted or source-invalid rows remain
/// inspectable for review but are not injected into future requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRecord {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub topic: Option<String>,
    pub status: String,
    pub source_session_id: Option<String>,
    pub source_turn_id: Option<String>,
    pub source_message_id: Option<i64>,
    pub evidence: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub confirmed_at: Option<String>,
    pub recallable: bool,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("storage lock is poisoned")]
    Poisoned,
    #[error("invalid todo task: {0}")]
    InvalidTodo(String),
    #[error("memory limit: {0}")]
    MemoryLimit(String),
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                StorageError::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        Ok(Self {
            connection: Arc::new(Mutex::new(schema::initialize(connection)?)),
        })
    }

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
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, parent_id, head_turn_id, child_role) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            "SELECT id, title, parent_id FROM sessions WHERE workspace = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC, created_at DESC",
        )?;
        let rows = statement.query_map([workspace.display().to_string()], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                parent_id: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
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

    pub fn undo(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let head: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(head) = head else { return Ok(false) };
        let parent: Option<String> = connection.query_row(
            "SELECT parent_id FROM turns WHERE id = ?1",
            [&head],
            |row| row.get(0),
        )?;
        let Some(parent) = parent else {
            return Ok(false);
        };
        connection.execute(
            "UPDATE sessions SET head_turn_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, parent, Utc::now().to_rfc3339()],
        )?;
        Ok(true)
    }

    pub fn redo(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let head: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(head) = head else { return Ok(false) };
        let child: Option<String> = connection
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 AND parent_id = ?2 ORDER BY created_at DESC LIMIT 1",
                params![session_id, head],
                |row| row.get(0),
            )
            .optional()?;
        let Some(child) = child else { return Ok(false) };
        connection.execute(
            "UPDATE sessions SET head_turn_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, child, Utc::now().to_rfc3339()],
        )?;
        Ok(true)
    }

    /// The maximum bytes of a single file snapshot. Files larger than this are
    /// recorded as a marker row (pre/post images NULL, existed=0) so undo/redo
    /// can tell the user the file was not snapshot rather than silently
    /// skipping it.
    pub fn snapshot_file_limit(&self, max_file_bytes: usize) -> usize {
        max_file_bytes
    }

    /// Records a pre-execution snapshot for `path` under `turn_id`. `pre_image`
    /// is `None` when the file did not exist before the tool ran (`existed=0`).
    /// Snapshots at or above the per-file limit are stored as markers so the
    /// undo path can report "exceeds snapshot limit". Per-session total bytes
    /// are enforced here (oldest rows are dropped first) in the same write
    /// transaction as the insert.
    #[allow(clippy::too_many_arguments)]
    pub fn snapshot_file(
        &self,
        session_id: &str,
        turn_id: &str,
        tool_call_id: &str,
        path: &str,
        pre_image: Option<&[u8]>,
        existed: bool,
        max_file_bytes: usize,
        max_session_bytes: usize,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        if let Some(pre_image) = pre_image {
            if pre_image.len() > max_file_bytes {
                // Marker row: too large to snapshot, existed=0 signals "skip".
                tx.execute(
                    "INSERT INTO file_snapshots(session_id, turn_id, tool_call_id, path, pre_image, post_image, existed, created_at) VALUES (?1, ?2, ?3, ?4, NULL, NULL, 0, ?5)",
                    params![session_id, turn_id, tool_call_id, path, Utc::now().to_rfc3339()],
                )?;
                tx.commit()?;
                return Ok(());
            }
        }
        tx.execute(
            "INSERT INTO file_snapshots(session_id, turn_id, tool_call_id, path, pre_image, post_image, existed, created_at) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
            params![
                session_id,
                turn_id,
                tool_call_id,
                path,
                pre_image.map(<[u8]>::to_vec),
                existed as i64,
                Utc::now().to_rfc3339()
            ],
        )?;
        // Enforce the per-session total byte cap: drop the oldest rows until
        // the sum of pre_image bytes fits. Rows are small; the cap is on
        // pre_image bytes which dominate, so deleting one row at a time keeps
        // the accounting exact.
        loop {
            let total: i64 = tx.query_row(
                "SELECT COALESCE(SUM(LENGTH(pre_image)), 0) FROM file_snapshots WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )?;
            if total as usize <= max_session_bytes {
                break;
            }
            let removed = tx.execute(
                "DELETE FROM file_snapshots WHERE session_id = ?1 AND id = (SELECT MIN(id) FROM file_snapshots WHERE session_id = ?1)",
                [session_id],
            )?;
            if removed == 0 {
                break;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Backfills the post-execution image for a snapshot recorded by
    /// `snapshot_file`. `post_image` is `None` when the file no longer exists
    /// after the tool ran (deleted).
    pub fn save_post_image(
        &self,
        tool_call_id: &str,
        post_image: Option<&[u8]>,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE file_snapshots SET post_image = ?2 WHERE tool_call_id = ?1",
            params![tool_call_id, post_image.map(<[u8]>::to_vec)],
        )?;
        Ok(())
    }

    /// Returns the snapshots recorded for a single turn, keyed by path. Used by
    /// undo/redo to roll files back or forward.
    pub fn restore_turn_files(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Vec<FileSnapshot>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT path, pre_image, post_image, existed FROM file_snapshots WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id ASC",
        )?;
        let rows = statement
            .query_map(params![session_id, turn_id], |row| {
                Ok(FileSnapshot {
                    path: row.get(0)?,
                    pre_image: row.get(1)?,
                    post_image: row.get(2)?,
                    existed: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns the chain of turn ids strictly after `from_turn` and up to and
    /// including `to_turn` following parent links. Order is from oldest to
    /// newest. Used by undo/redo to enumerate the snapshot turns being
    /// detached/reattached.
    pub fn turns_between(
        &self,
        session_id: &str,
        from_turn: &str,
        to_turn: &str,
    ) -> Result<Vec<String>, StorageError> {
        let connection = self.lock()?;
        let mut turns = Vec::new();
        let mut cursor = Some(to_turn.to_owned());
        while let Some(current) = cursor {
            let (parent,): (Option<String>,) = connection.query_row(
                "SELECT parent_id FROM turns WHERE session_id = ?1 AND id = ?2",
                params![session_id, current],
                |row| Ok((row.get(0)?,)),
            )?;
            if parent.as_deref() == Some(from_turn) {
                turns.push(current);
                break;
            }
            turns.push(current);
            cursor = parent;
        }
        turns.reverse();
        Ok(turns)
    }

    /// Deletes snapshot rows for sessions that were soft-deleted. Called lazily
    /// after `delete_session`; keeps the global snapshot footprint bounded.
    pub fn purge_soft_deleted_snapshots(&self) -> Result<usize, StorageError> {
        self.lock()?.execute(
            "DELETE FROM file_snapshots WHERE session_id IN (SELECT id FROM sessions WHERE deleted_at IS NOT NULL)",
            [],
        )?;
        Ok(0)
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

    pub fn list_memories(
        &self,
        workspace: &str,
        query: Option<&str>,
        include_deleted: bool,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let connection = self.lock()?;
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let pattern = query.map(|value| format!("%{}%", value.replace('%', "\\%")));
        let status_clause = if include_deleted {
            "1 = 1"
        } else {
            "e.status <> 'deleted'"
        };
        let mut statement = connection.prepare(&format!(
            "SELECT e.id, e.kind, e.title, e.content, e.topic, e.status,
                    e.created_at, e.updated_at, e.confirmed_at,
                    ms.session_id, ms.turn_id, ms.message_id, ms.evidence,
                    CASE WHEN e.status = 'active' AND NOT EXISTS (
                        SELECT 1 FROM memory_sources invalid_ms
                        LEFT JOIN sessions invalid_s ON invalid_s.id = invalid_ms.session_id
                        LEFT JOIN messages invalid_m ON invalid_m.id = invalid_ms.message_id
                        WHERE invalid_ms.memory_id = e.id
                          AND (
                              (invalid_ms.session_id IS NOT NULL AND
                               (invalid_s.id IS NULL OR invalid_s.deleted_at IS NOT NULL
                                OR invalid_s.workspace <> e.workspace))
                              OR (invalid_ms.message_id IS NOT NULL AND
                                  (invalid_m.id IS NULL OR invalid_m.hidden <> 0 OR invalid_m.partial <> 0))
                              OR (invalid_ms.turn_id IS NOT NULL AND NOT EXISTS (
                                  WITH RECURSIVE source_chain(id) AS (
                                      SELECT head_turn_id FROM sessions
                                      WHERE id = invalid_ms.session_id
                                      UNION ALL
                                      SELECT turns.parent_id FROM turns
                                      JOIN source_chain ON turns.id = source_chain.id
                                      WHERE turns.parent_id IS NOT NULL
                                  )
                                  SELECT 1 FROM source_chain
                                  WHERE id = invalid_ms.turn_id
                              ))
                          )
                    ) THEN 1 ELSE 0 END AS recallable
             FROM memory_entries e
             LEFT JOIN memory_sources ms ON ms.id = (
                 SELECT source.id FROM memory_sources source
                 WHERE source.memory_id = e.id ORDER BY source.id LIMIT 1
             )
             WHERE e.workspace = ?1 AND {status_clause}
               AND (?2 IS NULL OR e.title LIKE ?2 ESCAPE '\\'
                    OR e.content LIKE ?2 ESCAPE '\\'
                    OR COALESCE(e.topic, '') LIKE ?2 ESCAPE '\\')
             ORDER BY CASE e.status WHEN 'candidate' THEN 0 WHEN 'active' THEN 1 ELSE 2 END,
                      e.updated_at DESC, e.id DESC
             LIMIT 512"
        ))?;
        statement
            .query_map(params![workspace, pattern], memory_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_memory(
        &self,
        workspace: &str,
        kind: &str,
        title: &str,
        content: &str,
        topic: Option<&str>,
        status: &str,
        source_session_id: Option<&str>,
        source_turn_id: Option<&str>,
        source_message_id: Option<i64>,
        evidence: Option<&str>,
        max_entries: usize,
        max_candidates: usize,
        max_entry_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<MemoryRecord, StorageError> {
        let title = title.trim();
        let content = content.trim();
        if title.is_empty() || content.is_empty() {
            return Err(StorageError::MemoryLimit(
                "memory title and content are required".into(),
            ));
        }
        if title.len().saturating_add(content.len()) > max_entry_bytes {
            return Err(StorageError::MemoryLimit(format!(
                "memory entry exceeds {max_entry_bytes} bytes"
            )));
        }
        let status = if status == "candidate" {
            "candidate"
        } else {
            "active"
        };
        let connection = self.lock()?;
        let (count, candidates, total_bytes): (i64, i64, i64) = connection.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN status = 'candidate' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(length(title) + length(content)), 0)
             FROM memory_entries WHERE workspace = ?1 AND status <> 'deleted'",
            [workspace],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if count as usize >= max_entries {
            return Err(StorageError::MemoryLimit(
                "memory capacity is full; review or delete existing entries".into(),
            ));
        }
        if status == "candidate" && candidates as usize >= max_candidates {
            return Err(StorageError::MemoryLimit(
                "memory candidate queue is full; review candidates first".into(),
            ));
        }
        if (total_bytes as usize).saturating_add(title.len() + content.len()) > max_total_bytes {
            return Err(StorageError::MemoryLimit(
                "memory storage capacity is full; review or delete existing entries".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let confirmed_at = (status == "active").then_some(now.as_str());
        connection.execute(
            "INSERT INTO memory_entries
             (workspace, kind, title, content, topic, status, created_at, updated_at, confirmed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
            params![
                workspace,
                kind,
                title,
                content,
                topic,
                status,
                now,
                confirmed_at
            ],
        )?;
        let id = connection.last_insert_rowid();
        if source_session_id.is_some()
            || source_turn_id.is_some()
            || source_message_id.is_some()
            || evidence.is_some()
        {
            connection.execute(
                "INSERT INTO memory_sources(memory_id, session_id, turn_id, message_id, evidence)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    source_session_id,
                    source_turn_id,
                    source_message_id,
                    evidence
                ],
            )?;
        }
        self.memory_by_id_locked(&connection, id)
    }

    pub fn confirm_memory(&self, workspace: &str, id: i64) -> Result<MemoryRecord, StorageError> {
        let connection = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let changed = connection.execute(
            "UPDATE memory_entries SET status = 'active', updated_at = ?3, confirmed_at = ?3
             WHERE id = ?1 AND workspace = ?2 AND status <> 'deleted'",
            params![id, workspace, now],
        )?;
        if changed == 0 {
            return Err(StorageError::MemoryLimit("memory entry not found".into()));
        }
        self.memory_by_id_locked(&connection, id)
    }

    pub fn update_memory(
        &self,
        workspace: &str,
        id: i64,
        title: &str,
        content: &str,
        max_entry_bytes: usize,
    ) -> Result<MemoryRecord, StorageError> {
        let title = title.trim();
        let content = content.trim();
        if title.is_empty() || content.is_empty() {
            return Err(StorageError::MemoryLimit(
                "memory title and content are required".into(),
            ));
        }
        if title.len().saturating_add(content.len()) > max_entry_bytes {
            return Err(StorageError::MemoryLimit(format!(
                "memory entry exceeds {max_entry_bytes} bytes"
            )));
        }
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE memory_entries SET title = ?3, content = ?4, updated_at = ?5
             WHERE id = ?1 AND workspace = ?2 AND status <> 'deleted'",
            params![id, workspace, title, content, Utc::now().to_rfc3339()],
        )?;
        if changed == 0 {
            return Err(StorageError::MemoryLimit("memory entry not found".into()));
        }
        self.memory_by_id_locked(&connection, id)
    }

    pub fn delete_memory(&self, workspace: &str, id: i64) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE memory_entries SET status = 'deleted', updated_at = ?3
             WHERE id = ?1 AND workspace = ?2 AND status <> 'deleted'",
            params![id, workspace, Utc::now().to_rfc3339()],
        )?;
        if changed == 0 {
            return Err(StorageError::MemoryLimit("memory entry not found".into()));
        }
        Ok(())
    }

    pub fn recall_memories(
        &self,
        workspace: &str,
        query: Option<&str>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let records = self.list_memories(workspace, query, false)?;
        let mut used = 0usize;
        Ok(records
            .into_iter()
            .filter(|record| record.recallable)
            .take_while(|record| {
                let bytes = record.title.len().saturating_add(record.content.len());
                if used.saturating_add(bytes) > max_bytes {
                    return false;
                }
                used = used.saturating_add(bytes);
                true
            })
            .take(limit.max(1))
            .collect())
    }

    fn memory_by_id_locked(
        &self,
        connection: &rusqlite::Connection,
        id: i64,
    ) -> Result<MemoryRecord, StorageError> {
        connection
            .query_row(
                "SELECT e.id, e.kind, e.title, e.content, e.topic, e.status,
                        e.created_at, e.updated_at, e.confirmed_at,
                        ms.session_id, ms.turn_id, ms.message_id, ms.evidence,
                        CASE WHEN e.status = 'active' AND NOT EXISTS (
                            SELECT 1 FROM memory_sources invalid_ms
                            LEFT JOIN sessions invalid_s ON invalid_s.id = invalid_ms.session_id
                            LEFT JOIN messages invalid_m ON invalid_m.id = invalid_ms.message_id
                            WHERE invalid_ms.memory_id = e.id
                              AND (
                                  (invalid_ms.session_id IS NOT NULL AND
                                   (invalid_s.id IS NULL OR invalid_s.deleted_at IS NOT NULL
                                    OR invalid_s.workspace <> e.workspace))
                                  OR (invalid_ms.message_id IS NOT NULL AND
                                      (invalid_m.id IS NULL OR invalid_m.hidden <> 0 OR invalid_m.partial <> 0))
                                  OR (invalid_ms.turn_id IS NOT NULL AND NOT EXISTS (
                                      WITH RECURSIVE source_chain(id) AS (
                                          SELECT head_turn_id FROM sessions
                                          WHERE id = invalid_ms.session_id
                                          UNION ALL
                                          SELECT turns.parent_id FROM turns
                                          JOIN source_chain ON turns.id = source_chain.id
                                          WHERE turns.parent_id IS NOT NULL
                                      )
                                      SELECT 1 FROM source_chain
                                      WHERE id = invalid_ms.turn_id
                                  ))
                              )
                        ) THEN 1 ELSE 0 END
                 FROM memory_entries e
                 LEFT JOIN memory_sources ms ON ms.id = (
                     SELECT source.id FROM memory_sources source
                     WHERE source.memory_id = e.id ORDER BY source.id LIMIT 1
                 )
                 WHERE e.id = ?1",
                [id],
                memory_from_row,
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

    /// Upserts one model-metadata cache row (`fetched_at` = now). `payload`
    /// carries the serialized model-id list for `provider-list|…` keys.
    pub fn save_model_metadata(
        &self,
        key: &str,
        source: &str,
        context_window_tokens: Option<u64>,
        max_output_tokens: Option<u32>,
        payload: Option<&str>,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "INSERT INTO model_metadata(key, source, context_window_tokens, max_output_tokens, payload, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET
                source = excluded.source,
                context_window_tokens = excluded.context_window_tokens,
                max_output_tokens = excluded.max_output_tokens,
                payload = excluded.payload,
                fetched_at = excluded.fetched_at",
            params![
                key,
                source,
                context_window_tokens,
                max_output_tokens,
                payload,
                Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }

    /// Upserts many per-model metadata rows in one transaction (used for a
    /// models.dev refresh). Rows with no metadata at all are skipped.
    pub fn save_model_metadata_batch(
        &self,
        source: &str,
        rows: &[(String, Option<u64>, Option<u32>)],
    ) -> Result<usize, StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let now = Utc::now().timestamp();
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO model_metadata(key, source, context_window_tokens, max_output_tokens, payload, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)
                 ON CONFLICT(key) DO UPDATE SET
                    source = excluded.source,
                    context_window_tokens = excluded.context_window_tokens,
                    max_output_tokens = excluded.max_output_tokens,
                    payload = excluded.payload,
                    fetched_at = excluded.fetched_at",
            )?;
            for (key, context_window_tokens, max_output_tokens) in rows {
                if context_window_tokens.is_none() && max_output_tokens.is_none() {
                    continue;
                }
                statement.execute(params![
                    key,
                    source,
                    context_window_tokens,
                    max_output_tokens,
                    now
                ])?;
                written += 1;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Reads one cache row. Errors are meant to be treated as a miss by
    /// callers (the metadata chain falls through to the snapshot/registry).
    pub fn model_metadata(&self, key: &str) -> Result<Option<ModelMetadataRow>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT source, context_window_tokens, max_output_tokens, payload, fetched_at
                 FROM model_metadata WHERE key = ?1",
                [key],
                |row| {
                    Ok(ModelMetadataRow {
                        source: row.get(0)?,
                        context_window_tokens: row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                        max_output_tokens: row
                            .get::<_, Option<i64>>(2)?
                            .and_then(|v| u32::try_from(v).ok()),
                        payload: row.get(3)?,
                        fetched_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)
    }
}

fn decode_conversation_item(
    role: &str,
    content: String,
    kind: &str,
    metadata: Option<String>,
) -> Result<ConversationItem, StorageError> {
    match kind {
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
            role: match role {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                _ => Role::User,
            },
            content,
        }),
    }
}

fn memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    Ok(MemoryRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        topic: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        confirmed_at: row.get(8)?,
        source_session_id: row.get(9)?,
        source_turn_id: row.get(10)?,
        source_message_id: row.get(11)?,
        evidence: row.get(12)?,
        recallable: row.get::<_, i64>(13)? != 0,
    })
}

fn conversation_item_bytes(item: &ConversationItem) -> usize {
    match item {
        ConversationItem::Message { content, .. }
        | ConversationItem::Context { content, .. }
        | ConversationItem::CompactionSummary { content }
        | ConversationItem::ThinkingSummary { content } => content.len(),
        ConversationItem::ProviderItem { item } => item.to_string().len(),
        ConversationItem::AssistantToolCalls { calls } => calls
            .iter()
            .map(|call| {
                call.name
                    .len()
                    .saturating_add(call.arguments.to_string().len())
            })
            .sum(),
        ConversationItem::ToolOutput { call_id, output } => {
            call_id.len().saturating_add(output.len())
        }
    }
}

/// One cached model-metadata row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMetadataRow {
    pub source: String,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub payload: Option<String>,
    pub fetched_at: i64,
}

fn append_message_on_turn(
    connection: &Connection,
    session_id: &str,
    turn_id: &str,
    role: Role,
    content: &str,
) -> Result<(), StorageError> {
    let role_name = match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let now = Utc::now().to_rfc3339();
    connection.execute(
        "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden) VALUES (?1, ?2, ?3, ?4, ?5, 'message', 0)",
        params![session_id, role_name, content, now, turn_id],
    )?;
    connection.execute(
        "UPDATE sessions SET updated_at = ?2, title = CASE WHEN title = 'New session' AND ?3 = 'user' THEN substr(?4, 1, 80) ELSE title END WHERE id = ?1",
        params![session_id, now, role_name, content],
    )?;
    Ok(())
}

fn parse_todo_status(value: String) -> Result<TodoStatus, rusqlite::Error> {
    match value.as_str() {
        "pending" => Ok(TodoStatus::Pending),
        "in_progress" => Ok(TodoStatus::InProgress),
        "done" => Ok(TodoStatus::Done),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::fmt::Error),
        )),
    }
}

fn validate_todo_tasks(tasks: &[TodoTask]) -> Result<(), StorageError> {
    if tasks.len() > 50 {
        return Err(StorageError::InvalidTodo("at most 50 tasks".into()));
    }
    let mut ids = std::collections::HashSet::with_capacity(tasks.len());
    for task in tasks {
        let title = task.title.trim();
        let title_chars = title.chars().count();
        if title_chars == 0 || title_chars > 240 {
            return Err(StorageError::InvalidTodo(
                "task title must contain 1 to 240 characters".into(),
            ));
        }
        if !ids.insert(task.id.as_str()) {
            return Err(StorageError::InvalidTodo("duplicate task id".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
