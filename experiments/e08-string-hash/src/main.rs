//! e08: Length-classed string hashing (ClickHouse StringHashTable idea)
//! vs one generic byte-slice hash table, for GROUP BY on string keys.
//!
//! 20M rows over 10k distinct keys (mixed lengths, zipf-ish popularity).
//! Short keys (≤8B / ≤16B) become fixed-width integers — integer hash + compare
//! instead of slice hash + memcmp.

use common::*;
use hashbrown::HashMap;

fn key_checksum(bytes: &[u8], count: u64) -> u64 {
    let mut h = 0u64;
    for &b in bytes {
        h = h.wrapping_mul(131).wrapping_add(b as u64)
    }
    h.wrapping_mul(count)
}

fn main() {
    println!("e08-string-hash  N = {N_ORDERS}, distinct keys = 10k");
    let mut rng = Lcg::new(42);

    // dictionary of 10k distinct keys with realistic length mix
    let dictionary: Vec<String> = (0..10_000)
        .map(|i| match i % 4 {
            0 => format!("k{}", i),                                   // 2-6 bytes
            1 => format!("channel_{:04}", i),                         // 12 bytes
            2 => format!("sku-{:08}-{:04}", i, i % 97),               // 17 bytes
            _ => format!("customer-cohort-{:06}-region-{:02}", i, i % 32), // 32 bytes
        })
        .collect();

    // row keys: zipf-ish popularity via squared uniform
    let mut chars: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(N_ORDERS + 1);
    offsets.push(0);
    for _ in 0..N_ORDERS {
        let u = (rng.below(1_000_000) as f64 / 1e6) * (rng.below(1_000_000) as f64 / 1e6); // biases toward small indices
        let key = &dictionary[(u * 10_000.0) as usize % 10_000];
        chars.extend_from_slice(key.as_bytes());
        offsets.push(chars.len() as u32);
    }

    let mut rs = vec![];

    rs.push(bench("generic HashMap<&[u8], u64>", || {
        let mut m: HashMap<&[u8], u64> = HashMap::with_capacity(16_384);
        for i in 0..N_ORDERS {
            let s = &chars[offsets[i] as usize..offsets[i + 1] as usize];
            *m.entry(s).or_insert(0) += 1;
        }
        let mut ck = 0u64;
        for (k, c) in &m {
            ck = ck.wrapping_add(key_checksum(k, *c));
        }
        ck
    }));

    rs.push(bench("length-classed: u64 / u128 / generic", || {
        let mut short: HashMap<u64, u64> = HashMap::with_capacity(8_192);
        let mut medium: HashMap<u128, u64> = HashMap::with_capacity(8_192);
        let mut long: HashMap<&[u8], u64> = HashMap::with_capacity(8_192);
        for i in 0..N_ORDERS {
            let s = &chars[offsets[i] as usize..offsets[i + 1] as usize];
            match s.len() {
                0..=8 => {
                    let mut k = [0u8; 8];
                    k[..s.len()].copy_from_slice(s);
                    // length folded into the key so "a" != "a\0"-like collisions
                    *short.entry(u64::from_le_bytes(k) ^ ((s.len() as u64) << 56)).or_insert(0) += 1;
                }
                9..=16 => {
                    let mut k = [0u8; 16];
                    k[..s.len()].copy_from_slice(s);
                    *medium.entry(u128::from_le_bytes(k) ^ ((s.len() as u128) << 120)).or_insert(0) += 1;
                }
                _ => *long.entry(s).or_insert(0) += 1,
            }
        }
        // reconstruct byte keys for the cross-variant checksum
        let mut ck = 0u64;
        for (k, c) in &short {
            let len = (k >> 56) as usize // valid: corpus short keys are < 8 bytes
            ;
            let raw = (k ^ ((len as u64) << 56)).to_le_bytes();
            ck = ck.wrapping_add(key_checksum(&raw[..len], *c));
        }
        for (k, c) in &medium {
            let len = (k >> 120) as usize;
            let raw = (k ^ ((len as u128) << 120)).to_le_bytes();
            ck = ck.wrapping_add(key_checksum(&raw[..len], *c));
        }
        for (k, c) in &long {
            ck = ck.wrapping_add(key_checksum(k, *c));
        }
        ck
    }));

    check_consistency(&rs);
}
