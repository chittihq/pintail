//! e04: Hash join structure shoot-out (Q8 shape: users ⋈ orders, group by region).
//!
//! Contested questions:
//!  - hashbrown (Swiss table) vs Umbra-style unchained layout vs dense
//!    direct-address (perfect hash) for the build side
//!  - semi-join filtering: hash set vs dense bitmap vs register-blocked Bloom
//!
//! users: 200k dense ids → region (u8). orders: 20M rows probing user_id.

use common::*;
use hashbrown::{HashMap, HashSet};

const BBITS: u32 = 18; // 262,144 buckets for 200k keys (~0.76 avg chain)
const B: usize = 1 << BBITS;

#[inline]
fn hash32(k: u32) -> u64 {
    (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Simplified Umbra-style unchained table: bucket directory of [start,end)
/// ranges into one contiguous (key, val) array sorted by bucket, plus a 16-bit
/// Bloom tag word per bucket for early miss rejection.
struct Unchained {
    offsets: Vec<u32>, // B + 1
    keys: Vec<u32>,
    vals: Vec<u8>,
    tags: Vec<u16>,
}

impl Unchained {
    fn build(keys_in: &[u32], vals_in: &[u8]) -> Self {
        let mut counts = vec![0u32; B + 1];
        for &k in keys_in {
            let h = hash32(k);
            counts[(h >> (64 - BBITS)) as usize + 1] += 1;
        }
        for i in 1..=B {
            counts[i] += counts[i - 1];
        }
        let offsets = counts;
        let mut cursor = offsets.clone();
        let mut keys = vec![0u32; keys_in.len()];
        let mut vals = vec![0u8; vals_in.len()];
        let mut tags = vec![0u16; B];
        for (&k, &v) in keys_in.iter().zip(vals_in) {
            let h = hash32(k);
            let b = (h >> (64 - BBITS)) as usize;
            let slot = cursor[b] as usize;
            cursor[b] += 1;
            keys[slot] = k;
            vals[slot] = v;
            tags[b] |= 1 << ((h >> (64 - BBITS - 4)) & 15);
        }
        Self {
            offsets,
            keys,
            vals,
            tags,
        }
    }

    #[inline]
    fn probe(&self, k: u32) -> Option<u8> {
        let h = hash32(k);
        let b = (h >> (64 - BBITS)) as usize;
        if self.tags[b] & (1 << ((h >> (64 - BBITS - 4)) & 15)) == 0 {
            return None;
        }
        let (lo, hi) = (self.offsets[b] as usize, self.offsets[b + 1] as usize);
        for i in lo..hi {
            if self.keys[i] == k {
                return Some(self.vals[i]);
            }
        }
        None
    }
}

/// Register-blocked Bloom filter: one u64 block per key-partition, two bits set
/// from independent hash slices (VLDB 2019 "Performance-Optimal Filtering").
struct BlockedBloom {
    blocks: Vec<u64>,
    mask: u64,
}

impl BlockedBloom {
    fn build(keys: &[u32], bits_per_key: usize) -> Self {
        let nblocks = ((keys.len() * bits_per_key) / 64 + 1).next_power_of_two();
        let mut blocks = vec![0u64; nblocks];
        let mask = (nblocks - 1) as u64;
        for &k in keys {
            let h = hash32(k);
            let b = (h & mask) as usize;
            blocks[b] |= (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 52) & 63));
        }
        Self { blocks, mask }
    }

    #[inline]
    fn contains(&self, k: u32) -> bool {
        let h = hash32(k);
        let word = self.blocks[(h & self.mask) as usize];
        let need = (1u64 << ((h >> 32) & 63)) | (1u64 << ((h >> 52) & 63));
        word & need == need
    }
}

fn ck_regions(acc: &[(i64, u64)]) -> u64 {
    let mut ck = 0u64;
    for (k, (s, c)) in acc.iter().enumerate() {
        if *c > 0 {
            ck = ck.wrapping_add((k as u64 + 1).wrapping_mul((*s as u64) ^ *c));
        }
    }
    ck
}

fn main() {
    println!("e04-join  users = {N_USERS}, orders = {N_ORDERS}");
    let o = gen_orders(N_ORDERS, 42);
    let mut r = Lcg::new(7);
    let region_by_user: Vec<u8> = (0..N_USERS)
        .map(|_| r.below(N_REGIONS as u64) as u8)
        .collect();
    let user_ids: Vec<u32> = (0..N_USERS).collect();

    println!("\n== build cost (200k users) ==");
    bench("build hashbrown HashMap<u32,u8>", || {
        let m: HashMap<u32, u8> = user_ids
            .iter()
            .copied()
            .zip(region_by_user.iter().copied())
            .collect();
        m.len() as u64
    });
    bench("build unchained (counts+prefix+fill+tags)", || {
        let t = Unchained::build(&user_ids, &region_by_user);
        t.keys.len() as u64
    });
    bench("build dense direct array (clone)", || {
        let d = region_by_user.clone();
        d.len() as u64
    });

    let map: HashMap<u32, u8> = user_ids
        .iter()
        .copied()
        .zip(region_by_user.iter().copied())
        .collect();
    let unchained = Unchained::build(&user_ids, &region_by_user);

    println!("\n== inner join + GROUP BY region: SUM(amount), COUNT (20M probes, all hit) ==");
    let mut rs = vec![];
    rs.push(bench("probe hashbrown per row", || {
        let mut acc = [(0i64, 0u64); 8];
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            let reg = *map.get(&u).unwrap();
            let a = &mut acc[reg as usize];
            a.0 += v;
            a.1 += 1;
        }
        ck_regions(&acc)
    }));
    rs.push(bench("probe unchained (tag + range scan)", || {
        let mut acc = [(0i64, 0u64); 8];
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            let reg = unchained.probe(u).unwrap();
            let a = &mut acc[reg as usize];
            a.0 += v;
            a.1 += 1;
        }
        ck_regions(&acc)
    }));
    rs.push(bench("probe dense direct array (perfect hash)", || {
        let mut acc = [(0i64, 0u64); 8];
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            let reg = region_by_user[u as usize];
            let a = &mut acc[reg as usize];
            a.0 += v;
            a.1 += 1;
        }
        ck_regions(&acc)
    }));
    check_consistency(&rs);

    println!("\n== semi-join: SUM/COUNT of orders whose user is in region 3 (~12.5% of users) ==");
    let sel_users: Vec<u32> = (0..N_USERS)
        .filter(|&u| region_by_user[u as usize] == 3)
        .collect();
    println!("   build side: {} users", sel_users.len());
    let set: HashSet<u32> = sel_users.iter().copied().collect();
    let mut bitmap = vec![0u64; (N_USERS as usize).div_ceil(64)];
    for &u in &sel_users {
        bitmap[(u / 64) as usize] |= 1 << (u % 64);
    }
    let bloom = BlockedBloom::build(&sel_users, 10);

    let mut rs = vec![];
    rs.push(bench("membership: hashbrown HashSet", || {
        let (mut s, mut c) = (0i64, 0u64);
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            if set.contains(&u) {
                s += v;
                c += 1;
            }
        }
        (s as u64) ^ c
    }));
    rs.push(bench("membership: dense bitmap (3.2 KB, L1)", || {
        let (mut s, mut c) = (0i64, 0u64);
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            if bitmap[(u / 64) as usize] & (1 << (u % 64)) != 0 {
                s += v;
                c += 1;
            }
        }
        (s as u64) ^ c
    }));
    // Bloom is approximate: verify against exact membership after the bloom hit.
    rs.push(bench("membership: blocked bloom + exact confirm", || {
        let (mut s, mut c) = (0i64, 0u64);
        for (&u, &v) in o.user_id.iter().zip(&o.amount) {
            if bloom.contains(u) && region_by_user[u as usize] == 3 {
                s += v;
                c += 1;
            }
        }
        (s as u64) ^ c
    }));
    check_consistency(&rs);
}
