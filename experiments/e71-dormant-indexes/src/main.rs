//! e71: retain compact index seeds and require several signals before germination.
use common::{Lcg, bench, check_consistency};

#[derive(Clone, Copy)]
enum Shape {
    Seasonal,
    OneOff,
    FalseStarts,
}
#[derive(Clone, Copy)]
enum Policy {
    Hot,
    Drop,
    Dormant,
    Germinate,
}
struct Out {
    checksum: u64,
    memory: u64,
    rebuild: u64,
    p95: u64,
    false_rate: u64,
}
fn main() {
    println!("e71 — dormant auxiliary indexes (simulation tier)");
    for sh in [Shape::Seasonal, Shape::OneOff, Shape::FalseStarts] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("retain hot", Policy::Hot),
            ("drop/rebuild", Policy::Drop),
            ("compressed dormant", Policy::Dormant),
            ("multi-cue germination", Policy::Germinate),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<24} memory-area {:>10} rebuild {:>8} p95 {:>4} false {:>3}%",
                o.memory, o.rebuild, o.p95, o.false_rate
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Seasonal => "recurring season",
        Shape::OneOff => "one-off predicates",
        Shape::FalseStarts => "false starts",
    }
}
fn trace(s: Shape) -> Vec<(usize, bool, u64)> {
    let mut r = Lcg::new(0x7100_0071);
    (0..20_000)
        .map(|i| {
            let idx = match s {
                Shape::Seasonal => (i / 800) % 12,
                Shape::OneOff => r.below(80) as usize,
                Shape::FalseStarts => (i / 37 + r.below(4) as usize) % 24,
            };
            let stable = matches!(s, Shape::Seasonal) && i % 800 > 80;
            (idx, stable, r.next_u64())
        })
        .collect()
}
fn run(t: &[(usize, bool, u64)], p: Policy) -> Out {
    let n = 80;
    let mut hot = vec![false; n];
    let mut cues = vec![0u8; n];
    let mut last = vec![0usize; n];
    let mut memory = 0;
    let mut rebuild = 0;
    let mut lats = Vec::new();
    let mut germ = 0;
    let mut false_germ = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(x, stable, token)) in t.iter().enumerate() {
        if i % 200 == 0 {
            for j in 0..n {
                if hot[j] && i - last[j] > 600 && !matches!(p, Policy::Hot) {
                    hot[j] = false;
                }
            }
        }
        let repeated = i - last[x] < 120;
        cues[x] = if repeated {
            cues[x].saturating_add(1)
        } else {
            1
        };
        last[x] = i;
        let awaken = match p {
            Policy::Hot => true,
            Policy::Drop => false,
            Policy::Dormant => repeated,
            Policy::Germinate => cues[x] >= 3 && stable,
        };
        if awaken && !hot[x] {
            hot[x] = true;
            rebuild += 300;
            germ += 1;
            if !stable {
                false_germ += 1;
            }
        }
        let lat = if hot[x] {
            12
        } else {
            if matches!(p, Policy::Drop) {
                rebuild += 300;
            }
            310
        };
        lats.push(lat);
        let hot_count = hot.iter().filter(|x| **x).count() as u64;
        memory +=
            hot_count * 100 + (n as u64 - hot_count) * u64::from(!matches!(p, Policy::Drop)) * 8;
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    lats.sort_unstable();
    Out {
        checksum,
        memory,
        rebuild,
        p95: lats[lats.len() * 95 / 100],
        false_rate: false_germ * 100 / germ.max(1),
    }
}
