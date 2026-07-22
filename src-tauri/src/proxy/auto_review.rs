//! Codex `codex-auto-review` routing policy and wire helpers.
//!
//! Auto Review is deliberately separate from the normal provider failover queue:
//! the official attempt is pinned to `codex-official`, and the fallback is pinned
//! to the provider selected in these settings.

use crate::{
    database::{Database, CODEX_OFFICIAL_PROVIDER_ID},
    error::AppError,
    provider::Provider,
    proxy::{providers::codex_responses_sse, ProxyError},
};
use bytes::Bytes;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const AUTO_REVIEW_MODEL: &str = "codex-auto-review";
const SETTINGS_KEY: &str = "codex_auto_review_settings";
const STATS_KEY: &str = "codex_auto_review_stats";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoReviewMode {
    #[default]
    Off,
    Auto,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewSettings {
    #[serde(default)]
    pub mode: AutoReviewMode,
    #[serde(default)]
    pub fallback_provider_id: String,
    #[serde(default)]
    pub fallback_model: String,
    #[serde(default = "default_fallback_effort")]
    pub fallback_effort: String,
}

fn default_fallback_effort() -> String {
    "none".to_string()
}

impl Default for AutoReviewSettings {
    fn default() -> Self {
        Self {
            mode: AutoReviewMode::Off,
            fallback_provider_id: String::new(),
            fallback_model: String::new(),
            fallback_effort: default_fallback_effort(),
        }
    }
}

impl AutoReviewSettings {
    fn normalize(mut self) -> Self {
        self.fallback_provider_id = self.fallback_provider_id.trim().to_string();
        self.fallback_model = self.fallback_model.trim().to_string();
        self.fallback_effort = self.fallback_effort.trim().to_ascii_lowercase();
        if self.fallback_effort.is_empty() {
            self.fallback_effort = default_fallback_effort();
        }
        self
    }

    fn validate(&self, db: &Database) -> Result<(), AppError> {
        const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
        if !EFFORTS.contains(&self.fallback_effort.as_str()) {
            return Err(AppError::InvalidInput(format!(
                "fallbackEffort must be one of: {}",
                EFFORTS.join(", ")
            )));
        }
        if self.mode == AutoReviewMode::Off {
            return Ok(());
        }
        if self.fallback_provider_id.is_empty() {
            return Err(AppError::InvalidInput(
                "Select a fallback provider before enabling Auto Review".to_string(),
            ));
        }
        if self.fallback_model.is_empty() {
            return Err(AppError::InvalidInput(
                "Select a fallback model before enabling Auto Review".to_string(),
            ));
        }
        if self.fallback_model == AUTO_REVIEW_MODEL {
            return Err(AppError::InvalidInput(
                "The Auto Review fallback model cannot be codex-auto-review".to_string(),
            ));
        }
        resolve_fallback_provider(db, self)?;
        Ok(())
    }
}

pub fn load_settings(db: &Database) -> Result<AutoReviewSettings, AppError> {
    let Some(raw) = db.get_setting(SETTINGS_KEY)? else {
        return Ok(AutoReviewSettings::default());
    };
    serde_json::from_str::<AutoReviewSettings>(&raw)
        .map(AutoReviewSettings::normalize)
        .map_err(|error| AppError::Config(format!("Invalid Auto Review settings: {error}")))
}

pub fn save_settings(
    db: &Database,
    settings: AutoReviewSettings,
) -> Result<AutoReviewSettings, AppError> {
    let settings = settings.normalize();
    settings.validate(db)?;
    let raw = serde_json::to_string(&settings).map_err(|error| {
        AppError::Config(format!("Unable to serialize Auto Review settings: {error}"))
    })?;
    db.set_setting(SETTINGS_KEY, &raw)?;
    Ok(settings)
}

pub(crate) fn resolve_official_provider(db: &Database) -> Result<Provider, ProxyError> {
    db.get_provider_by_id(CODEX_OFFICIAL_PROVIDER_ID, "codex")
        .map_err(|error| ProxyError::DatabaseError(error.to_string()))?
        .ok_or_else(|| {
            ProxyError::ConfigError(
                "The built-in OpenAI Official provider is unavailable; restore official providers in CC Switch"
                    .to_string(),
            )
        })
}

pub(crate) fn resolve_fallback_provider(
    db: &Database,
    settings: &AutoReviewSettings,
) -> Result<Provider, AppError> {
    let provider = db
        .get_provider_by_id(&settings.fallback_provider_id, "codex")?
        .ok_or_else(|| {
            AppError::InvalidInput(format!(
                "Auto Review fallback provider '{}' no longer exists",
                settings.fallback_provider_id
            ))
        })?;
    if provider.id == CODEX_OFFICIAL_PROVIDER_ID || provider.category.as_deref() == Some("official")
    {
        return Err(AppError::InvalidInput(
            "Auto Review fallback must be a third-party Codex provider".to_string(),
        ));
    }
    Ok(provider)
}

pub(crate) fn resolve_fallback_provider_for_request(
    db: &Database,
    settings: &AutoReviewSettings,
) -> Result<Provider, ProxyError> {
    resolve_fallback_provider(db, settings)
        .map_err(|error| ProxyError::ConfigError(error.to_string()))
}

pub(crate) fn is_auto_review_request(body: &Value) -> bool {
    body.get("model").and_then(Value::as_str) == Some(AUTO_REVIEW_MODEL)
}

/// Replace only routing-owned fields. Input, tools, text/response format, and
/// the caller's stream contract remain untouched.
pub(crate) fn prepare_fallback_body(body: &Value, settings: &AutoReviewSettings) -> Value {
    let mut fallback = body.clone();
    fallback["model"] = Value::String(settings.fallback_model.clone());
    let reasoning = fallback
        .as_object_mut()
        .expect("Responses request must be an object")
        .entry("reasoning")
        .or_insert_with(|| json!({}));
    if !reasoning.is_object() {
        *reasoning = json!({});
    }
    reasoning["effort"] = Value::String(settings.fallback_effort.clone());
    fallback
}

/// Only an explicit usage/quota exhaustion 429 engages Auto Review. Generic
/// rate limiting, gateway 429s, auth failures, and all other statuses stay on
/// the official error path.
pub(crate) fn is_official_quota_429(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError {
        status: 429,
        body: Some(body),
    } = error
    else {
        return false;
    };
    let body = body.to_ascii_lowercase();
    [
        "insufficient_quota",
        "usage limit",
        "usage_limit_reached",
        "quota exceeded",
        "limit_reached",
        "credit balance",
        "billing limit",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoReviewStats {
    pub official_attempts: u64,
    pub official_quota_429: u64,
    pub fallbacks: u64,
    pub successful_fallbacks: u64,
    pub failed_fallbacks: u64,
    pub last_fallback_at: Option<String>,
    pub last_fallback_provider_id: Option<String>,
    pub last_fallback_model: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct AutoReviewRuntime {
    stats: Mutex<AutoReviewStats>,
    db: Option<Arc<Database>>,
}

impl AutoReviewRuntime {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        let stats = load_stats(&db);
        Self {
            stats: Mutex::new(stats),
            db: Some(db),
        }
    }

    fn update(&self, update: impl FnOnce(&mut AutoReviewStats)) {
        let serialized = {
            let mut stats = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            update(&mut stats);
            serde_json::to_string(&*stats).ok()
        };
        if let (Some(db), Some(serialized)) = (&self.db, serialized) {
            if let Err(error) = db.set_setting(STATS_KEY, &serialized) {
                log::warn!("[AutoReview] unable to persist counters: {error}");
            }
        }
    }

    pub(crate) fn record_official_attempt(&self) {
        self.update(|stats| stats.official_attempts += 1);
    }

    pub(crate) fn record_official_quota_429(&self) {
        self.update(|stats| stats.official_quota_429 += 1);
    }

    pub(crate) async fn record_fallback(&self, provider_id: &str, model: &str) {
        self.update(|stats| {
            stats.fallbacks += 1;
            stats.last_fallback_at = Some(Utc::now().to_rfc3339());
            stats.last_fallback_provider_id = Some(provider_id.to_string());
            stats.last_fallback_model = Some(model.to_string());
            stats.last_error = None;
        });
    }

    pub(crate) async fn record_fallback_success(&self) {
        self.update(|stats| {
            stats.successful_fallbacks += 1;
            stats.last_error = None;
        });
    }

    pub(crate) async fn record_fallback_failure(&self, error: &ProxyError) {
        self.update(|stats| {
            stats.failed_fallbacks += 1;
            stats.last_error = Some(error.to_string());
        });
    }

    pub(crate) async fn snapshot(&self) -> AutoReviewStats {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) fn load_stats(db: &Database) -> AutoReviewStats {
    db.get_setting(STATS_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn strip_json_fence_from_response(response: &mut Value) -> usize {
    let mut stripped = 0;
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return stripped;
    };
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(Value::as_str) else {
                continue;
            };
            if let Some(unfenced) = strip_json_fence(text) {
                part["text"] = Value::String(unfenced);
                stripped += 1;
            }
        }
    }
    stripped
}

fn strip_json_fence(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let after_ticks = trimmed.strip_prefix("```")?;
    let after_language = if after_ticks
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("json"))
    {
        &after_ticks[4..]
    } else if after_ticks
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
    {
        return None;
    } else {
        after_ticks
    };
    let inner = after_language.trim_start();
    let inner = inner.strip_suffix("```")?.trim();
    Some(inner.to_string())
}

/// Rebuild a completed Responses object as a standards-shaped SSE sequence.
/// The fallback is intentionally buffered so a surrounding JSON fence can be
/// removed without leaking its opening bytes to the client.
pub(crate) fn response_to_sse(response: &Value) -> Result<Vec<u8>, ProxyError> {
    let mut events: Vec<Bytes> = Vec::new();
    let mut started = response.clone();
    let started_object = started.as_object_mut().ok_or_else(|| {
        ProxyError::TransformError("Auto Review response is not an object".to_string())
    })?;
    started_object.insert(
        "status".to_string(),
        Value::String("in_progress".to_string()),
    );
    started_object.insert("output".to_string(), Value::Array(Vec::new()));
    events.push(codex_responses_sse::response_created(&started));
    events.push(codex_responses_sse::response_in_progress(&started));

    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for (index, item) in output.iter().enumerate() {
            let output_index = index as u32;
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("auto_review_item");
            let mut added = item.clone();
            if let Some(object) = added.as_object_mut() {
                object.insert(
                    "status".to_string(),
                    Value::String("in_progress".to_string()),
                );
                if item_type == "message" {
                    object.insert("content".to_string(), Value::Array(Vec::new()));
                }
            }
            events.push(codex_responses_sse::output_item_added(output_index, &added));

            match item_type {
                "message" => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for (content_index, part) in content.iter().enumerate() {
                            let Some(text) = part.get("text").and_then(Value::as_str) else {
                                continue;
                            };
                            let content_index = content_index as u32;
                            events.push(codex_responses_sse::sse_event(
                                "response.content_part.added",
                                json!({
                                    "type": "response.content_part.added",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": {"type": "output_text", "text": "", "annotations": []}
                                }),
                            ));
                            events.push(codex_responses_sse::sse_event(
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "delta": text
                                }),
                            ));
                            events.push(codex_responses_sse::sse_event(
                                "response.output_text.done",
                                json!({
                                    "type": "response.output_text.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "text": text
                                }),
                            ));
                            events.push(codex_responses_sse::sse_event(
                                "response.content_part.done",
                                json!({
                                    "type": "response.content_part.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": part
                                }),
                            ));
                        }
                    }
                }
                "function_call" => {
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        events.push(codex_responses_sse::function_call_arguments_delta(
                            output_index,
                            item_id,
                            arguments,
                        ));
                        events.push(codex_responses_sse::function_call_arguments_done(
                            output_index,
                            item_id,
                            arguments,
                        ));
                    }
                }
                "custom_tool_call" => {
                    if let Some(input) = item.get("input").and_then(Value::as_str) {
                        events.push(codex_responses_sse::custom_tool_call_input_delta(
                            output_index,
                            item_id,
                            input,
                        ));
                        events.push(codex_responses_sse::custom_tool_call_input_done(
                            output_index,
                            item_id,
                            input,
                        ));
                    }
                }
                _ => {}
            }
            events.push(codex_responses_sse::output_item_done(output_index, item));
        }
    }
    events.push(codex_responses_sse::response_completed(response));

    let total_len = events.iter().map(Bytes::len).sum();
    let mut output = Vec::with_capacity(total_len);
    for event in events {
        output.extend_from_slice(&event);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_off_until_a_fallback_is_configured() {
        assert_eq!(AutoReviewSettings::default().mode, AutoReviewMode::Off);
    }

    #[test]
    fn enabled_mode_requires_an_exact_provider_and_model() {
        let db = Database::memory().unwrap();
        let settings = AutoReviewSettings {
            mode: AutoReviewMode::Auto,
            ..Default::default()
        };
        assert!(save_settings(&db, settings).is_err());
    }

    #[test]
    fn generic_rate_limit_does_not_trigger_fallback() {
        assert!(!is_official_quota_429(&ProxyError::UpstreamError {
            status: 429,
            body: Some(r#"{"error":{"code":"rate_limit_exceeded"}}"#.to_string()),
        }));
        assert!(is_official_quota_429(&ProxyError::UpstreamError {
            status: 429,
            body: Some(r#"{"error":{"code":"usage_limit_reached"}}"#.to_string()),
        }));
        assert!(!is_official_quota_429(&ProxyError::UpstreamError {
            status: 401,
            body: Some("usage limit".to_string()),
        }));
    }

    #[test]
    fn fallback_body_preserves_contract_fields() {
        let body = json!({
            "model": AUTO_REVIEW_MODEL,
            "input": [{"role":"user","content":"review"}],
            "tools": [{"type":"function","name":"check","parameters":{}}],
            "text": {"format":{"type":"json_schema","name":"review","schema":{"type":"object"}}},
            "stream": true,
            "reasoning": {"summary":"auto"}
        });
        let settings = AutoReviewSettings {
            mode: AutoReviewMode::Always,
            fallback_provider_id: "third-party".to_string(),
            fallback_model: "glm-5".to_string(),
            fallback_effort: "high".to_string(),
        };
        let fallback = prepare_fallback_body(&body, &settings);
        assert_eq!(fallback["model"], "glm-5");
        assert_eq!(fallback["reasoning"]["effort"], "high");
        assert_eq!(fallback["reasoning"]["summary"], "auto");
        assert_eq!(fallback["tools"], body["tools"]);
        assert_eq!(fallback["text"], body["text"]);
        assert_eq!(fallback["stream"], true);
    }

    #[test]
    fn strips_only_whole_json_fences() {
        let mut response = json!({
            "output": [{
                "id":"msg_1",
                "type":"message",
                "content":[{"type":"output_text","text":"```json\n{\"ok\":true}\n```"}]
            }]
        });
        assert_eq!(strip_json_fence_from_response(&mut response), 1);
        assert_eq!(response["output"][0]["content"][0]["text"], "{\"ok\":true}");
        assert_eq!(strip_json_fence("prefix ```json\n{}\n```"), None);
        assert_eq!(strip_json_fence("```rust\n{}\n```"), None);
    }

    #[test]
    fn rebuilt_stream_preserves_text_and_tool_items() {
        let response = json!({
            "id":"resp_1",
            "status":"completed",
            "model":"glm-5",
            "output":[
                {"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"{\"ok\":true}","annotations":[]}]},
                {"id":"fc_1","type":"function_call","status":"completed","call_id":"call_1","name":"check","arguments":"{\"x\":1}"}
            ]
        });
        let stream = String::from_utf8(response_to_sse(&response).unwrap()).unwrap();
        assert!(stream.contains("event: response.created"));
        assert!(stream.contains("event: response.output_text.delta"));
        assert!(stream.contains("event: response.function_call_arguments.done"));
        assert!(stream.contains("event: response.completed"));
        assert!(stream.contains("{\\\"ok\\\":true}"));
    }

    #[tokio::test]
    async fn runtime_counters_survive_proxy_server_recreation() {
        let db = Arc::new(Database::memory().unwrap());
        let runtime = AutoReviewRuntime::new(db.clone());
        runtime.record_official_attempt();
        runtime.record_official_quota_429();
        runtime
            .record_fallback("provider-1", "fallback-model")
            .await;
        runtime.record_fallback_success().await;

        let restored = AutoReviewRuntime::new(db).snapshot().await;
        assert_eq!(restored.official_attempts, 1);
        assert_eq!(restored.official_quota_429, 1);
        assert_eq!(restored.fallbacks, 1);
        assert_eq!(restored.successful_fallbacks, 1);
        assert_eq!(
            restored.last_fallback_provider_id.as_deref(),
            Some("provider-1")
        );
        assert_eq!(
            restored.last_fallback_model.as_deref(),
            Some("fallback-model")
        );
    }
}
