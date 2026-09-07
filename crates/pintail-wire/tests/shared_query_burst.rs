//! What a burst of identical reads costs with and without one execution
//! answering all of them. Ignored: a measurement, not a gate. Run with
//! `cargo test --release -p pintail-wire --test shared_query_burst --
//! --ignored --nocapture`, and again with
//! `PINTAIL_DISABLE_SHARED_QUERIES=1` for the arm that executes each
//! request on its own.

use std::sync::{Arc, Barrier};
use std::time::Instant;

use pintail_meta::MetaStore;
use pintail_wire::{ReplicaEngine, shared_query_stats};
use pintail_write::LocalDatabase;

const ROWS: u64 = 200_000;
const CLIENTS: usize = 16;
const RUNS: usize = 5;

const QUERY: &str = "SELECT region, COUNT(*) AS orders, SUM(total) AS revenue FROM orders \
                     GROUP BY region ORDER BY region";

#[test]
#[ignore = "a measurement of concurrent demand, not a gate"]
fn a_burst_of_identical_reads() {
    let directory = tempfile::tempdir().expect("directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .create_local_database("db-burst", "scratch", "2026-09-07T00:00:00Z")
        .expect("database");
    drop(metadata);
    std::fs::create_dir_all(data_dir.join("databases").join("db-burst").join("tables"))
        .expect("table root");
    LocalDatabase::new(&data_dir, &metadata_path, "db-burst")
        .recover()
        .expect("catalog");
    let engine = ReplicaEngine::new(&data_dir, &metadata_path);

    engine
        .execute(
            "db-burst",
            "CREATE TABLE orders (id BIGINT UNSIGNED NOT NULL, region VARCHAR(16) NOT NULL, \
             total BIGINT NOT NULL, PRIMARY KEY (id))",
            1,
        )
        .expect("create");
    for chunk in 0..(ROWS / 10_000) {
        let values = (0..10_000_u64)
            .map(|offset| {
                let id = chunk * 10_000 + offset + 1;
                let region =
                    ["north", "south", "east", "west"][usize::try_from(id % 4).expect("small")];
                format!("({id}, '{region}', {})", id % 977)
            })
            .collect::<Vec<_>>()
            .join(",");
        engine
            .execute(
                "db-burst",
                &format!("INSERT INTO orders (id, region, total) VALUES {values}"),
                1,
            )
            .expect("insert");
    }

    // Warms the replica cache, so the reading is the query rather than the
    // first load of the table.
    let reference = engine.execute("db-burst", QUERY, 1_000).expect("reference");

    let mut best = f64::MAX;
    let before = shared_query_stats();
    for _ in 0..RUNS {
        let barrier = Arc::new(Barrier::new(CLIENTS));
        let clock = Instant::now();
        let clients = (0..CLIENTS)
            .map(|_| {
                let (engine, barrier) = (engine.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    engine.execute("db-burst", QUERY, 1_000).expect("query")
                })
            })
            .collect::<Vec<_>>();
        let answers = clients
            .into_iter()
            .map(|client| client.join().expect("client"))
            .collect::<Vec<_>>();
        let elapsed = clock.elapsed().as_secs_f64() * 1_000.0;
        for answer in &answers {
            assert_eq!(answer.rows, reference.rows, "every client gets the answer");
        }
        best = best.min(elapsed);
    }
    let after = shared_query_stats();

    println!();
    println!("{ROWS} rows, {CLIENTS} simultaneous copies of one grouped aggregate");
    println!("  best burst        = {best:.1} ms");
    println!(
        "  executions        = {} led, {} answered by another",
        after.led - before.led,
        after.followed - before.followed
    );
    println!(
        "  fell back         = {}, refused = {}",
        after.fell_back - before.fell_back,
        after.refused - before.refused
    );
}
