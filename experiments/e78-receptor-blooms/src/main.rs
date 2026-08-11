//! e78: split one bit budget across feature-specific Bloom receptors.
use common::{Lcg, bench, check_consistency};
const KEYS: usize = 20_000;
const BITS: usize = 1 << 18;
#[derive(Clone, Copy)]
enum Policy {
    Pk,
    Partitioned,
    Ensemble,
}
struct Bloom {
    bits: Vec<u64>,
    seed: u64,
}
impl Bloom {
    fn new(seed: u64, bits: usize) -> Self {
        Self {
            bits: vec![0; bits / 64],
            seed,
        }
    }
    fn hash(&self, x: u64, k: u64) -> usize {
        (x.wrapping_mul(self.seed ^ k.wrapping_mul(0x9e37_79b9)) as usize) % (self.bits.len() * 64)
    }
    fn add(&mut self, x: u64) {
        for k in 0..3 {
            let h = self.hash(x, k);
            self.bits[h / 64] |= 1 << (h % 64)
        }
    }
    fn has(&self, x: u64) -> bool {
        (0..3).all(|k| {
            let h = self.hash(x, k);
            self.bits[h / 64] & (1 << (h % 64)) != 0
        })
    }
}
fn main() {
    println!("e78 — olfactory Bloom receptor ensemble (kernel tier)");
    let mut r = Lcg::new(0x7800_0078);
    let keys = (0..KEYS).map(|_| r.next_u64()).collect::<Vec<_>>();
    for p in [Policy::Pk, Policy::Partitioned, Policy::Ensemble] {
        let (fp, neg, fnn, build, _) = run(&keys, p);
        println!(
            "{:<18} false reads {:>6}/{neg} false negatives {fnn} build probes {build}",
            name(p),
            fp
        );
    }
    let rs = [
        bench("PK Bloom", || run(&keys, Policy::Pk).4),
        bench("partitioned", || run(&keys, Policy::Partitioned).4),
        bench("ensemble", || run(&keys, Policy::Ensemble).4),
    ];
    check_consistency(&rs);
}
fn name(p: Policy) -> &'static str {
    match p {
        Policy::Pk => "one PK Bloom",
        Policy::Partitioned => "partitioned",
        Policy::Ensemble => "receptor ensemble",
    }
}
fn run(keys: &[u64], p: Policy) -> (u64, u64, u64, u64, u64) {
    let parts = match p {
        Policy::Pk => 1,
        _ => 4,
    };
    let mut bs = (0..parts)
        .map(|i| Bloom::new(0xa5a5_1001 + i as u64, BITS / parts))
        .collect::<Vec<_>>();
    for &x in keys {
        let part = match p {
            Policy::Pk => 0,
            Policy::Partitioned => x as usize % parts,
            Policy::Ensemble => (x ^ (x >> 32)) as usize % parts,
        };
        bs[part].add(x)
    }
    let mut r = Lcg::new(0x7811);
    let mut fp = 0;
    let mut neg = 0;
    for _ in 0..100_000 {
        let x = r.next_u64();
        if !keys.contains(&x) {
            neg += 1;
            let part = match p {
                Policy::Pk => 0,
                Policy::Partitioned => x as usize % parts,
                Policy::Ensemble => (x ^ (x >> 32)) as usize % parts,
            };
            fp += u64::from(bs[part].has(x));
        }
    }
    let fnn = keys
        .iter()
        .filter(|&&x| {
            let part = match p {
                Policy::Pk => 0,
                Policy::Partitioned => x as usize % parts,
                Policy::Ensemble => (x ^ (x >> 32)) as usize % parts,
            };
            !bs[part].has(x)
        })
        .count() as u64;
    let checksum = keys.iter().fold(0xcbf2_9ce4_8422_2325, |h, x| {
        (h ^ *x).wrapping_mul(0x100_0000_01b3)
    });
    (fp, neg, fnn, keys.len() as u64 * 3, checksum)
}
