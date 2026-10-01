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
    let outputs = run_background_merge(
        directory.path(),
        &schema,
        options,
        &inputs,
        true,
        true,
        None,
        999,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
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
fn keyed_schema() -> TableSchema {
    TableSchema::new(1, vec![Column::new(1, "id", DataType::UInt64, false)]).unwrap()
}

fn keyed_row(id: u64, version: u64, deleted: bool) -> StoredRow {
    StoredRow::new(
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap(),
        vec![pintail_types::Value::UInt64(id)],
        version,
        deleted,
    )
}

/// Two overlapping flushed segments: keys 1..=40 and then 20..=60, half of
/// the second a delete.
fn two_overlapping_segments(table: &mut TableStore) {
    table
        .ingest((1..=40).map(|id| keyed_row(id, 1, false)).collect())
        .unwrap();
    table.flush().unwrap();
    table
        .ingest((20..=60).map(|id| keyed_row(id, 2, id % 2 == 0)).collect())
        .unwrap();
    table.flush().unwrap();
}

fn visible_ids(snapshot: &TableSnapshot) -> Vec<u64> {
    snapshot
        .scan()
        .unwrap()
        .iter()
        .map(|row| match row.key().parts() {
            [KeyPart::UInt64(id)] => *id,
            other => panic!("unexpected key {other:?}"),
        })
        .collect()
}

fn expected_ids() -> Vec<u64> {
    (1..=60)
        .filter(|id| *id < 20 || id % 2 == 1)
        .collect::<Vec<u64>>()
}

#[test]
fn a_reader_opened_from_the_files_keeps_merged_segments_until_it_closes() {
    let directory = tempfile::tempdir().unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), keyed_schema(), options).unwrap();
    two_overlapping_segments(&mut table);
    // A query's reader: its manifest is loaded from the files, not handed
    // out by the writer.
    let reader = TableSnapshot::open(directory.path(), keyed_schema()).unwrap();
    assert_eq!(table.compact().unwrap().input_segments(), 2);
    assert_eq!(
        table.reclaim_obsolete_segments().unwrap(),
        0,
        "the merged inputs are still what the open reader reads"
    );
    assert_eq!(visible_ids(&reader), expected_ids());
    drop(reader);
    assert_eq!(table.reclaim_obsolete_segments().unwrap(), 2);
    assert_eq!(visible_ids(&table.snapshot()), expected_ids());
}

#[test]
fn a_table_that_stops_taking_writes_still_merges_and_a_close_publishes_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut table =
        TableStore::open(directory.path(), keyed_schema(), StoreOptions::default()).unwrap();
    two_overlapping_segments(&mut table);
    assert_eq!(table.manifest.segments.len(), 2);
    // No write follows: only maintenance can start the merge, and the
    // close, not a later flush, has to publish it.
    assert!(table.maintain().unwrap(), "a merge is running");
    drop(table);
    let reopened =
        TableStore::open(directory.path(), keyed_schema(), StoreOptions::default()).unwrap();
    assert_eq!(reopened.manifest.segments.len(), 1);
    assert!(reopened.manifest.segments[0].unique_keys);
    assert_eq!(visible_ids(&reopened.snapshot()), expected_ids());
}

#[test]
fn a_merge_asked_to_yield_publishes_nothing_and_loses_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut table =
        TableStore::open(directory.path(), keyed_schema(), StoreOptions::default()).unwrap();
    two_overlapping_segments(&mut table);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
    table.yield_merges_to(Arc::clone(&flag));
    assert!(table.maintain().unwrap());
    drop(table);
    let mut reopened =
        TableStore::open(directory.path(), keyed_schema(), StoreOptions::default()).unwrap();
    assert_eq!(reopened.manifest.segments.len(), 2, "nothing was published");
    assert_eq!(visible_ids(&reopened.snapshot()), expected_ids());
    // Planned again once nobody asks it to yield.
    assert!(reopened.maintain().unwrap());
    drop(reopened);
    let settled =
        TableStore::open(directory.path(), keyed_schema(), StoreOptions::default()).unwrap();
    assert_eq!(settled.manifest.segments.len(), 1);
    assert_eq!(visible_ids(&settled.snapshot()), expected_ids());
}

#[test]
fn a_replayed_insert_does_not_bring_back_a_row_whose_delete_was_merged_away() {
    let directory = tempfile::tempdir().unwrap();
    let options = StoreOptions {
        background_compaction: false,
        ..StoreOptions::default()
    };
    let mut table = TableStore::open(directory.path(), keyed_schema(), options).unwrap();
    // Two source transactions: one inserts keys 1 and 2, the next deletes
    // key 1. Each is flushed.
    let first = || vec![keyed_row(1, 10, false), keyed_row(2, 11, false)];
    let second = || vec![keyed_row(1, 20, true)];
    table.ingest_cdc_in_order(first()).unwrap();
    table.flush().unwrap();
    table.ingest_cdc_in_order(second()).unwrap();
    table.flush().unwrap();
    // The merge takes every segment: nothing is left for the delete to
    // hide, so it is dropped, and no stored row is as new as it was.
    assert_eq!(table.compact().unwrap().input_segments(), 2);
    assert_eq!(visible_ids(&table.snapshot()), vec![2]);
    assert_eq!(table.snapshot().max_row_version(), Some(11));
    drop(table);

    // The process died before the stream's checkpoint moved past either
    // transaction; the stream reads both again, one batch each.
    let mut table = TableStore::open(directory.path(), keyed_schema(), options).unwrap();
    assert_eq!(
        table.applied_version(),
        20,
        "the table remembers the delete it no longer stores"
    );
    assert_eq!(
        table.ingest_cdc_in_order(first()).unwrap().accepted_rows(),
        0
    );
    assert_eq!(
        visible_ids(&table.snapshot()),
        vec![2],
        "the deleted row is not visible between the replayed insert and the replayed delete"
    );
    assert_eq!(
        table.ingest_cdc_in_order(second()).unwrap().accepted_rows(),
        0
    );
    assert_eq!(visible_ids(&table.snapshot()), vec![2]);
    // What the table never held is applied, replayed in one batch or not.
    let mut mixed = second();
    mixed.push(keyed_row(3, 30, false));
    assert_eq!(table.ingest_cdc_in_order(mixed).unwrap().accepted_rows(), 1);
    assert_eq!(visible_ids(&table.snapshot()), vec![2, 3]);

    // A table copied again starts its versions again.
    table.reset_for_resnapshot().unwrap();
    assert_eq!(table.applied_version(), 0);
    assert_eq!(
        table.ingest_cdc_in_order(first()).unwrap().accepted_rows(),
        2
    );
    assert_eq!(visible_ids(&table.snapshot()), vec![1, 2]);
}

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
                super::scan::ScanPart::Layered { .. } => "layered",
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

/// A range scan over part of one segment keeps its key bounds when the
/// segment does not fit the budget and is decoded in row slices instead.
#[test]
fn a_memory_bounded_range_scan_keeps_its_key_bounds() {
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
    table
        .ingest(
            (0..200_000)
                .map(|id| {
                    StoredRow::new(
                        key(id),
                        vec![
                            pintail_types::Value::UInt64(id),
                            pintail_types::Value::Int64(1),
                        ],
                        1,
                        false,
                    )
                })
                .collect(),
        )
        .unwrap();
    table.flush().unwrap();
    let snapshot = table.snapshot();
    let (low, high) = (50_000_u64, 59_999_u64);
    let mut exercised = 0;
    for (single, keyed) in [(true, false), (false, false), (true, true), (false, true)] {
        for limit in (10..=24).map(|shift| 1_usize << shift) {
            let mut stream = snapshot
                .scan_projected_range_stream(&key(low), &key(high), &[1, 2])
                .unwrap()
                .expect("stream");
            if keyed {
                stream.enable_memtable_overlay(&[1]);
            }
            let mut ids = Vec::new();
            let outcome = loop {
                let chunks = if single {
                    stream
                        .next_column_chunk(limit)
                        .map(|chunk| chunk.into_iter().collect::<Vec<_>>())
                } else {
                    stream.next_column_chunks(1, limit)
                };
                match chunks {
                    Ok(chunks) if chunks.is_empty() => break Ok(()),
                    Ok(chunks) => {
                        for chunk in chunks {
                            let columns = chunk.into_decoded_columns();
                            for value in columns.into_iter().next().unwrap().into_values() {
                                let pintail_types::Value::UInt64(id) = value else {
                                    panic!("unexpected {value:?}")
                                };
                                ids.push(id);
                            }
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            match outcome {
                Ok(()) => {
                    exercised += 1;
                    let outside = ids.iter().filter(|id| !(low..=high).contains(id)).count();
                    assert_eq!(
                        (outside, ids.len()),
                        (0, 10_000),
                        "single={single} keyed={keyed} limit={limit}: rows outside the range, or missing"
                    );
                }
                Err(StoreError::MemoryLimitExceeded { .. }) => {}
                Err(error) => panic!("single={single} limit={limit}: {error}"),
            }
        }
    }
    assert!(exercised > 0, "no budget answered at all");
}

/// A segment skipped on its statistics must not let an older memtable
/// version of one of its keys stand in for the version it holds.
#[test]
fn value_pruning_never_surfaces_an_older_memtable_version() {
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
    let row = |id: u64, amount: i64, version: u64| {
        StoredRow::new(
            key(id),
            vec![
                pintail_types::Value::UInt64(id),
                pintail_types::Value::Int64(amount),
            ],
            version,
            false,
        )
    };
    // The current version of key 5 (amount 10) is flushed; a replayed older
    // version (amount 1) then lands in the memtable.
    table
        .ingest((1..=70_000).map(|id| row(id, 10, 5)).collect())
        .unwrap();
    table.flush().unwrap();
    table.ingest(vec![row(5, 1, 3)]).unwrap();
    let snapshot = table.snapshot();
    let current = snapshot
        .scan_projected_range(&key(0), &key(100_000), &[1, 2])
        .unwrap()
        .into_rows()
        .into_iter()
        .map(ProjectedRow::into_values)
        .find(|values| values[0] == pintail_types::Value::UInt64(5))
        .expect("key 5");
    assert_eq!(
        current[1],
        pintail_types::Value::Int64(10),
        "the newest version wins"
    );
    // amount < 5: the segment's statistics (every amount is 10) prove it
    // fails, so it may be skipped - but key 5's answer is still amount 10.
    let bounds = [crate::segment::ColumnBounds {
        column_id: 2,
        domain: crate::segment::BoundDomain::Int,
        lower: None,
        upper: Some(4),
    }];
    // The stream declines a small merge; the bounded scan below serves it.
    let mut surfaced = Vec::new();
    let mut stream = snapshot
        .scan_projected_range_stream_pruned(&key(0), &key(100_000), &[1, 2], &bounds)
        .unwrap();
    while let Some(chunk) = stream
        .as_mut()
        .and_then(|stream| stream.next_column_chunk(usize::MAX).unwrap())
    {
        let mut columns = chunk.into_decoded_columns().into_iter();
        let ids = columns.next().unwrap().into_values();
        let amounts = columns.next().unwrap().into_values();
        surfaced.extend(ids.into_iter().zip(amounts));
    }
    assert!(
        surfaced
            .iter()
            .all(|(_, amount)| *amount != pintail_types::Value::Int64(1)),
        "a stale version surfaced: {surfaced:?}"
    );
    let bounded = snapshot
        .scan_projected_range_bounded_pruned(&key(0), &key(100_000), &[1, 2], usize::MAX, &bounds)
        .unwrap()
        .into_rows()
        .into_iter()
        .map(ProjectedRow::into_values)
        .filter(|values| values[1] == pintail_types::Value::Int64(1))
        .collect::<Vec<_>>();
    assert!(
        bounded.is_empty(),
        "the bounded scan surfaced a stale version: {bounded:?}"
    );
}

/// Two disjoint base segments under a flushed segment of scattered updates,
/// deletes and inserts, plus newer memtable rows, decode as layered bases:
/// each base column by column with the newer keys masked and their live rows
/// placed, the rows between bases served on their own. The answer is the
/// merge's, with the overlay key named or not, and a newer segment older than
/// a base keeps the merge.
#[test]
#[allow(clippy::too_many_lines)]
fn scattered_updates_layer_over_their_bases() {
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
    let mut model: std::collections::BTreeMap<u64, Option<i64>> = std::collections::BTreeMap::new();
    // Bases: keys 2..=70000 and 70002..=140000, step 2.
    for (first, last, version) in [(1_u64, 35_000_u64, 1_u64), (35_001, 70_000, 2)] {
        table
            .ingest(
                (first..=last)
                    .map(|n| row(n * 2, i64::try_from(n).unwrap(), version, false))
                    .collect(),
            )
            .unwrap();
        table.flush().unwrap();
        for n in first..=last {
            model.insert(n * 2, Some(i64::try_from(n).unwrap()));
        }
    }
    // Every 97th key updated, every 1001st deleted, and rows inserted before
    // the first base, between the bases and after the last.
    let mut newer = Vec::new();
    for n in (1..=70_000_u64).step_by(97) {
        newer.push((n * 2, -i64::try_from(n).unwrap(), false));
    }
    for n in (5..=70_000_u64).step_by(1001) {
        newer.push((n * 2, 0, true));
    }
    newer.extend([(1, -1, false), (70_001, -2, false), (150_000, -3, false)]);
    newer.sort_by_key(|(id, _, _)| *id);
    newer.dedup_by_key(|(id, _, _)| *id);
    table
        .ingest(
            newer
                .iter()
                .map(|(id, amount, deleted)| row(*id, *amount, 3, *deleted))
                .collect(),
        )
        .unwrap();
    table.flush().unwrap();
    for (id, amount, deleted) in &newer {
        model.insert(*id, if *deleted { None } else { Some(*amount) });
    }
    assert_eq!(table.manifest.segments.len(), 3);
    // Newer still, in the memtable: one update over a flushed update, one
    // delete of a base row, one insert.
    let latest = [
        (2_u64, -100_i64, false),
        (4, 0, true),
        (70_003, -101, false),
    ];
    table
        .ingest(
            latest
                .iter()
                .map(|(id, amount, deleted)| row(*id, *amount, 4, *deleted))
                .collect(),
        )
        .unwrap();
    for (id, amount, deleted) in latest {
        model.insert(id, if deleted { None } else { Some(amount) });
    }
    let expected = model
        .iter()
        .filter_map(|(id, amount)| amount.map(|amount| (*id, amount)))
        .collect::<Vec<_>>();

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
    let layered = |stream: &ProjectedScanStream| {
        stream
            .parts
            .iter()
            .any(|part| matches!(part, super::scan::ScanPart::Layered { .. }))
    };

    let snapshot = table.snapshot();
    let mut stream = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    stream.enable_memtable_overlay(&[1]);
    assert!(layered(&stream));
    assert_eq!(drain(stream), expected);

    // Unnamed key: the cluster merges and answers the same.
    let fallback = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    assert_eq!(drain(fallback), expected);

    // A range inside the first base does not layer that base (it is not
    // wholly scanned) and still answers exactly.
    let mut partial = snapshot
        .scan_projected_range_stream(&key(1_000), &key(90_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    partial.enable_memtable_overlay(&[1]);
    let within = expected
        .iter()
        .copied()
        .filter(|(id, _)| (1_000..=90_000).contains(id))
        .collect::<Vec<_>>();
    assert_eq!(drain(partial), within);

    // A replayed row older than the bases cannot win over them unseen: the
    // cluster merges, and the base's newer version stands.
    table.ingest(vec![row(8_000, -99, 0, false)]).unwrap();
    let snapshot = table.snapshot();
    let mut stale = snapshot
        .scan_projected_range_stream(&key(0), &key(400_000), &[1, 2])
        .unwrap()
        .expect("streaming scan");
    stale.enable_memtable_overlay(&[1]);
    assert!(!layered(&stale));
    assert_eq!(drain(stale), expected);
}

/// A two-column table whose snapshot copy lands in chunks of a thousand
/// keys, with the source modelled as a plain map beside it.
mod resumed_copy {
    use super::*;
    use pintail_types::Value;
    use std::ops::Bound;

    type Model = BTreeMap<u64, i64>;

    fn schema() -> TableSchema {
        TableSchema::new(
            1,
            vec![
                Column::new(1, "id", DataType::UInt64, false),
                Column::new(2, "amount", DataType::Int64, false),
            ],
        )
        .unwrap()
    }

    fn options() -> StoreOptions {
        StoreOptions {
            background_compaction: false,
            ..StoreOptions::default()
        }
    }

    fn key(id: u64) -> PrimaryKey {
        PrimaryKey::new(vec![KeyPart::UInt64(id)]).unwrap()
    }

    fn source() -> Model {
        (1..=3_000)
            .map(|id| (id, i64::try_from(id % 97).unwrap()))
            .collect()
    }

    /// One chunk as the copy reads it: the source rows in `(after, through]`.
    fn chunk(source: &Model, after: u64, through: u64) -> Vec<StoredRow> {
        source
            .range(after + 1..=through)
            .map(|(id, amount)| {
                StoredRow::new(
                    key(*id),
                    vec![Value::UInt64(*id), Value::Int64(*amount)],
                    0,
                    false,
                )
            })
            .collect()
    }

    fn covers(after: u64, through: Option<u64>) -> (Bound<PrimaryKey>, Bound<PrimaryKey>) {
        (
            if after == 0 {
                Bound::Unbounded
            } else {
                Bound::Excluded(key(after))
            },
            through.map_or(Bound::Unbounded, |through| Bound::Included(key(through))),
        )
    }

    fn visible(table: &TableStore) -> Model {
        table
            .snapshot()
            .scan()
            .unwrap()
            .into_iter()
            .map(|row| {
                let [Value::UInt64(id), Value::Int64(amount)] = row.values() else {
                    panic!("unexpected row shape {row:?}");
                };
                (*id, *amount)
            })
            .collect()
    }

    /// The first run copies two chunks and dies before its journal records
    /// the second; the source then deletes one row and changes another in
    /// that range. The resumed run re-reads the second chunk and the rest.
    fn changed(source: &Model) -> Model {
        let mut changed = source.clone();
        changed.remove(&1_500);
        changed.insert(1_600, -1);
        changed
    }

    #[test]
    fn a_resumed_chunk_retires_the_copy_an_interrupted_run_published() {
        let directory = tempfile::tempdir().unwrap();
        let mut table = TableStore::open(directory.path(), schema(), options()).unwrap();
        let first = source();
        table
            .bulk_ingest_snapshot_covering(chunk(&first, 0, 1_000), covers(0, Some(1_000)))
            .unwrap();
        table
            .bulk_ingest_snapshot_covering(chunk(&first, 1_000, 2_000), covers(1_000, Some(2_000)))
            .unwrap();
        let second = changed(&first);
        table
            .bulk_ingest_snapshot_covering(chunk(&second, 1_000, 2_000), covers(1_000, Some(2_000)))
            .unwrap();
        table
            .bulk_ingest_snapshot_covering(chunk(&second, 2_000, 3_000), covers(2_000, None))
            .unwrap();
        assert_eq!(visible(&table), second);
        assert_eq!(table.manifest.segments.len(), 3, "no chunk kept two copies");
        assert!(
            table.snapshot().sma_fold_state().is_some(),
            "the segments are key-disjoint again"
        );
    }

    #[test]
    fn a_copy_that_finds_the_source_shorter_retires_the_old_tail() {
        let directory = tempfile::tempdir().unwrap();
        let mut table = TableStore::open(directory.path(), schema(), options()).unwrap();
        let first = source();
        for (after, through) in [(0, 1_000), (1_000, 2_000), (2_000, 3_000)] {
            table
                .bulk_ingest_snapshot_covering(
                    chunk(&first, after, through),
                    covers(after, Some(through)),
                )
                .unwrap();
        }
        // The source lost its last thousand rows before the resumed run
        // re-read the second chunk; the page after it comes back empty.
        let shorter = first
            .range(..=2_000)
            .map(|(k, v)| (*k, *v))
            .collect::<Model>();
        table
            .bulk_ingest_snapshot_covering(
                chunk(&shorter, 1_000, 2_000),
                covers(1_000, Some(2_000)),
            )
            .unwrap();
        table
            .bulk_ingest_snapshot_covering(Vec::new(), covers(2_000, None))
            .unwrap();
        assert_eq!(visible(&table), shorter);
        assert_eq!(table.manifest.segments.len(), 2);
    }

    /// The defect as it stood: a resumed chunk published beside its first
    /// copy. Merge-on-read resolves keys both copies hold to the later one,
    /// but a row the source deleted between the runs survives in the older
    /// copy - a row the source does not have, which no later change event
    /// removes because the delete predates the copy.
    #[test]
    fn a_republished_chunk_without_its_range_keeps_a_deleted_row() {
        let directory = tempfile::tempdir().unwrap();
        let mut table = TableStore::open(directory.path(), schema(), options()).unwrap();
        let first = source();
        table.bulk_ingest_snapshot(chunk(&first, 0, 2_000)).unwrap();
        let second = changed(&first);
        table
            .bulk_ingest_snapshot(chunk(&second, 0, 2_000))
            .unwrap();
        let seen = visible(&table);
        assert_eq!(seen.get(&1_600), Some(&-1), "ties go to the later copy");
        assert_eq!(
            seen.get(&1_500),
            first.get(&1_500),
            "the stale row survives"
        );
        assert!(table.snapshot().sma_fold_state().is_none());
    }

    /// A chunk re-read from an unchanged source is byte-identical to its
    /// first copy, and replaces it whatever the key type.
    #[test]
    fn an_identical_republished_chunk_replaces_its_first_copy() {
        let directory = tempfile::tempdir().unwrap();
        let mut table = TableStore::open(directory.path(), schema(), options()).unwrap();
        let first = source();
        table.bulk_ingest_snapshot(chunk(&first, 0, 1_000)).unwrap();
        table
            .bulk_ingest_snapshot(chunk(&first, 1_000, 2_000))
            .unwrap();
        table
            .bulk_ingest_snapshot(chunk(&first, 1_000, 2_000))
            .unwrap();
        assert_eq!(table.manifest.segments.len(), 2);
        assert_eq!(visible(&table), chunk_model(&first, 2_000));
    }

    fn chunk_model(source: &Model, through: u64) -> Model {
        source.range(..=through).map(|(k, v)| (*k, *v)).collect()
    }

    /// A data directory written before the fix holds identical segment
    /// pairs in its manifest. The next writer open drops the earlier file of
    /// each pair; the answer is unchanged and the segments are disjoint.
    #[test]
    fn identical_segment_pairs_left_by_earlier_copies_collapse_on_open() {
        let directory = tempfile::tempdir().unwrap();
        let first = source();
        {
            let mut table = TableStore::open(directory.path(), schema(), options()).unwrap();
            for (after, through) in [(0, 1_000), (1_000, 2_000), (2_000, 3_000)] {
                table
                    .bulk_ingest_snapshot(chunk(&first, after, through))
                    .unwrap();
            }
            // Republish copies of the last two segments the way the old
            // resumed copy did: same bytes, newer files, both live.
            let mut manifest = table.manifest.as_ref().clone();
            for original in &table.manifest.segments[1..] {
                let id = manifest.next_segment_id;
                let file_name = format!("segment-{id:020}.ptseg");
                std::fs::copy(
                    directory.path().join(&original.file_name),
                    directory.path().join(&file_name),
                )
                .unwrap();
                let mut copy = original.clone();
                copy.id = id;
                copy.file_name = file_name;
                manifest.segments.push(copy);
                manifest.next_segment_id += 1;
            }
            manifest.generation += 1;
            manifest.epoch += 1;
            manifest::publish(directory.path(), &manifest).unwrap();
            table.manifest = Arc::new(manifest);
            assert_eq!(visible(&table), first, "the duplicates change no answer");
            assert!(table.snapshot().sma_fold_state().is_none());
        }
        let reopened = TableStore::open(directory.path(), schema(), options()).unwrap();
        assert_eq!(reopened.manifest.segments.len(), 3);
        assert_eq!(visible(&reopened), first);
        assert!(reopened.snapshot().sma_fold_state().is_some());
        let files = std::fs::read_dir(directory.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "ptseg")
            })
            .count();
        assert_eq!(files, 3, "the dropped copies are swept from disk");
    }
}
