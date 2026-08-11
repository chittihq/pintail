//! e51: exchange immutable decoded blocks using source/sink value rather than recency.
use common::{Lcg, bench, check_consistency};

const BLOCKS: usize = 96;
const CAP: u64 = 96;
const REQUESTS: usize = 50_000;

#[derive(Clone, Copy)]
enum Policy {
    None,
    Lru,
    Frequency,
    Flow,
}
#[derive(Clone, Copy, Default)]
struct Slot {
    in_cache: bool,
    last: u64,
    hits: u64,
    demand: f64,
}
#[derive(Clone, Copy)]
struct Req {
    block: usize,
    projection: u64,
    token: u64,
}
struct Out {
    checksum: u64,
    decode: u64,
    p95: u64,
    peak: u64,
    recovery: u64,
}

fn main() {
    println!("e51 — mycelial decoded-block exchange (simulation tier)");
    for shifted in [false, true] {
        let t = trace(shifted);
        println!(
            "\n=== {} ===",
            if shifted {
                "phase-shifted dashboards"
            } else {
                "concurrent dashboards"
            }
        );
        let ps = [
            ("no sharing", Policy::None),
            ("LRU blocks", Policy::Lru),
            ("frequency blocks", Policy::Frequency),
            ("source/sink flow", Policy::Flow),
        ];
        let mut expected = None;
        for (n, p) in ps {
            let o = run(&t, p);
            expected.map_or_else(
                || expected = Some(o.checksum),
                |v| assert_eq!(v, o.checksum),
            );
            println!(
                "{n:<22} decode {:>10}  p95 {:>4}  peak {:>3}/{CAP}  recovery {:>3}",
                o.decode, o.p95, o.peak, o.recovery
            );
        }
        let rs = ps.map(|(n, p)| bench(n, || run(&t, p).checksum));
        check_consistency(&rs);
    }
}

fn trace(shifted: bool) -> Vec<Req> {
    let mut r = Lcg::new(0x5100_0051);
    (0..REQUESTS)
        .map(|i| {
            let phase = usize::from(shifted && i >= REQUESTS / 2);
            let dash = (r.below(6) as usize + phase * 3) % 6;
            let block = if r.below(100) < 92 {
                (dash * 12 + r.below(18) as usize) % 72
            } else {
                72 + r.below(24) as usize
            };
            Req {
                block,
                projection: 1 << r.below(4),
                token: r.next_u64(),
            }
        })
        .collect()
}
fn size(i: usize) -> u64 {
    1 + (i as u64 % 3)
}
fn cost(i: usize, p: u64) -> u64 {
    80 + (i as u64 * 19 % 240) + p.count_ones() as u64 * 30
}

fn run(t: &[Req], p: Policy) -> Out {
    let mut s = [Slot::default(); BLOCKS];
    let mut used = 0;
    let mut peak = 0;
    let mut decode = 0;
    let mut lats = Vec::with_capacity(t.len());
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    let mut recovery = 0;
    for (i, q) in t.iter().enumerate() {
        if i % 512 == 0 && matches!(p, Policy::Flow) {
            for x in &mut s {
                x.demand *= 0.75;
            }
        }
        let hit = !matches!(p, Policy::None) && s[q.block].in_cache;
        if hit {
            s[q.block].hits += 1;
            s[q.block].demand += cost(q.block, q.projection) as f64;
            lats.push(8);
        } else {
            let c = cost(q.block, q.projection);
            decode += c;
            lats.push(c);
            if !matches!(p, Policy::None) {
                let need = size(q.block);
                while used + need > CAP {
                    let v = (0..BLOCKS)
                        .filter(|&x| s[x].in_cache)
                        .min_by(|&a, &b| score(s[a], a, p).total_cmp(&score(s[b], b, p)))
                        .unwrap();
                    s[v].in_cache = false;
                    used -= size(v);
                }
                s[q.block].in_cache = true;
                s[q.block].hits = 1;
                s[q.block].demand = c as f64;
                used += need;
            }
        }
        s[q.block].last = i as u64;
        peak = peak.max(used);
        if i >= REQUESTS / 2 && recovery == 0 && hit {
            recovery = (i - REQUESTS / 2 + 1) as u64;
        }
        checksum = (checksum ^ q.token).wrapping_mul(0x100_0000_01b3);
    }
    lats.sort_unstable();
    Out {
        checksum,
        decode,
        p95: lats[lats.len() * 95 / 100],
        peak,
        recovery,
    }
}
fn score(s: Slot, i: usize, p: Policy) -> f64 {
    match p {
        Policy::None => 0.0,
        Policy::Lru => s.last as f64,
        Policy::Frequency => s.hits as f64,
        Policy::Flow => (s.demand + s.hits as f64 * cost(i, 1) as f64) / size(i) as f64,
    }
}
