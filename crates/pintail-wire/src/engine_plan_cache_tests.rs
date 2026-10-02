//! An execution from a kept plan against one that prepares its own.
//!
//! Every test holds two engines over one data directory: `kept` has a plan
//! cache, `fresh` has none. Whatever is changed between two executions -
//! the data, the schema, a session setting, a literal - `kept` must answer
//! what `fresh` answers, and be stopped by what stops `fresh`.

use super::*;
use pintail_write::LocalDatabase;

struct Pair {
    directory: tempfile::TempDir,
    writer: LocalDatabase,
    kept: ReplicaEngine,
    fresh: ReplicaEngine,
}

/// What a client sees of an answer.
type Seen = Result<(Vec<QueryField>, ResultRows, bool), String>;

fn seen(result: Result<QueryOutput, QueryError>) -> Seen {
    result
        .map(|output| (output.fields, output.rows, output.truncated))
        .map_err(|error| error.to_string())
}

/// A local database holding `a` with ids `1..=rows`.
fn pair(rows: u64) -> Pair {
    let directory = tempfile::tempdir().unwrap();
    let metadata_path = directory.path().join("meta.db");
    let meta = MetaStore::open(&metadata_path).unwrap();
    meta.create_local_database("db", "scratch", "2026-10-02T00:00:00Z")
        .unwrap();
    drop(meta);
    std::fs::create_dir_all(directory.path().join("databases/db/tables")).unwrap();
    let writer = LocalDatabase::new(directory.path(), &metadata_path, "db");
    writer.recover().unwrap();
    let values = (1..=rows)
        .map(|id| format!("({id}, {}, 'label-{}')", id % 7, id % 3))
        .collect::<Vec<_>>()
        .join(",");
    for sql in [
        "CREATE TABLE a (id BIGINT UNSIGNED NOT NULL, n BIGINT NOT NULL, \
         label VARCHAR(16) NOT NULL, PRIMARY KEY (id))"
            .to_owned(),
        format!("INSERT INTO a VALUES {values}"),
    ] {
        writer
            .execute(&pintail_sql::parse_statement(&sql).unwrap())
            .unwrap();
    }
    let engine = ReplicaEngine::new(directory.path(), &metadata_path);
    Pair {
        kept: engine.clone().with_plan_cache(256, 1 << 22),
        fresh: engine.with_plan_cache(0, 0),
        writer,
        directory,
    }
}

impl Pair {
    fn write(&self, sql: &str) {
        self.writer
            .execute(&pintail_sql::parse_statement(sql).unwrap())
            .unwrap();
    }

    fn hits(&self) -> u64 {
        self.kept.plan_cache_stats().hits
    }

    /// Runs `sql` three times with the cache - a plan is kept the second
    /// time its statement is prepared, and used from the third - and once
    /// without, under whatever session is installed, and requires one
    /// answer of all four.
    fn agree(&self, sql: &str) -> Seen {
        let fresh = seen(self.fresh.execute("db", sql, 1000));
        for execution in 1..=3 {
            let kept = seen(self.kept.execute("db", sql, 1000));
            assert_eq!(kept, fresh, "{sql}: execution {execution}");
        }
        fresh
    }

    /// [`Self::agree`], requiring that an execution ran from a kept plan.
    fn agree_kept(&self, sql: &str) -> Seen {
        let before = self.hits();
        let answer = self.agree(sql);
        assert!(self.hits() > before, "{sql} was not run from its plan");
        answer
    }
}

const READS: &[&str] = &[
    "SELECT 1 + 1",
    "SELECT id, n, label FROM a WHERE id = 7",
    "SELECT id FROM a WHERE n = 3 ORDER BY id LIMIT 5",
    "SELECT n, COUNT(*) AS rows_seen, SUM(id) FROM a GROUP BY n ORDER BY n",
    "SELECT x.id, y.label FROM a x JOIN a y ON y.id = x.n + 1 WHERE x.id < 20 ORDER BY x.id",
    "SELECT id FROM a WHERE id IN (SELECT n FROM a WHERE id < 10) ORDER BY id",
    "SELECT UPPER(label), LENGTH(label) FROM a WHERE id BETWEEN 3 AND 6 ORDER BY id",
    "SELECT /*+ MAX_EXECUTION_TIME(60000) */ COUNT(*) FROM a",
];

#[test]
fn a_repeated_statement_runs_from_its_plan_and_answers_what_a_fresh_one_does() {
    let pair = pair(200);
    for sql in READS {
        assert!(pair.agree_kept(sql).is_ok(), "{sql}");
    }
    // And again, every one of them from its plan.
    let before = pair.hits();
    for sql in READS {
        pair.agree(sql).unwrap();
    }
    assert_eq!(pair.hits(), before + 3 * READS.len() as u64);
}

#[test]
fn an_error_is_never_kept_and_is_the_same_error_every_time() {
    let pair = pair(10);
    let kept = pair.kept.plan_cache_stats().inserted;
    for sql in [
        "SELECT nope FROM a",
        "SELECT id FROM missing",
        "SELEC 1",
        "SELECT id FROM a WHERE id = (SELECT id FROM a)",
    ] {
        assert!(pair.agree(sql).is_err(), "{sql}");
    }
    // The last one prepares and fails when it runs: its plan may be kept,
    // its error is still raised by every execution.
    assert!(pair.kept.plan_cache_stats().inserted <= kept + 1);
}

#[test]
fn a_statement_whose_answer_can_move_is_never_kept() {
    let pair = pair(10);
    let kept = pair.kept.plan_cache_stats().inserted;
    for sql in [
        "SELECT NOW() IS NOT NULL",
        "SELECT RAND() < 2",
        "SELECT @unset IS NULL",
        "SELECT DATABASE()",
        "SELECT id FROM a WHERE id < UNIX_TIMESTAMP() ORDER BY id LIMIT 2",
        "SHOW TABLES",
        "EXPLAIN SELECT id FROM a",
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'scratch'",
    ] {
        // Whatever each answers - some are answered above the engine - it
        // answers the same with a cache as without, and is never kept.
        let _ = pair.agree(sql);
    }
    assert_eq!(pair.kept.plan_cache_stats().inserted, kept);
    assert_eq!(pair.hits(), 0);
}

/// A preparation that raises a warning is made by every execution, so the
/// client is told every time.
#[test]
fn a_statement_that_warns_while_it_is_prepared_warns_every_time() {
    let pair = pair(10);
    let warnings = |engine: &ReplicaEngine, sql: &str| {
        // What a connection does before each statement.
        pintail_exec::set_session_group_concat_max_len(None);
        let answer = seen(engine.execute("db", sql, 1000));
        (
            answer,
            pintail_exec::take_session_division_warnings(),
            pintail_exec::take_session_conversion_warnings().1,
            pintail_exec::take_session_group_concat_warnings(),
        )
    };
    for sql in [
        "SELECT 7 / 0",
        "SELECT 1 + 'abc'",
        "SELECT id, id / 0 FROM a WHERE id = 3",
        "SELECT id + 'x' FROM a WHERE id < 3 ORDER BY id",
    ] {
        let fresh = warnings(&pair.fresh, sql);
        assert!(
            fresh.1 + fresh.2 + fresh.3 > 0,
            "{sql} was chosen because it warns: {fresh:?}"
        );
        // The first two prepare; whether the later ones run from a kept
        // plan depends on whether preparing or executing raised the
        // warning, and the client is told the same either way.
        for round in 0..5 {
            assert_eq!(warnings(&pair.kept, sql), fresh, "{sql}, execution {round}");
        }
    }
}

#[test]
fn a_write_puts_the_next_execution_on_a_new_plan() {
    let pair = pair(50);
    let sql = "SELECT COUNT(*), SUM(n), MAX(id) FROM a";
    let before_write = pair.agree_kept(sql);
    for sql in READS {
        pair.agree(sql).unwrap();
    }
    pair.write("INSERT INTO a VALUES (1000, 5, 'late')");
    let hits = pair.hits();
    let first = seen(pair.kept.execute("db", sql, 1000));
    assert_eq!(pair.hits(), hits, "a plan from before the write was used");
    assert_ne!(first, before_write);
    assert_eq!(pair.agree_kept(sql), first);
    // The replaced load's plans are gone, not left to fill the cache.
    assert_eq!(pair.kept.plan_cache_stats().entries, 1);

    // A table that did not exist is an error that is never kept, and a
    // statement once it does.
    let sql = "SELECT id, v FROM b ORDER BY id";
    assert!(pair.agree(sql).is_err());
    pair.write("CREATE TABLE b (id BIGINT UNSIGNED NOT NULL, v BIGINT NOT NULL, PRIMARY KEY (id))");
    pair.write("INSERT INTO b VALUES (1, 10), (2, 20)");
    let (_, rows, _) = pair.agree_kept(sql).unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_literal_or_a_limit_is_part_of_the_statement() {
    let pair = pair(100);
    let rows = |sql: &str| pair.agree_kept(sql).unwrap().1;
    assert_ne!(
        rows("SELECT label FROM a WHERE id = 1"),
        rows("SELECT label FROM a WHERE id = 2")
    );
    assert_eq!(rows("SELECT id FROM a ORDER BY id LIMIT 1").len(), 1);
    assert_eq!(rows("SELECT id FROM a ORDER BY id LIMIT 3").len(), 3);
    assert_eq!(rows("SELECT id FROM a ORDER BY id LIMIT 0").len(), 0);
    assert_eq!(
        rows("SELECT id FROM a ORDER BY id LIMIT 3 OFFSET 99").len(),
        1
    );
    // A literal's type is its value's: the same text shape, another plan.
    assert_ne!(
        pair.agree_kept("SELECT 1 + 1").unwrap().0,
        pair.agree_kept("SELECT 1 + 1.5").unwrap().0
    );
    assert_ne!(
        pair.agree_kept("SELECT 9223372036854775807 + 0").unwrap().0,
        pair.agree_kept("SELECT 9223372036854775808 + 0").unwrap().0
    );
    // The row ceiling is the caller's, and decides where a result ends.
    let sql = "SELECT id FROM a ORDER BY id";
    for ceiling in [3, 1000, 3] {
        let kept = seen(pair.kept.execute("db", sql, ceiling));
        assert_eq!(kept, seen(pair.fresh.execute("db", sql, ceiling)));
        assert_eq!(kept.unwrap().2, ceiling == 3, "truncated at {ceiling}");
    }
}

/// One statement under two values of a session setting: each is answered
/// as a fresh preparation under that setting answers it, the two answers
/// differ (the statement was chosen to read the setting), and going back
/// to the first value gives the first answer again.
fn under_each(pair: &Pair, sql: &str, install: &dyn Fn(bool)) {
    install(false);
    let first = pair.agree_kept(sql);
    install(true);
    let second = pair.agree_kept(sql);
    install(false);
    let again = pair.agree(sql);
    assert!(first.is_ok() || second.is_ok(), "{sql}: {first:?}");
    assert_ne!(first, second, "{sql} does not read the setting");
    assert_eq!(first, again, "{sql}");
}

/// `sql_mode` reaches a statement as the flags it parses to.
#[test]
fn the_sql_mode_a_statement_is_read_under_is_part_of_its_plan() {
    let pair = pair(40);
    for (mode, sql) in [
        ("PIPES_AS_CONCAT", "SELECT 'a' || 'b'"),
        ("ANSI_QUOTES", "SELECT \"label\" FROM a WHERE id = 1"),
        ("NO_BACKSLASH_ESCAPES", "SELECT LENGTH('a\\nb')"),
        ("HIGH_NOT_PRECEDENCE", "SELECT NOT 1 BETWEEN -5 AND 5"),
        (
            "NO_UNSIGNED_SUBTRACTION",
            "SELECT id - 2 FROM a WHERE id = 1",
        ),
    ] {
        let run = |alternate: bool| {
            let mode = pintail_sql::ParseMode::from_sql_mode(if alternate { mode } else { "" });
            pintail_sql::with_parse_mode(mode, || {
                let kept = pair.agree(sql);
                assert_eq!(kept, pair.agree(sql));
                kept
            })
        };
        let (plain, moded) = (run(false), run(true));
        assert_ne!(plain, moded, "{mode}: {sql}");
        assert_eq!(run(false), plain, "{mode}: {sql}");
    }
}

#[test]
fn every_session_setting_a_statement_reads_is_part_of_its_plan() {
    let pair = pair(40);
    under_each(&pair, "SELECT FROM_UNIXTIME(86400)", &|alternate| {
        assert!(pintail_exec::set_session_time_zone(Some(if alternate {
            "+05:30"
        } else {
            "+00:00"
        })));
    });
    under_each(&pair, "SELECT FROM_UNIXTIME(86400)", &|alternate| {
        assert!(pintail_exec::set_session_time_zone(Some(if alternate {
            "Asia/Tokyo"
        } else {
            "America/Chicago"
        })));
    });
    let _ = pintail_exec::set_session_time_zone(None);

    under_each(
        &pair,
        "SELECT 'a' = 'A', label FROM a WHERE id = 1",
        &|alternate| {
            pintail_sql::set_session_default_collation(Some(if alternate {
                "utf8mb4_bin"
            } else {
                "utf8mb4_0900_ai_ci"
            }));
        },
    );
    pintail_sql::set_session_default_collation(None);

    under_each(&pair, "SELECT 'a' = 'A'", &|alternate| {
        pintail_sql::set_session_binary_literals(alternate);
    });
    pintail_sql::set_session_binary_literals(false);

    under_each(
        &pair,
        "SELECT 1 / 3, id / 7 FROM a WHERE id = 3",
        &|alternate| {
            pintail_sql::set_session_div_precision_increment(Some(if alternate { 8 } else { 4 }));
        },
    );
    pintail_sql::set_session_div_precision_increment(None);

    under_each(&pair, "SELECT id FROM a ORDER BY id", &|alternate| {
        pintail_sql::set_session_select_limit(alternate.then_some(2));
    });
    pintail_sql::set_session_select_limit(None);

    under_each(
        &pair,
        "SELECT GROUP_CONCAT(id ORDER BY id) FROM a",
        &|alternate| {
            pintail_exec::set_session_group_concat_max_len(Some(if alternate { 8 } else { 1024 }));
        },
    );
    pintail_exec::set_session_group_concat_max_len(None);

    under_each(&pair, "SELECT WEEK('2026-01-01')", &|alternate| {
        pintail_exec::set_session_default_week_format(Some(u8::from(alternate)));
    });
    pintail_exec::set_session_default_week_format(None);

    under_each(&pair, "SELECT DAYNAME('2026-01-01')", &|alternate| {
        assert!(pintail_exec::set_session_calendar_locale(Some(
            if alternate { "de_DE" } else { "en_US" }
        )));
    });
    let _ = pintail_exec::set_session_calendar_locale(None);

    // The pinned clock: no clock function is named, and the answer still
    // moves with the session's date.
    under_each(
        &pair,
        "SELECT CAST(TIME '10:20:30' AS DATETIME)",
        &|alternate| {
            pintail_exec::set_session_timestamp_micros(Some(if alternate {
                1_800_000_000_000_000
            } else {
                1_700_000_000_000_000
            }));
        },
    );
    pintail_exec::set_session_timestamp_micros(None);

    let recursive = "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 50) \
                     SELECT COUNT(*) FROM c";
    under_each(&pair, recursive, &|alternate| {
        pintail_exec::set_session_cte_max_recursion_depth(Some(if alternate { 10 } else { 1000 }));
    });
    pintail_exec::set_session_cte_max_recursion_depth(None);
}

/// Two databases are two replicas: one statement text, sent to each, is
/// answered from each one's own rows, whichever was asked first.
#[test]
fn another_database_is_another_plan() {
    let directory = tempfile::tempdir().unwrap();
    let metadata_path = directory.path().join("meta.db");
    let meta = MetaStore::open(&metadata_path).unwrap();
    for (id, name) in [("one", "first"), ("two", "second")] {
        meta.create_local_database(id, name, "2026-10-02T00:00:00Z")
            .unwrap();
        std::fs::create_dir_all(directory.path().join("databases").join(id).join("tables"))
            .unwrap();
        let writer = LocalDatabase::new(directory.path(), &metadata_path, id);
        writer.recover().unwrap();
        // The same table name, another column type and other rows.
        let column = if id == "one" { "BIGINT" } else { "VARCHAR(8)" };
        let value = if id == "one" { "11" } else { "'eleven'" };
        for sql in [
            format!(
                "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL, v {column} NOT NULL, PRIMARY KEY (id))"
            ),
            format!("INSERT INTO t VALUES (1, {value})"),
        ] {
            writer
                .execute(&pintail_sql::parse_statement(&sql).unwrap())
                .unwrap();
        }
    }
    drop(meta);
    let engine = ReplicaEngine::new(directory.path(), &metadata_path);
    let kept = engine.clone().with_plan_cache(64, 1 << 20);
    let fresh = engine.with_plan_cache(0, 0);
    let sql = "SELECT v FROM t WHERE id = 1";
    // Each database's first execution loads its replica, the next two
    // prepare, and the fourth runs from the kept plan.
    for database in ["one", "two", "one", "two", "one", "two", "one", "two"] {
        let answer = seen(kept.execute(database, sql, 10));
        assert!(answer.is_ok());
        assert_eq!(answer, seen(fresh.execute(database, sql, 10)), "{database}");
    }
    assert_eq!(kept.plan_cache_stats().hits, 2);
    assert_ne!(
        seen(kept.execute("one", sql, 10)),
        seen(kept.execute("two", sql, 10))
    );
}

/// `KILL QUERY` and `max_execution_time` are read by the execution, which a
/// kept plan still starts: each stops it exactly as it stops a fresh one.
#[test]
fn a_kept_plan_is_killed_and_timed_out_as_a_fresh_one_is() {
    let pair = pair(3000);
    let heavy = "SELECT COUNT(*) FROM a x JOIN a y ON x.n < y.n JOIN a z ON z.n = x.n";
    for sql in ["SELECT id, label FROM a WHERE id = 5", "SELECT 1 + 1"] {
        pair.agree_kept(sql).unwrap();
        let hits = pair.hits();
        // A deadline that has passed.
        let elapsed = Instant::now().checked_sub(Duration::from_millis(1));
        let kept = seen(pair.kept.execute_with_deadline("db", sql, 1000, elapsed));
        assert_eq!(
            kept,
            seen(pair.fresh.execute_with_deadline("db", sql, 1000, elapsed)),
            "{sql} past its deadline"
        );
        // An execution cancelled before it started.
        let killed = |engine: &ReplicaEngine| {
            let cancellation = pintail_exec::ExecutionCancellation::new();
            cancellation.cancel();
            pintail_exec::with_execution_cancellation(cancellation, || {
                seen(engine.execute("db", sql, 1000))
            })
        };
        assert_eq!(killed(&pair.kept), killed(&pair.fresh), "{sql} killed");
        assert_eq!(pair.hits(), hits + 2, "{sql}: both ran from the kept plan");
    }
    // The statement's own hint is kept with its plan and still ends it.
    let hinted = heavy.replacen("SELECT", "SELECT /*+ MAX_EXECUTION_TIME(20) */", 1);
    let interrupted = Err(QueryError::Interrupted.to_string());
    assert_eq!(seen(pair.fresh.execute("db", &hinted, 10)), interrupted);
    let hits = pair.hits();
    for _ in 0..3 {
        assert_eq!(seen(pair.kept.execute("db", &hinted, 10)), interrupted);
    }
    assert_eq!(
        pair.hits(),
        hits + 1,
        "the hinted statement ran from its plan"
    );
    // A session deadline tighter than the hint wins, as it does fresh.
    let hinted = heavy.replacen("SELECT", "SELECT /*+ MAX_EXECUTION_TIME(600000) */", 1);
    let soon = || Instant::now().checked_add(Duration::from_millis(20));
    for engine in [&pair.kept, &pair.kept, &pair.kept, &pair.fresh] {
        assert_eq!(
            seen(engine.execute_with_deadline("db", &hinted, 10, soon())),
            interrupted
        );
    }
    // A cancellation that arrives while the kept plan runs.
    let cancellation = pintail_exec::ExecutionCancellation::new();
    let canceller = {
        let cancellation = cancellation.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            cancellation.cancel();
        })
    };
    let started = Instant::now();
    let answer = pintail_exec::with_execution_cancellation(cancellation, || {
        seen(pair.kept.execute("db", &hinted, 10))
    });
    canceller.join().unwrap();
    assert_eq!(answer, interrupted);
    assert!(started.elapsed() < Duration::from_secs(60));
}

/// The memory ceiling is the executing engine's, not the plan's.
#[test]
fn a_kept_plan_runs_under_the_ceiling_of_the_engine_that_runs_it() {
    let pair = pair(3000);
    let sql =
        "SELECT x.id, y.id, x.label FROM a x JOIN a y ON x.n = y.n ORDER BY x.label, y.id LIMIT 5";
    pair.agree_kept(sql).unwrap();
    let small = |engine: &ReplicaEngine| {
        seen(
            engine
                .clone()
                .with_memory_limit(64 * 1024)
                .execute("db", sql, 1000),
        )
    };
    let refused = small(&pair.fresh);
    assert!(
        refused
            .as_ref()
            .is_err_and(|error| error.contains("memory")),
        "{refused:?}"
    );
    let hits = pair.hits();
    assert_eq!(small(&pair.kept), refused);
    assert_eq!(pair.hits(), hits + 1, "refused while running the kept plan");
    pair.agree_kept(sql).unwrap();
}

/// Admission is decided for a kept plan as for a fresh one: a short
/// statement takes the reserve when general capacity is gone, a general
/// one is refused, and the inline lane declines rather than waits.
#[test]
fn a_kept_plan_is_admitted_as_a_fresh_one_is() {
    let mut pair = pair(2000);
    let admission = Arc::new(QueryAdmission::with_wait(4, Duration::from_millis(1)));
    pair.kept.admission = Arc::clone(&admission);
    pair.fresh.admission = Arc::clone(&admission);
    let short = "SELECT id FROM a WHERE id = 1";
    let general = "SELECT COUNT(*) FROM a x JOIN a y ON x.n = y.n";
    let table_less = "SELECT 1 + 1";
    for sql in [short, general, table_less] {
        pair.agree_kept(sql).unwrap();
    }
    let inline = |engine: &ReplicaEngine, sql: &str| match engine
        .execute_answer_inline("db", sql, 1000, None)
    {
        Ok(InlineAnswer::Answered(Answer::Whole(output))) => format!("{:?}", output.rows),
        Ok(other) => format!("{other:?}"),
        Err(error) => error.to_string(),
    };
    // Nothing held: each lane does what it does without a cache.
    for sql in [short, general, table_less] {
        assert_eq!(inline(&pair.kept, sql), inline(&pair.fresh, sql), "{sql}");
    }
    assert_eq!(inline(&pair.kept, short), "NotBounded");
    // General capacity gone, the reserve free.
    let general_slots = (0..3)
        .map(|_| admission.try_admit().unwrap())
        .collect::<Vec<_>>();
    let hits = pair.hits();
    for (sql, admitted) in [(short, true), (table_less, true), (general, false)] {
        let kept = seen(pair.kept.execute("db", sql, 1000));
        assert_eq!(kept, seen(pair.fresh.execute("db", sql, 1000)), "{sql}");
        assert_eq!(kept.is_ok(), admitted, "{sql}: {kept:?}");
        if !admitted {
            assert_eq!(kept, Err(QueryError::Overloaded.to_string()));
        }
    }
    assert_eq!(
        pair.hits(),
        hits + 3,
        "all three were decided from kept plans"
    );
    // Every slot gone: a worker is refused, the inline lane declines.
    let _reserve = admission.try_admit_class(QueryClass::Short).unwrap();
    for sql in [short, table_less] {
        assert_eq!(
            seen(pair.kept.execute("db", sql, 1000)),
            Err(QueryError::Overloaded.to_string())
        );
        assert_eq!(
            seen(pair.fresh.execute("db", sql, 1000)),
            Err(QueryError::Overloaded.to_string())
        );
    }
    assert_eq!(inline(&pair.kept, table_less), "NotNow");
    assert_eq!(inline(&pair.fresh, table_less), "NotNow");
    drop(general_slots);
    pair.agree(short).unwrap();
}

/// A statement whose table is being copied again after a schema change
/// waits for the copy whether or not its plan is kept, and the refusal past
/// the wait is the same.
#[test]
fn a_kept_plan_waits_for_a_recopied_table_as_a_fresh_one_does() {
    const NOW: &str = "2026-10-02T00:00:00Z";
    let pair = pair(20);
    let sql = "SELECT COUNT(*) FROM a";
    let ready = pair.agree_kept(sql);
    let metadata_path = pair.directory.path().join("meta.db");
    // A second schema generation, as a streamed ALTER records one, and the
    // recopy it queued begins.
    let mut meta = MetaStore::open(&metadata_path).unwrap();
    let columns = meta
        .schema_history("db", "a")
        .unwrap()
        .last()
        .map(|record| record.columns_json.clone())
        .or_else(|| {
            let database = meta.database("db").unwrap().unwrap();
            let report: ProbeReport = serde_json::from_str(database.probe_json.as_deref()?).ok()?;
            let table = report.tables.into_iter().find(|table| table.name == "a")?;
            serde_json::to_string(&table.columns).ok()
        })
        .expect("the table's columns");
    let version = meta
        .schema_history("db", "a")
        .unwrap()
        .last()
        .map_or(2, |record| record.version + 1);
    for version in [version, version + 1] {
        if meta.schema_history("db", "a").unwrap().len() < 2 {
            meta.record_schema_history("db", "a", version, Some("ALTER TABLE a"), &columns, NOW)
                .unwrap();
        }
    }
    meta.begin_table_resnapshot("db", "a").unwrap();
    drop(meta);

    // The copy outlasts a short wait: refused, after waiting.
    let impatient = |engine: &ReplicaEngine| {
        let started = Instant::now();
        let refused = seen(
            engine
                .clone()
                .with_recopy_wait(Duration::from_millis(150))
                .execute("db", sql, 1000),
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "{refused:?}"
        );
        refused
    };
    let refused = impatient(&pair.fresh);
    assert!(
        refused
            .as_ref()
            .is_err_and(|error| error.contains("not ready")),
        "{refused:?}"
    );
    for _ in 0..3 {
        assert_eq!(impatient(&pair.kept), refused);
    }
    // The copy ends while a statement waits for it.
    let finisher = {
        let metadata_path = metadata_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            MetaStore::open(&metadata_path)
                .unwrap()
                .finish_table_resnapshot("db", "a", "ready")
                .unwrap();
        })
    };
    let patient = pair.kept.clone().with_recopy_wait(Duration::from_secs(60));
    let started = Instant::now();
    let waited = seen(patient.execute("db", sql, 1000));
    assert!(started.elapsed() >= Duration::from_millis(250));
    finisher.join().unwrap();
    assert_eq!(
        waited.map(|answer| answer.1),
        ready.clone().map(|answer| answer.1)
    );
    assert_eq!(
        pair.agree(sql).map(|answer| answer.1),
        ready.map(|answer| answer.1)
    );
}

/// What [`ReplicaEngine::execute_answer_in_place`] did with a statement,
/// and how many times it entered the place it was given to run a read in.
fn in_place(engine: &ReplicaEngine, sql: &str, deadline: Option<Instant>) -> (String, usize) {
    let entered = std::cell::Cell::new(0);
    let outcome = engine.execute_answer_in_place("db", sql, 1000, deadline, &|read| {
        entered.set(entered.get() + 1);
        read();
    });
    let outcome = match outcome {
        Ok(InlineAnswer::Answered(Answer::Whole(output))) => {
            format!("{:?}", seen(Ok(output)))
        }
        Ok(other) => format!("{other:?}"),
        Err(error) => format!("{:?}", seen(Err(error))),
    };
    (outcome, entered.get())
}

/// A small read that is kept prepared runs where it is received, inside
/// the place its caller gives it, and answers what a worker answers.
/// Everything else is declined untouched.
#[test]
fn a_kept_small_read_runs_in_place_and_nothing_else_does() {
    let pair = pair(5000);
    // The replica is loaded: what follows is about plans.
    pair.fresh
        .execute("db", "SELECT COUNT(*) FROM a", 10)
        .unwrap();
    let lookup = "SELECT id, n, label FROM a WHERE id = 4242";
    let aggregate = "SELECT COUNT(*), MAX(n) FROM a WHERE id = 17";
    let table_less = "SELECT 1 + 1";
    let whole = "SELECT COUNT(*) FROM a WHERE n = 3";
    let joined = "SELECT COUNT(*) FROM a x JOIN a y ON x.n = y.n";

    // Not kept yet: declined, and not for good - a worker prepares it.
    for sql in [lookup, aggregate, table_less, whole, joined, "SELEC 1"] {
        assert_eq!(
            in_place(&pair.kept, sql, None),
            ("NotNow".to_owned(), 0),
            "{sql}"
        );
    }
    // A worker that runs a kept small read says so, and only then.
    for (sql, small) in [
        (lookup, true),
        (aggregate, true),
        (table_less, false),
        (whole, false),
        (joined, false),
    ] {
        let _ = take_small_read_seen();
        pair.kept.execute("db", sql, 1000).unwrap();
        assert!(!take_small_read_seen(), "{sql}: prepared, not kept");
        pair.kept.execute("db", sql, 1000).unwrap();
        assert!(!take_small_read_seen(), "{sql}: kept by this execution");
        pair.kept.execute("db", sql, 1000).unwrap();
        assert_eq!(take_small_read_seen(), small, "{sql}");
        assert!(!take_small_read_seen(), "taken once");
    }
    for sql in [lookup, aggregate] {
        let worker = format!("{:?}", seen(pair.fresh.execute("db", sql, 1000)));
        assert_eq!(in_place(&pair.kept, sql, None), (worker, 1), "{sql}");
    }
    // No table: bounded by its text, it runs without the place.
    let worker = format!("{:?}", seen(pair.fresh.execute("db", table_less, 1000)));
    assert_eq!(in_place(&pair.kept, table_less, None), (worker, 0));
    // Kept, and not small.
    for sql in [whole, joined] {
        assert_eq!(
            in_place(&pair.kept, sql, None),
            ("NotBounded".to_owned(), 0),
            "{sql}"
        );
    }
    // An engine without a cache keeps nothing to run.
    assert_eq!(
        in_place(&pair.fresh, lookup, None),
        ("NotBounded".to_owned(), 0)
    );

    // A write replaces the replica the plan was kept against.
    pair.write("INSERT INTO a VALUES (900000, 1, 'late')");
    assert_eq!(in_place(&pair.kept, lookup, None), ("NotNow".to_owned(), 0));

    // Over a database small enough that anything of bounded shape is a
    // short query, a scan is a small read too.
    let small = super::plan_cache_tests::pair(100);
    let scan = "SELECT id, label FROM a WHERE n > 2";
    for _ in 0..4 {
        small.kept.execute("db", scan, 1000).unwrap();
    }
    assert!(take_small_read_seen());
    let worker = format!("{:?}", seen(small.fresh.execute("db", scan, 1000)));
    assert_eq!(in_place(&small.kept, scan, None), (worker, 1));
}

/// A small read in place is stopped, refused and made to wait by what
/// does so on a worker.
#[test]
fn a_small_read_in_place_is_killed_timed_out_and_admitted_as_on_a_worker() {
    let mut pair = pair(5000);
    let admission = Arc::new(QueryAdmission::with_wait(4, Duration::from_millis(1)));
    pair.kept.admission = Arc::clone(&admission);
    pair.fresh.admission = Arc::clone(&admission);
    let sql = "SELECT id, n, label FROM a WHERE id = 4242";
    pair.agree_kept(sql).unwrap();
    let worker = |deadline| {
        format!(
            "{:?}",
            seen(pair.fresh.execute_with_deadline("db", sql, 1000, deadline))
        )
    };

    // max_execution_time: a deadline that has passed.
    let elapsed = Instant::now().checked_sub(Duration::from_millis(1));
    assert_eq!(in_place(&pair.kept, sql, elapsed).0, worker(elapsed));
    // KILL QUERY: an execution cancelled before it started.
    let cancellation = pintail_exec::ExecutionCancellation::new();
    cancellation.cancel();
    pintail_exec::with_execution_cancellation(cancellation, || {
        assert_eq!(in_place(&pair.kept, sql, None).0, worker(None));
    });
    // The memory ceiling is the running engine's.
    let small = |engine: &ReplicaEngine| engine.clone().with_memory_limit(1);
    assert_eq!(
        in_place(&small(&pair.kept), sql, None).0,
        format!("{:?}", seen(small(&pair.fresh).execute("db", sql, 1000)))
    );

    // General capacity gone: a short read still has the reserve.
    let general = (0..3)
        .map(|_| admission.try_admit().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(in_place(&pair.kept, sql, None), (worker(None), 1));
    // Every slot gone: waiting for one is a worker's to do.
    let reserve = admission.try_admit_class(QueryClass::Short).unwrap();
    assert_eq!(in_place(&pair.kept, sql, None), ("NotNow".to_owned(), 0));
    drop((general, reserve));

    // A table under copy is waited for, which is a worker's to do too.
    let metadata_path = pair.directory.path().join("meta.db");
    let meta = MetaStore::open(&metadata_path).unwrap();
    meta.begin_table_resnapshot("db", "a").unwrap();
    assert_eq!(in_place(&pair.kept, sql, None).0, "NotNow");
    meta.finish_table_resnapshot("db", "a", "ready").unwrap();
    drop(meta);
    assert_eq!(pair.agree(sql).map(|answer| answer.1.len()), Ok(1));
}
