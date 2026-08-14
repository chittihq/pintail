//! e36: negative-selection CDC sentinels over observable transaction facts.
//!
//! The oracle label exists only in `LabeledTxn` and is never passed to `decide`.

use common::{Lcg, bench};

const TRANSACTIONS: usize = 100_000;
const EXPECTED_SCHEMA: u8 = 7;

#[derive(Clone, Copy, Debug)]
enum Policy {
    FixedThresholds,
    DiagonalDistance,
    NegativeSelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultKind {
    Truncation,
    VersionRegression,
    SchemaMismatch,
    RowCountMismatch,
    ChecksumMismatch,
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    decoded_rows: u32,
    declared_rows: u32,
    bytes_per_row: u16,
    lag_ms: u16,
    tables: u8,
    version_step: i8,
    schema: u8,
    checksum_ok: bool,
    token: u64,
}

#[derive(Clone, Copy, Debug)]
struct LabeledTxn {
    observation: Observation,
    fault: Option<FaultKind>,
}

#[derive(Default)]
struct Outcome {
    decision_checksum: u64,
    true_positive: u64,
    false_positive: u64,
    faults: u64,
    healthy: u64,
}

fn main() {
    println!("e36 — negative-selection CDC sentinels (simulation tier, audited)");
    let trace = trace();
    for (name, policy) in [
        ("fixed thresholds", Policy::FixedThresholds),
        ("diagonal distance", Policy::DiagonalDistance),
        ("negative selection", Policy::NegativeSelection),
    ] {
        let outcome = evaluate(&trace, policy);
        println!(
            "{name:<20} recall {:>6.2}% false quarantine {:>6.3}% decision {:016x}",
            percentage(outcome.true_positive, outcome.faults),
            percentage(outcome.false_positive, outcome.healthy),
            outcome.decision_checksum,
        );
    }

    bench("fixed thresholds", || {
        evaluate(&trace, Policy::FixedThresholds).decision_checksum
    });
    bench("diagonal distance", || {
        evaluate(&trace, Policy::DiagonalDistance).decision_checksum
    });
    bench("negative selection", || {
        evaluate(&trace, Policy::NegativeSelection).decision_checksum
    });
}

fn percentage(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 * 100.0 / denominator.max(1) as f64
}

fn trace() -> Vec<LabeledTxn> {
    let mut random = Lcg::new(0x3600_0036);
    (0..TRANSACTIONS)
        .map(|index| {
            let mut observation = Observation {
                decoded_rows: 850 + random.below(301) as u32,
                declared_rows: 0,
                bytes_per_row: 72 + random.below(17) as u16,
                lag_ms: 8 + random.below(25) as u16,
                tables: 1 + random.below(3) as u8,
                version_step: 1,
                schema: EXPECTED_SCHEMA,
                checksum_ok: true,
                token: random.next_u64(),
            };
            observation.declared_rows = observation.decoded_rows;

            // Valid but unfamiliar traffic is deliberately outside the learned envelope.
            // It must not be quarantined because its exact invariants still hold.
            if index % 997 == 0 {
                observation.decoded_rows = 4_000;
                observation.declared_rows = 4_000;
                observation.lag_ms = 140;
                observation.tables = 8;
            }

            let fault = if index % 97 == 0 {
                let kind = match (index / 97) % 5 {
                    0 => FaultKind::Truncation,
                    1 => FaultKind::VersionRegression,
                    2 => FaultKind::SchemaMismatch,
                    3 => FaultKind::RowCountMismatch,
                    _ => FaultKind::ChecksumMismatch,
                };
                inject(&mut observation, kind);
                Some(kind)
            } else {
                None
            };
            LabeledTxn { observation, fault }
        })
        .collect()
}

fn inject(observation: &mut Observation, kind: FaultKind) {
    match kind {
        FaultKind::Truncation => {
            observation.decoded_rows = observation.declared_rows.saturating_sub(17);
            observation.bytes_per_row = 41;
        }
        FaultKind::VersionRegression => {
            observation.version_step = -1;
            observation.lag_ms = 91;
        }
        FaultKind::SchemaMismatch => {
            observation.schema = EXPECTED_SCHEMA + 1;
            observation.tables = 6;
        }
        FaultKind::RowCountMismatch => {
            observation.declared_rows += 2_500;
            observation.lag_ms = 73;
        }
        FaultKind::ChecksumMismatch => {
            observation.checksum_ok = false;
            observation.bytes_per_row = 121;
        }
    }
}

fn invariant_violation(observation: &Observation) -> bool {
    observation.decoded_rows != observation.declared_rows
        || observation.version_step != 1
        || observation.schema != EXPECTED_SCHEMA
        || !observation.checksum_ok
}

fn decide(observation: &Observation, policy: Policy) -> bool {
    let anomaly = match policy {
        Policy::FixedThresholds => {
            observation.decoded_rows > 2_000
                || observation.bytes_per_row > 110
                || observation.lag_ms > 100
                || observation.tables > 5
        }
        Policy::DiagonalDistance => diagonal_distance(observation) > 36.0,
        Policy::NegativeSelection => negative_votes(observation) >= 2,
    };
    anomaly && invariant_violation(observation)
}

fn diagonal_distance(observation: &Observation) -> f64 {
    let row_delta = observation.decoded_rows.abs_diff(observation.declared_rows) as f64;
    let features = [
        (observation.decoded_rows as f64 - 1_000.0) / 90.0,
        (observation.bytes_per_row as f64 - 80.0) / 5.0,
        (observation.lag_ms as f64 - 20.0) / 8.0,
        (observation.tables as f64 - 2.0) / 0.8,
        row_delta / 12.0,
        (observation.version_step as f64 - 1.0) / 0.25,
        (observation.schema as f64 - EXPECTED_SCHEMA as f64) / 0.25,
        f64::from(!observation.checksum_ok) / 0.25,
    ];
    features.iter().map(|value| value * value).sum()
}

fn negative_votes(observation: &Observation) -> u8 {
    let row_delta = observation.decoded_rows.abs_diff(observation.declared_rows);
    [
        row_delta > 8,
        observation.bytes_per_row < 55 || observation.bytes_per_row > 108,
        observation.version_step != 1,
        observation.lag_ms > 65,
        observation.schema != EXPECTED_SCHEMA,
        observation.tables > 5,
        !observation.checksum_ok,
        observation.declared_rows > observation.decoded_rows + 1_000,
    ]
    .into_iter()
    .map(u8::from)
    .sum()
}

fn evaluate(trace: &[LabeledTxn], policy: Policy) -> Outcome {
    let mut outcome = Outcome::default();
    for transaction in trace {
        let quarantine = decide(&transaction.observation, policy);
        let is_fault = transaction.fault.is_some();
        outcome.faults += u64::from(is_fault);
        outcome.healthy += u64::from(!is_fault);
        outcome.true_positive += u64::from(quarantine && is_fault);
        outcome.false_positive += u64::from(quarantine && !is_fault);
        outcome.decision_checksum =
            (outcome.decision_checksum ^ transaction.observation.token ^ u64::from(quarantine))
                .wrapping_mul(0x100_0000_01b3);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_kinds_rotate_across_the_trace() {
        let trace = trace();
        let mut counts = [0; 5];
        for transaction in trace {
            if let Some(kind) = transaction.fault {
                counts[kind as usize] += 1;
            }
        }
        assert!(counts.into_iter().all(|count| count > 190));
    }

    #[test]
    fn valid_outliers_do_not_violate_invariants() {
        let transaction = trace()[997];
        assert!(transaction.fault.is_none());
        assert!(!invariant_violation(&transaction.observation));
        assert!(!decide(&transaction.observation, Policy::NegativeSelection));
    }

    #[test]
    fn decision_has_no_label_parameter() {
        let transaction = trace().into_iter().find(|txn| txn.fault.is_some()).unwrap();
        let relabeled = LabeledTxn {
            observation: transaction.observation,
            fault: None,
        };
        assert_eq!(
            decide(&transaction.observation, Policy::NegativeSelection),
            decide(&relabeled.observation, Policy::NegativeSelection)
        );
    }
}
