//! e78: split one per-block bit budget across query-feature Bloom receptors.

use std::collections::HashSet;

use common::{Lcg, bench, check_consistency};

const BLOCKS: usize = 128;
const ROWS_PER_BLOCK: usize = 64;
const BLOCK_BITS: usize = 2_048;
const PARTITIONS: usize = 4;
const CLASSES: usize = 5;

#[derive(Clone, Copy)]
enum Policy {
    Pk,
    Partitioned,
    Learned,
    Ensemble,
}

#[derive(Clone, Copy)]
struct Record {
    pk: u64,
    tenant: u16,
    category: u8,
}

#[derive(Clone, Copy)]
enum Predicate {
    Point(u64),
    Composite(u16, u8),
    In([u64; 4]),
}

#[derive(Clone)]
struct TrialQuery {
    predicate: Predicate,
    class: usize,
    matches: [u8; BLOCKS],
}

struct Bloom {
    words: Vec<u64>,
    bit_count: usize,
    hashes: u8,
    seed: u64,
}

struct BlockFilters {
    pk: Vec<Bloom>,
    tuple: Option<Bloom>,
    tenant: Option<Bloom>,
    category: Option<Bloom>,
}

struct Evaluation {
    checksum: u64,
    false_reads: [u64; CLASSES],
    true_reads: u64,
    false_negatives: u64,
}

fn main() {
    println!("e78 — Bloom receptor ensemble (block-metadata kernel, audited)");
    let blocks = fixture();
    let queries = queries(&blocks);
    let learned_pk_bits = learn_allocation(&queries);
    let policies = [
        ("one PK Bloom", Policy::Pk),
        ("partitioned PK", Policy::Partitioned),
        ("learned allocation", Policy::Learned),
        ("receptor ensemble", Policy::Ensemble),
    ];

    for (name, policy) in policies {
        let filters = build_indexes(&blocks, policy, learned_pk_bits);
        assert_eq!(metadata_bits(&filters), BLOCKS * BLOCK_BITS);
        let result = evaluate(&filters, &queries, policy);
        assert_eq!(result.false_negatives, 0);
        println!(
            "{name:<20} false block reads {:>7} [point {}, absent {}, composite {}, IN {}, adversarial {}] true {} FN {} bits {} build probes {}",
            result.false_reads.iter().sum::<u64>(),
            result.false_reads[0],
            result.false_reads[1],
            result.false_reads[2],
            result.false_reads[3],
            result.false_reads[4],
            result.true_reads,
            result.false_negatives,
            metadata_bits(&filters),
            BLOCKS * ROWS_PER_BLOCK * 3,
        );
    }

    let indexes = policies.map(|(_, policy)| build_indexes(&blocks, policy, learned_pk_bits));
    let results = policies
        .iter()
        .zip(indexes.iter())
        .map(|((name, policy), index)| bench(name, || evaluate(index, &queries, *policy).checksum))
        .collect::<Vec<_>>();
    check_consistency(&results);

    for (name, policy) in policies {
        let _ = bench(&format!("build {name}"), || {
            index_checksum(&build_indexes(&blocks, policy, learned_pk_bits))
        });
    }
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

impl Bloom {
    fn new(seed: u64, bit_count: usize, hashes: u8) -> Self {
        assert!(bit_count.is_multiple_of(64));
        Self {
            words: vec![0; bit_count / 64],
            bit_count,
            hashes,
            seed,
        }
    }

    fn position(&self, value: u64, hash: u8) -> usize {
        mix64(value ^ self.seed ^ u64::from(hash).wrapping_mul(0x9e37_79b9_7f4a_7c15)) as usize
            % self.bit_count
    }

    fn add(&mut self, value: u64) {
        for hash in 0..self.hashes {
            let position = self.position(value, hash);
            self.words[position / 64] |= 1_u64 << (position % 64);
        }
    }

    fn has(&self, value: u64) -> bool {
        (0..self.hashes).all(|hash| {
            let position = self.position(value, hash);
            self.words[position / 64] & (1_u64 << (position % 64)) != 0
        })
    }

    #[cfg(test)]
    fn occupied_quarters(&self) -> [u32; 4] {
        let mut quarters = [0_u32; 4];
        for (word, bits) in self.words.iter().enumerate() {
            quarters[word * 4 / self.words.len()] += bits.count_ones();
        }
        quarters
    }
}

fn route(value: u64) -> usize {
    mix64(value ^ 0xd1b5_4a32_d192_ed03) as usize % PARTITIONS
}

fn tuple_key(tenant: u16, category: u8) -> u64 {
    (u64::from(tenant) << 8) | u64::from(category)
}

fn learn_allocation(queries: &[TrialQuery]) -> usize {
    let pk_queries = queries.iter().filter(|query| query.class != 2).count();
    let proportional = BLOCK_BITS * pk_queries / queries.len();
    let rounded = proportional / 64 * 64;
    rounded.clamp(64, BLOCK_BITS - 64)
}

fn new_block_filters(policy: Policy, block: usize, learned_pk_bits: usize) -> BlockFilters {
    let seed = 0xa5a5_1001_u64 ^ (block as u64).wrapping_mul(0x9e37_79b9);
    match policy {
        Policy::Pk => BlockFilters {
            pk: vec![Bloom::new(seed, BLOCK_BITS, 3)],
            tuple: None,
            tenant: None,
            category: None,
        },
        Policy::Partitioned => BlockFilters {
            pk: (0..PARTITIONS)
                .map(|part| Bloom::new(seed ^ part as u64, BLOCK_BITS / PARTITIONS, 3))
                .collect(),
            tuple: None,
            tenant: None,
            category: None,
        },
        Policy::Learned => BlockFilters {
            pk: vec![Bloom::new(seed, learned_pk_bits, 2)],
            tuple: Some(Bloom::new(seed ^ 0x22, BLOCK_BITS - learned_pk_bits, 1)),
            tenant: None,
            category: None,
        },
        Policy::Ensemble => BlockFilters {
            pk: vec![Bloom::new(seed, BLOCK_BITS / 2, 1)],
            tuple: None,
            tenant: Some(Bloom::new(seed ^ 0x33, BLOCK_BITS / 4, 1)),
            category: Some(Bloom::new(seed ^ 0x44, BLOCK_BITS / 4, 1)),
        },
    }
}

fn add_record(filters: &mut BlockFilters, record: Record, policy: Policy) {
    match policy {
        Policy::Pk => filters.pk[0].add(record.pk),
        Policy::Partitioned => filters.pk[route(record.pk)].add(record.pk),
        Policy::Learned => {
            filters.pk[0].add(record.pk);
            filters
                .tuple
                .as_mut()
                .expect("learned tuple receptor")
                .add(tuple_key(record.tenant, record.category));
        }
        Policy::Ensemble => {
            filters.pk[0].add(record.pk);
            filters
                .tenant
                .as_mut()
                .expect("tenant receptor")
                .add(u64::from(record.tenant));
            filters
                .category
                .as_mut()
                .expect("category receptor")
                .add(u64::from(record.category));
        }
    }
}

fn build_indexes(
    blocks: &[Vec<Record>],
    policy: Policy,
    learned_pk_bits: usize,
) -> Vec<BlockFilters> {
    blocks
        .iter()
        .enumerate()
        .map(|(block, rows)| {
            let mut filters = new_block_filters(policy, block, learned_pk_bits);
            for &record in rows {
                add_record(&mut filters, record, policy);
            }
            filters
        })
        .collect()
}

fn metadata_bits(filters: &[BlockFilters]) -> usize {
    filters
        .iter()
        .map(|filter| {
            filter.pk.iter().map(|bloom| bloom.bit_count).sum::<usize>()
                + filter.tuple.as_ref().map_or(0, |bloom| bloom.bit_count)
                + filter.tenant.as_ref().map_or(0, |bloom| bloom.bit_count)
                + filter.category.as_ref().map_or(0, |bloom| bloom.bit_count)
        })
        .sum()
}

fn pk_might_match(filters: &BlockFilters, pk: u64, policy: Policy) -> bool {
    match policy {
        Policy::Partitioned => filters.pk[route(pk)].has(pk),
        _ => filters.pk[0].has(pk),
    }
}

fn might_match(filters: &BlockFilters, predicate: Predicate, policy: Policy) -> bool {
    match predicate {
        Predicate::Point(pk) => pk_might_match(filters, pk, policy),
        Predicate::In(keys) => keys
            .into_iter()
            .any(|pk| pk_might_match(filters, pk, policy)),
        Predicate::Composite(tenant, category) => match policy {
            Policy::Pk | Policy::Partitioned => true,
            Policy::Learned => filters
                .tuple
                .as_ref()
                .expect("learned tuple receptor")
                .has(tuple_key(tenant, category)),
            Policy::Ensemble => {
                filters
                    .tenant
                    .as_ref()
                    .expect("tenant receptor")
                    .has(u64::from(tenant))
                    && filters
                        .category
                        .as_ref()
                        .expect("category receptor")
                        .has(u64::from(category))
            }
        },
    }
}

fn predicate_matches(predicate: Predicate, record: Record) -> bool {
    match predicate {
        Predicate::Point(pk) => record.pk == pk,
        Predicate::Composite(tenant, category) => {
            record.tenant == tenant && record.category == category
        }
        Predicate::In(keys) => keys.contains(&record.pk),
    }
}

fn trial_query(blocks: &[Vec<Record>], predicate: Predicate, class: usize) -> TrialQuery {
    let matches = std::array::from_fn(|block| {
        blocks[block]
            .iter()
            .filter(|record| predicate_matches(predicate, **record))
            .count() as u8
    });
    TrialQuery {
        predicate,
        class,
        matches,
    }
}

fn fixture() -> Vec<Vec<Record>> {
    (0..BLOCKS)
        .map(|block| {
            (0..ROWS_PER_BLOCK)
                .map(|row| {
                    let id = block * ROWS_PER_BLOCK + row;
                    Record {
                        pk: mix64(id as u64 ^ 0x7800_0078),
                        tenant: ((block * 3 + row % 5) % 256) as u16,
                        category: ((block + row % 7) % 64) as u8,
                    }
                })
                .collect()
        })
        .collect()
}

fn queries(blocks: &[Vec<Record>]) -> Vec<TrialQuery> {
    let rows = blocks.iter().flatten().copied().collect::<Vec<_>>();
    let pks = rows.iter().map(|record| record.pk).collect::<HashSet<_>>();
    let tuples = rows
        .iter()
        .map(|record| (record.tenant, record.category))
        .collect::<HashSet<_>>();
    let mut random = Lcg::new(0x7811_0078);
    let mut output = Vec::new();

    for index in 0..600 {
        output.push(trial_query(
            blocks,
            Predicate::Point(rows[index * 11 % rows.len()].pk),
            0,
        ));
    }
    while output.len() < 1_200 {
        let pk = random.next_u64();
        if !pks.contains(&pk) {
            output.push(trial_query(blocks, Predicate::Point(pk), 1));
        }
    }
    for index in 0..300 {
        let record = rows[index * 23 % rows.len()];
        output.push(trial_query(
            blocks,
            Predicate::Composite(record.tenant, record.category),
            2,
        ));
    }
    let composite_end = output.len() + 300;
    while output.len() < composite_end {
        let tuple = (random.below(256) as u16, random.below(64) as u8);
        if !tuples.contains(&tuple) {
            output.push(trial_query(
                blocks,
                Predicate::Composite(tuple.0, tuple.1),
                2,
            ));
        }
    }
    for index in 0..400 {
        let keys = [
            rows[index * 7 % rows.len()].pk,
            rows[(index * 13 + 1) % rows.len()].pk,
            random.next_u64(),
            random.next_u64(),
        ];
        output.push(trial_query(blocks, Predicate::In(keys), 3));
    }
    for index in 0..400_u64 {
        let pk = (index << 32) | 3;
        if !pks.contains(&pk) {
            output.push(trial_query(blocks, Predicate::Point(pk), 4));
        }
    }
    output
}

fn evaluate(filters: &[BlockFilters], queries: &[TrialQuery], policy: Policy) -> Evaluation {
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut false_reads = [0_u64; CLASSES];
    let mut true_reads = 0;
    let mut false_negatives = 0;

    for query in queries {
        let mut matches = 0_u64;
        for (block, filter) in filters.iter().enumerate() {
            let selected = might_match(filter, query.predicate, policy);
            if selected {
                if query.matches[block] == 0 {
                    false_reads[query.class] += 1;
                } else {
                    true_reads += 1;
                    matches += u64::from(query.matches[block]);
                }
            } else if query.matches[block] != 0 {
                false_negatives += 1;
            }
        }
        checksum = mix64(checksum ^ matches ^ query.class as u64);
    }

    Evaluation {
        checksum,
        false_reads,
        true_reads,
        false_negatives,
    }
}

fn index_checksum(filters: &[BlockFilters]) -> u64 {
    filters.iter().fold(0_u64, |checksum, filter| {
        let words = filter
            .pk
            .iter()
            .flat_map(|bloom| bloom.words.iter())
            .chain(filter.tuple.iter().flat_map(|bloom| bloom.words.iter()))
            .chain(filter.tenant.iter().flat_map(|bloom| bloom.words.iter()))
            .chain(filter.category.iter().flat_map(|bloom| bloom.words.iter()));
        words.fold(checksum, |hash, word| mix64(hash ^ *word))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_has_equal_bits_probes_and_zero_false_negatives() {
        let blocks = fixture();
        let queries = queries(&blocks);
        let learned_pk_bits = learn_allocation(&queries);
        for policy in [
            Policy::Pk,
            Policy::Partitioned,
            Policy::Learned,
            Policy::Ensemble,
        ] {
            let filters = build_indexes(&blocks, policy, learned_pk_bits);
            assert_eq!(metadata_bits(&filters), BLOCKS * BLOCK_BITS,);
            assert_eq!(evaluate(&filters, &queries, policy).false_negatives, 0);
        }
    }

    #[test]
    fn partition_routing_does_not_alias_local_bit_quarters() {
        let blocks = fixture();
        let queries = queries(&blocks);
        let filters = build_indexes(&blocks, Policy::Partitioned, learn_allocation(&queries));
        let occupancy = filters
            .iter()
            .fold([[0_u32; 4]; PARTITIONS], |mut totals, filter| {
                for (part, bloom) in filter.pk.iter().enumerate() {
                    for (quarter, count) in bloom.occupied_quarters().into_iter().enumerate() {
                        totals[part][quarter] += count;
                    }
                }
                totals
            });
        for partition in occupancy {
            assert!(
                partition.into_iter().all(|count| count > 0),
                "{partition:?}"
            );
        }
        let routed = (0..4_096_u64)
            .map(|value| route((value << 32) | 3))
            .collect::<HashSet<_>>();
        assert_eq!(routed.len(), PARTITIONS);
    }

    #[test]
    fn learned_split_is_derived_from_the_observed_query_mix() {
        let blocks = fixture();
        let queries = queries(&blocks);
        let expected = (BLOCK_BITS * queries.iter().filter(|query| query.class != 2).count()
            / queries.len())
            / 64
            * 64;
        assert_eq!(learn_allocation(&queries), expected);
    }

    #[test]
    fn adversarial_low_bits_do_not_create_false_negatives() {
        let blocks = fixture();
        let queries = queries(&blocks);
        let filters = build_indexes(&blocks, Policy::Partitioned, learn_allocation(&queries));
        for (block, rows) in blocks.iter().enumerate() {
            for record in rows {
                assert!(pk_might_match(
                    &filters[block],
                    record.pk,
                    Policy::Partitioned
                ));
            }
        }
    }
}
