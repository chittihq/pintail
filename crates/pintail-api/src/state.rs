use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead as _, KeyInit as _},
};
use pintail_meta::MetaStore;
use rand::RngCore as _;
use tokio::sync::broadcast;

use crate::{error::ApiError, events::ApiEvent};

const NONCE_BYTES: usize = 12;

/// Shared configuration for Pintail's authenticated HTTP surface.
#[derive(Clone)]
pub struct ApiState {
    inner: Option<Arc<ApiStateInner>>,
}

struct ApiStateInner {
    metadata_path: PathBuf,
    data_dir: PathBuf,
    jwt_secret: Vec<u8>,
    dsn_key: [u8; 32],
    events: broadcast::Sender<ApiEvent>,
    active_jobs: Mutex<BTreeSet<String>>,
}

impl ApiState {
    /// Builds configured API state.
    ///
    /// # Errors
    ///
    /// Returns an error when the DSN key is not exactly 32 hex-encoded bytes
    /// or the metadata store cannot be opened.
    pub fn new(
        data_dir: impl Into<PathBuf>,
        metadata_path: impl Into<PathBuf>,
        jwt_secret: impl Into<Vec<u8>>,
        dsn_encryption_key: &str,
    ) -> Result<Self> {
        let metadata_path = metadata_path.into();
        MetaStore::open(&metadata_path)?;
        let dsn_key = decode_hex_key(dsn_encryption_key)?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Some(Arc::new(ApiStateInner {
                metadata_path,
                data_dir: data_dir.into(),
                jwt_secret: jwt_secret.into(),
                dsn_key,
                events,
                active_jobs: Mutex::new(BTreeSet::new()),
            })),
        })
    }

    pub(crate) const fn unconfigured() -> Self {
        Self { inner: None }
    }

    pub(crate) fn metadata(&self) -> Result<MetaStore, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        MetaStore::open(&inner.metadata_path).map_err(ApiError::internal)
    }

    pub(crate) fn jwt_secret(&self) -> Result<&[u8], ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.jwt_secret.as_slice())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn data_dir(&self) -> Result<&Path, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.data_dir.as_path())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn metadata_path(&self) -> Result<&Path, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.metadata_path.as_path())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn encrypt_dsn(&self, dsn: &str) -> Result<Vec<u8>, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(&inner.dsn_key).map_err(ApiError::internal)?;
        let mut nonce = [0_u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let nonce_array = Nonce::try_from(nonce.as_slice()).map_err(ApiError::internal)?;
        let encrypted = cipher
            .encrypt(&nonce_array, dsn.as_bytes())
            .map_err(ApiError::internal)?;
        let mut encoded = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        encoded.extend_from_slice(&nonce);
        encoded.extend_from_slice(&encrypted);
        Ok(encoded)
    }

    pub(crate) fn decrypt_dsn(&self, encrypted: &[u8]) -> Result<String, ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let (nonce, ciphertext) = encrypted
            .split_at_checked(NONCE_BYTES)
            .ok_or_else(|| ApiError::internal("encrypted DSN is truncated"))?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(&inner.dsn_key).map_err(ApiError::internal)?;
        let nonce = Nonce::try_from(nonce).map_err(ApiError::internal)?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(ApiError::internal)?;
        String::from_utf8(plaintext).map_err(ApiError::internal)
    }

    pub(crate) fn subscribe(&self) -> Result<broadcast::Receiver<ApiEvent>, ApiError> {
        self.inner
            .as_ref()
            .map(|inner| inner.events.subscribe())
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))
    }

    pub(crate) fn publish(&self, event: ApiEvent) {
        if let Some(inner) = &self.inner {
            let _ = inner.events.send(event);
        }
    }

    pub(crate) fn acquire_job(&self, database_id: &str) -> Result<(), ApiError> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("control-plane API is not configured"))?;
        let mut jobs = inner.active_jobs.lock().map_err(ApiError::internal)?;
        if jobs.insert(database_id.to_owned()) {
            Ok(())
        } else {
            Err(ApiError::conflict(
                "a replication job is already active for this database",
            ))
        }
    }

    pub(crate) fn release_job(&self, database_id: &str) {
        if let Some(inner) = &self.inner {
            if let Ok(mut jobs) = inner.active_jobs.lock() {
                jobs.remove(database_id);
            }
        }
    }
}

pub(crate) fn random_identifier(prefix: &str, bytes: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    let mut output = String::with_capacity(prefix.len() + bytes * 2);
    output.push_str(prefix);
    for byte in random {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex_key(encoded: &str) -> Result<[u8; 32]> {
    if encoded.len() != 64 {
        bail!("DSN encryption key must contain 64 hexadecimal characters");
    }
    let mut decoded = [0_u8; 32];
    for (index, output) in decoded.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&encoded[offset..offset + 2], 16)
            .with_context(|| format!("DSN encryption key has invalid hex at byte {index}"))?;
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::ApiState;

    #[test]
    fn dsn_encryption_is_randomized_and_authenticated() {
        let data = tempfile::tempdir().expect("temporary API state");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"jwt-secret",
            &"11".repeat(32),
        )
        .expect("API state");
        let first = state.encrypt_dsn("mysql://source/app").unwrap();
        let second = state.encrypt_dsn("mysql://source/app").unwrap();
        assert_ne!(first, second);
        assert_eq!(state.decrypt_dsn(&first).unwrap(), "mysql://source/app");
        let mut corrupt = first;
        *corrupt.last_mut().expect("ciphertext") ^= 1;
        assert!(state.decrypt_dsn(&corrupt).is_err());
    }
}
