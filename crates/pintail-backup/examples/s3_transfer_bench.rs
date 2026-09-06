//! Synthetic S3 transport benchmark. Run each operation in a fresh process
//! under `/usr/bin/time -v` to measure peak resident memory independently.

use std::{env, fs, io::Write as _, path::Path, sync::Arc, time::Instant};

use anyhow::{Context as _, Result, ensure};
use futures_util::TryStreamExt as _;
use object_store::{ObjectStore, ObjectStoreExt as _, path::Path as ObjectPath};
use pintail_backup::{
    BackupSource, S3Destination, SourceSegment, SourceTable, build_s3, create_backup,
    load_manifest, restore_backup,
};
use serde_json::json;

fn prepare(root: &Path, count: usize, mib: usize) -> Result<()> {
    fs::create_dir_all(root)?;
    for index in 0..count {
        for changed in [false, true] {
            if changed && index % 4 != 0 {
                continue;
            }
            let mut file = fs::File::create(root.join(file_name(index, changed)))?;
            let mut state = u64::try_from(index)? + 1 + u64::from(changed) * 1_000_000;
            let mut block = vec![0_u8; 1024 * 1024];
            for word in block.chunks_exact_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                word.copy_from_slice(&state.to_le_bytes());
            }
            for _ in 0..mib {
                file.write_all(&block)?;
            }
            file.sync_all()?;
        }
    }
    Ok(())
}

fn file_name(index: usize, changed: bool) -> String {
    format!(
        "segment-{index:04}-{}.pts",
        if changed { "new" } else { "base" }
    )
}

fn source(root: &Path, count: usize, incremental: bool) -> BackupSource {
    BackupSource {
        database_id: "synthetic".into(),
        backup_id: if incremental { "delta" } else { "base" }.into(),
        parent_id: incremental.then(|| "base".into()),
        control_plane: json!({"synthetic": true}),
        tables: vec![SourceTable {
            name: "records".into(),
            directory_name: "table-records".into(),
            manifest: b"synthetic transport benchmark manifest".to_vec(),
            segments: (0..count)
                .map(|index| SourceSegment {
                    file_name: file_name(index, false),
                    path: root.join(file_name(index, incremental && index % 4 == 0)),
                })
                .collect(),
        }],
    }
}

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().collect();
    ensure!(
        args.len() == 6,
        "usage: s3_transfer_bench prepare|full|incremental|restore|cleanup ROOT COUNT MIB PREFIX"
    );
    let mode = &args[1];
    let root = Path::new(&args[2]);
    let count: usize = args[3].parse()?;
    let mib: usize = args[4].parse()?;
    let prefix = &args[5];
    ensure!(count > 0 && mib > 0, "count and size must be positive");
    pintail_backup::validate_prefix(prefix)?;
    ensure!(
        !prefix.contains('/'),
        "benchmark prefix must be one component"
    );
    if mode == "prepare" {
        return prepare(root, count, mib);
    }
    let store: Arc<dyn ObjectStore> = build_s3(&S3Destination {
        bucket: env::var("BENCH_S3_BUCKET")?,
        prefix: prefix.clone(),
        endpoint: Some(env::var("BENCH_S3_ENDPOINT")?),
        region: "us-east-1".into(),
        access_key_id: Some(env::var("BENCH_S3_ACCESS_KEY")?),
        secret_access_key: Some(env::var("BENCH_S3_SECRET_KEY")?),
    })?;
    let destination = root.join(format!("restore-{prefix}"));
    let started = Instant::now();
    let counters = match mode.as_str() {
        "full" | "incremental" => {
            let incremental = mode == "incremental";
            let parent = if incremental {
                Some(load_manifest(store.as_ref(), prefix, "synthetic", "base").await?)
            } else {
                None
            };
            let (_, summary) = create_backup(
                store.clone(),
                prefix,
                source(root, count, incremental),
                parent.as_ref(),
            )
            .await?;
            let changed = count.div_ceil(4);
            ensure!(
                summary.reused_segments
                    == u64::try_from(if incremental { count - changed } else { 0 })?,
                "incorrect segment reuse"
            );
            json!({"uploaded_bytes": summary.uploaded_bytes, "reused_segments": summary.reused_segments})
        }
        "restore" => {
            let manifest = load_manifest(store.as_ref(), prefix, "synthetic", "delta").await?;
            let restored = restore_backup(store.as_ref(), manifest, &destination).await?;
            let expected = u64::try_from(count)? * u64::try_from(mib)? * 1024 * 1024
                + u64::try_from(b"synthetic transport benchmark manifest".len())?;
            ensure!(
                restored.restored_bytes == expected,
                "incorrect restored byte count"
            );
            ensure!(
                restored.restored_objects == u64::try_from(count + 1)?,
                "incorrect restored object count"
            );
            json!({"restored_bytes": restored.restored_bytes})
        }
        "cleanup" => {
            let objects: Vec<_> = store
                .list(Some(&ObjectPath::from(prefix.as_str())))
                .try_collect()
                .await?;
            for object in objects {
                store.delete(&object.location).await?;
            }
            if destination.exists() {
                fs::remove_dir_all(&destination).context("remove benchmark restore")?;
            }
            return Ok(());
        }
        _ => anyhow::bail!("unknown operation"),
    };
    println!(
        "{}",
        json!({
            "operation": mode, "segments": count, "segment_mib": mib,
            "seconds": started.elapsed().as_secs_f64(), "counters": counters,
        })
    );
    Ok(())
}
