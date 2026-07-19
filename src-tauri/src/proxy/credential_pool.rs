//! Secure, quota-aware provider credential pools.
//!
//! SQLite contains only routable metadata and opaque SecretVault references.
//! Secret material is returned only to the forwarder and is never serialized.

use super::secret_vault::{
    default_vault, mask_secret, CredentialKind, SecretReference, SecretVault,
};
use crate::database::{CredentialQuotaSnapshotRow, Database, ProviderCredentialRow};
use crate::error::AppError;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredentialQuotaView {
    pub quota_kind: String,
    pub remaining_ratio: Option<f64>,
    pub used_ratio: Option<f64>,
    pub reset_at: Option<String>,
    pub detail: Value,
    pub queried_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderCredentialView {
    pub id: String,
    pub app_type: String,
    pub provider_id: String,
    pub kind: CredentialKind,
    pub label: String,
    pub masked_hint: String,
    pub enabled: bool,
    pub priority: i32,
    pub auth_header: String,
    pub auth_prefix: String,
    pub public_metadata: Value,
    pub status: String,
    pub last_error_code: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub quotas: Vec<CredentialQuotaView>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveProviderCredentialRequest {
    pub id: Option<String>,
    pub app_type: String,
    pub provider_id: String,
    pub kind: CredentialKind,
    pub label: String,
    pub secret: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_priority")]
    pub priority: i32,
    pub auth_header: Option<String>,
    pub auth_prefix: Option<String>,
    #[serde(default)]
    pub public_metadata: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveCredentialQuotaRequest {
    pub credential_id: String,
    pub quota_kind: String,
    pub remaining_ratio: Option<f64>,
    pub used_ratio: Option<f64>,
    pub reset_at: Option<String>,
    #[serde(default)]
    pub detail: Value,
}

pub(crate) struct ResolvedCredential {
    pub id: String,
    pub header_name: String,
    pub header_value: Zeroizing<Vec<u8>>,
}

pub(crate) struct CredentialPool {
    db: Arc<Database>,
    vault: Option<Arc<dyn SecretVault>>,
}

impl CredentialPool {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            vault: default_vault().map(Arc::from),
        }
    }

    #[cfg(test)]
    fn with_vault(db: Arc<Database>, vault: Arc<dyn SecretVault>) -> Self {
        Self {
            db,
            vault: Some(vault),
        }
    }

    pub(crate) fn vault_available(&self) -> bool {
        self.vault.as_ref().is_some_and(|vault| vault.available())
    }

    pub(crate) fn list(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<Vec<ProviderCredentialView>, AppError> {
        self.db
            .provider_credentials(app_type, provider_id)?
            .into_iter()
            .map(|row| self.view(row))
            .collect()
    }

    pub(crate) fn enabled_count(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<usize, AppError> {
        self.db
            .enabled_provider_credential_count(app_type, provider_id)
    }

    pub(crate) fn save(
        &self,
        request: SaveProviderCredentialRequest,
    ) -> Result<ProviderCredentialView, AppError> {
        validate_request(&request)?;
        let now = now_iso();
        let id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = self.db.provider_credential(&id)?;
        if let Some(row) = existing.as_ref() {
            if row.app_type != request.app_type || row.provider_id != request.provider_id {
                return Err(AppError::InvalidInput(
                    "a credential cannot be moved to another provider".to_string(),
                ));
            }
        }

        let (reference, masked_hint, new_reference) = match request.secret {
            Some(secret) => {
                let secret = Zeroizing::new(secret.into_bytes());
                if secret.is_empty() {
                    return Err(AppError::InvalidInput(
                        "credential secret cannot be empty".to_string(),
                    ));
                }
                let vault = self.vault()?;
                let vault_object_id = format!("{id}-{}", Uuid::new_v4());
                let reference = vault.store(&request.provider_id, &vault_object_id, &secret)?;
                let masked = mask_secret(&secret);
                (reference, masked, true)
            }
            None => {
                let existing = existing.as_ref().ok_or_else(|| {
                    AppError::InvalidInput("secret is required for a new credential".to_string())
                })?;
                (
                    SecretReference::new(
                        existing.secret_backend.clone(),
                        existing.secret_handle.clone(),
                    ),
                    existing.masked_hint.clone(),
                    false,
                )
            }
        };

        let row = ProviderCredentialRow {
            id: id.clone(),
            app_type: request.app_type,
            provider_id: request.provider_id,
            kind: request.kind.as_str().to_string(),
            label: request.label.trim().to_string(),
            masked_hint,
            secret_backend: reference.backend.clone(),
            secret_handle: reference.handle.clone(),
            enabled: request.enabled,
            priority: request.priority,
            auth_header: request
                .auth_header
                .unwrap_or_else(|| "authorization".to_string()),
            auth_prefix: request.auth_prefix.unwrap_or_else(|| "Bearer ".to_string()),
            public_metadata: request.public_metadata.to_string(),
            status: existing
                .as_ref()
                .map(|value| value.status.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            last_error_code: existing
                .as_ref()
                .and_then(|value| value.last_error_code.clone()),
            last_used_at: existing
                .as_ref()
                .and_then(|value| value.last_used_at.clone()),
            created_at: existing
                .as_ref()
                .map(|value| value.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        if let Err(error) = self.db.upsert_provider_credential(&row) {
            if new_reference {
                let _ = self.vault()?.delete(&reference);
            }
            return Err(error);
        }
        if new_reference {
            if let Some(old) = existing.as_ref() {
                let old_reference =
                    SecretReference::new(old.secret_backend.clone(), old.secret_handle.clone());
                if old_reference.handle != reference.handle {
                    if let Err(error) = self.vault()?.delete(&old_reference) {
                        log::warn!(
                            "credential {} updated but obsolete vault cleanup failed: {}",
                            id,
                            sanitize_error(&error.to_string())
                        );
                    }
                }
            }
        }
        self.view(row)
    }

    pub(crate) fn delete(&self, credential_id: &str) -> Result<bool, AppError> {
        let Some(row) = self.db.provider_credential(credential_id)? else {
            return Ok(false);
        };
        self.vault()?
            .delete(&SecretReference::new(row.secret_backend, row.secret_handle))?;
        self.db.delete_provider_credential(credential_id)
    }

    pub(crate) fn set_status(
        &self,
        credential_id: &str,
        enabled: bool,
        status: &str,
        error_code: Option<&str>,
    ) -> Result<(), AppError> {
        if !matches!(status, "unknown" | "valid" | "invalid" | "warning") {
            return Err(AppError::InvalidInput(
                "invalid credential status".to_string(),
            ));
        }
        self.db.update_provider_credential_status(
            credential_id,
            enabled,
            status,
            error_code,
            &now_iso(),
        )
    }

    pub(crate) fn save_quota(&self, request: SaveCredentialQuotaRequest) -> Result<(), AppError> {
        validate_ratio(request.remaining_ratio)?;
        validate_ratio(request.used_ratio)?;
        if request.quota_kind.trim().is_empty() {
            return Err(AppError::InvalidInput("quota kind is required".to_string()));
        }
        self.db
            .upsert_credential_quota_snapshot(&CredentialQuotaSnapshotRow {
                credential_id: request.credential_id,
                quota_kind: request.quota_kind,
                remaining_ratio: request.remaining_ratio,
                used_ratio: request.used_ratio,
                reset_at: request.reset_at,
                detail_json: request.detail.to_string(),
                queried_at: now_iso(),
            })
    }

    pub(crate) fn resolve(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<Option<ResolvedCredential>, AppError> {
        let enabled_count = self.enabled_count(app_type, provider_id)?;
        if enabled_count == 0 {
            return Ok(None);
        }
        let now = now_iso();
        let Some(row) = self
            .db
            .available_provider_credential(app_type, provider_id, &now)?
        else {
            return Err(AppError::Message(
                "all credentials for this provider are disabled or quota-latched".to_string(),
            ));
        };
        let secret = self.vault()?.load(&SecretReference::new(
            row.secret_backend.clone(),
            row.secret_handle.clone(),
        ))?;
        let mut value = Zeroizing::new(Vec::with_capacity(row.auth_prefix.len() + secret.len()));
        value.extend_from_slice(row.auth_prefix.as_bytes());
        value.extend_from_slice(&secret);
        self.db.touch_provider_credential(&row.id, &now)?;
        Ok(Some(ResolvedCredential {
            id: row.id,
            header_name: row.auth_header,
            header_value: value,
        }))
    }

    fn view(&self, row: ProviderCredentialRow) -> Result<ProviderCredentialView, AppError> {
        let quotas = self
            .db
            .credential_quota_snapshots(&row.id)?
            .into_iter()
            .map(|quota| CredentialQuotaView {
                quota_kind: quota.quota_kind,
                remaining_ratio: quota.remaining_ratio,
                used_ratio: quota.used_ratio,
                reset_at: quota.reset_at,
                detail: serde_json::from_str(&quota.detail_json).unwrap_or(Value::Null),
                queried_at: quota.queried_at,
            })
            .collect();
        Ok(ProviderCredentialView {
            id: row.id,
            app_type: row.app_type,
            provider_id: row.provider_id,
            kind: parse_kind(&row.kind)?,
            label: row.label,
            masked_hint: row.masked_hint,
            enabled: row.enabled,
            priority: row.priority,
            auth_header: row.auth_header,
            auth_prefix: row.auth_prefix,
            public_metadata: serde_json::from_str(&row.public_metadata).unwrap_or(Value::Null),
            status: row.status,
            last_error_code: row.last_error_code,
            last_used_at: row.last_used_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            quotas,
        })
    }

    fn vault(&self) -> Result<&Arc<dyn SecretVault>, AppError> {
        self.vault
            .as_ref()
            .filter(|vault| vault.available())
            .ok_or_else(|| {
                AppError::Message(
                    "secure credential storage is unavailable; legacy provider auth remains active"
                        .to_string(),
                )
            })
    }
}

fn validate_request(request: &SaveProviderCredentialRequest) -> Result<(), AppError> {
    if request.app_type.trim().is_empty()
        || request.provider_id.trim().is_empty()
        || request.label.trim().is_empty()
    {
        return Err(AppError::InvalidInput(
            "app type, provider and label are required".to_string(),
        ));
    }
    if !request.public_metadata.is_object() && !request.public_metadata.is_null() {
        return Err(AppError::InvalidInput(
            "credential public metadata must be an object".to_string(),
        ));
    }
    let header = request.auth_header.as_deref().unwrap_or("authorization");
    http::HeaderName::from_bytes(header.as_bytes())
        .map_err(|_| AppError::InvalidInput("invalid credential header name".to_string()))?;
    if request
        .auth_prefix
        .as_deref()
        .is_some_and(|value| value.contains(['\r', '\n']))
    {
        return Err(AppError::InvalidInput(
            "credential header prefix cannot contain newlines".to_string(),
        ));
    }
    Ok(())
}

fn validate_ratio(value: Option<f64>) -> Result<(), AppError> {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return Err(AppError::InvalidInput(
            "quota ratios must be between zero and one".to_string(),
        ));
    }
    Ok(())
}

fn parse_kind(value: &str) -> Result<CredentialKind, AppError> {
    match value {
        "oauth" => Ok(CredentialKind::Oauth),
        "api_key" => Ok(CredentialKind::ApiKey),
        "token" => Ok(CredentialKind::Token),
        _ => Err(AppError::Config(format!(
            "unknown credential kind in database: {value}"
        ))),
    }
}

fn default_true() -> bool {
    true
}

fn default_priority() -> i32 {
    100
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn sanitize_error(value: &str) -> String {
    value.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryVault {
        secrets: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl SecretVault for MemoryVault {
        fn backend_name(&self) -> &'static str {
            "memory-test"
        }

        fn store(
            &self,
            provider_id: &str,
            credential_id: &str,
            secret: &Zeroizing<Vec<u8>>,
        ) -> Result<SecretReference, AppError> {
            let handle = format!("{provider_id}/{credential_id}");
            self.secrets
                .lock()
                .unwrap()
                .insert(handle.clone(), secret.to_vec());
            Ok(SecretReference::new(self.backend_name(), handle))
        }

        fn load(&self, reference: &SecretReference) -> Result<Zeroizing<Vec<u8>>, AppError> {
            self.secrets
                .lock()
                .unwrap()
                .get(&reference.handle)
                .cloned()
                .map(Zeroizing::new)
                .ok_or_else(|| AppError::Message("missing test secret".to_string()))
        }

        fn delete(&self, reference: &SecretReference) -> Result<(), AppError> {
            self.secrets.lock().unwrap().remove(&reference.handle);
            Ok(())
        }

        fn available(&self) -> bool {
            true
        }
    }

    #[test]
    fn secret_is_vaulted_and_never_returned_in_metadata() {
        let db = Arc::new(Database::memory().unwrap());
        db.save_provider(
            "codex",
            &Provider::with_id(
                "provider-a".to_string(),
                "Provider A".to_string(),
                serde_json::json!({"base_url":"https://example.invalid/v1"}),
                None,
            ),
        )
        .unwrap();
        let pool = CredentialPool::with_vault(db.clone(), Arc::new(MemoryVault::default()));
        let secret = "sk-super-secret-value";
        let saved = pool
            .save(SaveProviderCredentialRequest {
                id: None,
                app_type: "codex".to_string(),
                provider_id: "provider-a".to_string(),
                kind: CredentialKind::ApiKey,
                label: "Work".to_string(),
                secret: Some(secret.to_string()),
                enabled: true,
                priority: 10,
                auth_header: Some("authorization".to_string()),
                auth_prefix: Some("Bearer ".to_string()),
                public_metadata: serde_json::json!({}),
            })
            .unwrap();
        assert!(!serde_json::to_string(&saved).unwrap().contains(secret));
        assert!(!db.export_sql_string().unwrap().contains(secret));

        let resolved = pool.resolve("codex", "provider-a").unwrap().unwrap();
        assert_eq!(
            resolved.header_value.as_slice(),
            b"Bearer sk-super-secret-value"
        );
        assert!(pool.delete(&saved.id).unwrap());
        assert!(pool.list("codex", "provider-a").unwrap().is_empty());
    }
}
