use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

#[test]
fn binary_boots_serves_health_and_prints_secrets_only_once() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");

    let first_boot = run_until_healthy(data_dir.path());
    assert!(first_boot.contains("PINTAIL_JWT_SECRET="));
    assert!(first_boot.contains("PINTAIL_DSN_ENCRYPTION_KEY="));
    assert!(data_dir.path().join("pintail-meta.db").exists());
    let metadata = rusqlite::Connection::open(data_dir.path().join("pintail-meta.db"))
        .expect("inspect metadata");
    let jwt_secret: String = metadata
        .query_row(
            "SELECT value FROM settings WHERE key = 'jwt_secret'",
            [],
            |row| row.get(0),
        )
        .expect("persisted JWT secret");
    assert_eq!(jwt_secret.len(), 64);
    drop(metadata);

    let restart = run_until_healthy(data_dir.path());
    assert!(!restart.contains("PINTAIL_JWT_SECRET="));
    assert!(!restart.contains("PINTAIL_DSN_ENCRYPTION_KEY="));
}

fn run_until_healthy(data_dir: &Path) -> String {
    let address = unused_address();
    let child = Command::new(env!("CARGO_BIN_EXE_pintail"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--http-bind")
        .arg(address.to_string())
        .arg("--wire-bind")
        .arg("127.0.0.1:0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start pintail");
    let mut process = ProcessGuard(child);

    let response = wait_for_health(&mut process.0, address);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains(r#"{"status":"ok"}"#), "{response}");

    process.0.kill().expect("stop pintail");
    let status = process.0.wait().expect("wait for pintail");
    assert!(!status.success(), "test terminates the running server");

    let mut stderr = String::new();
    process
        .0
        .stderr
        .take()
        .expect("pintail stderr")
        .read_to_string(&mut stderr)
        .expect("read pintail stderr");
    stderr
}

fn wait_for_health(process: &mut Child, address: SocketAddr) -> String {
    for _ in 0..400 {
        if let Some(status) = process.try_wait().expect("inspect pintail process") {
            let mut stderr = String::new();
            process
                .stderr
                .take()
                .expect("pintail stderr")
                .read_to_string(&mut stderr)
                .expect("read failed pintail stderr");
            panic!("pintail exited with {status} before listening on {address}:\n{stderr}");
        }
        match TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("read timeout");
                stream
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .expect("health request");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("health response");
                return response;
            }
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    panic!("pintail did not listen on {address}");
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral listener");
    listener.local_addr().expect("ephemeral address")
}

struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
