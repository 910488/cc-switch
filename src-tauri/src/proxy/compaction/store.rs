use super::crypto::JournalCipher;
use super::model::{
    BridgeEnvelope, CompactionContext, CompactionTaskState, Snapshot, StoreCounts,
    StoredCompaction, ENVELOPE_PREFIX, ENVELOPE_VERSION, KEY_VERSION,
};
use crate::config::get_app_config_dir;
use crate::database::Database;
use crate::error::AppError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) struct CompactionStore {
    db: Arc<Database>,
    cipher: JournalCipher,
}

impl CompactionStore {
    pub(crate) fn new(db: Arc<Database>) -> Result<Self, AppError> {
        let cipher = JournalCipher::load_or_create(&get_app_config_dir())?;
        Ok(Self { db, cipher })
    }

    #[cfg(test)]
    pub(crate) fn with_key(db: Arc<Database>, key: Vec<u8>) -> Result<Self, AppError> {
        Ok(Self {
            db,
            cipher: JournalCipher::from_key(key, "test")?,
        })
    }

    pub(crate) fn key_protection(&self) -> &str {
        self.cipher.protection()
    }

    #[cfg(test)]
    pub(crate) fn database(&self) -> Arc<Database> {
        self.db.clone()
    }

    pub(crate) fn save_snapshot(
        &self,
        context: &CompactionContext,
        source_model: &str,
        parent_id: Option<&str>,
        payload: &Value,
    ) -> Result<Snapshot, AppError> {
        let id = new_id("snap");
        let canonical = serde_json::to_vec(payload)
            .map_err(|e| AppError::Message(format!("cannot serialize compaction snapshot: {e}")))?;
        let payload_hash = self.cipher.fingerprint(&canonical)?;
        let payload_blob = self.cipher.seal(&canonical, id.as_bytes())?;
        let created_at = now_iso();
        let token_estimate = estimate_tokens(payload) as i64;
        self.db
            .insert_compaction_snapshot(&crate::database::CompactionSnapshotRow {
                id: id.clone(),
                thread_id: context.thread_id.clone(),
                session_id: context.session_id.clone(),
                request_id: context.request_id.clone(),
                source_model: source_model.to_string(),
                parent_id: parent_id.map(ToString::to_string),
                token_estimate,
                payload_hash: payload_hash.clone(),
                envelope_version: ENVELOPE_VERSION,
                key_version: KEY_VERSION,
                payload_blob,
                created_at: created_at.clone(),
            })?;
        Ok(Snapshot {
            id,
            thread_id: context.thread_id.clone(),
            session_id: context.session_id.clone(),
            request_id: context.request_id.clone(),
            source_model: source_model.to_string(),
            parent_id: parent_id.map(ToString::to_string),
            token_estimate,
            payload_hash,
            payload: payload.clone(),
            created_at,
        })
    }

    pub(crate) fn get_snapshot(&self, id: &str) -> Result<Option<Snapshot>, AppError> {
        let Some(row) = self.db.get_compaction_snapshot_row(id)? else {
            return Ok(None);
        };
        if row.envelope_version != ENVELOPE_VERSION || row.key_version != KEY_VERSION {
            return Err(AppError::Message(format!(
                "unsupported compaction snapshot version {}/{}",
                row.envelope_version, row.key_version
            )));
        }
        let plaintext = self.cipher.open(&row.payload_blob, row.id.as_bytes())?;
        let actual_hash = self.cipher.fingerprint(&plaintext)?;
        if actual_hash != row.payload_hash {
            return Err(AppError::Message(
                "compaction snapshot hash mismatch; original context was not modified".to_string(),
            ));
        }
        let payload = serde_json::from_slice(&plaintext)
            .map_err(|e| AppError::Message(format!("invalid compaction snapshot JSON: {e}")))?;
        Ok(Some(Snapshot {
            id: row.id,
            thread_id: row.thread_id,
            session_id: row.session_id,
            request_id: row.request_id,
            source_model: row.source_model,
            parent_id: row.parent_id,
            token_estimate: row.token_estimate,
            payload_hash: row.payload_hash,
            payload,
            created_at: row.created_at,
        }))
    }

    #[allow(dead_code)]
    pub(crate) fn latest_snapshot(
        &self,
        thread_id: &str,
        session_id: Option<&str>,
    ) -> Result<Option<Snapshot>, AppError> {
        let Some(row) = self
            .db
            .latest_compaction_snapshot_row(thread_id, session_id)?
        else {
            return Ok(None);
        };
        self.get_snapshot(&row.id)
    }

    #[allow(dead_code)]
    pub(crate) fn register_compaction(
        &self,
        id: &str,
        snapshot_id: &str,
        realm: &str,
        source_model: &str,
        item: &Value,
    ) -> Result<(), AppError> {
        if let Some(existing) = self.get_compaction(id)? {
            if existing.snapshot_id == snapshot_id
                && existing.realm == realm
                && existing.source_model == source_model
                && existing.item == *item
            {
                return Ok(());
            }
            return Err(AppError::Database(format!(
                "compaction id {id} conflicts with an existing continuity record"
            )));
        }
        let plaintext = serde_json::to_vec(item)
            .map_err(|e| AppError::Message(format!("cannot serialize compaction item: {e}")))?;
        let item_blob = self.cipher.seal(&plaintext, id.as_bytes())?;
        self.db
            .upsert_compaction_row(&crate::database::CompactionRow {
                id: id.to_string(),
                snapshot_id: snapshot_id.to_string(),
                realm: realm.to_string(),
                source_model: source_model.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_version: KEY_VERSION,
                item_blob,
                created_at: now_iso(),
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_compaction_and_task_state(
        &self,
        id: &str,
        snapshot_id: &str,
        realm: &str,
        source_model: &str,
        item: &Value,
        state: &CompactionTaskState,
    ) -> Result<(), AppError> {
        if let Some(existing) = self.get_compaction(id)? {
            if existing.snapshot_id != snapshot_id
                || existing.realm != realm
                || existing.source_model != source_model
                || existing.item != *item
            {
                return Err(AppError::Database(format!(
                    "compaction id {id} conflicts with an existing continuity record"
                )));
            }
            return self.save_task_state(state);
        }
        let item_plaintext = serde_json::to_vec(item)
            .map_err(|e| AppError::Message(format!("cannot serialize compaction item: {e}")))?;
        let item_blob = self.cipher.seal(&item_plaintext, id.as_bytes())?;
        let state_aad = format!("{}:{}", state.thread_id, state.session_id);
        let state_plaintext = serde_json::to_vec(state).map_err(|e| {
            AppError::Message(format!("cannot serialize compaction task state: {e}"))
        })?;
        let state_blob = self.cipher.seal(&state_plaintext, state_aad.as_bytes())?;
        self.db.commit_compaction_and_task_state(
            &crate::database::CompactionRow {
                id: id.to_string(),
                snapshot_id: snapshot_id.to_string(),
                realm: realm.to_string(),
                source_model: source_model.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_version: KEY_VERSION,
                item_blob,
                created_at: now_iso(),
            },
            &state.thread_id,
            &state.session_id,
            id,
            &state_blob,
            &state.updated_at,
        )
    }

    pub(crate) fn get_compaction(&self, id: &str) -> Result<Option<StoredCompaction>, AppError> {
        let Some(row) = self.db.get_compaction_row(id)? else {
            return Ok(None);
        };
        if row.envelope_version != ENVELOPE_VERSION || row.key_version != KEY_VERSION {
            return Err(AppError::Message(format!(
                "unsupported compaction item version {}/{}",
                row.envelope_version, row.key_version
            )));
        }
        let plaintext = self.cipher.open(&row.item_blob, row.id.as_bytes())?;
        let item = serde_json::from_slice(&plaintext)
            .map_err(|e| AppError::Message(format!("invalid compaction item JSON: {e}")))?;
        Ok(Some(StoredCompaction {
            id: row.id,
            snapshot_id: row.snapshot_id,
            realm: row.realm,
            source_model: row.source_model,
            item,
            created_at: row.created_at,
        }))
    }

    pub(crate) fn save_migration(
        &self,
        source_compaction_id: &str,
        target_model: &str,
        target_realm: &str,
        target_provider_id: &str,
        prompt_version: i64,
        item: &Value,
    ) -> Result<(), AppError> {
        let aad = migration_aad(
            source_compaction_id,
            target_model,
            target_realm,
            target_provider_id,
            prompt_version,
        );
        let plaintext = serde_json::to_vec(item).map_err(|e| {
            AppError::Message(format!("cannot serialize compaction migration: {e}"))
        })?;
        let blob = self.cipher.seal(&plaintext, aad.as_bytes())?;
        self.db.upsert_compaction_migration(
            source_compaction_id,
            target_model,
            target_realm,
            target_provider_id,
            prompt_version,
            &blob,
            &now_iso(),
        )
    }

    pub(crate) fn get_migration(
        &self,
        source_compaction_id: &str,
        target_model: &str,
        target_realm: &str,
        target_provider_id: &str,
        prompt_version: i64,
    ) -> Result<Option<Value>, AppError> {
        let Some(blob) = self.db.get_compaction_migration_blob(
            source_compaction_id,
            target_model,
            target_realm,
            target_provider_id,
            prompt_version,
        )?
        else {
            return Ok(None);
        };
        let aad = migration_aad(
            source_compaction_id,
            target_model,
            target_realm,
            target_provider_id,
            prompt_version,
        );
        let plaintext = self.cipher.open(&blob, aad.as_bytes())?;
        serde_json::from_slice(&plaintext)
            .map(Some)
            .map_err(|e| AppError::Message(format!("invalid compaction migration JSON: {e}")))
    }

    pub(crate) fn save_task_state(&self, state: &CompactionTaskState) -> Result<(), AppError> {
        let aad = format!("{}:{}", state.thread_id, state.session_id);
        let plaintext = serde_json::to_vec(state).map_err(|e| {
            AppError::Message(format!("cannot serialize compaction task state: {e}"))
        })?;
        let blob = self.cipher.seal(&plaintext, aad.as_bytes())?;
        self.db.upsert_compaction_task_state(
            &state.thread_id,
            &state.session_id,
            state.compaction_id.as_deref(),
            &blob,
            &state.updated_at,
        )
    }

    pub(crate) fn recent_task_states(
        &self,
        limit: usize,
    ) -> Result<Vec<CompactionTaskState>, AppError> {
        self.db
            .recent_compaction_task_state_rows(limit)?
            .into_iter()
            .map(|row| {
                let aad = format!("{}:{}", row.thread_id, row.session_id);
                let plaintext = self.cipher.open(&row.state_blob, aad.as_bytes())?;
                serde_json::from_slice(&plaintext).map_err(|e| {
                    AppError::Message(format!("invalid compaction task state JSON: {e}"))
                })
            })
            .collect()
    }

    pub(crate) fn make_envelope(
        &self,
        compaction_id: &str,
        snapshot_id: &str,
        source_model: &str,
        summary: &str,
        token_estimate: i64,
    ) -> Result<String, AppError> {
        if compaction_id.is_empty() || snapshot_id.is_empty() || summary.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "compaction envelope identity and summary are required".to_string(),
            ));
        }
        let payload = BridgeEnvelope {
            v: ENVELOPE_VERSION,
            compaction_id: compaction_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            source_model: source_model.to_string(),
            summary: summary.to_string(),
            token_estimate,
            created_at: now_iso(),
        };
        let plaintext = serde_json::to_vec(&payload)
            .map_err(|e| AppError::Message(format!("cannot serialize bridge envelope: {e}")))?;
        let sealed = self.cipher.seal(&plaintext, ENVELOPE_PREFIX.as_bytes())?;
        Ok(format!(
            "{ENVELOPE_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    pub(crate) fn open_envelope(
        &self,
        encoded: &str,
        expected_outer_id: Option<&str>,
    ) -> Result<Option<BridgeEnvelope>, AppError> {
        let Some(encoded) = encoded.strip_prefix(ENVELOPE_PREFIX) else {
            return Ok(None);
        };
        let sealed = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| {
            AppError::InvalidInput("invalid bridge compaction envelope".to_string())
        })?;
        let plaintext = self.cipher.open(&sealed, ENVELOPE_PREFIX.as_bytes())?;
        let payload: BridgeEnvelope = serde_json::from_slice(&plaintext).map_err(|e| {
            AppError::InvalidInput(format!("invalid bridge compaction payload: {e}"))
        })?;
        if payload.v != ENVELOPE_VERSION
            || payload.compaction_id.is_empty()
            || payload.snapshot_id.is_empty()
            || payload.summary.trim().is_empty()
        {
            return Err(AppError::InvalidInput(
                "unsupported bridge compaction envelope".to_string(),
            ));
        }
        if expected_outer_id.is_some_and(|id| id != payload.compaction_id) {
            return Err(AppError::InvalidInput(
                "outer compaction id does not match its encrypted envelope".to_string(),
            ));
        }
        Ok(Some(payload))
    }

    pub(crate) fn counts(&self) -> Result<StoreCounts, AppError> {
        let counts = self.db.compaction_counts()?;
        Ok(StoreCounts {
            snapshots: counts.snapshots,
            compactions: counts.compactions,
            migrations: counts.migrations,
            task_states: counts.task_states,
        })
    }

    pub(crate) fn delete_thread(&self, thread_id: &str) -> Result<usize, AppError> {
        self.db.delete_compaction_thread(thread_id)
    }
}

pub(crate) fn canonical_snapshot_body(body: &Value) -> Value {
    let mut snapshot = body.clone();
    snapshot["stream"] = Value::Bool(false);
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        snapshot["input"] = Value::Array(
            input
                .iter()
                .filter(|item| {
                    item.get("type").and_then(Value::as_str) != Some("compaction_trigger")
                })
                .cloned()
                .collect(),
        );
    }
    snapshot
}

pub(crate) fn estimate_tokens(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len().div_ceil(4))
        .unwrap_or(0)
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn migration_aad(
    source_compaction_id: &str,
    target_model: &str,
    target_realm: &str,
    target_provider_id: &str,
    prompt_version: i64,
) -> String {
    format!(
        "{source_compaction_id}:{target_model}:{target_realm}:{target_provider_id}:p{prompt_version}"
    )
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde_json::json;

    fn test_store() -> CompactionStore {
        CompactionStore::with_key(Arc::new(Database::memory().unwrap()), vec![3; 32]).unwrap()
    }

    #[test]
    fn encrypted_journal_envelope_migration_and_cleanup_round_trip() {
        let store = test_store();
        let context = CompactionContext {
            thread_id: "thread-a".into(),
            session_id: "session-a".into(),
            request_id: "request-a".into(),
        };
        let secret = "continuity secret must never be plaintext";
        let first = store
            .save_snapshot(&context, "GLM-5.2", None, &json!({"input": secret}))
            .unwrap();
        let other_context = CompactionContext {
            thread_id: "thread-b".into(),
            session_id: "session-b".into(),
            request_id: "request-b".into(),
        };
        store
            .save_snapshot(&other_context, "gpt-5", None, &json!({"input": "other"}))
            .unwrap();
        let envelope = store
            .make_envelope("cmp-a", &first.id, "GLM-5.2", secret, 8)
            .unwrap();
        let item = json!({"id":"cmp-a","type":"compaction","encrypted_content":envelope});
        store
            .register_compaction("cmp-a", &first.id, "bridge", "GLM-5.2", &item)
            .unwrap();
        store
            .save_migration(
                "cmp-a",
                "gpt-5",
                "official",
                "official-provider",
                1,
                &json!({"type":"compaction","encrypted_content":"opaque"}),
            )
            .unwrap();
        store
            .save_task_state(&CompactionTaskState {
                thread_id: context.thread_id.clone(),
                session_id: context.session_id.clone(),
                compaction_id: Some("cmp-a".into()),
                model: "GLM-5.2".into(),
                realm: "bridge".into(),
                state: "warning".into(),
                phase: "awaiting_resume".into(),
                journal_saved: true,
                summary_created: true,
                resume_verified: false,
                original_context_retained: true,
                error_code: None,
                updated_at: Utc::now().to_rfc3339(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            store.get_snapshot(&first.id).unwrap().unwrap().payload["input"],
            secret
        );
        assert_eq!(store.get_compaction("cmp-a").unwrap().unwrap().item, item);
        assert_eq!(
            store
                .get_migration("cmp-a", "gpt-5", "official", "official-provider", 1)
                .unwrap()
                .unwrap()["encrypted_content"],
            "opaque"
        );
        assert_eq!(store.recent_task_states(8).unwrap().len(), 1);
        assert_eq!(
            store.counts().unwrap(),
            StoreCounts {
                snapshots: 2,
                compactions: 1,
                migrations: 1,
                task_states: 1
            }
        );
        assert_eq!(store.delete_thread("thread-a").unwrap(), 1);
        assert_eq!(
            store.counts().unwrap(),
            StoreCounts {
                snapshots: 1,
                compactions: 0,
                migrations: 0,
                task_states: 0
            }
        );
    }

    #[test]
    fn envelope_rejects_tamper_and_outer_id_mismatch() {
        let store = test_store();
        let envelope = store
            .make_envelope("cmp-a", "snap-a", "GLM-5.2", "summary", 10)
            .unwrap();
        assert!(store.open_envelope(&envelope, Some("cmp-b")).is_err());
        let mut bytes = envelope.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        assert!(store
            .open_envelope(&String::from_utf8(bytes).unwrap(), Some("cmp-a"))
            .is_err());
    }

    #[test]
    fn canonical_snapshot_removes_only_trigger_and_forces_non_stream() {
        let snapshot = canonical_snapshot_body(&json!({
            "stream": true,
            "input": [
                {"type":"message","content":"keep"},
                {"type":"compaction_trigger"}
            ]
        }));
        assert_eq!(snapshot["stream"], false);
        assert_eq!(snapshot["input"].as_array().unwrap().len(), 1);
    }
}
