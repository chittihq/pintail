//! e10: Morsel-size sweep × core-scaling curve for a fused scan+filter+agg
//! pipeline. Answers: what morsel size should pintail's scheduler use, and how
//! close to linear does rayon work-stealing get on this machine?

use common::*;
use rayon::prelude::*;

fn main() {
    println!("e10-parallel-scan  N = {N_ORDERS} (SUM WHERE amount > median)");
    let o = gen_orders(N_ORDERS, 42);
    let amount = &o.amount;
    let t = 500_050i64;

    let mut baseline = 0.0;
    for threads in [1usize, 2, 4, 8, 10] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        for morsel in [4_096usize, 65_536, 1 << 20] {
            let name = format!("threads={threads:>2} morsel={morsel:>8}");
            let result = pool.install(|| {
                bench(&name, || {
                    amount
                        .par_chunks(morsel)
                        .map(|chunk| {
                            let mut s = 0i64;
                            for &v in chunk {
                                s += ((v > t) as i64) * v;
                            }
                            s
                        })
                        .sum::<i64>() as u64
                })
            });
            if threads == 1 && morsel == 1 << 20 {
                baseline = result.median_ms;
            }
        }
    }
    println!(
        "\nnote: scaling efficiency = (1-thread median / N-thread median) / N; 1-thread best = {baseline:.2} ms"
    );
}
