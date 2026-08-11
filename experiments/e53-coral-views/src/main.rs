//! e53: compose exact immutable-segment aggregates and rescan dirty overlap only.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Clean,
    TenPct,
    Changing,
    Schema,
}
#[derive(Clone, Copy)]
enum Policy {
    Scan,
    Sma,
    Eager,
    Coral,
}
struct Out {
    checksum: u64,
    work: u64,
    storage: u64,
    build: u64,
}
fn main() {
    println!("e53 — coral-accretion partial views (simulation tier)");
    for sh in [Shape::Clean, Shape::TenPct, Shape::Changing, Shape::Schema] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("full scan", Policy::Scan),
            ("fixed SMA", Policy::Sma),
            ("eager MV", Policy::Eager),
            ("lazy accretion", Policy::Coral),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<16} work {:>10} storage {}% build {:>8}",
                o.work, o.storage, o.build
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Clean => "clean repeats",
        Shape::TenPct => "10% dirty overlap",
        Shape::Changing => "changing templates",
        Shape::Schema => "schema generation",
    }
}
fn trace(s: Shape) -> Vec<(u64, u64, u64)> {
    let mut r = Lcg::new(0x5300_0053);
    (0..20_000)
        .map(|i| {
            let template = if matches!(s, Shape::Changing) {
                r.below(40)
            } else {
                r.below(4)
            };
            let dirty = match s {
                Shape::Clean => 0,
                Shape::TenPct => 10,
                Shape::Changing => r.below(30),
                Shape::Schema => u64::from(i % 4000 == 0) * 100,
            };
            (template, dirty, r.next_u64())
        })
        .collect()
}
fn run(t: &[(u64, u64, u64)], p: Policy) -> Out {
    let mut seen = [0u64; 40];
    let mut work = 0;
    let mut build = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for &(template, dirty, token) in t {
        let x = template as usize;
        seen[x] += 1;
        match p {
            Policy::Scan => work += 10_000,
            Policy::Sma => work += 4_000 + dirty * 60,
            Policy::Eager => {
                work += 100 + dirty * 100;
                build += 200
            }
            Policy::Coral => {
                if seen[x] < 3 {
                    work += 10_000
                } else {
                    work += 200 + dirty * 80;
                    if seen[x] == 3 {
                        build += 3_000
                    }
                }
            }
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    work += build;
    Out {
        checksum,
        work,
        storage: match p {
            Policy::Eager => 7,
            Policy::Coral => 4,
            _ => 0,
        },
        build,
    }
}
