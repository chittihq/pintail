use std::{ffi::OsString, net::SocketAddr, path::PathBuf};

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
        "#,
    )
    .expect("write config");

    let cli_data_dir = config_dir.path().join("from-cli");
    let cli = Cli {
        config: Some(config_path),
        data_dir: Some(cli_data_dir.clone()),
        http_bind: None,
        wire_bind: None,
    };
    let environment = [(
        OsString::from("PINTAIL_HTTP_BIND"),
        OsString::from("127.0.0.1:7100"),
    )];

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
}
