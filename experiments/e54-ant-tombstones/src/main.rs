//! e54: decay spatial tombstone heat and compact connected hot intervals.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Recent,
    Moving,
    Scattered,
    Append,
}
#[derive(Clone, Copy)]
enum Policy {
    SizeTier,
    Overlap,
    Heat,
    Evaporating,
}
struct Out {
    checksum: u64,
    read: u64,
    write: u64,
}
fn main() {
    println!("e54 — ant-trail tombstone compaction (simulation tier)");
    for sh in [
        Shape::Recent,
        Shape::Moving,
        Shape::Scattered,
        Shape::Append,
    ] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("size tier", Policy::SizeTier),
            ("overlap count", Policy::Overlap),
            ("non-decaying heat", Policy::Heat),
            ("evaporating trails", Policy::Evaporating),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!("{n:<21} read {:>10} write {:>9}", o.read, o.write);
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Recent => "recent hot",
        Shape::Moving => "moving hot",
        Shape::Scattered => "scattered",
        Shape::Append => "append only",
    }
}
fn trace(s: Shape) -> Vec<(usize, bool, u64)> {
    let mut r = Lcg::new(0x5400_0054);
    (0..40_000)
        .map(|i| {
            let x = match s {
                Shape::Recent => r.below(16),
                Shape::Moving => (i / 200 % 8) * 16 + r.below(16),
                Shape::Scattered => r.below(128),
                Shape::Append => i % 128,
            };
            (x as usize, !matches!(s, Shape::Append), r.next_u64())
        })
        .collect()
}
fn run(t: &[(usize, bool, u64)], p: Policy) -> Out {
    let mut versions = [1u64; 128];
    let mut heat = [0f64; 128];
    let mut read = 0;
    let mut write = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(x, update, token)) in t.iter().enumerate() {
        if i % 128 == 0 && matches!(p, Policy::Evaporating) {
            for h in &mut heat {
                *h *= 0.6
            }
        }
        if update {
            versions[x] += 1;
            heat[x] += 1.0
        }
        read += versions[x] * 100;
        if i % 100 == 0 {
            let target = match p {
                Policy::SizeTier => i / 100 % 128,
                Policy::Overlap => (0..128).max_by_key(|j| versions[*j]).unwrap(),
                Policy::Heat | Policy::Evaporating => (0..128)
                    .max_by(|a, b| heat[*a].total_cmp(&heat[*b]))
                    .unwrap(),
            };
            if versions[target] > 1 {
                write += versions[target] * 100;
                versions[target] = 1;
                heat[target] = 0.0
            }
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        read,
        write,
    }
}
