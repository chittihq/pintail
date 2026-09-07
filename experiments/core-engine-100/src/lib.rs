//! Isolated algorithm experiments. None changes the production executor.
use std::collections::BTreeMap;

pub mod distinct;
pub mod high;
pub mod join;
pub mod low;
pub mod merge;
pub mod scan;
pub mod topk;

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub id: usize,
    pub key: usize,
    pub low: usize,
    pub value: i64,
    pub valid: bool,
}

pub struct Data {
    pub rows: Vec<Row>,
    pub domain: usize,
    pub low_domain: usize,
    pub scenario: usize,
}

pub fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}
impl Data {
    pub fn new(n: usize, seed: u64, scenario: usize) -> Self {
        let domain = (n / 8).clamp(17, 65536);
        let low_domain = if scenario == 1 { 64 } else { 16 };
        let mut state = seed;
        let rows = (0..n)
            .map(|id| {
                let r = random(&mut state);
                let key = match scenario {
                    1 if r % 10 < 9 => 0,
                    2 => id * domain / n.max(1),
                    _ => r as usize % domain,
                };
                Row {
                    id,
                    key,
                    low: key % low_domain,
                    value: (random(&mut state) % 20001) as i64 - 10000,
                    valid: !random(&mut state).is_multiple_of(17),
                }
            })
            .collect();
        Self {
            rows,
            domain,
            low_domain,
            scenario,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Agg {
    pub rows: u64,
    pub count: u64,
    pub sum: i128,
}
impl Agg {
    pub fn add(&mut self, r: &Row) {
        self.rows += 1;
        if r.valid {
            self.count += 1;
            self.sum += i128::from(r.value);
        }
    }
    pub fn merge(&mut self, rhs: Self) {
        self.rows += rhs.rows;
        self.count += rhs.count;
        self.sum += rhs.sum;
    }
}
pub type Groups = BTreeMap<usize, Agg>;
pub fn flat(groups: impl IntoIterator<Item = (usize, Agg)>) -> Vec<i128> {
    groups
        .into_iter()
        .flat_map(|(k, a)| [k as i128, i128::from(a.rows), i128::from(a.count), a.sum])
        .collect()
}
pub fn reference_group(rows: &[Row], low: bool) -> Groups {
    let mut result = Groups::new();
    for r in rows {
        result
            .entry(if low { r.low } else { r.key })
            .or_default()
            .add(r);
    }
    result
}
pub fn names(case: usize) -> &'static [&'static str] {
    match case {
        1 => scan::NAMES,
        2 => merge::NAMES,
        3 => low::NAMES,
        4 => high::NAMES,
        5 => join::NAMES,
        6 => distinct::NAMES,
        7 => topk::NAMES,
        _ => panic!("unknown case"),
    }
}
pub fn run(case: usize, variant: usize, data: &Data) -> Vec<i128> {
    match case {
        1 => scan::run(variant, data),
        2 => merge::run(variant, data),
        3 => low::run(variant, data),
        4 => high::run(variant, data),
        5 => join::run(variant, data),
        6 => distinct::run(variant, data),
        7 => topk::run(variant, data),
        _ => panic!("unknown case"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_approach_matches_complete_reference() {
        for case in 1..=7 {
            for scenario in 0..3 {
                for seed in [1, 7, 991] {
                    for n in [0, 1, 63, 257, 4097] {
                        let d = Data::new(n, seed, scenario);
                        let expected = run(case, 0, &d);
                        for v in 1..=10 {
                            assert_eq!(
                                run(case, v, &d),
                                expected,
                                "case={case} variant={v} n={n} seed={seed} scenario={scenario}"
                            );
                        }
                    }
                }
            }
        }
    }
}
