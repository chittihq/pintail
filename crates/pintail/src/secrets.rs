//! Persistent process secrets created on Pintail's first boot.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use anyhow::{Context, Result};
use fs2::FileExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};

const SECRETS_FILE: &str = "secrets.toml";
const SECRETS_LOCK_FILE: &str = ".secrets.lock";
const SECRETS_TEMP_FILE: &str = ".secrets.toml.tmp";
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
/// Creation is serialized by an OS file lock. The winner writes and syncs a
/// private temporary file, atomically renames it, then syncs the data
/// directory before releasing the lock to waiting boots.
///
/// # Errors
///
/// Returns an error when the data directory or secrets file cannot be
/// created, read, encoded, decoded, or durably synchronized.
pub fn load_or_create(data_dir: &Path) -> Result<LoadedBootSecrets> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;
    secure_data_directory(data_dir)?;

    let lock_path = data_dir.join(SECRETS_LOCK_FILE);
    let lock_file = open_private_file(&lock_path, false)
        .with_context(|| format!("failed to open boot-secret lock {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock_file)
        .with_context(|| format!("failed to lock boot secrets {}", lock_path.display()))?;

    load_or_create_locked(data_dir)
}

fn load_or_create_locked(data_dir: &Path) -> Result<LoadedBootSecrets> {
    let path = data_dir.join(SECRETS_FILE);
    if path.exists() {
        secure_secret_file(&path)?;
        sync_data_directory(data_dir)?;
        return load(&path, false);
    }

    let secrets = BootSecrets::generate();
    let encoded = toml::to_string(&secrets).context("failed to encode first-boot secrets")?;
    let temporary_path = data_dir.join(SECRETS_TEMP_FILE);
    let mut temporary_file = open_private_file(&temporary_path, true).with_context(|| {
        format!(
            "failed to create temporary boot-secret file {}",
            temporary_path.display()
        )
    })?;
    temporary_file
        .write_all(encoded.as_bytes())
        .context("failed to persist first-boot secrets")?;
    temporary_file
        .sync_all()
        .context("failed to sync first-boot secrets")?;
    drop(temporary_file);
    fs::rename(&temporary_path, &path)
        .with_context(|| format!("failed to publish boot-secret file {}", path.display()))?;
    sync_data_directory(data_dir)?;

    Ok(LoadedBootSecrets {
        secrets,
        first_boot: true,
    })
}

#[cfg(unix)]
fn secure_data_directory(data_dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "failed to secure data directory permissions {}",
            data_dir.display()
        )
    })
}

#[cfg(not(unix))]
fn secure_data_directory(_data_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_secret_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to secure boot-secret permissions {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn secure_secret_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_data_directory(data_dir: &Path) -> Result<()> {
    std::fs::File::open(data_dir)
        .with_context(|| {
            format!(
                "failed to open data directory for sync {}",
                data_dir.display()
            )
        })?
        .sync_all()
        .with_context(|| format!("failed to sync data directory {}", data_dir.display()))
}

#[cfg(not(unix))]
fn sync_data_directory(_data_dir: &Path) -> Result<()> {
    Ok(())
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
fn open_private_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)
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
