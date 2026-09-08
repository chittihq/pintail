use core_engine_100::{Data, names, run};
use std::{hint::black_box, time::Instant};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |i: usize, default: usize| {
        args.get(i)
            .map_or(default, |s| s.parse().expect("numeric argument"))
    };
    let case = number(1, 1);
    let variant = number(2, 0);
    let n = number(3, 100000);
    let scenario = number(4, 0);
    let seed = number(5, 41);
    let rounds = number(6, 5);
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build_global()
        .unwrap();
    let data = Data::new(n, seed as u64, scenario);
    let reference = run(case, 0, &data);
    assert_eq!(run(case, variant, &data), reference, "warm correctness");
    let mut timings = Vec::new();
    for _ in 0..rounds {
        let start = Instant::now();
        let answer = black_box(run(case, variant, black_box(&data)));
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(answer, reference, "timed correctness");
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let hwm = status
        .lines()
        .find(|s| s.starts_with("VmHWM:"))
        .unwrap_or("unavailable");
    println!(
        "{}",
        serde_json::json!({"case":case,"variant":variant,"name":names(case)[variant],"rows":n,"scenario":scenario,"seed":seed,"ms":timings,"output_cells":reference.len(),"process_peak":hwm,"correct":true})
    );
}
