use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use pintail::{
    config::{AppConfig, Cli},
    secrets::{LoadedBootSecrets, generate_secret, load_or_create},
};
use pintail_api::{ApiState, router_with_state, spawn_supervisor};
use pintail_meta::MetaStore;
use pintail_wire::{WireOptions, load_wire_tls, serve_until_configured};
use tokio::net::TcpListener;

// glibc malloc keeps what a thread frees inside that thread's own arena, and
// a server whose supervisor opens and drops every table store every few
// seconds across a pool of threads grows one 128 MiB arena per thread and
// never gives them back: a staging node held 7 GB of heap for 500 MB of
// data, most of it swapped. jemalloc returns freed pages to the operating
// system on a decay timer, so resident memory follows what is live.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<()> {
    let started = std::time::Instant::now();
    // First, before anything that can fail. Secrets loading, metadata open and
    // spill preparation all abort startup on error, and a boot that dies
    // before telemetry exists is exactly the failure nobody can diagnose
    // remotely. This installs the panic hook too, so a crash from here on
    // carries a stack trace off the node.
    pintail_log::log_info!("{}", pintail_telemetry::init());
    let cli = Cli::parse();
    let config = AppConfig::load(&cli)?;

    let boot_secrets = load_or_create(config.data_dir())?;
    display_first_boot_secret(&boot_secrets, config.data_dir());

    let metadata_path = config.data_dir().join("pintail-meta.db");
    let metadata = MetaStore::open(&metadata_path)?;
    let jwt_secret = metadata.get_or_insert_setting("jwt_secret", &generate_secret())?;
    if jwt_secret.was_inserted() {
        if !boot_secrets.is_first_boot() {
            eprintln!("pintail first boot — save this secret now:");
        }
        eprintln!("PINTAIL_JWT_SECRET={}", jwt_secret.value());
        eprintln!("JWT secret saved to {}", metadata_path.display());
    }
    // Spill must land on the volume provisioned for data, not the system
    // temp directory the container gives us. Prove the location works now:
    // a query that spills only to discover an unwritable directory has
    // already done all of its work.
    pintail_exec::spill::configure_spill(
        config.spill_dir().to_path_buf(),
        config.query_spill_limit_bytes(),
        config.global_spill_limit_bytes(),
    )
    .with_context(|| {
        format!(
            "failed to prepare spill directory {}",
            config.spill_dir().display()
        )
    })?;
    match pintail_exec::spill::reclaim_orphaned_spill(config.spill_dir()) {
        Ok(0) => {}
        Ok(count) => eprintln!("reclaimed {count} spill paths from a previous run"),
        Err(error) => eprintln!("could not reclaim old spill files: {error}"),
    }

    // Installed before either listener binds so every query on both
    // surfaces draws from one bound.
    pintail_wire::init_shared_admission_with_reserved(
        config.max_concurrent_queries(),
        config.query_queue_wait(),
        config.reserved_query_slots(),
    );
    pintail_exec::init_shared_memory_budget(config.total_query_memory_limit_bytes());
    raise_open_file_limit();
    // This process is its data directory's only writer: a table stays
    // locked to it between replication cycles, so queries prove the replica
    // current from its generation instead of walking the table's files.
    pintail_store::retain_writer_locks();
    report_effective_limits(&config);

    let api_state = ApiState::new(
        config.data_dir(),
        &metadata_path,
        jwt_secret.value().as_bytes(),
        boot_secrets.secrets().dsn_encryption_key(),
    )?
    .with_query_memory_limit(config.query_memory_limit_bytes());

    let http_listener = TcpListener::bind(config.http_bind())
        .await
        .with_context(|| format!("failed to bind HTTP server to {}", config.http_bind()))?;
    let wire_listener = TcpListener::bind(config.wire_bind())
        .await
        .with_context(|| format!("failed to bind MySQL wire server to {}", config.wire_bind()))?;
    let wire_address = wire_listener.local_addr()?;
    let api_state = api_state.with_wire_bind(wire_address);
    eprintln!(
        "pintail listening on http://{}",
        http_listener.local_addr()?
    );
    eprintln!("pintail MySQL wire listening on {wire_address}");
    // Everything above is what a restart costs: control plane opened,
    // manifests loaded, WAL replayed, listeners bound.
    pintail_api::record_startup(started.elapsed());

    let (shutdown, _) = tokio::sync::broadcast::channel::<()>(1);
    let shutdown_signal_sender = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_signal_sender.send(());
    });
    let mut http_shutdown = shutdown.subscribe();
    let mut wire_shutdown = shutdown.subscribe();
    let supervisor = spawn_supervisor(api_state.clone(), shutdown.subscribe());
    let watchdog = pintail::watchdog::spawn(shutdown.subscribe());
    // with_connect_info: the audit trail records the network peer of every
    // action, and without this the socket address never reaches the router.
    let http = axum::serve(
        http_listener,
        router_with_state(api_state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = http_shutdown.recv().await;
    });
    let wire_tls = resolve_wire_tls(&config, &metadata)?;
    let wire = serve_until_configured(
        wire_listener,
        config.data_dir(),
        &metadata_path,
        WireOptions {
            query_memory_limit: config.query_memory_limit_bytes(),
            tls: wire_tls,
            idle_timeout: config.wire_idle_timeout(),
            limits: config.wire_limits(),
        },
        async move {
            let _ = wire_shutdown.recv().await;
        },
    );
    tokio::try_join!(async { http.await.context("HTTP server failed") }, async {
        wire.await.context("MySQL wire server failed")
    })?;
    supervisor.await.context("replication supervisor failed")?;
    watchdog.await.context("memory watchdog failed")?;
    Ok(())
}

/// The certificate the wire listener serves.
///
/// An explicitly configured one always wins. Otherwise the node issues its
/// own, because with no certificate the server never advertises `CLIENT_SSL`:
/// a client that would have preferred TLS gets plaintext and has no way to ask
/// for better. Generating one moves the default from cleartext to TLS without
/// the operator configuring anything, which is how a managed database service
/// behaves.
fn resolve_wire_tls(
    config: &AppConfig,
    metadata: &MetaStore,
) -> Result<Option<pintail_wire::WireTls>> {
    if let Some((certificate, key, required)) = config.wire_tls() {
        return Ok(Some(load_wire_tls(certificate, key, required)?));
    }
    // Resolved by the same code the settings API reads, so what an operator
    // sees on the page and what the certificate covers cannot drift apart.
    let hostnames = pintail_api::wire_tls_hostnames(metadata);
    // Failure here is not fatal. A database that refuses to boot because it
    // could not write a certificate is worse than one serving without it, and
    // the operator can still supply their own.
    match pintail_wire::managed_tls::ensure(config.data_dir(), &hostnames) {
        Ok(managed) => {
            if managed.generated {
                pintail_log::log_info!(
                    "wire tls: generated a node certificate covering {} name(s)",
                    hostnames.len() + 3
                );
            }
            Ok(load_wire_tls(
                &managed.certificate_path,
                &managed.key_path,
                config.wire_require_tls(),
            )
            .ok())
        }
        Err(error) => {
            pintail_log::log_error!(
                "wire tls: could not prepare a node certificate, serving without TLS: {error}"
            );
            Ok(None)
        }
    }
}

fn display_first_boot_secret(loaded: &LoadedBootSecrets, data_dir: &Path) {
    if !loaded.is_first_boot() {
        return;
    }

    eprintln!("pintail first boot — save this secret now:");
    eprintln!(
        "PINTAIL_DSN_ENCRYPTION_KEY={}",
        loaded.secrets().dsn_encryption_key()
    );
    eprintln!(
        "secrets saved to {}",
        data_dir.join("secrets.toml").display()
    );
}

async fn shutdown_signal() {
    let control_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = control_c => {}
        () = terminate => {}
    }
}

/// Names the resource ceilings actually in force, once, at startup.
///
/// A deployment can configure a knob the container never receives - a
/// compose file that forwards nine environment variables and drops the
/// tenth reads as configured and is not - and an operator has no way to
/// tell from the outside. Two production failures on 2026-09-06 were a
/// dropped `PINTAIL_MAX_CONCURRENT_QUERIES` and a descriptor soft limit
/// nobody had set, both invisible until a dashboard failed. Print what
/// the process resolved, including the limits it did not choose.
fn report_effective_limits(config: &pintail::config::AppConfig) {
    // Integer arithmetic: a byte ceiling near u64::MAX does not survive an
    // f64 mantissa, and a limits line that rounds is worse than useless.
    let describe = |bytes: u64| {
        if bytes == 0 {
            return "unbounded".to_owned();
        }
        let mib = bytes / (1024 * 1024);
        format!("{}.{:02}GiB", mib / 1024, (mib % 1024) * 100 / 1024)
    };
    let admission = match config.max_concurrent_queries() {
        0 => "unbounded".to_owned(),
        limit => limit.to_string(),
    };
    pintail_log::log_info!(
        "pintail limits: concurrent_queries={admission} queue_wait={:.1}s query_memory={} \
         shared_memory={} process_memory={} open_files={} spill_dir={} \
         query_spill={} global_spill={}",
        config.query_queue_wait().as_secs_f64(),
        describe(config.query_memory_limit_bytes() as u64),
        describe(config.total_query_memory_limit_bytes() as u64),
        pintail::config::available_memory_bytes().map_or_else(|| "undetected".to_owned(), describe),
        open_file_limit(),
        config.spill_dir().display(),
        describe(config.query_spill_limit_bytes()),
        describe(config.global_spill_limit_bytes()),
    );
}

/// The descriptor soft limit the process raises itself to when it inherits
/// a lower one. A columnar scan opens the segment files it reads and every
/// concurrent spilling query holds a bounded handful of run files, so a
/// desktop default of 1024 runs out under load while this does not.
const OPEN_FILE_TARGET: u64 = 65_536;

/// Raises the soft descriptor limit toward [`OPEN_FILE_TARGET`], capped by
/// the inherited hard limit.
///
/// Best effort and conservative: a soft limit already at or above the
/// target is left alone, the hard limit is never touched, and a refusal is
/// reported rather than treated as fatal, since the process can run under
/// the inherited limit and the limits line that follows says what it got.
/// `PINTAIL_KEEP_OPEN_FILE_LIMIT=1` opts out for an operator who set the
/// soft limit deliberately. The shipped compose file already sets soft and
/// hard equal, so this matters for the bare binary and other packagings.
fn raise_open_file_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    if std::env::var_os("PINTAIL_KEEP_OPEN_FILE_LIMIT").is_some_and(|value| value == "1") {
        return;
    }
    let current = getrlimit(Resource::Nofile);
    let target = current
        .maximum
        .map_or(OPEN_FILE_TARGET, |hard| hard.min(OPEN_FILE_TARGET));
    if current.current.is_some_and(|soft| soft >= target) {
        return;
    }
    let raised = Rlimit {
        current: Some(target),
        maximum: current.maximum,
    };
    if let Err(error) = setrlimit(Resource::Nofile, raised) {
        pintail_log::log_error!(
            "could not raise the open file soft limit from {} to {target}: {error}",
            current
                .current
                .map_or_else(|| "unlimited".to_owned(), |soft| soft.to_string())
        );
    }
}

/// The descriptor soft and hard limits as the kernel reports them.
fn open_file_limit() -> String {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let describe =
        |value: Option<u64>| value.map_or_else(|| "unlimited".to_owned(), |v| v.to_string());
    format!("{}/{}", describe(limit.current), describe(limit.maximum))
}
