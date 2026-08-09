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

#[tokio::main]
async fn main() -> Result<()> {
    let started = std::time::Instant::now();
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
    let http = axum::serve(http_listener, router_with_state(api_state)).with_graceful_shutdown(
        async move {
            let _ = http_shutdown.recv().await;
        },
    );
    let wire_tls = config
        .wire_tls()
        .map(|(certificate, key, required)| load_wire_tls(certificate, key, required))
        .transpose()?;
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
