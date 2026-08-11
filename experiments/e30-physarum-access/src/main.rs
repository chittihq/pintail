//! e30: decay projection co-access flow into bounded column bundles.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Stable,
    Shift,
    Adversarial,
}
#[derive(Clone, Copy)]
enum Policy {
    Canonical,
    Frequency,
    Flow,
}
struct Out {
    checksum: u64,
    bytes: u64,
    p95: u64,
    rewrite: u64,
    storage: u64,
    recovery: u64,
}
fn main() {
    println!("e30 — Physarum access network (simulation tier)");
    for sh in [Shape::Stable, Shape::Shift, Shape::Adversarial] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("canonical chunks", Policy::Canonical),
            ("frequency bundle", Policy::Frequency),
            ("flow + decay", Policy::Flow),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<20} bytes {:>9} p95 {:>3} rewrite {:>7} storage +{}% recovery {}",
                o.bytes, o.p95, o.rewrite, o.storage, o.recovery
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Stable => "stable projections",
        Shape::Shift => "phase shift",
        Shape::Adversarial => "adversarial projections",
    }
}
fn trace(s: Shape) -> Vec<(u16, u64)> {
    let mut r = Lcg::new(0x3000_0030);
    (0..30_000)
        .map(|i| {
            let group = match s {
                Shape::Stable => r.below(4),
                Shape::Shift => {
                    if i < 15_000 {
                        r.below(2)
                    } else {
                        2 + r.below(2)
                    }
                }
                Shape::Adversarial => r.below(12),
            } as u16;
            (
                (1 << (group % 12)) | (1 << ((group + 1) % 12)),
                r.next_u64(),
            )
        })
        .collect()
}
fn run(t: &[(u16, u64)], p: Policy) -> Out {
    let mut edges = [[0f64; 12]; 12];
    let mut bytes = 0;
    let mut costs = Vec::new();
    let mut rewrite = 0;
    let mut recovery = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(mask, token)) in t.iter().enumerate() {
        if i % 256 == 0 && matches!(p, Policy::Flow) {
            for row in &mut edges {
                for x in row {
                    *x *= 0.75
                }
            }
        }
        let cols = (0..12).filter(|c| mask & (1 << c) != 0).collect::<Vec<_>>();
        let bundled = match p {
            Policy::Canonical => false,
            Policy::Frequency => cols[0] < 4,
            Policy::Flow => edges[cols[0]][cols[1]] > 3.0,
        };
        let cost = if bundled { 110 } else { 200 };
        bytes += cost;
        costs.push(cost);
        if matches!(p, Policy::Flow) {
            edges[cols[0]][cols[1]] += 1.0;
            if i % 256 == 0 {
                rewrite += 80;
            }
            if i >= 15_000 && recovery == 0 && bundled {
                recovery = (i - 15_000 + 1) as u64;
            }
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    costs.sort_unstable();
    Out {
        checksum,
        bytes: bytes + rewrite,
        p95: costs[costs.len() * 95 / 100],
        rewrite,
        storage: u64::from(!matches!(p, Policy::Canonical)) * 7,
        recovery,
    }
}
