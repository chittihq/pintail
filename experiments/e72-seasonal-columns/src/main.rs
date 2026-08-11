//! e72: migrate decoded column mirrors only when periodic savings repay the move.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Periodic,
    Drift,
    Random,
    Wide,
}
#[derive(Clone, Copy)]
enum Policy {
    Compressed,
    Decoded,
    Recency,
    Seasonal,
}
struct Out {
    checksum: u64,
    latency: u64,
    migrations: u64,
    peak: u64,
}
fn main() {
    println!("e72 — seasonal decoded-column migration (simulation tier)");
    for sh in [Shape::Periodic, Shape::Drift, Shape::Random, Shape::Wide] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("always compressed", Policy::Compressed),
            ("always decoded", Policy::Decoded),
            ("recency", Policy::Recency),
            ("seasonal migration", Policy::Seasonal),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<22} latency {:>10} migrations {:>5} peak {:>2}/8",
                o.latency, o.migrations, o.peak
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Periodic => "periodic dashboards",
        Shape::Drift => "drifting period",
        Shape::Random => "random access",
        Shape::Wide => "wide-column control",
    }
}
fn trace(s: Shape) -> Vec<(u16, u64)> {
    let mut r = Lcg::new(0x7200_0072);
    (0..30_000)
        .map(|i| {
            let mask = match s {
                Shape::Periodic => 1 << ((i / 600) % 24),
                Shape::Drift => 1 << ((i / (400 + i / 4000 * 100)) % 24),
                Shape::Random => 1 << r.below(24),
                Shape::Wide => (1 << 12) - 1,
            };
            (mask, r.next_u64())
        })
        .collect()
}
fn run(t: &[(u16, u64)], p: Policy) -> Out {
    let mut hot = [false; 24];
    let mut last = [usize::MAX; 24];
    let mut period = [0usize; 24];
    let mut latency = 0;
    let mut migrations = 0;
    let mut peak = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(mask, token)) in t.iter().enumerate() {
        for c in 0..24 {
            if mask & (1 << c) == 0 {
                continue;
            }
            let gap = if last[c] == usize::MAX {
                usize::MAX
            } else {
                i - last[c]
            };
            let predict = period[c] > 0 && gap.abs_diff(period[c]) < period[c] / 4 + 1;
            let want = match p {
                Policy::Compressed => false,
                Policy::Decoded => c < 8,
                Policy::Recency => gap < 80,
                Policy::Seasonal => predict && period[c] * 4 > 180,
            };
            if want && !hot[c] {
                if hot.iter().filter(|x| **x).count() >= 8 {
                    let v = (0..24)
                        .filter(|&x| hot[x])
                        .max_by_key(|&x| i.saturating_sub(last[x]))
                        .unwrap();
                    hot[v] = false;
                }
                hot[c] = true;
                migrations += 1;
                latency += 180;
            }
            latency += if hot[c] { 5 } else { 28 };
            if last[c] != usize::MAX {
                period[c] = if period[c] == 0 {
                    gap
                } else {
                    (period[c] * 3 + gap) / 4
                };
            }
            last[c] = i;
        }
        peak = peak.max(hot.iter().filter(|x| **x).count() as u64);
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        latency,
        migrations,
        peak,
    }
}
