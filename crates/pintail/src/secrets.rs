//! Persistent process secrets created on Pintail's first boot.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

const SECRETS_FILE: &str = "secrets.toml";
const SECRET_BYTES: usize = 32;
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Boot secret used to encrypt source connection strings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BootSecrets {
    dsn_encryption_key: String,
}

impl BootSecrets {
    /// Hex-encoded key used to encrypt source DSNs at rest.
    #[must_use]
    pub fn dsn_encryption_key(&self) -> &str {
        &self.dsn_encryption_key
    }

    fn generate() -> Self {
        Self {
            dsn_encryption_key: generate_secret(),
        }
    }
}

/// Result of loading the persistent secrets file.
#[derive(Debug)]
pub struct LoadedBootSecrets {
    secrets: BootSecrets,
    first_boot: bool,
}

impl LoadedBootSecrets {
    /// Returns the process boot secrets.
    #[must_use]
    pub fn secrets(&self) -> &BootSecrets {
        &self.secrets
    }

    /// Whether this invocation created the secrets and should display them.
    #[must_use]
    pub fn is_first_boot(&self) -> bool {
        self.first_boot
    }
}

/// Loads secrets from the data directory, creating them if this is the first boot.
///
/// Creation uses an exclusive file open so concurrent boots never replace an
/// already-persisted key set.
///
/// # Errors
///
/// Returns an error when the data directory or secrets file cannot be
/// created, read, encoded, decoded, or durably synchronized.
pub fn load_or_create(data_dir: &Path) -> Result<LoadedBootSecrets> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;

    let path = data_dir.join(SECRETS_FILE);
    if path.exists() {
        return load(&path, false);
    }

    let secrets = BootSecrets::generate();
    let encoded = toml::to_string(&secrets).context("failed to encode first-boot secrets")?;
    match open_secret_file(&path) {
        Ok(mut file) => {
            file.write_all(encoded.as_bytes())
                .context("failed to persist first-boot secrets")?;
            file.sync_all()
                .context("failed to sync first-boot secrets")?;
            Ok(LoadedBootSecrets {
                secrets,
                first_boot: true,
            })
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => load(&path, false),
        Err(error) => {
            Err(error).with_context(|| format!("failed to create secrets file {}", path.display()))
        }
    }
}

fn load(path: &Path, first_boot: bool) -> Result<LoadedBootSecrets> {
    let encoded = fs::read_to_string(path)
        .with_context(|| format!("failed to read secrets file {}", path.display()))?;
    let secrets = toml::from_str(&encoded)
        .with_context(|| format!("failed to decode secrets file {}", path.display()))?;
    Ok(LoadedBootSecrets {
        secrets,
        first_boot,
    })
}

#[cfg(unix)]
fn open_secret_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_secret_file(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Generates a 256-bit secret encoded as lowercase hexadecimal.
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0_u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);

    let mut encoded = String::with_capacity(SECRET_BYTES * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
