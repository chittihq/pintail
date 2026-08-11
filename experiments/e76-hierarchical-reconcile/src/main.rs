//! e76: descend a digest hierarchy to exact mismatching key chunks.
use common::{Lcg, bench, check_consistency};
const N: usize = 131_072;
const LEAF: usize = 128;
#[derive(Clone, Copy)]
enum Shape {
    Sparse,
    Clustered,
    Dense,
}
fn main() {
    println!("e76 — DNA-style hierarchical reconciliation (kernel tier)");
    for sh in [Shape::Sparse, Shape::Clustered, Shape::Dense] {
        let (a, b) = fixture(sh);
        let exact = (0..N).filter(|&i| a[i] != b[i]).collect::<Vec<_>>();
        let (flat_rows, flat) = flat(&a, &b);
        let (tree_rows, tree) = tree(&a, &b);
        assert_eq!(flat, exact);
        assert_eq!(tree, exact);
        println!(
            "{} differences {:>6} flat rows {:>7} tree rows {:>7} ratio {:>6.1}x",
            name(sh),
            exact.len(),
            flat_rows,
            tree_rows,
            flat_rows as f64 / tree_rows.max(1) as f64
        );
        let rs = [
            bench("flat", || digest(&flat)),
            bench("tree", || digest(&tree)),
        ];
        check_consistency(&rs);
    }
}
fn name(s: Shape) -> &'static str {
    match s {
        Shape::Sparse => "sparse",
        Shape::Clustered => "clustered",
        Shape::Dense => "dense",
    }
}
fn fixture(s: Shape) -> (Vec<u64>, Vec<u64>) {
    let mut r = Lcg::new(0x7600_0076);
    let a = (0..N).map(|_| r.next_u64()).collect::<Vec<_>>();
    let mut b = a.clone();
    match s {
        Shape::Sparse => {
            for i in (17..N).step_by(4096) {
                b[i] ^= 1
            }
        }
        Shape::Clustered => {
            for x in b.iter_mut().skip(50_000).take(1000) {
                *x ^= 1
            }
        }
        Shape::Dense => {
            for i in (0..N).step_by(3) {
                b[i] ^= 1
            }
        }
    }
    (a, b)
}
fn hash(x: &[u64]) -> u64 {
    x.iter().fold(0xcbf2_9ce4_8422_2325, |h, v| {
        (h ^ *v).wrapping_mul(0x100_0000_01b3)
    })
}
fn flat(a: &[u64], b: &[u64]) -> (u64, Vec<usize>) {
    let mut rows = 0;
    let mut out = Vec::new();
    for base in (0..N).step_by(LEAF) {
        if hash(&a[base..base + LEAF]) != hash(&b[base..base + LEAF]) {
            rows += LEAF as u64;
            for i in base..base + LEAF {
                if a[i] != b[i] {
                    out.push(i)
                }
            }
        }
    }
    (rows, out)
}
fn tree(a: &[u64], b: &[u64]) -> (u64, Vec<usize>) {
    fn walk(a: &[u64], b: &[u64], base: usize, rows: &mut u64, out: &mut Vec<usize>) {
        if hash(a) == hash(b) {
            return;
        }
        if a.len() <= LEAF {
            *rows += a.len() as u64;
            for i in 0..a.len() {
                if a[i] != b[i] {
                    out.push(base + i)
                }
            }
        } else {
            let m = a.len() / 2;
            walk(&a[..m], &b[..m], base, rows, out);
            walk(&a[m..], &b[m..], base + m, rows, out)
        }
    }
    let mut rows = 0;
    let mut out = Vec::new();
    walk(a, b, 0, &mut rows, &mut out);
    (rows, out)
}
fn digest(x: &[usize]) -> u64 {
    x.iter().fold(0, |h, v| h.rotate_left(5) ^ *v as u64)
}
