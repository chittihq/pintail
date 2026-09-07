use pintail_types::{Float64, Value};
use std::{hint::black_box, mem::size_of, time::Instant};

#[derive(Debug)]
enum FrozenValue {
    Null,
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    Float64(Float64),
    Utf8(Box<str>),
    Binary(Box<[u8]>),
    Enum { index: u64, label: Box<str> },
}

impl From<&Value> for FrozenValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Boolean(x) => Self::Boolean(*x),
            Value::Int64(x) => Self::Int64(*x),
            Value::UInt64(x) => Self::UInt64(*x),
            Value::Float64(x) => Self::Float64(*x),
            Value::Utf8(x) => Self::Utf8(x.as_str().into()),
            Value::Binary(x) => Self::Binary(x.as_slice().into()),
            Value::Enum { index, label } => Self::Enum {
                index: *index,
                label: label.as_str().into(),
            },
        }
    }
}

impl FrozenValue {
    fn thaw(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(x) => Value::Boolean(*x),
            Self::Int64(x) => Value::Int64(*x),
            Self::UInt64(x) => Value::UInt64(*x),
            Self::Float64(x) => Value::Float64(*x),
            Self::Utf8(x) => Value::Utf8(x.to_string()),
            Self::Binary(x) => Value::Binary(x.to_vec()),
            Self::Enum { index, label } => Value::Enum {
                index: *index,
                label: label.to_string(),
            },
        }
    }
}

trait Cell: Sync {
    fn token(&self) -> u64;
    fn payload(&self) -> usize;
    fn allocated(&self) -> usize;
}

impl Cell for Value {
    fn token(&self) -> u64 {
        match self {
            Self::Null => 0,
            Self::Boolean(x) => u64::from(*x),
            Self::Int64(x) => *x as u64,
            Self::UInt64(x) => *x,
            Self::Float64(x) => x.to_bits(),
            Self::Utf8(x) => text_token(x.as_bytes()),
            Self::Binary(x) => text_token(x),
            Self::Enum { index, label } => index.wrapping_add(text_token(label.as_bytes())),
        }
    }
    fn payload(&self) -> usize {
        match self {
            Self::Utf8(x) | Self::Enum { label: x, .. } => x.capacity(),
            Self::Binary(x) => x.capacity(),
            _ => 0,
        }
    }
    fn allocated(&self) -> usize {
        usize::from(self.payload() != 0)
    }
}

impl Cell for FrozenValue {
    fn token(&self) -> u64 {
        match self {
            Self::Null => 0,
            Self::Boolean(x) => u64::from(*x),
            Self::Int64(x) => *x as u64,
            Self::UInt64(x) => *x,
            Self::Float64(x) => x.to_bits(),
            Self::Utf8(x) => text_token(x.as_bytes()),
            Self::Binary(x) => text_token(x),
            Self::Enum { index, label } => index.wrapping_add(text_token(label.as_bytes())),
        }
    }
    fn payload(&self) -> usize {
        match self {
            Self::Utf8(x) | Self::Enum { label: x, .. } => x.len(),
            Self::Binary(x) => x.len(),
            _ => 0,
        }
    }
    fn allocated(&self) -> usize {
        usize::from(self.payload() != 0)
    }
}

fn text_token(x: &[u8]) -> u64 {
    (x.len() as u64)
        .wrapping_add(u64::from(x.first().copied().unwrap_or(0)))
        .wrapping_add(u64::from(x.last().copied().unwrap_or(0)))
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn fixture(n: usize, width: usize, text_percent: u64, length: usize) -> Vec<Vec<Value>> {
    (0..n)
        .map(|r| {
            (0..width)
                .map(|c| {
                    let x = mix((r * width + c + 1) as u64);
                    if x.is_multiple_of(31) {
                        Value::Null
                    } else if x % 100 < text_percent {
                        let bytes = vec![b'a' + (x % 26) as u8; length + (x % 17) as usize];
                        if x.is_multiple_of(3) {
                            Value::Binary(bytes)
                        } else if x.is_multiple_of(5) {
                            Value::Enum {
                                index: x % 8,
                                label: String::from_utf8(bytes).unwrap(),
                            }
                        } else {
                            Value::Utf8(String::from_utf8(bytes).unwrap())
                        }
                    } else {
                        match x % 4 {
                            0 => Value::UInt64(x % 10000),
                            1 => Value::Int64(-((x % 10000) as i64)),
                            2 => Value::Boolean(x.is_multiple_of(2)),
                            _ => Value::float64((x % 10000) as f64 / 8.0),
                        }
                    }
                })
                .collect()
        })
        .collect()
}

#[derive(Debug)]
enum ArenaCell {
    Null,
    Boolean(bool),
    Int64(i64),
    UInt64(u64),
    Float64(Float64),
    Utf8(u32, u32),
    Binary(u32, u32),
    Enum(u64, u32, u32),
}

struct Arena {
    cells: Box<[ArenaCell]>,
    bytes: Box<[u8]>,
}

impl Arena {
    fn build(source: &[Vec<Value>]) -> Self {
        let payload: usize = source
            .iter()
            .flatten()
            .map(|v| match v {
                Value::Utf8(x) | Value::Enum { label: x, .. } => x.len(),
                Value::Binary(x) => x.len(),
                _ => 0,
            })
            .sum();
        let mut bytes = Vec::with_capacity(payload);
        let mut cells = Vec::with_capacity(source.iter().map(Vec::len).sum());
        for v in source.iter().flatten() {
            let mut append = |s: &[u8]| {
                let start = u32::try_from(bytes.len()).expect("arena exceeds 4 GiB");
                let len = u32::try_from(s.len()).expect("cell exceeds 4 GiB");
                start.checked_add(len).expect("arena exceeds 4 GiB");
                bytes.extend_from_slice(s);
                (start, len)
            };
            cells.push(match v {
                Value::Null => ArenaCell::Null,
                Value::Boolean(x) => ArenaCell::Boolean(*x),
                Value::Int64(x) => ArenaCell::Int64(*x),
                Value::UInt64(x) => ArenaCell::UInt64(*x),
                Value::Float64(x) => ArenaCell::Float64(*x),
                Value::Utf8(x) => {
                    let (s, l) = append(x.as_bytes());
                    ArenaCell::Utf8(s, l)
                }
                Value::Binary(x) => {
                    let (s, l) = append(x);
                    ArenaCell::Binary(s, l)
                }
                Value::Enum { index, label } => {
                    let (s, l) = append(label.as_bytes());
                    ArenaCell::Enum(*index, s, l)
                }
            });
        }
        Self {
            cells: cells.into_boxed_slice(),
            bytes: bytes.into_boxed_slice(),
        }
    }
    fn slice(&self, start: u32, len: u32) -> &[u8] {
        &self.bytes[start as usize..start as usize + len as usize]
    }
    fn thaw(&self, cell: &ArenaCell) -> Value {
        match cell {
            ArenaCell::Null => Value::Null,
            ArenaCell::Boolean(x) => Value::Boolean(*x),
            ArenaCell::Int64(x) => Value::Int64(*x),
            ArenaCell::UInt64(x) => Value::UInt64(*x),
            ArenaCell::Float64(x) => Value::Float64(*x),
            ArenaCell::Utf8(s, l) => {
                Value::Utf8(String::from_utf8(self.slice(*s, *l).to_vec()).unwrap())
            }
            ArenaCell::Binary(s, l) => Value::Binary(self.slice(*s, *l).to_vec()),
            ArenaCell::Enum(i, s, l) => Value::Enum {
                index: *i,
                label: String::from_utf8(self.slice(*s, *l).to_vec()).unwrap(),
            },
        }
    }
    fn token(&self, cell: &ArenaCell) -> u64 {
        match cell {
            ArenaCell::Null => 0,
            ArenaCell::Boolean(x) => u64::from(*x),
            ArenaCell::Int64(x) => *x as u64,
            ArenaCell::UInt64(x) => *x,
            ArenaCell::Float64(x) => x.to_bits(),
            ArenaCell::Utf8(s, l) | ArenaCell::Binary(s, l) => text_token(self.slice(*s, *l)),
            ArenaCell::Enum(i, s, l) => i.wrapping_add(text_token(self.slice(*s, *l))),
        }
    }
}

enum Rows {
    Arena(Arena),
    Nested(Vec<Vec<Value>>),
    Boxed(Box<[Box<[Value]>]>),
    Flat(Box<[Value]>),
    Compact(Box<[FrozenValue]>),
}

impl Rows {
    fn build(arm: &str, source: &[Vec<Value>]) -> Self {
        match arm {
            "arena" => Self::Arena(Arena::build(source)),
            "nested" => Self::Nested(source.to_vec()),
            "boxed" => Self::Boxed(
                source
                    .iter()
                    .map(|r| r.clone().into_boxed_slice())
                    .collect(),
            ),
            "flat" => Self::Flat(source.iter().flatten().cloned().collect()),
            "compact" => Self::Compact(source.iter().flatten().map(FrozenValue::from).collect()),
            _ => panic!("unknown arm"),
        }
    }
    fn memory(&self) -> (usize, usize) {
        fn payload<'a, T: Cell + 'a>(cells: impl Iterator<Item = &'a T>) -> (usize, usize) {
            cells.fold((0, 0), |(b, a), c| (b + c.payload(), a + c.allocated()))
        }
        match self {
            Self::Arena(a) => (
                size_of_val(a.cells.as_ref()) + a.bytes.len(),
                1 + usize::from(!a.bytes.is_empty()),
            ),
            Self::Nested(rows) => {
                let (b, a) = payload(rows.iter().flatten());
                (
                    b + rows.capacity() * size_of::<Vec<Value>>()
                        + rows
                            .iter()
                            .map(|r| r.capacity() * size_of::<Value>())
                            .sum::<usize>(),
                    a + rows.len() + 1,
                )
            }
            Self::Boxed(rows) => {
                let (b, a) = payload(rows.iter().flat_map(|r| r.iter()));
                (
                    b + rows.len() * size_of::<Box<[Value]>>()
                        + rows
                            .iter()
                            .map(|r| r.len() * size_of::<Value>())
                            .sum::<usize>(),
                    a + rows.len() + 1,
                )
            }
            Self::Flat(cells) => {
                let (b, a) = payload(cells.iter());
                (b + size_of_val(cells.as_ref()), a + 1)
            }
            Self::Compact(cells) => {
                let (b, a) = payload(cells.iter());
                (b + size_of_val(cells.as_ref()), a + 1)
            }
        }
    }
    fn verify(&self, source: &[Vec<Value>], width: usize) {
        for (i, row) in source.iter().enumerate() {
            match self {
                Self::Arena(a) => {
                    for (cell, value) in a.cells[i * width..(i + 1) * width].iter().zip(row) {
                        assert_eq!(&a.thaw(cell), value);
                    }
                }
                Self::Nested(rows) => assert_eq!(&rows[i], row),
                Self::Boxed(rows) => assert_eq!(rows[i].as_ref(), row.as_slice()),
                Self::Flat(cells) => assert_eq!(&cells[i * width..(i + 1) * width], row.as_slice()),
                Self::Compact(cells) => {
                    for (a, b) in cells[i * width..(i + 1) * width].iter().zip(row) {
                        assert_eq!(&a.thaw(), b);
                    }
                }
            }
        }
    }
    fn materialize(&self, indices: &[usize], width: usize) -> Vec<Vec<Value>> {
        indices
            .iter()
            .map(|i| match self {
                Self::Nested(rows) => rows[*i].clone(),
                Self::Boxed(rows) => rows[*i].to_vec(),
                Self::Flat(cells) => cells[i * width..(i + 1) * width].to_vec(),
                Self::Compact(cells) => cells[i * width..(i + 1) * width]
                    .iter()
                    .map(FrozenValue::thaw)
                    .collect(),
                Self::Arena(a) => a.cells[i * width..(i + 1) * width]
                    .iter()
                    .map(|c| a.thaw(c))
                    .collect(),
            })
            .collect()
    }
    fn probe(&self, indices: &[usize], width: usize) -> u64 {
        fn row_token<T: Cell>(row: &[T]) -> u64 {
            row.iter().enumerate().fold(0u64, |s, (c, v)| {
                s.wrapping_add(v.token().rotate_left(c as u32))
            })
        }
        match self {
            Self::Arena(a) => indices.iter().fold(0u64, |sum, i| {
                let token = black_box(&a.cells[i * width..(i + 1) * width])
                    .iter()
                    .enumerate()
                    .fold(0u64, |s, (c, v)| {
                        s.wrapping_add(a.token(v).rotate_left(c as u32))
                    });
                sum.wrapping_add(token)
            }),
            Self::Nested(rows) => indices
                .iter()
                .fold(0u64, |s, i| s.wrapping_add(row_token(black_box(&rows[*i])))),
            Self::Boxed(rows) => indices
                .iter()
                .fold(0u64, |s, i| s.wrapping_add(row_token(black_box(&rows[*i])))),
            Self::Flat(cells) => indices.iter().fold(0u64, |s, i| {
                s.wrapping_add(row_token(black_box(&cells[i * width..(i + 1) * width])))
            }),
            Self::Compact(cells) => indices.iter().fold(0u64, |s, i| {
                s.wrapping_add(row_token(black_box(&cells[i * width..(i + 1) * width])))
            }),
        }
    }
}

fn run_batch(rows: &Rows, indices: &[usize], width: usize, workers: usize) -> u64 {
    if workers == 1 {
        return rows.probe(indices, width);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = indices
            .chunks(indices.len().div_ceil(workers))
            .map(|part| scope.spawn(move || rows.probe(part, width)))
            .collect();
        handles
            .into_iter()
            .fold(0u64, |sum, h| sum.wrapping_add(h.join().unwrap()))
    })
}

fn rss_kib() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmHWM:")
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        args.len(),
        8,
        "arm n width text_percent length workers probes"
    );
    let n: usize = args[2].parse().unwrap();
    let width: usize = args[3].parse().unwrap();
    let workers: usize = args[6].parse().unwrap();
    let source = fixture(n, width, args[4].parse().unwrap(), args[5].parse().unwrap());
    let indices: Vec<_> = (0..args[7].parse().unwrap())
        .map(|i| mix(i as u64 + 91) as usize % n)
        .collect();
    let start = Instant::now();
    let rows = black_box(Rows::build(&args[1], black_box(&source)));
    let build_ns = start.elapsed().as_nanos();
    let (bytes, allocations) = rows.memory();
    let peak_kib = rss_kib();
    rows.verify(&source, width);
    let expected = Rows::Nested(source).probe(&indices, width);
    assert_eq!(run_batch(&rows, &indices, width, workers), expected);
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        assert_eq!(
            black_box(run_batch(black_box(&rows), &indices, width, workers)),
            expected
        );
        samples.push(start.elapsed().as_nanos());
    }
    let projection_indices = &indices[..indices.len().min(16384)];
    let mut materialize_ns = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let output = black_box(rows.materialize(black_box(projection_indices), width));
        materialize_ns.push(start.elapsed().as_nanos());
        assert_eq!(
            Rows::Nested(output).probe(&(0..projection_indices.len()).collect::<Vec<_>>(), width),
            rows.probe(projection_indices, width)
        );
    }
    let start = Instant::now();
    drop(rows);
    let drop_ns = start.elapsed().as_nanos();
    println!(
        "{{\"arm\":\"{}\",\"n\":{n},\"width\":{width},\"workers\":{workers},\"value_bytes\":{},\"frozen_value_bytes\":{},\"retained_bytes\":{bytes},\"live_allocations\":{allocations},\"build_ns\":{build_ns},\"drop_ns\":{drop_ns},\"fixture_and_build_peak_kib\":{peak_kib},\"checksum\":{expected},\"arena_cell_bytes\":{},\"materialize_ns\":{materialize_ns:?},\"batch_ns\":{samples:?}}}",
        args[1],
        size_of::<Value>(),
        size_of::<FrozenValue>(),
        size_of::<ArenaCell>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_variants_roundtrip_and_probe_identically() {
        let row = vec![
            Value::Null,
            Value::Boolean(true),
            Value::Int64(i64::MIN),
            Value::UInt64(u64::MAX),
            Value::float64(-0.0),
            Value::float64(f64::NAN),
            Value::Utf8("é水\0".into()),
            Value::Binary(vec![0, 255]),
            Value::Enum {
                index: 19,
                label: "z".into(),
            },
            Value::Utf8(String::new()),
            Value::Binary(Vec::new()),
        ];
        let source = vec![row.clone(), row];
        let width = source[0].len();
        let expected = Rows::build("nested", &source).probe(&[1, 0, 1], width);
        for arm in ["nested", "boxed", "flat", "compact", "arena"] {
            let rows = Rows::build(arm, &source);
            rows.verify(&source, width);
            assert_eq!(run_batch(&rows, &[1, 0, 1], width, 2), expected);
        }
    }
}
