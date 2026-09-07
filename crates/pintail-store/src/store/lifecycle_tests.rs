use super::*;
use pintail_types::{Column, DataType};
use std::{sync::mpsc, time::Duration};

#[test]
fn dropping_store_keeps_writer_lock_until_background_output_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap();
    let mut table =
        TableStore::open(directory.path(), schema.clone(), StoreOptions::default()).unwrap();
    let (result, receiver) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (finished, finish) = mpsc::channel();
    let output = directory.path().join("segment-00000000000000000099.ptseg");
    let worker_output = output.clone();
    // Pause the same worker/receiver ownership pattern used by compaction
    // immediately before output publication. No compaction timing lottery.
    let worker = std::thread::spawn(move || {
        released.recv().unwrap();
        std::fs::write(worker_output, b"unpublished output").unwrap();
        let _ = result.send(Ok(Vec::new()));
        finished.send(()).unwrap();
    });
    table.background = Some(BackgroundMerge {
        worker,
        receiver,
        input_files: Vec::new(),
    });
    let (dropping, started) = mpsc::channel();
    let (dropped, done) = mpsc::channel();
    let closer = std::thread::spawn(move || {
        dropping.send(()).unwrap();
        drop(table);
        dropped.send(()).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let returned_early = done.recv_timeout(Duration::from_millis(100)).is_ok();
    let probe = open_lock(&directory.path().join(WRITER_LOCK_FILE)).unwrap();
    let lock_released_early = FileExt::try_lock_exclusive(&probe).is_ok();
    if lock_released_early {
        FileExt::unlock(&probe).unwrap();
    }
    // Always release the paused worker, including on the failing path.
    release.send(()).unwrap();
    finish.recv_timeout(Duration::from_secs(2)).unwrap();
    closer.join().unwrap();
    assert!(
        !returned_early,
        "store dropped before its background worker finished"
    );
    assert!(
        !lock_released_early,
        "a new writer could race background publication"
    );
    let reopened = TableStore::open(directory.path(), schema, StoreOptions::default()).unwrap();
    assert!(reopened.snapshot().scan().unwrap().is_empty());
    assert!(
        !output.exists(),
        "reopen removes the completed unpublished output"
    );
}

#[test]
fn reset_discards_completed_unpublished_compaction_outputs() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema.clone(), options).unwrap();
    for id in 1..=2 {
        let row = StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
            vec![pintail_types::Value::UInt64(id)],
            id,
            false,
        );
        table.ingest(vec![row]).unwrap();
        table.flush().unwrap();
    }
    let inputs = table.manifest.segments.clone();
    let outputs =
        run_background_merge(directory.path(), &schema, options, &inputs, true, 999).unwrap();
    let (sender, receiver) = mpsc::channel();
    let (ready, sent) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.send(Ok(outputs)).unwrap();
        ready.send(()).unwrap();
    });
    table.background = Some(BackgroundMerge {
        worker,
        receiver,
        input_files: inputs
            .iter()
            .map(|segment| segment.file_name.clone())
            .collect(),
    });
    sent.recv_timeout(Duration::from_secs(2)).unwrap();
    table.reset_for_resnapshot().unwrap();
    table.poll_background_merge().unwrap();
    assert!(
        table.snapshot().scan().unwrap().is_empty(),
        "pre-reset compaction resurrected discarded rows"
    );
    drop(table);
    let reopened = TableStore::open(directory.path(), schema, options).unwrap();
    assert!(reopened.snapshot().scan().unwrap().is_empty());
}

/// Two overlapping segments (a base and a later flush of updates and a
/// delete over part of its range) are merged on read. The merge seeks each
/// stream to the range's lower bound and stops at its upper bound, and the
/// answer is the same as walking everything.
#[test]
fn merge_on_read_over_a_key_range_answers_from_the_newest_versions() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, true),
        ],
    )
    .unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema, options).unwrap();
    let key = |id: u64| PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap();
    let row = |id: u64, amount: i64, version: u64, deleted: bool| {
        StoredRow::new(
            key(id),
            vec![
                pintail_types::Value::UInt64(id),
                pintail_types::Value::Int64(amount),
            ],
            version,
            deleted,
        )
    };
    table
        .ingest(
            (1..=3000)
                .map(|id| row(id, i64::try_from(id).unwrap(), 1, false))
                .collect(),
        )
        .unwrap();
    table.flush().unwrap();
    let mut updates = (1000..=1200)
        .map(|id| row(id, -i64::try_from(id).unwrap(), 2, false))
        .collect::<Vec<_>>();
    updates.push(row(1500, 0, 2, true));
    table.ingest(updates).unwrap();
    table.flush().unwrap();
    assert_eq!(table.manifest.segments.len(), 2, "two overlapping segments");

    let snapshot = table.snapshot();
    let amounts = |start: u64, end: u64| {
        snapshot
            .scan_projected_range(&key(start), &key(end), &[1, 2])
            .unwrap()
            .rows()
            .iter()
            .map(|row| match (&row.values()[0], &row.values()[1]) {
                (pintail_types::Value::UInt64(id), pintail_types::Value::Int64(amount)) => {
                    (*id, *amount)
                }
                other => panic!("unexpected row {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let expected = |start: u64, end: u64| {
        (start..=end)
            .filter(|id| *id != 1500)
            .map(|id| {
                let amount = i64::try_from(id).unwrap();
                (
                    id,
                    if (1000..=1200).contains(&id) {
                        -amount
                    } else {
                        amount
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    // Straddling the updated range's start, inside it, over the delete, and
    // the whole table.
    for (start, end) in [
        (900, 1100),
        (1050, 1150),
        (1400, 1600),
        (1, 3000),
        (2900, 3500),
    ] {
        assert_eq!(
            amounts(start, end),
            expected(start, end.min(3000)),
            "{start}..={end}"
        );
    }
    assert!(amounts(3001, 4000).is_empty());
}

/// A unique-key segment the memtable overlaps is decoded directly with the
/// superseded rows masked by the key column and the memtable's live rows
/// added, once the key column is named; without the name it merges as
/// before. Both answer the same, including at block boundaries.
#[test]
#[allow(clippy::too_many_lines)]
fn a_memtable_overlap_is_masked_from_a_direct_decode() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, true),
        ],
    )
    .unwrap();
    let options = StoreOptions {
        background_compaction: false,
        block_rows: 1_000,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema, options).unwrap();
    let key = |id: u64| PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap();
    let row = |id: u64, amount: i64, version: u64, deleted: bool| {
        StoredRow::new(
            key(id),
            vec![
                pintail_types::Value::UInt64(id),
                pintail_types::Value::Int64(amount),
            ],
            version,
            deleted,
        )
    };
    // Seventy blocks of a thousand rows, keys 2..=140000 step 2.
    table
        .ingest(
            (1..=70_000)
                .map(|n| row(n * 2, i64::try_from(n).unwrap(), 1, false))
                .collect(),
        )
        .unwrap();
    table.flush().unwrap();
    assert_eq!(table.manifest.segments.len(), 1);
    // Updates, deletes and inserts, several on block boundaries: 2000 is the
    // last key of block 0, 2002 the first of block 1, 2001 lies between
    // them, 140000 is the segment's last key.
    let mut model: std::collections::BTreeMap<u64, Option<i64>> = (1..=70_000_u64)
        .map(|n| (n * 2, Some(i64::try_from(n).unwrap())))
        .collect();
    let ops = [
        (6_000_u64, -1_i64, false),
        (2_000, 0, true),
        (2_002, -2, false),
        (2_001, -3, false),
        (140_000, -4, false),
        (140_001, -5, false),
        (300_000, -6, false),
        (14_000, 0, true),
        (14_001, -7, false),
    ];
    table
        .ingest(
            ops.iter()
                .map(|(id, amount, deleted)| row(*id, *amount, 2, *deleted))
                .collect(),
        )
        .unwrap();
    for (id, amount, deleted) in ops {
        model.insert(id, if deleted { None } else { Some(amount) });
    }
    let expected = model
        .iter()
        .filter_map(|(id, amount)| amount.map(|amount| (*id, amount)))
        .collect::<Vec<_>>();

    let snapshot = table.snapshot();
    let drain = |mut stream: ProjectedScanStream| {
        let mut rows = Vec::new();
        loop {
            let chunks = stream.next_column_chunks(3, usize::MAX).unwrap();
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                let mut columns = chunk
                    .into_decoded_columns()
                    .into_iter()
                    .map(DecodedColumn::into_values);
                let ids = columns.next().unwrap();
                let amounts = columns.next().unwrap();
                for (id, amount) in ids.into_iter().zip(amounts) {
                    match (id, amount) {
                        (pintail_types::Value::UInt64(id), pintail_types::Value::Int64(amount)) => {
                            rows.push((id, amount));
                        }
                        other => panic!("unexpected row {other:?}"),
                    }
                }
            }
        }
        assert!(
            rows.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the stream stays in key order"
        );
        rows
    };
    let shape = |stream: &ProjectedScanStream| {
        stream
            .parts
            .iter()
            .map(|part| match part {
                super::scan::ScanPart::Overlay { .. } => "overlay",
                super::scan::ScanPart::Merge { .. } => "merge",
                super::scan::ScanPart::Direct { .. }
                | super::scan::ScanPart::DirectRange { .. } => "direct",
                super::scan::ScanPart::MemtableOnly { .. } => "memtable",
            })
            .collect::<Vec<_>>()
    };

    let mut overlay = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    overlay.enable_memtable_overlay(&[1]);
    assert_eq!(shape(&overlay), ["overlay", "memtable"]);
    assert_eq!(drain(overlay), expected);

    // The key column unnamed: the same part merges instead.
    let fallback = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    assert_eq!(shape(&fallback), ["overlay", "memtable"]);
    assert_eq!(drain(fallback), expected);

    // A projection without the key column still masks by it.
    let mut amounts_only = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[2])
        .unwrap()
        .expect("streaming scan");
    amounts_only.enable_memtable_overlay(&[1]);
    let mut total = 0_i64;
    loop {
        let chunks = amounts_only.next_column_chunks(3, usize::MAX).unwrap();
        if chunks.is_empty() {
            break;
        }
        for chunk in chunks {
            for value in chunk.into_decoded_columns().remove(0).into_values() {
                let pintail_types::Value::Int64(amount) = value else {
                    panic!("unexpected {value:?}")
                };
                total += amount;
            }
        }
    }
    assert_eq!(
        total,
        expected.iter().map(|(_, amount)| amount).sum::<i64>()
    );

    // A memtable row older than the segment's newest version cannot be
    // masked in unseen: the segment merges again and the segment's version
    // wins, as the merge decides.
    table.ingest(vec![row(8_000, -99, 0, false)]).unwrap();
    let snapshot = table.snapshot();
    let mut stale = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    stale.enable_memtable_overlay(&[1]);
    assert_eq!(shape(&stale), ["merge", "memtable"]);
    let rows = drain(stale);
    assert_eq!(rows, expected, "the stale row changes nothing");
}

/// An overlay segment reached after a memtable-only gap, and one reached
/// after a merge, is still masked, whichever API pulls the chunks.
#[test]
fn an_overlay_reached_after_another_part_is_still_masked() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "id", DataType::UInt64, false),
            Column::new(2, "amount", DataType::Int64, true),
        ],
    )
    .unwrap();
    let options = StoreOptions {
        background_compaction: false,
        block_rows: 1_000,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema, options).unwrap();
    let key = |id: u64| PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap();
    let row = |id: u64, amount: i64, version: u64, deleted: bool| {
        StoredRow::new(
            key(id),
            vec![
                pintail_types::Value::UInt64(id),
                pintail_types::Value::Int64(amount),
            ],
            version,
            deleted,
        )
    };
    // One segment of 70,000 rows (enough for the streaming scan), keys
    // 10000..=79999 with one row per key.
    table
        .ingest((10_000..80_000).map(|id| row(id, 1, 1, false)).collect())
        .unwrap();
    table.flush().unwrap();
    // Memtable: inserts below the segment (a memtable-only gap comes first),
    // then updates and deletes inside it.
    table
        .ingest(vec![
            row(5, 7, 2, false),
            row(6, 7, 2, false),
            row(10_500, -1, 2, false),
            row(15_000, 0, 2, true),
        ])
        .unwrap();
    let snapshot = table.snapshot();
    // Two inserts of 7, one row deleted, one row changed from 1 to -1.
    let expected_total: i64 = 7 + 7 + (70_000 - 1) - 2;
    let expected_rows = 2 + 70_000 - 1;

    let sum_via = |single: bool| {
        let mut stream = snapshot
            .scan_projected_range_stream(&key(0), &key(1_000_000), &[1, 2])
            .unwrap()
            .expect("stream");
        stream.enable_memtable_overlay(&[1]);
        let (mut rows, mut total) = (0_usize, 0_i64);
        loop {
            let chunks = if single {
                stream
                    .next_column_chunk(usize::MAX)
                    .unwrap()
                    .into_iter()
                    .collect()
            } else {
                stream.next_column_chunks(4, usize::MAX).unwrap()
            };
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                let columns = chunk.into_decoded_columns();
                let amounts = columns.into_iter().nth(1).unwrap().into_values();
                for value in amounts {
                    let pintail_types::Value::Int64(amount) = value else {
                        panic!("unexpected {value:?}")
                    };
                    rows += 1;
                    total += amount;
                }
            }
        }
        (rows, total)
    };
    assert_eq!(
        sum_via(true),
        (expected_rows, expected_total),
        "single-chunk API"
    );
    assert_eq!(
        sum_via(false),
        (expected_rows, expected_total),
        "multi-chunk API"
    );
}

/// A two-part integer key is masked and interleaved part by part: updates,
/// a delete, an insert between two rows sharing a first part, and an insert
/// past the end all land in key order, and the stream answers as the merge
/// would.
#[test]
#[allow(clippy::too_many_lines)]
fn a_composite_integer_key_takes_the_overlay() {
    let directory = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(
        1,
        vec![
            Column::new(1, "user_id", DataType::Int64, false),
            Column::new(2, "course_id", DataType::Int64, false),
            Column::new(3, "progress", DataType::Int64, true),
        ],
    )
    .unwrap();
    let options = StoreOptions {
        background_compaction: false,
        block_rows: 1_000,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), schema, options).unwrap();
    let key = |user: i64, course: i64| {
        PrimaryKey::new(vec![KeyPart::Int64(user), KeyPart::Int64(course)]).unwrap()
    };
    let row = |user: i64, course: i64, progress: i64, version: u64, deleted: bool| {
        StoredRow::new(
            key(user, course),
            vec![
                pintail_types::Value::Int64(user),
                pintail_types::Value::Int64(course),
                pintail_types::Value::Int64(progress),
            ],
            version,
            deleted,
        )
    };
    // 300 users x 240 courses (even course ids), 72,000 rows.
    let mut rows = Vec::new();
    for user in 1..=300_i64 {
        for course in (2..=480_i64).step_by(2) {
            rows.push(row(user, course, user + course, 1, false));
        }
    }
    table.ingest(rows).unwrap();
    table.flush().unwrap();
    let mut model: std::collections::BTreeMap<(i64, i64), i64> = (1..=300_i64)
        .flat_map(|user| {
            (2..=480_i64)
                .step_by(2)
                .map(move |course| ((user, course), user + course))
        })
        .collect();
    let ops = [
        (7_i64, 10_i64, -1_i64, false), // update
        (7, 11, -2, false),             // insert between (7,10) and (7,12)
        (150, 2, 0, true),              // delete a user's first course
        (300, 480, -3, false),          // update the last row
        (300, 481, -4, false),          // insert past the last row
        (301, 2, -5, false),            // insert past the last user
    ];
    table
        .ingest(
            ops.iter()
                .map(|(user, course, progress, deleted)| {
                    row(*user, *course, *progress, 2, *deleted)
                })
                .collect(),
        )
        .unwrap();
    for (user, course, progress, deleted) in ops {
        if deleted {
            model.remove(&(user, course));
        } else {
            model.insert((user, course), progress);
        }
    }
    let expected = model
        .iter()
        .map(|((user, course), progress)| (*user, *course, *progress))
        .collect::<Vec<_>>();

    let snapshot = table.snapshot();
    let mut stream = snapshot
        .scan_projected_range_stream(&key(0, 0), &key(1_000, 0), &[1, 2, 3])
        .unwrap()
        .expect("stream");
    stream.enable_memtable_overlay(&[1, 2]);
    assert!(matches!(
        stream.parts.front(),
        Some(super::scan::ScanPart::Overlay { .. })
    ));
    let mut actual = Vec::new();
    loop {
        let chunks = stream.next_column_chunks(3, usize::MAX).unwrap();
        if chunks.is_empty() {
            break;
        }
        for chunk in chunks {
            let mut columns = chunk
                .into_decoded_columns()
                .into_iter()
                .map(DecodedColumn::into_values);
            let users = columns.next().unwrap();
            let courses = columns.next().unwrap();
            let progress = columns.next().unwrap();
            for ((user, course), progress) in users.into_iter().zip(courses).zip(progress) {
                match (user, course, progress) {
                    (
                        pintail_types::Value::Int64(user),
                        pintail_types::Value::Int64(course),
                        pintail_types::Value::Int64(progress),
                    ) => actual.push((user, course, progress)),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }
    assert!(
        actual
            .windows(2)
            .all(|pair| (pair[0].0, pair[0].1) < (pair[1].0, pair[1].1)),
        "the stream stays in key order"
    );
    assert_eq!(actual, expected);
}
/// A live table's directory moves without closing the writer: the WAL and
/// lock handles follow, later writes and flushes land in the new place, a
/// snapshot taken after the move reads everything, and the old path is gone.
#[test]
fn a_table_directory_renames_under_a_live_writer() {
    let root = tempfile::tempdir().unwrap();
    let schema = TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let old = root.path().join("table-old");
    let new = root.path().join("table-new");
    let mut table = TableStore::open(&old, schema, options).unwrap();
    let row = |id: u64| {
        StoredRow::new(
            PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
            vec![pintail_types::Value::UInt64(id)],
            id,
            false,
        )
    };
    table.ingest(vec![row(1), row(2)]).unwrap();
    table.flush().unwrap();
    table.ingest(vec![row(3)]).unwrap();

    table.rename_directory(&new).unwrap();
    assert!(!old.exists(), "the old directory is gone");
    assert_eq!(table.directory(), std::fs::canonicalize(&new).unwrap());

    table.ingest(vec![row(4)]).unwrap();
    table.flush().unwrap();
    let ids = table
        .snapshot()
        .scan()
        .unwrap()
        .into_iter()
        .map(|row| match row.values()[0] {
            pintail_types::Value::UInt64(id) => id,
            ref other => panic!("unexpected {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, [1, 2, 3, 4]);
    assert!(new.join("table.wal").exists());
    // A second writer cannot open the moved directory while this one lives.
    assert!(
        TableStore::open(
            &new,
            TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap(),
            options
        )
        .is_err(),
        "the writer lock followed the directory"
    );
    // Renaming onto an existing directory is refused and changes nothing.
    std::fs::create_dir_all(root.path().join("occupied")).unwrap();
    assert!(
        table
            .rename_directory(root.path().join("occupied"))
            .is_err()
    );
    assert_eq!(table.directory(), std::fs::canonicalize(&new).unwrap());
}
