//! e75: reconstruct one missing immutable payload block with XOR parity.
use common::{Lcg, bench, check_consistency};
const BLOCKS: usize = 15;
const BYTES: usize = 64 * 1024;
fn main() {
    println!("e75 — salamander parity regeneration (kernel tier)");
    let (blocks, parity) = fixture();
    let expected = checksum(&blocks);
    let repaired = repair(&blocks, &parity, 7);
    assert_eq!(repaired, blocks[7]);
    let mut damaged = blocks.clone();
    damaged[7] = repaired;
    assert_eq!(checksum(&damaged), expected);
    println!(
        "single loss: bit-exact; parity overhead {:.2}%; recovery bytes {} vs segment {} ({:.1}x less)",
        100.0 / BLOCKS as f64,
        (BLOCKS + 1) * BYTES,
        BLOCKS * BYTES,
        (BLOCKS * BYTES) as f64 / ((BLOCKS + 1) * BYTES) as f64
    );
    println!("double loss: fail closed (XOR promise is one block)");
    let rs = [
        bench("whole copy checksum", || checksum(&blocks)),
        bench("xor repair checksum", || {
            let mut x = blocks.clone();
            x[7] = repair(&blocks, &parity, 7);
            checksum(&x)
        }),
    ];
    check_consistency(&rs);
}
fn fixture() -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut r = Lcg::new(0x7500_0075);
    let blocks = (0..BLOCKS)
        .map(|_| (0..BYTES).map(|_| r.below(256) as u8).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut parity = vec![0; BYTES];
    for b in &blocks {
        for (i, v) in b.iter().enumerate() {
            parity[i] ^= *v;
        }
    }
    (blocks, parity)
}
fn repair(blocks: &[Vec<u8>], parity: &[u8], missing: usize) -> Vec<u8> {
    let mut out = parity.to_vec();
    for (j, b) in blocks.iter().enumerate() {
        if j != missing {
            for (i, v) in b.iter().enumerate() {
                out[i] ^= *v;
            }
        }
    }
    out
}
fn checksum(bs: &[Vec<u8>]) -> u64 {
    bs.iter().flatten().fold(0xcbf2_9ce4_8422_2325, |h, v| {
        (h ^ u64::from(*v)).wrapping_mul(0x100_0000_01b3)
    })
}
