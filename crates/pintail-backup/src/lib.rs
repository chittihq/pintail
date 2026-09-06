//! Native, checksum-verified backup and restore for Pintail.
//!
//! A backup manifest contains a complete view of a database. Incremental
//! manifests reuse immutable segment objects from an ancestor when their
//! logical identity and SHA-256 digest are unchanged.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::Utc;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use object_store::{
    ObjectStore, ObjectStoreExt as _, PutPayload, aws::AmazonS3Builder, path::Path,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Current JSON backup-manifest format.
pub const BACKUP_FORMAT_VERSION: u32 = 1;

/// Bounds simultaneous segment transfers independently of segment size.
#[derive(Clone, Copy, Debug)]
pub struct TransferOptions {
    pub concurrency: usize,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self { concurrency: 4 }
    }
}

impl TransferOptions {
    fn validate(self) -> Result<()> {
        if !(1..=32).contains(&self.concurrency) {
            bail!("transfer concurrency must be between 1 and 32");
        }
        Ok(())
    }
}

// Each object holds at most two in-flight parts plus one read buffer.
const PART_BYTES: usize = 8 * 1024 * 1024;
const PART_CONCURRENCY: usize = 2;

/// S3-compatible object-store connection settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3Destination {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// A local immutable segment to include in a backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSegment {
    pub file_name: String,
    pub path: PathBuf,
}

/// A pinned local table generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceTable {
    pub name: String,
    pub directory_name: String,
    pub manifest: Vec<u8>,
    pub segments: Vec<SourceSegment>,
}

/// A complete pinned database view passed to the uploader.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupSource {
    pub database_id: String,
    pub backup_id: String,
    pub parent_id: Option<String>,
    pub control_plane: Value,
    pub tables: Vec<SourceTable>,
}

/// One checksummed object referenced by a backup manifest.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ObjectReference {
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
    pub source_backup_id: String,
}

/// The physical objects for one table generation.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct BackupTable {
    pub name: String,
    pub directory_name: String,
    pub manifest: ObjectReference,
    pub segments: Vec<ObjectReference>,
}

/// Portable database backup manifest.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct BackupManifest {
    pub format_version: u32,
    pub database_id: String,
    pub backup_id: String,
    pub parent_id: Option<String>,
    pub created_at: String,
    pub control_plane: Value,
    pub tables: Vec<BackupTable>,
}

/// Counters returned after a manifest is durably published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSummary {
    pub manifest_key: String,
    pub uploaded_bytes: u64,
    pub uploaded_objects: u64,
    pub reused_segments: u64,
}

/// Result of a verified restore into a new local directory.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoredBackup {
    pub manifest: BackupManifest,
    pub restored_bytes: u64,
    pub restored_objects: u64,
}

/// Validates the user-controlled object prefix.
///
/// This only guards against accidental broad writes. It is not a tenant
/// isolation or authorization boundary.
///
/// # Errors
///
/// Returns an error for absolute, empty, dot, or parent-traversal components.
pub fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() || prefix.starts_with('/') || prefix.ends_with('/') {
        bail!("backup prefix must be a non-empty relative object path");
    }
    for component in prefix.split('/') {
        validate_component(component, "backup prefix")?;
    }
    Ok(())
}

/// Builds an S3-compatible store, including MinIO-style HTTP endpoints.
///
/// # Errors
///
/// Returns an error for invalid destination settings or client construction.
pub fn build_s3(destination: &S3Destination) -> Result<Arc<dyn ObjectStore>> {
    validate_prefix(&destination.prefix)?;
    if destination.bucket.trim().is_empty() {
        bail!("backup bucket cannot be empty");
    }
    if destination.access_key_id.is_some() != destination.secret_access_key.is_some() {
        bail!("backup access key ID and secret must be provided together");
    }
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&destination.bucket)
        .with_region(&destination.region);
    if let Some(endpoint) = &destination.endpoint {
        builder = builder
            .with_endpoint(endpoint)
            .with_allow_http(endpoint.starts_with("http://"));
    }
    if let (Some(access_key), Some(secret)) = (
        destination.access_key_id.as_deref(),
        destination.secret_access_key.as_deref(),
    ) {
        builder = builder
            .with_access_key_id(access_key)
            .with_secret_access_key(secret);
    }
    Ok(Arc::new(
        builder
            .build()
            .context("failed to build S3 backup client")?,
    ))
}

/// Logs the shape of a backup run before any object is written.
fn log_backup_start(source: &BackupSource, parent: Option<&BackupManifest>) {
    pintail_log::log_info!(
        "backup start db={} backup={} tables={} kind={}",
        source.database_id,
        source.backup_id,
        source.tables.len(),
        if parent.is_some() {
            "incremental"
        } else {
            "full"
        }
    );
}

/// Logs what a completed run actually uploaded versus reused.
///
/// Reuse is the number that says whether the incremental chain is working: an
/// incremental run that reuses nothing is a full backup wearing the wrong
/// label, and the only way to notice was to diff object counts by hand.
fn log_backup_done(
    manifest: &BackupManifest,
    uploaded_objects: u64,
    uploaded_bytes: u64,
    reused_segments: u64,
) {
    pintail_log::log_info!(
        "backup done db={} backup={} uploaded_objects={uploaded_objects} uploaded_bytes={uploaded_bytes} reused_segments={reused_segments}",
        manifest.database_id,
        manifest.backup_id
    );
}

/// Uploads a full or incremental database backup and publishes its manifest
/// last.
///
/// # Errors
///
/// Returns an error for an invalid identity, inconsistent parent, local I/O,
/// serialization, or object-store failure.
pub async fn create_backup(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    source: BackupSource,
    parent: Option<&BackupManifest>,
) -> Result<(BackupManifest, BackupSummary)> {
    create_backup_with_options(store, prefix, source, parent, TransferOptions::default()).await
}

/// Creates a backup with bounded segment transfer concurrency.
///
/// # Errors
/// Returns an error for invalid options or any backup failure.
pub async fn create_backup_with_options(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    source: BackupSource,
    parent: Option<&BackupManifest>,
    options: TransferOptions,
) -> Result<(BackupManifest, BackupSummary)> {
    options.validate()?;
    validate_prefix(prefix)?;
    validate_component(&source.database_id, "database ID")?;
    validate_component(&source.backup_id, "backup ID")?;
    validate_parent(&source, parent)?;

    let root = backup_root(prefix, &source.database_id, &source.backup_id);
    let inherited = inherited_segments(parent);
    log_backup_start(&source, parent);
    let mut tables = Vec::with_capacity(source.tables.len());
    let mut uploaded_bytes = 0_u64;
    let mut uploaded_objects = 0_u64;
    let mut reused_segments = 0_u64;

    for table in source.tables {
        validate_component(&table.name, "table name")?;
        validate_component(&table.directory_name, "table directory name")?;
        let table_key = hex_component(&table.name);
        let manifest_key = format!("{root}/tables/{table_key}/manifest.ptm");
        let manifest_ref = put_bytes(
            store.as_ref(),
            &manifest_key,
            Bytes::from(table.manifest),
            &source.backup_id,
        )
        .await?;
        uploaded_bytes = uploaded_bytes
            .checked_add(manifest_ref.bytes)
            .context("backup byte counter overflow")?;
        uploaded_objects += 1;

        let transfers = stream::iter(table.segments.iter().map(|segment| {
            let key = format!("{root}/tables/{table_key}/segments/{}", segment.file_name);
            let logical = format!("{}/{}", table.name, segment.file_name);
            transfer_segment(
                store.as_ref(),
                segment,
                key,
                &source.backup_id,
                inherited.get(&logical).copied(),
            )
        }))
        .buffered(options.concurrency)
        // Drain all transfers before returning an error so each active upload
        // can complete or abort; no background task outlives the snapshot pin.
        .collect::<Vec<_>>()
        .await;
        let mut segments = Vec::with_capacity(transfers.len());
        for transfer in transfers {
            let (reference, reused) = transfer?;
            if reused {
                reused_segments += 1;
            } else {
                uploaded_bytes = uploaded_bytes
                    .checked_add(reference.bytes)
                    .context("backup byte counter overflow")?;
                uploaded_objects += 1;
            }
            segments.push(reference);
        }
        tables.push(BackupTable {
            name: table.name,
            directory_name: table.directory_name,
            manifest: manifest_ref,
            segments,
        });
    }

    tables.sort_by(|left, right| left.name.cmp(&right.name));
    let manifest = BackupManifest {
        format_version: BACKUP_FORMAT_VERSION,
        database_id: source.database_id,
        backup_id: source.backup_id.clone(),
        parent_id: source.parent_id,
        created_at: Utc::now().to_rfc3339(),
        control_plane: source.control_plane,
        tables,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("failed to encode backup manifest")?;
    let manifest_key = format!("{root}/backup.json");
    let published = put_bytes(
        store.as_ref(),
        &manifest_key,
        Bytes::from(manifest_bytes),
        &source.backup_id,
    )
    .await?;
    uploaded_bytes = uploaded_bytes
        .checked_add(published.bytes)
        .context("backup byte counter overflow")?;
    uploaded_objects += 1;

    log_backup_done(&manifest, uploaded_objects, uploaded_bytes, reused_segments);
    Ok((
        manifest,
        BackupSummary {
            manifest_key,
            uploaded_bytes,
            uploaded_objects,
            reused_segments,
        },
    ))
}

/// Downloads one published backup manifest.
///
/// # Errors
///
/// Returns an error for an invalid prefix/identity, missing object, malformed
/// JSON, or unsupported format.
pub async fn load_manifest(
    store: &dyn ObjectStore,
    prefix: &str,
    database_id: &str,
    backup_id: &str,
) -> Result<BackupManifest> {
    validate_prefix(prefix)?;
    validate_component(database_id, "database ID")?;
    validate_component(backup_id, "backup ID")?;
    let key = format!(
        "{}/backup.json",
        backup_root(prefix, database_id, backup_id)
    );
    let bytes = get_bytes(store, &key).await?;
    let manifest: BackupManifest =
        serde_json::from_slice(&bytes).context("failed to decode backup manifest")?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        bail!(
            "unsupported backup format version {}",
            manifest.format_version
        );
    }
    if manifest.database_id != database_id || manifest.backup_id != backup_id {
        bail!("backup manifest identity does not match its object path");
    }
    Ok(manifest)
}

/// Object keys a manifest references (table manifests and segments).
/// Incremental manifests reuse ancestor objects, so retention must keep any
/// key referenced by a retained manifest.
#[must_use]
pub fn manifest_object_keys(manifest: &BackupManifest) -> Vec<String> {
    manifest
        .tables
        .iter()
        .flat_map(|table| {
            std::iter::once(table.manifest.key.clone())
                .chain(table.segments.iter().map(|segment| segment.key.clone()))
        })
        .collect()
}

/// Deletes a pruned backup: every object it references that no retained
/// manifest still needs, then its own manifest. Returns deleted objects.
///
/// # Errors
///
/// Returns an error when the manifest cannot be loaded or a delete fails.
pub async fn delete_backup(
    store: &dyn ObjectStore,
    prefix: &str,
    database_id: &str,
    backup_id: &str,
    retained_keys: &std::collections::HashSet<String, impl std::hash::BuildHasher>,
) -> Result<u64> {
    let manifest = load_manifest(store, prefix, database_id, backup_id).await?;
    let mut deleted = 0_u64;
    for key in manifest_object_keys(&manifest) {
        if retained_keys.contains(&key) {
            continue;
        }
        store
            .delete(&Path::from(key.as_str()))
            .await
            .with_context(|| format!("failed to delete backup object {key}"))?;
        deleted += 1;
    }
    let manifest_key = format!(
        "{}/backup.json",
        backup_root(prefix, database_id, backup_id)
    );
    store
        .delete(&Path::from(manifest_key.as_str()))
        .await
        .with_context(|| format!("failed to delete backup manifest {manifest_key}"))?;
    Ok(deleted + 1)
}

/// Restores a manifest into a new side-by-side database directory.
///
/// Objects are downloaded into a staging directory and checksummed before the
/// directory is atomically renamed into place.
///
/// # Errors
///
/// Returns an error if the destination exists, a path is invalid, an object is
/// unavailable or corrupt, or local publication fails.
pub async fn restore_backup(
    store: &dyn ObjectStore,
    manifest: BackupManifest,
    destination: &FsPath,
) -> Result<RestoredBackup> {
    restore_backup_with_options(store, manifest, destination, TransferOptions::default()).await
}

/// Restores a backup with bounded segment transfer concurrency.
///
/// # Errors
/// Returns an error for invalid options or any restore failure.
pub async fn restore_backup_with_options(
    store: &dyn ObjectStore,
    manifest: BackupManifest,
    destination: &FsPath,
    options: TransferOptions,
) -> Result<RestoredBackup> {
    options.validate()?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        bail!(
            "unsupported backup format version {}",
            manifest.format_version
        );
    }
    if destination.exists() {
        bail!(
            "restore destination {} already exists; restore is side-by-side only",
            destination.display()
        );
    }
    let parent = destination
        .parent()
        .context("restore destination has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create restore parent {}", parent.display()))?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("restore destination has an invalid final component")?;
    validate_component(file_name, "restore database ID")?;
    let staging = parent.join(format!(".restore-{file_name}-{}.tmp", manifest.backup_id));
    if staging.exists() {
        bail!(
            "restore staging directory {} already exists",
            staging.display()
        );
    }
    std::fs::create_dir(&staging)
        .with_context(|| format!("failed to create restore staging {}", staging.display()))?;

    let restored = restore_objects(store, &manifest, &staging, options).await;
    let (restored_bytes, restored_objects) = match restored {
        Ok(counters) => counters,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    std::fs::rename(&staging, destination).with_context(|| {
        format!(
            "failed to publish restored database {}",
            destination.display()
        )
    })?;
    Ok(RestoredBackup {
        manifest,
        restored_bytes,
        restored_objects,
    })
}

async fn restore_objects(
    store: &dyn ObjectStore,
    manifest: &BackupManifest,
    staging: &FsPath,
    options: TransferOptions,
) -> Result<(u64, u64)> {
    let tables_root = staging.join("tables");
    std::fs::create_dir(&tables_root).context("failed to create restored table root")?;
    let mut objects = Vec::new();
    for table in &manifest.tables {
        validate_component(&table.name, "table name")?;
        validate_component(&table.directory_name, "table directory name")?;
        let table_dir = tables_root.join(&table.directory_name);
        std::fs::create_dir(&table_dir)?;
        objects.push((&table.manifest, table_dir.join("manifest.ptm")));
        for segment in &table.segments {
            let file_name = segment
                .key
                .rsplit('/')
                .next()
                .context("segment object key has no file name")?;
            validate_component(file_name, "segment file name")?;
            objects.push((segment, table_dir.join(file_name)));
        }
    }
    // Concurrent writes require unique targets; reject ambiguous manifests
    // before opening files, including a segment named manifest.ptm.
    let mut targets = std::collections::HashSet::new();
    for (_, path) in &objects {
        if !targets.insert(path) {
            bail!("backup manifest contains a duplicate restore path");
        }
    }
    let count = u64::try_from(objects.len())?;
    let transfers = stream::iter(
        objects
            .into_iter()
            .map(|(reference, path)| async move { restore_object(store, reference, &path).await }),
    )
    .buffered(options.concurrency)
    // Drain before removing staging, including when another transfer fails.
    .collect::<Vec<_>>()
    .await;
    let mut bytes = 0_u64;
    for transfer in transfers {
        bytes = bytes
            .checked_add(transfer?)
            .context("restore byte counter overflow")?;
    }
    Ok((bytes, count))
}

async fn restore_object(
    store: &dyn ObjectStore,
    reference: &ObjectReference,
    path: &FsPath,
) -> Result<u64> {
    let mut body = store
        .get(&Path::parse(&reference.key)?)
        .await?
        .into_stream();
    let mut file =
        tokio::io::BufWriter::with_capacity(1024 * 1024, tokio::fs::File::create(path).await?);
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    while let Some(chunk) = body.try_next().await? {
        bytes = bytes
            .checked_add(u64::try_from(chunk.len())?)
            .context("restore byte counter overflow")?;
        if bytes > reference.bytes {
            bail!("backup object {} has an unexpected size", reference.key);
        }
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if bytes != reference.bytes {
        bail!("backup object {} has an unexpected size", reference.key);
    }
    if digest_hex(&digest.finalize()) != reference.sha256 {
        bail!(
            "backup object {} failed SHA-256 verification",
            reference.key
        );
    }
    file.flush().await?;
    Ok(bytes)
}

async fn hash_file(path: &FsPath) -> Result<(String, u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes = bytes
            .checked_add(u64::try_from(read)?)
            .context("backup byte counter overflow")?;
    }
    Ok((digest_hex(&digest.finalize()), bytes))
}

async fn transfer_segment(
    store: &dyn ObjectStore,
    segment: &SourceSegment,
    key: String,
    backup_id: &str,
    inherited: Option<&ObjectReference>,
) -> Result<(ObjectReference, bool)> {
    validate_component(&segment.file_name, "segment file name")?;
    if let Some(existing) = inherited {
        let (digest, bytes) = hash_file(&segment.path).await?;
        if digest == existing.sha256 && bytes == existing.bytes {
            return Ok((existing.clone(), true));
        }
    }
    let mut file = tokio::fs::File::open(&segment.path).await?;
    let bytes = file.metadata().await?.len();
    if bytes <= u64::try_from(PART_BYTES)? {
        // Small segments retain one PUT and avoid multipart setup overhead.
        let mut data = vec![0; usize::try_from(bytes)?];
        file.read_exact(&mut data).await?;
        return Ok((
            put_bytes(store, &key, Bytes::from(data), backup_id).await?,
            false,
        ));
    }
    let mut upload = store.put_multipart(&Path::parse(&key)?).await?;
    let result: Result<String> = async {
        let mut parts = stream::FuturesUnordered::new();
        let mut remaining = bytes;
        let mut digest = Sha256::new();
        while remaining > 0 {
            while parts.len() >= PART_CONCURRENCY {
                parts.try_next().await?;
            }
            let length = usize::try_from(remaining.min(u64::try_from(PART_BYTES)?))?;
            let mut data = vec![0_u8; length];
            file.read_exact(&mut data).await?;
            digest.update(&data);
            remaining -= u64::try_from(length)?;
            parts.push(upload.put_part(Bytes::from(data).into()));
        }
        while parts.try_next().await?.is_some() {}
        upload.complete().await?;
        Ok(digest_hex(&digest.finalize()))
    }
    .await;
    let sha256 = match result {
        Ok(digest) => digest,
        Err(error) => {
            if let Err(abort_error) = upload.abort().await {
                return Err(error.context(format!("multipart abort also failed: {abort_error}")));
            }
            return Err(error);
        }
    };
    Ok((
        ObjectReference {
            key,
            sha256,
            bytes,
            source_backup_id: backup_id.to_owned(),
        },
        false,
    ))
}

async fn get_bytes(store: &dyn ObjectStore, key: &str) -> Result<Bytes> {
    let path = Path::parse(key).with_context(|| format!("invalid backup object key {key}"))?;
    store
        .get(&path)
        .await
        .with_context(|| format!("failed to download backup object {key}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read backup object {key}"))
}

async fn put_bytes(
    store: &dyn ObjectStore,
    key: &str,
    bytes: Bytes,
    backup_id: &str,
) -> Result<ObjectReference> {
    let path = Path::parse(key).with_context(|| format!("invalid backup object key {key}"))?;
    let reference = ObjectReference {
        key: key.to_owned(),
        sha256: sha256_hex(&bytes),
        bytes: u64::try_from(bytes.len())?,
        source_backup_id: backup_id.to_owned(),
    };
    store
        .put(&path, PutPayload::from(bytes))
        .await
        .with_context(|| format!("failed to upload backup object {key}"))?;
    Ok(reference)
}

fn validate_parent(source: &BackupSource, parent: Option<&BackupManifest>) -> Result<()> {
    match (source.parent_id.as_deref(), parent) {
        (None, None) => Ok(()),
        (Some(parent_id), Some(parent))
            if parent.backup_id == parent_id && parent.database_id == source.database_id =>
        {
            Ok(())
        }
        (Some(_), Some(_)) => bail!("incremental parent identity does not match the backup source"),
        (Some(_), None) => bail!("incremental backup requires its parent manifest"),
        (None, Some(_)) => bail!("full backup cannot receive a parent manifest"),
    }
}

fn inherited_segments(parent: Option<&BackupManifest>) -> BTreeMap<String, &ObjectReference> {
    parent
        .into_iter()
        .flat_map(|manifest| &manifest.tables)
        .flat_map(|table| {
            table.segments.iter().map(|segment| {
                let file_name = segment.key.rsplit('/').next().unwrap_or_default();
                (format!("{}/{}", table.name, file_name), segment)
            })
        })
        .collect()
}

fn backup_root(prefix: &str, database_id: &str, backup_id: &str) -> String {
    format!("{prefix}/{database_id}/{backup_id}")
}

fn validate_component(component: &str, label: &str) -> Result<()> {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.contains(['/', '\\'])
        || component.chars().any(char::is_control)
    {
        bail!("{label} contains an unsafe path component");
    }
    Ok(())
}

fn hex_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex(&Sha256::digest(bytes))
}

fn digest_hex(digest: &[u8]) -> String {
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}
