//! What a materialized row costs in this representation. Ignored: a
//! measurement, not a gate. Run with
//! `cargo test --release -p pintail-types --test layout -- --ignored --nocapture`.
use std::time::Instant;

use pintail_types::Value;

/// Heap a value owns beyond its inline struct.
fn heap_of(value: &Value) -> usize {
    match value {
        Value::Utf8(text) => text.capacity(),
        Value::Binary(bytes) => bytes.capacity(),
        Value::Enum { label, .. } => label.capacity(),
        _ => 0,
    }
}

/// Bytes the value actually carries.
fn payload_of(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Utf8(text) => text.len(),
        Value::Binary(bytes) => bytes.len(),
        Value::Enum { label, .. } => label.len() + 8,
        _ => 8,
    }
}

fn report_row() -> Vec<Value> {
    vec![
        Value::UInt64(431_610),
        Value::Utf8("msg-431610".to_owned()),
        Value::UInt64(11_611),
        Value::UInt64(1_611),
        Value::UInt64(1),
        Value::Utf8("template-10".to_owned()),
        Value::Utf8("9000431610".to_owned()),
        Value::Utf8("8003021270".to_owned()),
        Value::Utf8("sender6@example.invalid".to_owned()),
        Value::Utf8("user11611@example.invalid".to_owned()),
        Value::Utf8("whatsapp".to_owned()),
        Value::Null,
        Value::Utf8("gupshup".to_owned()),
        Value::UInt64(1),
        Value::UInt64(2),
        Value::UInt64(3),
        Value::Null,
        Value::Null,
        Value::UInt64(4),
    ]
}

#[test]
#[ignore = "a layout measurement, not a gate"]
// The ratios below are printed to one decimal place; the precision a cast
// to f64 gives up is far below what a printed measurement resolves.
#[allow(clippy::cast_precision_loss)]
fn a_materialized_row_costs_far_more_than_it_carries() {
    const ROWS: usize = 3_200;

    println!("size_of::<Value>()   = {}", size_of::<Value>());
    println!("size_of::<String>()  = {}", size_of::<String>());
    println!("size_of::<Vec<Value>>() = {}", size_of::<Vec<Value>>());
    println!("size_of::<Box<[Value]>>() = {}", size_of::<Box<[Value]>>());
    println!("size_of::<Box<str>>() = {}", size_of::<Box<str>>());

    let clock = Instant::now();
    let rows: Vec<Vec<Value>> = (0..ROWS).map(|_| report_row()).collect();
    let build = clock.elapsed();

    let columns = rows[0].len();
    let inline = columns * size_of::<Value>();
    let heap: usize = rows[0].iter().map(heap_of).sum();
    let payload: usize = rows[0].iter().map(payload_of).sum();
    let vec_header = size_of::<Vec<Value>>();

    println!();
    println!("one row of {columns} columns:");
    println!("  Value structs       = {inline} bytes");
    println!("  heap for its text   = {heap} bytes");
    println!("  Vec header          = {vec_header} bytes");
    println!(
        "  total               = {} bytes",
        inline + heap + vec_header
    );
    println!("  payload it carries  = {payload} bytes");
    println!(
        "  overhead factor     = {:.1}x",
        (inline + heap + vec_header) as f64 / payload as f64
    );
    println!();
    println!(
        "{ROWS} rows: {} KiB, built in {:.2} ms ({:.0} ns per row)",
        (inline + heap + vec_header) * ROWS / 1024,
        build.as_secs_f64() * 1e3,
        build.as_nanos() as f64 / ROWS as f64
    );

    // What the same rows cost when only the fifty a LIMIT keeps are built:
    // the shape today's engine could reach by sorting on keys first.
    let clock = Instant::now();
    let kept: Vec<Vec<Value>> = (0..50).map(|_| report_row()).collect();
    let late = clock.elapsed();
    println!(
        "50 rows: {} KiB, built in {:.2} ms",
        (inline + heap + vec_header) * kept.len() / 1024,
        late.as_secs_f64() * 1e3
    );
    println!(
        "materializing every candidate costs {:.0}x the rows the query returns",
        build.as_secs_f64() / late.as_secs_f64()
    );
    assert_eq!(rows.len(), ROWS);
}

/// The two shapes a paginated report can take, on the numbers the engine
/// actually meets: three thousand candidates, fifty returned.
#[test]
#[ignore = "a layout measurement, not a gate"]
fn sorting_on_keys_before_building_rows_is_the_lever() {
    const ROWS: usize = 3_200;
    const KEPT: usize = 50;
    const RUNS: usize = 200;

    // What the engine does today: every candidate becomes a full row, the
    // sort orders those rows, and all but fifty are dropped.
    let mut eager = f64::MAX;
    for _ in 0..RUNS {
        let clock = Instant::now();
        let mut rows: Vec<Vec<Value>> = (0..ROWS)
            .map(|id| {
                let mut row = report_row();
                row[0] = Value::UInt64(u64::try_from(id).expect("small"));
                row
            })
            .collect();
        rows.sort_by(|left, right| right[0].cmp(&left[0]));
        rows.truncate(KEPT);
        eager = eager.min(clock.elapsed().as_secs_f64());
        assert_eq!(rows.len(), KEPT);
    }

    // What it could do: order the sort keys beside a row identity, keep
    // fifty, and build only those. The keys are the two the report sorts
    // on; the identity is where the row came from.
    let mut late = f64::MAX;
    for _ in 0..RUNS {
        let clock = Instant::now();
        let mut keys: Vec<(u64, usize)> = (0..ROWS)
            .map(|id| (u64::try_from(id).expect("small"), id))
            .collect();
        keys.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        keys.truncate(KEPT);
        let rows: Vec<Vec<Value>> = keys
            .iter()
            .map(|(id, _)| {
                let mut row = report_row();
                row[0] = Value::UInt64(*id);
                row
            })
            .collect();
        late = late.min(clock.elapsed().as_secs_f64());
        assert_eq!(rows.len(), KEPT);
    }

    println!("{ROWS} candidates, {KEPT} returned, minimum of {RUNS} runs:");
    println!("  build every row, then sort   = {:8.3} ms", eager * 1e3);
    println!("  sort keys, then build fifty  = {:8.3} ms", late * 1e3);
    println!("  ratio                        = {:8.1}x", eager / late);
}
