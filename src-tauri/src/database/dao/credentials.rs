//! Metadata persistence for provider credential pools.
//!
//! Secret values never enter this module. `secret_backend` and `secret_handle`
//! are opaque references owned by the platform SecretVault.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderCredentialRow {
    pub id: String,
    pub app_type: String,
    pub provider_id: String,
    pub kind: String,
    pub label: String,
    pub masked_hint: String,
    pub secret_backend: String,
    pub secret_handle: String,
    pub enabled: bool,
    pub priority: i32,
    pub auth_header: String,
    pub auth_prefix: String,
    pub public_metadata: String,
    pub status: String,
    pub last_error_code: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CredentialQuotaSnapshotRow {
    pub credential_id: String,
    pub quota_kind: String,
    pub remaining_ratio: Option<f64>,
    pub used_ratio: Option<f64>,
    pub reset_at: Option<String>,
    pub detail_json: String,
    pub queried_at: String,
}

impl Database {
    pub(crate) fn upsert_provider_credential(
        &self,
        row: &ProviderCredentialRow,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO provider_credentials
             (id, app_type, provider_id, kind, label, masked_hint, secret_backend,
              secret_handle, enabled, priority, auth_header, auth_prefix, public_metadata,
              status, last_error_code, last_used_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET
               app_type=excluded.app_type,
               provider_id=excluded.provider_id,
               kind=excluded.kind,
               label=excluded.label,
               masked_hint=excluded.masked_hint,
               secret_backend=excluded.secret_backend,
               secret_handle=excluded.secret_handle,
               enabled=excluded.enabled,
               priority=excluded.priority,
               auth_header=excluded.auth_header,
               auth_prefix=excluded.auth_prefix,
               public_metadata=excluded.public_metadata,
               status=excluded.status,
               last_error_code=excluded.last_error_code,
               last_used_at=excluded.last_used_at,
               updated_at=excluded.updated_at",
            params![
                row.id,
                row.app_type,
                row.provider_id,
                row.kind,
                row.label,
                row.masked_hint,
                row.secret_backend,
                row.secret_handle,
                row.enabled,
                row.priority,
                row.auth_header,
                row.auth_prefix,
                row.public_metadata,
                row.status,
                row.last_error_code,
                row.last_used_at,
                row.created_at,
                row.updated_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("failed to save provider credential: {e}")))?;
        Ok(())
    }

    pub(crate) fn provider_credential(
        &self,
        credential_id: &str,
    ) -> Result<Option<ProviderCredentialRow>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            &format!(
                "SELECT {} FROM provider_credentials WHERE id = ?1",
                CREDENTIAL_COLUMNS
            ),
            [credential_id],
            map_credential_row,
        )
        .optional()
        .map_err(|e| AppError::Database(format!("failed to load provider credential: {e}")))
    }

    pub(crate) fn provider_credentials(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<Vec<ProviderCredentialRow>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {} FROM provider_credentials
                 WHERE app_type = ?1 AND provider_id = ?2
                 ORDER BY enabled DESC, priority ASC, created_at ASC",
                CREDENTIAL_COLUMNS
            ))
            .map_err(|e| AppError::Database(format!("failed to prepare credential list: {e}")))?;
        let rows = stmt
            .query_map(params![app_type, provider_id], map_credential_row)
            .map_err(|e| AppError::Database(format!("failed to list provider credentials: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(format!("failed to parse provider credentials: {e}")))
    }

    pub(crate) fn enabled_provider_credential_count(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<usize, AppError> {
        let conn = lock_conn!(self.conn);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM provider_credentials
                 WHERE app_type = ?1 AND provider_id = ?2 AND enabled = 1",
                params![app_type, provider_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                AppError::Database(format!("failed to count provider credentials: {e}"))
            })?;
        Ok(count.max(0) as usize)
    }

    pub(crate) fn available_provider_credential(
        &self,
        app_type: &str,
        provider_id: &str,
        now_iso: &str,
    ) -> Result<Option<ProviderCredentialRow>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            &format!(
                "SELECT {} FROM provider_credentials c
                 WHERE c.app_type = ?1 AND c.provider_id = ?2 AND c.enabled = 1
                   AND NOT EXISTS (
                     SELECT 1 FROM quota_latches l
                     WHERE l.app_type = c.app_type
                       AND l.provider_id = c.provider_id
                       AND l.account_id = c.id
                       AND (l.blocked_until IS NULL OR l.blocked_until > ?3)
                   )
                 ORDER BY
                   COALESCE((
                     SELECT MIN(q.remaining_ratio)
                     FROM credential_quota_snapshots q
                     WHERE q.credential_id = c.id
                       AND q.remaining_ratio IS NOT NULL
                       AND (q.reset_at IS NULL OR q.reset_at > ?3)
                   ), 1.0) DESC,
                   c.priority ASC,
                   CASE WHEN c.last_used_at IS NULL THEN 0 ELSE 1 END ASC,
                   c.last_used_at ASC,
                   c.created_at ASC
                 LIMIT 1",
                CREDENTIAL_COLUMNS_PREFIXED
            ),
            params![app_type, provider_id, now_iso],
            map_credential_row,
        )
        .optional()
        .map_err(|e| AppError::Database(format!("failed to select provider credential: {e}")))
    }

    pub(crate) fn delete_provider_credential(&self, credential_id: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM provider_credentials WHERE id = ?1",
            [credential_id],
        )
        .map(|count| count > 0)
        .map_err(|e| AppError::Database(format!("failed to delete provider credential: {e}")))
    }

    pub(crate) fn touch_provider_credential(
        &self,
        credential_id: &str,
        used_at: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE provider_credentials
             SET last_used_at = ?2, status = 'valid', last_error_code = NULL, updated_at = ?2
             WHERE id = ?1",
            params![credential_id, used_at],
        )
        .map_err(|e| AppError::Database(format!("failed to touch provider credential: {e}")))?;
        Ok(())
    }

    pub(crate) fn update_provider_credential_status(
        &self,
        credential_id: &str,
        enabled: bool,
        status: &str,
        error_code: Option<&str>,
        updated_at: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE provider_credentials
             SET enabled = ?2, status = ?3, last_error_code = ?4, updated_at = ?5
             WHERE id = ?1",
            params![credential_id, enabled, status, error_code, updated_at],
        )
        .map_err(|e| AppError::Database(format!("failed to update credential status: {e}")))?;
        Ok(())
    }

    pub(crate) fn upsert_credential_quota_snapshot(
        &self,
        row: &CredentialQuotaSnapshotRow,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO credential_quota_snapshots
             (credential_id, quota_kind, remaining_ratio, used_ratio, reset_at,
              detail_json, queried_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(credential_id, quota_kind) DO UPDATE SET
               remaining_ratio=excluded.remaining_ratio,
               used_ratio=excluded.used_ratio,
               reset_at=excluded.reset_at,
               detail_json=excluded.detail_json,
               queried_at=excluded.queried_at",
            params![
                row.credential_id,
                row.quota_kind,
                row.remaining_ratio,
                row.used_ratio,
                row.reset_at,
                row.detail_json,
                row.queried_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("failed to save credential quota: {e}")))?;
        Ok(())
    }

    pub(crate) fn credential_quota_snapshots(
        &self,
        credential_id: &str,
    ) -> Result<Vec<CredentialQuotaSnapshotRow>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT credential_id, quota_kind, remaining_ratio, used_ratio,
                        reset_at, detail_json, queried_at
                 FROM credential_quota_snapshots
                 WHERE credential_id = ?1 ORDER BY quota_kind ASC",
            )
            .map_err(|e| AppError::Database(format!("failed to prepare credential quota: {e}")))?;
        let rows = stmt
            .query_map([credential_id], |row| {
                Ok(CredentialQuotaSnapshotRow {
                    credential_id: row.get(0)?,
                    quota_kind: row.get(1)?,
                    remaining_ratio: row.get(2)?,
                    used_ratio: row.get(3)?,
                    reset_at: row.get(4)?,
                    detail_json: row.get(5)?,
                    queried_at: row.get(6)?,
                })
            })
            .map_err(|e| AppError::Database(format!("failed to list credential quota: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(format!("failed to parse credential quota: {e}")))
    }
}

const CREDENTIAL_COLUMNS: &str =
    "id, app_type, provider_id, kind, label, masked_hint, secret_backend, secret_handle,
     enabled, priority, auth_header, auth_prefix, public_metadata, status, last_error_code,
     last_used_at, created_at, updated_at";

const CREDENTIAL_COLUMNS_PREFIXED: &str =
    "c.id, c.app_type, c.provider_id, c.kind, c.label, c.masked_hint, c.secret_backend,
     c.secret_handle, c.enabled, c.priority, c.auth_header, c.auth_prefix, c.public_metadata,
     c.status, c.last_error_code, c.last_used_at, c.created_at, c.updated_at";

fn map_credential_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderCredentialRow> {
    Ok(ProviderCredentialRow {
        id: row.get(0)?,
        app_type: row.get(1)?,
        provider_id: row.get(2)?,
        kind: row.get(3)?,
        label: row.get(4)?,
        masked_hint: row.get(5)?,
        secret_backend: row.get(6)?,
        secret_handle: row.get(7)?,
        enabled: row.get(8)?,
        priority: row.get(9)?,
        auth_header: row.get(10)?,
        auth_prefix: row.get(11)?,
        public_metadata: row.get(12)?,
        status: row.get(13)?,
        last_error_code: row.get(14)?,
        last_used_at: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::QuotaLatchRow;
    use crate::provider::Provider;
    use serde_json::json;

    fn row(id: &str, priority: i32, now: &str) -> ProviderCredentialRow {
        ProviderCredentialRow {
            id: id.to_string(),
            app_type: "codex".to_string(),
            provider_id: "pool-provider".to_string(),
            kind: "api_key".to_string(),
            label: id.to_string(),
            masked_hint: "sk-…last".to_string(),
            secret_backend: "test".to_string(),
            secret_handle: format!("handle-{id}"),
            enabled: true,
            priority,
            auth_header: "authorization".to_string(),
            auth_prefix: "Bearer ".to_string(),
            public_metadata: "{}".to_string(),
            status: "unknown".to_string(),
            last_error_code: None,
            last_used_at: None,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        }
    }

    #[test]
    fn selection_skips_latched_credentials_and_prefers_quota() {
        let db = Database::memory().unwrap();
        db.save_provider(
            "codex",
            &Provider::with_id(
                "pool-provider".to_string(),
                "Pool Provider".to_string(),
                json!({"base_url":"https://example.invalid/v1"}),
                None,
            ),
        )
        .unwrap();
        let now = "2026-07-19T00:00:00Z";
        db.upsert_provider_credential(&row("cred-a", 10, now))
            .unwrap();
        db.upsert_provider_credential(&row("cred-b", 20, now))
            .unwrap();
        db.upsert_credential_quota_snapshot(&CredentialQuotaSnapshotRow {
            credential_id: "cred-a".to_string(),
            quota_kind: "weekly".to_string(),
            remaining_ratio: Some(0.2),
            used_ratio: Some(0.8),
            reset_at: Some("2026-07-25T00:00:00Z".to_string()),
            detail_json: "{}".to_string(),
            queried_at: now.to_string(),
        })
        .unwrap();
        db.upsert_credential_quota_snapshot(&CredentialQuotaSnapshotRow {
            credential_id: "cred-b".to_string(),
            quota_kind: "weekly".to_string(),
            remaining_ratio: Some(0.8),
            used_ratio: Some(0.2),
            reset_at: Some("2026-07-25T00:00:00Z".to_string()),
            detail_json: "{}".to_string(),
            queried_at: now.to_string(),
        })
        .unwrap();

        assert_eq!(
            db.available_provider_credential("codex", "pool-provider", now)
                .unwrap()
                .unwrap()
                .id,
            "cred-b"
        );
        db.upsert_quota_latch(&QuotaLatchRow {
            app_type: "codex".to_string(),
            provider_id: "pool-provider".to_string(),
            account_id: "cred-b".to_string(),
            quota_kind: "weekly".to_string(),
            blocked_until: Some("2026-07-25T00:00:00Z".to_string()),
            signal_hash: None,
            detail_blob: None,
            updated_at: now.to_string(),
        })
        .unwrap();
        assert_eq!(
            db.available_provider_credential("codex", "pool-provider", now)
                .unwrap()
                .unwrap()
                .id,
            "cred-a"
        );
    }
}
