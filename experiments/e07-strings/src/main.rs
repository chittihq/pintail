//! e07: String column representation shoot-out.
//!
//! 20M strings (statuses 40%, country codes 20%, names 20%, emails 20%) in
//! three layouts: Vec<String> (pointer-per-row), flat chars+offsets
//! (ClickHouse ColumnString), German-string 16-byte views (DuckDB string_t).
//! Workloads: equality vs short constant (inline path), equality vs long
//! constant (heap path), ordering comparison.

use common::*;

const STATUSES: [&str; 5] = ["pending", "confirmed", "completed", "cancelled", "shipped"];
const COUNTRIES: [&str; 8] = ["IN", "US", "DE", "GB", "AE", "SG", "FR", "JP"];

/// 16-byte view: len <= 12 → bytes inline; else prefix + heap offset.
#[derive(Clone, Copy)]
#[repr(C)]
struct GView {
    len: u32,
    prefix: [u8; 4],
    tail: [u8; 8], // inline bytes 5..12, or LE offset into heap when len > 12
}

struct GColumn {
    views: Vec<GView>,
    heap: Vec<u8>,
}

impl GColumn {
    fn push(&mut self, s: &[u8]) {
        let mut prefix = [0u8; 4];
        let mut tail = [0u8; 8];
        let p = s.len().min(4);
        prefix[..p].copy_from_slice(&s[..p]);
        if s.len() <= 12 {
            if s.len() > 4 {
                let t = s.len() - 4;
                tail[..t].copy_from_slice(&s[4..4 + t]);
            }
        } else {
            let offset = self.heap.len() as u64;
            self.heap.extend_from_slice(s);
            tail = offset.to_le_bytes();
        }
        self.views.push(GView {
            len: s.len() as u32,
            prefix,
            tail,
        })
    }

    #[inline]
    fn eq_const(&self, view: &GView, needle: &GView, needle_bytes: &[u8]) -> bool {
        if view.len != needle.len || view.prefix != needle.prefix {
            return false;
        }
        if view.len <= 12 {
            view.tail == needle.tail
        } else {
            let offset = u64::from_le_bytes(view.tail) as usize;
            &self.heap[offset..offset + view.len as usize] == needle_bytes
        }
    }

    #[inline]
    fn bytes<'a>(&'a self, view: &'a GView, scratch: &'a mut [u8; 12]) -> &'a [u8] {
        if view.len <= 12 {
            scratch[..4].copy_from_slice(&view.prefix);
            scratch[4..12].copy_from_slice(&view.tail);
            &scratch[..view.len as usize]
        } else {
            let offset = u64::from_le_bytes(view.tail) as usize;
            &self.heap[offset..offset + view.len as usize]
        }
    }
}

fn make_view(s: &[u8]) -> GView {
    let mut column = GColumn {
        views: Vec::new(),
        heap: Vec::new(),
    };
    column.push(s);
    column.views[0]
}

fn main() {
    println!("e07-strings  N = {N_ORDERS}");
    let mut rng = Lcg::new(42);

    let mut owned: Vec<String> = Vec::with_capacity(N_ORDERS);
    let mut chars: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(N_ORDERS + 1);
    offsets.push(0);
    let mut gcol = GColumn {
        views: Vec::with_capacity(N_ORDERS),
        heap: Vec::new(),
    };

    for _ in 0..N_ORDERS {
        let roll = rng.below(100);
        let s: String = if roll < 40 {
            STATUSES[rng.below(5) as usize].to_string()
        } else if roll < 60 {
            COUNTRIES[rng.below(8) as usize].to_string()
        } else if roll < 80 {
            format!("user_{}", rng.below(1_000_000))
        } else {
            format!("user{}@example{}.com", rng.below(1_000_000), rng.below(100))
        };
        chars.extend_from_slice(s.as_bytes());
        offsets.push(chars.len() as u32);
        gcol.push(s.as_bytes());
        owned.push(s);
    }
    println!(
        "memory: Vec<String> ~{:.0} MB | chars+offsets {:.0} MB | views {:.0} MB",
        (owned.iter().map(|s| s.capacity()).sum::<usize>() + owned.len() * 24) as f64 / 1e6,
        (chars.len() + offsets.len() * 4) as f64 / 1e6,
        (gcol.views.len() * 16 + gcol.heap.len()) as f64 / 1e6,
    );

    for (label, needle) in [
        ("short (inline path)", "shipped"),
        ("long (heap path)", "user42@example7.com"),
    ] {
        println!("\n== COUNT WHERE s = \"{needle}\"  [{label}] ==");
        let needle_bytes = needle.as_bytes();
        let needle_view = make_view(needle_bytes);
        let mut rs = vec![];
        rs.push(bench("Vec<String>", || {
            owned.iter().filter(|s| s.as_str() == needle).count() as u64
        }));
        rs.push(bench("chars+offsets slices", || {
            let mut c = 0u64;
            for i in 0..N_ORDERS {
                let s = &chars[offsets[i] as usize..offsets[i + 1] as usize];
                c += (s == needle_bytes) as u64;
            }
            c
        }));
        rs.push(bench("German-string views", || {
            let mut c = 0u64;
            for view in &gcol.views {
                c += gcol.eq_const(view, &needle_view, needle_bytes) as u64;
            }
            c
        }));
        check_consistency(&rs);
    }

    println!("\n== COUNT WHERE s < \"m\"  [ordering comparison] ==");
    let bound = "m".as_bytes();
    let mut rs = vec![];
    rs.push(bench("Vec<String>", || {
        owned.iter().filter(|s| s.as_bytes() < bound).count() as u64
    }));
    rs.push(bench("chars+offsets slices", || {
        let mut c = 0u64;
        for i in 0..N_ORDERS {
            let s = &chars[offsets[i] as usize..offsets[i + 1] as usize];
            c += (s < bound) as u64;
        }
        c
    }));
    rs.push(bench("German-string views (prefix fast path)", || {
        let mut c = 0u64;
        let mut scratch = [0u8; 12];
        for view in &gcol.views {
            // resolve on the 4-byte prefix when it differs from the bound prefix
            let plen = (view.len as usize).min(4).min(bound.len());
            let vp = &view.prefix[..plen];
            let bp = &bound[..plen];
            if vp != bp {
                c += (vp < bp) as u64;
            } else {
                c += (gcol.bytes(view, &mut scratch) < bound) as u64;
            }
        }
        c
    }));
    check_consistency(&rs);
}
