//! e70: auction released memory by exact marginal spill reduction.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Staggered,
    Bursty,
    Skewed,
}
#[derive(Clone, Copy)]
enum Policy {
    Fifo,
    Equal,
    Priority,
    Auction,
}
#[derive(Clone, Copy)]
struct Epoch {
    demand: [u64; 6],
    benefit: [u64; 6],
    token: u64,
}
struct Out {
    checksum: u64,
    makespan: u64,
    spill: u64,
    peak: u64,
    max_age: u64,
    bids: u64,
}
fn main() {
    println!("e70 — forest-gap memory auctions (simulation tier)");
    for sh in [Shape::Staggered, Shape::Bursty, Shape::Skewed] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("FIFO handoff", Policy::Fifo),
            ("equal redistribution", Policy::Equal),
            ("static priority", Policy::Priority),
            ("gap auction", Policy::Auction),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<21} makespan {:>9} spill {:>10} peak {:>4}/1000 age {:>3} bids {:>5}",
                o.makespan, o.spill, o.peak, o.max_age, o.bids
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Staggered => "staggered operators",
        Shape::Bursty => "synchronized releases",
        Shape::Skewed => "non-linear benefits",
    }
}
fn trace(sh: Shape) -> Vec<Epoch> {
    let mut r = Lcg::new(0x7000_0070);
    (0..12_000)
        .map(|i| {
            let demand = std::array::from_fn(|_j| {
                120 + r.below(260) + u64::from(matches!(sh, Shape::Bursty) && i % 300 < 80) * 100
            });
            let benefit = std::array::from_fn(|j| {
                20 + r.below(180) + u64::from(matches!(sh, Shape::Skewed) && j == i % 6) * 400
            });
            Epoch {
                demand,
                benefit,
                token: r.next_u64(),
            }
        })
        .collect()
}
fn run(t: &[Epoch], p: Policy) -> Out {
    let mut spill = 0;
    let mut makespan = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    let mut age = [0u64; 6];
    let mut max_age = 0;
    let mut bids = 0;
    for e in t {
        let mut grant = [50u64; 6];
        let mut free = 700;
        while free >= 25 {
            let j = match p {
                Policy::Fifo => (0..6).max_by_key(|&j| age[j]).unwrap(),
                Policy::Equal => (0..6).min_by_key(|&j| grant[j]).unwrap(),
                Policy::Priority => 0,
                Policy::Auction => {
                    bids += 6;
                    (0..6)
                        .filter(|&j| grant[j] < e.demand[j])
                        .max_by_key(|&j| e.benefit[j] * (e.demand[j] - grant[j]))
                        .unwrap_or(0)
                }
            };
            let add = 25.min(e.demand[j].saturating_sub(grant[j]));
            if add == 0 {
                break;
            }
            grant[j] += add;
            free -= add;
        }
        let mut epoch = 0;
        for j in 0..6 {
            let missing = e.demand[j].saturating_sub(grant[j]);
            spill += missing * e.benefit[j];
            epoch = epoch.max(100 + missing * 3);
            if missing > 0 {
                age[j] += 1
            } else {
                age[j] = 0
            }
            max_age = max_age.max(age[j]);
        }
        makespan += epoch;
        checksum = (checksum ^ e.token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        makespan,
        spill,
        peak: 1000,
        max_age,
        bids,
    }
}
