use chrono::Utc;
use rusqlite::params;

use super::{MemoryRecord, Storage, StorageError, memory_from_row};

impl Storage {
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
}
