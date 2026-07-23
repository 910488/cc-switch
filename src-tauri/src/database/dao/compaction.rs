//! Encrypted Codex continuity journal persistence.
//!
//! This DAO deliberately treats encrypted payloads as opaque bytes. Encryption,
//! envelope validation and canonicalization belong to `proxy::compaction` so no
//! database or sync path ever needs plaintext conversation history.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub(crate) struct CompactionSnapshotRow {
    pub id: String,
    pub thread_id: String,
    pub session_id: String,
    pub request_id: String,
    pub source_model: String,
    pub parent_id: Option<String>,
    pub token_estimate: i64,
    pub payload_hash: String,
    pub envelope_version: i64,
    pub key_version: i64,
    pub payload_blob: Vec<u8>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CompactionRow {
    pub id: String,
    pub snapshot_id: String,
    pub realm: String,
    pub source_model: String,
    pub envelope_version: i64,
    pub key_version: i64,
    pub item_blob: Vec<u8>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct TaskStateRow {
    pub thread_id: String,
    pub session_id: String,
    pub state_blob: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompactionCounts {
    pub snapshots: i64,
    pub compactions: i64,
    pub migrations: i64,
    pub task_states: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct QuotaLatchRow {
    pub app_type: String,
    pub provider_id: String,
    pub account_id: String,
    pub quota_kind: String,
    pub blocked_until: Option<String>,
    pub signal_hash: Option<String>,
    pub detail_blob: Option<Vec<u8>>,
    pub updated_at: String,
}

impl Database {
    pub(crate) fn insert_compaction_snapshot(
        &self,
        row: &CompactionSnapshotRow,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO compaction_snapshots
             (id, thread_id, session_id, request_id, source_model, parent_id,
              token_estimate, payload_hash, envelope_version, key_version, payload_blob, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.id,
                row.thread_id,
                row.session_id,
                row.request_id,
                row.source_model,
                row.parent_id,
                row.token_estimate,
                row.payload_hash,
                row.envelope_version,
                row.key_version,
                row.payload_blob,
                row.created_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("保存 compaction snapshot 失败: {e}")))?;
        Ok(())
    }

    pub(crate) fn get_compaction_snapshot_row(
        &self,
        id: &str,
    ) -> Result<Option<CompactionSnapshotRow>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT id, thread_id, session_id, request_id, source_model, parent_id,
                    token_estimate, payload_hash, envelope_version, key_version, payload_blob, created_at
             FROM compaction_snapshots WHERE id = ?1",
            [id],
            map_snapshot_row,
        )
        .optional()
        .map_err(|e| AppError::Database(format!("读取 compaction snapshot 失败: {e}")))
    }

    #[allow(dead_code)]
    pub(crate) fn latest_compaction_snapshot_row(
        &self,
        thread_id: &str,
        session_id: Option<&str>,
    ) -> Result<Option<CompactionSnapshotRow>, AppError> {
        let conn = lock_conn!(self.conn);
        let result = match session_id {
            Some(session_id) => conn
                .query_row(
                    "SELECT id, thread_id, session_id, request_id, source_model, parent_id,
                            token_estimate, payload_hash, envelope_version, key_version, payload_blob, created_at
                     FROM compaction_snapshots
                     WHERE thread_id = ?1 AND session_id = ?2
                     ORDER BY created_at DESC LIMIT 1",
                    params![thread_id, session_id],
                    map_snapshot_row,
                )
                .optional(),
            None => conn
                .query_row(
                    "SELECT id, thread_id, session_id, request_id, source_model, parent_id,
                            token_estimate, payload_hash, envelope_version, key_version, payload_blob, created_at
                     FROM compaction_snapshots
                     WHERE thread_id = ?1 ORDER BY created_at DESC LIMIT 1",
                    [thread_id],
                    map_snapshot_row,
                )
                .optional(),
        };
        result.map_err(|e| AppError::Database(format!("读取最新 compaction snapshot 失败: {e}")))
    }

    #[allow(dead_code)]
    pub(crate) fn upsert_compaction_row(&self, row: &CompactionRow) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        let inserted = conn.execute(
            "INSERT INTO compactions
             (id, snapshot_id, realm, source_model, envelope_version, key_version, item_blob, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO NOTHING",
            params![
                row.id,
                row.snapshot_id,
                row.realm,
                row.source_model,
                row.envelope_version,
                row.key_version,
                row.item_blob,
                row.created_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("保存 compaction item 失败: {e}")))?;
        if inserted == 0 {
            let existing: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT snapshot_id, realm, source_model FROM compactions WHERE id = ?1",
                    [&row.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| AppError::Database(format!("校验 compaction identity 失败: {e}")))?;
            if existing.as_ref()
                != Some(&(
                    row.snapshot_id.clone(),
                    row.realm.clone(),
                    row.source_model.clone(),
                ))
            {
                return Err(AppError::Database(format!(
                    "compaction id {} is already bound to different continuity state",
                    row.id
                )));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_compaction_and_task_state(
        &self,
        row: &CompactionRow,
        thread_id: &str,
        session_id: &str,
        compaction_id: &str,
        state_blob: &[u8],
        updated_at: &str,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction().map_err(|e| {
            AppError::Database(format!("cannot begin compaction commit transaction: {e}"))
        })?;
        let inserted = tx
            .execute(
                "INSERT INTO compactions
                 (id, snapshot_id, realm, source_model, envelope_version, key_version, item_blob, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO NOTHING",
                params![
                    row.id,
                    row.snapshot_id,
                    row.realm,
                    row.source_model,
                    row.envelope_version,
                    row.key_version,
                    row.item_blob,
                    row.created_at,
                ],
            )
            .map_err(|e| AppError::Database(format!("cannot persist compaction item: {e}")))?;
        if inserted == 0 {
            let existing: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT snapshot_id, realm, source_model FROM compactions WHERE id = ?1",
                    [&row.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| {
                    AppError::Database(format!("cannot verify compaction identity: {e}"))
                })?;
            if existing.as_ref()
                != Some(&(
                    row.snapshot_id.clone(),
                    row.realm.clone(),
                    row.source_model.clone(),
                ))
            {
                return Err(AppError::Database(format!(
                    "compaction id {} is already bound to different continuity state",
                    row.id
                )));
            }
        }
        tx.execute(
            "INSERT INTO compaction_task_states
             (thread_id, session_id, compaction_id, state_blob, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id, session_id) DO UPDATE SET
               compaction_id=excluded.compaction_id,
               state_blob=excluded.state_blob,
               updated_at=excluded.updated_at",
            params![thread_id, session_id, compaction_id, state_blob, updated_at],
        )
        .map_err(|e| AppError::Database(format!("cannot persist compaction task state: {e}")))?;
        tx.commit().map_err(|e| {
            AppError::Database(format!(
                "cannot commit compaction continuity transaction: {e}"
            ))
        })
    }

    pub(crate) fn get_compaction_row(&self, id: &str) -> Result<Option<CompactionRow>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT id, snapshot_id, realm, source_model, envelope_version, key_version, item_blob, created_at
             FROM compactions WHERE id = ?1",
            [id],
            |row| {
                Ok(CompactionRow {
                    id: row.get(0)?,
                    snapshot_id: row.get(1)?,
                    realm: row.get(2)?,
                    source_model: row.get(3)?,
                    envelope_version: row.get(4)?,
                    key_version: row.get(5)?,
                    item_blob: row.get(6)?,
                    created_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|e| AppError::Database(format!("读取 compaction item 失败: {e}")))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn upsert_compaction_migration(
        &self,
        source_compaction_id: &str,
        target_model: &str,
        target_realm: &str,
        target_provider_id: &str,
        prompt_version: i64,
        item_blob: &[u8],
        created_at: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO compaction_migrations
             (source_compaction_id, target_model, target_realm, target_provider_id,
              prompt_version, envelope_version, key_version, item_blob, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, ?6, ?7)
             ON CONFLICT(source_compaction_id, target_model, target_realm, target_provider_id, prompt_version)
             DO UPDATE SET item_blob=excluded.item_blob, created_at=excluded.created_at",
            params![source_compaction_id, target_model, target_realm, target_provider_id, prompt_version, item_blob, created_at],
        )
        .map_err(|e| AppError::Database(format!("保存 compaction migration 失败: {e}")))?;
        Ok(())
    }

    pub(crate) fn get_compaction_migration_blob(
        &self,
        source_compaction_id: &str,
        target_model: &str,
        target_realm: &str,
        target_provider_id: &str,
        prompt_version: i64,
    ) -> Result<Option<Vec<u8>>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT item_blob FROM compaction_migrations
             WHERE source_compaction_id = ?1 AND target_model = ?2 AND target_realm = ?3
               AND target_provider_id = ?4 AND prompt_version = ?5",
            // Provider and prompt version are part of the cache identity: summaries
            // produced by different gateways or prompt revisions are not equivalent.
            params![
                source_compaction_id,
                target_model,
                target_realm,
                target_provider_id,
                prompt_version
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| AppError::Database(format!("读取 compaction migration 失败: {e}")))
    }

    pub(crate) fn upsert_compaction_task_state(
        &self,
        thread_id: &str,
        session_id: &str,
        compaction_id: Option<&str>,
        state_blob: &[u8],
        updated_at: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO compaction_task_states
             (thread_id, session_id, compaction_id, state_blob, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id, session_id) DO UPDATE SET
               compaction_id=excluded.compaction_id,
               state_blob=excluded.state_blob,
               updated_at=excluded.updated_at",
            params![thread_id, session_id, compaction_id, state_blob, updated_at],
        )
        .map_err(|e| AppError::Database(format!("保存 compaction task state 失败: {e}")))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn recent_compaction_task_state_rows(
        &self,
        limit: usize,
    ) -> Result<Vec<TaskStateRow>, AppError> {
        let safe_limit = limit.clamp(1, 100) as i64;
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, session_id, state_blob
                 FROM compaction_task_states ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(|e| AppError::Database(format!("准备读取 compaction task state 失败: {e}")))?;
        let rows = stmt
            .query_map([safe_limit], |row| {
                Ok(TaskStateRow {
                    thread_id: row.get(0)?,
                    session_id: row.get(1)?,
                    state_blob: row.get(2)?,
                })
            })
            .map_err(|e| AppError::Database(format!("读取 compaction task state 失败: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(format!("解析 compaction task state 失败: {e}")))
    }

    pub(crate) fn compaction_task_state_row_by_compaction_id(
        &self,
        compaction_id: &str,
    ) -> Result<Option<TaskStateRow>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT thread_id, session_id, state_blob
             FROM compaction_task_states
             WHERE compaction_id = ?1
             LIMIT 1",
            [compaction_id],
            |row| {
                Ok(TaskStateRow {
                    thread_id: row.get(0)?,
                    session_id: row.get(1)?,
                    state_blob: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|e| {
            AppError::Database(format!(
                "failed to read compaction task state for {compaction_id}: {e}"
            ))
        })
    }

    pub(crate) fn compaction_counts(&self) -> Result<CompactionCounts, AppError> {
        let conn = lock_conn!(self.conn);
        let count = |table: &str| -> Result<i64, AppError> {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|e| AppError::Database(format!("统计 {table} 失败: {e}")))
        };
        Ok(CompactionCounts {
            snapshots: count("compaction_snapshots")?,
            compactions: count("compactions")?,
            migrations: count("compaction_migrations")?,
            task_states: count("compaction_task_states")?,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn delete_compaction_thread(&self, thread_id: &str) -> Result<usize, AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(format!("开始 compaction 清理事务失败: {e}")))?;
        tx.execute(
            "DELETE FROM compaction_migrations WHERE source_compaction_id IN (
                SELECT id FROM compactions WHERE snapshot_id IN (
                    SELECT id FROM compaction_snapshots WHERE thread_id = ?1
                )
            )",
            [thread_id],
        )?;
        tx.execute(
            "DELETE FROM compactions WHERE snapshot_id IN (
                SELECT id FROM compaction_snapshots WHERE thread_id = ?1
            )",
            [thread_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM compaction_snapshots WHERE thread_id = ?1",
            [thread_id],
        )?;
        tx.execute(
            "DELETE FROM compaction_task_states WHERE thread_id = ?1",
            [thread_id],
        )?;
        tx.commit()
            .map_err(|e| AppError::Database(format!("提交 compaction 清理事务失败: {e}")))?;
        Ok(deleted)
    }

    pub(crate) fn upsert_quota_latch(&self, row: &QuotaLatchRow) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO quota_latches
             (app_type, provider_id, account_id, quota_kind, blocked_until,
              signal_hash, detail_blob, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(app_type, provider_id, account_id, quota_kind) DO UPDATE SET
               blocked_until=excluded.blocked_until,
               signal_hash=excluded.signal_hash,
               detail_blob=excluded.detail_blob,
               updated_at=excluded.updated_at",
            params![
                row.app_type,
                row.provider_id,
                row.account_id,
                row.quota_kind,
                row.blocked_until,
                row.signal_hash,
                row.detail_blob,
                row.updated_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("保存 quota latch 失败: {e}")))?;
        Ok(())
    }

    pub(crate) fn active_quota_latches(
        &self,
        app_type: &str,
        now_iso: &str,
    ) -> Result<Vec<QuotaLatchRow>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT app_type, provider_id, account_id, quota_kind, blocked_until,
                        signal_hash, detail_blob, updated_at
                 FROM quota_latches
                 WHERE app_type = ?1 AND (blocked_until IS NULL OR blocked_until > ?2)",
            )
            .map_err(|e| AppError::Database(format!("准备读取 quota latch 失败: {e}")))?;
        let rows = stmt
            .query_map(params![app_type, now_iso], |row| {
                Ok(QuotaLatchRow {
                    app_type: row.get(0)?,
                    provider_id: row.get(1)?,
                    account_id: row.get(2)?,
                    quota_kind: row.get(3)?,
                    blocked_until: row.get(4)?,
                    signal_hash: row.get(5)?,
                    detail_blob: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .map_err(|e| AppError::Database(format!("读取 quota latch 失败: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(format!("解析 quota latch 失败: {e}")))
    }

    pub(crate) fn clear_quota_latches_for_app(&self, app_type: &str) -> Result<usize, AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute("DELETE FROM quota_latches WHERE app_type = ?1", [app_type])
            .map_err(|e| AppError::Database(format!("清理应用 quota latch 失败: {e}")))
    }

    pub(crate) fn clear_expired_quota_latches(&self, now_iso: &str) -> Result<usize, AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM quota_latches WHERE blocked_until IS NOT NULL AND blocked_until <= ?1",
            [now_iso],
        )
        .map_err(|e| AppError::Database(format!("清理过期 quota latch 失败: {e}")))
    }
}

fn map_snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompactionSnapshotRow> {
    Ok(CompactionSnapshotRow {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        session_id: row.get(2)?,
        request_id: row.get(3)?,
        source_model: row.get(4)?,
        parent_id: row.get(5)?,
        token_estimate: row.get(6)?,
        payload_hash: row.get(7)?,
        envelope_version: row.get(8)?,
        key_version: row.get(9)?,
        payload_blob: row.get(10)?,
        created_at: row.get(11)?,
    })
}
