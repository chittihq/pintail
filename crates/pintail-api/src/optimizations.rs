//! What this process is built as and which execution paths it runs: the
//! facts an operator needs to tell one deployment's build from another's.
//!
//! Every value comes from the code that decides it - the pool that was
//! built, the dispatcher's level, the function an operator asks before it
//! takes a path - so the report cannot say one thing while the engine does
//! another. The startup log prints it as two lines and `GET /api/storage`
//! carries it as `optimizations`.

use serde::Serialize;

/// Set at compile time by a profile-guided build (`scripts/pgo-build.sh`):
/// `pgo`, or `pgo+bolt` when the binary was also laid out after linking.
const BUILD_VARIANT: &str = match option_env!("PINTAIL_BUILD_VARIANT") {
    Some(variant) if !variant.is_empty() => variant,
    _ => "standard",
};

/// The instruction-set level the compiler was allowed to assume, from the
/// target features this crate was compiled with.
const BUILD_TARGET: &str = if cfg!(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512cd",
    target_feature = "avx512dq",
    target_feature = "avx512vl"
)) {
    "x86-64-v4"
} else if cfg!(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    target_feature = "bmi2",
    target_feature = "fma"
)) {
    "x86-64-v3"
} else if cfg!(all(
    target_arch = "x86_64",
    target_feature = "sse4.2",
    target_feature = "popcnt"
)) {
    "x86-64-v2"
} else {
    "generic"
};

/// Settings that resize or instrument a path without turning it off. Named
/// in `non_default` when set; their values are not printed.
const TUNING_VARIABLES: [&str; 13] = [
    "PINTAIL_SECONDARY_INDEX_CACHE_MB",
    "PINTAIL_SECONDARY_INDEX_COLUMNS",
    "PINTAIL_LAYER_INDEX_MB",
    "PINTAIL_REPLICA_CACHE_DATABASES",
    "PINTAIL_SNAPSHOT_WORKERS",
    "PINTAIL_PROBE_PREFETCH_ALWAYS",
    "PINTAIL_PROFILE",
    "PINTAIL_QUERY_TRACE",
    "PINTAIL_QUERY_TRACE_JSON",
    "PINTAIL_AGG_DEBUG",
    "PINTAIL_PHASE_TIMING",
    "PINTAIL_DECODE_DEBUG",
    "PINTAIL_SIDE_INDEX_TRACE",
];

/// The processor as the process sees it.
#[derive(Clone, Debug, Serialize)]
pub struct Cpu {
    /// The first `model name` of `/proc/cpuinfo`; `unknown` elsewhere.
    pub model: String,
    /// Logical cores available to the process.
    pub cores: usize,
    /// The instruction sets the processor reports that the engine has a
    /// use for.
    pub features: Vec<&'static str>,
}

/// How the vector kernels dispatch.
#[derive(Clone, Debug, Serialize)]
pub struct Simd {
    /// `baseline`, `avx2` or `avx512`.
    pub level: &'static str,
    /// `PINTAIL_SIMD`, when set.
    pub setting: Option<String>,
}

/// What the binary was compiled as.
#[derive(Clone, Debug, Serialize)]
pub struct Build {
    pub version: &'static str,
    /// `generic`, `x86-64-v2`, `x86-64-v3` or `x86-64-v4`.
    pub target: &'static str,
    /// `standard`, `pgo` or `pgo+bolt`.
    pub variant: &'static str,
    pub debug_assertions: bool,
}

/// One pool's width and what chose it.
#[derive(Clone, Debug, Serialize)]
pub struct Pool {
    pub threads: usize,
    /// `cores`, or the environment variable that set the width.
    pub source: &'static str,
}

/// The two worker pools.
#[derive(Clone, Debug, Serialize)]
pub struct Threads {
    pub scan: Pool,
    pub execute: Pool,
}

/// One switchable execution path.
#[derive(Clone, Debug, Serialize)]
pub struct Path {
    pub name: &'static str,
    pub enabled: bool,
    /// The environment variable that controls it.
    pub variable: &'static str,
}

/// The whole report.
#[derive(Clone, Debug, Serialize)]
pub struct Optimizations {
    pub cpu: Cpu,
    pub simd: Simd,
    pub build: Build,
    pub threads: Threads,
    pub paths: Vec<Path>,
    /// The segment format version this build writes.
    pub segment_format: u8,
    /// Compaction and memtable sizes an environment variable overrides, as
    /// `NAME=value`.
    pub size_overrides: Vec<String>,
    /// Every setting that moves this process off its defaults: a path
    /// turned off, a pool resized, a diagnostic switched on.
    pub non_default: Vec<String>,
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(target_arch = "x86_64")]
fn cpu_features() -> Vec<&'static str> {
    [
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
        ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ("avx512bw", std::arch::is_x86_feature_detected!("avx512bw")),
        ("avx512vl", std::arch::is_x86_feature_detected!("avx512vl")),
        ("avx512dq", std::arch::is_x86_feature_detected!("avx512dq")),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

#[cfg(target_arch = "aarch64")]
fn cpu_features() -> Vec<&'static str> {
    if std::arch::is_aarch64_feature_detected!("neon") {
        vec!["neon"]
    } else {
        Vec::new()
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn cpu_features() -> Vec<&'static str> {
    Vec::new()
}

/// Reads the report from the running process.
#[must_use]
pub fn optimizations() -> Optimizations {
    let (level, setting) = pintail_store::simd_dispatch();
    let scan_setting = pintail_store::scan_threads_setting();
    let (execute_threads, execute_overridden) = pintail_exec::parallel_pool_threads();
    let mut paths = vec![
        Path {
            name: "inline_statements",
            enabled: pintail_wire::inline_statements(),
            variable: "PINTAIL_INLINE_STATEMENTS",
        },
        Path {
            name: "shared_queries",
            enabled: pintail_wire::shared_queries_enabled(),
            variable: "PINTAIL_DISABLE_SHARED_QUERIES",
        },
        Path {
            name: "secondary_index",
            enabled: pintail_store::side_index_enabled(),
            variable: "PINTAIL_SECONDARY_INDEX",
        },
    ];
    paths.extend(pintail_exec::path_switches().map(|switch| Path {
        name: switch.name,
        enabled: switch.enabled,
        variable: switch.variable,
    }));

    let (memtable_bytes, compaction_input_rows, compaction_output_rows) =
        pintail_store::size_overrides();
    let size_overrides: Vec<String> = [
        memtable_bytes.map(|bytes| format!("PINTAIL_MEMTABLE_KB={}", bytes / 1024)),
        compaction_input_rows.map(|rows| format!("PINTAIL_COMPACTION_INPUT_ROWS={rows}")),
        compaction_output_rows.map(|rows| format!("PINTAIL_COMPACTION_OUTPUT_ROWS={rows}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut non_default = Vec::new();
    if let Some(setting) = &setting {
        non_default.push(format!("PINTAIL_SIMD={setting}"));
    }
    if let Some(threads) = scan_setting {
        non_default.push(format!("PINTAIL_SCAN_THREADS={threads}"));
    }
    if execute_overridden {
        non_default.push(format!("RAYON_NUM_THREADS={execute_threads}"));
    }
    non_default.extend(
        paths
            .iter()
            .filter(|path| !path.enabled)
            .map(|path| path.variable.to_owned()),
    );
    non_default.extend(size_overrides.iter().cloned());
    non_default.extend(
        TUNING_VARIABLES
            .iter()
            .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
            .map(|name| (*name).to_owned()),
    );

    Optimizations {
        cpu: Cpu {
            model: cpu_model(),
            cores: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            features: cpu_features(),
        },
        simd: Simd { level, setting },
        build: Build {
            version: env!("CARGO_PKG_VERSION"),
            target: BUILD_TARGET,
            variant: BUILD_VARIANT,
            debug_assertions: cfg!(debug_assertions),
        },
        threads: Threads {
            scan: Pool {
                threads: pintail_store::projected_scan_width(),
                source: if scan_setting.is_some() {
                    "PINTAIL_SCAN_THREADS"
                } else {
                    "cores"
                },
            },
            execute: Pool {
                threads: execute_threads,
                source: if execute_overridden {
                    "RAYON_NUM_THREADS"
                } else {
                    "cores"
                },
            },
        },
        paths,
        segment_format: pintail_store::WRITTEN_SEGMENT_FORMAT,
        size_overrides,
        non_default,
    }
}

impl Optimizations {
    /// The report as the two startup log lines: what the build and the
    /// machine are, then which paths run. Stable `key=value` pairs.
    #[must_use]
    pub fn log_lines(&self) -> [String; 2] {
        let on = |enabled: bool| if enabled { "on" } else { "off" };
        let list = |items: &[String]| format!("[{}]", items.join(","));
        let features = if self.cpu.features.is_empty() {
            "none".to_owned()
        } else {
            self.cpu.features.join(",")
        };
        let machine = format!(
            "pintail optimizations: cpu_model=\"{}\" cpu_cores={} cpu_features={features} \
             simd={} simd_setting={} build_version={} build_target={} build_variant={} \
             debug_assertions={}",
            self.cpu.model,
            self.cpu.cores,
            self.simd.level,
            self.simd.setting.as_deref().unwrap_or("unset"),
            self.build.version,
            self.build.target,
            self.build.variant,
            on(self.build.debug_assertions),
        );
        let paths = self
            .paths
            .iter()
            .map(|path| format!("{}={}", path.name, on(path.enabled)))
            .collect::<Vec<_>>()
            .join(" ");
        let running = format!(
            "pintail paths: scan_threads={} scan_threads_from={} execute_threads={} \
             execute_threads_from={} {paths} segment_format={} size_overrides={} non_default={}",
            self.threads.scan.threads,
            self.threads.scan.source,
            self.threads.execute.threads,
            self.threads.execute.source,
            self.segment_format,
            list(&self.size_overrides),
            list(&self.non_default),
        );
        [machine, running]
    }
}

#[cfg(test)]
mod tests {
    use super::optimizations;

    /// Set for the child process of the test below, which prints its
    /// report and nothing else.
    const CHILD: &str = "PINTAIL_OPTIMIZATIONS_TEST_CHILD";

    const KEYS: [&str; 22] = [
        "cpu_model=",
        "cpu_cores=",
        "cpu_features=",
        "simd=",
        "simd_setting=",
        "build_version=",
        "build_target=",
        "build_variant=",
        "debug_assertions=",
        "scan_threads=",
        "scan_threads_from=",
        "execute_threads=",
        "execute_threads_from=",
        "inline_statements=",
        "shared_queries=",
        "secondary_index=",
        "settled_memo=",
        "packed_group=",
        "grouped_fold=",
        "argument_projection=",
        "segment_format=",
        "non_default=",
    ];

    #[test]
    fn the_startup_lines_carry_every_key_and_name_no_setting_by_default() {
        if std::env::var_os(CHILD).is_some() {
            let report = optimizations();
            for line in report.log_lines() {
                println!("{line}");
            }
            println!("{}", serde_json::to_string(&report).expect("json"));
            return;
        }
        let lines = optimizations().log_lines().join("\n");
        for key in KEYS {
            assert!(lines.contains(key), "{key} is missing from {lines}");
        }
        assert!(lines.contains("build_variant=standard"), "{lines}");
        assert!(lines.starts_with("pintail optimizations: cpu_model=\""));
        assert!(lines.contains("\npintail paths: scan_threads="), "{lines}");
    }

    /// The environment of a running test cannot be changed without
    /// `unsafe`, and the dispatcher reads its setting once, so the settings
    /// are given to a second copy of this test binary.
    #[test]
    fn a_setting_that_turns_a_path_off_is_reported_and_listed_as_non_default() {
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "optimizations::tests::the_startup_lines_carry_every_key_and_name_no_setting_by_default",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("PINTAIL_DISABLE_SETTLED_MEMO")
            .env("PINTAIL_SIMD", "off")
            .env("PINTAIL_DISABLE_PACKED_GROUP", "1")
            .env("PINTAIL_INLINE_STATEMENTS", "0")
            .env("PINTAIL_SCAN_THREADS", "3")
            .output()
            .expect("run the child");
        let output = String::from_utf8_lossy(&child.stdout);
        assert!(child.status.success(), "{output}");
        let machine = output
            .lines()
            .find(|line| line.starts_with("pintail optimizations:"))
            .expect("the first line");
        let paths = output
            .lines()
            .find(|line| line.starts_with("pintail paths:"))
            .expect("the second line");
        assert!(
            machine.contains(" simd=baseline simd_setting=off "),
            "{machine}"
        );
        assert!(
            paths.contains(" scan_threads=3 scan_threads_from=PINTAIL_SCAN_THREADS "),
            "{paths}"
        );
        assert!(paths.contains(" packed_group=off "), "{paths}");
        assert!(paths.contains(" inline_statements=off "), "{paths}");
        assert!(paths.contains(" settled_memo=on "), "{paths}");
        let non_default = paths.split_once("non_default=").expect("the list").1;
        for setting in [
            "PINTAIL_SIMD=off",
            "PINTAIL_SCAN_THREADS=3",
            "PINTAIL_DISABLE_PACKED_GROUP",
            "PINTAIL_INLINE_STATEMENTS",
        ] {
            assert!(
                non_default.contains(setting),
                "{setting} not in {non_default}"
            );
        }
        assert!(
            !non_default.contains("PINTAIL_DISABLE_SETTLED_MEMO"),
            "{non_default}"
        );

        let json: serde_json::Value = serde_json::from_str(
            output
                .lines()
                .find(|line| line.starts_with('{'))
                .expect("the json line"),
        )
        .expect("json");
        assert_eq!(json["simd"]["level"], "baseline");
        assert_eq!(json["threads"]["scan"]["threads"], 3);
        assert!(
            json["non_default"]
                .as_array()
                .expect("a list")
                .iter()
                .any(|entry| entry == "PINTAIL_DISABLE_PACKED_GROUP")
        );
    }
}
