use std::{
    io::Write as _,
    process::{Command, Output, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

struct MysqlContainer {
    name: String,
    host: String,
    port: u16,
}

impl MysqlContainer {
    fn start() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let name = format!("pintail-m6-api-{}-{nonce}", std::process::id());
        checked_output(
            Command::new("docker").args([
                "run",
                "--detach",
                "--name",
                &name,
                "--publish",
                "0:3306",
                "--tmpfs",
                "/var/lib/mysql:rw,size=1g",
                "--env",
                "MYSQL_ROOT_PASSWORD=pintail-root",
                "--env",
                "MYSQL_DATABASE=analytics",
                "mysql:8.4",
                "--skip-log-bin",
                "--default-time-zone=+00:00",
                "--sql-mode=NO_ENGINE_SUBSTITUTION",
            ]),
            "start API MySQL source",
        )?;
        let host = docker_host()?;
        let port_output = checked_output(
            Command::new("docker").args(["port", &name, "3306/tcp"]),
            "inspect MySQL published port",
        )?;
        let port = String::from_utf8(port_output.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next())
            .and_then(|port| port.parse().ok())
            .ok_or_else(|| "Docker did not report a numeric MySQL port".to_owned())?;
        let container = Self { name, host, port };
        for _ in 0..120 {
            if container.query_batch("SELECT 1;").is_ok() {
                return Ok(container);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("API MySQL source did not become ready within 60 seconds".to_owned())
    }

    fn dsn(&self) -> String {
        format!(
            "mysql://pintail:pintail@{}:{}/analytics",
            self.host, self.port
        )
    }

    fn query_batch(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "--interactive",
                &self.name,
                "mysql",
                "--user=root",
                "--password=pintail-root",
                "--database=analytics",
                "--default-character-set=utf8mb4",
                "--batch",
                "--raw",
                "--skip-column-names",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn MySQL client: {error}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| "MySQL stdin was not piped".to_owned())?
            .write_all(sql.as_bytes())
            .map_err(|error| format!("write MySQL batch: {error}"))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for MySQL batch: {error}"))?;
        if output.status.success() {
            String::from_utf8(output.stdout).map_err(|error| error.to_string())
        } else {
            Err(format_output_error("execute MySQL batch", &output))
        }
    }
}

impl Drop for MysqlContainer {
    fn drop(&mut self) {
        let _status = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the configured Docker host and mysql:8.4 image"]
#[allow(clippy::too_many_lines)]
async fn wizard_snapshot_query_reconcile_and_resync_happy_path() {
    let mysql = MysqlContainer::start().unwrap_or_else(|error| panic!("{error}"));
    mysql
        .query_batch(
            "CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail';\
             GRANT SELECT, RELOAD, REPLICATION CLIENT, REPLICATION SLAVE ON *.* TO 'pintail'@'%';\
             CREATE TABLE events (\
               id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,\
               name VARCHAR(255) NULL,\
               updated_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)\
                 ON UPDATE CURRENT_TIMESTAMP(6)\
             ) ENGINE=InnoDB;\
             INSERT INTO events (name) VALUES ('launch'), ('land');",
        )
        .unwrap_or_else(|error| panic!("{error}"));

    let data = tempfile::tempdir().expect("API data directory");
    let state = pintail_api::ApiState::new(
        data.path(),
        data.path().join("pintail-meta.db"),
        b"test-jwt-secret-with-enough-entropy",
        &"42".repeat(32),
    )
    .expect("configured API state");
    let app = pintail_api::router_with_state(state);
    let setup = json_response(
        request(
            &app,
            Method::POST,
            "/api/auth/setup",
            None,
            Some(json!({
                "email": "admin@example.com",
                "password": "correct horse battery"
            })),
        )
        .await,
    )
    .await;
    let authorization = format!("Bearer {}", setup["token"].as_str().expect("setup token"));

    let created = request(
        &app,
        Method::POST,
        "/api/databases",
        Some(&authorization),
        Some(json!({
            "name": "analytics",
            "dsn": mysql.dsn(),
            "mode": "polling",
            "include_tables": ["events"]
        })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json_response(created).await;
    let database_id = created["id"].as_str().expect("database ID");

    assert_eq!(
        request(
            &app,
            Method::POST,
            &format!("/api/databases/{database_id}/test"),
            Some(&authorization),
            None,
        )
        .await
        .status(),
        StatusCode::OK
    );
    let probe = request(
        &app,
        Method::GET,
        &format!("/api/databases/{database_id}/probe"),
        Some(&authorization),
        None,
    )
    .await;
    assert_eq!(probe.status(), StatusCode::OK);
    assert_eq!(
        json_response(probe).await["capabilities"]["recommended_mode"],
        "polling"
    );

    let snapshot = request(
        &app,
        Method::POST,
        &format!("/api/databases/{database_id}/snapshot"),
        Some(&authorization),
        Some(json!({"force": false})),
    )
    .await;
    assert_eq!(snapshot.status(), StatusCode::ACCEPTED);
    let snapshot_run = json_response(snapshot).await["run_id"]
        .as_str()
        .expect("snapshot run")
        .to_owned();
    wait_for_run(
        &app,
        &authorization,
        database_id,
        &snapshot_run,
        "completed",
    )
    .await;
    assert_query_rows(
        &app,
        &authorization,
        database_id,
        json!([[1, "launch"], [2, "land"]]),
    )
    .await;

    mysql
        .query_batch(
            "DELETE FROM events WHERE id = 1;\
             UPDATE events SET name = 'touchdown' WHERE id = 2;\
             INSERT INTO events (name) VALUES ('orbit');",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let reconcile = request(
        &app,
        Method::POST,
        &format!("/api/databases/{database_id}/tables/events/reconcile"),
        Some(&authorization),
        None,
    )
    .await;
    assert_eq!(reconcile.status(), StatusCode::ACCEPTED);
    let reconcile = json_response(reconcile).await;
    assert_eq!(reconcile["table"], "events");
    let reconcile_run = reconcile["run_id"]
        .as_str()
        .expect("reconcile run")
        .to_owned();
    wait_for_run(
        &app,
        &authorization,
        database_id,
        &reconcile_run,
        "completed",
    )
    .await;
    assert_query_rows(
        &app,
        &authorization,
        database_id,
        json!([[2, "touchdown"], [3, "orbit"]]),
    )
    .await;

    mysql
        .query_batch("INSERT INTO events (name) VALUES ('return');")
        .unwrap_or_else(|error| panic!("{error}"));
    let resync = request(
        &app,
        Method::POST,
        &format!("/api/databases/{database_id}/tables/events/resync"),
        Some(&authorization),
        None,
    )
    .await;
    assert_eq!(resync.status(), StatusCode::ACCEPTED);
    let resync_run = json_response(resync).await["run_id"]
        .as_str()
        .expect("resync run")
        .to_owned();
    wait_for_run(&app, &authorization, database_id, &resync_run, "completed").await;
    assert_query_rows(
        &app,
        &authorization,
        database_id,
        json!([[2, "touchdown"], [3, "orbit"], [4, "return"]]),
    )
    .await;
}

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    authorization: Option<&str>,
    body: Option<Value>,
) -> Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    let (builder, body) = if let Some(body) = body {
        (
            builder.header(header::CONTENT_TYPE, "application/json"),
            Body::from(body.to_string()),
        )
    } else {
        (builder, Body::empty())
    };
    app.clone()
        .oneshot(builder.body(body).expect("API request"))
        .await
        .expect("API response")
}

async fn json_response(response: Response) -> Value {
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{status} did not return JSON: {error}"))
}

async fn wait_for_run(
    app: &Router,
    authorization: &str,
    database_id: &str,
    run_id: &str,
    expected: &str,
) {
    for _ in 0..600 {
        let activity = json_response(
            request(
                app,
                Method::GET,
                &format!("/api/activity?db={database_id}&limit=100"),
                Some(authorization),
                None,
            )
            .await,
        )
        .await;
        if let Some(run) = activity
            .as_array()
            .expect("activity array")
            .iter()
            .find(|run| run["id"] == run_id)
        {
            if run["status"] == expected {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return;
            }
            assert_ne!(run["status"], "error", "API job {run_id} failed: {run}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("API job {run_id} did not reach {expected}");
}

async fn assert_query_rows(app: &Router, authorization: &str, database_id: &str, expected: Value) {
    let response = request(
        app,
        Method::POST,
        "/api/query",
        Some(authorization),
        Some(json!({
            "db": database_id,
            "sql": "SELECT id, name FROM events ORDER BY id"
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_response(response).await["rows"], expected);
}

fn checked_output(command: &mut Command, action: &str) -> Result<Output, String> {
    command
        .output()
        .map_err(|error| format!("{action}: {error}"))
        .and_then(|output| {
            if output.status.success() {
                Ok(output)
            } else {
                Err(format_output_error(action, &output))
            }
        })
}

fn format_output_error(action: &str, output: &Output) -> String {
    format!(
        "{action} failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn docker_host() -> Result<String, String> {
    let context = checked_output(
        Command::new("docker").args(["context", "show"]),
        "read Docker context",
    )?;
    let context = String::from_utf8(context.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    let endpoint = checked_output(
        Command::new("docker").args([
            "context",
            "inspect",
            &context,
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]),
        "read Docker endpoint",
    )?;
    let endpoint = String::from_utf8(endpoint.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    if let Some(target) = endpoint.strip_prefix("ssh://") {
        let target = target.split('@').next_back().unwrap_or(target);
        let target = target.split(':').next().unwrap_or(target);
        let config = checked_output(
            Command::new("ssh").args(["-G", target]),
            "resolve Docker SSH host",
        )?;
        return String::from_utf8(config.stdout)
            .map_err(|error| error.to_string())?
            .lines()
            .find_map(|line| line.strip_prefix("hostname ").map(str::to_owned))
            .ok_or_else(|| "SSH configuration did not expose a hostname".to_owned());
    }
    if let Some(endpoint) = endpoint
        .strip_prefix("tcp://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .or_else(|| endpoint.strip_prefix("https://"))
    {
        return Ok(endpoint
            .trim_start_matches('[')
            .split([']', ':'])
            .next()
            .unwrap_or(endpoint)
            .to_owned());
    }
    Ok("127.0.0.1".to_owned())
}
