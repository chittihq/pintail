//! e74: controlled partial cache reset after sustained low productivity and shift.
use common::{Lcg, bench, check_consistency};
const ITEMS: usize = 160;
const CAP: usize = 32;
#[derive(Clone, Copy)]
enum Shape {
    Abrupt,
    Gradual,
    FalseAlarm,
}
#[derive(Clone, Copy)]
enum Policy {
    Lru,
    Ttl,
    Flush,
    Fire,
}
#[derive(Clone, Copy, Default)]
struct Slot {
    inside: bool,
    last: usize,
    hits: u64,
    pinned: bool,
}
struct Out {
    checksum: u64,
    miss: u64,
    recovery: u64,
    resets: u64,
    pinned_evicted: u64,
}
fn main() {
    println!("e74 — fire-ecology cache reset (simulation tier)");
    for sh in [Shape::Abrupt, Shape::Gradual, Shape::FalseAlarm] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("LRU", Policy::Lru),
            ("TTL", Policy::Ttl),
            ("full flush", Policy::Flush),
            ("partial reset + refuges", Policy::Fire),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<24} miss-cost {:>9} recovery {:>4} resets {:>3} pinned-evicted {}",
                o.miss, o.recovery, o.resets, o.pinned_evicted
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Abrupt => "abrupt phase shift",
        Shape::Gradual => "gradual drift",
        Shape::FalseAlarm => "false alarms",
    }
}
fn trace(sh: Shape) -> Vec<(usize, u64)> {
    let mut r = Lcg::new(0x7400_0074);
    (0..50_000)
        .map(|i| {
            let base = match sh {
                Shape::Abrupt => {
                    if i < 25_000 {
                        0
                    } else {
                        80
                    }
                }
                Shape::Gradual => (i / 2500 * 4).min(80),
                Shape::FalseAlarm => 0,
            };
            let item = if matches!(sh, Shape::FalseAlarm) && i % 3000 < 30 {
                80 + r.below(32) as usize
            } else {
                base + r.below(32) as usize
            };
            (item, r.next_u64())
        })
        .collect()
}
fn run(t: &[(usize, u64)], p: Policy) -> Out {
    let mut s = [Slot::default(); ITEMS];
    for q in s.iter_mut().take(4) {
        q.pinned = true;
        q.inside = true;
    }
    let mut miss = 0;
    let mut recent = std::collections::VecDeque::new();
    let mut resets = 0;
    let mut pinned_evicted = 0;
    let mut recovery = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(x, token)) in t.iter().enumerate() {
        if i == 25_000 {
            recent.clear();
        }
        let hit = s[x].inside;
        if !hit {
            miss += 100;
            if i >= 25_000
                && recovery == 0
                && recent.len() == 256
                && recent.iter().filter(|v| **v).count() > 230
            {
                recovery = (i - 25_000 + 1) as u64;
            }
            if s.iter().filter(|q| q.inside).count() >= CAP {
                let victim = (0..ITEMS)
                    .filter(|&j| s[j].inside && !s[j].pinned)
                    .min_by_key(|&j| s[j].last)
                    .unwrap();
                s[victim].inside = false;
            }
            s[x].inside = true;
            s[x].pinned = x < 4;
        } else {
            s[x].hits += 1;
        }
        s[x].last = i;
        recent.push_back(hit);
        if recent.len() > 256 {
            recent.pop_front();
        }
        let low = recent.len() == 256 && recent.iter().filter(|v| **v).count() < 80;
        if low && i % 256 == 0 {
            match p {
                Policy::Flush => {
                    for q in &mut s {
                        if q.inside {
                            pinned_evicted += u64::from(q.pinned);
                            q.inside = false;
                        }
                    }
                    resets += 1
                }
                Policy::Fire => {
                    let mut victims = (0..ITEMS)
                        .filter(|&j| s[j].inside && !s[j].pinned)
                        .collect::<Vec<_>>();
                    victims.sort_by_key(|&j| s[j].hits);
                    for j in victims.into_iter().take(CAP / 2) {
                        s[j].inside = false;
                    }
                    resets += 1
                }
                _ => {}
            }
        }
        if matches!(p, Policy::Ttl) && i % 512 == 0 {
            for q in &mut s {
                if q.inside && !q.pinned && i - q.last > 1500 {
                    q.inside = false;
                }
            }
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        miss,
        recovery,
        resets,
        pinned_evicted,
    }
}
