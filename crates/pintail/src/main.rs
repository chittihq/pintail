use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use pintail::{
    config::{AppConfig, Cli},
    secrets::{LoadedBootSecrets, generate_secret, load_or_create},
};
use pintail_api::router;
use pintail_meta::MetaStore;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
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

    let listener = TcpListener::bind(config.http_bind())
        .await
        .with_context(|| format!("failed to bind HTTP server to {}", config.http_bind()))?;
    eprintln!("pintail listening on http://{}", config.http_bind());

    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")
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
