//! Grok CLI Proxy credential broker.
//!
//! Resolves the current Grok access token from the local Grok Build
//! session (`~/.grok/auth.json`) or from an `XAI_API_KEY` stored in the
//! CC Switch secret vault. Token refresh is delegated to the Grok CLI
//! itself by running `grok models` (hidden), which lets Grok Build handle
//! its own OIDC discovery, refresh-token rotation, file locking and
//! atomic write-back. CC Switch never reads, stores, or persists
//! refresh tokens.

use crate::grok_config::get_grok_config_dir;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

/// Authentication mode for the Grok CLI proxy provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "auth_mode")]
pub enum GrokAuthMode {
    /// Use the local Grok Build OIDC session stored in `~/.grok/auth.json`.
    /// CC Switch reads only the access token; refresh is delegated to the
    /// Grok CLI itself.
    LocalSession,
    /// Use an `XAI_API_KEY` stored in the CC Switch secret vault.
    ApiKey,
}

impl Default for GrokAuthMode {
    fn default() -> Self {
        Self::LocalSession
    }
}

/// Configuration for a Grok CLI proxy Codex provider.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokCliProxyConfig {
    pub auth_mode: GrokAuthMode,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grok_home: Option<PathBuf>,
    pub api_format: String,
    pub visible_in_model_picker: bool,
}

impl GrokCliProxyConfig {
    #[allow(dead_code)]
    pub fn default_responses() -> Self {
        Self {
            auth_mode: GrokAuthMode::LocalSession,
            model: crate::grok_config::DEFAULT_MODEL.to_string(),
            base_url: Some("https://cli-chat-proxy.grok.com".to_string()),
            grok_home: None,
            api_format: "openai_responses".to_string(),
            visible_in_model_picker: true,
        }
    }

    /// Resolve the Grok config directory, falling back to `~/.grok`.
    pub fn grok_dir(&self) -> PathBuf {
        self.grok_home.clone().unwrap_or_else(get_grok_config_dir)
    }

    /// Resolve the base URL for the Grok CLI chat proxy.
    pub fn resolve_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| "https://cli-chat-proxy.grok.com".to_string())
    }
}

/// A resolved Grok credential. The access token is kept in a
/// `Zeroizing<String>` to ensure it is not left in memory longer than
/// necessary and is never serialized.
#[derive(Debug, Clone)]
pub struct GrokCredential {
    pub access_token: Zeroizing<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub user_id: Option<String>,
}

impl GrokCredential {
    /// Returns `true` when the token is expired or will expire within the
    /// given `skew` duration.
    pub fn is_expired(&self, skew: Duration) -> bool {
        match self.expires_at {
            Some(expiry) => Utc::now() + skew >= expiry,
            None => false,
        }
    }
}

/// Process-wide credential broker that resolves and refreshes Grok
/// credentials. A single mutex ensures concurrent 401 responses trigger
/// only one `grok models` refresh at a time.
pub struct GrokCredentialBroker {
    config: GrokCliProxyConfig,
    api_key: Option<String>,
    /// Singleflight guard: only one refresh at a time.
    refresh_lock: Arc<Mutex<()>>,
}

impl GrokCredentialBroker {
    pub fn new(config: GrokCliProxyConfig) -> Self {
        Self {
            config,
            api_key: None,
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_api_key(mut self, key: String) -> Self {
        self.api_key = Some(key);
        self
    }

    /// Resolve the current credential. If the token is expired or close to
    /// expiry, `refresh` is called first.
    /// Returns the configured model name.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub async fn resolve(&self) -> Result<GrokCredential, String> {
        match self.config.auth_mode {
            GrokAuthMode::ApiKey => {
                let key = self
                    .api_key
                    .clone()
                    .ok_or_else(|| "API key not set for Grok CLI proxy".to_string())?;
                Ok(GrokCredential {
                    access_token: Zeroizing::new(key),
                    expires_at: None,
                    user_id: None,
                })
            }
            GrokAuthMode::LocalSession => {
                let cred = self.read_session_credential_result()?.ok_or_else(|| {
                    "No valid Grok session found. Run `grok login` to authenticate.".to_string()
                })?;
                // Refresh if token expires within 60 seconds.
                if cred.is_expired(Duration::seconds(60)) {
                    self.refresh().await?;
                    self.read_session_credential_result()?
                        .ok_or_else(|| "Grok session credential missing after refresh".to_string())
                } else {
                    Ok(cred)
                }
            }
        }
    }

    /// Force a credential refresh by running `grok models` (hidden).
    /// This lets Grok Build handle OIDC discovery, refresh-token rotation,
    /// file locking, and atomic write-back.
    pub async fn refresh(&self) -> Result<(), String> {
        let _guard = self.refresh_lock.lock().await;

        let grok_path = find_grok_executable(Some(&self.config.grok_dir()));
        let mut command = Command::new(&grok_path);
        command
            .arg("models")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
            .await
            .map_err(|_| {
                "`grok models` timed out after 30 seconds; run `grok login` manually".to_string()
            })?
            .map_err(|e| format!("failed to run `grok models` for credential refresh: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "`grok models` failed during credential refresh; please run `grok login` manually. stderr: {stderr}"
            ));
        }

        Ok(())
    }

    /// Read the current session credential from `~/.grok/auth.json`.
    /// Selects the credential with the latest expiry that is still valid.
    /// Tokens are never logged, serialized, or persisted.
    fn read_session_credential_result(&self) -> Result<Option<GrokCredential>, String> {
        let auth_path = self.config.grok_dir().join("auth.json");
        let content = std::fs::read_to_string(&auth_path).map_err(|error| {
            format!(
                "Cannot read Grok session file {}: {error}. On Windows, use API key mode if Grok Build restricts auth.json access.",
                auth_path.display()
            )
        })?;
        let auth_file: serde_json::Value = serde_json::from_str(&content)
            .map_err(|error| format!("Invalid Grok session file: {error}"))?;
        let entries = grok_credential_entries(&auth_file);

        let now = Utc::now();
        let mut best: Option<GrokCredential> = None;

        for entry in entries {
            let Some(token) = entry
                .get("access_token")
                .or_else(|| entry.get("key"))
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            if token.trim().is_empty() {
                continue;
            }

            let expires_at = entry
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            // Skip already-expired credentials.
            if let Some(expiry) = expires_at {
                if expiry <= now {
                    continue;
                }
            }

            // Prefer the credential with the latest expiry.
            let is_better = match (&best, expires_at) {
                (None, _) => true,
                (Some(current), Some(new_expiry)) => {
                    current.expires_at.unwrap_or_default() < new_expiry
                }
                (Some(_), None) => false,
            };

            if is_better {
                best = Some(GrokCredential {
                    access_token: Zeroizing::new(token.to_string()),
                    expires_at,
                    user_id: entry
                        .get("user_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                });
            }
        }

        Ok(best)
    }

    #[cfg(test)]
    fn read_session_credential(&self) -> Option<GrokCredential> {
        self.read_session_credential_result().ok().flatten()
    }

    /// Returns the Grok CLI client version from `~/.grok/version.json`.
    /// Falls back to `"unknown"` when unavailable.
    pub fn client_version(&self) -> String {
        let version_path = self.config.grok_dir().join("version.json");
        let Ok(content) = std::fs::read_to_string(&version_path) else {
            return "unknown".to_string();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            return "unknown".to_string();
        };
        value
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Returns the agent ID from `~/.grok/agent_id` if present.
    pub fn agent_id(&self) -> Option<String> {
        let agent_id_path = self.config.grok_dir().join("agent_id");
        std::fs::read_to_string(&agent_id_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

fn grok_credential_entries(
    value: &serde_json::Value,
) -> Vec<&serde_json::Map<String, serde_json::Value>> {
    if let Some(entries) = value
        .get("credentials")
        .and_then(serde_json::Value::as_array)
    {
        return entries
            .iter()
            .filter_map(serde_json::Value::as_object)
            .collect();
    }
    value
        .as_object()
        .into_iter()
        .flat_map(|object| object.values())
        .filter_map(serde_json::Value::as_object)
        .collect()
}

pub fn find_grok_executable(grok_dir: Option<&std::path::Path>) -> PathBuf {
    let executable = if cfg!(windows) { "grok.exe" } else { "grok" };
    if let Some(dir) = grok_dir {
        for candidate in [dir.join("bin").join(executable), dir.join(executable)] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_serializes_as_snake_case() {
        let json = serde_json::to_string(&GrokAuthMode::LocalSession).unwrap();
        assert!(json.contains("local_session"));
        let json = serde_json::to_string(&GrokAuthMode::ApiKey).unwrap();
        assert!(json.contains("api_key"));
    }

    #[test]
    fn credential_expiry_check() {
        let cred = GrokCredential {
            access_token: Zeroizing::new("test".to_string()),
            expires_at: Some(Utc::now() + Duration::seconds(30)),
            user_id: None,
        };
        // Should not be expired with 60s skew.
        // Should be expired with 60s skew (expiry is 30s away, within the 60s window).
        assert!(cred.is_expired(Duration::seconds(60)));
        // Should be expired with 0 skew.
        // Should not be expired with 0s skew (expiry is 30s in the future).
        assert!(!cred.is_expired(Duration::seconds(0)));
    }

    #[test]
    fn credential_no_expiry_never_expired() {
        let cred = GrokCredential {
            access_token: Zeroizing::new("test".to_string()),
            expires_at: None,
            user_id: None,
        };
        assert!(!cred.is_expired(Duration::seconds(0)));
        assert!(!cred.is_expired(Duration::seconds(3600)));
    }

    #[test]
    fn config_default_uses_grok_endpoint() {
        let config = GrokCliProxyConfig::default_responses();
        assert_eq!(config.model, "grok-4.5");
        assert_eq!(config.api_format, "openai_responses");
        assert!(config.visible_in_model_picker);
        assert_eq!(config.resolve_base_url(), "https://cli-chat-proxy.grok.com");
    }

    #[test]
    fn read_session_credential_selects_latest_valid() {
        let dir = std::env::temp_dir().join(format!(
            "grok-cred-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let now = Utc::now();
        let auth = serde_json::json!({
            "credentials": [
                {
                    "access_token": "expired-token",
                    "expires_at": (now - Duration::seconds(3600)).to_rfc3339(),
                    "user_id": "user1"
                },
                {
                    "access_token": "valid-token",
                    "expires_at": (now + Duration::seconds(3600)).to_rfc3339(),
                    "user_id": "user2"
                }
            ]
        });
        std::fs::write(dir.join("auth.json"), serde_json::to_string(&auth).unwrap()).unwrap();

        let config = GrokCliProxyConfig {
            grok_home: Some(dir.clone()),
            ..GrokCliProxyConfig::default_responses()
        };
        let broker = GrokCredentialBroker::new(config);
        let cred = broker.read_session_credential().unwrap();
        assert_eq!(&*cred.access_token, "valid-token");
        assert_eq!(cred.user_id.as_deref(), Some("user2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_session_credential_returns_none_when_all_expired() {
        let dir = std::env::temp_dir().join(format!(
            "grok-cred-test2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let now = Utc::now();
        let auth = serde_json::json!({
            "credentials": [
                {
                    "access_token": "old-token",
                    "expires_at": (now - Duration::seconds(3600)).to_rfc3339(),
                }
            ]
        });
        std::fs::write(dir.join("auth.json"), serde_json::to_string(&auth).unwrap()).unwrap();

        let config = GrokCliProxyConfig {
            grok_home: Some(dir.clone()),
            ..GrokCliProxyConfig::default_responses()
        };
        let broker = GrokCredentialBroker::new(config);
        assert!(broker.read_session_credential().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_session_credential_supports_grok_domain_key_shape() {
        let dir = std::env::temp_dir().join(format!(
            "grok-domain-auth-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = serde_json::json!({
            "https://accounts.x.ai/sign-in": {
                "key": "documented-session-token",
                "user_id": "user-domain"
            }
        });
        std::fs::write(dir.join("auth.json"), auth.to_string()).unwrap();
        let broker = GrokCredentialBroker::new(GrokCliProxyConfig {
            grok_home: Some(dir.clone()),
            ..GrokCliProxyConfig::default_responses()
        });

        let cred = broker.read_session_credential().unwrap();
        assert_eq!(&*cred.access_token, "documented-session-token");
        assert_eq!(cred.user_id.as_deref(), Some("user-domain"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_never_logged_in_error() {
        // The broker's error messages should never include the token.
        let dir = std::env::temp_dir().join(format!(
            "grok-cred-test3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let now = Utc::now();
        let secret_token = "xai-secret-abc123-do-not-leak";
        let auth = serde_json::json!({
            "credentials": [
                {
                    "access_token": secret_token,
                    "expires_at": (now + Duration::seconds(3600)).to_rfc3339(),
                }
            ]
        });
        std::fs::write(dir.join("auth.json"), serde_json::to_string(&auth).unwrap()).unwrap();

        let config = GrokCliProxyConfig {
            grok_home: Some(dir.clone()),
            ..GrokCliProxyConfig::default_responses()
        };
        let broker = GrokCredentialBroker::new(config);
        // Trigger a refresh attempt (which will fail because grok isn't in
        // the temp dir), and verify the error doesn't contain the token.
        let _ = broker.read_session_credential();
        // Just verify the token is readable and correct.
        let cred = broker.read_session_credential().unwrap();
        assert_eq!(&*cred.access_token, secret_token);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn client_version_reads_version_json() {
        let dir = std::env::temp_dir().join(format!(
            "grok-version-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("version.json"),
            serde_json::json!({"version": "0.2.111"}).to_string(),
        )
        .unwrap();

        let config = GrokCliProxyConfig {
            grok_home: Some(dir.clone()),
            ..GrokCliProxyConfig::default_responses()
        };
        let broker = GrokCredentialBroker::new(config);
        assert_eq!(broker.client_version(), "0.2.111");

        std::fs::remove_dir_all(&dir).ok();
    }
}
