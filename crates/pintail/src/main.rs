use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use pintail::{
    config::{AppConfig, Cli},
    secrets::{LoadedBootSecrets, generate_secret, load_or_create},
};
use pintail_api::{ApiState, router_with_state, spawn_supervisor};
use pintail_meta::MetaStore;
use pintail_wire::{load_wire_tls, serve_until_with_options};
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
    pintail_wire::init_shared_admission(config.max_concurrent_queries());
    pintail_exec::init_shared_memory_budget(config.total_query_memory_limit_bytes());

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
    let wire = serve_until_with_options(
        wire_listener,
        config.data_dir(),
        &metadata_path,
        config.query_memory_limit_bytes(),
        wire_tls,
        config.wire_idle_timeout(),
        async move {
            let _ = wire_shutdown.recv().await;
        },
    );
    tokio::try_join!(async { http.await.context("HTTP server failed") }, async {
        wire.await.context("MySQL wire server failed")
    })?;
    supervisor.await.context("replication supervisor failed")?;
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
