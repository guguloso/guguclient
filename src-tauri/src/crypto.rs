//! Electron `safeStorage` compatible encryption (Windows).
//!
//! Electron/Chromium store a random AES-256 key in `Local State`
//! (`os_crypt.encrypted_key`, base64 of `"DPAPI" || DPAPI(key)`), and encrypt
//! strings as `"v10" || nonce(12) || AES-GCM(ciphertext || tag)`.
//! Implementing the same scheme lets the Rust build read the cookies saved by
//! the Electron build: users keep their logged-in accounts after the switch.

use std::path::Path;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use crate::error::{AppError, AppResult};

const DPAPI_PREFIX: &[u8] = b"DPAPI";
const VERSION_PREFIX: &[u8] = b"v10";
const NONCE_LEN: usize = 12;

pub struct SafeStorage {
    cipher: Option<Aes256Gcm>,
}

impl SafeStorage {
    /// Loads the key from Chromium's `Local State`. When it is missing or
    /// unreadable, encryption is reported unavailable (Electron falls back to
    /// plaintext in that case, and so do we).
    pub fn load(local_state: &Path) -> Self {
        let cipher = read_master_key(local_state)
            .map_err(|error| tracing::warn!(%error, "safeStorage key unavailable"))
            .ok()
            .map(|key| Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)));
        Self { cipher }
    }

    pub fn is_available(&self) -> bool {
        self.cipher.is_some()
    }

    /// Decrypts a base64 value written by `safeStorage.encryptString(..).toString("base64")`.
    pub fn decrypt_base64(&self, encoded: &str) -> AppResult<String> {
        let Some(cipher) = &self.cipher else {
            return Ok(encoded.to_owned());
        };
        let blob = BASE64
            .decode(encoded)
            .map_err(|e| AppError::msg(format!("invalid base64: {e}")))?;
        let payload = blob
            .strip_prefix(VERSION_PREFIX)
            .ok_or_else(|| AppError::msg("unsupported safeStorage blob version"))?;
        if payload.len() <= NONCE_LEN {
            return Err(AppError::msg("safeStorage blob too short"));
        }
        let (nonce, ciphertext) = payload.split_at(NONCE_LEN);
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| AppError::msg("safeStorage decryption failed"))?;
        String::from_utf8(plain).map_err(|e| AppError::msg(e.to_string()))
    }

    /// Encrypts to the same base64 format the Electron build writes.
    pub fn encrypt_base64(&self, plain: &str) -> AppResult<String> {
        let Some(cipher) = &self.cipher else {
            return Ok(plain.to_owned());
        };
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plain.as_bytes())
            .map_err(|_| AppError::msg("safeStorage encryption failed"))?;
        let mut blob = Vec::with_capacity(VERSION_PREFIX.len() + NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(VERSION_PREFIX);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(blob))
    }
}

fn read_master_key(local_state: &Path) -> AppResult<Vec<u8>> {
    let raw = std::fs::read(local_state)?;
    let json: serde_json::Value = serde_json::from_slice(&raw)?;
    let encoded = json
        .pointer("/os_crypt/encrypted_key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::msg("os_crypt.encrypted_key missing"))?;
    let wrapped = BASE64
        .decode(encoded)
        .map_err(|e| AppError::msg(format!("invalid key encoding: {e}")))?;
    let protected = wrapped
        .strip_prefix(DPAPI_PREFIX)
        .ok_or_else(|| AppError::msg("key is not DPAPI-wrapped"))?;
    let key = dpapi::unprotect(protected)?;
    if key.len() != 32 {
        return Err(AppError::msg("unexpected master key length"));
    }
    Ok(key)
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    use crate::error::{AppError, AppResult};

    /// Decrypts data protected for the current Windows user.
    pub fn unprotect(data: &[u8]) -> AppResult<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(data.len()).map_err(|_| AppError::msg("DPAPI input too large"))?,
            pbData: data.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: `input` points at `data`, which outlives the call; DPAPI only
        // reads from it. On success `output.pbData` is a LocalAlloc'd buffer of
        // `cbData` bytes that we copy and then release with LocalFree.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(AppError::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: see above; the buffer is valid for `cbData` bytes until freed.
        let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(output.pbData.cast());
        }
        Ok(bytes)
    }
}

#[cfg(not(windows))]
mod dpapi {
    use crate::error::{AppError, AppResult};

    pub fn unprotect(_data: &[u8]) -> AppResult<Vec<u8>> {
        Err(AppError::msg("DPAPI is only available on Windows"))
    }
}
