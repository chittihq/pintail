//! e33: selective higher-order min/max summaries over ordinary block zone maps.
//!
//! Evidence tier: isolated metadata kernel with exact row-count validation.

use common::{Lcg, bench, check_consistency};

const BLOCKS: usize = 1024;
const BLOCK_ROWS: usize = 256;
const QUERIES: usize = 160;

#[derive(Clone, Copy)]
enum Shape {
    Clustered,
    Partial,
    Scattered,
}

#[derive(Clone, Copy)]
enum IndexKind {
    Flat,
    CompleteBinary,
    Venation,
}

#[derive(Clone, Copy)]
struct Bounds {
    min: u32,
    max: u32,
}

#[derive(Default)]
struct Outcome {
    checksum: u64,
    metadata_probes: u64,
    blocks_read: u64,
    matches: u64,
}

fn main() {
    println!("e33 — leaf-venation metadata (isolated metadata kernel)");
    println!("{BLOCKS} blocks × {BLOCK_ROWS} rows, {QUERIES} range predicates\n");

    for shape in [Shape::Clustered, Shape::Partial, Shape::Scattered] {
        let values = make_values(shape);
        let queries = make_queries();
        println!("=== {} ===", shape_name(shape));
        let flat = evaluate(&values, &queries, IndexKind::Flat);
        let tree = evaluate(&values, &queries, IndexKind::CompleteBinary);
        let veins = evaluate(&values, &queries, IndexKind::Venation);
        report("fixed leaf zone maps", &flat, BLOCKS);
        report("complete binary hierarchy", &tree, BLOCKS * 2 - 1);
        report(
            "selective 16/128 venation",
            &veins,
            BLOCKS + BLOCKS / 16 + BLOCKS / 128,
        );
        assert_eq!(flat.checksum, tree.checksum);
        assert_eq!(flat.checksum, veins.checksum);
        assert_eq!(flat.matches, tree.matches);
        assert_eq!(flat.matches, veins.matches);

        let results = [
            bench("fixed leaf maps", || {
                evaluate(&values, &queries, IndexKind::Flat).checksum
            }),
            bench("complete hierarchy", || {
                evaluate(&values, &queries, IndexKind::CompleteBinary).checksum
            }),
            bench("selective venation", || {
                evaluate(&values, &queries, IndexKind::Venation).checksum
            }),
        ];
        check_consistency(&results);
        println!();
    }
}

fn report(name: &str, outcome: &Outcome, summaries: usize) {
    println!(
        "{name:<30} probes {:>8}  blocks {:>7}  summaries {:>5}  matches {}",
        outcome.metadata_probes, outcome.blocks_read, summaries, outcome.matches
    );
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Clustered => "clustered",
        Shape::Partial => "partially clustered",
        Shape::Scattered => "scattered hostile control",
    }
}

fn make_values(shape: Shape) -> Vec<u32> {
    let mut random = Lcg::new(0x1EAF_0033_u64);
    let mut values = Vec::with_capacity(BLOCKS * BLOCK_ROWS);
    for block in 0..BLOCKS {
        for row in 0..BLOCK_ROWS {
            let value = match shape {
                Shape::Clustered => block as u32 * BLOCK_ROWS as u32 + row as u32,
                Shape::Partial => {
                    if block % 4 == 0 {
                        random.below((BLOCKS * BLOCK_ROWS) as u64) as u32
                    } else {
                        block as u32 * BLOCK_ROWS as u32 + row as u32
                    }
                }
                Shape::Scattered => random.below((BLOCKS * BLOCK_ROWS) as u64) as u32,
            };
            values.push(value);
        }
    }
    values
}

fn make_queries() -> Vec<(u32, u32)> {
    let mut random = Lcg::new(0xC0A2_0033_u64);
    (0..QUERIES)
        .map(|index| {
            let width = if index % 8 == 0 { 8192 } else { 384 };
            let start = random.below((BLOCKS * BLOCK_ROWS - width) as u64) as u32;
            (start, start + width as u32)
        })
        .collect()
}

fn evaluate(values: &[u32], queries: &[(u32, u32)], kind: IndexKind) -> Outcome {
    let leaves = values
        .chunks_exact(BLOCK_ROWS)
        .map(bounds)
        .collect::<Vec<_>>();
    let mut outcome = Outcome {
        checksum: 0xcbf2_9ce4_8422_2325,
        ..Outcome::default()
    };

    for query in queries {
        let mut candidates = Vec::new();
        match kind {
            IndexKind::Flat => {
                for (block, bound) in leaves.iter().enumerate() {
                    outcome.metadata_probes += 1;
                    if overlaps(*bound, *query) {
                        candidates.push(block);
                    }
                }
            }
            IndexKind::CompleteBinary => {
                binary_visit(&leaves, 0, BLOCKS, *query, &mut candidates, &mut outcome);
            }
            IndexKind::Venation => {
                for coarse in (0..BLOCKS).step_by(128) {
                    outcome.metadata_probes += 1;
                    if !overlaps(bounds_of(&leaves[coarse..coarse + 128]), *query) {
                        continue;
                    }
                    for medium in (coarse..coarse + 128).step_by(16) {
                        outcome.metadata_probes += 1;
                        if !overlaps(bounds_of(&leaves[medium..medium + 16]), *query) {
                            continue;
                        }
                        for (offset, bound) in leaves[medium..medium + 16].iter().enumerate() {
                            outcome.metadata_probes += 1;
                            if overlaps(*bound, *query) {
                                candidates.push(medium + offset);
                            }
                        }
                    }
                }
            }
        }

        let mut query_count = 0_u64;
        for block in candidates {
            outcome.blocks_read += 1;
            query_count += values[block * BLOCK_ROWS..(block + 1) * BLOCK_ROWS]
                .iter()
                .filter(|value| **value >= query.0 && **value <= query.1)
                .count() as u64;
        }
        outcome.matches += query_count;
        outcome.checksum = (outcome.checksum ^ query_count).wrapping_mul(0x100_0000_01b3);
    }
    outcome
}

fn binary_visit(
    leaves: &[Bounds],
    start: usize,
    end: usize,
    query: (u32, u32),
    candidates: &mut Vec<usize>,
    outcome: &mut Outcome,
) {
    outcome.metadata_probes += 1;
    if !overlaps(bounds_of(&leaves[start..end]), query) {
        return;
    }
    if end - start == 1 {
        candidates.push(start);
        return;
    }
    let middle = (start + end) / 2;
    binary_visit(leaves, start, middle, query, candidates, outcome);
    binary_visit(leaves, middle, end, query, candidates, outcome);
}

fn bounds(values: &[u32]) -> Bounds {
    Bounds {
        min: *values.iter().min().expect("non-empty block"),
        max: *values.iter().max().expect("non-empty block"),
    }
}

fn bounds_of(bounds: &[Bounds]) -> Bounds {
    Bounds {
        min: bounds
            .iter()
            .map(|value| value.min)
            .min()
            .expect("non-empty range"),
        max: bounds
            .iter()
            .map(|value| value.max)
            .max()
            .expect("non-empty range"),
    }
}

fn overlaps(bounds: Bounds, query: (u32, u32)) -> bool {
    bounds.max >= query.0 && bounds.min <= query.1
}
