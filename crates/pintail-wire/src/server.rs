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

    fn authenticate_native(
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
        compatibility_query(sql, &authenticated.database_name).map_or_else(
            || {
                self.engine
                    .execute(&authenticated.database_id, sql, DEFAULT_MAX_ROWS)
            },
            Ok,
        )
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

    async fn authenticate(
        &self,
        auth_plugin: &str,
        username: &[u8],
        salt: &[u8],
        auth_data: &[u8],
    ) -> bool {
        auth_plugin == "mysql_native_password"
            && self
                .authenticate_native(username, salt, auth_data)
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
            return results.completed(OkResponse::default()).await;
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
    let Some(expected) = key.mysql_native_password_hash.as_deref() else {
        return false;
    };
    verify_native_password(expected, salt, response)
}

fn key_has_query_scope(key: &ApiKeyRecord) -> bool {
    serde_json::from_str::<Vec<String>>(&key.scopes_json).is_ok_and(|scopes| {
        scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "*" | "query"))
    })
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

fn compatibility_query(sql: &str, database: &str) -> Option<QueryOutput> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
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
        ValueInner::Date(_) | ValueInner::Time(_) | ValueInner::Datetime(_) => {
            Err("temporal prepared parameters are not yet supported".to_owned())
        }
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
    use sha1::{Digest as _, Sha1};

    use super::{placeholder_offsets, substitute_parameters, verify_native_password};

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
    fn substitutes_only_unquoted_placeholders() {
        let sql = "SELECT '?', `?`, value FROM events WHERE id = ? AND name = ?";
        assert_eq!(placeholder_offsets(sql).len(), 2);
        assert_eq!(
            substitute_parameters(sql, &["7".to_owned(), "'launch'".to_owned()]).unwrap(),
            "SELECT '?', `?`, value FROM events WHERE id = 7 AND name = 'launch'"
        );
    }
}
