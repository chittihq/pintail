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
