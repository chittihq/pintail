//! e42: encode exact residuals only when sampled bit width predicts a win.
use common::{Lcg, bench, check_consistency};
#[derive(Clone, Copy)]
enum Shape {
    Linear,
    Product,
    Seasonal,
    Random,
}
fn main() {
    println!("e42 — predictive-coding columns (kernel tier)");
    for sh in [
        Shape::Linear,
        Shape::Product,
        Shape::Seasonal,
        Shape::Random,
    ] {
        let (x, y) = data(sh);
        let base = bytes(&y);
        let residual = y.iter().zip(&x).map(|(y, x)| y - x).collect::<Vec<_>>();
        let predicted = bytes(&residual) + 16;
        let chosen = if predicted < base { predicted } else { base };
        let exact = y
            .iter()
            .zip(&x)
            .zip(&residual)
            .all(|((y, x), r)| x + r == *y);
        println!(
            "{:<12} baseline {:>7} predictive {:>7} chosen {:>7} delta {:>6.1}% exact {exact}",
            name(sh),
            base,
            predicted,
            chosen,
            (chosen as f64 / base as f64 - 1.0) * 100.0
        );
        let rs = [
            bench("baseline", || checksum(&y)),
            bench("residual", || {
                checksum(
                    &x.iter()
                        .zip(&residual)
                        .map(|(x, r)| x + r)
                        .collect::<Vec<_>>(),
                )
            }),
        ];
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Linear => "linear",
        Shape::Product => "product",
        Shape::Seasonal => "seasonal",
        Shape::Random => "random",
    }
}
fn data(s: Shape) -> (Vec<i64>, Vec<i64>) {
    let mut r = Lcg::new(0x4200_0042);
    let x = (0..65_536)
        .map(|i| i as i64 + r.below(8) as i64)
        .collect::<Vec<_>>();
    let y = x
        .iter()
        .enumerate()
        .map(|(i, v)| match s {
            Shape::Linear => v * 3 + 5 + (i % 3) as i64,
            Shape::Product => v * (1 + (i % 5) as i64),
            Shape::Seasonal => v + ((i % 1024) as i64 - 512) * 20,
            Shape::Random => r.next_u64() as i64,
        })
        .collect();
    (x, y)
}
fn bytes(v: &[i64]) -> u64 {
    let (min, max) = v
        .iter()
        .fold((i64::MAX, i64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    let width = (u64::try_from(max.wrapping_sub(min))
        .unwrap_or(u64::MAX)
        .ilog2()
        + 1) as u64;
    (width * v.len() as u64).div_ceil(8)
}
fn checksum(v: &[i64]) -> u64 {
    v.iter().fold(0, |h, x| h.rotate_left(7) ^ *x as u64)
}
