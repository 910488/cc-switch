use crate::database::{Database, QuotaLatchRow};
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::ProxyError;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct QuotaLatch {
    #[allow(dead_code)]
    pub provider_id: String,
    #[allow(dead_code)]
    pub account_id: String,
    pub quota_kind: String,
    pub blocked_until: DateTime<Utc>,
}

pub(crate) struct QuotaPolicy {
    db: Arc<Database>,
}

impl QuotaPolicy {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub(crate) fn is_quota_error(error: &ProxyError) -> bool {
        matches!(error, ProxyError::UpstreamError { status: 429, .. })
    }

    pub(crate) fn record_from_error(
        &self,
        app_type: &str,
        provider: &Provider,
        error: &ProxyError,
    ) -> Result<Option<QuotaLatch>, AppError> {
        let ProxyError::UpstreamError { status: 429, body } = error else {
            return Ok(None);
        };
        let now = Utc::now();
        let body_value = body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok());
        let body_lower = body.as_deref().unwrap_or_default().to_ascii_lowercase();
        let hard_quota = [
            "insufficient_quota",
            "usage limit",
            "quota exceeded",
            "limit_reached",
            "credit balance",
            "billing",
        ]
        .iter()
        .any(|needle| body_lower.contains(needle));
        let blocked_until = body_value
            .as_ref()
            .and_then(|value| reset_time_from_value(value, now))
            .unwrap_or_else(|| now + Duration::seconds(if hard_quota { 900 } else { 60 }));
        let quota_kind = if hard_quota {
            "hard_quota"
        } else {
            "rate_limit"
        };
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            .or_else(|| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.managed_account_id_for("github_copilot"))
            })
            .unwrap_or_default();
        let latch = QuotaLatch {
            provider_id: provider.id.clone(),
            account_id: account_id.clone(),
            quota_kind: quota_kind.to_string(),
            blocked_until,
        };
        self.db.upsert_quota_latch(&QuotaLatchRow {
            app_type: app_type.to_string(),
            provider_id: provider.id.clone(),
            account_id,
            quota_kind: quota_kind.to_string(),
            blocked_until: Some(iso(blocked_until)),
            // Error bodies can contain gateway/user details. The latch only needs
            // routing metadata, so never persist the raw body or a searchable hash.
            signal_hash: None,
            detail_blob: None,
            updated_at: iso(now),
        })?;
        Ok(Some(latch))
    }

    pub(crate) fn active_provider_ids(&self, app_type: &str) -> Result<HashSet<String>, AppError> {
        let now = iso(Utc::now());
        self.db.clear_expired_quota_latches(&now)?;
        Ok(self
            .db
            .active_quota_latches(app_type, &now)?
            .into_iter()
            .map(|row| row.provider_id)
            .collect())
    }

    pub(crate) fn is_provider_latched(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<bool, AppError> {
        Ok(self.active_provider_ids(app_type)?.contains(provider_id))
    }

    pub(crate) fn earliest_release(&self, app_type: &str) -> Result<Option<String>, AppError> {
        let now = iso(Utc::now());
        Ok(self
            .db
            .active_quota_latches(app_type, &now)?
            .into_iter()
            .filter_map(|row| row.blocked_until)
            .min())
    }
}

fn reset_time_from_value(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    const ABSOLUTE_KEYS: &[&str] = &[
        "blocked_until",
        "reset_at",
        "reset_time",
        "resets_at",
        "limit_reset_at",
    ];
    const DELAY_KEYS: &[&str] = &["retry_after", "retry_after_seconds"];
    const DELAY_MS_KEYS: &[&str] = &["retry_after_ms", "retry_after_milliseconds"];
    match value {
        Value::Object(object) => {
            for key in ABSOLUTE_KEYS {
                if let Some(parsed) = object.get(*key).and_then(parse_absolute_time) {
                    return Some(parsed);
                }
            }
            for key in DELAY_KEYS {
                if let Some(seconds) = object.get(*key).and_then(Value::as_f64) {
                    return Some(now + Duration::milliseconds((seconds * 1000.0) as i64));
                }
            }
            for key in DELAY_MS_KEYS {
                if let Some(milliseconds) = object.get(*key).and_then(Value::as_i64) {
                    return Some(now + Duration::milliseconds(milliseconds));
                }
            }
            object
                .values()
                .find_map(|child| reset_time_from_value(child, now))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| reset_time_from_value(child, now)),
        _ => None,
    }
}

fn parse_absolute_time(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(value) = value.as_str() {
        return DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.with_timezone(&Utc));
    }
    let numeric = value.as_f64()?;
    let milliseconds = if numeric > 10_000_000_000.0 {
        numeric as i64
    } else {
        (numeric * 1000.0) as i64
    };
    DateTime::from_timestamp_millis(milliseconds)
}

fn iso(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(id: &str) -> Provider {
        Provider::with_id(id.to_string(), id.to_string(), json!({}), None)
    }

    #[test]
    fn hard_quota_creates_persistent_provider_latch() {
        let db = Arc::new(Database::memory().unwrap());
        let provider = provider("quota-a");
        db.save_provider("codex", &provider).unwrap();
        let policy = QuotaPolicy::new(db.clone());
        let latch = policy
            .record_from_error(
                "codex",
                &provider,
                &ProxyError::UpstreamError {
                    status: 429,
                    body: Some(
                        json!({"error":{"code":"insufficient_quota"},"retry_after":120})
                            .to_string(),
                    ),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(latch.quota_kind, "hard_quota");
        assert!(policy.is_provider_latched("codex", "quota-a").unwrap());
        let recreated = QuotaPolicy::new(db);
        assert!(recreated.is_provider_latched("codex", "quota-a").unwrap());
    }

    #[test]
    fn reset_parser_supports_nested_retry_after_and_epoch() {
        let now = Utc::now();
        let delay = reset_time_from_value(&json!({"error":{"retry_after_ms":5000}}), now).unwrap();
        assert!((delay - now).num_seconds() >= 4);
        let absolute = now + Duration::minutes(4);
        let parsed = reset_time_from_value(
            &json!({"rate_limit":{"reset_at":absolute.timestamp()}}),
            now,
        )
        .unwrap();
        assert!((parsed - absolute).num_seconds().abs() <= 1);
    }
}
