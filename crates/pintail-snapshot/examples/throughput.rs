//! Synthetic snapshot throughput experiment; run against an isolated `MySQL` source.
use anyhow::{Context, Result, ensure};
use mysql_async::{Opts, Pool, prelude::Queryable};
use pintail_meta::MetaStore;
use pintail_probe::probe;
use pintail_snapshot::{
    SnapshotError, SnapshotOptions, SnapshotTarget, run_snapshot, run_snapshot_with_progress,
};
use pintail_store::{StoreOptions, TableStore};
use pintail_types::Value;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dsn = std::env::var("SNAPSHOT_BENCH_DSN").context("SNAPSHOT_BENCH_DSN is required")?;
    let pool = Pool::new(Opts::from_url(&dsn)?);
    if args.get(1).is_some_and(|arg| arg == "setup") {
        let count: u64 = args.get(2).context("row count")?.parse()?;
        let mut conn = pool.get_conn().await?;
        conn.query_drop("CREATE DATABASE snapshot_lab").await?;
        conn.query_drop("USE snapshot_lab").await?;
        conn.query_drop("CREATE TABLE digits (n INT PRIMARY KEY)")
            .await?;
        conn.query_drop("INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)")
            .await?;
        conn.query_drop("CREATE TABLE seed (id BIGINT UNSIGNED PRIMARY KEY, bucket BIGINT UNSIGNED NOT NULL, amount DECIMAL(16,2) NOT NULL, stamp DATETIME(6) NOT NULL, payload VARCHAR(512) NOT NULL, token VARBINARY(32) NOT NULL, state ENUM('idle','busy','done') NOT NULL, note VARCHAR(40) NULL)").await?;
        for start in (0..count).step_by(100_000) {
            let end = (start + 100_000).min(count);
            let hashes = (0..8)
                .map(|salt| format!("SHA2(CONCAT(id, '/{salt}'),256)"))
                .collect::<Vec<_>>()
                .join(",");
            conn.query_drop(format!("INSERT INTO seed SELECT id, MOD(id,1000), MOD(id,1000000)/100, '2024-02-29 12:34:56.123456', CONCAT({hashes}), UNHEX(SHA2(CAST(id AS CHAR),256)), ELT(1+MOD(id,3),'idle','busy','done'), IF(MOD(id,7)=0,NULL,CONCAT('note-',MOD(id,100))) FROM (SELECT {start}+1+a.n+10*b.n+100*c.n+1000*d.n+10000*e.n AS id FROM digits a CROSS JOIN digits b CROSS JOIN digits c CROSS JOIN digits d CROSS JOIN digits e) s WHERE id <= {end}")).await?;
        }
        for shard in 0..4 {
            conn.query_drop(format!("CREATE TABLE shard_{shard} LIKE seed"))
                .await?;
            conn.query_drop(format!(
                "INSERT INTO shard_{shard} SELECT * FROM seed WHERE MOD(id,4)={shard}"
            ))
            .await?;
        }
        ensure!(
            count.is_multiple_of(4),
            "row count must be divisible by four"
        );
        conn.query_drop("CREATE TABLE composite_seed LIKE seed")
            .await?;
        conn.query_drop("ALTER TABLE composite_seed DROP PRIMARY KEY, ADD PRIMARY KEY(bucket,id)")
            .await?;
        conn.query_drop("INSERT INTO composite_seed SELECT * FROM seed")
            .await?;
        for range in 0..4 {
            let lo = range * (count / 4) + 1;
            let hi = (range + 1) * (count / 4);
            conn.query_drop(format!(
                "CREATE VIEW range_{range} AS SELECT * FROM seed WHERE id BETWEEN {lo} AND {hi}"
            ))
            .await?;
        }
        println!("SETUP-DONE rows={count}");
        drop(conn);
    } else {
        let mode = args.get(1).context("single or multi")?;
        let workers: usize = args.get(2).context("workers")?.parse()?;
        let directory = PathBuf::from(args.get(3).context("fresh output directory")?);
        ensure!(!directory.exists(), "output directory must be fresh");
        std::fs::create_dir_all(&directory)?;
        let mut report = probe(&pool, "snapshot_lab").await?;
        if mode == "ranges" {
            let seed = report
                .tables
                .iter()
                .find(|table| table.name == "seed")
                .context("seed")?
                .clone();
            for range in 0..4 {
                let mut source = seed.clone();
                source.name = format!("range_{range}");
                source.estimated_rows = source.estimated_rows.map(|rows| rows / 4);
                report.tables.push(source);
            }
        }
        let metadata = directory.join("meta.db");
        MetaStore::open(&metadata)?.upsert_database(
            "bench",
            "snapshot_lab",
            b"synthetic",
            "2026-09-06T00:00:00Z",
        )?;
        let mut targets = Vec::new();
        for source in &report.tables {
            if (mode == "single" && source.name == "seed")
                || (mode == "multi" && source.name.starts_with("shard_"))
                || (mode == "composite" && source.name == "composite_seed")
                || (mode == "ranges" && source.name.starts_with("range_"))
            {
                targets.push(SnapshotTarget::new(
                    source.clone(),
                    TableStore::open(
                        directory.join(&source.name),
                        source.table_schema()?,
                        StoreOptions::default(),
                    )?,
                )?);
            }
        }
        ensure!(!targets.is_empty(), "no selected tables");
        let chunk_rows = std::env::var("SNAPSHOT_BENCH_CHUNK_ROWS")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(100_000);
        let options = SnapshotOptions {
            workers,
            chunk_rows,
            allow_degraded_lock: false,
            ..SnapshotOptions::default()
        };
        let resumed = std::env::var_os("SNAPSHOT_BENCH_RESUME").is_some();
        let mut checkpoint_before = None;
        if resumed {
            let sources = targets
                .iter()
                .map(|target| target.source().clone())
                .collect::<Vec<_>>();
            let paused = run_snapshot(
                &pool,
                &metadata,
                "bench",
                &report,
                targets,
                SnapshotOptions {
                    max_new_chunks: Some(2),
                    ..options.clone()
                },
            )
            .await;
            ensure!(
                matches!(paused, Err(SnapshotError::Paused { .. })),
                "expected a durable pause"
            );
            checkpoint_before = MetaStore::open(&metadata)?.snapshot_checkpoint("bench")?;
            ensure!(checkpoint_before.is_some(), "missing original checkpoint");
            targets = sources
                .into_iter()
                .map(|source| {
                    let store = TableStore::open(
                        directory.join(&source.name),
                        source.table_schema()?,
                        StoreOptions::default(),
                    )?;
                    SnapshotTarget::new(source, store)
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        let progress = Arc::new(Mutex::new(BTreeMap::new()));
        let recorded = Arc::clone(&progress);
        let started = Instant::now();
        let result = run_snapshot_with_progress(
            &pool,
            &metadata,
            "bench",
            &report,
            targets,
            options,
            move |p| {
                recorded.lock().unwrap().insert(p.table, (p.rows, p.bytes));
            },
        )
        .await?;
        let seconds = started.elapsed().as_secs_f64();
        if resumed {
            ensure!(
                checkpoint_before == MetaStore::open(&metadata)?.snapshot_checkpoint("bench")?,
                "resume advanced the handoff checkpoint"
            );
        }
        ensure!(
            result.failed.is_empty(),
            "failed tables: {:?}",
            result.failed
        );
        ensure!(result.globally_consistent, "inconsistent snapshot");
        let mut verified_rows = 0_u64;
        let mut conn = pool.get_conn().await?;
        for target in &result.targets {
            let (count, sum, crc): (u64, u64, u64) = conn.query_first(format!("SELECT COUNT(*), CAST(SUM(id) AS UNSIGNED), CAST(SUM(CRC32(CONCAT_WS('#',id,bucket,amount,stamp,payload,HEX(token),state,COALESCE(note,'<null>')))) AS UNSIGNED) FROM snapshot_lab.`{}`",target.source().name)).await?.context("source checksum")?;
            let snapshot = target.store().snapshot();
            let mut actual = (0_u64, 0_u64, 0_u64);
            for row in snapshot.scan()? {
                let Value::UInt64(id) = row.values()[0] else {
                    anyhow::bail!("id carrier");
                };
                let fields = row
                    .values()
                    .iter()
                    .map(|value| match value {
                        Value::UInt64(value) => Ok(value.to_string()),
                        Value::Int64(value) => Ok(value.to_string()),
                        Value::Utf8(value) => Ok(value.clone()),
                        Value::Null => Ok("<null>".to_owned()),
                        Value::Binary(bytes) => {
                            let mut hex = String::with_capacity(bytes.len() * 2);
                            for byte in bytes {
                                write!(&mut hex, "{byte:02X}").expect("write to string");
                            }
                            Ok(hex)
                        }
                        _ => Err(anyhow::anyhow!("unexpected carrier: {value:?}")),
                    })
                    .collect::<Result<Vec<_>>>()?;
                actual.0 += 1;
                actual.1 += id;
                actual.2 += u64::from(crc32fast::hash(fields.join("#").as_bytes()));
            }
            ensure!(
                actual == (count, sum, crc),
                "checksum mismatch: {:?} != {:?}",
                actual,
                (count, sum, crc)
            );
            verified_rows += actual.0;
        }
        let source_bytes: u64 = progress
            .lock()
            .unwrap()
            .values()
            .map(|(_, bytes)| bytes)
            .sum();
        println!(
            "{}",
            json!({"mode":mode,"workers":workers,"seconds":seconds,"rows":verified_rows,"estimated_row_bytes":source_bytes,"rows_per_second":verified_rows as f64/seconds,"estimated_mb_per_second":source_bytes as f64/seconds/1e6,"verified":true,"resumed":resumed,"checksum":"all-columns"})
        );
        drop(conn);
    }
    pool.disconnect().await?;
    Ok(())
}
