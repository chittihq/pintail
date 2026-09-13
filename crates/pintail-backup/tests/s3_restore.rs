use std::{
    process::{Command, Output, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use object_store::ObjectStore;
use pintail_backup::{
    BackupSource, S3Destination, SourceSegment, SourceTable, build_s3, create_backup,
    load_manifest, restore_backup,
};
use serde_json::json;

/// An S3-compatible object store for the round trip to run against.
///
/// `RustFS` rather than `MinIO`: `MinIO`'s images stopped being anonymously
/// pullable from Docker Hub, which failed this on every CI run while passing
/// anywhere the image was already cached. The browser gate was already on
/// `RustFS`, so this is the same pinned image rather than a second choice.
struct ObjectStoreContainer {
    name: String,
    endpoint: String,
}

/// Pinned, and the same version the browser gate starts.
const RUSTFS_IMAGE: &str = "rustfs/rustfs:1.0.0-beta.12";
const ACCESS_KEY: &str = "rustfsadmin";
const SECRET_KEY: &str = "rustfs-secret";

impl ObjectStoreContainer {
    fn start() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let name = format!("pintail-m8-objectstore-{}-{nonce}", std::process::id());
        checked_output(
            Command::new("docker").args([
                "run",
                "--detach",
                "--name",
                &name,
                "--publish",
                "0:9000",
                "--env",
                &format!("RUSTFS_ACCESS_KEY={ACCESS_KEY}"),
                "--env",
                &format!("RUSTFS_SECRET_KEY={SECRET_KEY}"),
                RUSTFS_IMAGE,
            ]),
            "start the object store",
        )?;
        let host = docker_host()?;
        let port_output = checked_output(
            Command::new("docker").args(["port", &name, "9000/tcp"]),
            "inspect the object store's published port",
        )?;
        let port = String::from_utf8(port_output.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next())
            .and_then(|port| port.parse::<u16>().ok())
            .ok_or_else(|| "Docker did not report a numeric port".to_owned())?;
        let container = Self {
            name,
            endpoint: format!("http://{host}:{port}"),
        };
        for _ in 0..60 {
            if container.create_bucket().is_ok() {
                return Ok(container);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("the object store did not become ready within 30 seconds".to_owned())
    }

    /// Creates the bucket over S3, from inside the container's own network
    /// namespace so it does not depend on the published port being reachable
    /// from here.
    ///
    /// The AWS CLI rather than `mc`: `MinIO`'s client image is no longer
    /// anonymously pullable either, and this one comes from AWS's public
    /// registry, which does not gate pulls. It speaks S3, not `MinIO`, so the
    /// server underneath it can be anything that does.
    fn create_bucket(&self) -> Result<(), String> {
        checked_output(
            Command::new("docker").args([
                "run",
                "--rm",
                "--network",
                &format!("container:{}", self.name),
                "--env",
                &format!("AWS_ACCESS_KEY_ID={ACCESS_KEY}"),
                "--env",
                &format!("AWS_SECRET_ACCESS_KEY={SECRET_KEY}"),
                "--env",
                "AWS_DEFAULT_REGION=us-east-1",
                "public.ecr.aws/aws-cli/aws-cli:latest",
                "s3",
                "mb",
                "s3://pintail",
                "--endpoint-url",
                "http://127.0.0.1:9000",
            ]),
            "create the bucket",
        )
        .map(|_| ())
    }
}

impl Drop for ObjectStoreContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
#[ignore = "requires the configured Docker host and its object-store images"]
async fn s3_full_incremental_restore_round_trip() {
    let store_container = ObjectStoreContainer::start().unwrap_or_else(|error| panic!("{error}"));
    let store: Arc<dyn ObjectStore> = build_s3(&S3Destination {
        bucket: "pintail".into(),
        prefix: "gate/backups".into(),
        endpoint: Some(store_container.endpoint.clone()),
        region: "us-east-1".into(),
        access_key_id: Some(ACCESS_KEY.into()),
        secret_access_key: Some(SECRET_KEY.into()),
    })
    .expect("object store client");
    let local = tempfile::tempdir().expect("local backup data");
    let first = local.path().join("segment-1.pts");
    let second = local.path().join("segment-2.pts");
    std::fs::write(&first, b"first segment payload").expect("first segment");
    std::fs::write(&second, b"second segment payload").expect("second segment");

    let (full, _) = create_backup(
        store.clone(),
        "gate/backups",
        source(
            "full",
            None,
            vec![SourceSegment {
                file_name: "segment-1.pts".into(),
                path: first.clone(),
            }],
        ),
        None,
    )
    .await
    .expect("full MinIO backup");
    let (_, incremental) = create_backup(
        store.clone(),
        "gate/backups",
        source(
            "incremental",
            Some("full"),
            vec![
                SourceSegment {
                    file_name: "segment-1.pts".into(),
                    path: first,
                },
                SourceSegment {
                    file_name: "segment-2.pts".into(),
                    path: second,
                },
            ],
        ),
        Some(&full),
    )
    .await
    .expect("incremental MinIO backup");
    assert_eq!(incremental.reused_segments, 1);

    let manifest = load_manifest(store.as_ref(), "gate/backups", "analytics", "incremental")
        .await
        .expect("published MinIO manifest");
    let destination = local.path().join("restored");
    let restored = restore_backup(store.as_ref(), manifest, &destination)
        .await
        .expect("verified MinIO restore");
    assert_eq!(restored.restored_objects, 3);
    assert_eq!(
        std::fs::read(destination.join("tables/table-events/segment-1.pts"))
            .expect("first restored segment"),
        b"first segment payload"
    );
    assert_eq!(
        std::fs::read(destination.join("tables/table-events/segment-2.pts"))
            .expect("second restored segment"),
        b"second segment payload"
    );
}

fn source(backup_id: &str, parent_id: Option<&str>, segments: Vec<SourceSegment>) -> BackupSource {
    BackupSource {
        database_id: "analytics".into(),
        backup_id: backup_id.into(),
        parent_id: parent_id.map(str::to_owned),
        control_plane: json!({"database": "analytics", "checkpoint": "gtid:1-9"}),
        tables: vec![SourceTable {
            name: "events".into(),
            directory_name: "table-events".into(),
            manifest: b"manifest".to_vec(),
            segments,
        }],
    }
}

fn checked_output(command: &mut Command, action: &str) -> Result<Output, String> {
    command
        .output()
        .map_err(|error| format!("{action}: {error}"))
        .and_then(|output| {
            if output.status.success() {
                Ok(output)
            } else {
                Err(format!(
                    "{action} failed with {}\nstdout: {}\nstderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
}

fn docker_host() -> Result<String, String> {
    let context = checked_output(
        Command::new("docker").args(["context", "show"]),
        "read Docker context",
    )?;
    let context = String::from_utf8(context.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    let endpoint = checked_output(
        Command::new("docker").args([
            "context",
            "inspect",
            &context,
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]),
        "read Docker endpoint",
    )?;
    let endpoint = String::from_utf8(endpoint.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    if let Some(target) = endpoint.strip_prefix("ssh://") {
        let target = target.split('@').next_back().unwrap_or(target);
        let target = target.split(':').next().unwrap_or(target);
        let config = checked_output(
            Command::new("ssh").args(["-G", target]),
            "resolve Docker SSH host",
        )?;
        return String::from_utf8(config.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .find_map(|line| line.strip_prefix("hostname ").map(str::to_owned))
            .ok_or_else(|| "SSH configuration did not expose a hostname".to_owned());
    }
    Ok("127.0.0.1".to_owned())
}
