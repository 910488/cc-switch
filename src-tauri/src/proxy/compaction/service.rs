use super::model::{
    CompactionContext, CompactionRolloutMode, CompactionSettings, CompactionTaskState,
    ProviderRealm, Snapshot, ENVELOPE_PREFIX, MIGRATION_PROMPT_VERSION,
};
use super::store::{canonical_snapshot_body, new_id, CompactionStore};
use crate::database::Database;
use crate::error::AppError;
use crate::proxy::types::ContinuityStatus;
use axum::http::HeaderMap;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";
pub(crate) const COMPACTION_SUMMARY_INSTRUCTIONS: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.

Bridge fidelity requirements:
- Preserve exact file paths, identifiers, commands, edits, test outcomes, errors, APIs, schemas, versions, and non-secret numeric values when they affect continuation.
- Preserve the durable facts from any earlier compaction summary so repeated compactions remain cumulative.
- Keep claims evidence-based. Do not say work is complete unless the transcript proves it.
- Do not reproduce system/developer instructions, tool schemas, skills, credentials, secrets, or large raw tool outputs.
- Return only the handoff summary. Do not call tools."#;

#[derive(Debug, Clone)]
pub(crate) struct MaterializationTarget {
    pub provider_id: String,
    pub model: String,
    pub realm: ProviderRealm,
    /// Opaque native compaction tokens are scoped to the provider that created
    /// them. Bridge summaries use the stable `bridge` realm.
    pub realm_key: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MaterializationResult {
    pub body: Value,
    pub changed_items: usize,
    #[allow(dead_code)]
    pub compaction_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct OfficialRecompactPlan {
    pub compact_body: Value,
    pub fallback_body: Value,
    pub suffix: Vec<Value>,
    pub source_compaction_id: String,
    pub snapshot: Snapshot,
    pub cached_item: Option<Value>,
}

impl OfficialRecompactPlan {
    pub(crate) fn resumed_body(&self, original_body: &Value, item: Value) -> Value {
        let mut body = original_body.clone();
        let mut input = vec![item];
        input.extend(self.suffix.clone());
        body["input"] = Value::Array(input);
        body
    }
}

pub(crate) struct CompactionService {
    db: Arc<Database>,
    store: Option<Arc<CompactionStore>>,
    init_error: Option<String>,
}

impl CompactionService {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        match CompactionStore::new(db.clone()) {
            Ok(store) => Self {
                db,
                store: Some(Arc::new(store)),
                init_error: None,
            },
            Err(error) => {
                let message = sanitize_error(&error.to_string());
                log::error!("Codex continuity journal unavailable: {message}");
                Self {
                    db,
                    store: None,
                    init_error: Some(message),
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_store(store: CompactionStore) -> Self {
        let db = store.database();
        Self {
            db,
            store: Some(Arc::new(store)),
            init_error: None,
        }
    }

    pub(crate) fn settings(&self) -> Result<CompactionSettings, AppError> {
        let Some(raw) = self.db.get_setting("codex_compaction_settings")? else {
            return Ok(CompactionSettings::default());
        };
        serde_json::from_str(&raw)
            .map_err(|error| AppError::Config(format!("invalid compaction settings: {error}")))
    }

    pub(crate) fn update_settings(&self, settings: &CompactionSettings) -> Result<(), AppError> {
        let raw = serde_json::to_string(settings)
            .map_err(|error| AppError::Config(format!("invalid compaction settings: {error}")))?;
        self.db.set_setting("codex_compaction_settings", &raw)
    }

    pub(crate) fn rollout_mode(&self) -> CompactionRolloutMode {
        self.settings()
            .map(|settings| settings.rollout_mode)
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub(crate) fn is_available(&self) -> bool {
        self.store.is_some()
    }

    #[allow(dead_code)]
    pub(crate) fn key_protection(&self) -> Option<&str> {
        self.store.as_ref().map(|store| store.key_protection())
    }

    pub(crate) fn status(&self) -> ContinuityStatus {
        let Some(store) = &self.store else {
            return ContinuityStatus {
                error: self.init_error.clone(),
                ..ContinuityStatus::default()
            };
        };
        match store.counts() {
            Ok(counts) => ContinuityStatus {
                available: true,
                key_protection: Some(store.key_protection().to_string()),
                snapshots: counts.snapshots,
                compactions: counts.compactions,
                migrations: counts.migrations,
                task_states: counts.task_states,
                rollout_mode: serde_json::to_value(self.rollout_mode())
                    .ok()
                    .and_then(|value| value.as_str().map(ToString::to_string))
                    .unwrap_or_else(|| "full-switching".to_string()),
                error: None,
            },
            Err(error) => ContinuityStatus {
                key_protection: Some(store.key_protection().to_string()),
                error: Some(sanitize_error(&error.to_string())),
                ..ContinuityStatus::default()
            },
        }
    }

    pub(crate) fn context_from_request(
        headers: &HeaderMap,
        body: &Value,
        stable_session_id: &str,
    ) -> CompactionContext {
        let session_id = header_value(headers, &["session-id", "session_id", "x-session-id"])
            .or_else(|| metadata_string(body, "session_id"))
            .unwrap_or_else(|| stable_session_id.to_string());
        let thread_id = header_value(headers, &["thread-id", "x-thread-id"])
            .or_else(|| metadata_string(body, "thread_id"))
            .unwrap_or_else(|| session_id.clone());
        let request_id = header_value(headers, &["x-client-request-id", "x-request-id"])
            .or_else(|| metadata_string(body, "request_id"))
            .unwrap_or_else(|| new_id("req"));
        CompactionContext {
            thread_id,
            session_id,
            request_id,
        }
    }

    pub(crate) fn is_compaction_request(body: &Value) -> bool {
        body.get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("compaction_trigger")
                })
            })
    }

    pub(crate) fn prepare_local_summary_request_with(
        body: &Value,
        prompt: &str,
        max_output_tokens: usize,
    ) -> Value {
        let mut prepared = canonical_snapshot_body(body);
        let mut input = prepared
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": prompt,
            }],
        }));
        prepared["input"] = Value::Array(input);
        prepared["stream"] = Value::Bool(false);
        prepared["tools"] = Value::Array(Vec::new());
        prepared["tool_choice"] = Value::String("none".to_string());
        prepared["max_output_tokens"] = Value::from(max_output_tokens as u64);
        prepared
    }

    pub(crate) fn save_snapshot(
        &self,
        context: &CompactionContext,
        body: &Value,
        realm: ProviderRealm,
    ) -> Result<Snapshot, AppError> {
        let store = self.store()?;
        let payload = canonical_snapshot_body(body);
        let source_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let snapshot = store.save_snapshot(context, source_model, None, &payload)?;
        store.save_task_state(&CompactionTaskState {
            thread_id: context.thread_id.clone(),
            session_id: context.session_id.clone(),
            compaction_id: None,
            model: source_model.to_string(),
            realm: realm.as_str().to_string(),
            state: "working".to_string(),
            phase: "journal_saved".to_string(),
            journal_saved: true,
            summary_created: false,
            resume_verified: false,
            original_context_retained: true,
            error_code: None,
            strategy: None,
            input_tokens_before: snapshot.token_estimate.max(0) as usize,
            summary_tokens: 0,
            chunks_completed: 0,
            chunks_total: 0,
            retry_count: 0,
            overflow_retry_count: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            provider_id: None,
            updated_at: now_iso(),
        })?;
        Ok(snapshot)
    }

    pub(crate) fn create_bridge_compaction(
        &self,
        context: &CompactionContext,
        snapshot: &Snapshot,
        provider_id: &str,
        summary: &str,
    ) -> Result<Value, AppError> {
        self.create_bridge_compaction_with_execution(context, snapshot, provider_id, summary, None)
    }

    pub(crate) fn create_bridge_compaction_with_execution(
        &self,
        context: &CompactionContext,
        snapshot: &Snapshot,
        provider_id: &str,
        summary: &str,
        execution: Option<&super::executor::SummaryExecution>,
    ) -> Result<Value, AppError> {
        let store = self.store()?;
        if summary.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "third-party compaction returned an empty summary".to_string(),
            ));
        }
        let compaction_id = new_id("cmp");
        let envelope = store.make_envelope(
            &compaction_id,
            &snapshot.id,
            &snapshot.source_model,
            summary.trim(),
            super::store::estimate_tokens(&Value::String(summary.to_string())) as i64,
        )?;
        let item = json!({
            "id": compaction_id,
            "type": "compaction",
            "encrypted_content": envelope,
        });
        let state = CompactionTaskState {
            thread_id: context.thread_id.clone(),
            session_id: context.session_id.clone(),
            compaction_id: item["id"].as_str().map(ToString::to_string),
            model: snapshot.source_model.clone(),
            realm: "bridge".to_string(),
            state: "warning".to_string(),
            phase: "awaiting_resume".to_string(),
            journal_saved: true,
            summary_created: true,
            resume_verified: false,
            original_context_retained: true,
            error_code: None,
            strategy: execution.map(|value| value.strategy.clone()),
            input_tokens_before: execution
                .map(|value| value.input_tokens_before)
                .unwrap_or(snapshot.token_estimate.max(0) as usize),
            summary_tokens: execution
                .map(|value| value.summary_tokens)
                .unwrap_or_default(),
            chunks_completed: execution.map(|value| value.chunks).unwrap_or_default(),
            chunks_total: execution.map(|value| value.chunks).unwrap_or_default(),
            retry_count: execution.map(|value| value.retries).unwrap_or_default(),
            overflow_retry_count: execution
                .map(|value| value.overflow_retries)
                .unwrap_or_default(),
            prompt_tokens: execution
                .map(|value| value.prompt_tokens)
                .unwrap_or_default(),
            completion_tokens: execution
                .map(|value| value.completion_tokens)
                .unwrap_or_default(),
            total_tokens: execution
                .map(|value| value.total_tokens)
                .unwrap_or_default(),
            provider_id: Some(provider_id.to_string()),
            updated_at: now_iso(),
        };
        store.register_compaction_and_task_state(
            item["id"].as_str().unwrap_or_default(),
            &snapshot.id,
            &format!("bridge:{provider_id}"),
            &snapshot.source_model,
            &item,
            &state,
        )?;
        Ok(item)
    }

    pub(crate) fn register_native_compaction(
        &self,
        context: &CompactionContext,
        snapshot: &Snapshot,
        provider_id: &str,
        item: &Value,
    ) -> Result<(), AppError> {
        let store = self.store()?;
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                AppError::InvalidInput("native compaction item has no id".to_string())
            })?;
        let state = CompactionTaskState {
            thread_id: context.thread_id.clone(),
            session_id: context.session_id.clone(),
            compaction_id: Some(id.to_string()),
            model: snapshot.source_model.clone(),
            realm: "official".to_string(),
            state: "warning".to_string(),
            phase: "awaiting_resume".to_string(),
            journal_saved: true,
            summary_created: true,
            resume_verified: false,
            original_context_retained: true,
            error_code: None,
            strategy: Some("native".to_string()),
            input_tokens_before: snapshot.token_estimate.max(0) as usize,
            summary_tokens: 0,
            chunks_completed: 1,
            chunks_total: 1,
            retry_count: 0,
            overflow_retry_count: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            provider_id: Some(provider_id.to_string()),
            updated_at: now_iso(),
        };
        store.register_compaction_and_task_state(
            id,
            &snapshot.id,
            &format!("native:{provider_id}"),
            &snapshot.source_model,
            item,
            &state,
        )
    }

    pub(crate) fn materialize_for_target(
        &self,
        body: &Value,
        target: &MaterializationTarget,
    ) -> Result<MaterializationResult, AppError> {
        let result = self.materialize_for_target_unbounded(body, target)?;
        if result.changed_items == 0 {
            return Ok(result);
        }
        Ok(MaterializationResult {
            body: super::planner::bound_migrated_continuation(&result.body),
            ..result
        })
    }

    fn materialize_for_target_unbounded(
        &self,
        body: &Value,
        target: &MaterializationTarget,
    ) -> Result<MaterializationResult, AppError> {
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return Ok(MaterializationResult {
                body: body.clone(),
                ..Default::default()
            });
        };
        if !input.iter().any(is_compaction_item) {
            return Ok(MaterializationResult {
                body: body.clone(),
                ..Default::default()
            });
        }
        let store = self.store()?;
        let mut visited = HashSet::new();
        let mut ids = Vec::new();
        let materialized = self.materialize_items(store, input, target, &mut visited, &mut ids)?;
        let mut output = body.clone();
        output["input"] = Value::Array(materialized);
        Ok(MaterializationResult {
            body: output,
            changed_items: ids.len(),
            compaction_ids: ids,
        })
    }

    pub(crate) fn prepare_official_recompact(
        &self,
        body: &Value,
        target: &MaterializationTarget,
    ) -> Result<Option<OfficialRecompactPlan>, AppError> {
        if target.realm != ProviderRealm::Official || !self.rollout_mode().allows_cross_realm() {
            return Ok(None);
        }
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return Ok(None);
        };
        let store = self.store()?;
        let mut source = None;
        let mut last_compaction_index = None;
        for (index, item) in input.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("compaction") {
                continue;
            }
            let encrypted = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !encrypted.starts_with(ENVELOPE_PREFIX) {
                continue;
            }
            let envelope = store
                .open_envelope(encrypted, item.get("id").and_then(Value::as_str))?
                .ok_or_else(|| AppError::InvalidInput("invalid bridge compaction".to_string()))?;
            source = Some(envelope);
            last_compaction_index = Some(index);
        }
        let (Some(envelope), Some(last_index)) = (source, last_compaction_index) else {
            return Ok(None);
        };
        let suffix = input[last_index + 1..]
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"))
            .cloned()
            .collect::<Vec<_>>();
        let materialized = self.materialize_for_target_unbounded(body, target)?;
        let mut compact_body = materialized.body.clone();
        let compact_input = compact_body
            .get_mut("input")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                AppError::InvalidInput("materialized compact body has no input".to_string())
            })?;
        if compact_input.len() < suffix.len() {
            return Err(AppError::InvalidInput(
                "materialized compact suffix is inconsistent".to_string(),
            ));
        }
        compact_input.truncate(compact_input.len() - suffix.len());
        compact_body["stream"] = Value::Bool(false);
        let snapshot = store.get_snapshot(&envelope.snapshot_id)?.ok_or_else(|| {
            AppError::InvalidInput(format!(
                "canonical snapshot {} is unavailable",
                envelope.snapshot_id
            ))
        })?;
        let cached_item = store.get_migration(
            &envelope.compaction_id,
            &target.model,
            "official",
            &target.provider_id,
            MIGRATION_PROMPT_VERSION,
        )?;
        Ok(Some(OfficialRecompactPlan {
            compact_body,
            fallback_body: super::planner::bound_migrated_continuation(&materialized.body),
            suffix,
            source_compaction_id: envelope.compaction_id,
            snapshot,
            cached_item,
        }))
    }

    pub(crate) fn finish_official_recompact(
        &self,
        context: &CompactionContext,
        plan: &OfficialRecompactPlan,
        target: &MaterializationTarget,
        item: &Value,
    ) -> Result<(), AppError> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                AppError::InvalidInput("native compaction item has no id".to_string())
            })?;
        let store = self.store()?;
        self.register_native_compaction(context, &plan.snapshot, &target.provider_id, item)?;
        store.save_migration(
            &plan.source_compaction_id,
            &target.model,
            "official",
            &target.provider_id,
            MIGRATION_PROMPT_VERSION,
            item,
        )?;
        log::info!(
            "[Compaction] bridge {} re-tokenized as official native item {} for provider {}",
            plan.source_compaction_id,
            id,
            target.provider_id
        );
        Ok(())
    }

    fn materialize_items(
        &self,
        store: &CompactionStore,
        items: &[Value],
        target: &MaterializationTarget,
        visited: &mut HashSet<String>,
        ids: &mut Vec<String>,
    ) -> Result<Vec<Value>, AppError> {
        let mut output = Vec::new();
        for item in items {
            if !is_compaction_item(item) {
                if item.get("type").and_then(Value::as_str) != Some("compaction_trigger") {
                    output.push(item.clone());
                }
                continue;
            }
            let outer_id = item.get("id").and_then(Value::as_str);
            let encrypted = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if encrypted.starts_with(ENVELOPE_PREFIX) {
                let envelope = store.open_envelope(encrypted, outer_id)?.ok_or_else(|| {
                    AppError::InvalidInput("invalid bridge compaction".to_string())
                })?;
                ids.push(envelope.compaction_id.clone());
                if target.realm == ProviderRealm::Bridge {
                    let message = summary_message(&envelope.summary);
                    if store
                        .get_migration(
                            &envelope.compaction_id,
                            &target.model,
                            "bridge",
                            &target.provider_id,
                            MIGRATION_PROMPT_VERSION,
                        )?
                        .is_none()
                    {
                        store.save_migration(
                            &envelope.compaction_id,
                            &target.model,
                            "bridge",
                            &target.provider_id,
                            MIGRATION_PROMPT_VERSION,
                            &message,
                        )?;
                    }
                    output.push(message);
                } else {
                    if !self.rollout_mode().allows_cross_realm() {
                        return Err(AppError::InvalidInput(format!(
                            "bridge-to-official compaction migration is disabled in {:?} mode",
                            self.rollout_mode()
                        )));
                    }
                    output.extend(self.materialize_snapshot(
                        store,
                        &envelope.snapshot_id,
                        target,
                        visited,
                        ids,
                    )?);
                }
                continue;
            }

            let id = outer_id.filter(|id| !id.is_empty()).ok_or_else(|| {
                AppError::InvalidInput("opaque compaction item has no id".to_string())
            })?;
            ids.push(id.to_string());
            let stored = store.get_compaction(id)?.ok_or_else(|| {
                AppError::Message(format!(
                    "compaction {id} has no canonical journal entry; refusing to drop context"
                ))
            })?;
            let same_native_realm = target.realm == ProviderRealm::Official
                && (stored.realm == target.realm_key
                    || (stored.realm == "official" && target.realm_key.starts_with("native:")));
            if same_native_realm {
                output.push(item.clone());
            } else {
                let source_is_official =
                    stored.realm == "official" || stored.realm.starts_with("native:");
                if source_is_official
                    && target.realm == ProviderRealm::Bridge
                    && !self.rollout_mode().allows_cross_realm()
                {
                    return Err(AppError::InvalidInput(format!(
                        "official-to-bridge compaction migration is disabled in {:?} mode",
                        self.rollout_mode()
                    )));
                }
                output.extend(self.materialize_snapshot(
                    store,
                    &stored.snapshot_id,
                    target,
                    visited,
                    ids,
                )?);
            }
        }
        Ok(output)
    }

    fn materialize_snapshot(
        &self,
        store: &CompactionStore,
        snapshot_id: &str,
        target: &MaterializationTarget,
        visited: &mut HashSet<String>,
        ids: &mut Vec<String>,
    ) -> Result<Vec<Value>, AppError> {
        if !visited.insert(snapshot_id.to_string()) {
            return Err(AppError::InvalidInput(format!(
                "cyclic compaction journal reference at {snapshot_id}"
            )));
        }
        let result = (|| {
            let snapshot = store.get_snapshot(snapshot_id)?.ok_or_else(|| {
                AppError::Message(format!(
                    "canonical snapshot {snapshot_id} is unavailable; refusing provider migration"
                ))
            })?;
            let input = snapshot
                .payload
                .get("input")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    AppError::InvalidInput(format!(
                        "canonical snapshot {snapshot_id} has no Responses input"
                    ))
                })?;
            self.materialize_items(store, input, target, visited, ids)
        })();
        visited.remove(snapshot_id);
        result
    }

    fn store(&self) -> Result<&CompactionStore, AppError> {
        self.store.as_deref().ok_or_else(|| {
            AppError::Message(format!(
                "compaction journal unavailable: {}",
                self.init_error.as_deref().unwrap_or("not initialized")
            ))
        })
    }
}

fn is_compaction_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("compaction")
}

fn summary_message(summary: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!("{COMPACTION_SUMMARY_PREFIX}\n{summary}"),
        }],
    })
}

fn header_value(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn metadata_string(body: &Value, key: &str) -> Option<String> {
    body.get("metadata")
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn sanitize_error(value: &str) -> String {
    let without_keys = regex::Regex::new(r"(?i)(sk|key|token)-[A-Za-z0-9_-]+")
        .ok()
        .map(|regex| regex.replace_all(value, "[redacted]").into_owned())
        .unwrap_or_else(|| value.to_string());
    without_keys.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::compaction::store::CompactionStore;
    use serde_json::json;

    fn service() -> CompactionService {
        let db = Arc::new(Database::memory().unwrap());
        let store = CompactionStore::with_key(db, vec![5; 32]).unwrap();
        CompactionService::with_store(store)
    }

    #[test]
    fn context_prefers_real_codex_hyphen_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("thread-id", "thread-real".parse().unwrap());
        headers.insert("session-id", "session-real".parse().unwrap());
        headers.insert("x-client-request-id", "request-real".parse().unwrap());
        let context = CompactionService::context_from_request(&headers, &json!({}), "fallback");
        assert_eq!(context.thread_id, "thread-real");
        assert_eq!(context.session_id, "session-real");
        assert_eq!(context.request_id, "request-real");
    }

    #[test]
    fn repeated_bridge_compactions_recursively_preserve_earliest_history() {
        let service = service();
        let context = CompactionContext {
            thread_id: "thread".into(),
            session_id: "session".into(),
            request_id: "r1".into(),
        };
        let first_snapshot = service
            .save_snapshot(
                &context,
                &json!({"model":"a","input":[{"type":"message","role":"user","content":"EARLIEST"}]}),
                ProviderRealm::Bridge,
            )
            .unwrap();
        let first = service
            .create_bridge_compaction(&context, &first_snapshot, "provider-a", "first summary")
            .unwrap();
        let second_snapshot = service
            .save_snapshot(
                &context,
                &json!({"model":"a","input":[first,{"type":"message","role":"user","content":"MIDDLE"}]}),
                ProviderRealm::Bridge,
            )
            .unwrap();
        let second = service
            .create_bridge_compaction(&context, &second_snapshot, "provider-a", "second summary")
            .unwrap();
        let result = service
            .materialize_for_target(
                &json!({"model":"native","input":[second,{"type":"message","role":"user","content":"SUFFIX"}]}),
                &MaterializationTarget {
                    provider_id: "native-provider".into(),
                    model: "native".into(),
                    realm: ProviderRealm::Official,
                    realm_key: "native:native-provider".into(),
                },
            )
            .unwrap();
        let serialized = result.body.to_string();
        assert!(serialized.contains("EARLIEST"));
        assert!(serialized.contains("MIDDLE"));
        assert_eq!(serialized.matches("SUFFIX").count(), 1);
        assert!(!serialized.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn corrupt_envelope_is_fail_closed_before_materialization() {
        let service = service();
        let error = service
            .materialize_for_target(
                &json!({"input":[{"id":"cmp-x","type":"compaction","encrypted_content":"bcmp1.invalid"}]}),
                &MaterializationTarget {
                    provider_id: "chat".into(),
                    model: "chat".into(),
                    realm: ProviderRealm::Bridge,
                    realm_key: "bridge:chat".into(),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("invalid bridge compaction"));
    }

    #[test]
    fn official_recompact_uses_full_snapshot_excludes_suffix_and_caches_native_item() {
        let service = service();
        let context = CompactionContext {
            thread_id: "thread-recompact".into(),
            session_id: "session-recompact".into(),
            request_id: "request-recompact".into(),
        };
        let snapshot = service
            .save_snapshot(
                &context,
                &json!({"model":"bridge-model","input":[
                    {"type":"message","role":"user","content":"CANONICAL_HISTORY"}
                ]}),
                ProviderRealm::Bridge,
            )
            .unwrap();
        let bridge = service
            .create_bridge_compaction(&context, &snapshot, "bridge-provider", "bridge summary")
            .unwrap();
        let original = json!({"model":"official-model","input":[
            bridge,
            {"type":"message","role":"user","content":"NEW_SUFFIX"}
        ]});
        let target = MaterializationTarget {
            provider_id: "official-provider".into(),
            model: "official-model".into(),
            realm: ProviderRealm::Official,
            realm_key: "native:official-provider".into(),
        };
        let plan = service
            .prepare_official_recompact(&original, &target)
            .unwrap()
            .expect("plan");
        assert!(plan.compact_body.to_string().contains("CANONICAL_HISTORY"));
        assert!(!plan.compact_body.to_string().contains("NEW_SUFFIX"));
        assert!(plan.fallback_body.to_string().contains("NEW_SUFFIX"));

        let native = json!({"id":"cmp-native","type":"compaction","encrypted_content":"opaque"});
        service
            .finish_official_recompact(&context, &plan, &target, &native)
            .unwrap();
        let cached = service
            .prepare_official_recompact(&original, &target)
            .unwrap()
            .expect("cached plan");
        assert_eq!(cached.cached_item, Some(native.clone()));
        let resumed = cached.resumed_body(&original, native);
        assert_eq!(resumed["input"].as_array().unwrap().len(), 2);
        assert!(resumed.to_string().contains("NEW_SUFFIX"));
    }

    #[test]
    fn third_party_only_rejects_bridge_to_official_materialization() {
        let service = service();
        service
            .update_settings(&CompactionSettings {
                rollout_mode: CompactionRolloutMode::ThirdPartyOnly,
                official_compact_fallback: true,
            })
            .unwrap();
        let context = CompactionContext {
            thread_id: "thread-mode".into(),
            session_id: "session-mode".into(),
            request_id: "request-mode".into(),
        };
        let snapshot = service
            .save_snapshot(
                &context,
                &json!({"model":"bridge","input":[{"type":"message","content":"history"}]}),
                ProviderRealm::Bridge,
            )
            .unwrap();
        let bridge = service
            .create_bridge_compaction(&context, &snapshot, "bridge", "summary")
            .unwrap();
        let error = service
            .materialize_for_target(
                &json!({"model":"official","input":[bridge]}),
                &MaterializationTarget {
                    provider_id: "official".into(),
                    model: "official".into(),
                    realm: ProviderRealm::Official,
                    realm_key: "native:official".into(),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("bridge-to-official"));
    }
}
