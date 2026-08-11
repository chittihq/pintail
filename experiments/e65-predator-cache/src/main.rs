//! e65: damped predator/prey control of cache-class populations.
use common::{Lcg, bench, check_consistency};
const ITEMS: usize = 120;
const CAP: usize = 30;
#[derive(Clone, Copy)]
enum Shape {
    Loop,
    Scan,
    Burst,
    Shift,
}
#[derive(Clone, Copy)]
enum Policy {
    Lru,
    Lfu,
    Arc,
    Predator,
}
#[derive(Clone, Copy, Default)]
struct Slot {
    in_cache: bool,
    last: usize,
    hits: u64,
    utility: f64,
}
struct Out {
    checksum: u64,
    saved: u64,
    peak: usize,
    recovery: u64,
    amplitude: usize,
}
fn main() {
    println!("e65 — predator-prey cache control (simulation tier)");
    for sh in [Shape::Loop, Shape::Scan, Shape::Burst, Shape::Shift] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("LRU", Policy::Lru),
            ("LFU", Policy::Lfu),
            ("ARC-like", Policy::Arc),
            ("damped predator-prey", Policy::Predator),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<23} saved {:>9} peak {:>2}/{CAP} recovery {:>3} amplitude {:>2}",
                o.saved, o.peak, o.recovery, o.amplitude
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Loop => "tight loops",
        Shape::Scan => "one-pass scan",
        Shape::Burst => "reuse bursts",
        Shape::Shift => "phase change",
    }
}
fn trace(s: Shape) -> Vec<(usize, u64)> {
    let mut r = Lcg::new(0x6500_0065);
    (0..40_000)
        .map(|i| {
            let x = match s {
                Shape::Loop => r.below(20),
                Shape::Scan => (i % ITEMS) as u64,
                Shape::Burst => {
                    if i % 500 < 400 {
                        r.below(18)
                    } else {
                        30 + r.below(90)
                    }
                }
                Shape::Shift => {
                    if i < 20_000 {
                        r.below(24)
                    } else {
                        72 + r.below(24)
                    }
                }
            };
            (x as usize, r.next_u64())
        })
        .collect()
}
fn val(i: usize) -> u64 {
    20 + (i as u64 * 31 % 180)
}
fn run(t: &[(usize, u64)], p: Policy) -> Out {
    let mut s = [Slot::default(); ITEMS];
    let mut saved = 0;
    let mut peak = 0;
    let mut min_pop = CAP;
    let mut max_pop = 0;
    let mut recovery = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(x, token)) in t.iter().enumerate() {
        if i % 256 == 0 && matches!(p, Policy::Predator) {
            for q in &mut s {
                q.utility *= 0.78;
            }
        }
        if s[x].in_cache {
            saved += val(x);
            s[x].hits += 1;
            s[x].utility += val(x) as f64;
        } else {
            if s.iter().filter(|q| q.in_cache).count() >= CAP {
                let v = (0..ITEMS)
                    .filter(|&j| s[j].in_cache)
                    .min_by(|&a, &b| score(s[a], a, p, i).total_cmp(&score(s[b], b, p, i)))
                    .unwrap();
                s[v].in_cache = false;
            }
            s[x].in_cache = true;
            s[x].hits = 1;
            s[x].utility = val(x) as f64;
        }
        s[x].last = i;
        let pop = s.iter().filter(|q| q.in_cache).count();
        peak = peak.max(pop);
        if i >= CAP {
            min_pop = min_pop.min(pop);
            max_pop = max_pop.max(pop);
        }
        if i >= 20_000 && recovery == 0 && s[x].hits > 2 {
            recovery = (i - 20_000 + 1) as u64;
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        saved,
        peak,
        recovery,
        amplitude: max_pop - min_pop,
    }
}
fn score(s: Slot, item: usize, p: Policy, now: usize) -> f64 {
    match p {
        Policy::Lru => s.last as f64,
        Policy::Lfu => s.hits as f64,
        Policy::Arc => s.hits as f64 * 0.6 + 1.0 / (now - s.last + 1) as f64 * 100.0,
        Policy::Predator => s.utility + val(item) as f64 / (now - s.last + 1) as f64,
    }
}
