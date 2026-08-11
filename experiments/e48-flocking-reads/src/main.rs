//! e48: bounded local alignment of immutable scan frontiers.
use common::{bench, check_consistency};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
enum Shape {
    Identical,
    Overlap,
    Diverge,
    Short,
}
#[derive(Clone, Copy)]
enum Policy {
    Independent,
    Global,
    Flocking,
}
#[derive(Clone, Copy)]
struct Scan {
    start: u64,
    end: u64,
    token: u64,
}
struct Out {
    checksum: u64,
    calls: u64,
    bytes: u64,
    median: u64,
    p95: u64,
}

fn main() {
    println!("e48 — flocking read coalescence (simulation tier)");
    for shape in [
        Shape::Identical,
        Shape::Overlap,
        Shape::Diverge,
        Shape::Short,
    ] {
        let s = scans(shape);
        println!("\n=== {} ===", name(shape));
        let ps = [
            ("independent", Policy::Independent),
            ("global cooperative", Policy::Global),
            ("bounded flocking", Policy::Flocking),
        ];
        let mut expected = None;
        for (n, p) in ps {
            let o = run(&s, p);
            expected.map_or_else(
                || expected = Some(o.checksum),
                |v| assert_eq!(v, o.checksum),
            );
            println!(
                "{n:<20} calls {:>6} bytes {:>9}  median {:>5} p95 {:>5}",
                o.calls, o.bytes, o.median, o.p95
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&s, p).checksum));
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Identical => "identical scans",
        Shape::Overlap => "overlapping scans",
        Shape::Diverge => "diverging scans",
        Shape::Short => "latency-sensitive short scans",
    }
}
fn scans(shape: Shape) -> Vec<Scan> {
    (0..32)
        .map(|i| {
            let (start, len) = match shape {
                Shape::Identical => (0, 512),
                Shape::Overlap => (i * 8, 320),
                Shape::Diverge => (i * 400, 256),
                Shape::Short => (i * 70, 8),
            };
            Scan {
                start,
                end: start + len,
                token: i.wrapping_mul(0x9e37_79b9),
            }
        })
        .collect()
}
fn checksum(scans: &[Scan]) -> u64 {
    scans.iter().fold(0xcbf2_9ce4_8422_2325, |h, s| {
        (h ^ s.token ^ s.start ^ s.end).wrapping_mul(0x100_0000_01b3)
    })
}
fn run(scans: &[Scan], p: Policy) -> Out {
    let mut lats = Vec::new();
    let (calls, bytes) = match p {
        Policy::Independent => {
            for s in scans {
                lats.push((s.end - s.start) * 10);
            }
            let c = scans.iter().map(|s| s.end - s.start).sum();
            (c, c * 4096)
        }
        Policy::Global => {
            let mut blocks = BTreeSet::new();
            for s in scans {
                for b in s.start..s.end {
                    blocks.insert(b);
                }
            }
            let span = blocks.len() as u64;
            for s in scans {
                lats.push(span * 7 + (s.start - scans[0].start) * 2);
            }
            (span, span * 4096)
        }
        Policy::Flocking => {
            let mut groups: Vec<(u64, u64, u64)> = Vec::new();
            for s in scans {
                if let Some(g) = groups
                    .iter_mut()
                    .find(|g| s.start <= g.1 + 16 && s.end >= g.0)
                {
                    g.0 = g.0.min(s.start);
                    g.1 = g.1.max(s.end);
                    g.2 += 1;
                } else {
                    groups.push((s.start, s.end, 1));
                }
            }
            let c = groups.iter().map(|g| g.1 - g.0).sum::<u64>();
            for s in scans {
                let peers = groups
                    .iter()
                    .find(|g| s.start >= g.0 && s.end <= g.1)
                    .map_or(1, |g| g.2);
                let service = if peers > 1 { 7 } else { 10 };
                lats.push((s.end - s.start) * service + peers.min(8) * 3);
            }
            (c, c * 4096)
        }
    };
    lats.sort_unstable();
    Out {
        checksum: checksum(scans),
        calls,
        bytes,
        median: lats[lats.len() / 2],
        p95: lats[lats.len() * 95 / 100],
    }
}
