//! e41: prune cached subplans by decayed saved-work and dependency reinforcement.
use common::{Lcg, bench, check_consistency};

const ITEMS: usize = 64;
const CAP: u64 = 120;
const REQUESTS: usize = 40_000;

#[derive(Clone, Copy)]
enum Policy {
    Lru,
    Lfu,
    Greedy,
    Synaptic,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    resident: bool,
    last: u64,
    hits: u64,
    strength: f64,
}

#[derive(Clone, Copy)]
struct Request {
    item: usize,
    token: u64,
}

struct Out {
    checksum: u64,
    saved: u64,
    peak: u64,
    maintenance: u64,
}

fn main() {
    println!("e41 — synaptic plan-cache pruning (simulation tier)");
    for shifted in [false, true] {
        let trace = trace(shifted);
        println!(
            "\n=== {} ===",
            if shifted {
                "phase shift"
            } else {
                "overlapping dashboards"
            }
        );
        let policies = [
            ("entry LRU", Policy::Lru),
            ("LFU", Policy::Lfu),
            ("GreedyDual-size", Policy::Greedy),
            ("graph reinforcement", Policy::Synaptic),
        ];
        let mut expected = None;
        for (name, policy) in policies {
            let o = run(&trace, policy);
            expected.map_or_else(
                || expected = Some(o.checksum),
                |v| assert_eq!(v, o.checksum),
            );
            println!(
                "{name:<24} saved {:>10}  peak {:>3}/{CAP}  maintenance {:>7}",
                o.saved, o.peak, o.maintenance
            );
        }
        let rs = policies.map(|(n, p)| bench(n, || run(&trace, p).checksum));
        check_consistency(&rs);
    }
}

fn trace(shifted: bool) -> Vec<Request> {
    let mut r = Lcg::new(0x4100_0041);
    (0..REQUESTS)
        .map(|i| {
            let phase = usize::from(shifted && i >= REQUESTS / 2);
            let dashboard = (r.below(8) as usize + phase * 4) % 8;
            let item = if r.below(100) < 88 {
                dashboard * 6 + r.below(6) as usize
            } else {
                48 + r.below(16) as usize
            };
            Request {
                item,
                token: r.next_u64(),
            }
        })
        .collect()
}

fn size(i: usize) -> u64 {
    2 + (i as u64 * 7 % 5)
}
fn value(i: usize) -> u64 {
    40 + (i as u64 * 37 % 240)
}

fn run(trace: &[Request], policy: Policy) -> Out {
    let mut slots = [Slot::default(); ITEMS];
    let mut used = 0;
    let mut peak = 0;
    let mut saved = 0;
    let mut maintenance = 0;
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    let mut previous = 0;
    for (tick, request) in trace.iter().enumerate() {
        if tick > 0 && matches!(policy, Policy::Synaptic) && tick % 512 == 0 {
            for slot in &mut slots {
                slot.strength *= 0.82;
            }
            maintenance += ITEMS as u64;
        }
        if slots[request.item].resident {
            saved += value(request.item);
            slots[request.item].hits += 1;
            if matches!(policy, Policy::Synaptic) && previous / 6 == request.item / 6 {
                slots[request.item].strength += value(request.item) as f64;
                slots[previous].strength += value(previous) as f64 / 2.0;
            }
        } else {
            let need = size(request.item);
            while used + need > CAP {
                let victim = (0..ITEMS)
                    .filter(|&i| slots[i].resident)
                    .min_by(|&a, &b| {
                        score(slots[a], a, policy).total_cmp(&score(slots[b], b, policy))
                    })
                    .expect("resident victim");
                slots[victim].resident = false;
                used -= size(victim);
                maintenance += 1;
            }
            slots[request.item].resident = true;
            slots[request.item].hits = 1;
            slots[request.item].strength = value(request.item) as f64;
            used += need;
        }
        slots[request.item].last = tick as u64;
        previous = request.item;
        peak = peak.max(used);
        checksum = (checksum ^ request.token).wrapping_mul(0x100_0000_01b3);
    }
    Out {
        checksum,
        saved,
        peak,
        maintenance,
    }
}

fn score(s: Slot, item: usize, p: Policy) -> f64 {
    match p {
        Policy::Lru => s.last as f64,
        Policy::Lfu => s.hits as f64,
        Policy::Greedy => s.hits as f64 * value(item) as f64 / size(item) as f64,
        Policy::Synaptic => (s.hits as f64 * value(item) as f64 + s.strength) / size(item) as f64,
    }
}
