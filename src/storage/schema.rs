use chrono::Utc;
use rusqlite::{Connection, params};
use uuid::Uuid;

use super::StorageError;

const VISIBLE_HISTORY_TRIGGERS: &str = r#"
CREATE TRIGGER IF NOT EXISTS messages_visible_history_insert
AFTER INSERT ON messages
WHEN NEW.hidden = 0 AND NEW.partial = 0
     AND (NEW.turn_id IS NULL OR EXISTS (
         SELECT 1 FROM turns WHERE id = NEW.turn_id AND on_active_path = 1
     ))
BEGIN
    INSERT OR IGNORE INTO visible_history(session_id, message_id)
    VALUES (NEW.session_id, NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS messages_visible_history_update
AFTER UPDATE OF session_id, turn_id, hidden, partial ON messages
BEGIN
    DELETE FROM visible_history WHERE message_id = OLD.id;
    INSERT OR IGNORE INTO visible_history(session_id, message_id)
    SELECT NEW.session_id, NEW.id
    WHERE NEW.hidden = 0 AND NEW.partial = 0
      AND (NEW.turn_id IS NULL OR EXISTS (
          SELECT 1 FROM turns WHERE id = NEW.turn_id AND on_active_path = 1
      ));
END;
CREATE TRIGGER IF NOT EXISTS messages_visible_history_delete
AFTER DELETE ON messages
BEGIN
    DELETE FROM visible_history WHERE message_id = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS turns_visible_history_path
AFTER UPDATE OF on_active_path ON turns
WHEN OLD.on_active_path != NEW.on_active_path
BEGIN
    DELETE FROM visible_history WHERE message_id IN (
        SELECT id FROM messages
        WHERE session_id = NEW.session_id AND turn_id = NEW.id
    );
    INSERT OR IGNORE INTO visible_history(session_id, message_id)
    SELECT session_id, id FROM messages
    WHERE session_id = NEW.session_id AND turn_id = NEW.id
      AND hidden = 0 AND partial = 0
      AND NEW.on_active_path = 1;
END;
"#;

pub(super) fn initialize(connection: Connection) -> Result<Connection, StorageError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            workspace TEXT NOT NULL,
            title TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            mode TEXT NOT NULL DEFAULT 'build',
            provider TEXT NOT NULL DEFAULT 'openai',
            model TEXT NOT NULL DEFAULT '',
            parent_id TEXT,
            deleted_at TEXT,
            head_turn_id TEXT
        );
        CREATE TABLE IF NOT EXISTS turns (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            parent_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
            created_at TEXT NOT NULL,
            on_active_path INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL,
            turn_id TEXT,
            kind TEXT NOT NULL DEFAULT 'message',
            hidden INTEGER NOT NULL DEFAULT 0,
            metadata TEXT
        );
        CREATE TABLE IF NOT EXISTS tool_calls (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            arguments TEXT NOT NULL,
            decision TEXT NOT NULL,
            result TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT
        );
        CREATE TABLE IF NOT EXISTS provider_state (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            response_id TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS compactions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            hidden_ids TEXT NOT NULL,
            summary TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS session_tasks (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            position INTEGER NOT NULL,
            title TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS file_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            turn_id TEXT,
            tool_call_id TEXT,
            path TEXT NOT NULL,
            pre_image BLOB,
            post_image BLOB,
            existed INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL
        );
        -- Model metadata cache: provider `/models` and models.dev rows.
        -- `key` is 'provider-list|{base_url}' (payload = the full id
        -- list), 'provider|{base_url}|{model}', or 'community|{model}'.
        -- TTL is judged by the reader; cached values stay usable until a
        -- newer fetch replaces them.
        CREATE TABLE IF NOT EXISTS model_metadata (
            key TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            context_window_tokens INTEGER,
            max_output_tokens INTEGER,
            payload TEXT,
            fetched_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS memory_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace TEXT NOT NULL,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            content TEXT NOT NULL,
            topic TEXT,
            status TEXT NOT NULL DEFAULT 'candidate',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            confirmed_at TEXT
        );
        CREATE TABLE IF NOT EXISTS memory_sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id INTEGER NOT NULL REFERENCES memory_entries(id) ON DELETE CASCADE,
            session_id TEXT,
            turn_id TEXT,
            message_id INTEGER,
            evidence TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_memory_entries_workspace_status
            ON memory_entries(workspace, status, updated_at);
        CREATE INDEX IF NOT EXISTS idx_memory_sources_memory
            ON memory_sources(memory_id);
        -- Cursor pagination reads messages newest-first along the head
        -- chain; the session_id+hidden+id index keeps that query index-only.
        CREATE INDEX IF NOT EXISTS idx_messages_session_hidden_id
            ON messages(session_id, hidden, id);
        CREATE TABLE IF NOT EXISTS visible_history (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            PRIMARY KEY (session_id, message_id)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_messages_session_turn_id
            ON messages(session_id, turn_id, id);
        CREATE INDEX IF NOT EXISTS idx_turns_session_parent_created
            ON turns(session_id, parent_id, created_at DESC);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (1, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (2, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (3, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (4, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (5, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (6, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (7, CURRENT_TIMESTAMP);
        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (8, CURRENT_TIMESTAMP);
        ",
    )?;
    // These checks keep databases created by the first release compatible
    // without relying on SQLite's optional ALTER TABLE syntax extensions.
    ensure_column(
        &connection,
        "sessions",
        "mode",
        "TEXT NOT NULL DEFAULT 'build'",
    )?;
    ensure_column(
        &connection,
        "sessions",
        "provider",
        "TEXT NOT NULL DEFAULT 'openai'",
    )?;
    ensure_column(&connection, "sessions", "model", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(&connection, "sessions", "parent_id", "TEXT")?;
    ensure_column(&connection, "sessions", "deleted_at", "TEXT")?;
    ensure_column(&connection, "sessions", "head_turn_id", "TEXT")?;
    ensure_column(
        &connection,
        "turns",
        "on_active_path",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(&connection, "sessions", "child_role", "TEXT")?;
    ensure_column(&connection, "sessions", "child_status", "TEXT")?;
    ensure_column(&connection, "sessions", "child_allowed_tools", "TEXT")?;
    ensure_column(&connection, "messages", "turn_id", "TEXT")?;
    ensure_column(
        &connection,
        "messages",
        "kind",
        "TEXT NOT NULL DEFAULT 'message'",
    )?;
    ensure_column(
        &connection,
        "messages",
        "hidden",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(&connection, "messages", "metadata", "TEXT")?;
    // `assistant_partial`: a persisted incomplete assistant answer (0/1).
    // Partial rows are written on interruption/failure/cancel, cleared on
    // normal completion, and filtered out of the normal history page so a
    // partial never re-enters context or previous_response_id replay.
    ensure_column(
        &connection,
        "messages",
        "partial",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Snapshots read the partial answer even when it is absent. Restrict the
    // lookup to partial rows instead of scanning a session's entire history.
    let tx = connection.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_partial_session_id
         ON messages(session_id, id DESC) WHERE partial = 1;",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (10, CURRENT_TIMESTAMP)",
        [],
    )?;
    tx.commit()?;
    migrate_visible_history(&connection)?;
    connection.execute_batch(VISIBLE_HISTORY_TRIGGERS)?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (3, CURRENT_TIMESTAMP)",
        [],
    )?;
    Ok(connection)
}

fn migrate_visible_history(connection: &Connection) -> Result<(), StorageError> {
    let applied: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = 9)",
        [],
        |row| row.get(0),
    )?;
    if applied {
        return Ok(());
    }

    let tx = connection.unchecked_transaction()?;
    // Backfill state set-wise: running the live path trigger once per turn
    // would rewrite every visible message several times during migration.
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS messages_visible_history_insert;
         DROP TRIGGER IF EXISTS messages_visible_history_update;
         DROP TRIGGER IF EXISTS messages_visible_history_delete;
         DROP TRIGGER IF EXISTS turns_visible_history_path;",
    )?;
    // Version 9 also absorbs the old per-open turn backfill so both legacy
    // repair and the new projection are committed exactly once.
    backfill_turns(&tx)?;
    tx.execute("UPDATE turns SET on_active_path = 0", [])?;
    tx.execute(
        "WITH RECURSIVE active(session_id, id) AS (
             SELECT id, head_turn_id FROM sessions WHERE head_turn_id IS NOT NULL
             UNION ALL
             SELECT active.session_id, turns.parent_id
             FROM turns JOIN active ON turns.id = active.id
             WHERE turns.parent_id IS NOT NULL
         )
         UPDATE turns SET on_active_path = 1
         WHERE id IN (SELECT id FROM active)",
        [],
    )?;
    tx.execute("DELETE FROM visible_history", [])?;
    tx.execute(
        "INSERT INTO visible_history(session_id, message_id)
         SELECT messages.session_id, messages.id FROM messages
         LEFT JOIN turns ON turns.id = messages.turn_id
         WHERE messages.hidden = 0 AND messages.partial = 0
           AND (messages.turn_id IS NULL OR turns.on_active_path = 1)",
        [],
    )?;
    tx.execute_batch(VISIBLE_HISTORY_TRIGGERS)?;
    tx.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (9, ?1)",
        [Utc::now().to_rfc3339()],
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StorageError> {
    let exists = connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    if !exists {
        connection.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn backfill_turns(connection: &Connection) -> Result<(), StorageError> {
    let sessions = {
        let mut statement = connection.prepare("SELECT id, head_turn_id FROM sessions")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (session_id, head) in sessions {
        let turn_id = if let Some(head) = head {
            head
        } else {
            let turn_id = Uuid::new_v4().to_string();
            connection.execute(
                "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
                params![turn_id, session_id, Utc::now().to_rfc3339()],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, turn_id],
            )?;
            turn_id
        };
        connection.execute(
            "UPDATE messages SET turn_id = ?2 WHERE session_id = ?1 AND turn_id IS NULL",
            params![session_id, turn_id],
        )?;
    }
    Ok(())
}
