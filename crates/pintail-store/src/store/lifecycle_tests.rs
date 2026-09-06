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
