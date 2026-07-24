use crate::database::{Database, QuotaLatchRow};
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::ProxyError;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

const MODEL_SCOPED_QUOTA_LATCH_MIGRATION_KEY: &str = "codex_model_scoped_quota_latch_migration_v1";

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
        let policy = Self { db };
        if let Err(error) = policy.clear_legacy_codex_provider_latches_once() {
            log::warn!("Failed to clear legacy model-agnostic Codex quota latches: {error}");
        }
        policy
    }

    fn clear_legacy_codex_provider_latches_once(&self) -> Result<(), AppError> {
        if self
            .db
            .get_bool_flag(MODEL_SCOPED_QUOTA_LATCH_MIGRATION_KEY)?
        {
            return Ok(());
        }

        // Older releases stored every upstream 429 as a provider-wide Codex
        // latch. The row has no model dimension, so a GLM-5.2p exhaustion can
        // incorrectly block GLM-5.2 until the upstream reset date. These rows
        // are an operational cache, not durable user data; clear them once when
        // upgrading to model-scoped quota handling.
        self.db.clear_quota_latches_for_app("codex")?;
        self.db
            .set_setting(MODEL_SCOPED_QUOTA_LATCH_MIGRATION_KEY, "true")?;
        Ok(())
    }

    pub(crate) fn is_quota_error(error: &ProxyError) -> bool {
        matches!(error, ProxyError::UpstreamError { status: 429, .. })
    }

    /// Temporary upstream capacity exhaustion is not an account/provider quota.
    /// Gateways commonly encode NVIDIA worker saturation as HTTP 429, but
    /// persisting a quota latch for it prevents the short retry requested by the
    /// upstream and makes the whole provider appear unavailable.
    pub(crate) fn is_transient_capacity_error(error: &ProxyError) -> bool {
        let ProxyError::UpstreamError {
            status,
            body: Some(body),
        } = error
        else {
            return false;
        };
        if !matches!(*status, 429 | 500 | 502 | 503 | 504) {
            return false;
        }

        let body = body.to_ascii_lowercase();
        [
            "no deployments available",
            "worker local total request limit",
            "resourceexhausted",
            "resource exhausted",
            "worker capacity",
            "worker is busy",
            "worker overloaded",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }

    /// Whether the upstream explicitly says only one model/model-group is out
    /// of quota. Such an error must not disable every model behind the same
    /// provider: GLM-5.2p and GLM-5.2, for example, may have independent
    /// weekly/monthly pools even though they share one API key and endpoint.
    pub(crate) fn is_model_scoped_quota_error(error: &ProxyError) -> bool {
        let ProxyError::UpstreamError {
            status,
            body: Some(body),
        } = error
        else {
            return false;
        };
        if !matches!(*status, 400 | 429) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        [
            "received model group=",
            "received model group =",
            "model group quota",
            "model-specific quota",
            "per-model quota",
            "quota for model",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }

    /// Persistent balance/subscription exhaustion, as opposed to a transient
    /// request-rate throttle. This classification is also used by the Codex
    /// error response path to avoid hiding an actionable upstream message
    /// behind generic retry exhaustion.
    pub(crate) fn is_hard_quota_error(error: &ProxyError) -> bool {
        let ProxyError::UpstreamError {
            status,
            body: Some(body),
        } = error
        else {
            return false;
        };
        // Some OpenAI-compatible gateways incorrectly encode subscription
        // exhaustion as HTTP 400 (not 429). Classify only explicit quota/balance
        // wording so an ordinary malformed request remains a normal 400.
        if !matches!(*status, 400 | 429) {
            return false;
        }
        let body = body.to_ascii_lowercase();
        [
            "insufficient_quota",
            "usage limit",
            "quota exceeded",
            "limit_reached",
            "limit exhausted",
            "weekly/monthly limit",
            "credit balance",
            "billing",
            "配额不足",
            "额度不足",
            "餘額不足",
            "余额不足",
        ]
        .iter()
        .any(|needle| body.contains(needle))
    }

    pub(crate) fn record_from_error_for_account(
        &self,
        app_type: &str,
        provider: &Provider,
        error: &ProxyError,
        account_override: Option<&str>,
    ) -> Result<Option<QuotaLatch>, AppError> {
        let ProxyError::UpstreamError { status: 429, body } = error else {
            return Ok(None);
        };
        if Self::is_transient_capacity_error(error) {
            return Ok(None);
        }
        // Do not turn a model-specific exhaustion into a provider-wide latch.
        // The original 429 is still returned to the caller, but sibling models
        // remain routable immediately.
        if Self::is_model_scoped_quota_error(error) {
            return Ok(None);
        }
        let now = Utc::now();
        let body_value = body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok());
        let hard_quota = Self::is_hard_quota_error(error);
        let blocked_until = body_value
            .as_ref()
            .and_then(|value| reset_time_from_value(value, now))
            .unwrap_or_else(|| now + Duration::seconds(if hard_quota { 900 } else { 60 }));
        let quota_kind = if hard_quota {
            "hard_quota"
        } else {
            "rate_limit"
        };
        let account_id = account_override
            .map(ToString::to_string)
            .or_else(|| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            })
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
            .filter(|row| row.account_id.is_empty())
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
    fn clears_legacy_model_agnostic_codex_latches_once() {
        let db = Arc::new(Database::memory().unwrap());
        db.save_provider("codex", &provider("weikuwu")).unwrap();
        db.upsert_quota_latch(&QuotaLatchRow {
            app_type: "codex".to_string(),
            provider_id: "weikuwu".to_string(),
            account_id: String::new(),
            quota_kind: "hard_quota".to_string(),
            blocked_until: Some(iso(Utc::now() + Duration::days(2))),
            signal_hash: None,
            detail_blob: None,
            updated_at: iso(Utc::now()),
        })
        .unwrap();

        let policy = QuotaPolicy::new(db.clone());
        assert!(!policy.is_provider_latched("codex", "weikuwu").unwrap());
        assert!(db
            .get_bool_flag(MODEL_SCOPED_QUOTA_LATCH_MIGRATION_KEY)
            .unwrap());

        let provider = provider("new-quota");
        db.save_provider("codex", &provider).unwrap();
        policy
            .record_from_error_for_account(
                "codex",
                &provider,
                &ProxyError::UpstreamError {
                    status: 429,
                    body: Some(json!({"error":{"code":"insufficient_quota"}}).to_string()),
                },
                None,
            )
            .unwrap();
        let recreated = QuotaPolicy::new(db);
        assert!(recreated.is_provider_latched("codex", "new-quota").unwrap());
    }

    #[test]
    fn hard_quota_creates_persistent_provider_latch() {
        let db = Arc::new(Database::memory().unwrap());
        let provider = provider("quota-a");
        db.save_provider("codex", &provider).unwrap();
        let policy = QuotaPolicy::new(db.clone());
        let latch = policy
            .record_from_error_for_account(
                "codex",
                &provider,
                &ProxyError::UpstreamError {
                    status: 429,
                    body: Some(
                        json!({"error":{"code":"insufficient_quota"},"retry_after":120})
                            .to_string(),
                    ),
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(latch.quota_kind, "hard_quota");
        assert!(policy.is_provider_latched("codex", "quota-a").unwrap());
        let recreated = QuotaPolicy::new(db);
        assert!(recreated.is_provider_latched("codex", "quota-a").unwrap());
    }

    #[test]
    fn account_latch_does_not_disable_the_whole_provider() {
        let db = Arc::new(Database::memory().unwrap());
        let provider = provider("quota-pool");
        db.save_provider("codex", &provider).unwrap();
        let policy = QuotaPolicy::new(db.clone());
        policy
            .record_from_error_for_account(
                "codex",
                &provider,
                &ProxyError::UpstreamError {
                    status: 429,
                    body: Some("{\"error\":{\"code\":\"insufficient_quota\"}}".to_string()),
                },
                Some("credential-a"),
            )
            .unwrap();
        assert!(!policy.is_provider_latched("codex", "quota-pool").unwrap());
        assert_eq!(
            db.active_quota_latches("codex", &iso(Utc::now()))
                .unwrap()
                .first()
                .unwrap()
                .account_id,
            "credential-a"
        );
    }

    #[test]
    fn model_scoped_quota_does_not_latch_the_whole_provider() {
        let db = Arc::new(Database::memory().unwrap());
        let provider = provider("weikuwu");
        db.save_provider("codex", &provider).unwrap();
        let policy = QuotaPolicy::new(db.clone());
        let error = ProxyError::UpstreamError {
            status: 429,
            body: Some(
                json!({"error":{"message":"Weekly/Monthly Limit Exhausted. Received Model Group=GLM-5.2p"}})
                    .to_string(),
            ),
        };

        assert!(QuotaPolicy::is_hard_quota_error(&error));
        assert!(QuotaPolicy::is_model_scoped_quota_error(&error));
        assert!(policy
            .record_from_error_for_account("codex", &provider, &error, None)
            .unwrap()
            .is_none());
        assert!(!policy.is_provider_latched("codex", "weikuwu").unwrap());
    }

    #[test]
    fn transient_rate_limit_is_not_classified_as_hard_quota() {
        let error = ProxyError::UpstreamError {
            status: 429,
            body: Some(json!({"error":{"message":"Too many requests"}}).to_string()),
        };
        assert!(!QuotaPolicy::is_hard_quota_error(&error));
        assert!(!QuotaPolicy::is_model_scoped_quota_error(&error));
    }

    #[test]
    fn nvidia_worker_saturation_does_not_create_quota_latch() {
        let db = Arc::new(Database::memory().unwrap());
        let provider = provider("nvidia-busy");
        db.save_provider("codex", &provider).unwrap();
        let policy = QuotaPolicy::new(db.clone());
        let error = ProxyError::UpstreamError {
            status: 429,
            body: Some(
                json!({"error":{"message":"No deployments available for selected model, Try again in 5 seconds. Passed model=nemotron-3-ultra"}})
                    .to_string(),
            ),
        };

        assert!(QuotaPolicy::is_transient_capacity_error(&error));
        assert!(policy
            .record_from_error_for_account("codex", &provider, &error, None)
            .unwrap()
            .is_none());
        assert!(!policy.is_provider_latched("codex", "nvidia-busy").unwrap());
    }

    #[test]
    fn chinese_model_quota_reported_as_http_400_is_classified() {
        let error = ProxyError::UpstreamError {
            status: 400,
            body: Some(
                json!({"error":{"message":"当前codeplan或资源包订阅，所剩配额不足～. Received Model Group=GLM-5.2"}})
                    .to_string(),
            ),
        };
        assert!(QuotaPolicy::is_hard_quota_error(&error));
        assert!(QuotaPolicy::is_model_scoped_quota_error(&error));
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
