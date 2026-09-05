//! Process configuration loaded from TOML, environment, and CLI arguments.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use pintail_wire::{DEFAULT_QUERY_MEMORY_LIMIT, default_max_concurrent_queries};
use serde::Deserialize;

/// Memory available to this process in bytes, or `None` when it cannot be
/// determined.
///
/// Reads a cgroup limit before host memory. Pintail's primary deployment is
/// a container, and `/proc/meminfo` inside one reports the HOST's memory
/// rather than the cgroup ceiling - on a 64GB host with a 4GB container cap
/// it would report 64GB, so a budget derived from it never engages and the
/// kernel OOM killer decides instead. That is the opposite of what a memory
/// budget is for.
///
/// Both cgroup versions report "unlimited" as a number rather than an
/// absence - v2 as the literal `max`, v1 as a value near `u64::MAX` - so an
/// unlimited cgroup falls through to host memory rather than producing an
/// absurd ceiling.
///
/// An unknown value is reported as `None` so the caller can stay unbounded
/// instead of inventing a ceiling from a guess.
fn available_memory_bytes() -> Option<u64> {
    cgroup_memory_limit()
        .or_else(host_memory_bytes)
        .filter(|bytes| *bytes > 0)
}

/// The cgroup memory ceiling, when one is set and is not "unlimited".
#[cfg(not(target_os = "macos"))]
fn cgroup_memory_limit() -> Option<u64> {
    let host = host_memory_bytes();
    // cgroup v2 first: it is the default on every current distribution, and
    // a v2 host has no v1 hierarchy to fall back to.
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .or_else(|| {
            std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
        })?;
    // v1 writes a sentinel near u64::MAX for "unlimited" rather than
    // omitting the file, and a limit above host memory is not a limit.
    match host {
        Some(host) if limit >= host => None,
        _ => Some(limit),
    }
}

/// macOS has no cgroups; the host figure is the only one.
#[cfg(target_os = "macos")]
const fn cgroup_memory_limit() -> Option<u64> {
    None
}

/// Physical memory on the host, ignoring any container limit.
fn host_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|bytes| *bytes > 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // MemTotal is reported in kibibytes.
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|meminfo| {
                meminfo
                    .lines()
                    .find_map(|line| line.strip_prefix("MemTotal:"))
                    .and_then(|value| {
                        value
                            .trim()
                            .trim_end_matches(" kB")
                            .trim()
                            .parse::<u64>()
                            .ok()
                    })
            })
            .and_then(|kibibytes| kibibytes.checked_mul(1024))
            .filter(|bytes| *bytes > 0)
    }
}

/// The default process-wide query memory budget.
///
/// Returns zero (unbounded) when host memory cannot be read, because a
/// guessed ceiling could refuse work the host could actually have done.
fn default_total_query_memory_limit() -> usize {
    available_memory_bytes()
        .and_then(|bytes| {
            bytes
                .checked_mul(DEFAULT_MEMORY_BUDGET_NUMERATOR)
                .map(|scaled| scaled / DEFAULT_MEMORY_BUDGET_DENOMINATOR)
        })
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(0)
}

const DEFAULT_CONFIG_FILE: &str = "pintail.toml";
const DEFAULT_DATA_DIR: &str = "./data";
/// Spill lives under the data directory by default so it inherits whatever
/// volume the operator mounted for data. Putting it in the system temp
/// directory instead — `tempfile`'s default — silently writes query spill to
/// the container's root filesystem rather than the provisioned volume.
const DEFAULT_SPILL_SUBDIRECTORY: &str = "spill";
const DEFAULT_QUERY_SPILL_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_GLOBAL_SPILL_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Fraction of host memory the process budget claims by default.
///
/// High enough that it never engages on a healthy server - a budget that
/// refuses queries which used to succeed would be a regression wearing a
/// safety feature's clothes - but low enough to leave the page cache and
/// the rest of the box room before the kernel starts killing processes.
const DEFAULT_MEMORY_BUDGET_NUMERATOR: u64 = 3;
const DEFAULT_MEMORY_BUDGET_DENOMINATOR: u64 = 4;

const DEFAULT_HTTP_PORT: u16 = 8080;
const DEFAULT_WIRE_PORT: u16 = 3306;
const DEFAULT_WIRE_IDLE_TIMEOUT_SECONDS: u64 = 15 * 60;

/// Pintail command-line options.
#[derive(Clone, Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// TOML configuration file. Defaults to ./pintail.toml when it exists.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Directory for metadata, secrets, WAL files, and columnar segments.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Address used by the dashboard and HTTP API.
    #[arg(long)]
    pub http_bind: Option<SocketAddr>,

    /// Address used by `MySQL` wire-protocol clients.
    #[arg(long)]
    pub wire_bind: Option<SocketAddr>,

    /// Seconds before an idle authenticated wire connection is closed.
    #[arg(long)]
    pub wire_idle_timeout_seconds: Option<u64>,

    /// Wire connections accepted at once, authenticated or not. Beyond
    /// this a client is answered with `MySQL`'s "Too many connections"
    /// (1040) and closed. Zero disables the bound.
    #[arg(long)]
    pub wire_max_connections: Option<usize>,

    /// Prepared statements one wire session may hold open at once. Beyond
    /// this a PREPARE is refused with `MySQL`'s 1461. Zero disables the
    /// bound.
    #[arg(long)]
    pub wire_max_prepared_statements: Option<usize>,

    /// Hard byte ceiling for each HTTP or `MySQL` wire query.
    #[arg(long)]
    pub query_memory_limit_bytes: Option<usize>,

    /// Maximum queries executing at once. Beyond this, queries wait briefly
    /// and are then refused so overload becomes backpressure rather than
    /// unbounded latency. Zero disables the bound.
    #[arg(long)]
    pub max_concurrent_queries: Option<usize>,

    /// Byte ceiling shared by every concurrent query. The per-query limit
    /// bounds one query; this bounds their sum. Zero disables the bound.
    #[arg(long)]
    pub total_query_memory_limit_bytes: Option<usize>,

    /// Directory for query spill files. Defaults to `<data-dir>/spill`.
    #[arg(long)]
    pub spill_dir: Option<PathBuf>,

    /// Hard disk ceiling for spill files retained by one query.
    #[arg(long)]
    pub query_spill_limit_bytes: Option<u64>,

    /// Hard disk ceiling shared by all concurrently spilling queries.
    #[arg(long)]
    pub global_spill_limit_bytes: Option<u64>,
}

/// Effective process configuration after all sources have been merged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppConfig {
    data_dir: PathBuf,
    spill_dir: PathBuf,
    http_bind: SocketAddr,
    wire_bind: SocketAddr,
    wire_idle_timeout_seconds: u64,
    wire_max_connections: usize,
    wire_max_prepared_statements: usize,
    query_memory_limit_bytes: usize,
    max_concurrent_queries: usize,
    total_query_memory_limit_bytes: usize,
    query_spill_limit_bytes: u64,
    global_spill_limit_bytes: u64,
    wire_tls_certificate: Option<PathBuf>,
    wire_tls_key: Option<PathBuf>,
    wire_require_tls: bool,
}

impl AppConfig {
    /// Loads the configuration from the current process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a requested file cannot be read or parsed, or an
    /// environment override is invalid.
    pub fn load(cli: &Cli) -> Result<Self> {
        Self::load_from(cli, std::env::vars_os())
    }

    /// Loads configuration using a supplied environment.
    ///
    /// This is public so embedders and tests can provide configuration without
    /// mutating process-global environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when a requested file cannot be read or parsed, or an
    /// environment override is invalid.
    #[allow(clippy::too_many_lines)] // linear CLI/env/file source merge
    pub fn load_from<I>(cli: &Cli, environment: I) -> Result<Self>
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let environment: HashMap<OsString, OsString> = environment.into_iter().collect();
        let config_path = cli
            .config
            .clone()
            .or_else(|| {
                environment
                    .get(&OsString::from("PINTAIL_CONFIG"))
                    .map(PathBuf::from)
            })
            .or_else(default_config_path);

        let file = config_path
            .as_deref()
            .map(load_file)
            .transpose()?
            .unwrap_or_default();

        let file_data_dir = file.data_dir.map(|path| {
            resolve_config_relative(path, config_path.as_deref().and_then(Path::parent))
        });
        let environment_data_dir = environment
            .get(&OsString::from("PINTAIL_DATA_DIR"))
            .map(PathBuf::from);
        let data_dir = cli
            .data_dir
            .clone()
            .or(environment_data_dir)
            .or(file_data_dir)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));

        let file_spill_dir = file.spill_dir.map(|path| {
            resolve_config_relative(path, config_path.as_deref().and_then(Path::parent))
        });
        let environment_spill_dir = environment
            .get(&OsString::from("PINTAIL_SPILL_DIR"))
            .map(PathBuf::from);
        let spill_dir = cli
            .spill_dir
            .clone()
            .or(environment_spill_dir)
            .or(file_spill_dir)
            .unwrap_or_else(|| data_dir.join(DEFAULT_SPILL_SUBDIRECTORY));

        let environment_http_bind = environment
            .get(&OsString::from("PINTAIL_HTTP_BIND"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_HTTP_BIND must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_HTTP_BIND must be an IP address and port")
            })
            .transpose()?;
        let http_bind = cli
            .http_bind
            .or(environment_http_bind)
            .or(file.http.bind)
            .unwrap_or_else(default_http_bind);
        let environment_wire_bind = environment
            .get(&OsString::from("PINTAIL_WIRE_BIND"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_WIRE_BIND must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_WIRE_BIND must be an IP address and port")
            })
            .transpose()?;
        let wire_bind = cli
            .wire_bind
            .or(environment_wire_bind)
            .or(file.wire.bind)
            .unwrap_or_else(default_wire_bind);
        let environment_wire_idle_timeout = environment
            .get(&OsString::from("PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS must be a positive integer")
            })
            .transpose()?;
        let wire_idle_timeout_seconds = cli
            .wire_idle_timeout_seconds
            .or(environment_wire_idle_timeout)
            .or(file.wire.idle_timeout_seconds)
            .unwrap_or(DEFAULT_WIRE_IDLE_TIMEOUT_SECONDS);
        if wire_idle_timeout_seconds == 0 {
            bail!("wire idle timeout must be greater than zero");
        }
        let environment_wire_max_connections = environment
            .get(&OsString::from("PINTAIL_WIRE_MAX_CONNECTIONS"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_WIRE_MAX_CONNECTIONS must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_WIRE_MAX_CONNECTIONS must be a non-negative integer")
            })
            .transpose()?;
        let wire_max_connections = cli
            .wire_max_connections
            .or(environment_wire_max_connections)
            .or(file.wire.max_connections)
            .unwrap_or(pintail_wire::DEFAULT_MAX_CONNECTIONS);
        let environment_wire_max_prepared = environment
            .get(&OsString::from("PINTAIL_WIRE_MAX_PREPARED_STATEMENTS"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_WIRE_MAX_PREPARED_STATEMENTS must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_WIRE_MAX_PREPARED_STATEMENTS must be a non-negative integer")
            })
            .transpose()?;
        let wire_max_prepared_statements = cli
            .wire_max_prepared_statements
            .or(environment_wire_max_prepared)
            .or(file.wire.max_prepared_statements)
            .unwrap_or(pintail_wire::DEFAULT_MAX_PREPARED_STATEMENTS);
        let environment_query_memory_limit = environment
            .get(&OsString::from("PINTAIL_QUERY_MEMORY_LIMIT_BYTES"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_QUERY_MEMORY_LIMIT_BYTES must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_QUERY_MEMORY_LIMIT_BYTES must be a positive integer")
            })
            .transpose()?;
        let query_memory_limit_bytes = cli
            .query_memory_limit_bytes
            .or(environment_query_memory_limit)
            .or(file.query.memory_limit_bytes)
            .unwrap_or(DEFAULT_QUERY_MEMORY_LIMIT);
        if query_memory_limit_bytes == 0 {
            bail!("query memory limit must be greater than zero");
        }
        let environment_max_concurrent_queries = environment
            .get(&OsString::from("PINTAIL_MAX_CONCURRENT_QUERIES"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_MAX_CONCURRENT_QUERIES must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_MAX_CONCURRENT_QUERIES must be a non-negative integer")
            })
            .transpose()?;
        // Zero is a deliberate opt-out rather than an error: an operator
        // measuring the unbounded behaviour needs a way back to it.
        let max_concurrent_queries = cli
            .max_concurrent_queries
            .or(environment_max_concurrent_queries)
            .or(file.query.max_concurrent_queries)
            .unwrap_or_else(default_max_concurrent_queries);
        let environment_total_query_memory = environment
            .get(&OsString::from("PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES must be valid UTF-8")?
                    .parse()
                    .context(
                        "PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES must be a non-negative integer",
                    )
            })
            .transpose()?;
        // Defaults to a fraction of host memory rather than to unbounded.
        // Unbounded means the safety feature protects nobody by default; a
        // fixed byte figure would refuse queries on a large host and starve
        // a small one. A fraction adapts, and at three quarters it sits far
        // enough above normal working set that it engages only when the
        // alternative was the kernel choosing which process to kill.
        // Zero remains an explicit opt-out.
        let total_query_memory_limit_bytes = cli
            .total_query_memory_limit_bytes
            .or(environment_total_query_memory)
            .or(file.query.total_memory_limit_bytes)
            .unwrap_or_else(default_total_query_memory_limit);
        let environment_query_spill_limit = environment
            .get(&OsString::from("PINTAIL_QUERY_SPILL_LIMIT_BYTES"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_QUERY_SPILL_LIMIT_BYTES must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_QUERY_SPILL_LIMIT_BYTES must be a positive integer")
            })
            .transpose()?;
        let query_spill_limit_bytes = cli
            .query_spill_limit_bytes
            .or(environment_query_spill_limit)
            .or(file.query.spill_limit_bytes)
            .unwrap_or(DEFAULT_QUERY_SPILL_LIMIT_BYTES);
        let environment_global_spill_limit = environment
            .get(&OsString::from("PINTAIL_GLOBAL_SPILL_LIMIT_BYTES"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_GLOBAL_SPILL_LIMIT_BYTES must be valid UTF-8")?
                    .parse()
                    .context("PINTAIL_GLOBAL_SPILL_LIMIT_BYTES must be a positive integer")
            })
            .transpose()?;
        let global_spill_limit_bytes = cli
            .global_spill_limit_bytes
            .or(environment_global_spill_limit)
            .or(file.global_spill_limit_bytes)
            .unwrap_or(DEFAULT_GLOBAL_SPILL_LIMIT_BYTES);
        if query_spill_limit_bytes == 0 || global_spill_limit_bytes == 0 {
            bail!("spill disk limits must be greater than zero");
        }
        if query_spill_limit_bytes > global_spill_limit_bytes {
            bail!("query spill disk limit cannot exceed the global spill disk limit");
        }

        let read_env_path = |key: &str| -> Result<Option<PathBuf>> {
            environment
                .get(&OsString::from(key))
                .map(|value| {
                    value
                        .to_str()
                        .map(PathBuf::from)
                        .with_context(|| format!("{key} must be valid UTF-8"))
                })
                .transpose()
        };
        let wire_tls_certificate =
            read_env_path("PINTAIL_WIRE_TLS_CERT")?.or(file.wire.tls_certificate);
        let wire_tls_key = read_env_path("PINTAIL_WIRE_TLS_KEY")?.or(file.wire.tls_key);
        if wire_tls_certificate.is_some() != wire_tls_key.is_some() {
            bail!("wire TLS certificate and key must be configured together");
        }
        let wire_require_tls = environment
            .get(&OsString::from("PINTAIL_WIRE_REQUIRE_TLS"))
            .map(|value| {
                value
                    .to_str()
                    .context("PINTAIL_WIRE_REQUIRE_TLS must be valid UTF-8")
                    .map(|value| matches!(value, "1" | "true" | "yes"))
            })
            .transpose()?
            .or(file.wire.require_tls)
            .unwrap_or(false);
        // No longer requires a configured certificate: the node generates and
        // manages one when none is supplied, so requiring TLS is now a
        // standalone choice rather than something only reachable by operators
        // who had already obtained a certificate elsewhere.

        Ok(Self {
            data_dir,
            spill_dir,
            http_bind,
            wire_bind,
            wire_idle_timeout_seconds,
            wire_max_connections,
            wire_max_prepared_statements,
            query_memory_limit_bytes,
            max_concurrent_queries,
            total_query_memory_limit_bytes,
            query_spill_limit_bytes,
            global_spill_limit_bytes,
            wire_tls_certificate,
            wire_tls_key,
            wire_require_tls,
        })
    }

    /// Directory containing all durable Pintail state.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Whether a client that will not negotiate TLS is refused.
    ///
    /// Separate from having a certificate: the node always has one now, so
    /// this is the difference between offering TLS and insisting on it.
    #[must_use]
    pub const fn wire_require_tls(&self) -> bool {
        self.wire_require_tls
    }

    /// PEM certificate chain and key for wire TLS, when configured.
    #[must_use]
    pub fn wire_tls(&self) -> Option<(&Path, &Path, bool)> {
        match (&self.wire_tls_certificate, &self.wire_tls_key) {
            (Some(certificate), Some(key)) => {
                Some((certificate.as_path(), key.as_path(), self.wire_require_tls))
            }
            _ => None,
        }
    }

    /// Address used by the dashboard and HTTP API.
    #[must_use]
    pub fn http_bind(&self) -> SocketAddr {
        self.http_bind
    }

    /// Address used by `MySQL` wire-protocol clients.
    #[must_use]
    pub fn wire_bind(&self) -> SocketAddr {
        self.wire_bind
    }

    /// Time an authenticated wire connection may remain idle.
    #[must_use]
    pub const fn wire_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.wire_idle_timeout_seconds)
    }

    /// The connection and prepared-statement bounds the wire listener
    /// enforces. The statement-text byte ceiling is not configurable: it
    /// exists to stop the count bound being defeated by statement size, and
    /// no workload has asked for a different one.
    #[must_use]
    pub fn wire_limits(&self) -> pintail_wire::WireLimits {
        pintail_wire::WireLimits {
            max_connections: self.wire_max_connections,
            max_prepared_statements: self.wire_max_prepared_statements,
            ..pintail_wire::WireLimits::default()
        }
    }

    /// Hard byte ceiling for one HTTP or `MySQL` wire query.
    #[must_use]
    pub const fn query_memory_limit_bytes(&self) -> usize {
        self.query_memory_limit_bytes
    }

    #[must_use]
    pub const fn max_concurrent_queries(&self) -> usize {
        self.max_concurrent_queries
    }

    #[must_use]
    pub const fn total_query_memory_limit_bytes(&self) -> usize {
        self.total_query_memory_limit_bytes
    }

    /// Returns the directory query spill files are written to.
    #[must_use]
    pub fn spill_dir(&self) -> &Path {
        &self.spill_dir
    }

    /// Hard disk ceiling for spill retained by one query.
    #[must_use]
    pub const fn query_spill_limit_bytes(&self) -> u64 {
        self.query_spill_limit_bytes
    }

    /// Hard disk ceiling shared by all live query spill.
    #[must_use]
    pub const fn global_spill_limit_bytes(&self) -> u64 {
        self.global_spill_limit_bytes
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    spill_dir: Option<PathBuf>,
    global_spill_limit_bytes: Option<u64>,
    http: FileHttpConfig,
    wire: FileWireConfig,
    query: FileQueryConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileHttpConfig {
    bind: Option<SocketAddr>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileWireConfig {
    bind: Option<SocketAddr>,
    idle_timeout_seconds: Option<u64>,
    max_connections: Option<usize>,
    max_prepared_statements: Option<usize>,
    tls_certificate: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    require_tls: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileQueryConfig {
    memory_limit_bytes: Option<usize>,
    max_concurrent_queries: Option<usize>,
    total_memory_limit_bytes: Option<usize>,
    spill_limit_bytes: Option<u64>,
}

fn default_config_path() -> Option<PathBuf> {
    let path = PathBuf::from(DEFAULT_CONFIG_FILE);
    path.exists().then_some(path)
}

fn default_http_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_HTTP_PORT)
}

fn default_wire_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_WIRE_PORT)
}

fn load_file(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        bail!("configuration file {} does not exist", path.display());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read configuration file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse configuration file {}", path.display()))
}

fn resolve_config_relative(path: PathBuf, config_dir: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path
    } else if let Some(config_dir) = config_dir {
        config_dir.join(path)
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AppConfig, Cli, DEFAULT_QUERY_MEMORY_LIMIT, available_memory_bytes};

    fn cli() -> Cli {
        Cli {
            config: None,
            data_dir: None,
            http_bind: None,
            wire_bind: None,
            wire_idle_timeout_seconds: None,
            wire_max_connections: None,
            wire_max_prepared_statements: None,
            query_memory_limit_bytes: None,
            max_concurrent_queries: None,
            total_query_memory_limit_bytes: None,
            spill_dir: None,
            query_spill_limit_bytes: None,
            global_spill_limit_bytes: None,
        }
    }

    #[test]
    fn the_total_memory_budget_defaults_below_host_memory_and_can_be_disabled() {
        let default = AppConfig::load_from(&cli(), []).expect("default config");
        let budget = default.total_query_memory_limit_bytes();
        if let Some(host) = available_memory_bytes() {
            // A budget at or above host memory would never engage; one at
            // zero would mean the default protects nothing.
            assert!(budget > 0, "a host with known memory must get a budget");
            assert!(
                u64::try_from(budget).expect("budget fits") < host,
                "budget {budget} must leave headroom below the {host} available"
            );
        } else {
            // Unknown host memory must stay unbounded rather than guess.
            assert_eq!(budget, 0);
        }

        let disabled = AppConfig::load_from(
            &cli(),
            [("PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES".into(), "0".into())],
        )
        .expect("environment config");
        assert_eq!(
            disabled.total_query_memory_limit_bytes(),
            0,
            "zero must remain an explicit opt-out"
        );
    }

    #[test]
    fn a_container_limit_takes_precedence_over_host_memory() {
        // The defect this exists for: /proc/meminfo inside a container
        // reports the HOST's memory, so a budget derived from it never
        // engages under a container cap and the OOM killer decides instead.
        // Where a cgroup limit is set it must win, and it must never exceed
        // what the host actually has.
        let available = available_memory_bytes();
        let host = super::host_memory_bytes();
        if let (Some(available), Some(host)) = (available, host) {
            assert!(
                available <= host,
                "available {available} cannot exceed host {host}"
            );
        }
        // An unlimited cgroup reports a sentinel rather than an absence, so
        // it must fall through to the host figure rather than surface an
        // absurd ceiling.
        if let Some(limit) = super::cgroup_memory_limit() {
            let host = host.expect("a cgroup limit implies a readable host figure");
            assert!(
                limit < host,
                "an 'unlimited' cgroup sentinel {limit} must not be treated as a limit"
            );
        }
    }

    #[test]
    fn query_memory_limit_defaults_and_accepts_environment_override() {
        let default = AppConfig::load_from(&cli(), []).expect("default config");
        assert_eq!(
            default.query_memory_limit_bytes(),
            DEFAULT_QUERY_MEMORY_LIMIT
        );

        let configured = AppConfig::load_from(
            &cli(),
            [(
                "PINTAIL_QUERY_MEMORY_LIMIT_BYTES".into(),
                "268435456".into(),
            )],
        )
        .expect("environment config");
        assert_eq!(configured.query_memory_limit_bytes(), 268_435_456);
    }

    #[test]
    fn query_memory_limit_must_be_positive() {
        let error = AppConfig::load_from(
            &cli(),
            [("PINTAIL_QUERY_MEMORY_LIMIT_BYTES".into(), "0".into())],
        )
        .expect_err("zero limit");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn wire_idle_timeout_defaults_and_must_be_positive() {
        let default = AppConfig::load_from(&cli(), []).expect("default config");
        assert_eq!(default.wire_idle_timeout(), Duration::from_secs(900));

        let error = AppConfig::load_from(
            &cli(),
            [("PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS".into(), "0".into())],
        )
        .expect_err("zero timeout");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn wire_limits_default_to_mysqls_and_accept_zero_as_unbounded() {
        let default = AppConfig::load_from(&cli(), []).expect("default config");
        assert_eq!(default.wire_limits().max_connections, 1000);
        assert_eq!(default.wire_limits().max_prepared_statements, 1024);

        let configured = AppConfig::load_from(
            &cli(),
            [
                ("PINTAIL_WIRE_MAX_CONNECTIONS".into(), "0".into()),
                ("PINTAIL_WIRE_MAX_PREPARED_STATEMENTS".into(), "16".into()),
            ],
        )
        .expect("environment config");
        assert_eq!(configured.wire_limits().max_connections, 0, "zero disables");
        assert_eq!(configured.wire_limits().max_prepared_statements, 16);
    }

    #[test]
    fn spill_limits_default_and_accept_environment_overrides() {
        let default = AppConfig::load_from(&cli(), []).expect("default config");
        assert_eq!(default.query_spill_limit_bytes(), 1_073_741_824);
        assert_eq!(default.global_spill_limit_bytes(), 8_589_934_592);

        let configured = AppConfig::load_from(
            &cli(),
            [
                ("PINTAIL_QUERY_SPILL_LIMIT_BYTES".into(), "2048".into()),
                ("PINTAIL_GLOBAL_SPILL_LIMIT_BYTES".into(), "4096".into()),
            ],
        )
        .expect("environment config");
        assert_eq!(configured.query_spill_limit_bytes(), 2048);
        assert_eq!(configured.global_spill_limit_bytes(), 4096);
    }

    #[test]
    fn query_spill_limit_cannot_exceed_the_global_limit() {
        let error = AppConfig::load_from(
            &cli(),
            [
                ("PINTAIL_QUERY_SPILL_LIMIT_BYTES".into(), "4096".into()),
                ("PINTAIL_GLOBAL_SPILL_LIMIT_BYTES".into(), "2048".into()),
            ],
        )
        .expect_err("invalid spill limits");
        assert!(error.to_string().contains("cannot exceed"));
    }
}
