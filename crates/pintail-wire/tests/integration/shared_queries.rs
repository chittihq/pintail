//! Concurrent identical reads through the engine entry point a client
//! actually uses.
//!
//! The unit tests beside the coordinator prove its own behaviour. This
//! proves the thing that matters to a caller: that a request answered by
//! another request's execution receives exactly the rows it would have
//! produced for itself, and that a write between two reads is never
//! answered from the execution that ran before it.

use std::sync::{Arc, Barrier};

use pintail_meta::MetaStore;
use pintail_wire::{QueryOutput, ReplicaEngine, shared_query_stats};
use pintail_write::LocalDatabase;

const CLIENTS: usize = 16;

struct Fixture {
    _directory: tempfile::TempDir,
    engine: ReplicaEngine,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let data_dir = directory.path().to_path_buf();
    let metadata_path = data_dir.join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path).expect("metadata");
    metadata
        .create_local_database("db-shared", "scratch", "2026-09-07T00:00:00Z")
        .expect("create local database");
    drop(metadata);
    std::fs::create_dir_all(data_dir.join("databases").join("db-shared").join("tables"))
        .expect("table root");
    LocalDatabase::new(&data_dir, &metadata_path, "db-shared")
        .recover()
        .expect("initialize catalog");
    Fixture {
        _directory: directory,
        engine: ReplicaEngine::new(&data_dir, &metadata_path),
    }
}

fn run(fixture: &Fixture, sql: &str) -> QueryOutput {
    fixture
        .engine
        .execute("db-shared", sql, 1_000)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn seed(fixture: &Fixture) {
    run(
        fixture,
        "CREATE TABLE orders (id BIGINT UNSIGNED NOT NULL, region VARCHAR(16) NOT NULL, \
         total BIGINT NOT NULL, PRIMARY KEY (id))",
    );
    let rows = (1..=200_u64)
        .map(|id| {
            let region =
                ["north", "south", "east", "west"][usize::try_from(id % 4).expect("small")];
            format!("({id}, '{region}', {})", id * 3)
        })
        .collect::<Vec<_>>()
        .join(", ");
    run(
        fixture,
        &format!("INSERT INTO orders (id, region, total) VALUES {rows}"),
    );
}

/// Fires `CLIENTS` copies of `sql` at once and returns every answer.
fn burst(fixture: &Fixture, sql: &str) -> Vec<QueryOutput> {
    let barrier = Arc::new(Barrier::new(CLIENTS));
    let threads = (0..CLIENTS)
        .map(|_| {
            let (engine, sql, barrier) =
                (fixture.engine.clone(), sql.to_owned(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                engine.execute("db-shared", &sql, 1_000).expect("query")
            })
        })
        .collect::<Vec<_>>();
    threads
        .into_iter()
        .map(|thread| thread.join().expect("client"))
        .collect()
}

/// One test, not three. The coordinator's counters are process-wide, so
/// two of these running side by side would each see the other's flights
/// and the "never shared" assertion could not be an equality.
#[test]
fn a_burst_shares_one_execution_and_only_when_the_answer_cannot_move() {
    identical_reads_answer_every_client_the_same_rows();
    a_write_between_two_bursts_is_never_answered_from_before_it();
    a_statement_that_reads_the_clock_is_never_shared();
}

fn identical_reads_answer_every_client_the_same_rows() {
    let fixture = fixture();
    seed(&fixture);
    let sql = "SELECT region, COUNT(*) AS orders, SUM(total) AS revenue FROM orders \
               GROUP BY region ORDER BY region";

    // The answer each client would have produced alone.
    let reference = run(&fixture, sql);
    assert_eq!(reference.rows.len(), 4);

    let before = shared_query_stats();
    let answers = burst(&fixture, sql);
    let after = shared_query_stats();

    for answer in &answers {
        assert_eq!(answer.rows, reference.rows, "a shared answer is the answer");
        assert_eq!(answer.fields.len(), reference.fields.len());
        assert_eq!(answer.truncated, reference.truncated);
        assert_eq!(answer.affected, None);
    }
    // Sharing is opportunistic - a client that arrives after the leader
    // finished executes for itself - so the assertion is that it happened
    // at all, not how often.
    assert!(
        after.followed > before.followed,
        "sixteen simultaneous copies of one statement should not be sixteen executions"
    );
    assert!(after.led > before.led);
}

fn a_write_between_two_bursts_is_never_answered_from_before_it() {
    let fixture = fixture();
    seed(&fixture);
    let sql = "SELECT COUNT(*) AS orders FROM orders";

    let first = burst(&fixture, sql);
    for answer in &first {
        assert_eq!(answer.rows, vec![vec![pintail_types::Value::UInt64(200)]]);
    }

    run(
        &fixture,
        "INSERT INTO orders (id, region, total) VALUES (201, 'north', 9)",
    );

    // The write reloads the replica, so the same text is a different key
    // and cannot be answered by anything that ran before the row landed.
    let second = burst(&fixture, sql);
    for answer in &second {
        assert_eq!(answer.rows, vec![vec![pintail_types::Value::UInt64(201)]]);
    }
}

fn a_statement_that_reads_the_clock_is_never_shared() {
    let fixture = fixture();
    seed(&fixture);

    let before = shared_query_stats();
    let answers = burst(
        &fixture,
        "SELECT COUNT(*) FROM orders WHERE total > 0 AND UNIX_TIMESTAMP() > 0",
    );
    assert_eq!(answers.len(), CLIENTS);
    let after = shared_query_stats();
    assert_eq!(
        after.followed, before.followed,
        "a statement whose answer can move between two runs must execute for each caller"
    );
    assert_eq!(after.led, before.led, "and must not even open a flight");
}
