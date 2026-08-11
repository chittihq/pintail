//! e67: move segments through lifecycle policies using age plus measured heat.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Append,
    Recent,
    Dashboard,
    Archive,
}
#[derive(Clone, Copy)]
enum Policy {
    One,
    Age,
    Heat,
    Succession,
}
struct Out {
    checksum: u64,
    cpu: u64,
    bytes: u64,
    transitions: u64,
    needless: u64,
}
fn main() {
    println!("e67 — segment ecological succession (simulation tier)");
    for sh in [
        Shape::Append,
        Shape::Recent,
        Shape::Dashboard,
        Shape::Archive,
    ] {
        let t = trace(sh);
        println!("\n=== {} ===", name(sh));
        let ps = [
            ("one policy", Policy::One),
            ("age tiers", Policy::Age),
            ("heat tiers", Policy::Heat),
            ("succession", Policy::Succession),
        ];
        let mut e = None;
        for (n, p) in ps {
            let o = run(&t, p);
            e.map_or_else(|| e = Some(o.checksum), |v| assert_eq!(v, o.checksum));
            println!(
                "{n:<14} cpu {:>9} bytes {:>9} transitions {:>4} needless {}",
                o.cpu, o.bytes, o.transitions, o.needless
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Append => "append",
        Shape::Recent => "recent updates",
        Shape::Dashboard => "stable dashboard",
        Shape::Archive => "archive scans",
    }
}
fn trace(s: Shape) -> Vec<(bool, bool, u64)> {
    let mut r = Lcg::new(0x6700_0067);
    (0..30_000)
        .map(|i| {
            let write = match s {
                Shape::Append => true,
                Shape::Recent => i < 10_000 && r.below(100) < 60,
                Shape::Dashboard => r.below(100) < 3,
                Shape::Archive => false,
            };
            let read = !write || matches!(s, Shape::Dashboard | Shape::Archive);
            (write, read, r.next_u64())
        })
        .collect()
}
fn run(t: &[(bool, bool, u64)], p: Policy) -> Out {
    let mut state = 0u64;
    let mut heat = 0f64;
    let mut cpu = 0;
    let mut bytes = 0;
    let mut transitions = 0;
    let mut needless = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for (i, &(w, r, token)) in t.iter().enumerate() {
        heat = heat * 0.98 + u64::from(r) as f64;
        let wanted = match p {
            Policy::One => 0,
            Policy::Age => (i / 7500).min(3) as u64,
            Policy::Heat => {
                if heat > 30.0 {
                    2
                } else {
                    0
                }
            }
            Policy::Succession => {
                if w {
                    0
                } else if i < 4000 {
                    1
                } else if heat > 20.0 {
                    2
                } else {
                    3
                }
            }
        };
        if wanted != state {
            transitions += 1;
            cpu += 300;
            bytes += 200;
            needless += u64::from(i > 0 && transitions > 4);
            state = wanted
        }
        if w {
            cpu += [70, 90, 120, 160][state as usize];
            bytes += [100, 85, 70, 60][state as usize]
        }
        if r {
            cpu += [130, 100, 55, 85][state as usize];
            bytes += [100, 85, 65, 50][state as usize]
        }
        checksum = (checksum ^ token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        cpu,
        bytes,
        transitions,
        needless,
    }
}
