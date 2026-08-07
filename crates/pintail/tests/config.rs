use std::{ffi::OsString, net::SocketAddr, path::PathBuf, time::Duration};

use pintail::config::{AppConfig, Cli};

#[test]
fn cli_and_environment_override_the_toml_configuration() {
    let config_dir = tempfile::tempdir().expect("temporary config directory");
    let config_path = config_dir.path().join("pintail.toml");
    std::fs::write(
        &config_path,
        r#"
            data_dir = "./from-file"

            [http]
            bind = "127.0.0.1:7000"

            [wire]
            bind = "127.0.0.1:3307"
            idle_timeout_seconds = 120

            [query]
            memory_limit_bytes = 134217728
        "#,
    )
    .expect("write config");

    let cli_data_dir = config_dir.path().join("from-cli");
    let cli = Cli {
        config: Some(config_path),
        data_dir: Some(cli_data_dir.clone()),
        http_bind: None,
        wire_bind: None,
        wire_idle_timeout_seconds: None,
        query_memory_limit_bytes: Some(268_435_456),
        spill_dir: None,
        query_spill_limit_bytes: Some(536_870_912),
        global_spill_limit_bytes: Some(1_073_741_824),
    };
    let environment = [
        (
            OsString::from("PINTAIL_HTTP_BIND"),
            OsString::from("127.0.0.1:7100"),
        ),
        (
            OsString::from("PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS"),
            OsString::from("60"),
        ),
    ];

    let config = AppConfig::load_from(&cli, environment).expect("load config");

    assert_eq!(config.data_dir(), cli_data_dir);
    assert_eq!(
        config.http_bind(),
        "127.0.0.1:7100".parse::<SocketAddr>().expect("address")
    );
    assert_ne!(config.data_dir(), PathBuf::from("./from-file"));
    assert_eq!(
        config.wire_bind(),
        "127.0.0.1:3307"
            .parse::<SocketAddr>()
            .expect("wire address")
    );
    assert_eq!(config.query_memory_limit_bytes(), 268_435_456);
    assert_eq!(config.wire_idle_timeout(), Duration::from_secs(60));
    assert_eq!(config.query_spill_limit_bytes(), 536_870_912);
    assert_eq!(config.global_spill_limit_bytes(), 1_073_741_824);
}
