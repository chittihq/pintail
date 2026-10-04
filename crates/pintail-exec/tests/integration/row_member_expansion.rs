//! A row constructor's `IN (subquery)` whose members read nothing of the
//! outer row is answered by comparing each outer row with the few members,
//! read once, rather than by a subquery per outer row.
//!
//! Every expectation is computed here from the fixture's rows under
//! three-valued logic. Which path answered is read from the execution
//! counters and the dependent path's execution count, never from timing.

use pintail_exec::take_exec_counters;

use super::outer_set_subquery::{fixture, run, scans_of, weight};

/// Parcel `id`'s label as the fixture writes it: NULL for every
/// seventeenth.
fn label(id: u64) -> Option<String> {
    (!id.is_multiple_of(17)).then(|| format!("L{:02}", id * 7 % 13))
}

/// `a = b` in three-valued logic.
fn equal<T: PartialEq>(a: Option<T>, b: Option<T>) -> Option<bool> {
    Some(a? == b?)
}

/// `a AND b` in three-valued logic.
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// `row [NOT] IN (members)` in three-valued logic, rendered as the engine
/// renders it.
fn membership(comparisons: impl IntoIterator<Item = Option<bool>>, negated: bool) -> String {
    let mut answer = Some(false);
    for comparison in comparisons {
        answer = match (answer, comparison) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        };
    }
    answer.map_or_else(
        || "NULL".to_owned(),
        |found| u8::from(found != negated).to_string(),
    )
}

/// Each outer parcel's `(weight, label)` compared with each member parcel's.
fn compared(outer: u64, members: &[u64], lowered: bool) -> Vec<Option<bool>> {
    members
        .iter()
        .map(|&member| {
            let member_label =
                label(member).map(|label| if lowered { label.to_lowercase() } else { label });
            and(
                Some(weight(outer) == weight(member)),
                // utf8mb4_0900_ai_ci: case folds away.
                equal(
                    label(outer).map(|label| label.to_lowercase()),
                    member_label.map(|label| label.to_lowercase()),
                ),
            )
        })
        .collect()
}

const PARCELS: u64 = 300;

#[test]
fn few_members_are_compared_with_every_row_once_read() {
    let fixture = fixture(PARCELS);
    // Parcel 17's label is NULL: a member of unknown equality.
    let members = [1, 2, 17];
    let _ = take_exec_counters();
    let (rows, per_row, _) = run(
        &fixture,
        "SELECT id, (weight, label) IN (SELECT weight, label FROM parcels WHERE id IN (1, 2, 17)), \
           (weight, label) NOT IN (SELECT weight, label FROM parcels WHERE id IN (1, 2, 17)) \
         FROM parcels ORDER BY id",
    );
    let counters = take_exec_counters();
    let expected = (1..=PARCELS)
        .map(|id| {
            vec![
                id.to_string(),
                membership(compared(id, &members, false), false),
                membership(compared(id, &members, false), true),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
    // Each membership is two EXISTS: the found one and the undecided one.
    assert_eq!(counters.row_members_expanded, 4);
    assert_eq!(per_row, 0, "no subquery runs for an outer row");
    assert!(
        expected.iter().any(|row| row[1] == "NULL") && expected.iter().any(|row| row[1] == "1"),
        "the fixture exercises true, NULL and false answers"
    );
}

#[test]
fn members_compare_under_the_comparison_collation() {
    let fixture = fixture(PARCELS);
    let members = [1, 2, 3];
    let _ = take_exec_counters();
    let (rows, _, _) = run(
        &fixture,
        "SELECT id, (weight, label) IN (SELECT weight, LOWER(label) FROM parcels WHERE id <= 3) \
         FROM parcels ORDER BY id",
    );
    assert_eq!(take_exec_counters().row_members_expanded, 2);
    let expected = (1..=PARCELS)
        .map(|id| {
            vec![
                id.to_string(),
                membership(compared(id, &members, true), false),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
    assert!(expected.iter().any(|row| row[1] == "1"));
}

#[test]
fn a_where_membership_keeps_the_rows_that_match() {
    let fixture = fixture(PARCELS);
    let members = [1, 2, 17];
    for negated in [false, true] {
        let not = if negated { "NOT " } else { "" };
        let _ = take_exec_counters();
        let (rows, per_row, _) = run(
            &fixture,
            &format!(
                "SELECT id FROM parcels WHERE (weight, label) {not}IN \
                   (SELECT weight, label FROM parcels WHERE id IN (1, 2, 17)) ORDER BY id"
            ),
        );
        let expanded = take_exec_counters().row_members_expanded;
        // IN as a WHERE conjunct is one EXISTS the planner joins; NOT IN
        // keeps both of its EXISTS, each compared with the members.
        assert_eq!(
            (expanded, per_row),
            (if negated { 2 } else { 0 }, 0),
            "{not}IN"
        );
        let expected = (1..=PARCELS)
            .filter(|&id| membership(compared(id, &members, false), negated) == "1")
            .map(|id| vec![id.to_string()])
            .collect::<Vec<_>>();
        assert_eq!(rows, expected, "{not}IN");
    }
}

#[test]
fn no_members_is_false_and_too_many_stay_on_the_dependent_path() {
    let fixture = fixture(PARCELS);
    let _ = take_exec_counters();
    let (rows, _, _) = run(
        &fixture,
        "SELECT id, (weight, label) IN (SELECT weight, label FROM parcels WHERE id > 100000), \
           (weight, label) NOT IN (SELECT weight, label FROM parcels WHERE id > 100000) \
         FROM parcels ORDER BY id",
    );
    assert_eq!(take_exec_counters().row_members_expanded, 4);
    assert!(rows.iter().all(|row| row[1] == "0" && row[2] == "1"));
    assert_eq!(u64::try_from(rows.len()).ok(), Some(PARCELS));

    // A hundred members are more than the expansion compares with.
    let members = (1..=100).collect::<Vec<_>>();
    let _ = take_exec_counters();
    let (rows, _, _) = run(
        &fixture,
        "SELECT id, (weight, label) IN (SELECT weight, label FROM parcels WHERE id <= 100) \
         FROM parcels ORDER BY id",
    );
    assert_eq!(take_exec_counters().row_members_expanded, 0);
    let expected = (1..=PARCELS)
        .map(|id| {
            vec![
                id.to_string(),
                membership(compared(id, &members, false), false),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
}

#[test]
fn members_that_read_the_outer_row_stay_on_the_dependent_path() {
    let fixture = fixture(PARCELS);
    let _ = take_exec_counters();
    let (rows, _, _) = run(
        &fixture,
        "SELECT p.id, (p.id, 1) IN (SELECT s.parcel_id, s.ok FROM scans s WHERE s.parcel_id = p.id) \
         FROM parcels p ORDER BY p.id",
    );
    assert_eq!(take_exec_counters().row_members_expanded, 0);
    let expected = (1..=PARCELS)
        .map(|id| {
            let comparisons = scans_of(id)
                .into_iter()
                .map(|(ok, _)| equal(Some(1), ok))
                .collect::<Vec<_>>();
            vec![id.to_string(), membership(comparisons, false)]
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected);
}
