//! Correlated subqueries that are not one aggregate value - `EXISTS`, `IN`
//! and `NOT IN`, row constructors against a subquery, a subquery inside a
//! subquery's select list, a subquery in an outer join's ON - answered
//! without one inner execution per outer row.
//!
//! Every expectation is computed here from the fixture's rows. Which path
//! answered is read from the dependent path's counters and from the
//! reasons it records, never from how long anything took.

use pintail_catalog::{DatabaseId, TableId};
use pintail_exec::collation::Collation;
use pintail_exec::{
    Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    dependent_index_builds, dependent_set_executions, dependent_subquery_executions,
    take_dependent_declines,
};
use pintail_sql::{Binder, parse_statement};
use pintail_types::Value;

use super::outer_set_subquery::{Fixture, SCANS, fixture, nullable, run, scan, scans_of, weight};

/// What one statement did on the dependent path.
struct Ran {
    rows: Vec<Vec<String>>,
    per_row: u64,
    sets: u64,
    indexes: u64,
    profile: String,
}

fn ran(fixture: &Fixture, sql: &str) -> Ran {
    let database_id = DatabaseId::new(11);
    let provider = SnapshotScanProvider::new([
        (database_id, TableId::new(111), &fixture.snapshots[0]),
        (database_id, TableId::new(112), &fixture.snapshots[1]),
        (database_id, TableId::new(113), &fixture.snapshots[2]),
    ])
    .expect("provider");
    let statement = parse_statement(sql).expect("parse");
    let bound = Binder::new(&fixture.catalog, Some("app"))
        .bind(&statement)
        .unwrap_or_else(|error| panic!("bind {sql}: {error}"));
    let physical = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .expect("plan");
    let per_row = dependent_subquery_executions();
    let sets = dependent_set_executions();
    let indexes = dependent_index_builds();
    let mut execution = Execution::start_profiled(
        physical,
        &provider,
        256 * 1024 * 1024,
        None,
        Collation::default(),
    )
    .unwrap_or_else(|error| panic!("start {sql}: {error}"));
    let mut rows = Vec::new();
    while let Some(batch) = execution
        .next_batch()
        .unwrap_or_else(|error| panic!("run {sql}: {error}"))
    {
        for row in batch.selection().selected_rows() {
            rows.push(
                (0..batch.columns().len())
                    .map(|column| {
                        match batch
                            .column(column)
                            .and_then(|values| values.value(row))
                            .expect("selected value")
                        {
                            Value::Null => "NULL".to_owned(),
                            Value::Boolean(value) => u8::from(*value).to_string(),
                            Value::Int64(value) => value.to_string(),
                            Value::UInt64(value) => value.to_string(),
                            Value::Utf8(value) => value.clone(),
                            other => format!("{other:?}"),
                        }
                    })
                    .collect(),
            );
        }
    }
    let profile = execution.profile().expect("profile").render();
    Ran {
        rows,
        per_row: dependent_subquery_executions() - per_row,
        sets: dependent_set_executions() - sets,
        indexes: dependent_index_builds() - indexes,
        profile,
    }
}

/// `value [NOT] IN (members)` as `MySQL` answers it: true on a match, else
/// NULL when a member's comparison is undecided, else false.
fn membership(found: bool, undecided: bool, negated: bool) -> String {
    if found {
        u8::from(!negated).to_string()
    } else if undecided {
        "NULL".to_owned()
    } else {
        u8::from(negated).to_string()
    }
}

#[test]
fn exists_over_an_ungrouped_aggregate_is_known_without_running_it() {
    let fixture = fixture(260);
    let always = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE EXISTS \
           (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id AND s.ok > 100) ORDER BY p.id",
    );
    assert_eq!(always.rows.len(), 260, "an aggregate over no rows is a row");
    assert_eq!((always.per_row, always.sets, always.indexes), (0, 0, 0));
    let never = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE NOT EXISTS \
           (SELECT SUM(s.ok) FROM scans s WHERE s.parcel_id = p.id) ORDER BY p.id",
    );
    assert!(never.rows.is_empty());
    assert_eq!((never.per_row, never.sets, never.indexes), (0, 0, 0));
    // LIMIT 0 keeps no row, whatever the aggregate.
    let emptied = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE EXISTS \
           (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id LIMIT 0) ORDER BY p.id",
    );
    assert!(emptied.rows.is_empty());
}

#[test]
fn having_decides_whether_an_ungrouped_aggregate_row_exists() {
    let fixture = fixture(260);
    let some = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE EXISTS \
           (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id HAVING SUM(s.ok) > 2) \
         ORDER BY p.id",
    );
    let sum = |parcel: u64| {
        let oks = scans_of(parcel)
            .into_iter()
            .filter_map(|(ok, _)| ok)
            .collect::<Vec<_>>();
        (!oks.is_empty()).then(|| oks.iter().sum::<u64>())
    };
    let expected = (1..=260_u64)
        .filter(|parcel| sum(*parcel).is_some_and(|sum| sum > 2))
        .map(|parcel| vec![parcel.to_string()])
        .collect::<Vec<_>>();
    assert!(!expected.is_empty() && expected.len() < 260);
    assert_eq!(some.rows, expected);
    assert!(some.sets >= 1 && some.per_row <= 2, "{}", some.profile);
    // A NULL sum is not true, so NOT EXISTS keeps the parcels with no scan.
    let none = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE NOT EXISTS \
           (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id HAVING SUM(s.ok) > 2) \
         ORDER BY p.id",
    );
    let expected = (1..=260_u64)
        .filter(|parcel| sum(*parcel).is_none_or(|sum| sum <= 2))
        .map(|parcel| vec![parcel.to_string()])
        .collect::<Vec<_>>();
    assert_eq!(none.rows, expected);
    assert!(none.sets >= 1 && none.per_row <= 2, "{}", none.profile);
}

#[test]
fn a_correlated_row_membership_reads_its_table_once() {
    let fixture = fixture(300);
    let found = ran(
        &fixture,
        "SELECT p.id FROM parcels p WHERE (p.id, 1) IN \
           (SELECT s.parcel_id, s.ok FROM scans s WHERE s.parcel_id = p.id) ORDER BY p.id",
    );
    let expected = (1..=300_u64)
        .filter(|parcel| scans_of(*parcel).iter().any(|(ok, _)| *ok == Some(1)))
        .map(|parcel| vec![parcel.to_string()])
        .collect::<Vec<_>>();
    assert!(!expected.is_empty());
    assert_eq!(found.rows, expected);
    assert!(found.per_row <= 40, "{}", found.profile);

    // In the select list the answer is three-valued: a scan whose `ok` is
    // NULL leaves a parcel with no matching scan undecided.
    let listed = ran(
        &fixture,
        "SELECT p.id, (p.id, 1) IN \
             (SELECT s.parcel_id, s.ok FROM scans s WHERE s.parcel_id = p.id), \
           (p.id, 1) NOT IN \
             (SELECT s.parcel_id, s.ok FROM scans s WHERE s.parcel_id = p.id) \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(listed.rows.len(), 300);
    let mut undecided_seen = 0;
    for row in &listed.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let scans = scans_of(parcel);
        let found = scans.iter().any(|(ok, _)| *ok == Some(1));
        let undecided = scans.iter().any(|(ok, _)| ok.is_none());
        undecided_seen += usize::from(!found && undecided);
        assert_eq!(
            row[1..],
            [
                membership(found, undecided, false),
                membership(found, undecided, true)
            ],
            "parcel {parcel}"
        );
    }
    assert!(undecided_seen > 0, "the fixture must reach the NULL answer");
    assert!(
        listed.indexes >= 4 && listed.per_row <= 140,
        "{}",
        listed.profile
    );
}

#[test]
fn a_row_membership_in_an_uncorrelated_subquery_runs_it_once() {
    let fixture = fixture(300);
    let listed = ran(
        &fixture,
        "SELECT p.id, (p.id, p.weight) IN (SELECT z.id, z.open FROM zones z), \
           (p.weight, p.id) NOT IN (SELECT s.ok, s.zone_id FROM scans s WHERE s.id <= 20) \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(listed.rows.len(), 300);
    let members = (1..=20_u64).map(scan).collect::<Vec<_>>();
    let mut answers = std::collections::BTreeSet::new();
    for row in &listed.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let in_zones = parcel <= 4 && weight(parcel) == parcel % 2;
        let found = members
            .iter()
            .any(|(_, ok, zone)| *ok == Some(weight(parcel)) && *zone == parcel);
        // `NULL = weight AND zone = id` is NULL only where the zone matches.
        let undecided = members
            .iter()
            .any(|(_, ok, zone)| ok.is_none() && *zone == parcel);
        let expected = membership(found, undecided, true);
        answers.insert(expected.clone());
        assert_eq!(
            row[1..],
            [u8::from(in_zones).to_string(), expected],
            "parcel {parcel}"
        );
    }
    assert!(
        answers.contains("NULL") && answers.contains("1"),
        "{answers:?}"
    );
    // Each membership test waits out a few rows, then reads its subquery
    // once for all of them.
    assert!(
        listed.indexes >= 2 && listed.per_row <= 140,
        "{}",
        listed.profile
    );
}

#[test]
fn a_subquery_in_a_subquery_select_list_is_resolved_per_qualifying_row() {
    let fixture = fixture(700);
    let nested = ran(
        &fixture,
        "SELECT p.id, (SELECT (SELECT z.open FROM zones z WHERE z.id = s.zone_id) \
                         FROM scans s WHERE s.id = p.id) AS open \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(nested.rows.len(), 700);
    for row in &nested.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let expected = (parcel <= SCANS).then(|| scan(parcel).2 % 2);
        assert_eq!(row[1], nullable(expected), "parcel {parcel}");
    }
    assert!(
        nested.sets >= 1 && nested.per_row <= 4,
        "{}",
        nested.profile
    );

    // With a LIMIT of its own the nested subquery is no join of the middle
    // query's: the middle one is a keyed lookup whose select list is
    // resolved against the row it finds.
    let limited = ran(
        &fixture,
        "SELECT p.id, (SELECT (SELECT z.open FROM zones z WHERE z.id = s.zone_id LIMIT 1) \
                         FROM scans s WHERE s.id = p.id) AS open \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(limited.rows, nested.rows);
    assert!(
        limited.indexes >= 1 && limited.per_row <= 80,
        "{}",
        limited.profile
    );

    // A nested subquery that also reads the outermost row is only the
    // per-row path's to substitute.
    let fixture = super::outer_set_subquery::fixture(90);
    let outermost = ran(
        &fixture,
        "SELECT p.id, (SELECT (SELECT COUNT(*) FROM zones z \
                                 WHERE z.id = s.zone_id AND z.open = p.weight % 2) \
                         FROM scans s WHERE s.id = p.id) AS n \
         FROM parcels p ORDER BY p.id",
    );
    for row in &outermost.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let expected = u64::from(scan(parcel).2 % 2 == weight(parcel) % 2);
        assert_eq!(row[1], expected.to_string(), "parcel {parcel}");
    }
    assert!(outermost.per_row >= 90, "{}", outermost.profile);
}

#[test]
fn a_subquery_in_an_outer_join_condition_answers_every_left_row_at_once() {
    let fixture = fixture(260);
    // Correlated to the left side through the second table of its join.
    let joined = ran(
        &fixture,
        "SELECT p.id, COUNT(s.id) FROM parcels p LEFT JOIN scans s \
           ON s.parcel_id = p.id AND s.id IN \
             (SELECT s2.id FROM scans s2 INNER JOIN parcels p2 ON p2.id = s2.parcel_id \
               WHERE p2.id = p.id AND s2.ok > 0) \
         GROUP BY p.id ORDER BY p.id",
    );
    assert_eq!(joined.rows.len(), 260);
    for row in &joined.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let expected = scans_of(parcel)
            .iter()
            .filter(|(ok, _)| ok.is_some_and(|ok| ok > 0))
            .count();
        assert_eq!(row[1], expected.to_string(), "parcel {parcel}");
    }
    assert!(
        joined.sets >= 1 && joined.per_row == 0,
        "{}",
        joined.profile
    );
    assert!(
        joined.profile.contains("Subquery set execution"),
        "{}",
        joined.profile
    );

    // NOT IN keeps a pair only where no member equals and none is NULL.
    let excluded = ran(
        &fixture,
        "SELECT p.id, COUNT(s.id) FROM parcels p LEFT JOIN scans s \
           ON s.parcel_id = p.id AND s.zone_id NOT IN \
             (SELECT s2.ok FROM scans s2 WHERE s2.parcel_id = p.id) \
         GROUP BY p.id ORDER BY p.id",
    );
    let mut kept = 0;
    for row in &excluded.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let scans = scans_of(parcel);
        let any_null = scans.iter().any(|(ok, _)| ok.is_none());
        let expected = scans
            .iter()
            .filter(|(_, zone)| !any_null && !scans.iter().any(|(ok, _)| *ok == Some(*zone)))
            .count();
        kept += expected;
        assert_eq!(row[1], expected.to_string(), "parcel {parcel}");
    }
    assert!(kept > 0, "the fixture must keep some pairs");
    assert!(
        excluded.sets >= 1 && excluded.per_row == 0,
        "{}",
        excluded.profile
    );
}

#[test]
fn exists_and_in_over_a_join_answer_every_row_in_one_execution() {
    let fixture = fixture(300);
    let listed = ran(
        &fixture,
        "SELECT p.id, \
           EXISTS (SELECT 1 FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
                    WHERE s.parcel_id = p.id AND z.open = 1), \
           NOT EXISTS (SELECT s.id, z.id FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
                    WHERE s.parcel_id = p.id AND z.open = 0), \
           p.weight IN (SELECT s.ok FROM scans s WHERE s.parcel_id = p.id), \
           p.weight NOT IN (SELECT s.ok FROM scans s WHERE s.parcel_id = p.id) \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(listed.rows.len(), 300);
    for row in &listed.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let scans = scans_of(parcel);
        let open = scans.iter().any(|(_, zone)| zone % 2 == 1);
        let closed = scans.iter().any(|(_, zone)| zone % 2 == 0);
        let found = scans.iter().any(|(ok, _)| *ok == Some(weight(parcel)));
        let undecided = scans.iter().any(|(ok, _)| ok.is_none());
        assert_eq!(
            row[1..],
            [
                u8::from(open).to_string(),
                u8::from(!closed).to_string(),
                membership(found, undecided, false),
                membership(found, undecided, true),
            ],
            "parcel {parcel}"
        );
    }
    assert!(
        listed.sets >= 3 && listed.per_row == 0,
        "{}",
        listed.profile
    );
}

#[test]
fn an_aggregate_under_a_function_is_not_taken_for_volatile() {
    let fixture = fixture(260);
    let wrapped = ran(
        &fixture,
        "SELECT p.id, (SELECT COALESCE(MAX(s.ok), -1) FROM scans s WHERE s.parcel_id = p.id) \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(wrapped.rows.len(), 260);
    for row in &wrapped.rows {
        let parcel: u64 = row[0].parse().expect("id");
        let expected = scans_of(parcel)
            .into_iter()
            .filter_map(|(ok, _)| ok)
            .max()
            .map_or_else(|| "-1".to_owned(), |ok| ok.to_string());
        assert_eq!(row[1], expected, "parcel {parcel}");
    }
    // One execution for every parcel, and the one that takes the value the
    // subquery has over no rows.
    assert!(
        wrapped.sets >= 1 && wrapped.per_row <= 1,
        "{}",
        wrapped.profile
    );
}

#[test]
fn an_aggregate_of_the_enclosing_query_has_no_set_form() {
    let fixture = fixture(40);
    // `SUM(p.weight)` is the outer query's aggregate: the subquery only
    // decides, per group, whether its value is shown.
    let lifted = ran(
        &fixture,
        "SELECT p.label, (SELECT SUM(p.weight) FROM zones z WHERE z.id = 1) \
         FROM parcels p GROUP BY p.label ORDER BY p.label",
    );
    let mut sums = std::collections::BTreeMap::new();
    for parcel in 1..=40_u64 {
        let label = (parcel % 17 != 0).then(|| format!("L{:02}", parcel * 7 % 13));
        *sums.entry(label).or_insert(0) += weight(parcel);
    }
    let expected = sums
        .into_iter()
        .map(|(label, sum)| vec![label.unwrap_or_else(|| "NULL".to_owned()), sum.to_string()])
        .collect::<Vec<_>>();
    let shown = lifted
        .rows
        .iter()
        .map(|row| vec![row[0].clone(), row[1].trim_end_matches(".0").to_owned()])
        .collect::<Vec<_>>();
    assert_eq!(shown, expected, "{:?}", lifted.rows);
    assert_eq!(lifted.sets, 0, "{}", lifted.profile);
}

#[test]
fn a_volatile_subquery_is_run_for_every_row() {
    let fixture = fixture(60);
    let (rows, per_row, sets) = run(
        &fixture,
        "SELECT p.id, EXISTS (SELECT 1 FROM scans s INNER JOIN zones z ON z.id = s.zone_id \
                    WHERE s.parcel_id = p.id AND RAND() >= 0) FROM parcels p ORDER BY p.id",
    );
    assert_eq!((per_row, sets), (60, 0));
    for row in &rows {
        let parcel: u64 = row[0].parse().expect("id");
        assert_eq!(row[1], u8::from(!scans_of(parcel).is_empty()).to_string());
    }
}

#[test]
fn a_subquery_left_to_the_per_row_path_says_why() {
    let fixture = fixture(40);
    let _ = take_dependent_declines();
    let grouped = ran(
        &fixture,
        "SELECT p.id, (SELECT COUNT(*) FROM scans s WHERE s.parcel_id = p.id \
                         GROUP BY s.parcel_id) FROM parcels p ORDER BY p.id",
    );
    assert_eq!(grouped.sets, 0);
    let declines = take_dependent_declines();
    assert!(
        declines
            .iter()
            .any(|line| line.contains("no set form: it has GROUP BY or HAVING")),
        "{declines:?}"
    );
    assert!(
        grouped
            .profile
            .contains("Subquery no set form: it has GROUP BY or HAVING")
            && grouped.profile.contains("Subquery per-row execution"),
        "{}",
        grouped.profile
    );
}
