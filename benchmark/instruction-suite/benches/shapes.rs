//! One instruction count per query shape.
//!
//! Each case runs in its own process: the fixture is loaded and the
//! statement run once with instrumentation off, so neither the load nor
//! anything initialized on first use is counted, and then the statement is
//! run once more between the two instrumentation requests. The region is
//! process-wide, which matters: scans run on the store's own pool, and a
//! region toggled on one function would see only the calling thread.
use std::hint::black_box;

use iai_callgrind::client_requests::callgrind::{start_instrumentation, stop_instrumentation};
use iai_callgrind::{
    Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use pintail_instruction_suite::{Fixture, case};

#[library_benchmark]
#[bench::q1_count("q1_count")]
#[bench::q2_filtered_count("q2_filtered_count")]
#[bench::q3_group_status("q3_group_status")]
#[bench::q4_region_status("q4_region_status")]
#[bench::q5_monthly("q5_monthly")]
#[bench::q6_top_spenders("q6_top_spenders")]
#[bench::q7_regional("q7_regional")]
#[bench::q8_join_users("q8_join_users")]
#[bench::n1_filtered_count_bounded("n1_filtered_count_bounded")]
#[bench::n2_group_region("n2_group_region")]
#[bench::n3_monthly_other_year("n3_monthly_other_year")]
#[bench::n4_regional_other_range("n4_regional_other_range")]
#[bench::text_filter_two_columns("text_filter_two_columns")]
#[bench::many_groups("many_groups")]
#[bench::star_join("star_join")]
#[bench::count_distinct("count_distinct")]
#[bench::order_limit("order_limit")]
#[bench::correlated_scalar_page("correlated_scalar_page")]
#[bench::window_day("window_day")]
#[bench::window_hour("window_hour")]
#[bench::zoned_window_day("zoned_window_day")]
#[bench::zoned_window_formatted_day("zoned_window_formatted_day")]
#[bench::zoned_window_hour("zoned_window_hour")]
#[bench::zoned_since_day("zoned_since_day")]
#[bench::zoned_all_days("zoned_all_days")]
#[bench::zoned_named_zone_hour("zoned_named_zone_hour")]
#[bench::datetime_groups("datetime_groups")]
#[bench::datetime_distinct_days("datetime_distinct_days")]
#[bench::derived_datetime_groups("derived_datetime_groups")]
fn shape(name: &str) -> usize {
    let case = case(name);
    let fixture = Fixture::build(case.tables);
    let warm = fixture.run(case);
    start_instrumentation();
    let answer = black_box(fixture.run(black_box(case)));
    stop_instrumentation();
    assert_eq!(
        answer, warm,
        "{name} answered differently on its second run"
    );
    answer.len()
}

library_benchmark_group!(name = shapes; benchmarks = shape);

main!(
    config = LibraryBenchmarkConfig::default()
        .tool(
            Callgrind::with_args(["--instr-atstart=no", "--cache-sim=yes"])
                .entry_point(EntryPoint::None)
        )
        // One worker in each pool: with more, which thread takes which
        // piece of work varies from run to run, and so does the count.
        .env("RAYON_NUM_THREADS", "1")
        .env("PINTAIL_SCAN_THREADS", "1")
        .env("PINTAIL_DISABLE_SETTLED_MEMO", "1")
        // One allocator arena: with one per thread, which addresses a buffer
        // gets depends on which thread allocated first, and the C library's
        // copy routines take different paths for different alignments.
        .env("MALLOC_ARENA_MAX", "1");
    library_benchmark_groups = shapes
);
