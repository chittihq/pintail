use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter,
    IntermediaryOptions, OkResponse, ParamParser, QueryResultWriter, StatementMetaWriter,
    ToMysqlValue, ValueInner, plain_run_with_options, secure_run_with_options,
};
use pintail_meta::{ApiKeyRecord, MetaStore};
use pintail_types::{DataType, Value};
use rand::RngCore as _;
use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::AsyncWrite,
    net::{TcpListener, TcpStream},
};

use crate::{
    DEFAULT_MAX_ROWS, DEFAULT_QUERY_MEMORY_LIMIT, QueryError, QueryField, QueryOutput, QueryStats,
    ReplicaEngine,
};

static NEXT_CONNECTION_ID: AtomicU32 = AtomicU32::new(1);

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
            let _ = serve_connection(stream, backend, None).await;
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
                    let _ = serve_connection(stream, backend, tls).await;
                });
            }
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    mut backend: Backend,
    tls: Option<WireTls>,
) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let options = IntermediaryOptions {
        process_use_statement_on_query: true,
        reject_connection_on_dbname_absence: false,
    };
    let tls_config = tls.as_ref().map(|tls| std::sync::Arc::clone(&tls.config));
    let (client_requested_tls, init) =
        AsyncMysqlIntermediary::init_before_ssl(&mut backend, reader, &mut writer, &tls_config)
            .await?;
    match (client_requested_tls, tls) {
        (true, Some(tls)) => {
            secure_run_with_options(backend, writer, options, tls.config, init).await
        }
        // A required-TLS listener drops plaintext clients after the
        // greeting; MySQL clients report the closed connection as
        // "server requires secure transport".
        (false, Some(tls)) if tls.required => Ok(()),
        _ => plain_run_with_options(backend, writer, options, init).await,
    }
}

/// Per-connection session variables with real semantics: `time_zone`
/// shifts the statement-pinned time functions, `NAMES` accepts only the
/// utf8 charsets Pintail actually serves, and `sql_mode` is stored and
/// echoed with documented no-op semantics.
#[derive(Clone, Debug)]
struct Session {
    time_zone: String,
    sql_mode: String,
    charset: String,
    group_concat_max_len: usize,
    group_concat_warnings: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            time_zone: "SYSTEM".to_owned(),
            sql_mode: "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
                .to_owned(),
            charset: "utf8mb4".to_owned(),
            group_concat_max_len: 1024,
            group_concat_warnings: 0,
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
        let Ok(username) = std::str::from_utf8(username) else {
            return Ok(false);
        };
        let metadata = MetaStore::open(&self.metadata_path).map_err(io_other)?;
        let Some(database) = metadata
            .databases()
            .map_err(io_other)?
            .into_iter()
            .find(|database| database.name.eq_ignore_ascii_case(username))
        else {
            return Ok(false);
        };
        let key = metadata
            .api_keys(&database.id)
            .map_err(io_other)?
            .into_iter()
            .find(|key| wire_key_is_valid(key, salt, response));
        let Some(key) = key else {
            return Ok(false);
        };
        metadata
            .touch_api_key(&key.id, &Utc::now().to_rfc3339())
            .map_err(io_other)?;
        *self.authentication.lock().map_err(io_other)? = Some(Authenticated {
            database_id: database.id,
            database_name: database.name,
        });
        Ok(true)
    }

    fn execute(&self, sql: &str) -> Result<QueryOutput, QueryError> {
        let authenticated = self
            .authenticated()
            .map_err(|error| QueryError::Internal(error.to_string()))?;
        let session = self
            .session
            .lock()
            .map_err(|error| QueryError::Internal(error.to_string()))?
            .clone();
        compatibility_query(sql, &authenticated.database_name, &session).map_or_else(
            || {
                // The session zone shifts statement-pinned time functions;
                // optimization runs on this thread, so install-and-restore
                // brackets exactly one statement.
                let _ = pintail_exec::set_session_time_zone(Some(&session.time_zone));
                pintail_exec::set_session_group_concat_max_len(Some(session.group_concat_max_len));
                let result = self
                    .engine
                    .execute(&authenticated.database_id, sql, DEFAULT_MAX_ROWS);
                let warnings = pintail_exec::take_session_group_concat_warnings();
                pintail_exec::set_session_group_concat_max_len(None);
                let _ = pintail_exec::set_session_time_zone(None);
                if let Ok(mut current) = self.session.lock() {
                    current.group_concat_warnings = warnings;
                }
                result
            },
            Ok,
        )
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
                charset.clone_into(&mut session.charset);
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
                session.sql_mode = value;
                Ok(())
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
            // Everything else keeps the accepted-no-op compatibility
            // behavior (autocommit, isolation levels, probes).
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl<W> AsyncMysqlShim<W> for Backend
where
    W: AsyncWrite + Send + Unpin,
{
    type Error = io::Error;

    fn version(&self) -> String {
        format!("8.4.0-pintail-{}", env!("CARGO_PKG_VERSION"))
    }

    fn connect_id(&self) -> u32 {
        self.connection_id
    }

    fn salt(&self) -> [u8; 20] {
        self.salt
    }

    // The opensrv trait fixes the &str return; the literal is 'static.
    #[allow(clippy::unnecessary_literal_bound)]
    fn default_auth_plugin(&self) -> &str {
        "caching_sha2_password"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    async fn auth_plugin_for_username(&self, _user: &[u8]) -> &str {
        // Auth-switch target for legacy clients that answered the
        // caching_sha2_password greeting with an empty response.
        "mysql_native_password"
    }

    async fn authenticate(
        &self,
        _auth_plugin: &str,
        username: &[u8],
        salt: &[u8],
        auth_data: &[u8],
    ) -> bool {
        // The response length identifies the plugin the client actually used:
        // 32 bytes = caching_sha2_password fast auth, 20 = mysql_native_password.
        self.authenticate_wire_key(username, salt, auth_data)
            .unwrap_or(false)
    }

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> io::Result<()> {
        let parameters = placeholder_count(query);
        let preview = substitute_parameters(query, &vec!["NULL".to_owned(); parameters])
            .map_err(io_invalid)?;
        let output = match self.execute(&preview) {
            Ok(output) => output,
            Err(error) => {
                return info
                    .error(error_kind(&error), error.to_string().as_bytes())
                    .await;
            }
        };
        let statement_id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1).max(1);
        self.prepared.insert(
            statement_id,
            Prepared {
                sql: query.to_owned(),
                parameters,
            },
        );
        let params = (0..parameters)
            .map(|index| Column {
                table: String::new(),
                column: format!("param_{}", index + 1),
                coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
                colflags: ColumnFlags::empty(),
            })
            .collect::<Vec<_>>();
        let columns = output.fields.iter().map(mysql_column).collect::<Vec<_>>();
        info.reply(statement_id, &params, &columns).await
    }

    async fn on_execute<'a>(
        &'a mut self,
        id: u32,
        params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> io::Result<()> {
        let Some(statement) = self.prepared.get(&id).cloned() else {
            return results
                .error(
                    ErrorKind::ER_UNKNOWN_STMT_HANDLER,
                    b"unknown prepared statement",
                )
                .await;
        };
        let parameters = params
            .into_iter()
            .map(|parameter| parameter_literal(parameter.value.into_inner()))
            .collect::<Result<Vec<_>, _>>();
        let parameters = match parameters {
            Ok(parameters) if parameters.len() == statement.parameters => parameters,
            Ok(_) => {
                return results
                    .error(
                        ErrorKind::ER_WRONG_ARGUMENTS,
                        b"prepared statement parameter count does not match",
                    )
                    .await;
            }
            Err(error) => {
                return results
                    .error(ErrorKind::ER_WRONG_ARGUMENTS, error.as_bytes())
                    .await;
            }
        };
        let query = substitute_parameters(&statement.sql, &parameters).map_err(io_invalid)?;
        write_query_result(self.execute(&query), results).await
    }

    async fn on_close(&mut self, statement: u32) {
        self.prepared.remove(&statement);
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> io::Result<()> {
        if is_session_command(query) {
            return match self.apply_session_command(query) {
                Ok(()) => results.completed(OkResponse::default()).await,
                Err(error) => {
                    results
                        .error(ErrorKind::ER_WRONG_ARGUMENTS, error.as_bytes())
                        .await
                }
            };
        }
        write_query_result(self.execute(query), results).await
    }

    async fn on_init<'a>(
        &'a mut self,
        database: &'a str,
        writer: InitWriter<'a, W>,
    ) -> io::Result<()> {
        let authenticated = self.authenticated()?;
        if authenticated.database_name.eq_ignore_ascii_case(database) {
            writer.ok().await
        } else {
            writer
                .error(
                    ErrorKind::ER_DBACCESS_DENIED_ERROR,
                    b"API key is scoped to another database",
                )
                .await
        }
    }
}

async fn write_query_result<W>(
    result: Result<QueryOutput, QueryError>,
    writer: QueryResultWriter<'_, W>,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return writer
                .error(error_kind(&error), error.to_string().as_bytes())
                .await;
        }
    };
    let columns = output.fields.iter().map(mysql_column).collect::<Vec<_>>();
    let mut rows = writer.start(&columns).await?;
    for row in &output.rows {
        for (field, value) in output.fields.iter().zip(row) {
            write_value(&mut rows, field, value)?;
        }
        rows.end_row().await?;
    }
    rows.finish().await
}

fn write_value<W>(
    rows: &mut opensrv_mysql::RowWriter<'_, W>,
    field: &QueryField,
    value: &Value,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    match (field.data_type, value) {
        (_, Value::Null) => rows.write_col(None::<u8>),
        (_, Value::Boolean(value)) => rows.write_col(i8::from(*value)),
        (Some(DataType::Int8), Value::Int64(value)) => {
            rows.write_col(i8::try_from(*value).map_err(io_invalid)?)
        }
        (Some(DataType::Int16), Value::Int64(value)) => {
            rows.write_col(i16::try_from(*value).map_err(io_invalid)?)
        }
        (Some(DataType::Int32), Value::Int64(value)) => {
            rows.write_col(i32::try_from(*value).map_err(io_invalid)?)
        }
        (_, Value::Int64(value)) => rows.write_col(*value),
        (Some(DataType::UInt8), Value::UInt64(value)) => {
            rows.write_col(u8::try_from(*value).map_err(io_invalid)?)
        }
        (Some(DataType::UInt16), Value::UInt64(value)) => {
            rows.write_col(u16::try_from(*value).map_err(io_invalid)?)
        }
        (Some(DataType::UInt32), Value::UInt64(value)) => {
            rows.write_col(u32::try_from(*value).map_err(io_invalid)?)
        }
        (_, Value::UInt64(value)) => rows.write_col(*value),
        (Some(DataType::Float32), Value::Float64(value)) => {
            rows.write_col(value.get().to_string().parse::<f32>().map_err(io_invalid)?)
        }
        (_, Value::Float64(value)) => rows.write_col(value.get()),
        (Some(DataType::Date32), Value::Utf8(value)) => {
            let value = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(io_invalid)?;
            rows.write_col(value)
        }
        (Some(DataType::DateTime64 { .. }), Value::Utf8(value)) => {
            let value = parse_datetime(value).ok_or_else(|| {
                io_invalid(format!("invalid canonical MySQL DATETIME value: {value}"))
            })?;
            rows.write_col(value)
        }
        (Some(DataType::Time64 { .. }), Value::Utf8(value)) => {
            rows.write_col(MysqlTimeValue::parse(value).map_err(io_invalid)?)
        }
        (_, Value::Utf8(value)) => rows.write_col(value.as_str()),
        (_, Value::Binary(value)) => rows.write_col(value.as_slice()),
    }
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

struct MysqlTimeValue<'a> {
    raw: &'a str,
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
}

impl<'a> MysqlTimeValue<'a> {
    fn parse(raw: &'a str) -> Result<Self, String> {
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
            raw,
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

impl ToMysqlValue for MysqlTimeValue<'_> {
    fn to_mysql_text<W: std::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.raw.to_mysql_text(writer)
    }

    fn to_mysql_bin<W: std::io::Write>(&self, writer: &mut W, column: &Column) -> io::Result<()> {
        if column.coltype != ColumnType::MYSQL_TYPE_TIME {
            return Err(io_invalid("MySQL TIME value used with a non-TIME column"));
        }
        if self.days == 0
            && self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
            && self.micros == 0
        {
            return writer.write_all(&[0]);
        }
        writer.write_all(&[if self.micros == 0 { 8 } else { 12 }])?;
        writer.write_all(&[u8::from(self.negative)])?;
        writer.write_all(&self.days.to_le_bytes())?;
        writer.write_all(&[self.hours, self.minutes, self.seconds])?;
        if self.micros != 0 {
            writer.write_all(&self.micros.to_le_bytes())?;
        }
        Ok(())
    }
}

fn mysql_column(field: &QueryField) -> Column {
    let (coltype, unsigned) = match field.data_type {
        Some(DataType::Boolean | DataType::Int8 | DataType::UInt8) => (
            ColumnType::MYSQL_TYPE_TINY,
            matches!(field.data_type, Some(DataType::UInt8)),
        ),
        Some(DataType::Int16 | DataType::UInt16) => (
            ColumnType::MYSQL_TYPE_SHORT,
            matches!(field.data_type, Some(DataType::UInt16)),
        ),
        Some(DataType::Int32 | DataType::UInt32) => (
            ColumnType::MYSQL_TYPE_LONG,
            matches!(field.data_type, Some(DataType::UInt32)),
        ),
        Some(DataType::Int64 | DataType::UInt64) => (
            ColumnType::MYSQL_TYPE_LONGLONG,
            matches!(field.data_type, Some(DataType::UInt64)),
        ),
        Some(DataType::Float32) => (ColumnType::MYSQL_TYPE_FLOAT, false),
        Some(DataType::Float64) => (ColumnType::MYSQL_TYPE_DOUBLE, false),
        Some(DataType::Decimal { .. }) => (ColumnType::MYSQL_TYPE_NEWDECIMAL, false),
        Some(DataType::Date32) => (ColumnType::MYSQL_TYPE_DATE, false),
        Some(DataType::DateTime64 { .. }) => (ColumnType::MYSQL_TYPE_DATETIME, false),
        Some(DataType::Time64 { .. }) => (ColumnType::MYSQL_TYPE_TIME, false),
        Some(DataType::Year) => (ColumnType::MYSQL_TYPE_YEAR, true),
        Some(DataType::Binary) => (ColumnType::MYSQL_TYPE_BLOB, false),
        Some(DataType::Json) => (ColumnType::MYSQL_TYPE_JSON, false),
        Some(DataType::Utf8) | None => (ColumnType::MYSQL_TYPE_VAR_STRING, false),
    };
    let mut colflags = ColumnFlags::empty();
    colflags.set(ColumnFlags::UNSIGNED_FLAG, unsigned);
    colflags.set(ColumnFlags::NOT_NULL_FLAG, !field.nullable);
    // opensrv-mysql hardcodes charset 33 in column definitions, so clients
    // keying binary detection on charset 63 see text; the flag is the only
    // binary signal this server can emit (docs/limitations.md).
    colflags.set(
        ColumnFlags::BINARY_FLAG,
        matches!(field.data_type, Some(DataType::Binary)),
    );
    Column {
        table: String::new(),
        column: field.name.clone(),
        coltype,
        colflags,
    }
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
        QueryError::DatabaseNotFound => ErrorKind::ER_BAD_DB_ERROR,
        QueryError::Invalid(message) if message.contains("read-only") => {
            ErrorKind::ER_NOT_SUPPORTED_YET
        }
        QueryError::Invalid(_) => ErrorKind::ER_PARSE_ERROR,
        QueryError::NotReady(_) | QueryError::Internal(_) => ErrorKind::ER_UNKNOWN_ERROR,
    }
}

fn compatibility_query(sql: &str, database: &str, session: &Session) -> Option<QueryOutput> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    if normalized.starts_with("show warnings") {
        return Some(QueryOutput {
            fields: vec![
                QueryField {
                    name: "Level".to_owned(),
                    data_type: Some(DataType::Utf8),
                    nullable: false,
                },
                QueryField {
                    name: "Code".to_owned(),
                    data_type: Some(DataType::UInt64),
                    nullable: false,
                },
                QueryField {
                    name: "Message".to_owned(),
                    data_type: Some(DataType::Utf8),
                    nullable: false,
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
        });
    }
    let (name, value) = if normalized.starts_with("select version()") {
        (
            "VERSION()",
            Value::Utf8(format!("8.4.0-pintail-{}", env!("CARGO_PKG_VERSION"))),
        )
    } else if normalized.starts_with("select database()") {
        ("DATABASE()", Value::Utf8(database.to_owned()))
    } else if normalized.contains("@@version_comment") {
        (
            "@@version_comment",
            Value::Utf8("Pintail analytical mirror".to_owned()),
        )
    } else if normalized.contains("@@max_allowed_packet") {
        ("@@max_allowed_packet", Value::UInt64(64 * 1024 * 1024))
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
    } else if normalized.contains("@@session.time_zone") || normalized.contains("@@time_zone") {
        (
            "@@session.time_zone",
            Value::Utf8(session.time_zone.clone()),
        )
    } else if normalized.contains("@@sql_mode") {
        ("@@sql_mode", Value::Utf8(session.sql_mode.clone()))
    } else if normalized.contains("@@character_set_client")
        || normalized.contains("@@character_set_connection")
        || normalized.contains("@@character_set_results")
    {
        (
            "@@character_set_client",
            Value::Utf8(session.charset.clone()),
        )
    } else if normalized.contains("@@collation_connection") {
        (
            "@@collation_connection",
            Value::Utf8("utf8mb4_0900_ai_ci".to_owned()),
        )
    } else {
        return None;
    };
    Some(QueryOutput {
        fields: vec![QueryField {
            name: name.to_owned(),
            data_type: value.data_type(),
            nullable: false,
        }],
        rows: vec![vec![value]],
        stats: QueryStats {
            rows: 1,
            ..QueryStats::default()
        },
        truncated: false,
    })
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

fn placeholder_offsets(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut offsets = Vec::new();
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == b'\\' {
                index = index.saturating_add(2);
                continue;
            }
            if byte == delimiter {
                if bytes.get(index + 1) == Some(&delimiter) {
                    index += 2;
                    continue;
                }
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
        } else if byte == b'?' {
            offsets.push(index);
        }
        index += 1;
    }
    offsets
}

fn parameter_literal(value: ValueInner<'_>) -> Result<String, String> {
    match value {
        ValueInner::NULL => Ok("NULL".to_owned()),
        ValueInner::Bytes(value) => std::str::from_utf8(value).map_or_else(
            |_| Ok(format!("X'{}'", encode_hex(value))),
            |value| {
                Ok(format!(
                    "'{}'",
                    value.replace('\\', "\\\\").replace('\'', "''")
                ))
            },
        ),
        ValueInner::Int(value) => Ok(value.to_string()),
        ValueInner::UInt(value) => Ok(value.to_string()),
        ValueInner::Double(value) if value.is_finite() => Ok(value.to_string()),
        ValueInner::Double(_) => Err("non-finite prepared parameters are unsupported".to_owned()),
        ValueInner::Date(payload) | ValueInner::Datetime(payload) => temporal_date_literal(payload),
        ValueInner::Time(payload) => temporal_time_literal(payload),
    }
}

/// Renders `MySQL`'s binary `DATE`/`DATETIME` parameter encoding as a quoted
/// literal: 0, 4, 7, or 11 payload bytes for zero, date, seconds, and
/// microsecond precision respectively.
fn temporal_date_literal(payload: &[u8]) -> Result<String, String> {
    let field = |index: usize| payload.get(index).copied().unwrap_or(0);
    match payload.len() {
        0 => Ok("'0000-00-00'".to_owned()),
        4 | 7 | 11 => {
            let year = u16::from(field(0)) | (u16::from(field(1)) << 8);
            let mut literal = format!("'{year:04}-{:02}-{:02}", field(2), field(3));
            if payload.len() >= 7 {
                let _ = std::fmt::Write::write_fmt(
                    &mut literal,
                    format_args!(" {:02}:{:02}:{:02}", field(4), field(5), field(6)),
                );
            }
            if payload.len() == 11 {
                let micros = u32::from(field(7))
                    | (u32::from(field(8)) << 8)
                    | (u32::from(field(9)) << 16)
                    | (u32::from(field(10)) << 24);
                let _ = std::fmt::Write::write_fmt(&mut literal, format_args!(".{micros:06}"));
            }
            literal.push('\'');
            Ok(literal)
        }
        length => Err(format!("temporal parameter has invalid length {length}")),
    }
}

/// Renders `MySQL`'s binary `TIME` parameter encoding as a quoted literal:
/// 0, 8, or 12 payload bytes; hours fold in the day count.
fn temporal_time_literal(payload: &[u8]) -> Result<String, String> {
    let field = |index: usize| payload.get(index).copied().unwrap_or(0);
    match payload.len() {
        0 => Ok("'00:00:00'".to_owned()),
        8 | 12 => {
            let sign = if field(0) == 0 { "" } else { "-" };
            let days = u32::from(field(1))
                | (u32::from(field(2)) << 8)
                | (u32::from(field(3)) << 16)
                | (u32::from(field(4)) << 24);
            let hours = u64::from(days) * 24 + u64::from(field(5));
            let mut literal = format!("'{sign}{hours:02}:{:02}:{:02}", field(6), field(7));
            if payload.len() == 12 {
                let micros = u32::from(field(8))
                    | (u32::from(field(9)) << 8)
                    | (u32::from(field(10)) << 16)
                    | (u32::from(field(11)) << 24);
                let _ = std::fmt::Write::write_fmt(&mut literal, format_args!(".{micros:06}"));
            }
            literal.push('\'');
            Ok(literal)
        }
        length => Err(format!("time parameter has invalid length {length}")),
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
    use opensrv_mysql::{ColumnFlags, ColumnType};
    use pintail_types::DataType;
    use sha1::{Digest as _, Sha1};
    use sha2::Digest as _;

    use super::{
        Sha256, mysql_column, placeholder_offsets, substitute_parameters, verify_caching_sha2,
        verify_native_password,
    };
    use crate::QueryField;

    #[test]
    fn json_results_advertise_mysql_json_metadata() {
        let column = mysql_column(&QueryField {
            name: "document".to_owned(),
            data_type: Some(DataType::Json),
            nullable: true,
        });
        assert_eq!(column.coltype, ColumnType::MYSQL_TYPE_JSON);
    }

    #[test]
    fn result_columns_preserve_numeric_binary_and_nullability_flags() {
        let unsigned = mysql_column(&QueryField {
            name: "ordinal_position".to_owned(),
            data_type: Some(DataType::UInt64),
            nullable: false,
        });
        assert_eq!(unsigned.coltype, ColumnType::MYSQL_TYPE_LONGLONG);
        assert!(unsigned.colflags.contains(ColumnFlags::UNSIGNED_FLAG));
        assert!(unsigned.colflags.contains(ColumnFlags::NOT_NULL_FLAG));

        let nullable_text = mysql_column(&QueryField {
            name: "column_default".to_owned(),
            data_type: Some(DataType::Utf8),
            nullable: true,
        });
        assert_eq!(nullable_text.coltype, ColumnType::MYSQL_TYPE_VAR_STRING);
        assert!(!nullable_text.colflags.contains(ColumnFlags::NOT_NULL_FLAG));

        let binary = mysql_column(&QueryField {
            name: "payload".to_owned(),
            data_type: Some(DataType::Binary),
            nullable: true,
        });
        assert_eq!(binary.coltype, ColumnType::MYSQL_TYPE_BLOB);
        assert!(binary.colflags.contains(ColumnFlags::BINARY_FLAG));
    }

    #[test]
    fn renders_temporal_prepared_parameters_as_mysql_literals() {
        assert_eq!(
            super::temporal_date_literal(&[0xE8, 0x07, 2, 29]).unwrap(),
            "'2024-02-29'"
        );
        assert_eq!(
            super::temporal_date_literal(&[0xE8, 0x07, 2, 29, 12, 34, 56]).unwrap(),
            "'2024-02-29 12:34:56'"
        );
        assert_eq!(
            super::temporal_date_literal(&[0xE8, 0x07, 2, 29, 12, 34, 56, 0x40, 0xE2, 0x01, 0])
                .unwrap(),
            "'2024-02-29 12:34:56.123456'"
        );
        assert_eq!(super::temporal_date_literal(&[]).unwrap(), "'0000-00-00'");
        assert_eq!(
            super::temporal_time_literal(&[1, 2, 0, 0, 0, 3, 4, 5]).unwrap(),
            "'-51:04:05'"
        );
        assert_eq!(
            super::temporal_time_literal(&[0, 0, 0, 0, 0, 2, 3, 4, 0x40, 0xE2, 0x01, 0]).unwrap(),
            "'02:03:04.123456'"
        );
        assert!(super::temporal_date_literal(&[1, 2]).is_err());
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
}
