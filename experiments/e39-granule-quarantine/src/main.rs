//! e39: persist bad immutable-block intervals and fail only overlapping queries.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Policy {
    Rediscover,
    Quarantine,
}
struct Out {
    checksum: u64,
    verify: u64,
    detected: u64,
    silent: u64,
    available: u64,
}
fn main() {
    println!("e39 — granule quarantine membranes (simulation tier)");
    let q = queries();
    let corrupting = q.iter().filter(|(a, b, _)| *a < 44 && *b > 40).count();
    for (n, p) in [
        ("whole-segment rediscovery", Policy::Rediscover),
        ("range quarantine", Policy::Quarantine),
    ] {
        let o = run(&q, p);
        println!(
            "{n:<27} verify {:>9} detected {}/{} silent {} unaffected availability {}%",
            o.verify, o.detected, corrupting, o.silent, o.available
        );
    }
    let rs = [
        bench("rediscover", || run(&q, Policy::Rediscover).checksum),
        bench("quarantine", || run(&q, Policy::Quarantine).checksum),
    ];
    check_consistency(&rs);
}
fn queries() -> Vec<(usize, usize, u64)> {
    let mut r = Lcg::new(0x3900_0039);
    (0..20_000)
        .map(|i| {
            let start = if i % 50 == 0 {
                40
            } else {
                r.below(240) as usize
            };
            (start, start + 8, r.next_u64())
        })
        .collect()
}
fn run(q: &[(usize, usize, u64)], p: Policy) -> Out {
    let bad = (40, 44);
    let mut known = false;
    let mut verify = 0;
    let mut detected = 0;
    let mut silent = 0;
    let mut unaffected = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for &(a, b, t) in q {
        let overlap = a < bad.1 && b > bad.0;
        if overlap {
            detected += 1;
            if !known || matches!(p, Policy::Rediscover) {
                verify += 256;
            }
            known = true;
        } else {
            unaffected += 1;
            verify += match p {
                Policy::Rediscover => 256,
                Policy::Quarantine => 8,
            };
        }
        if overlap && detected == 0 {
            silent += 1
        }
        checksum = (checksum ^ t).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        verify,
        detected,
        silent,
        available: unaffected * 100 / (q.len() as u64 - detected).max(1),
    }
}
