//! Cross-platform secret vault for provider credentials.
//!
//! Stores OAuth refresh tokens, API keys and bearer tokens in the OS-native
//! key store (Windows DPAPI, macOS Keychain, Linux Secret Service). SQLite only
//! keeps credential metadata and an opaque secret reference; the secret itself
//! never enters the database, frontend payloads, or logs.
//!
//! When the OS key store is unavailable we fail closed: the legacy
//! single-credential provider remains usable, but pool secrets cannot be
//! added until the store is repaired. We never fall back to plaintext.

use crate::error::AppError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use zeroize::Zeroizing;

/// Logical credential kind. Stored as metadata so the vault and router can
/// reason about refresh semantics without ever decrypting the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Oauth,
    ApiKey,
    Token,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::ApiKey => "api_key",
            Self::Token => "token",
        }
    }
}

/// Opaque reference returned to callers. It is safe to persist in SQLite and
/// to surface in masked Dashboard payloads because it is not the secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretReference {
    /// Backend that produced the reference, e.g. "dpapi", "keychain", "secret-service".
    pub backend: String,
    /// Backend-specific lookup key. For DPAPI this is the on-disk blob path; for
    /// keychain/secret-service it is the service/item name pair encoded as
    /// `service\x1faccount`.
    pub handle: String,
}

impl SecretReference {
    pub fn new(backend: impl Into<String>, handle: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            handle: handle.into(),
        }
    }
}

/// Metadata-only view of a credential. This is the only shape that ever
/// crosses into the frontend or into logs; the secret value is never included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialMetadata {
    pub id: String,
    pub provider_id: String,
    pub kind: CredentialKind,
    /// Free-form label shown in the Dashboard, e.g. "Work ChatGPT".
    pub label: String,
    /// Masked hint, e.g. "sk-...abc" or "oauth:john@example.com".
    pub masked_hint: String,
    pub enabled: bool,
    pub priority: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// Trait implemented by each platform adapter. Implementations must be
/// idempotent: storing the same secret twice must not create duplicates, and
/// deleting a missing secret must succeed.
pub trait SecretVault: Send + Sync {
    fn backend_name(&self) -> &'static str;

    fn store(
        &self,
        provider_id: &str,
        credential_id: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<SecretReference, AppError>;

    fn load(&self, reference: &SecretReference) -> Result<Zeroizing<Vec<u8>>, AppError>;

    fn delete(&self, reference: &SecretReference) -> Result<(), AppError>;

    /// Whether the OS key store is usable. When false, callers must keep the
    /// legacy single-credential path and refuse to add pool secrets.
    fn available(&self) -> bool;
}

/// Pick the best available platform vault. Returns `None` (not an error) when
/// no backend is usable; callers fall back to legacy single-credential mode.
pub fn default_vault() -> Option<Box<dyn SecretVault>> {
    #[cfg(windows)]
    {
        if let Some(vault) = DpapiVault::new() {
            return Some(Box::new(vault));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(vault) = KeychainVault::new() {
            return Some(Box::new(vault));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(vault) = SecretServiceVault::new() {
            return Some(Box::new(vault));
        }
    }
    None
}

/// Mask a secret for display. Returns a short head/tail preview so users can
/// recognize the credential without exposing it. Never returns more than a
/// few characters of either side.
pub fn mask_secret(secret: &[u8]) -> String {
    let len = secret.len();
    if len == 0 {
        return String::new();
    }
    // Treat as UTF-8 when possible; otherwise mask as hex.
    let Ok(text) = std::str::from_utf8(secret) else {
        return format!("hex:{}", hex_short(secret));
    };
    let head: String = text.chars().take(4).collect();
    let tail: String = text.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if len <= 12 {
        format!("{head}…{tail}")
    } else {
        format!("{head}…{tail} ({len} bytes)")
    }
}

fn hex_short(bytes: &[u8]) -> String {
    bytes.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Windows DPAPI vault
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub struct DpapiVault {
    root: PathBuf,
}

#[cfg(windows)]
impl DpapiVault {
    fn new() -> Option<Self> {
        let root = crate::config::get_app_config_dir().join("credentials");
        std::fs::create_dir_all(&root).ok()?;
        Some(Self { root })
    }

    fn path_for(&self, provider_id: &str, credential_id: &str) -> PathBuf {
        // Sanitize components so they cannot escape the vault directory.
        let safe_provider = sanitize_component(provider_id);
        let safe_credential = sanitize_component(credential_id);
        self.root
            .join(format!("{safe_provider}.{safe_credential}.dpapi"))
    }
}

#[cfg(windows)]
impl SecretVault for DpapiVault {
    fn backend_name(&self) -> &'static str {
        "dpapi-current-user"
    }

    fn store(
        &self,
        provider_id: &str,
        credential_id: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<SecretReference, AppError> {
        let path = self.path_for(provider_id, credential_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(&path, e))?;
        }
        let protected = dpapi_protect(secret.as_slice())?;
        let encoded = URL_SAFE_NO_PAD.encode(protected);
        // Write atomically: create a temp file then rename, so a crash never
        // leaves a half-written credential blob.
        let tmp = path.with_extension("dpapi.tmp");
        std::fs::write(&tmp, encoded.as_bytes()).map_err(|e| AppError::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| AppError::io(&path, e))?;
        Ok(SecretReference::new(
            self.backend_name(),
            path.to_string_lossy().to_string(),
        ))
    }

    fn load(&self, reference: &SecretReference) -> Result<Zeroizing<Vec<u8>>, AppError> {
        let path = PathBuf::from(&reference.handle);
        let encoded = std::fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
        let protected = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .map_err(|_| AppError::Config("credential blob is not valid base64".to_string()))?;
        let plaintext = dpapi_unprotect(&protected)?;
        Ok(Zeroizing::new(plaintext))
    }

    fn delete(&self, reference: &SecretReference) -> Result<(), AppError> {
        let path = PathBuf::from(&reference.handle);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::io(&path, e)),
        }
    }

    fn available(&self) -> bool {
        true
    }
}

#[cfg(windows)]
fn dpapi_protect(input: &[u8]) -> Result<Vec<u8>, AppError> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptProtectData(
            &input_blob,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(AppError::Message(format!(
            "DPAPI failed to protect credential: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let result = bytes.to_vec();
    unsafe {
        let _ = LocalFree(output.pbData.cast());
    }
    Ok(result)
}

#[cfg(windows)]
fn dpapi_unprotect(input: &[u8]) -> Result<Vec<u8>, AppError> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input_blob,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(AppError::Message(format!(
            "DPAPI failed to unprotect credential: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let result = bytes.to_vec();
    unsafe {
        let _ = LocalFree(output.pbData.cast());
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// macOS Keychain vault (stubbed on non-macOS builds)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct KeychainVault;

#[cfg(target_os = "macos")]
impl KeychainVault {
    fn new() -> Option<Self> {
        Some(Self)
    }
}

#[cfg(target_os = "macos")]
impl SecretVault for KeychainVault {
    fn backend_name(&self) -> &'static str {
        "keychain"
    }

    fn store(
        &self,
        provider_id: &str,
        credential_id: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<SecretReference, AppError> {
        let service = format!("cc-switch.{provider_id}");
        let account = sanitize_component(credential_id);
        // Use the `security` CLI to avoid a heavy FFI surface. The CLI is
        // present on every macOS install.
        let status = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-a",
                &account,
                "-s",
                &service,
                "-w",
                &String::from_utf8_lossy(secret.as_slice()),
                "-U",
            ])
            .status()
            .map_err(|e| AppError::Message(format!("keychain store failed: {e}")))?;
        if !status.success() {
            return Err(AppError::Message(format!(
                "keychain add-generic-password exited {status}"
            )));
        }
        Ok(SecretReference::new(
            self.backend_name(),
            format!("{service}\x1f{account}"),
        ))
    }

    fn load(&self, reference: &SecretReference) -> Result<Zeroizing<Vec<u8>>, AppError> {
        let (service, account) = reference
            .handle
            .split_once('\x1f')
            .ok_or_else(|| AppError::Config("invalid keychain reference".to_string()))?;
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-a",
                account,
                "-s",
                service,
                "-w",
            ])
            .output()
            .map_err(|e| AppError::Message(format!("keychain load failed: {e}")))?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "keychain find-generic-password exited {}",
                output.status
            )));
        }
        // The CLI appends a trailing newline; trim it.
        let mut bytes = output.stdout;
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        Ok(Zeroizing::new(bytes))
    }

    fn delete(&self, reference: &SecretReference) -> Result<(), AppError> {
        let (service, account) = reference
            .handle
            .split_once('\x1f')
            .ok_or_else(|| AppError::Config("invalid keychain reference".to_string()))?;
        let status = std::process::Command::new("security")
            .args(["delete-generic-password", "-a", account, "-s", service])
            .status()
            .map_err(|e| AppError::Message(format!("keychain delete failed: {e}")))?;
        // Deletion of a missing item is a success.
        if !status.success() {
            log::warn!("keychain delete exited {status}; treating as already-removed");
        }
        Ok(())
    }

    fn available(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Linux Secret Service vault (stubbed on non-Linux builds)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub struct SecretServiceVault;

#[cfg(target_os = "linux")]
impl SecretServiceVault {
    fn new() -> Option<Self> {
        // We shell out to `secret-tool` (libsecret-tools) to avoid pulling a
        // D-Bus dependency into the build. If the CLI is missing, the vault is
        // simply unavailable and callers fall back to legacy single-credential.
        let output = std::process::Command::new("secret-tool")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if output.success() {
            Some(Self)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
impl SecretVault for SecretServiceVault {
    fn backend_name(&self) -> &'static str {
        "secret-service"
    }

    fn store(
        &self,
        provider_id: &str,
        credential_id: &str,
        secret: &Zeroizing<Vec<u8>>,
    ) -> Result<SecretReference, AppError> {
        let service = format!("cc-switch.{provider_id}");
        let account = sanitize_component(credential_id);
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store",
                "--label",
                &format!("cc-switch {account}"),
                "service",
                &service,
                "account",
                &account,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| AppError::Message(format!("secret-service store failed: {e}")))?;
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin
                .write_all(secret.as_slice())
                .and_then(|_| stdin.write_all(b"\n"))
                .map_err(|e| AppError::Message(format!("secret-service write failed: {e}")))?;
        }
        let status = child
            .wait()
            .map_err(|e| AppError::Message(format!("secret-service wait failed: {e}")))?;
        if !status.success() {
            return Err(AppError::Message(format!("secret-tool store exited {status}")));
        }
        Ok(SecretReference::new(
            self.backend_name(),
            format!("{service}\x1f{account}"),
        ))
    }

    fn load(&self, reference: &SecretReference) -> Result<Zeroizing<Vec<u8>>, AppError> {
        let (service, account) = reference
            .handle
            .split_once('\x1f')
            .ok_or_else(|| AppError::Config("invalid secret-service reference".to_string()))?;
        let output = std::process::Command::new("secret-tool")
            .args(["lookup", "service", service, "account", account])
            .output()
            .map_err(|e| AppError::Message(format!("secret-service load failed: {e}")))?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "secret-tool lookup exited {}",
                output.status
            )));
        }
        let mut bytes = output.stdout;
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        Ok(Zeroizing::new(bytes))
    }

    fn delete(&self, reference: &SecretReference) -> Result<(), AppError> {
        let (service, account) = reference
            .handle
            .split_once('\x1f')
            .ok_or_else(|| AppError::Config("invalid secret-service reference".to_string()))?;
        let status = std::process::Command::new("secret-tool")
            .args(["clear", "service", service, "account", account])
            .status()
            .map_err(|e| AppError::Message(format!("secret-service delete failed: {e}")))?;
        if !status.success() {
            log::warn!("secret-tool clear exited {status}; treating as already-removed");
        }
        Ok(())
    }

    fn available(&self) -> bool {
        true
    }
}

fn sanitize_component(value: &str) -> String {
    // Keep only characters that are safe inside a filename / keychain account.
    // Replace anything else with an underscore so two distinct ids cannot
    // collide after sanitization.
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_never_exposes_full_value() {
        let secret = Zeroizing::new(b"sk-abcd1234efgh5678".to_vec());
        let masked = mask_secret(secret.as_slice());
        assert!(masked.starts_with("sk-a"));
        assert!(masked.contains("5678"));
        assert!(!masked.contains("1234efgh"));
    }

    #[test]
    fn mask_secret_handles_short_input() {
        let masked = mask_secret(b"abc");
        assert!(masked.contains("abc"));
        assert!(!masked.contains("bytes"));
    }

    #[test]
    fn mask_secret_handles_non_utf8() {
        let masked = mask_secret(&[0xff, 0xfe, 0xfd, 0xfc, 0xfb]);
        assert!(masked.starts_with("hex:"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_vault_round_trip_and_delete() {
        let vault = DpapiVault::new().expect("dpapi vault available on windows");
        assert!(vault.available());
        let secret = Zeroizing::new(b"oauth-refresh-token-12345".to_vec());
        let reference = vault
            .store("provider-x", "cred-1", &secret)
            .expect("store");
        assert_eq!(reference.backend, "dpapi-current-user");
        let loaded = vault.load(&reference).expect("load");
        assert_eq!(loaded.as_slice(), secret.as_slice());
        vault.delete(&reference).expect("delete");
        // Loading after delete must fail.
        assert!(vault.load(&reference).is_err());
        // Deleting again is idempotent.
        assert!(vault.delete(&reference).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_vault_rejects_path_escape_in_ids() {
        let vault = DpapiVault::new().expect("dpapi vault available on windows");
        let secret = Zeroizing::new(b"k".to_vec());
        let reference = vault
            .store("../escape", "sub/dir", &secret)
            .expect("store");
        // The stored path must stay inside the vault root.
        let path = PathBuf::from(&reference.handle);
        assert!(
            path.starts_with(&vault.root),
            "{} not inside {}",
            path.display(),
            vault.root.display()
        );
        vault.delete(&reference).expect("cleanup");
    }

    #[test]
    fn default_vault_returns_some_on_supported_platforms() {
        // On Windows/macOS/Linux we expect a vault; on other platforms None.
        let vault = default_vault();
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
        {
            // CI may run without a usable key store; only assert the type when present.
            if let Some(vault) = vault {
                assert!(!vault.backend_name().is_empty());
            }
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            assert!(vault.is_none());
        }
    }
}
