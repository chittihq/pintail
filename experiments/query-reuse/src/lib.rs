//! Experimental reuse boundaries; not wired into any server request path.
use pintail_sql::{BoundExpr, BoundExprKind, BoundQuery};
use pintail_types::{StoredRow, Value};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

pub type Rows = Vec<Vec<Value>>;
pub type Answer = Result<Arc<Rows>, String>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RequestKey {
    pub snapshot: u64,
    pub sql: String,
    pub scope: u64,
    pub settings: u64,
    pub max_rows: usize,
}

#[derive(Default)]
struct Flight {
    result: Mutex<Option<Answer>>,
    ready: Condvar,
}

#[derive(Default)]
pub struct Flights {
    entries: Mutex<HashMap<RequestKey, Arc<Flight>>>,
    pub leaders: AtomicUsize,
    pub followers: AtomicUsize,
}

impl Flights {
    pub fn run(
        &self,
        key: RequestKey,
        cancelled: &AtomicBool,
        compute: impl FnOnce() -> Answer,
    ) -> Answer {
        if cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let (flight, leader) = {
            let mut entries = self.entries.lock().unwrap();
            if let Some(flight) = entries.get(&key) {
                (Arc::clone(flight), false)
            } else if entries.len() >= 32 {
                drop(entries);
                self.leaders.fetch_add(1, Ordering::Relaxed);
                return compute();
            } else {
                let flight = Arc::new(Flight::default());
                entries.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };
        if leader {
            self.leaders.fetch_add(1, Ordering::Relaxed);
            let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(compute))
                .unwrap_or_else(|_| Err("execution panicked".into()));
            *flight.result.lock().unwrap() = Some(answer.clone());
            self.entries.lock().unwrap().remove(&key);
            flight.ready.notify_all();
            answer
        } else {
            self.followers.fetch_add(1, Ordering::Relaxed);
            let mut result = flight.result.lock().unwrap();
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                if let Some(answer) = result.as_ref() {
                    return answer.clone();
                }
                result = flight
                    .ready
                    .wait_timeout(result, Duration::from_millis(2))
                    .unwrap()
                    .0;
            }
        }
    }
    pub fn active(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

/// Deliberately narrow grammar. Unknown expressions refuse reuse.
pub fn dependencies(query: &BoundQuery) -> Option<BTreeSet<u32>> {
    if query.tables.len() != 1
        || query.tables[0].input.is_some()
        || query.from.len() != 1
        || !query.from[0].joins.is_empty()
        || !query.windows.is_empty()
        || !query.union_all.is_empty()
        || !query.set_ops.is_empty()
        || query.recursive.is_some()
    {
        return None;
    }
    fn visit(expr: &BoundExpr, set: &mut BTreeSet<u32>) -> Option<()> {
        match &expr.kind {
            BoundExprKind::Column(c) if !c.outer => {
                set.insert(c.column_id);
            }
            BoundExprKind::Literal(_)
            | BoundExprKind::GroupKey(_)
            | BoundExprKind::Aggregate(_) => {}
            BoundExprKind::Binary { left, right, .. } => {
                visit(left, set)?;
                visit(right, set)?;
            }
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                visit(expr, set)?
            }
            _ => return None,
        }
        Some(())
    }
    let mut set = BTreeSet::new();
    for p in &query.projection {
        visit(&p.expr, &mut set)?;
    }
    for e in &query.group_by {
        visit(e, &mut set)?;
    }
    for e in query.filter.iter().chain(query.having.iter()) {
        visit(e, &mut set)?;
    }
    for a in &query.aggregates {
        if !matches!(
            a.function,
            pintail_sql::AggregateFunction::Count
                | pintail_sql::AggregateFunction::Sum
                | pintail_sql::AggregateFunction::Minimum
                | pintail_sql::AggregateFunction::Maximum
        ) {
            return None;
        }
        if let Some(e) = &a.expr {
            visit(e, &mut set)?;
        }
        for (e, _) in &a.order_within {
            visit(e, &mut set)?;
        }
    }
    Some(set)
}

#[derive(Clone, Debug, Default)]
pub struct Epochs {
    pub snapshot: u64,
    pub global: u64,
    pub membership: u64,
    pub columns: HashMap<u32, u64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Validity {
    global: u64,
    membership: u64,
    columns: Vec<(u32, u64)>,
}

impl Epochs {
    pub fn token(&self, deps: &BTreeSet<u32>) -> Validity {
        Validity {
            global: self.global,
            membership: self.membership,
            columns: deps
                .iter()
                .map(|id| (*id, *self.columns.get(id).unwrap_or(&0)))
                .collect(),
        }
    }
    pub fn unknown(&mut self) {
        self.snapshot = self.snapshot.checked_add(1).unwrap();
        self.global = self.global.checked_add(1).unwrap();
    }
    /// Called under the same publication lock as the corresponding store write.
    /// IDs here are fixture schema IDs, not inferred from SQL text.
    pub fn change(&mut self, ids: &[u32], before: Option<&StoredRow>, after: &StoredRow) {
        self.snapshot = self.snapshot.checked_add(1).unwrap();
        let Some(before) = before else {
            self.membership = self.membership.checked_add(1).unwrap();
            return;
        };
        if before.key() != after.key() || before.is_deleted() || after.is_deleted() {
            self.membership = self.membership.checked_add(1).unwrap();
            return;
        }
        if before.values().len() != ids.len() || after.values().len() != ids.len() {
            self.global = self.global.checked_add(1).unwrap();
            return;
        }
        for ((id, old), new) in ids.iter().zip(before.values()).zip(after.values()) {
            if old != new {
                let epoch = self.columns.entry(*id).or_default();
                *epoch = epoch.checked_add(1).unwrap();
            }
        }
    }
}

/// Bounded single-result cache; deliberately no production eviction policy.
#[derive(Default)]
pub struct Reuse {
    value: Option<(String, Validity, Arc<Rows>)>,
}
impl Reuse {
    pub fn get(&self, sql: &str, token: &Validity) -> Option<Arc<Rows>> {
        self.value
            .as_ref()
            .filter(|(s, t, _)| s == sql && t == token)
            .map(|(_, _, r)| Arc::clone(r))
    }
    pub fn put(&mut self, sql: String, token: Validity, rows: Arc<Rows>) {
        let bytes: usize = rows
            .iter()
            .map(|r| {
                std::mem::size_of_val(r.as_slice())
                    + r.iter()
                        .map(|v| match v {
                            Value::Utf8(s) | Value::Enum { label: s, .. } => s.capacity(),
                            Value::Binary(b) => b.capacity(),
                            _ => 0,
                        })
                        .sum::<usize>()
            })
            .sum();
        if bytes <= 1024 * 1024 {
            self.value = Some((sql, token, rows));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Instant};
    fn key() -> RequestKey {
        RequestKey {
            snapshot: 1,
            sql: "fixture".into(),
            scope: 1,
            settings: 1,
            max_rows: 10,
        }
    }
    fn wait_for(f: &Flights, count: usize) {
        let end = Instant::now() + Duration::from_secs(5);
        while f.followers.load(Ordering::Relaxed) < count {
            assert!(Instant::now() < end, "follower did not arrive");
            std::thread::yield_now();
        }
    }
    #[test]
    fn followers_share_only_inflight_work_and_cancellation_is_local() {
        let f = Arc::new(Flights::default());
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let leader = {
            let f = Arc::clone(&f);
            std::thread::spawn(move || {
                f.run(key(), &AtomicBool::new(false), || {
                    started_tx.send(()).unwrap();
                    go_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(Arc::new(vec![vec![Value::UInt64(7)]]))
                })
            })
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = {
            let f = Arc::clone(&f);
            let c = Arc::clone(&cancel);
            std::thread::spawn(move || f.run(key(), &c, || panic!("must not execute")))
        };
        let follower = {
            let f = Arc::clone(&f);
            std::thread::spawn(move || {
                f.run(key(), &AtomicBool::new(false), || {
                    panic!("must not execute")
                })
            })
        };
        wait_for(&f, 2);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(cancelled.join().unwrap().unwrap_err(), "cancelled");
        go_tx.send(()).unwrap();
        let a = leader.join().unwrap().unwrap();
        let b = follower.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(f.active(), 0);
        f.run(key(), &AtomicBool::new(false), || Ok(Arc::new(vec![])))
            .unwrap();
        assert_eq!(f.leaders.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn every_context_boundary_starts_independent_execution() {
        let f = Arc::new(Flights::default());
        let (tx, rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let leader = {
            let f = Arc::clone(&f);
            std::thread::spawn(move || {
                f.run(key(), &AtomicBool::new(false), || {
                    tx.send(()).unwrap();
                    go_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(Arc::new(vec![]))
                })
            })
        };
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        for field in 0..5 {
            let mut k = key();
            match field {
                0 => k.snapshot += 1,
                1 => k.sql.push('x'),
                2 => k.scope += 1,
                3 => k.settings += 1,
                _ => k.max_rows += 1,
            };
            f.run(k, &AtomicBool::new(false), || Ok(Arc::new(vec![])))
                .unwrap();
        }
        go_tx.send(()).unwrap();
        leader.join().unwrap().unwrap();
        assert_eq!(f.leaders.load(Ordering::Relaxed), 6);
    }
    #[test]
    fn failures_and_panics_release_followers_and_allow_retry() {
        for panic in [false, true] {
            let f = Arc::new(Flights::default());
            let (tx, rx) = mpsc::channel();
            let (go_tx, go_rx) = mpsc::channel();
            let leader = {
                let f = Arc::clone(&f);
                std::thread::spawn(move || {
                    f.run(key(), &AtomicBool::new(false), || {
                        tx.send(()).unwrap();
                        go_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        assert!(!panic, "injected failure");
                        Err("injected error".into())
                    })
                })
            };
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let follower = {
                let f = Arc::clone(&f);
                std::thread::spawn(move || {
                    f.run(key(), &AtomicBool::new(false), || {
                        panic!("must not execute")
                    })
                })
            };
            wait_for(&f, 1);
            go_tx.send(()).unwrap();
            assert_eq!(leader.join().unwrap(), follower.join().unwrap());
            assert_eq!(f.active(), 0);
            f.run(key(), &AtomicBool::new(false), || Ok(Arc::new(vec![])))
                .unwrap();
        }
    }
    #[test]
    fn unknown_and_membership_changes_invalidate_even_count_star() {
        use pintail_types::{KeyPart, PrimaryKey};
        let r = |key, version, deleted, v| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(key)]).unwrap(),
                vec![v],
                version,
                deleted,
            )
        };
        let mut e = Epochs::default();
        let count = BTreeSet::new();
        let deps = BTreeSet::from([17]);
        let a = r(1, 1, false, Value::Null);
        let b = r(1, 2, false, Value::Int64(2));
        let ct = e.token(&count);
        let dt = e.token(&deps);
        e.change(&[17], Some(&a), &b);
        assert_eq!(ct, e.token(&count));
        assert_ne!(dt, e.token(&deps));
        let ct = e.token(&count);
        e.change(&[17], Some(&b), &r(2, 3, false, Value::Int64(2)));
        assert_ne!(ct, e.token(&count));
        let ct = e.token(&count);
        e.unknown();
        assert_ne!(ct, e.token(&count));
    }
}
