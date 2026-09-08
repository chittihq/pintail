use core_engine_100::{
    live::{Fixture, advance, read},
    proof,
};
use std::{
    sync::{Arc, Barrier},
    time::Instant,
};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let arg = |i: usize, default: usize| args.get(i).map_or(default, |s| s.parse().unwrap());
    let (v, n, scenario, seed, metadata, cap) = (
        arg(1, 0),
        arg(2, 100000),
        arg(3, 0),
        arg(4, 31013),
        arg(5, 1),
        arg(6, 256 << 20),
    );
    let mut f = Fixture::new(n, seed as u64, scenario);
    let mut records = Vec::new();
    for phase in 0..8 {
        let expected = f.expected();
        let answer = proof::expected(&expected);
        let snapshot = f.table.snapshot();
        let events = f.events(phase + 1);
        let sql = proof::sql(v, &expected);
        let barrier = Arc::new(Barrier::new(2));
        let wb = barrier.clone();
        let clock = Instant::now();
        let (result, plan, profile, query_ms, qs, writer_ms, ws, compacted) =
            std::thread::scope(|scope| {
                let table = &mut f.table;
                let writer = scope.spawn(|| {
                    wb.wait();
                    let ws = clock.elapsed().as_secs_f64() * 1000.;
                    let start = Instant::now();
                    let (_, c) = advance(table, &events, phase + 1);
                    (start.elapsed().as_secs_f64() * 1000., ws, c)
                });
                barrier.wait();
                let qs = clock.elapsed().as_secs_f64() * 1000.;
                let start = Instant::now();
                let (result, plan, profile) =
                    proof::execute(&snapshot, &expected, &sql, metadata, cap, phase == 0);
                let query_ms = start.elapsed().as_secs_f64() * 1000.;
                let (wm, ws, c) = writer.join().unwrap();
                (result, plan, profile, query_ms, qs, wm, ws, c)
            });
        let cycle_ms = clock.elapsed().as_secs_f64() * 1000.;
        let (correct, error) = match result {
            Ok(rows) => {
                assert_eq!(rows, answer, "variant={v} phase={phase}");
                (true, None)
            }
            Err(e @ pintail_exec::ExecError::MemoryLimitExceeded { .. }) => {
                (false, Some(e.to_string()))
            }
            Err(pintail_exec::ExecError::Source(e)) if e.contains("memory limit exceeded") => {
                (false, Some(e))
            }
            Err(e) => panic!("unexpected failure {e}"),
        };
        assert_eq!(
            read(&snapshot, &expected).rows,
            expected.rows,
            "pinned view changed"
        );
        f.update_model(&events);
        let next = f.expected();
        assert_eq!(
            read(&f.table.snapshot(), &next).rows,
            next.rows,
            "committed state"
        );
        records.push(serde_json::json!({"phase":phase,"expected":answer,"correct":correct,"error":error,"query_ms":query_ms,"writer_ms":writer_ms,"cycle_ms":cycle_ms,"overlap_ms":((qs+query_ms).min(ws+writer_ms)-qs.max(ws)).max(0.),"compacted_inputs":compacted,"profile":profile,"plan":plan,"sql":sql}));
    }
    let expected = f.expected();
    drop(f.table);
    let reopened = pintail_store::TableStore::open(
        f.directory.path(),
        core_engine_100::live::schema(),
        pintail_store::StoreOptions::default(),
    )
    .unwrap();
    assert_eq!(read(&reopened.snapshot(), &expected).rows, expected.rows);
    println!(
        "{}",
        serde_json::json!({"pk_join":std::env::var_os("PINTAIL_PROOF_PK_JOIN").is_some(),"variant":v,"name":proof::NAMES[v],"rows":n,"scenario":scenario,"seed":seed,"metadata":metadata,"cap":cap,"phases":records,"restart_correct":true})
    );
}
