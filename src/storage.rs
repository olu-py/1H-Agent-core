use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

mod memory;
mod schema;
mod session;
mod snapshot;

use crate::{
    model::{TodoStatus, TodoTask},
    provider::{ConversationItem, Role},
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
