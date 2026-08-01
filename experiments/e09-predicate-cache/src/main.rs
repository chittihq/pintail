//! e09: Predicate/condition cache (Redshift SIGMOD 2024, ClickHouse 25.3)
//! on immutable granules: how much does the granule-bitmap hit path save,
//! and how does data clustering change the answer?
//!
//! 20M rows, 64K-row granules (306), predicate tenant_id = T (zipf-hot T).
//! Layouts: scattered (row order random) vs clustered (sorted by tenant).
//! Variants: full scan every query | minmax zone maps | predicate-cache warm hit.

use common::*;

const GRANULE: usize = 65_536;
const N_TENANTS: usize = 1_000;
const QUERIES: usize = 100;

struct Layout {
    tenant: Vec<u16>,
    amount: Vec<i64>,
}

fn run(label: &str, layout: &Layout, hot_tenants: &[u16]) {
    println!(
        "\n== layout: {label} — SUM(amount) WHERE tenant_id = T, {QUERIES} dashboard queries =="
    );
    let granules = layout.tenant.len().div_ceil(GRANULE);

    // zone maps (min/max tenant per granule)
    let zones: Vec<(u16, u16)> = layout
        .tenant
        .chunks(GRANULE)
        .map(|chunk| {
            let mut lo = u16::MAX;
            let mut hi = 0;
            for &t in chunk {
                lo = lo.min(t);
                hi = hi.max(t);
            }
            (lo, hi)
        })
        .collect();

    let scan_granule = |g: usize, t: u16| -> i64 {
        let start = g * GRANULE;
        let end = (start + GRANULE).min(layout.tenant.len());
        let mut sum = 0i64;
        for i in start..end {
            sum += ((layout.tenant[i] == t) as i64) * layout.amount[i];
        }
        sum
    };

    let mut rs = vec![];
    rs.push(bench("full scan per query", || {
        let mut total = 0i64;
        for &t in hot_tenants {
            for g in 0..granules {
                total += scan_granule(g, t);
            }
        }
        total as u64
    }));

    rs.push(bench("zone-map pruned scan per query", || {
        let mut total = 0i64;
        for &t in hot_tenants {
            for (g, &(lo, hi)) in zones.iter().enumerate() {
                if t >= lo && t <= hi {
                    total += scan_granule(g, t);
                }
            }
        }
        total as u64
    }));

    // predicate cache: bitmap of granules that matched, built on first touch
    let mut cache: Vec<Option<Vec<u64>>> = vec![None; N_TENANTS + 1];
    // cold pass builds every entry (measured separately, includes the scans)
    let cold = bench("predicate-cache COLD (build + scan)", || {
        for entry in cache.iter_mut() {
            *entry = None;
        }
        let mut total = 0i64;
        for &t in hot_tenants {
            let mut bitmap = vec![0u64; granules.div_ceil(64)];
            for g in 0..granules {
                let s = scan_granule(g, t);
                if s != 0 {
                    bitmap[g / 64] |= 1 << (g % 64);
                }
                total += s;
            }
            cache[t as usize] = Some(bitmap);
        }
        total as u64
    });
    rs.push(bench("predicate-cache WARM hit", || {
        let mut total = 0i64;
        for &t in hot_tenants {
            let bitmap = cache[t as usize].as_ref().unwrap();
            for g in 0..granules {
                if bitmap[g / 64] & (1 << (g % 64)) != 0 {
                    total += scan_granule(g, t);
                }
            }
        }
        total as u64
    }));
    rs.push(cold);
    check_consistency(&rs);
}

fn main() {
    println!("e09-predicate-cache  N = {N_ORDERS}, granule = {GRANULE}, tenants = {N_TENANTS}");
    let mut rng = Lcg::new(42);
    let zipf = Zipfish::new(N_TENANTS, 1.15);
    let tenants_scattered: Vec<u16> = (0..N_ORDERS)
        .map(|_| zipf.sample(&mut rng) as u16)
        .collect();
    let amount: Vec<i64> = (0..N_ORDERS)
        .map(|_| 100 + rng.below(999_900) as i64)
        .collect();

    let mut order: Vec<u32> = (0..N_ORDERS as u32).collect();
    order.sort_by_key(|&i| tenants_scattered[i as usize]);
    let tenants_clustered: Vec<u16> = order
        .iter()
        .map(|&i| tenants_scattered[i as usize])
        .collect();
    let amount_clustered: Vec<i64> = order.iter().map(|&i| amount[i as usize]).collect();

    // hot dashboard tenants: zipf-biased picks
    let mut qrng = Lcg::new(7);
    let hot: Vec<u16> = (0..QUERIES)
        .map(|_| zipf.sample(&mut qrng) as u16)
        .collect();

    run(
        "scattered (insert order)",
        &Layout {
            tenant: tenants_scattered,
            amount,
        },
        &hot,
    );
    run(
        "clustered (sorted by tenant)",
        &Layout {
            tenant: tenants_clustered,
            amount: amount_clustered,
        },
        &hot,
    );
}

/// zipf via cumulative weights (kept local: common::Lcg only)
struct Zipfish {
    cdf: Vec<f64>,
}

impl Zipfish {
    fn new(n: usize, s: f64) -> Self {
        let mut cdf = Vec::with_capacity(n);
        let mut sum = 0.0;
        for i in 0..n {
            sum += 1.0 / ((i + 1) as f64).powf(s);
            cdf.push(sum);
        }
        for v in cdf.iter_mut() {
            *v /= sum;
        }
        Self { cdf }
    }
    fn sample(&self, rng: &mut Lcg) -> usize {
        let u = (rng.below(1_000_000) as f64) / 1_000_000.0;
        self.cdf.partition_point(|&c| c < u)
    }
}
