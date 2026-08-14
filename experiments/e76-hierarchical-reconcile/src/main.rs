//! e76: reconcile exact mismatching key ranges through persisted digest indexes.

use common::{Lcg, bench, check_consistency};

const N: usize = 131_072;
const LEAF_ROWS: usize = 128;
const LEAVES: usize = N / LEAF_ROWS;

type Row = Option<u64>;

#[derive(Clone, Copy)]
enum Shape {
    Sparse,
    Clustered,
    Dense,
}

struct FlatIndex {
    leaves: Vec<u64>,
}

struct DigestTree {
    nodes: Vec<u64>,
}

#[derive(Debug, Eq, PartialEq)]
struct Reconciliation {
    rows_transferred: u64,
    digests_compared: u64,
    differences: Vec<usize>,
}

impl Reconciliation {
    fn new() -> Self {
        Self {
            rows_transferred: 0,
            digests_compared: 0,
            differences: Vec::new(),
        }
    }

    fn answer_checksum(&self) -> u64 {
        self.differences
            .iter()
            .fold(0_u64, |checksum, key| checksum.rotate_left(5) ^ *key as u64)
    }
}

fn main() {
    println!("e76 — hierarchical reconciliation (persisted-index kernel, audited)");
    for shape in [Shape::Sparse, Shape::Clustered, Shape::Dense] {
        let (source, replica) = fixture(shape);
        let source_flat = FlatIndex::build(&source);
        let replica_flat = FlatIndex::build(&replica);
        let source_tree = DigestTree::build(&source_flat);
        let replica_tree = DigestTree::build(&replica_flat);

        let exact = full_scan(&source, &replica);
        let flat_result = reconcile_flat(&source, &replica, &source_flat, &replica_flat);
        let tree_result = reconcile_tree(&source, &replica, &source_tree, &replica_tree);
        assert_eq!(flat_result.differences, exact.differences);
        assert_eq!(tree_result.differences, exact.differences);

        println!(
            "{} differences {:>6} | full rows {:>7} | flat rows {:>7}, digests {:>4} | tree rows {:>7}, digests {:>4}",
            shape_name(shape),
            exact.differences.len(),
            exact.rows_transferred,
            flat_result.rows_transferred,
            flat_result.digests_compared,
            tree_result.rows_transferred,
            tree_result.digests_compared,
        );

        let results = [
            bench("full key scan", || {
                full_scan(&source, &replica).answer_checksum()
            }),
            bench("persisted flat checksums", || {
                reconcile_flat(&source, &replica, &source_flat, &replica_flat).answer_checksum()
            }),
            bench("persisted digest tree", || {
                reconcile_tree(&source, &replica, &source_tree, &replica_tree).answer_checksum()
            }),
        ];
        check_consistency(&results);
    }

    let unchanged = fixture(Shape::Sparse).0;
    let flat = FlatIndex::build(&unchanged);
    let tree = DigestTree::build(&flat);
    let flat_poll = reconcile_flat(&unchanged, &unchanged, &flat, &flat);
    let tree_poll = reconcile_tree(&unchanged, &unchanged, &tree, &tree);
    println!(
        "\nunchanged poll: flat {} digests, tree {} digest",
        flat_poll.digests_compared, tree_poll.digests_compared,
    );

    let unchanged_results = [
        bench("unchanged flat poll", || {
            reconcile_flat(&unchanged, &unchanged, &flat, &flat).answer_checksum()
        }),
        bench("unchanged tree poll", || {
            reconcile_tree(&unchanged, &unchanged, &tree, &tree).answer_checksum()
        }),
    ];
    check_consistency(&unchanged_results);

    let _ = bench("build flat leaf index", || {
        FlatIndex::build(&unchanged).leaves[0]
    });
    let _ = bench("build tree from leaf index", || {
        DigestTree::build(&flat).nodes[1]
    });
}

fn shape_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Sparse => "sparse + missing/extra",
        Shape::Clustered => "clustered",
        Shape::Dense => "dense",
    }
}

fn fixture(shape: Shape) -> (Vec<Row>, Vec<Row>) {
    let mut random = Lcg::new(0x7600_0076);
    let mut source = (0..N).map(|_| Some(random.next_u64())).collect::<Vec<_>>();
    let mut replica = source.clone();
    match shape {
        Shape::Sparse => {
            for key in (17..N).step_by(4_096) {
                replica[key] = replica[key].map(|value| value ^ 1);
            }
            // Fixed key slots make absence and presence explicit without shifting
            // every later row into a different chunk.
            source[1_000] = None;
            replica[90_000] = None;
        }
        Shape::Clustered => {
            for value in replica.iter_mut().skip(50_000).take(1_000) {
                *value = value.map(|value| value ^ 1);
            }
            replica[49_999] = None;
        }
        Shape::Dense => {
            for key in (0..N).step_by(3) {
                replica[key] = replica[key].map(|value| value ^ 1);
            }
        }
    }
    (source, replica)
}

fn mix(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(0x100_0000_01b3)
}

fn hash_leaf(rows: &[Row], base: usize) -> u64 {
    rows.iter()
        .enumerate()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, (offset, row)| {
            let key = (base + offset) as u64;
            let value = row.unwrap_or(0xa5a5_a5a5_a5a5_a5a5);
            mix(mix(hash, key), value ^ u64::from(row.is_none()))
        })
}

fn combine(left: u64, right: u64, node: usize) -> u64 {
    mix(mix(0x9e37_79b9_7f4a_7c15 ^ node as u64, left), right)
}

impl FlatIndex {
    fn build(rows: &[Row]) -> Self {
        assert_eq!(rows.len(), N);
        let leaves = rows
            .chunks_exact(LEAF_ROWS)
            .enumerate()
            .map(|(leaf, rows)| hash_leaf(rows, leaf * LEAF_ROWS))
            .collect();
        Self { leaves }
    }
}

impl DigestTree {
    fn build(flat: &FlatIndex) -> Self {
        assert_eq!(flat.leaves.len(), LEAVES);
        assert!(LEAVES.is_power_of_two());
        let mut nodes = vec![0_u64; LEAVES * 2];
        nodes[LEAVES..].copy_from_slice(&flat.leaves);
        for node in (1..LEAVES).rev() {
            nodes[node] = combine(nodes[node * 2], nodes[node * 2 + 1], node);
        }
        Self { nodes }
    }
}

fn compare_leaf(source: &[Row], replica: &[Row], leaf: usize, result: &mut Reconciliation) {
    let start = leaf * LEAF_ROWS;
    let end = start + LEAF_ROWS;
    result.rows_transferred += LEAF_ROWS as u64;
    for key in start..end {
        if source[key] != replica[key] {
            result.differences.push(key);
        }
    }
}

fn full_scan(source: &[Row], replica: &[Row]) -> Reconciliation {
    let mut result = Reconciliation::new();
    result.rows_transferred = N as u64;
    for key in 0..N {
        if source[key] != replica[key] {
            result.differences.push(key);
        }
    }
    result
}

fn reconcile_flat(
    source: &[Row],
    replica: &[Row],
    source_index: &FlatIndex,
    replica_index: &FlatIndex,
) -> Reconciliation {
    let mut result = Reconciliation::new();
    for leaf in 0..LEAVES {
        result.digests_compared += 1;
        if source_index.leaves[leaf] != replica_index.leaves[leaf] {
            compare_leaf(source, replica, leaf, &mut result);
        }
    }
    result
}

fn reconcile_tree(
    source: &[Row],
    replica: &[Row],
    source_index: &DigestTree,
    replica_index: &DigestTree,
) -> Reconciliation {
    fn descend(
        source: &[Row],
        replica: &[Row],
        source_index: &DigestTree,
        replica_index: &DigestTree,
        node: usize,
        result: &mut Reconciliation,
    ) {
        result.digests_compared += 1;
        if source_index.nodes[node] == replica_index.nodes[node] {
            return;
        }
        if node >= LEAVES {
            compare_leaf(source, replica, node - LEAVES, result);
            return;
        }
        descend(
            source,
            replica,
            source_index,
            replica_index,
            node * 2,
            result,
        );
        descend(
            source,
            replica,
            source_index,
            replica_index,
            node * 2 + 1,
            result,
        );
    }

    let mut result = Reconciliation::new();
    descend(source, replica, source_index, replica_index, 1, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_indexes_find_exact_changed_missing_and_extra_keys() {
        for shape in [Shape::Sparse, Shape::Clustered, Shape::Dense] {
            let (source, replica) = fixture(shape);
            let source_flat = FlatIndex::build(&source);
            let replica_flat = FlatIndex::build(&replica);
            let source_tree = DigestTree::build(&source_flat);
            let replica_tree = DigestTree::build(&replica_flat);
            let exact = full_scan(&source, &replica).differences;
            assert_eq!(
                reconcile_flat(&source, &replica, &source_flat, &replica_flat).differences,
                exact,
            );
            assert_eq!(
                reconcile_tree(&source, &replica, &source_tree, &replica_tree).differences,
                exact,
            );
        }
    }

    #[test]
    fn unchanged_tree_poll_reads_only_the_root_digest() {
        let rows = fixture(Shape::Sparse).0;
        let flat = FlatIndex::build(&rows);
        let tree = DigestTree::build(&flat);
        let result = reconcile_tree(&rows, &rows, &tree, &tree);
        assert_eq!(result.digests_compared, 1);
        assert_eq!(result.rows_transferred, 0);
        assert!(result.differences.is_empty());
    }

    #[test]
    fn timed_paths_execute_reconciliation_not_precomputed_digests() {
        let (source, replica) = fixture(Shape::Sparse);
        let source_flat = FlatIndex::build(&source);
        let replica_flat = FlatIndex::build(&replica);
        let source_tree = DigestTree::build(&source_flat);
        let replica_tree = DigestTree::build(&replica_flat);
        assert_ne!(
            reconcile_flat(&source, &replica, &source_flat, &replica_flat).digests_compared,
            reconcile_tree(&source, &replica, &source_tree, &replica_tree).digests_compared,
        );
    }
}
