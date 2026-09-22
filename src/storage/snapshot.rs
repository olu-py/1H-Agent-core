use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use super::{FileSnapshot, Storage, StorageError};

impl Storage {
    pub fn undo(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let head: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
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
                |row| row.get::<_, Option<String>>(0),
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

    /// The maximum bytes of a single file snapshot.
    pub fn snapshot_file_limit(&self, max_file_bytes: usize) -> usize {
        max_file_bytes
    }

    /// Records the pre-execution image and enforces the per-session byte cap.
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

    /// Backfills the image after the tool runs.
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

    /// Returns the snapshots recorded for a single turn.
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

    /// Returns turn ids in parent order between two turns.
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

    /// Deletes snapshots for sessions that were soft-deleted.
    pub fn purge_soft_deleted_snapshots(&self) -> Result<usize, StorageError> {
        self.lock()?.execute(
            "DELETE FROM file_snapshots WHERE session_id IN (SELECT id FROM sessions WHERE deleted_at IS NOT NULL)",
            [],
        )?;
        Ok(0)
    }
}
