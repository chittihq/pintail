//! Column metadata and status codes.
//!
//! The three fields Pintail exists to control live on [`Column`]:
//! `column_length`, `character_set` and `decimals`. Clients read them to map
//! a result column onto a native type, so a fixed value there makes a
//! `DECIMAL(12,2)` announce scale zero and every string claim `utf8` — values
//! correct on the wire, metadata that lies.

/// Protocol type tag in `ColumnDefinition41`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ColumnType {
    /// `DECIMAL` in its modern, exact representation.
    MysqlTypeNewdecimal = 0xf6,
    /// 8-bit integer.
    MysqlTypeTiny = 0x01,
    /// 16-bit integer.
    MysqlTypeShort = 0x02,
    /// 32-bit integer.
    MysqlTypeLong = 0x03,
    /// 32-bit float.
    MysqlTypeFloat = 0x04,
    /// 64-bit float.
    MysqlTypeDouble = 0x05,
    /// SQL NULL.
    MysqlTypeNull = 0x06,
    /// `TIMESTAMP`.
    MysqlTypeTimestamp = 0x07,
    /// 64-bit integer.
    MysqlTypeLonglong = 0x08,
    /// `DATE`.
    MysqlTypeDate = 0x0a,
    /// `TIME`.
    MysqlTypeTime = 0x0b,
    /// `DATETIME`.
    MysqlTypeDatetime = 0x0c,
    /// `YEAR`.
    MysqlTypeYear = 0x0d,
    /// `VARCHAR`.
    MysqlTypeVarchar = 0x0f,
    /// `BIT`.
    MysqlTypeBit = 0x10,
    /// `JSON`, which clients decode as a document rather than text.
    MysqlTypeJson = 0xf5,
    /// `VARCHAR` in its length-prefixed form.
    MysqlTypeVarString = 0xfd,
    /// `CHAR`.
    MysqlTypeString = 0xfe,
    /// `BLOB` and `TEXT`.
    MysqlTypeBlob = 0xfc,
}

/// Column attribute bits clients read alongside the type tag.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ColumnFlags(u16);

impl ColumnFlags {
    /// The column never yields NULL.
    pub const NOT_NULL_FLAG: Self = Self(0x0001);
    /// The column is part of the primary key.
    pub const PRI_KEY_FLAG: Self = Self(0x0002);
    /// The column carries a UNIQUE constraint.
    pub const UNIQUE_KEY_FLAG: Self = Self(0x0004);
    /// The integer column is unsigned; without this a client decodes the
    /// high bit as a sign and reports negative row counts.
    pub const UNSIGNED_FLAG: Self = Self(0x0020);
    /// The column holds bytes rather than collated text.
    pub const BINARY_FLAG: Self = Self(0x0080);
    /// The column auto-increments.
    pub const AUTO_INCREMENT_FLAG: Self = Self(0x0200);

    /// An empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw protocol bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Sets or clears every bit in `flag` depending on `value`.
    pub const fn set(&mut self, flag: Self, value: bool) {
        if value {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

impl std::ops::BitOr for ColumnFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for ColumnFlags {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// Server status bits returned in OK and EOF packets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatusFlags(u16);

impl StatusFlags {
    /// A transaction is open.
    pub const SERVER_STATUS_IN_TRANS: Self = Self(0x0001);
    /// Autocommit is enabled.
    pub const SERVER_STATUS_AUTOCOMMIT: Self = Self(0x0002);
    /// Another result set follows this one.
    pub const SERVER_MORE_RESULTS_EXISTS: Self = Self(0x0008);

    /// An empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw protocol bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

impl std::ops::BitOr for StatusFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One result column as clients see it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    /// Schema the column belongs to.
    pub schema: String,
    /// Table alias as the query named it.
    pub table: String,
    /// Underlying table name.
    pub org_table: String,
    /// Column alias as the query named it.
    pub column: String,
    /// Underlying column name.
    pub org_column: String,
    /// Maximum width in bytes. Clients size buffers and choose native types
    /// from this, so a constant here breaks type mapping in every ORM.
    pub column_length: u32,
    /// Collation id. `63` is binary; the utf8mb4 collations differ from
    /// utf8mb3, and clients decode text according to this value.
    pub character_set: u16,
    /// Fractional digits: a DECIMAL's scale, or a temporal column's
    /// fractional-second precision.
    pub decimals: u8,
    /// Protocol type tag.
    pub coltype: ColumnType,
    /// Attribute bits.
    pub colflags: ColumnFlags,
}

impl Column {
    /// A column carrying the protocol defaults, for callers that fill in the
    /// distinguishing fields afterwards.
    #[must_use]
    pub fn new(column: impl Into<String>, coltype: ColumnType) -> Self {
        Self {
            schema: String::new(),
            table: String::new(),
            org_table: String::new(),
            column: column.into(),
            org_column: String::new(),
            column_length: 0,
            character_set: 63,
            decimals: 0,
            coltype,
            colflags: ColumnFlags::empty(),
        }
    }
}

/// Error codes Pintail returns. `MySQL` publishes the full list; these are
/// the ones a read-only replica actually produces, named so a reader can see
/// which condition each one reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ErrorKind {
    /// 1045: authentication failed.
    ErAccessDeniedError = 1045,
    /// 1049: the requested database does not exist.
    ErBadDbError = 1049,
    /// 1064: the statement did not parse.
    ErParseError = 1064,
    /// 1146: the table does not exist.
    ErNoSuchTable = 1146,
    /// 1149: the statement is syntactically valid but unsupported here.
    ErSyntaxError = 1149,
    /// 1152: the client went away mid-statement.
    ErAborting = 1152,
    /// 1317: the statement was interrupted, including by a deadline.
    ErQueryInterrupted = 1317,
    /// 1064 alias used for a malformed command packet.
    ErUnknownComError = 1047,
    /// 1290: the server is running read-only.
    ErOptionPreventsStatement = 1290,
    /// 1044: the authenticated key is scoped to a different database.
    ErDbaccessDeniedError = 1044,
    /// 1210: a prepared statement was executed with the wrong parameter
    /// count or an unusable value.
    ErWrongArguments = 1210,
    /// 1243: the statement handle in `COM_STMT_EXECUTE` is unknown, most
    /// often a client executing after `COM_STMT_CLOSE` or against the wrong
    /// connection.
    ErUnknownStmtHandler = 1243,
    /// 1040: the server refused to start the query because it is at its
    /// concurrency limit.
    ///
    /// `MySQL` publishes no code meaning exactly "query admission refused";
    /// 1040 names connections, not queries. It is used anyway because it is
    /// the code clients and pools already treat as retryable backpressure,
    /// which is the behaviour a refused query needs. The message says
    /// concurrent queries so an operator reading the log is not sent
    /// hunting a connection limit that is not the cause.
    ErConCountError = 1040,
    /// 1105: anything without a more specific code.
    ErUnknownError = 1105,
}

impl ErrorKind {
    /// The numeric code clients compare against.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// The five-character SQLSTATE clients read when they do not recognise
    /// the numeric code.
    #[must_use]
    pub const fn sql_state(self) -> &'static [u8; 5] {
        match self {
            Self::ErAccessDeniedError | Self::ErDbaccessDeniedError => b"28000",
            Self::ErBadDbError
            | Self::ErParseError
            | Self::ErSyntaxError
            | Self::ErOptionPreventsStatement => b"42000",
            Self::ErNoSuchTable => b"42S02",
            Self::ErAborting | Self::ErUnknownComError => b"08S01",
            Self::ErConCountError => b"08004",
            Self::ErQueryInterrupted => b"70100",
            Self::ErWrongArguments | Self::ErUnknownStmtHandler | Self::ErUnknownError => b"HY000",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Column, ColumnFlags, ColumnType, ErrorKind};

    #[test]
    fn column_flags_compose_and_test() {
        let flags = ColumnFlags::NOT_NULL_FLAG | ColumnFlags::UNSIGNED_FLAG;
        assert!(flags.contains(ColumnFlags::NOT_NULL_FLAG));
        assert!(flags.contains(ColumnFlags::UNSIGNED_FLAG));
        assert!(!flags.contains(ColumnFlags::PRI_KEY_FLAG));
        assert_eq!(flags.bits(), 0x0021);
    }

    #[test]
    fn column_flags_set_toggles_a_single_bit_without_disturbing_others() {
        let mut flags = ColumnFlags::NOT_NULL_FLAG;
        flags.set(ColumnFlags::UNSIGNED_FLAG, true);
        assert!(flags.contains(ColumnFlags::NOT_NULL_FLAG));
        assert!(flags.contains(ColumnFlags::UNSIGNED_FLAG));
        flags.set(ColumnFlags::UNSIGNED_FLAG, false);
        assert!(flags.contains(ColumnFlags::NOT_NULL_FLAG));
        assert!(!flags.contains(ColumnFlags::UNSIGNED_FLAG));
    }

    #[test]
    fn a_new_column_defaults_to_binary_and_zero_scale() {
        // Callers must set the distinguishing fields; defaulting them to a
        // constant is the defect this crate exists to avoid, so the default
        // is deliberately the neutral binary one rather than a guess.
        let column = Column::new("total", ColumnType::MysqlTypeNewdecimal);
        assert_eq!(column.character_set, 63);
        assert_eq!(column.decimals, 0);
        assert_eq!(column.column_length, 0);
    }

    #[test]
    fn error_kinds_carry_mysql_codes_and_sql_states() {
        assert_eq!(ErrorKind::ErQueryInterrupted.code(), 1317);
        assert_eq!(ErrorKind::ErQueryInterrupted.sql_state(), b"70100");
        assert_eq!(ErrorKind::ErAccessDeniedError.sql_state(), b"28000");
        assert_eq!(ErrorKind::ErNoSuchTable.sql_state(), b"42S02");
        assert_eq!(ErrorKind::ErDbaccessDeniedError.code(), 1044);
        assert_eq!(ErrorKind::ErWrongArguments.code(), 1210);
        assert_eq!(ErrorKind::ErUnknownStmtHandler.code(), 1243);
    }
}
