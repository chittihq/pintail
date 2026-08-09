use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use pintail_meta::{ApiKeyRecord, MetaStore};
use pintail_protocol::{
    BinaryValue, CapabilityFlags, Column, ColumnFlags, ColumnType, Connection, DisconnectWatch,
    ErrorKind, Handler, HandshakeResponse, IntWidth, OkPacket, PreparedStatement, Response,
    ResultSet, SCRAMBLE_SIZE, WatchOutcome, decode_execute_parameters, encode_binary_datetime,
    encode_binary_int, encode_binary_time, packet::put_length_encoded_bytes,
};
use pintail_sql::DEFAULT_TEXT_COLLATION;
use pintail_types::{DataType, Value};
use rand::RngCore as _;
use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};
use tokio::net::{TcpListener, TcpStream};

use crate::{
    DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, QueryError, QueryField, QueryOutput, QueryStats,
    ReplicaEngine,
};

static NEXT_CONNECTION_ID: AtomicU32 = AtomicU32::new(1);

/// Default time an authenticated wire connection may remain idle.
pub const DEFAULT_WIRE_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Accepts `MySQL` clients and serves read-only Pintail queries.
///
/// # Errors
///
/// Returns an error when the listener cannot accept another connection.
pub async fn serve(
    listener: TcpListener,
    data_dir: impl Into<PathBuf>,
    metadata_path: impl Into<PathBuf>,
) -> io::Result<()> {
    let data_dir = data_dir.into();
    let metadata_path = metadata_path.into();
    loop {
        let (stream, _) = listener.accept().await?;
        let backend = Backend::new(&data_dir, &metadata_path, DEFAULT_QUERY_MEMORY_LIMIT);
        tokio::spawn(async move {
            let _ = serve_connection(stream, backend, None, DEFAULT_WIRE_IDLE_TIMEOUT).await;
        });
    }
}

/// Serves clients until the supplied shutdown signal resolves.
///
/// # Errors
///
/// Returns an error when the listener cannot accept another connection.
pub async fn serve_until<F>(
    listener: TcpListener,
    data_dir: impl Into<PathBuf>,
    metadata_path: impl Into<PathBuf>,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    serve_until_with_memory_limit(
        listener,
        data_dir,
        metadata_path,
        DEFAULT_QUERY_MEMORY_LIMIT,
        shutdown,
    )
    .await
}

/// Serves clients with an explicit hard per-query memory ceiling until shutdown.
///
/// # Errors
///
/// Returns an error when the listener cannot accept another connection.
pub async fn serve_until_with_memory_limit<F>(
    listener: TcpListener,
    data_dir: impl Into<PathBuf>,
    metadata_path: impl Into<PathBuf>,
    query_memory_limit: usize,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    serve_until_with_options(
        listener,
        data_dir,
        metadata_path,
        query_memory_limit,
        None,
        DEFAULT_WIRE_IDLE_TIMEOUT,
        shutdown,
    )
    .await
}

/// TLS termination policy for the wire listener.
#[derive(Clone)]
pub struct WireTls {
    /// Certificate chain and key, ready for per-connection handshakes.
    pub config: std::sync::Arc<tokio_rustls::rustls::ServerConfig>,
    /// Whether plaintext clients are refused after the greeting.
    pub required: bool,
}

/// Loads a PEM certificate chain and private key into a wire TLS policy.
///
/// # Errors
///
/// Returns an error when either file is unreadable or not valid PEM, or the
/// key does not match rustls' supported formats.
pub fn load_wire_tls(
    certificate_path: &std::path::Path,
    key_path: &std::path::Path,
    required: bool,
) -> io::Result<WireTls> {
    let certificates = rustls_pemfile::certs(&mut io::BufReader::new(std::fs::File::open(
        certificate_path,
    )?))
    .collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "certificate file contains no certificates",
        ));
    }
    let key = rustls_pemfile::private_key(&mut io::BufReader::new(std::fs::File::open(key_path)?))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "key file contains no private key",
            )
        })?;
    // An explicit provider keeps the build deterministic even when feature
    // unification enables more than one rustls crypto backend.
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(WireTls {
        config: std::sync::Arc::new(config),
        required,
    })
}

/// Serves clients with an explicit memory ceiling and optional TLS policy
/// until shutdown.
///
/// # Errors
///
/// Returns an error when the listener cannot accept another connection.
pub async fn serve_until_with_options<F>(
    listener: TcpListener,
    data_dir: impl Into<PathBuf>,
    metadata_path: impl Into<PathBuf>,
    query_memory_limit: usize,
    tls: Option<WireTls>,
    idle_timeout: Duration,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let data_dir = data_dir.into();
    let metadata_path = metadata_path.into();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let backend = Backend::new(&data_dir, &metadata_path, query_memory_limit);
                let tls = tls.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, backend, tls, idle_timeout).await;
                });
            }
        }
    }
}

/// Watches a duplicated handle to the same socket for a disconnected peer.
///
/// `try_clone` dups the underlying file descriptor rather than sharing
/// `Connection`'s own reader, so this can poll read-readiness and peek
/// without any risk of a second waker overwriting the packet reader's —
/// they are independent registrations at the OS level, not two users of one
/// tokio-internal registration. `peek` never consumes bytes, so a false
/// alarm here has nothing to hand back through [`WatchOutcome::Primed`].
struct TcpDisconnectWatch {
    probe: TcpStream,
}

#[async_trait]
impl DisconnectWatch for TcpDisconnectWatch {
    async fn watch(&mut self) -> WatchOutcome {
        let mut probe_byte = [0_u8; 1];
        loop {
            if self.probe.readable().await.is_err() {
                return WatchOutcome::Disconnected;
            }
            match self.probe.peek(&mut probe_byte).await {
                Ok(0) => return WatchOutcome::Disconnected,
                // Data is genuinely there and untouched; not a disconnect.
                Ok(_) => return WatchOutcome::Primed(Vec::new()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return WatchOutcome::Disconnected,
            }
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    backend: Backend,
    tls: Option<WireTls>,
    idle_timeout: Duration,
) -> io::Result<()> {
    // Duplicating at the std level (rather than sharing a handle) gives the
    // watch its own OS-level socket registration, independent of the packet
    // reader's — two tokio users polling read-readiness on the SAME
    // registration can silently overwrite each other's waker; two
    // independent registrations over the same underlying socket cannot.
    let std_stream = stream.into_std()?;
    let probe_stream = std_stream.try_clone()?;
    let stream = TcpStream::from_std(std_stream)?;
    let watch = TcpDisconnectWatch {
        probe: TcpStream::from_std(probe_stream)?,
    };
    let scramble = backend.salt;
    let (reader, writer) = stream.into_split();
    let mut connection = Connection::new(reader, writer);
    let extra_capabilities = if tls.is_some() {
        CapabilityFlags::CLIENT_SSL
    } else {
        CapabilityFlags::empty()
    };
    connection
        .send_greeting(&backend, scramble, extra_capabilities)
        .await?;
    let initial = connection.read_initial_response().await?;
    match (initial, tls) {
        // A required-TLS listener drops a plaintext client after the
        // greeting rather than serve it unencrypted; MySQL clients report
        // the closed connection as "server requires secure transport".
        (pintail_protocol::InitialResponse::Full(_), Some(tls)) if tls.required => Ok(()),
        (pintail_protocol::InitialResponse::Full(response), _) => {
            run_connection(connection, response, backend, scramble, watch, idle_timeout).await
        }
        (pintail_protocol::InitialResponse::Ssl, Some(tls)) => {
            let (reader, writer, read_sequence, write_sequence) = connection.into_parts();
            let stream = reader.reunite(writer).map_err(io_other)?;
            let acceptor = tokio_rustls::TlsAcceptor::from(tls.config);
            let tls_stream = acceptor.accept(stream).await?;
            let (tls_reader, tls_writer) = tokio::io::split(tls_stream);
            let mut connection =
                Connection::new_at_sequence(tls_reader, tls_writer, read_sequence, write_sequence);
            let response = match connection.read_initial_response().await? {
                pintail_protocol::InitialResponse::Full(response) => response,
                pintail_protocol::InitialResponse::Ssl => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "client requested TLS twice",
                    ));
                }
            };
            // The disconnect watch peeks the plaintext socket; once TLS is
            // established the ciphertext stream carries no equivalent
            // non-consuming probe, so this connection loses that guard and
            // relies on the idle timeout and the next read to notice a gone
            // peer, same as before this driver existed.
            run_connection_without_watch(connection, response, backend, scramble, idle_timeout)
                .await
        }
        (pintail_protocol::InitialResponse::Ssl, None) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client requested TLS but this listener has none configured",
        )),
    }
}

async fn run_connection<R, W>(
    mut connection: Connection<R, W>,
    response: HandshakeResponse,
    mut backend: Backend,
    scramble: [u8; SCRAMBLE_SIZE],
    mut watch: TcpDisconnectWatch,
    idle_timeout: Duration,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    if connection
        .complete_authentication(&mut backend, response, &scramble)
        .await
        .is_err()
    {
        return Ok(());
    }
    loop {
        let served = tokio::time::timeout(
            idle_timeout,
            connection.serve_one_with_disconnect_watch(&mut backend, &mut watch),
        )
        .await;
        match served {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) | Err(_) => return Ok(()),
            Ok(Err(error)) => return Err(error),
        }
    }
}

async fn run_connection_without_watch<R, W>(
    mut connection: Connection<R, W>,
    response: HandshakeResponse,
    mut backend: Backend,
    scramble: [u8; SCRAMBLE_SIZE],
    idle_timeout: Duration,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    if connection
        .complete_authentication(&mut backend, response, &scramble)
        .await
        .is_err()
    {
        return Ok(());
    }
    loop {
        let served = tokio::time::timeout(idle_timeout, connection.serve_one(&mut backend)).await;
        match served {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) | Err(_) => return Ok(()),
            Ok(Err(error)) => return Err(error),
        }
    }
}

/// `sql_mode` flags that change how a statement parses or evaluates, and
/// that Pintail does not implement.
///
/// The parser is a fixed `MySqlDialect`, so `PIPES_AS_CONCAT` cannot make
/// `||` concatenate and `ANSI_QUOTES` cannot make `"x"` an identifier.
/// Storing such a mode and carrying on would answer a different question
/// than the client asked - `a || b` returning a boolean where the client
/// expected a string - with no error to notice. These are refused instead.
const RESULT_CHANGING_SQL_MODES: &[&str] = &[
    // Parsing.
    "ANSI_QUOTES",
    "PIPES_AS_CONCAT",
    "HIGH_NOT_PRECEDENCE",
    "NO_BACKSLASH_ESCAPES",
    "IGNORE_SPACE",
    // Evaluation.
    "REAL_AS_FLOAT",
    "NO_UNSIGNED_SUBTRACTION",
    // Would ask ingestion to keep values it normalizes to NULL.
    "ALLOW_INVALID_DATES",
];

/// Compound modes, each of which turns on result-changing flags.
const COMPOUND_SQL_MODES: &[&str] = &["ANSI", "DB2", "MAXDB", "MSSQL", "ORACLE", "POSTGRESQL"];

/// Refuses a `sql_mode` that would change results Pintail cannot deliver.
///
/// Write-and-DDL modes (`STRICT_*`, `NO_ZERO_*`, `NO_ENGINE_SUBSTITUTION`
/// and friends) are accepted quietly: this endpoint is read-only, so they
/// are genuinely inert here rather than silently ignored.
fn reject_unsupported_sql_modes(value: &str) -> Result<(), String> {
    for mode in value.split(',') {
        let mode = mode.trim().to_ascii_uppercase();
        if mode.is_empty() {
            continue;
        }
        if RESULT_CHANGING_SQL_MODES.contains(&mode.as_str()) {
            return Err(format!(
                "Variable 'sql_mode' can't be set to the value of '{mode}': \
                 Pintail does not implement it and would otherwise return a \
                 different result than the mode requests"
            ));
        }
        if COMPOUND_SQL_MODES.contains(&mode.as_str()) {
            return Err(format!(
                "Variable 'sql_mode' can't be set to the value of '{mode}': \
                 the combination mode enables parsing changes Pintail does \
                 not implement"
            ));
        }
    }
    Ok(())
}

/// Per-connection session variables with real semantics: `time_zone`
/// shifts the statement-pinned time functions, `NAMES` accepts only the
/// utf8 charsets Pintail actually serves, and `sql_mode` accepts only
/// modes that are genuinely inert on a read-only replica.
#[derive(Clone, Debug)]
struct Session {
    time_zone: String,
    sql_mode: String,
    charset_client: String,
    charset_connection: String,
    charset_results: String,
    group_concat_max_len: usize,
    group_concat_warnings: u64,
    cte_max_recursion_depth: u64,
    max_execution_time_ms: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            time_zone: "SYSTEM".to_owned(),
            sql_mode: "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
                .to_owned(),
            charset_client: "utf8mb4".to_owned(),
            charset_connection: "utf8mb4".to_owned(),
            charset_results: "utf8mb4".to_owned(),
            group_concat_max_len: 1024,
            group_concat_warnings: 0,
            cte_max_recursion_depth: pintail_exec::DEFAULT_CTE_MAX_RECURSION_DEPTH,
            max_execution_time_ms: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct Authenticated {
    database_id: String,
    database_name: String,
}

#[derive(Clone, Debug)]
struct Prepared {
    sql: String,
    parameters: usize,
    /// Types from the last EXECUTE that rebound them, reused when a later
    /// EXECUTE on the same handle does not.
    parameter_types: Option<Vec<pintail_protocol::ParameterType>>,
    /// `COM_STMT_SEND_LONG_DATA` is not implemented: decoding an EXECUTE
    /// body assumes every parameter's value is present in the body itself,
    /// which is false for one a client streamed in over several
    /// long-data chunks. Rather than silently misdecode the following
    /// parameters, EXECUTE on a statement that received one rejects
    /// explicitly.
    used_long_data: bool,
}

struct Backend {
    metadata_path: PathBuf,
    engine: ReplicaEngine,
    authentication: Mutex<Option<Authenticated>>,
    session: Mutex<Session>,
    prepared: BTreeMap<u32, Prepared>,
    next_statement_id: u32,
    connection_id: u32,
    salt: [u8; 20],
}

struct CancelExecutionOnDrop(Option<pintail_exec::ExecutionCancellation>);

impl CancelExecutionOnDrop {
    fn new(cancellation: pintail_exec::ExecutionCancellation) -> Self {
        Self(Some(cancellation))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelExecutionOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

impl Backend {
    fn new(data_dir: &Path, metadata_path: &Path, query_memory_limit: usize) -> Self {
        Self {
            metadata_path: metadata_path.to_path_buf(),
            engine: ReplicaEngine::new(data_dir, metadata_path)
                .with_memory_limit(query_memory_limit),
            authentication: Mutex::new(None),
            session: Mutex::new(Session::default()),
            prepared: BTreeMap::new(),
            next_statement_id: 1,
            connection_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            salt: random_salt(),
        }
    }

    fn authenticated(&self) -> io::Result<Authenticated> {
        self.authentication
            .lock()
            .map_err(io_other)?
            .clone()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "authentication required")
            })
    }

    fn authenticate_wire_key(
        &self,
        username: &[u8],
        salt: &[u8],
        response: &[u8],
    ) -> io::Result<bool> {
        let Some(authenticated) = self.verify_wire_key(username, salt, response, None)? else {
            return Ok(false);
        };
        *self.authentication.lock().map_err(io_other)? = Some(authenticated);
        Ok(true)
    }

    fn verify_wire_key(
        &self,
        username: &[u8],
        salt: &[u8],
        response: &[u8],
        requested_database: Option<&[u8]>,
    ) -> io::Result<Option<Authenticated>> {
        let Ok(username) = std::str::from_utf8(username) else {
            return Ok(None);
        };
        let metadata = MetaStore::open(&self.metadata_path).map_err(io_other)?;
        let Some(database) = metadata
            .databases()
            .map_err(io_other)?
            .into_iter()
            .find(|database| database.name.eq_ignore_ascii_case(username))
        else {
            return Ok(None);
        };
        if requested_database.is_some_and(|requested| {
            !requested.is_empty()
                && !database.name.as_bytes().eq_ignore_ascii_case(requested)
                && !requested.eq_ignore_ascii_case(b"information_schema")
        }) {
            return Ok(None);
        }
        let key = metadata
            .api_keys(&database.id)
            .map_err(io_other)?
            .into_iter()
            .find(|key| wire_key_is_valid(key, salt, response));
        let Some(key) = key else {
            return Ok(None);
        };
        metadata
            .touch_api_key(&key.id, &Utc::now().to_rfc3339())
            .map_err(io_other)?;
        Ok(Some(Authenticated {
            database_id: database.id,
            database_name: database.name,
        }))
    }

    fn reset_session_state(&mut self) -> io::Result<()> {
        *self.session.lock().map_err(io_other)? = Session::default();
        self.prepared.clear();
        self.next_statement_id = 1;
        Ok(())
    }

    async fn execute(&self, sql: &str) -> Result<QueryOutput, QueryError> {
        let authenticated = self
            .authenticated()
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let session = self
            .session
            .lock()
            .map_err(|error| QueryError::Internal(error.to_string()))?
            .clone();
        if let Some(output) = compatibility_query(sql, &authenticated.database_name, &session) {
            return Ok(output);
        }

        let deadline = (session.max_execution_time_ms > 0)
            .then(|| {
                Instant::now().checked_add(Duration::from_millis(session.max_execution_time_ms))
            })
            .flatten();
        let cancellation = pintail_exec::ExecutionCancellation::new();
        let mut cancel_on_drop = CancelExecutionOnDrop::new(cancellation.clone());
        let engine = self.engine.clone();
        let database_id = authenticated.database_id;
        let sql = sql.to_owned();
        let execution = tokio::task::spawn_blocking(move || {
            pintail_exec::with_execution_cancellation(cancellation, || {
                // The session zone shifts statement-pinned time functions;
                // optimization runs on this thread, so install-and-restore
                // brackets exactly one statement.
                let _ = pintail_exec::set_session_time_zone(Some(&session.time_zone));
                pintail_exec::set_session_group_concat_max_len(Some(session.group_concat_max_len));
                pintail_exec::set_session_cte_max_recursion_depth(Some(
                    session.cte_max_recursion_depth,
                ));
                let result =
                    engine.execute_with_deadline(&database_id, &sql, DEFAULT_MAX_ROWS, deadline);
                let warnings = pintail_exec::take_session_group_concat_warnings();
                pintail_exec::set_session_group_concat_max_len(None);
                pintail_exec::set_session_cte_max_recursion_depth(None);
                let _ = pintail_exec::set_session_time_zone(None);
                (result, warnings)
            })
        })
        .await
        .map_err(|error| QueryError::Internal(format!("query worker failed: {error}")))?;
        cancel_on_drop.disarm();
        if let Ok(mut current) = self.session.lock() {
            current.group_concat_warnings = execution.1;
        }
        execution.0
    }

    /// The two session fields every result path needs to render a
    /// `Column`. `None` only on a poisoned lock, meaning a prior panic while
    /// holding it — vanishingly rare, and safer surfaced as an explicit
    /// error to the client than propagated as a connection-ending one.
    fn session_snapshot(&self) -> Option<(usize, String)> {
        let session = self.session.lock().ok()?;
        Some((
            session.group_concat_max_len,
            session.charset_results.clone(),
        ))
    }

    /// Applies one `SET`/`SET NAMES` session command, or reports why it
    /// cannot be honored.
    fn apply_session_command(&self, sql: &str) -> Result<(), String> {
        let command = sql.trim().trim_end_matches(';').trim();
        let lowered = command.to_ascii_lowercase();
        let mut session = self.session.lock().map_err(|error| error.to_string())?;
        if let Some(rest) = lowered.strip_prefix("set names ") {
            let charset = rest.split_whitespace().next().unwrap_or("");
            let charset = charset.trim_matches(['\'', '"', '`']);
            if matches!(charset, "utf8" | "utf8mb3" | "utf8mb4" | "binary") {
                charset.clone_into(&mut session.charset_client);
                charset.clone_into(&mut session.charset_connection);
                charset.clone_into(&mut session.charset_results);
                return Ok(());
            }
            return Err(format!("Unknown character set: '{charset}'"));
        }
        let assignment = lowered.strip_prefix("set ").map(|rest| {
            rest.trim_start_matches("session ")
                .trim_start_matches("local ")
        });
        let Some(assignment) = assignment else {
            return Ok(());
        };
        let Some((name, _)) = assignment.split_once('=') else {
            return Ok(());
        };
        let name = name
            .trim()
            .trim_start_matches("@@session.")
            .trim_start_matches("@@");
        // Values come from the ORIGINAL text to preserve case (zone names).
        let value = command
            .split_once('=')
            .map_or("", |(_, value)| value)
            .trim()
            .trim_matches(['\'', '"'])
            .to_owned();
        match name {
            "time_zone" => {
                if pintail_exec::set_session_time_zone(Some(&value)) {
                    let _ = pintail_exec::set_session_time_zone(None);
                    session.time_zone = value;
                    Ok(())
                } else {
                    Err(format!("Unknown or incorrect time zone: '{value}'"))
                }
            }
            "sql_mode" => {
                reject_unsupported_sql_modes(&value)?;
                session.sql_mode = value;
                Ok(())
            }
            name @ ("character_set_client"
            | "character_set_connection"
            | "character_set_results") => {
                let charset = value.to_ascii_lowercase();
                if matches!(charset.as_str(), "utf8" | "utf8mb3" | "utf8mb4" | "binary") {
                    match name {
                        "character_set_client" => session.charset_client = charset,
                        "character_set_connection" => session.charset_connection = charset,
                        "character_set_results" => session.charset_results = charset,
                        _ => unreachable!(),
                    }
                    Ok(())
                } else {
                    Err(format!("Unknown character set: '{value}'"))
                }
            }
            "group_concat_max_len" => {
                let limit = value
                    .parse::<u64>()
                    .ok()
                    .and_then(|limit| usize::try_from(limit).ok())
                    .ok_or_else(|| "group_concat_max_len must be an unsigned integer".to_owned())?;
                session.group_concat_max_len = limit.max(4);
                Ok(())
            }
            "cte_max_recursion_depth" => {
                let limit = value
                    .parse::<u64>()
                    .map_err(|_| "cte_max_recursion_depth must be a positive integer".to_owned())?;
                if !(1..=1_000_000).contains(&limit) {
                    return Err("cte_max_recursion_depth must be between 1 and 1000000".to_owned());
                }
                session.cte_max_recursion_depth = limit;
                Ok(())
            }
            "max_execution_time" => {
                let limit = value.parse::<u64>().map_err(|_| {
                    "max_execution_time must be an unsigned millisecond count".to_owned()
                })?;
                if limit > u64::from(u32::MAX) {
                    return Err(format!(
                        "max_execution_time must be between 0 and {}",
                        u32::MAX
                    ));
                }
                session.max_execution_time_ms = limit;
                Ok(())
            }
            // Everything else keeps the accepted-no-op compatibility
            // behavior (autocommit, isolation levels, probes).
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl Handler for Backend {
    fn server_version(&self) -> String {
        format!("8.4.0-pintail-{}", env!("CARGO_PKG_VERSION"))
    }

    fn connection_id(&self) -> u32 {
        self.connection_id
    }

    // caching_sha2_password's fast-auth path (a single round trip against a
    // cached verifier) is what wire_key_is_valid implements; the full
    // RSA-key-exchange path a fresh connection can fall back to is not.
    // mysql_native_password needs no such fallback and every MySQL client
    // library speaks it, so it is the safer plugin to advertise even though
    // the previous implementation defaulted to caching_sha2_password.
    fn auth_plugin(&self) -> &'static str {
        "mysql_native_password"
    }

    async fn authenticate(&mut self, response: &HandshakeResponse, scramble: &[u8]) -> bool {
        self.authenticate_wire_key(&response.username, scramble, &response.auth_response)
            .unwrap_or(false)
    }

    async fn query(&mut self, sql: &[u8]) -> Response {
        let Ok(sql) = std::str::from_utf8(sql) else {
            return Response::Error(
                ErrorKind::ErParseError,
                "statement is not valid UTF-8".to_owned(),
            );
        };
        if is_session_command(sql) {
            return match self.apply_session_command(sql) {
                Ok(()) => Response::Ok(OkPacket::default(), String::new()),
                Err(error) => Response::Error(ErrorKind::ErWrongArguments, error),
            };
        }
        let Some((group_concat_max_len, charset)) = self.session_snapshot() else {
            return Response::Error(
                ErrorKind::ErUnknownError,
                "session state is unavailable".to_owned(),
            );
        };
        query_output_to_response(
            Backend::execute(self, sql).await,
            group_concat_max_len,
            &charset,
            false,
        )
    }

    async fn prepare(&mut self, sql: &[u8]) -> Result<PreparedStatement, (ErrorKind, String)> {
        let sql = std::str::from_utf8(sql).map_err(|_| {
            (
                ErrorKind::ErParseError,
                "statement is not valid UTF-8".to_owned(),
            )
        })?;
        let parameters = placeholder_count(sql);
        let preview = substitute_parameters(sql, &placeholder_preview_literals(sql))
            .map_err(|error| (ErrorKind::ErParseError, error))?;
        let output = Backend::execute(self, &preview)
            .await
            .map_err(|error| (error_kind(&error), error.to_string()))?;
        let statement_id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1).max(1);
        self.prepared.insert(
            statement_id,
            Prepared {
                sql: sql.to_owned(),
                parameters,
                parameter_types: None,
                used_long_data: false,
            },
        );
        let (group_concat_max_len, charset) = self.session_snapshot().ok_or_else(|| {
            (
                ErrorKind::ErUnknownError,
                "session state is unavailable".to_owned(),
            )
        })?;
        let params = (0..parameters)
            .map(|index| {
                let mut column = Column::new(
                    format!("param_{}", index + 1),
                    ColumnType::MysqlTypeVarString,
                );
                column.column_length = 1024;
                column.character_set = mysql_text_character_set(&charset);
                column
            })
            .collect::<Vec<_>>();
        let columns = output
            .fields
            .iter()
            .map(|field| mysql_column(field, group_concat_max_len, &charset))
            .collect::<Vec<_>>();
        Ok(PreparedStatement {
            id: statement_id,
            parameters: params,
            columns,
        })
    }

    async fn execute(&mut self, id: u32, body: &[u8]) -> Response {
        let Some(statement) = self.prepared.get(&id).cloned() else {
            return Response::Error(
                ErrorKind::ErUnknownStmtHandler,
                "unknown prepared statement".to_owned(),
            );
        };
        if statement.used_long_data {
            return Response::Error(
                ErrorKind::ErWrongArguments,
                "COM_STMT_SEND_LONG_DATA is not supported".to_owned(),
            );
        }
        let Some((values, types)) = decode_execute_parameters(
            body,
            statement.parameters,
            statement.parameter_types.as_deref(),
        ) else {
            return Response::Error(
                ErrorKind::ErWrongArguments,
                "malformed EXECUTE parameters".to_owned(),
            );
        };
        if let Some(entry) = self.prepared.get_mut(&id) {
            entry.parameter_types = Some(types);
        }
        let literals = match values
            .iter()
            .map(parameter_literal)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(literals) => literals,
            Err(error) => return Response::Error(ErrorKind::ErWrongArguments, error),
        };
        let query = match substitute_parameters(&statement.sql, &literals) {
            Ok(query) => query,
            Err(error) => return Response::Error(ErrorKind::ErWrongArguments, error),
        };
        let Some((group_concat_max_len, charset)) = self.session_snapshot() else {
            return Response::Error(
                ErrorKind::ErUnknownError,
                "session state is unavailable".to_owned(),
            );
        };
        query_output_to_response(
            Backend::execute(self, &query).await,
            group_concat_max_len,
            &charset,
            true,
        )
    }

    async fn send_long_data(&mut self, statement: u32, _parameter: u16, _data: &[u8]) {
        if let Some(entry) = self.prepared.get_mut(&statement) {
            entry.used_long_data = true;
        }
    }

    async fn close_statement(&mut self, statement: u32) {
        self.prepared.remove(&statement);
    }

    async fn reset_statement(&mut self, statement: u32) -> bool {
        let Some(entry) = self.prepared.get_mut(&statement) else {
            return false;
        };
        entry.used_long_data = false;
        true
    }

    async fn reset_connection(&mut self) {
        let _ = self.reset_session_state();
    }

    async fn init_database(&mut self, database: &[u8]) -> Result<(), (ErrorKind, String)> {
        let database = std::str::from_utf8(database).map_err(|_| {
            (
                ErrorKind::ErBadDbError,
                "database name is not valid UTF-8".to_owned(),
            )
        })?;
        let authenticated = self.authenticated().map_err(|_| {
            (
                ErrorKind::ErAccessDeniedError,
                "authentication required".to_owned(),
            )
        })?;
        if authenticated.database_name.eq_ignore_ascii_case(database)
            || database.eq_ignore_ascii_case("information_schema")
        {
            Ok(())
        } else {
            Err((
                ErrorKind::ErDbaccessDeniedError,
                "API key is scoped to another database".to_owned(),
            ))
        }
    }

    async fn change_user(
        &mut self,
        username: &[u8],
        auth_response: &[u8],
        database: &[u8],
    ) -> bool {
        let salt = self.salt;
        let Ok(Some(authenticated)) =
            self.verify_wire_key(username, &salt, auth_response, Some(database))
        else {
            return false;
        };
        if self.reset_session_state().is_err() {
            return false;
        }
        let Ok(mut current) = self.authentication.lock() else {
            return false;
        };
        *current = Some(authenticated);
        true
    }
}

/// Builds the response for one completed query, in whichever protocol the
/// command that asked for it uses: text for `COM_QUERY`, binary for
/// `COM_STMT_EXECUTE`.
fn query_output_to_response(
    result: Result<QueryOutput, QueryError>,
    group_concat_max_len: usize,
    charset: &str,
    binary: bool,
) -> Response {
    let output = match result {
        Ok(output) => output,
        Err(error) => return Response::Error(error_kind(&error), error.to_string()),
    };
    let columns = output
        .fields
        .iter()
        .map(|field| mysql_column(field, group_concat_max_len, charset))
        .collect::<Vec<_>>();
    let mut rows = Vec::with_capacity(output.rows.len());
    for row in &output.rows {
        let mut encoded = Vec::with_capacity(row.len());
        for (field, value) in output.fields.iter().zip(row) {
            let cell = if binary {
                match binary_column_value(field, value) {
                    Ok(cell) => cell,
                    Err(error) => {
                        return Response::Error(ErrorKind::ErUnknownError, error.to_string());
                    }
                }
            } else {
                text_column_value(value)
            };
            encoded.push(cell);
        }
        rows.push(encoded);
    }
    Response::Rows(Box::new(ResultSet {
        columns,
        rows,
        binary,
    }))
}

/// Renders one value in the text protocol: every `MySQL` client reads
/// `COM_QUERY` results as ASCII text regardless of the column's real type,
/// and Pintail's temporal/DECIMAL values already carry their canonical
/// `MySQL` text, so this is just picking the right `Display`.
fn text_column_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(i8::from(*value).to_string().into_bytes()),
        Value::Int64(value) => Some(value.to_string().into_bytes()),
        Value::UInt64(value) => Some(value.to_string().into_bytes()),
        Value::Float64(value) => Some(value.get().to_string().into_bytes()),
        Value::Utf8(value) => Some(value.clone().into_bytes()),
        Value::Binary(value) => Some(value.clone()),
    }
}

/// Renders one value in the binary protocol: fixed-width bytes for numeric
/// and temporal columns, length-encoded text for everything else. Getting a
/// column's width wrong here is silent — the client decodes *something*,
/// just not the value that was meant.
fn binary_column_value(field: &QueryField, value: &Value) -> io::Result<Option<Vec<u8>>> {
    let length_encoded = |bytes: &[u8]| {
        let mut encoded = Vec::new();
        put_length_encoded_bytes(&mut encoded, bytes);
        encoded
    };
    Ok(Some(match (field.data_type, value) {
        (_, Value::Null) => return Ok(None),
        (_, Value::Boolean(value)) => encode_binary_int(i64::from(*value), IntWidth::Tiny),
        (Some(DataType::Int8), Value::Int64(value)) => encode_binary_int(*value, IntWidth::Tiny),
        (Some(DataType::Int16), Value::Int64(value)) => encode_binary_int(*value, IntWidth::Short),
        (Some(DataType::Int32), Value::Int64(value)) => encode_binary_int(*value, IntWidth::Long),
        (_, Value::Int64(value)) => encode_binary_int(*value, IntWidth::LongLong),
        (Some(DataType::UInt8), Value::UInt64(value)) => {
            encode_binary_int(i64::from_le_bytes(value.to_le_bytes()), IntWidth::Tiny)
        }
        (Some(DataType::UInt16), Value::UInt64(value)) => {
            encode_binary_int(i64::from_le_bytes(value.to_le_bytes()), IntWidth::Short)
        }
        (Some(DataType::UInt32), Value::UInt64(value)) => {
            encode_binary_int(i64::from_le_bytes(value.to_le_bytes()), IntWidth::Long)
        }
        (_, Value::UInt64(value)) => {
            encode_binary_int(i64::from_le_bytes(value.to_le_bytes()), IntWidth::LongLong)
        }
        (Some(DataType::Float32), Value::Float64(value)) => {
            let value = value.get().to_string().parse::<f32>().map_err(io_invalid)?;
            value.to_le_bytes().to_vec()
        }
        (_, Value::Float64(value)) => value.get().to_le_bytes().to_vec(),
        (Some(DataType::Date32), Value::Utf8(value)) => {
            let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(io_invalid)?;
            encode_binary_datetime(
                u16::try_from(date.year()).unwrap_or(0),
                u8::try_from(date.month()).map_err(io_invalid)?,
                u8::try_from(date.day()).map_err(io_invalid)?,
                None,
            )
        }
        (Some(DataType::DateTime64 { .. }), Value::Utf8(value)) => {
            let datetime = parse_datetime(value).ok_or_else(|| {
                io_invalid(format!("invalid canonical MySQL DATETIME value: {value}"))
            })?;
            let micros = datetime.and_utc().timestamp_subsec_micros();
            encode_binary_datetime(
                u16::try_from(datetime.year()).unwrap_or(0),
                u8::try_from(datetime.month()).map_err(io_invalid)?,
                u8::try_from(datetime.day()).map_err(io_invalid)?,
                Some((
                    u8::try_from(datetime.hour()).map_err(io_invalid)?,
                    u8::try_from(datetime.minute()).map_err(io_invalid)?,
                    u8::try_from(datetime.second()).map_err(io_invalid)?,
                    Some(micros),
                )),
            )
        }
        (Some(DataType::Time64 { .. }), Value::Utf8(value)) => {
            let time = MysqlTimeValue::parse(value).map_err(io_invalid)?;
            encode_binary_time(
                time.negative,
                time.days,
                time.hours,
                time.minutes,
                time.seconds,
                Some(time.micros),
            )
        }
        (_, Value::Utf8(value)) => length_encoded(value.as_bytes()),
        (_, Value::Binary(value)) => length_encoded(value),
    }))
}

fn parse_datetime(value: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
}

struct MysqlTimeValue {
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
}

impl MysqlTimeValue {
    fn parse(raw: &str) -> Result<Self, String> {
        let (negative, unsigned) = raw
            .strip_prefix('-')
            .map_or((false, raw), |value| (true, value));
        let (clock, fraction) = unsigned
            .split_once('.')
            .map_or((unsigned, ""), |(clock, fraction)| (clock, fraction));
        let mut parts = clock.split(':');
        let total_hours = parts
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| format!("invalid canonical MySQL TIME value: {raw}"))?;
        let minutes = parts
            .next()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value < 60)
            .ok_or_else(|| format!("invalid canonical MySQL TIME value: {raw}"))?;
        let seconds = parts
            .next()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value < 60)
            .ok_or_else(|| format!("invalid canonical MySQL TIME value: {raw}"))?;
        if parts.next().is_some()
            || fraction.len() > 6
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(format!("invalid canonical MySQL TIME value: {raw}"));
        }
        let micros = if fraction.is_empty() {
            0
        } else {
            format!("{fraction:0<6}")
                .parse()
                .map_err(|_| format!("invalid canonical MySQL TIME value: {raw}"))?
        };
        Ok(Self {
            negative,
            days: total_hours / 24,
            hours: u8::try_from(total_hours % 24)
                .map_err(|_| format!("invalid canonical MySQL TIME value: {raw}"))?,
            minutes,
            seconds,
            micros,
        })
    }
}

fn mysql_text_character_set(charset: &str) -> u16 {
    match charset {
        "utf8" | "utf8mb3" => 33,
        "binary" => 63,
        _ => 255,
    }
}

fn mysql_column(field: &QueryField, group_concat_max_len: usize, charset: &str) -> Column {
    let (coltype, unsigned) = match field.data_type {
        Some(DataType::Utf8) if field.group_concat && group_concat_max_len > 512 => {
            (ColumnType::MysqlTypeBlob, false)
        }
        Some(DataType::Boolean | DataType::Int8 | DataType::UInt8) => (
            ColumnType::MysqlTypeTiny,
            matches!(field.data_type, Some(DataType::UInt8)),
        ),
        Some(DataType::Int16 | DataType::UInt16) => (
            ColumnType::MysqlTypeShort,
            matches!(field.data_type, Some(DataType::UInt16)),
        ),
        Some(DataType::Int32 | DataType::UInt32) => (
            ColumnType::MysqlTypeLong,
            matches!(field.data_type, Some(DataType::UInt32)),
        ),
        Some(DataType::Int64 | DataType::UInt64) => (
            ColumnType::MysqlTypeLonglong,
            matches!(field.data_type, Some(DataType::UInt64)),
        ),
        Some(DataType::Float32) => (ColumnType::MysqlTypeFloat, false),
        Some(DataType::Float64) => (ColumnType::MysqlTypeDouble, false),
        Some(DataType::Decimal { .. }) => (ColumnType::MysqlTypeNewdecimal, false),
        Some(DataType::Date32) => (ColumnType::MysqlTypeDate, false),
        Some(DataType::DateTime64 { .. }) => (ColumnType::MysqlTypeDatetime, false),
        Some(DataType::Time64 { .. }) => (ColumnType::MysqlTypeTime, false),
        Some(DataType::Year) => (ColumnType::MysqlTypeYear, true),
        Some(DataType::Binary) => (ColumnType::MysqlTypeBlob, false),
        Some(DataType::Json) => (ColumnType::MysqlTypeJson, false),
        Some(DataType::Utf8) | None => (ColumnType::MysqlTypeVarString, false),
    };
    let mut colflags = ColumnFlags::empty();
    colflags.set(ColumnFlags::UNSIGNED_FLAG, unsigned);
    colflags.set(ColumnFlags::NOT_NULL_FLAG, !field.nullable);
    // Binary values and a binary result character set carry both charset 63
    // and the binary flag, matching what clients use for byte-oriented fields.
    colflags.set(
        ColumnFlags::BINARY_FLAG,
        matches!(field.data_type, Some(DataType::Binary))
            || (field.data_type == Some(DataType::Utf8) && charset == "binary"),
    );
    let decimals = match field.data_type {
        Some(
            DataType::Decimal { scale, .. }
            | DataType::DateTime64 { fsp: scale }
            | DataType::Time64 { fsp: scale },
        ) => scale,
        Some(DataType::Float32 | DataType::Float64) => 31,
        _ => 0,
    };
    let column_length = match field.data_type {
        Some(DataType::Boolean | DataType::Int8 | DataType::UInt8 | DataType::Year) => 4,
        Some(DataType::Int16 | DataType::UInt16) => 6,
        Some(DataType::Int32 | DataType::UInt32) => 11,
        Some(DataType::Int64 | DataType::UInt64) => 20,
        Some(DataType::Float32) => 12,
        Some(DataType::Float64) => 22,
        Some(DataType::Decimal { precision, scale }) => {
            u32::from(precision) + 1 + u32::from(scale > 0)
        }
        Some(DataType::Date32) => 10,
        Some(DataType::DateTime64 { fsp }) => 19 + u32::from(fsp > 0) + u32::from(fsp),
        Some(DataType::Time64 { fsp }) => 10 + u32::from(fsp > 0) + u32::from(fsp),
        Some(DataType::Utf8) if field.group_concat => {
            u32::try_from(group_concat_max_len).unwrap_or(u32::MAX)
        }
        Some(DataType::Utf8 | DataType::Binary | DataType::Json) | None => 1024,
    };
    // Text results follow the connection's result charset; numeric, temporal,
    // binary and JSON results use MySQL's binary character set (63).
    let character_set = if matches!(field.data_type, Some(DataType::Utf8)) {
        mysql_text_character_set(charset)
    } else {
        63
    };
    let mut column = Column::new(field.name.clone(), coltype);
    column.column_length = column_length;
    column.character_set = character_set;
    column.colflags = colflags;
    column.decimals = decimals;
    column
}

fn wire_key_is_valid(key: &ApiKeyRecord, salt: &[u8], response: &[u8]) -> bool {
    if !key.enabled
        || key.expires_at.as_deref().is_some_and(is_expired)
        || !key_has_query_scope(key)
    {
        return false;
    }
    match response.len() {
        20 => key
            .mysql_native_password_hash
            .as_deref()
            .is_some_and(|expected| verify_native_password(expected, salt, response)),
        32 => key
            .caching_sha2_password_hash
            .as_deref()
            .is_some_and(|expected| verify_caching_sha2(expected, salt, response)),
        _ => false,
    }
}

fn key_has_query_scope(key: &ApiKeyRecord) -> bool {
    serde_json::from_str::<Vec<String>>(&key.scopes_json).is_ok_and(|scopes| {
        scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "*" | "query"))
    })
}

/// Verifies a `caching_sha2_password` fast-auth response against the stored
/// `SHA256(SHA256(password))` verifier.
///
/// The client sends `XOR(SHA256(password), SHA256(SHA256(SHA256(password)) || nonce))`,
/// so XOR-ing the response with `SHA256(verifier || nonce)` recovers a candidate
/// `SHA256(password)` whose hash must equal the verifier.
fn verify_caching_sha2(expected: &[u8], nonce: &[u8], response: &[u8]) -> bool {
    if expected.len() != 32 || response.len() != 32 {
        return false;
    }
    let challenge = Sha256::digest([expected, nonce].concat());
    let mut stage_one = [0_u8; 32];
    for (index, output) in stage_one.iter_mut().enumerate() {
        *output = response[index] ^ challenge[index];
    }
    let candidate = Sha256::digest(stage_one);
    constant_time_equal(candidate.as_slice(), expected)
}

fn verify_native_password(expected: &[u8], salt: &[u8], response: &[u8]) -> bool {
    if expected.len() != 20 || response.len() != 20 {
        return false;
    }
    let challenge = Sha1::digest([salt, expected].concat());
    let mut stage_one = [0_u8; 20];
    for (index, output) in stage_one.iter_mut().enumerate() {
        *output = response[index] ^ challenge[index];
    }
    let candidate = Sha1::digest(stage_one);
    constant_time_equal(candidate.as_slice(), expected)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn random_salt() -> [u8; 20] {
    let mut salt = [0_u8; 20];
    rand::rng().fill_bytes(&mut salt);
    for byte in &mut salt {
        if matches!(*byte, 0 | b'$') {
            *byte = byte.wrapping_add(1);
        }
    }
    salt
}

fn is_expired(value: &str) -> bool {
    DateTime::parse_from_rfc3339(value).map_or(true, |expires| expires <= Utc::now())
}

fn error_kind(error: &QueryError) -> ErrorKind {
    match error {
        QueryError::DatabaseNotFound => ErrorKind::ErBadDbError,
        // 1290 is what real MySQL's --read-only mode actually raises.
        QueryError::Invalid(message) if message.contains("read-only") => {
            ErrorKind::ErOptionPreventsStatement
        }
        QueryError::Invalid(_) => ErrorKind::ErParseError,
        QueryError::Interrupted => ErrorKind::ErQueryInterrupted,
        QueryError::Overloaded => ErrorKind::ErConCountError,
        QueryError::NotReady(_) | QueryError::Internal(_) => ErrorKind::ErUnknownError,
    }
}

fn compatibility_query(sql: &str, database: &str, session: &Session) -> Option<QueryOutput> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    if normalized.starts_with("show warnings") {
        return Some(group_concat_warnings_output(session));
    }
    let (name, value) = if normalized.starts_with("select version()") {
        ("VERSION()", Value::Utf8(mysql_compat_version()))
    } else if normalized.starts_with("select database()") {
        ("DATABASE()", Value::Utf8(database.to_owned()))
    } else if normalized.contains("@@version_comment") {
        (
            "@@version_comment",
            Value::Utf8("Pintail analytical mirror".to_owned()),
        )
    } else if normalized.contains("@@version") {
        ("@@version", Value::Utf8(mysql_compat_version()))
    } else if normalized.contains("@@max_allowed_packet") {
        ("@@max_allowed_packet", Value::UInt64(64 * 1024 * 1024))
    } else if normalized.contains("@@lower_case_table_names") {
        // Catalog names retain their source spelling but resolve
        // case-insensitively, matching MySQL mode 2.
        ("@@lower_case_table_names", Value::UInt64(2))
    } else if normalized.contains("@@group_concat_max_len") {
        (
            "@@group_concat_max_len",
            Value::UInt64(u64::try_from(session.group_concat_max_len).unwrap_or(u64::MAX)),
        )
    } else if normalized.contains("@@warning_count") {
        (
            "@@warning_count",
            Value::UInt64(session.group_concat_warnings),
        )
    } else if normalized.contains("@@cte_max_recursion_depth") {
        (
            "@@cte_max_recursion_depth",
            Value::UInt64(session.cte_max_recursion_depth),
        )
    } else if normalized.contains("@@max_execution_time") {
        (
            "@@max_execution_time",
            Value::UInt64(session.max_execution_time_ms),
        )
    } else if normalized.contains("@@session.time_zone") || normalized.contains("@@time_zone") {
        (
            "@@session.time_zone",
            Value::Utf8(session.time_zone.clone()),
        )
    } else if normalized.contains("@@sql_mode") {
        ("@@sql_mode", Value::Utf8(session.sql_mode.clone()))
    } else {
        compatibility_charset_query(&normalized, session)?
    };
    Some(QueryOutput {
        fields: vec![QueryField {
            name: name.to_owned(),
            data_type: value.data_type(),
            nullable: false,
            collation: (value.data_type() == Some(DataType::Utf8))
                .then(|| DEFAULT_TEXT_COLLATION.to_owned()),
            group_concat: false,
        }],
        rows: vec![vec![value]],
        stats: QueryStats {
            rows: 1,
            ..QueryStats::default()
        },
        truncated: false,
    })
}

fn group_concat_warnings_output(session: &Session) -> QueryOutput {
    QueryOutput {
        fields: vec![
            QueryField {
                name: "Level".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: false,
                collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
            },
            QueryField {
                name: "Code".to_owned(),
                data_type: Some(DataType::UInt64),
                nullable: false,
                collation: None,
                group_concat: false,
            },
            QueryField {
                name: "Message".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: false,
                collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
            },
        ],
        rows: (1..=session.group_concat_warnings)
            .map(|row| {
                vec![
                    Value::Utf8("Warning".to_owned()),
                    Value::UInt64(1260),
                    Value::Utf8(format!("Row {row} was cut by GROUP_CONCAT()")),
                ]
            })
            .collect(),
        stats: QueryStats {
            rows: usize::try_from(session.group_concat_warnings).unwrap_or(usize::MAX),
            ..QueryStats::default()
        },
        truncated: false,
    }
}

fn mysql_compat_version() -> String {
    format!("8.4.0-pintail-{}", env!("CARGO_PKG_VERSION"))
}

fn compatibility_charset_query(
    normalized: &str,
    session: &Session,
) -> Option<(&'static str, Value)> {
    let (name, value) = if normalized.contains("@@character_set_client") {
        ("@@character_set_client", session.charset_client.clone())
    } else if normalized.contains("@@character_set_connection") {
        (
            "@@character_set_connection",
            session.charset_connection.clone(),
        )
    } else if normalized.contains("@@character_set_results") {
        ("@@character_set_results", session.charset_results.clone())
    } else if normalized.contains("@@collation_connection") {
        let collation = match session.charset_connection.as_str() {
            "utf8" | "utf8mb3" => "utf8mb3_general_ci",
            "binary" => "binary",
            _ => "utf8mb4_0900_ai_ci",
        };
        ("@@collation_connection", collation.to_owned())
    } else {
        return None;
    };
    Some((name, Value::Utf8(value)))
}

fn is_session_command(sql: &str) -> bool {
    let command = sql.trim_start().to_ascii_lowercase();
    ["set ", "begin", "start transaction", "commit", "rollback"]
        .iter()
        .any(|prefix| command.starts_with(prefix))
}

fn placeholder_count(sql: &str) -> usize {
    placeholder_offsets(sql).len()
}

/// Prepared-statement metadata is derived before parameter types or values are
/// available. `NULL` is the least opinionated preview literal for ordinary
/// expressions, but `MySQL` requires `LIMIT`/`OFFSET` to be integer-valued even at
/// prepare time. Track clause context at each nesting depth so only pagination
/// placeholders receive a zero preview.
fn placeholder_preview_literals(sql: &str) -> Vec<String> {
    let code = sql_code_only(sql);
    let mut previews = Vec::new();
    let mut limit_context = vec![false];
    let mut word_start = None;
    for index in 0..=code.len() {
        let byte = code.get(index).copied();
        if byte.is_some_and(|value| value.is_ascii_alphanumeric() || value == b'_') {
            word_start.get_or_insert(index);
            continue;
        }
        if let Some(start) = word_start.take() {
            let word = String::from_utf8_lossy(&code[start..index]).to_ascii_lowercase();
            let context = limit_context.last_mut().expect("root preview context");
            if matches!(word.as_str(), "limit" | "offset") {
                *context = true;
            } else if matches!(
                word.as_str(),
                "select"
                    | "from"
                    | "where"
                    | "group"
                    | "having"
                    | "order"
                    | "union"
                    | "intersect"
                    | "except"
                    | "on"
                    | "window"
                    | "qualify"
            ) {
                *context = false;
            }
        }
        match byte {
            Some(b'(') => limit_context.push(false),
            Some(b')') if limit_context.len() > 1 => {
                limit_context.pop();
            }
            Some(b'?') => previews.push(
                if *limit_context.last().expect("root preview context") {
                    "0"
                } else {
                    "NULL"
                }
                .to_owned(),
            ),
            None => break,
            _ => {}
        }
    }
    previews
}

fn substitute_parameters(sql: &str, parameters: &[String]) -> Result<String, String> {
    let offsets = placeholder_offsets(sql);
    if offsets.len() != parameters.len() {
        return Err("prepared statement parameter count does not match".to_owned());
    }
    let mut output =
        String::with_capacity(sql.len() + parameters.iter().map(String::len).sum::<usize>());
    let mut start = 0;
    for (offset, value) in offsets.into_iter().zip(parameters) {
        output.push_str(&sql[start..offset]);
        output.push_str(value);
        start = offset + 1;
    }
    output.push_str(&sql[start..]);
    Ok(output)
}

/// Blanks every byte that is not executable SQL — string literals, quoted
/// identifiers and comments — while preserving byte length so offsets into
/// the result still index the original statement.
///
/// `MySQL` executes the body of a version comment (`/*! ... */`), so its
/// contents stay visible; a `?` in there really is a parameter.
fn sql_code_only(sql: &str) -> Vec<u8> {
    let bytes = sql.as_bytes();
    let mut code = vec![b' '; bytes.len()];
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            delimiter @ (b'\'' | b'"' | b'`') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    if bytes[index] == delimiter {
                        if bytes.get(index + 1) == Some(&delimiter) {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-')
                && matches!(
                    bytes.get(index + 2),
                    None | Some(b' ' | b'\t' | b'\n' | b'\r')
                ) =>
            {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let executable = bytes.get(index + 2) == Some(&b'!');
                index += if executable { 3 } else { 2 };
                while index < bytes.len() {
                    if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        index += 2;
                        break;
                    }
                    if executable {
                        code[index] = bytes[index];
                    }
                    index += 1;
                }
            }
            byte => {
                code[index] = byte;
                index += 1;
            }
        }
    }
    code
}

fn placeholder_offsets(sql: &str) -> Vec<usize> {
    sql_code_only(sql)
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'?')
        .map(|(index, _)| index)
        .collect()
}

/// Renders one decoded EXECUTE parameter as a `SQL` literal, substituted
/// directly into the prepared statement's text.
///
/// `DateTime`/`Time` need no re-parsing of the raw binary payload here: the
/// crate's own decoder already rendered exact `MySQL` text for them —
/// including the length-dependent shape and fractional seconds — so this is
/// just quoting a string that is already correct, not a second
/// implementation of the same layout rules that could drift from the first.
fn parameter_literal(value: &BinaryValue) -> Result<String, String> {
    match value {
        BinaryValue::Null => Ok("NULL".to_owned()),
        BinaryValue::Int(value) => Ok(value.to_string()),
        BinaryValue::UInt(value) => Ok(value.to_string()),
        BinaryValue::Float(value) if value.is_finite() => Ok(value.to_string()),
        BinaryValue::Double(value) if value.is_finite() => Ok(value.to_string()),
        BinaryValue::Float(_) | BinaryValue::Double(_) => {
            Err("non-finite prepared parameters are unsupported".to_owned())
        }
        BinaryValue::Bytes(value) => std::str::from_utf8(value).map_or_else(
            |_| Ok(format!("X'{}'", encode_hex(value))),
            |value| {
                Ok(format!(
                    "'{}'",
                    value.replace('\\', "\\\\").replace('\'', "''")
                ))
            },
        ),
        BinaryValue::DateTime(text) | BinaryValue::Time(text) => Ok(format!("'{text}'")),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn io_other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn io_invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

#[cfg(test)]
mod tests {
    use pintail_protocol::value::decode_binary_value;
    use pintail_protocol::{ColumnFlags, ColumnType, ParameterType};
    use pintail_sql::DEFAULT_TEXT_COLLATION;
    use pintail_types::DataType;
    use sha1::{Digest as _, Sha1};
    use sha2::Digest as _;

    use super::{
        Session, Sha256, compatibility_query, mysql_column, placeholder_offsets,
        placeholder_preview_literals, substitute_parameters, verify_caching_sha2,
        verify_native_password,
    };
    use crate::QueryField;

    #[test]
    fn json_results_advertise_mysql_json_metadata() {
        let column = mysql_column(
            &QueryField {
                name: "document".to_owned(),
                data_type: Some(DataType::Json),
                nullable: true,
                collation: None,
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(column.coltype, ColumnType::MysqlTypeJson);
    }

    #[test]
    fn result_columns_preserve_numeric_binary_and_nullability_flags() {
        let unsigned = mysql_column(
            &QueryField {
                name: "ordinal_position".to_owned(),
                data_type: Some(DataType::UInt64),
                nullable: false,
                collation: None,
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(unsigned.coltype, ColumnType::MysqlTypeLonglong);
        assert!(unsigned.colflags.contains(ColumnFlags::UNSIGNED_FLAG));
        assert!(unsigned.colflags.contains(ColumnFlags::NOT_NULL_FLAG));

        let nullable_text = mysql_column(
            &QueryField {
                name: "column_default".to_owned(),
                data_type: Some(DataType::Utf8),
                nullable: true,
                collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(nullable_text.coltype, ColumnType::MysqlTypeVarString);
        assert!(!nullable_text.colflags.contains(ColumnFlags::NOT_NULL_FLAG));

        let binary = mysql_column(
            &QueryField {
                name: "payload".to_owned(),
                data_type: Some(DataType::Binary),
                nullable: true,
                collation: None,
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(binary.coltype, ColumnType::MysqlTypeBlob);
        assert!(binary.colflags.contains(ColumnFlags::BINARY_FLAG));
        assert_eq!(nullable_text.character_set, 255);
        assert_eq!(binary.character_set, 63);
    }

    #[test]
    fn result_columns_report_decimal_and_temporal_scale() {
        let decimal = mysql_column(
            &QueryField {
                name: "amount".to_owned(),
                data_type: Some(DataType::Decimal {
                    precision: 18,
                    scale: 4,
                }),
                nullable: false,
                collation: None,
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(decimal.decimals, 4);
        assert_eq!(decimal.column_length, 20);
        assert_eq!(decimal.character_set, 63);

        let datetime = mysql_column(
            &QueryField {
                name: "created_at".to_owned(),
                data_type: Some(DataType::DateTime64 { fsp: 6 }),
                nullable: false,
                collation: None,
                group_concat: false,
            },
            1024,
            "utf8mb4",
        );
        assert_eq!(datetime.decimals, 6);
        assert_eq!(datetime.column_length, 26);
    }

    #[test]
    fn group_concat_metadata_follows_the_session_limit_threshold() {
        let field = QueryField {
            name: "labels".to_owned(),
            data_type: Some(DataType::Utf8),
            nullable: true,
            collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
            group_concat: true,
        };
        assert_eq!(
            mysql_column(&field, 512, "utf8mb4").coltype,
            ColumnType::MysqlTypeVarString
        );
        assert_eq!(
            mysql_column(&field, 513, "utf8mb4").coltype,
            ColumnType::MysqlTypeBlob
        );
    }

    #[test]
    fn text_result_metadata_follows_the_session_charset() {
        let field = QueryField {
            name: "label".to_owned(),
            data_type: Some(DataType::Utf8),
            nullable: false,
            collation: Some(DEFAULT_TEXT_COLLATION.to_owned()),
            group_concat: false,
        };
        assert_eq!(mysql_column(&field, 1024, "utf8mb3").character_set, 33);
        let binary = mysql_column(&field, 1024, "binary");
        assert_eq!(binary.character_set, 63);
        assert!(binary.colflags.contains(ColumnFlags::BINARY_FLAG));
        assert_eq!(mysql_column(&field, 1024, "utf8mb4").character_set, 255);
    }

    #[test]
    fn renders_temporal_prepared_parameters_as_mysql_literals() {
        // Decodes the same raw EXECUTE-parameter bytes the wire actually
        // sends, through the crate's own decoder, then through
        // parameter_literal — the full path pintail-wire uses, not a
        // reimplementation of the byte layout to test against itself.
        let literal = |tag, body: &[u8]| {
            let parameter = ParameterType {
                column_type: tag,
                unsigned: false,
            };
            let (value, _) = decode_binary_value(parameter, body).expect("decode");
            super::parameter_literal(&value).expect("literal")
        };
        let date_tag = ColumnType::MysqlTypeDate as u8;
        let time_tag = ColumnType::MysqlTypeTime as u8;

        assert_eq!(literal(date_tag, &[4, 0xE8, 0x07, 2, 29]), "'2024-02-29'");
        assert_eq!(
            literal(date_tag, &[7, 0xE8, 0x07, 2, 29, 12, 34, 56]),
            "'2024-02-29 12:34:56'"
        );
        assert_eq!(
            literal(
                date_tag,
                &[11, 0xE8, 0x07, 2, 29, 12, 34, 56, 0x40, 0xE2, 0x01, 0]
            ),
            "'2024-02-29 12:34:56.123456'"
        );
        assert_eq!(literal(date_tag, &[0]), "'0000-00-00'");
        assert_eq!(
            literal(time_tag, &[8, 1, 2, 0, 0, 0, 3, 4, 5]),
            "'-51:04:05'"
        );
        assert_eq!(
            literal(time_tag, &[12, 0, 0, 0, 0, 0, 2, 3, 4, 0x40, 0xE2, 0x01, 0]),
            "'02:03:04.123456'"
        );
    }

    #[test]
    fn verifies_mysql_native_password_challenges() {
        let password = b"pk_wire_secret";
        let salt = b"12345678901234567890";
        let stage_one = Sha1::digest(password);
        let stage_two = Sha1::digest(stage_one);
        let challenge = Sha1::digest([salt.as_slice(), stage_two.as_slice()].concat());
        let response = stage_one
            .iter()
            .zip(challenge)
            .map(|(left, right)| left ^ right)
            .collect::<Vec<_>>();
        assert!(verify_native_password(&stage_two, salt, &response));
        assert!(!verify_native_password(&stage_two, salt, &[0; 20]));
    }

    #[test]
    fn verifies_caching_sha2_fast_auth_responses() {
        let password = b"pk_wire_secret";
        let nonce = b"12345678901234567890";
        // Client-side scramble per mysql_common:
        // XOR(SHA256(password), SHA256(SHA256(SHA256(password)) || nonce)).
        let stage_one = Sha256::digest(password);
        let verifier = Sha256::digest(stage_one);
        let challenge = Sha256::digest([verifier.as_slice(), nonce.as_slice()].concat());
        let response = stage_one
            .iter()
            .zip(challenge)
            .map(|(left, right)| left ^ right)
            .collect::<Vec<_>>();
        assert!(verify_caching_sha2(&verifier, nonce, &response));
        assert!(!verify_caching_sha2(&verifier, nonce, &[0; 32]));
        assert!(!verify_caching_sha2(
            &verifier,
            b"09876543210987654321",
            &response
        ));
    }

    #[test]
    fn substitutes_only_unquoted_placeholders() {
        let sql = "SELECT '?', `?`, value FROM events WHERE id = ? AND name = ?";
        assert_eq!(placeholder_offsets(sql).len(), 2);
        assert_eq!(
            substitute_parameters(sql, &["7".to_owned(), "'launch'".to_owned()]).unwrap(),
            "SELECT '?', `?`, value FROM events WHERE id = 7 AND name = 'launch'"
        );
    }

    #[test]
    fn prepared_preview_uses_integers_only_for_limit_and_offset() {
        assert_eq!(
            placeholder_preview_literals("SELECT * FROM events WHERE name = ? LIMIT ? OFFSET ?"),
            vec!["NULL", "0", "0"]
        );
        assert_eq!(
            placeholder_preview_literals(
                "SELECT (SELECT id FROM events LIMIT ?) AS picked, ? AS label LIMIT ?, ?"
            ),
            vec!["0", "NULL", "0", "0"]
        );
        assert_eq!(
            placeholder_preview_literals("SELECT '?' AS literal LIMIT ?"),
            vec!["0"]
        );
    }

    #[test]
    fn sql_mode_refuses_modes_that_would_change_results() {
        // PIPES_AS_CONCAT is the sharpest case: the parser is a fixed
        // MySqlDialect, so `a || b` stays OR. Accepting the mode would
        // answer a different question than the client asked, silently.
        let refused = super::reject_unsupported_sql_modes("PIPES_AS_CONCAT")
            .expect_err("must refuse a mode it cannot honour");
        assert!(refused.contains("PIPES_AS_CONCAT"), "got: {refused}");

        for mode in ["ANSI_QUOTES", "NO_BACKSLASH_ESCAPES", "ALLOW_INVALID_DATES"] {
            assert!(
                super::reject_unsupported_sql_modes(mode).is_err(),
                "{mode} changes results and must be refused"
            );
        }
        // Compound modes turn the above on by another name.
        assert!(super::reject_unsupported_sql_modes("ANSI").is_err());
        // Refusal must survive being buried in a list, which is how clients
        // actually send sql_mode.
        assert!(
            super::reject_unsupported_sql_modes("STRICT_TRANS_TABLES,PIPES_AS_CONCAT,NO_ZERO_DATE")
                .is_err()
        );
    }

    #[test]
    fn sql_mode_accepts_modes_that_are_genuinely_inert_here() {
        // Write and DDL modes have nothing to act on in a read-only
        // replica, so accepting them is honest rather than a silent no-op.
        for mode in [
            "",
            "STRICT_TRANS_TABLES",
            "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION",
        ] {
            assert!(
                super::reject_unsupported_sql_modes(mode).is_ok(),
                "{mode} is inert on a read-only replica and must be accepted"
            );
        }
        // The default session value must not refuse itself.
        assert!(
            super::reject_unsupported_sql_modes(&super::Session::default().sql_mode).is_ok(),
            "the default sql_mode must be settable"
        );
    }

    #[test]
    fn placeholder_scanning_ignores_comments() {
        // A comment must not contribute placeholders, and a keyword inside
        // one must not flip clause context. Both scanners share one view of
        // what is executable, so they cannot disagree on the count.
        assert_eq!(
            placeholder_offsets("SELECT ? FROM t -- ? trailing\nWHERE x = ?").len(),
            2
        );
        assert_eq!(placeholder_offsets("SELECT ? FROM t # ? trailing").len(), 1);
        assert_eq!(placeholder_offsets("SELECT /* ? */ ? FROM t").len(), 1);
        // MySQL executes a version comment, so a parameter inside one counts.
        assert_eq!(placeholder_offsets("SELECT /*!40001 ? */ FROM t").len(), 1);

        // "limit" inside a comment previously flipped the clause context and
        // produced an integer preview for an ordinary expression parameter.
        assert_eq!(
            placeholder_preview_literals("SELECT /* limit */ ? FROM t"),
            vec!["NULL"]
        );
        assert_eq!(
            placeholder_preview_literals("SELECT ? FROM t -- limit\nLIMIT ?"),
            vec!["NULL", "0"]
        );
    }

    #[test]
    fn version_system_variable_matches_mysql_client_probes() {
        let output = compatibility_query("SELECT @@version", "analytics", &Session::default())
            .expect("compatibility response");
        assert_eq!(output.fields[0].name, "@@version");
        assert!(matches!(
            &output.rows[0][0],
            pintail_types::Value::Utf8(value) if value.starts_with("8.4.0-pintail-")
        ));
        let casing = compatibility_query(
            "SELECT @@lower_case_table_names",
            "analytics",
            &Session::default(),
        )
        .expect("compatibility response");
        assert_eq!(casing.rows, vec![vec![pintail_types::Value::UInt64(2)]]);
    }
}
