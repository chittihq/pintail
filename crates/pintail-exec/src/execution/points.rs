//! Scans pinned to a few scattered keys, read one run of keys at a time.
//!
//! `key IN (a, b, c)` bounds a scan's key range by its least and greatest
//! constants, and the scan reads every row between them. This stream reads
//! the runs of listed keys one after another instead, each a ranged read of
//! the table, so a scan pinned to scattered keys reads about a block per
//! run rather than the span they cover. Runs follow in key order, so the
//! stream keeps the scan's order.

use std::collections::VecDeque;

use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_types::Value;

use super::key_lookup::{bound, integer_key, key_ranges, unsigned_type};
use super::order::{integer_type, names_column};
use super::{BatchStream, ExecError, RecordBatch, Scan, ScanProvider};

/// Runs a scan reads one at a time; a list scattered wider than this reads
/// its whole span once.
const MAX_RUNS: usize = 64;

/// The scan's reads, one per run of the keys its `key IN (constants)`
/// conjunct lists, when those keys fall in two to `MAX_RUNS` runs. A list
/// holding anything but integer constants and NULLs compares by conversion
/// and is left to the one read.
fn runs(scan: &Scan) -> Option<Vec<Scan>> {
    let [key_id] = scan.table.key_column_ids.as_slice() else {
        return None;
    };
    scan.predicates.iter().find_map(|predicate| {
        let BoundExprKind::Scalar {
            function: ScalarFunction::InList { negated: false },
            args,
        } = &predicate.kind
        else {
            return None;
        };
        let (key, listed) = args.split_first()?;
        if !integer_type(key.data_type)
            || !matches!(&key.kind, BoundExprKind::Column(column) if names_column(scan, column, *key_id))
        {
            return None;
        }
        let mut keys = Vec::with_capacity(listed.len());
        for argument in listed {
            match &argument.kind {
                BoundExprKind::Literal(Value::Null) => {}
                BoundExprKind::Literal(value) => keys.push(integer_key(value)?),
                _ => return None,
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let runs = key_ranges(&keys, unsigned_type(key.data_type));
        (2..=MAX_RUNS)
            .contains(&runs.len())
            .then(|| runs.into_iter().map(|run| run_scan(scan, key, run)).collect())
    })
}

fn run_scan(scan: &Scan, key: &BoundExpr, (low, high): (i128, i128)) -> Scan {
    let mut run = scan.clone();
    run.predicates.extend([
        bound(key, BinaryOp::GreaterOrEqual, low),
        bound(key, BinaryOp::LessOrEqual, high),
    ]);
    run
}

/// The scan as a stream of its key runs, when it is pinned to scattered
/// keys and the provider can open further reads while the query runs. The
/// first run opens now, so a table that cannot be read fails the query
/// where a single read would.
pub(super) fn open(
    scan: &Scan,
    provider: &dyn ScanProvider,
    memory_limit: usize,
) -> Result<Option<Box<dyn BatchStream>>, ExecError> {
    let Some(runs) = runs(scan) else {
        return Ok(None);
    };
    let Some(tables) = provider.table_provider(scan.table.database_id, scan.table.table_id) else {
        return Ok(None);
    };
    let mut runs = VecDeque::from(runs);
    let first = runs
        .pop_front()
        .map(|run| tables.open_scan(&run, memory_limit))
        .transpose()?;
    Ok(Some(Box::new(RunStream {
        provider: tables,
        runs,
        current: first,
        restrictions: Vec::new(),
    })))
}

struct RunStream {
    provider: Box<dyn ScanProvider + Send + Sync>,
    /// Runs not yet opened, in key order.
    runs: VecDeque<Scan>,
    current: Option<Box<dyn BatchStream>>,
    /// Range restrictions taken before a run opens, replayed onto each run
    /// as it does. A stream that has started ignores one, so a restriction
    /// arriving mid-stream would otherwise reach only the open run.
    restrictions: Vec<(usize, Value, Value)>,
}

/// The fold and memo inputs a run stream does not answer.
///
/// Each of them describes the scan its stream was opened for -
/// `settled_identity` puts that scan's own predicates in the signature the
/// aggregate memo keys on - and a run stream is many scans, read in
/// sequence. Handing back the first run's would name a whole query with
/// the identity of a fraction of it, and the memo would answer the next
/// query from those rows, so a run stream declines all four rather than
/// forwarding one. `key IN (...)` over a settled snapshot therefore folds
/// no SMAs and fills no memo entry; composing the runs' identities into
/// one that names the original scan would lift that, and is not attempted
/// here because an identity that is merely plausible is worse than none.
impl BatchStream for RunStream {
    fn restrict_key_position_range(&mut self, position: usize, min: &Value, max: &Value) {
        if let Some(stream) = &mut self.current {
            stream.restrict_key_position_range(position, min, max);
        }
        self.restrictions.push((position, min.clone(), max.clone()));
    }

    fn next_batch(&mut self, available_memory: usize) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            if let Some(stream) = &mut self.current {
                if let Some(batch) = stream.next_batch(available_memory)? {
                    return Ok(Some(batch));
                }
                // A finished run's stream goes before the next one opens.
                self.current = None;
            }
            let Some(run) = self.runs.pop_front() else {
                return Ok(None);
            };
            let mut stream = self.provider.open_scan(&run, available_memory)?;
            for (position, min, max) in &self.restrictions {
                stream.restrict_key_position_range(*position, min, max);
            }
            self.current = Some(stream);
        }
    }

    fn retained_bytes(&self) -> usize {
        self.current
            .as_ref()
            .map_or(0, |stream| stream.retained_bytes())
    }

    fn next_batch_memory_upper_bound(&self, budget: usize) -> usize {
        self.current
            .as_ref()
            .map_or(0, |stream| stream.next_batch_memory_upper_bound(budget))
    }
}
