//! `FROM t, u WHERE t.k = u.k AND a OR t.k = u.k AND b`: the equality every
//! OR branch repeats is the join's condition. The answers are those of the
//! predicate as written, whichever way round a branch writes the equality
//! and whether or not a branch holds anything else.

use super::outer_set_subquery::{SCANS, fixture, run, scan};

fn scans_where(keep: impl Fn(u64, u64, u64) -> bool) -> Vec<Vec<String>> {
    (1..=SCANS)
        .filter(|k| {
            let (parcel, _, zone) = scan(*k);
            keep(*k, parcel, zone)
        })
        .map(|k| vec![k.to_string()])
        .collect()
}

#[test]
fn a_condition_shared_by_every_or_branch_keeps_the_answer() {
    let fixture = fixture(260);
    let (rows, ..) = run(
        &fixture,
        "SELECT s.id FROM scans s, parcels p \
          WHERE p.id = s.parcel_id AND p.id = 7 AND s.zone_id = 2 \
             OR p.id = s.parcel_id AND p.id = 9 ORDER BY s.id",
    );
    let expected = scans_where(|_, parcel, zone| (parcel == 7 && zone == 2) || parcel == 9);
    assert!(expected.len() > 1, "the fixture holds such scans");
    assert_eq!(rows, expected);

    // The equality written the other way round in one branch.
    let (rows, ..) = run(
        &fixture,
        "SELECT s.id FROM scans s, parcels p \
          WHERE p.id = s.parcel_id AND s.zone_id = 1 AND p.weight < 50 \
             OR s.parcel_id = p.id AND s.zone_id = 3 ORDER BY s.id",
    );
    let weight = |parcel: u64| parcel * 31 % 101;
    let expected = scans_where(|_, parcel, zone| (zone == 1 && weight(parcel) < 50) || zone == 3);
    assert_eq!(rows, expected);

    // A branch that is the equality alone decides the whole predicate.
    let (rows, ..) = run(
        &fixture,
        "SELECT s.id FROM scans s, parcels p \
          WHERE p.id = s.parcel_id AND p.id = 7 OR s.parcel_id = p.id ORDER BY s.id",
    );
    assert_eq!(rows, scans_where(|_, _, _| true));

    // No condition is shared: the predicate stays as it is.
    let (rows, ..) = run(
        &fixture,
        "SELECT s.id FROM scans s, parcels p \
          WHERE p.id = s.parcel_id AND p.id = 7 OR p.id = 9 AND s.id = 3 ORDER BY s.id",
    );
    assert_eq!(rows, scans_where(|k, parcel, _| parcel == 7 || k == 3));
}
