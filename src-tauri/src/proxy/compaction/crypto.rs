use crate::error::AppError;
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use zeroize::Zeroizing;

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

pub(crate) struct JournalCipher {
    key: Zeroizing<Vec<u8>>,
    protection: String,
}

impl std::fmt::Debug for JournalCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalCipher")
            .field("protection", &self.protection)
            .finish_non_exhaustive()
    }
}

impl JournalCipher {
    pub(crate) fn load_or_create(root: &Path) -> Result<Self, AppError> {
        if let Some(key) = explicit_key()? {
            return Self::from_key(key, "environment");
        }

        #[cfg(windows)]
        {
            let key_path = root.join("compaction-key.dpapi");
            let key = load_or_create_dpapi_key(&key_path)?;
            Self::from_key(key, "dpapi-current-user")
        }

        #[cfg(not(windows))]
        {
            let _ = root;
            Err(AppError::Config(
                "No secure compaction key store is available on this platform; set CC_SWITCH_COMPACTION_MASTER_KEY"
                    .to_string(),
            ))
        }
    }

    pub(crate) fn from_key(key: Vec<u8>, protection: &str) -> Result<Self, AppError> {
        if key.len() != KEY_BYTES {
            return Err(AppError::Config(format!(
                "compaction master key must be exactly {KEY_BYTES} bytes"
            )));
        }
        Ok(Self {
            key: Zeroizing::new(key),
            protection: protection.to_string(),
        })
    }

    pub(crate) fn protection(&self) -> &str {
        &self.protection
    }

    pub(crate) fn fingerprint(&self, value: &[u8]) -> Result<String, AppError> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Config("invalid compaction fingerprint key".to_string()))?;
        mac.update(value);
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    /// AES-256-GCM using the standalone bridge's byte layout:
    /// nonce (12) || authentication tag (16) || ciphertext.
    pub(crate) fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, AppError> {
        let cipher = Aes256Gcm::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Config("invalid compaction master key".to_string()))?;
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| AppError::Message(format!("cannot generate compaction nonce: {e}")))?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| AppError::Message("cannot encrypt compaction payload".to_string()))?;
        if encrypted.len() < TAG_BYTES {
            return Err(AppError::Message(
                "encrypted compaction payload is truncated".to_string(),
            ));
        }
        let split = encrypted.len() - TAG_BYTES;
        let mut output = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&encrypted[split..]);
        output.extend_from_slice(&encrypted[..split]);
        Ok(output)
    }

    pub(crate) fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, AppError> {
        if blob.len() < NONCE_BYTES + TAG_BYTES + 1 {
            return Err(AppError::Message(
                "encrypted compaction payload is truncated".to_string(),
            ));
        }
        let (nonce, remainder) = blob.split_at(NONCE_BYTES);
        let (tag, ciphertext) = remainder.split_at(TAG_BYTES);
        let mut aes_layout = Vec::with_capacity(ciphertext.len() + TAG_BYTES);
        aes_layout.extend_from_slice(ciphertext);
        aes_layout.extend_from_slice(tag);
        let cipher = Aes256Gcm::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Config("invalid compaction master key".to_string()))?;
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: &aes_layout,
                    aad,
                },
            )
            .map_err(|_| {
                AppError::Message(
                    "compaction payload authentication failed; original context was not modified"
                        .to_string(),
                )
            })
    }
}

fn explicit_key() -> Result<Option<Vec<u8>>, AppError> {
    let encoded = std::env::var("CC_SWITCH_COMPACTION_MASTER_KEY")
        .ok()
        .or_else(|| std::env::var("CODEX_COMPACTION_MASTER_KEY").ok());
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    let key = STANDARD
        .decode(encoded.trim())
        .map_err(|_| AppError::Config("compaction master key is not valid base64".to_string()))?;
    if key.len() != KEY_BYTES {
        return Err(AppError::Config(format!(
            "compaction master key must decode to {KEY_BYTES} bytes"
        )));
    }
    Ok(Some(key))
}

#[cfg(windows)]
fn load_or_create_dpapi_key(path: &Path) -> Result<Vec<u8>, AppError> {
    if path.exists() {
        return read_dpapi_key(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    let mut key = Zeroizing::new(vec![0u8; KEY_BYTES]);
    getrandom::getrandom(key.as_mut_slice())
        .map_err(|e| AppError::Message(format!("cannot generate compaction master key: {e}")))?;
    let protected = dpapi_protect(key.as_slice())?;
    let encoded = STANDARD.encode(protected);
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(encoded.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.sync_all())
                .map_err(|e| AppError::io(path, e))?;
            Ok(key.to_vec())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_dpapi_key(path),
        Err(error) => Err(AppError::io(path, error)),
    }
}

#[cfg(windows)]
fn read_dpapi_key(path: &Path) -> Result<Vec<u8>, AppError> {
    let mut encoded = String::new();
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|mut file| file.read_to_string(&mut encoded))
        .map_err(|e| AppError::io(path, e))?;
    let protected = STANDARD.decode(encoded.trim()).map_err(|_| {
        AppError::Config("DPAPI compaction key file is not valid base64".to_string())
    })?;
    let key = dpapi_unprotect(&protected)?;
    if key.len() != KEY_BYTES {
        return Err(AppError::Config(
            "DPAPI returned an invalid compaction master key".to_string(),
        ));
    }
    Ok(key)
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
            "DPAPI failed to protect compaction key: {}",
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
            "DPAPI failed to unprotect compaction key: {}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trip_and_aad_authentication() {
        let cipher = JournalCipher::from_key(vec![7; 32], "test").unwrap();
        let blob = cipher.seal(b"secret context", b"snapshot-1").unwrap();
        assert_ne!(blob.windows(6).any(|part| part == b"secret"), true);
        assert_eq!(
            cipher.open(&blob, b"snapshot-1").unwrap(),
            b"secret context"
        );
        assert!(cipher.open(&blob, b"snapshot-2").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let cipher = JournalCipher::from_key(vec![9; 32], "test").unwrap();
        let mut blob = cipher.seal(b"retained", b"aad").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x40;
        let error = cipher.open(&blob, b"aad").unwrap_err().to_string();
        assert!(error.contains("authentication failed"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_dpapi_current_user_round_trip_and_corruption_detection() {
        let plaintext = b"cc-switch-compaction-dpapi-test";
        let protected = dpapi_protect(plaintext).expect("protect with current Windows user");
        assert_ne!(protected, plaintext);
        assert_eq!(
            dpapi_unprotect(&protected).expect("unprotect with current Windows user"),
            plaintext
        );
        let mut corrupt = protected;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x5a;
        assert!(dpapi_unprotect(&corrupt).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_dpapi_key_file_survives_reload_without_plaintext() {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join("compaction-key.dpapi");
        let first = load_or_create_dpapi_key(&path).unwrap();
        let disk = std::fs::read(&path).unwrap();
        assert!(!disk.windows(first.len()).any(|window| window == first));
        let second = load_or_create_dpapi_key(&path).unwrap();
        assert_eq!(first, second);
    }
}
