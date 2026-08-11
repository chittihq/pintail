//! e73: donate immutable intermediates only when measured net reuse is positive.
use common::{Lcg, bench, check_consistency};
const FAMILIES: usize = 40;
const CAP: usize = 12;
#[derive(Clone, Copy)]
enum Shape {
    Related,
    OneOff,
    Versioned,
}
#[derive(Clone, Copy)]
enum Policy {
    Results,
    Exact,
    Donate,
    Symbiosis,
}
#[derive(Clone, Copy, Default)]
struct Slot {
    inside: bool,
    version: u64,
    last: usize,
    hits: u64,
    net: i64,
}
struct Out {
    checksum: u64,
    work: u64,
    saved: u64,
    producer: u64,
    peak: usize,
    invalidated: u64,
}
fn main() {
    println!("e73 — host-microbiome intermediate exchange (simulation tier)");
    for sh in [Shape::Related, Shape::OneOff, Shape::Versioned] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("result cache", Policy::Results),
            ("exact intermediate", Policy::Exact),
            ("unconditional donation", Policy::Donate),
            ("symbiotic admission", Policy::Symbiosis),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<23} work {:>9} saved {:>9} producer {:>7} intermediates {:>2} invalidated {:>3}",
                o.work, o.saved, o.producer, o.peak, o.invalidated
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Related => "related dashboards",
        Shape::OneOff => "adversarial one-offs",
        Shape::Versioned => "CDC version changes",
    }
}
fn trace(sh: Shape) -> Vec<(usize, usize, u64, u64)> {
    let mut r = Lcg::new(0x7300_0073);
    let mut version = 0;
    (0..40_000)
        .map(|i| {
            if matches!(sh, Shape::Versioned) && i % 2500 == 0 {
                version += 1
            }
            let family = match sh {
                Shape::Related => r.below(10),
                Shape::OneOff => r.below(40),
                Shape::Versioned => r.below(14),
            } as usize;
            let variant = r.below(5) as usize;
            (family, variant, version, r.next_u64())
        })
        .collect()
}
fn run(t: &[(usize, usize, u64, u64)], p: Policy) -> Out {
    let mut s = [Slot::default(); FAMILIES];
    let mut results = std::collections::VecDeque::new();
    let mut work = 0;
    let mut saved = 0;
    let mut producer = 0;
    let mut invalidated = 0;
    let mut peak = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    let result_cap = if matches!(p, Policy::Results) {
        CAP
    } else {
        CAP / 2
    };
    let intermediate_cap = CAP - result_cap;
    for (i, &(f, v, version, token)) in t.iter().enumerate() {
        let full = 500 + (f as u64 * 17 % 200);
        let key = (f, v, version);
        if let Some(position) = results.iter().position(|candidate| *candidate == key) {
            saved += full;
            let hit = results.remove(position).expect("result position");
            results.push_back(hit);
        } else {
            let reusable = !matches!(p, Policy::Results) && s[f].inside && s[f].version == version;
            if reusable {
                work += 120;
                saved += full - 120;
                s[f].hits += 1;
                s[f].net += full as i64 - 120;
            } else {
                if s[f].inside && s[f].version != version {
                    invalidated += 1;
                    s[f].inside = false;
                }
                work += full;
                let donate = match p {
                    Policy::Donate => true,
                    Policy::Exact => v == 0,
                    Policy::Symbiosis => f < 16 && i > 0,
                    Policy::Results => false,
                };
                if donate {
                    producer += 12;
                    let admit = !matches!(p, Policy::Symbiosis) || full > 200;
                    if admit {
                        if s.iter().filter(|q| q.inside).count() >= intermediate_cap {
                            let victim = (0..FAMILIES)
                                .filter(|&j| s[j].inside)
                                .min_by_key(|&j| {
                                    if matches!(p, Policy::Symbiosis) {
                                        s[j].net
                                    } else {
                                        s[j].last as i64
                                    }
                                })
                                .unwrap();
                            s[victim].inside = false;
                        }
                        s[f] = Slot {
                            inside: true,
                            version,
                            last: i,
                            hits: 0,
                            net: full as i64 - 12,
                        };
                    }
                }
            }
            if results.len() == result_cap {
                results.pop_front();
            }
            results.push_back(key);
        }
        s[f].last = i;
        peak = peak.max(s.iter().filter(|q| q.inside).count());
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        work,
        saved,
        producer,
        peak,
        invalidated,
    }
}
