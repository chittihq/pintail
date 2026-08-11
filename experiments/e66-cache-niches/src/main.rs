//! e66: move cache-class borders by marginal saved work.
use common::{Lcg, bench, check_consistency};
const ITEMS: usize = 128;
const CAP: usize = 32;
#[derive(Clone, Copy)]
enum Shape {
    Mixed,
    EtL,
    AdHoc,
}
#[derive(Clone, Copy)]
enum Policy {
    Lru,
    Fixed,
    Global,
    Adaptive,
}
#[derive(Clone, Copy, Default)]
struct Slot {
    inside: bool,
    last: usize,
    hits: u64,
    value: u64,
}
struct Out {
    checksum: u64,
    saved: u64,
    empty_value: u64,
    book: u64,
}
fn main() {
    println!("e66 — ecological cache niches (simulation tier)");
    for sh in [Shape::Mixed, Shape::EtL, Shape::AdHoc] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("one LRU", Policy::Lru),
            ("fixed partitions", Policy::Fixed),
            ("global GreedyDual", Policy::Global),
            ("adaptive niches", Policy::Adaptive),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<21} saved {:>10} zero-value-space {:>3} book {:>7}",
                o.saved, o.empty_value, o.book
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Mixed => "dashboard + ad-hoc + ETL",
        Shape::EtL => "ETL dominated",
        Shape::AdHoc => "ad-hoc dominated",
    }
}
fn trace(sh: Shape) -> Vec<(usize, u64, u64)> {
    let mut r = Lcg::new(0x6600_0066);
    (0..50_000)
        .map(|_| {
            let class = match sh {
                Shape::Mixed => r.below(4),
                Shape::EtL => {
                    if r.below(100) < 70 {
                        0
                    } else {
                        r.below(4)
                    }
                }
                Shape::AdHoc => {
                    if r.below(100) < 70 {
                        3
                    } else {
                        r.below(4)
                    }
                }
            } as usize;
            let base = class * 32;
            let item = if class == 3 {
                base + r.below(32) as usize
            } else {
                base + r.below(8 + class as u64 * 5) as usize
            };
            let value = 20 + [2, 5, 9, 16][class] * 10;
            (item, value, r.next_u64())
        })
        .collect()
}
fn run(t: &[(usize, u64, u64)], p: Policy) -> Out {
    let mut s = [Slot::default(); ITEMS];
    let mut saved = 0;
    let mut book = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(x, v, token)) in t.iter().enumerate() {
        if s[x].inside {
            saved += v;
            s[x].hits += 1;
            s[x].value += v;
        } else {
            let class = x / 32;
            let class_count = s
                .iter()
                .enumerate()
                .filter(|(j, q)| q.inside && *j / 32 == class)
                .count();
            let quota = match p {
                Policy::Fixed => 8,
                Policy::Adaptive => {
                    let total: u64 = (0..4)
                        .map(|c| {
                            s.iter()
                                .enumerate()
                                .filter(|(j, q)| q.inside && *j / 32 == c)
                                .map(|(_, q)| q.value)
                                .sum::<u64>()
                        })
                        .sum();
                    let mine = s
                        .iter()
                        .enumerate()
                        .filter(|(j, q)| q.inside && *j / 32 == class)
                        .map(|(_, q)| q.value)
                        .sum::<u64>();
                    ((mine * CAP as u64 / total.max(1)) as usize).max(2)
                }
                _ => CAP,
            };
            if s.iter().filter(|q| q.inside).count() >= CAP || class_count >= quota {
                let candidates =
                    (0..ITEMS).filter(|&j| s[j].inside && (class_count < quota || j / 32 == class));
                let victim = candidates
                    .min_by_key(|&j| match p {
                        Policy::Lru => s[j].last as u64,
                        Policy::Fixed => s[j].last as u64,
                        Policy::Global => s[j].value,
                        Policy::Adaptive => s[j].value / (s[j].hits + 1),
                    })
                    .unwrap();
                s[victim].inside = false;
                book += 1;
            }
            s[x] = Slot {
                inside: true,
                last: i,
                hits: 1,
                value: v,
            };
        }
        s[x].last = i;
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    let empty_value = s.iter().filter(|q| q.inside && q.value == 0).count() as u64;
    Out {
        checksum,
        saved,
        empty_value,
        book,
    }
}
