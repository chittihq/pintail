//! Shared data generation and timing harness for the pintail experiment lab.
//!
//! Every experiment variant returns a u64 checksum; variants answering the same
//! question must produce identical checksums or the comparison is void.

use std::hint::black_box;
use std::time::Instant;

pub const N_ORDERS: usize = 20_000_000;
pub const N_USERS: u32 = 200_000;
pub const N_STATUS: usize = 5;
pub const N_REGIONS: usize = 8;

/// Deterministic PRNG (LCG + output mix). No external deps, stable across runs.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = self.0 ^ (self.0 >> 31);
        x.wrapping_mul(0xD6E8_FEB8_6659_FD93)
    }

    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Struct-of-arrays orders table mirroring the shape of benchmark/seed.sql.
/// `status` cycles every 5 rows exactly like the seed data — this is what
/// defeats zone-map pruning on Q2 and must be preserved in experiments.
pub struct Orders {
    pub user_id: Vec<u32>,
    pub status: Vec<u8>,
    pub region: Vec<u8>,
    pub amount: Vec<i64>, // cents, uniform in [100, 1_000_000)
    pub date: Vec<i32>,   // days, ~3 years
}

pub fn gen_orders(n: usize, seed: u64) -> Orders {
    let mut r = Lcg::new(seed);
    let mut o = Orders {
        user_id: Vec::with_capacity(n),
        status: Vec::with_capacity(n),
        region: Vec::with_capacity(n),
        amount: Vec::with_capacity(n),
        date: Vec::with_capacity(n),
    };
    for i in 0..n {
        o.user_id.push(r.below(N_USERS as u64) as u32);
        o.status.push((i % N_STATUS) as u8);
        o.region.push(r.below(N_REGIONS as u64) as u8);
        o.amount.push(100 + r.below(999_900) as i64);
        o.date.push(19_000 + r.below(1_095) as i32);
    }
    o
}

pub struct BenchResult {
    pub name: String,
    pub median_ms: f64,
    pub min_ms: f64,
    pub checksum: u64,
}

/// Run `f` with warmup, report median-of-7. `f` returns a checksum that is
/// black-boxed so the work cannot be optimized away.
pub fn bench<F: FnMut() -> u64>(name: &str, mut f: F) -> BenchResult {
    const WARMUP: usize = 2;
    const RUNS: usize = 7;
    let mut checksum = 0u64;
    for _ in 0..WARMUP {
        checksum = black_box(f());
    }
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        checksum = black_box(f());
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let res = BenchResult {
        name: name.to_string(),
        median_ms: times[RUNS / 2],
        min_ms: times[0],
        checksum,
    };
    println!(
        "{:<48} median {:>9.3} ms   min {:>9.3} ms   ck {:016x}",
        res.name, res.median_ms, res.min_ms, res.checksum
    );
    res
}

/// All variants of one question must agree. Loud failure if not.
pub fn check_consistency(results: &[BenchResult]) {
    if let Some(first) = results.first() {
        let mut ok = true;
        for r in results {
            if r.checksum != first.checksum {
                println!(
                    "!! CHECKSUM MISMATCH: {} = {:016x} vs {} = {:016x}",
                    r.name, r.checksum, first.name, first.checksum
                );
                ok = false;
            }
        }
        if !ok {
            std::process::exit(1);
        }
    }
}
