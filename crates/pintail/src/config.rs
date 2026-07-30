//! Process configuration loaded from TOML, environment, and CLI arguments.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use pintail_wire::DEFAULT_QUERY_MEMORY_LIMIT;
use serde::Deserialize;

const DEFAULT_CONFIG_FILE: &str = "pintail.toml";
const DEFAULT_DATA_DIR: &str = "./data";
const DEFAULT_HTTP_PORT: u16 = 8080;
const DEFAULT_WIRE_PORT: u16 = 3306;

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

    /// Hard byte ceiling for each HTTP or `MySQL` wire query.
    #[arg(long)]
    pub query_memory_limit_bytes: Option<usize>,
}

/// Effective process configuration after all sources have been merged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppConfig {
    data_dir: PathBuf,
    http_bind: SocketAddr,
    wire_bind: SocketAddr,
    query_memory_limit_bytes: usize,
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

        Ok(Self {
            data_dir,
            http_bind,
            wire_bind,
            query_memory_limit_bytes,
        })
    }

    /// Directory containing all durable Pintail state.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
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

    /// Hard byte ceiling for one HTTP or `MySQL` wire query.
    #[must_use]
    pub const fn query_memory_limit_bytes(&self) -> usize {
        self.query_memory_limit_bytes
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
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
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileQueryConfig {
    memory_limit_bytes: Option<usize>,
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
    use super::{AppConfig, Cli, DEFAULT_QUERY_MEMORY_LIMIT};

    fn cli() -> Cli {
        Cli {
            config: None,
            data_dir: None,
            http_bind: None,
            wire_bind: None,
            query_memory_limit_bytes: None,
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
}
