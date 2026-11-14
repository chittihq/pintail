//! What `COLLATION`, `CHARSET` and `COERCIBILITY` report for an expression.
//!
//! `MySQL` gives every expression a collation and a rung on its coercibility
//! ladder: 0 for an explicit `COLLATE`, 2 for a column, 4 for a literal, 5
//! for a number or a date, 6 for `NULL`. The rules that combine them are
//! per function - `CONCAT` weighs every argument, `REPLACE` only its first,
//! `HEX` none - so this reads the expression as written and answers only
//! the shapes whose rule is known. Everything else is refused by name:
//! rung 1 (two collations with no winner), rung 3 (a value the server or
//! the connection supplies), aggregates, windows, subqueries and variables.

use pintail_types::{CharacterSet, DataType};
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType as SqlDataType, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, UnaryOperator, Value as SqlValue,
};

use super::{BindError, SubqueryResolver, bind_expr_inner, object_name_parts};
use crate::bound::{BoundExprKind, BoundTable};

const BINARY: &str = "binary";
const JSON_TEXT: &str = "utf8mb4_bin";
const SERVER_TEXT: &str = "utf8mb3_general_ci";

/// What kind of value carries the collation; the rules differ by kind where
/// a value is handed to a function of strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Class {
    Text,
    Bytes,
    Number,
    Temporal,
    /// A JSON column: `binary` by itself, `utf8mb4_bin` as a string.
    JsonColumn,
    Null,
}

/// A collation and its rung on the coercibility ladder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Identity {
    pub(super) collation: String,
    pub(super) coercibility: u8,
    class: Class,
}

impl Identity {
    fn text(collation: &str, coercibility: u8) -> Self {
        Self {
            collation: collation.to_owned(),
            coercibility,
            class: Class::Text,
        }
    }

    fn bytes(coercibility: u8) -> Self {
        Self {
            collation: BINARY.to_owned(),
            coercibility,
            class: Class::Bytes,
        }
    }

    fn of_class(class: Class) -> Self {
        Self {
            collation: BINARY.to_owned(),
            coercibility: match class {
                Class::Null => 6,
                Class::JsonColumn => 2,
                _ => 5,
            },
            class,
        }
    }

    /// The character set the collation belongs to.
    pub(super) fn charset(&self) -> &str {
        charset_of(&self.collation)
    }

    /// Whether the value is a character or byte string, the only kinds
    /// whose weights `WEIGHT_STRING` reads from the collation.
    pub(super) fn is_string(&self) -> bool {
        matches!(self.class, Class::Text | Class::Bytes)
    }

    /// Whether the value is the `NULL` literal.
    pub(super) fn is_null(&self) -> bool {
        self.class == Class::Null
    }
}

fn charset_of(collation: &str) -> &str {
    collation.split('_').next().unwrap_or(collation)
}

/// The collation literals and generated text take: the connection's, or
/// nothing when the connection's has no name to report.
fn connection_identity() -> Option<Identity> {
    if crate::session_binary_literals() {
        Some(Identity::bytes(4))
    } else if crate::session_collation_named() {
        let collation = crate::session_default_collation();
        // A utf8mb3 connection compares under the utf8mb4 collation of
        // the same name, and reports its own.
        Some(
            match collation
                .strip_prefix("utf8mb4_")
                .filter(|_| crate::session_character_set() == CharacterSet::Utf8Mb3)
            {
                Some(rest) => Identity::text(&format!("utf8mb3_{rest}"), 4),
                None => Identity::text(collation, 4),
            },
        )
    } else {
        None
    }
}

struct Scope<'a, 'b> {
    name: &'a str,
    tables: &'a [BoundTable],
    subqueries: Option<&'a SubqueryResolver<'b>>,
}

impl Scope<'_, '_> {
    fn refuse(&self, expression: &Expr, reason: &str) -> BindError {
        BindError::UnsupportedExpression(format!("{}({expression}): {reason}", self.name))
    }

    fn unmodelled(&self, expression: &Expr) -> BindError {
        self.refuse(
            expression,
            "the collation of this expression is not modelled",
        )
    }

    fn connection(&self, expression: &Expr) -> Result<Identity, BindError> {
        connection_identity().ok_or_else(|| {
            self.refuse(
                expression,
                "the collation of this connection is carried under another name",
            )
        })
    }

    fn bound_type(&self, expression: &Expr) -> Result<Option<DataType>, BindError> {
        Ok(bind_expr_inner(
            expression,
            self.tables,
            &mut None,
            &mut None,
            self.subqueries,
        )?
        .data_type)
    }

    /// A value handed to a function of strings: a number or a date is
    /// written in the connection's character set, a JSON column is JSON
    /// text.
    fn as_string(&self, expression: &Expr) -> Result<Identity, BindError> {
        let identity = self.identity(expression)?;
        Ok(match identity.class {
            Class::Number | Class::Temporal => self.connection(expression)?,
            Class::JsonColumn => Identity::text(JSON_TEXT, 2),
            _ => identity,
        })
    }

    /// `MySQL`'s aggregation of the collations several strings bring to one
    /// result, left to right.
    fn aggregate(&self, whole: &Expr, parts: Vec<Identity>) -> Result<Identity, BindError> {
        let mut parts = parts.into_iter();
        let Some(mut held) = parts.next() else {
            return Err(self.unmodelled(whole));
        };
        // Two collations of one rung with no winner between them: rung 1.
        let mut undecided = false;
        for next in parts {
            // An undecided pair stands on rung 1 until an explicit
            // collation overrides it.
            let rung = if undecided { 1 } else { held.coercibility };
            if held.charset() != next.charset() {
                if held.collation == BINARY {
                    if rung > next.coercibility {
                        held = next;
                    }
                } else if next.collation == BINARY {
                    if next.coercibility <= rung {
                        held = next;
                        undecided = false;
                    }
                } else {
                    return Err(self.refuse(
                        whole,
                        "the collation of strings in different character sets is not modelled",
                    ));
                }
            } else if next.coercibility < rung {
                held = next;
                undecided = false;
            } else if next.coercibility == rung && next.collation != held.collation {
                if rung == 0 {
                    return Err(self.refuse(whole, "illegal mix of explicit collations"));
                }
                if next.collation.ends_with("_bin") {
                    held = next;
                } else if !held.collation.ends_with("_bin") {
                    undecided = true;
                }
            }
        }
        if undecided {
            return Err(self.refuse(whole, "the NONE coercibility rung is not modelled"));
        }
        Ok(held)
    }

    /// The result of a function that returns one of its arguments: strings
    /// outrank numbers and dates, which outrank `NULL`.
    fn conditional(&self, whole: &Expr, results: &[&Expr]) -> Result<Identity, BindError> {
        let identities = results
            .iter()
            .map(|result| self.identity(result))
            .collect::<Result<Vec<_>, _>>()?;
        let strings = identities
            .iter()
            .filter(|identity| {
                matches!(
                    identity.class,
                    Class::Text | Class::Bytes | Class::JsonColumn
                )
            })
            .map(|identity| {
                if identity.class == Class::JsonColumn {
                    Identity::text(JSON_TEXT, 2)
                } else {
                    identity.clone()
                }
            })
            .collect::<Vec<_>>();
        if !strings.is_empty() {
            // Only JSON columns: the result is JSON, not its text.
            if identities
                .iter()
                .all(|identity| matches!(identity.class, Class::JsonColumn | Class::Null))
            {
                return Err(self.unmodelled(whole));
            }
            return self.aggregate(whole, strings);
        }
        if identities
            .iter()
            .any(|identity| identity.class == Class::Temporal)
        {
            return Err(self.unmodelled(whole));
        }
        if identities
            .iter()
            .any(|identity| identity.class == Class::Number)
        {
            return Ok(Identity::of_class(Class::Number));
        }
        Ok(Identity::of_class(Class::Null))
    }

    /// An operator or predicate that answers a number.
    fn number(&self, expression: &Expr) -> Result<Identity, BindError> {
        match self.bound_type(expression)? {
            Some(
                DataType::Boolean
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Float32
                | DataType::Float64
                | DataType::Decimal { .. },
            ) => Ok(Identity::of_class(Class::Number)),
            _ => Err(self.unmodelled(expression)),
        }
    }

    fn column(&self, expression: &Expr) -> Result<Identity, BindError> {
        let bound = bind_expr_inner(
            expression,
            self.tables,
            &mut None,
            &mut None,
            self.subqueries,
        )?;
        let BoundExprKind::Column(column) = &bound.kind else {
            return Err(self.unmodelled(expression));
        };
        // A column of a derived table carries whatever the expression under
        // it did, which depends on how the server chose to evaluate it.
        let stored = self
            .tables
            .iter()
            .any(|table| table.relation_name == column.relation_name && table.input.is_none());
        if !stored || column.outer || column.geometry || column.bit_width.is_some() {
            return Err(self.unmodelled(expression));
        }
        Ok(match column.data_type {
            DataType::Utf8 => Identity::text(
                column
                    .collation
                    .as_deref()
                    .unwrap_or(crate::bound::DEFAULT_TEXT_COLLATION),
                2,
            ),
            DataType::Binary => Identity::bytes(2),
            DataType::Json => Identity::of_class(Class::JsonColumn),
            DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. } => {
                Identity::of_class(Class::Temporal)
            }
            DataType::Year => return Err(self.unmodelled(expression)),
            _ => Identity::of_class(Class::Number),
        })
    }

    fn cast_target(&self, whole: &Expr, target: &SqlDataType) -> Result<Identity, BindError> {
        let written = target.to_string().to_ascii_uppercase();
        let plain = |name: &str| {
            written.strip_prefix(name).is_some_and(|rest| {
                rest.is_empty()
                    || rest
                        .strip_prefix('(')
                        .and_then(|rest| rest.strip_suffix(')'))
                        .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
            })
        };
        // A cast to text takes the connection's collation as a column
        // would, whatever its operand carried.
        if plain("CHAR") {
            let mut identity = self.connection(whole)?;
            identity.coercibility = 2;
            return Ok(identity);
        }
        if plain("BINARY") {
            return Ok(Identity::bytes(2));
        }
        match super::function::cast_data_type(target) {
            Some(
                DataType::Int64
                | DataType::UInt64
                | DataType::Float32
                | DataType::Float64
                | DataType::Decimal { .. },
            ) => Ok(Identity::of_class(Class::Number)),
            Some(DataType::Date32 | DataType::DateTime64 { .. } | DataType::Time64 { .. }) => {
                Ok(Identity::of_class(Class::Temporal))
            }
            Some(DataType::Json) => Ok(Identity::text(JSON_TEXT, 2)),
            _ => Err(self.unmodelled(whole)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn identity(&self, expression: &Expr) -> Result<Identity, BindError> {
        // A value the connection pinned for this statement is rung 3.
        if crate::system_variables::literal(expression).is_some() {
            return Err(self.refuse(expression, "the SYSCONST coercibility rung is not modelled"));
        }
        match expression {
            Expr::Nested(inner)
            | Expr::UnaryOp {
                op: UnaryOperator::Plus,
                expr: inner,
            } => self.identity(inner),
            Expr::Collate {
                expr: inner,
                collation,
            } => {
                let parts = object_name_parts(collation)?;
                let [name] = parts.as_slice() else {
                    return Err(self.unmodelled(expression));
                };
                let name = name.to_ascii_lowercase();
                // A legacy name is compared under the profile it maps to;
                // its own name and character set are not kept.
                let exact = crate::bound::NamedCollation::from_name(&name)
                    .is_some_and(|named| named.as_str() == name);
                let operand = self.identity(inner)?;
                if !exact || operand.class != Class::Text || operand.charset() != charset_of(&name)
                {
                    return Err(self.unmodelled(expression));
                }
                Ok(Identity::text(&name, 0))
            }
            Expr::Value(value) => match &value.value {
                SqlValue::SingleQuotedString(_) | SqlValue::DoubleQuotedString(_) => {
                    self.connection(expression)
                }
                SqlValue::NationalStringLiteral(_) => Ok(Identity::text(SERVER_TEXT, 4)),
                SqlValue::HexStringLiteral(_) | SqlValue::SingleQuotedByteStringLiteral(_) => {
                    Ok(Identity::bytes(4))
                }
                SqlValue::Number(..) | SqlValue::Boolean(_) => {
                    Ok(Identity::of_class(Class::Number))
                }
                SqlValue::Null => Ok(Identity::of_class(Class::Null)),
                _ => Err(self.unmodelled(expression)),
            },
            Expr::Prefixed { prefix, value } => {
                let string = matches!(
                    value.as_ref(),
                    Expr::Value(value) if matches!(
                        value.value,
                        SqlValue::SingleQuotedString(_) | SqlValue::DoubleQuotedString(_)
                    )
                );
                match prefix.value.to_ascii_lowercase().as_str() {
                    "_utf8mb4" if string => {
                        Ok(Identity::text(crate::bound::DEFAULT_TEXT_COLLATION, 4))
                    }
                    "_binary" if string => Ok(Identity::bytes(4)),
                    _ => Err(self.unmodelled(expression)),
                }
            }
            Expr::Identifier(identifier)
                if identifier.quote_style.is_none() && identifier.value.starts_with('@') =>
            {
                Err(self.refuse(expression, "the collation of a variable is not modelled"))
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => self.column(expression),
            Expr::Function(function) => self.call(expression, function),
            Expr::Cast {
                kind: CastKind::Cast,
                expr: inner,
                data_type,
                array: false,
                format: None,
            } => {
                // The operand is read for what it refuses, not for its rung.
                if matches!(inner.as_ref(), Expr::BinaryOp { .. }) {
                    return Err(self.unmodelled(expression));
                }
                self.identity(inner)?;
                self.cast_target(expression, data_type)
            }
            Expr::Convert {
                is_try: false,
                expr: inner,
                data_type,
                charset,
                target_before_value: false,
                styles,
            } if styles.is_empty() => {
                self.identity(inner)?;
                match (data_type, charset) {
                    (Some(target), None) => self.cast_target(expression, target),
                    (None, Some(charset)) => {
                        match charset.to_string().to_ascii_lowercase().as_str() {
                            "utf8mb4" => {
                                Ok(Identity::text(crate::bound::DEFAULT_TEXT_COLLATION, 2))
                            }
                            "binary" => Ok(Identity::bytes(2)),
                            _ => Err(self.unmodelled(expression)),
                        }
                    }
                    _ => Err(self.unmodelled(expression)),
                }
            }
            Expr::Trim { expr: inner, .. } | Expr::Substring { expr: inner, .. } => {
                self.as_string(inner)
            }
            Expr::Case {
                conditions,
                else_result,
                ..
            } => {
                let results = conditions
                    .iter()
                    .map(|branch| &branch.result)
                    .chain(else_result.as_deref())
                    .collect::<Vec<_>>();
                self.conditional(expression, &results)
            }
            Expr::UnaryOp {
                op: UnaryOperator::Minus | UnaryOperator::Not,
                ..
            }
            | Expr::BinaryOp {
                op:
                    BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo
                    | BinaryOperator::MyIntegerDivide
                    | BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Spaceship
                    | BinaryOperator::And
                    | BinaryOperator::Or
                    | BinaryOperator::Xor,
                ..
            }
            | Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsTrue(_)
            | Expr::IsNotTrue(_)
            | Expr::IsFalse(_)
            | Expr::IsNotFalse(_)
            | Expr::Like { .. }
            | Expr::InList { .. }
            | Expr::Between { .. }
            | Expr::Extract { .. }
            | Expr::Ceil { .. }
            | Expr::Floor { .. } => self.number(expression),
            _ => Err(self.unmodelled(expression)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn call(&self, whole: &Expr, function: &Function) -> Result<Identity, BindError> {
        let plain = !function.uses_odbc_syntax
            && matches!(function.parameters, FunctionArguments::None)
            && function.filter.is_none()
            && function.null_treatment.is_none()
            && function.over.is_none()
            && function.within_group.is_empty();
        let parts = object_name_parts(&function.name)?;
        let [name] = parts.as_slice() else {
            return Err(self.unmodelled(whole));
        };
        let name = name.to_ascii_uppercase();
        let mut arguments = Vec::new();
        match &function.args {
            FunctionArguments::None if plain => {}
            FunctionArguments::List(list)
                if plain && list.clauses.is_empty() && list.duplicate_treatment.is_none() =>
            {
                for argument in &list.args {
                    let FunctionArg::Unnamed(FunctionArgExpr::Expr(argument)) = argument else {
                        return Err(self.unmodelled(whole));
                    };
                    arguments.push(argument);
                }
            }
            _ => return Err(self.unmodelled(whole)),
        }
        let argument = |index: usize| {
            arguments
                .get(index)
                .copied()
                .ok_or_else(|| self.unmodelled(whole))
        };
        let strings = |from: usize| {
            let parts = arguments
                .iter()
                .skip(from)
                .map(|argument| self.as_string(argument))
                .collect::<Result<Vec<_>, _>>()?;
            self.aggregate(whole, parts)
        };
        match name.as_str() {
            // The first argument alone decides.
            "UPPER" | "UCASE" | "LOWER" | "LCASE" | "LTRIM" | "RTRIM" | "TRIM" | "SUBSTRING"
            | "SUBSTR" | "MID" | "LEFT" | "RIGHT" | "REPLACE" | "REVERSE" | "REPEAT" | "LPAD"
            | "RPAD" | "SUBSTRING_INDEX" | "INSERT" | "SOUNDEX" => self.as_string(argument(0)?),
            "QUOTE" => {
                let identity = self.identity(argument(0)?)?;
                if identity.is_string() {
                    Ok(identity)
                } else {
                    Err(self.unmodelled(whole))
                }
            }
            // Every string argument weighs in.
            "CONCAT" | "CONCAT_WS" => strings(0),
            "ELT" | "MAKE_SET" if arguments.len() >= 2 => strings(1),
            "REGEXP_REPLACE" | "REGEXP_SUBSTR" if arguments.len() >= 2 => {
                let parts = vec![self.as_string(arguments[0])?, self.as_string(arguments[1])?];
                self.aggregate(whole, parts)
            }
            "IF" if arguments.len() == 3 => self.conditional(whole, &arguments[1..]),
            "IFNULL" | "COALESCE" | "GREATEST" | "LEAST" if !arguments.is_empty() => {
                self.conditional(whole, &arguments)
            }
            "NULLIF" if arguments.len() == 2 => {
                let identity = self.identity(arguments[0])?;
                if matches!(identity.class, Class::Temporal | Class::JsonColumn) {
                    return Err(self.unmodelled(whole));
                }
                Ok(identity)
            }
            // Text the function writes itself, in the connection's
            // character set.
            "HEX" | "MD5" | "SHA1" | "SHA" | "SHA2" | "BIN" | "OCT" | "CONV" | "TO_BASE64"
            | "FORMAT" | "INET_NTOA" | "INET6_NTOA" | "BIN_TO_UUID" | "DAYNAME" | "MONTHNAME"
            | "SPACE" | "GET_FORMAT" => self.connection(whole),
            "UNHEX" | "FROM_BASE64" | "INET6_ATON" | "UUID_TO_BIN" | "CHAR" => {
                Ok(Identity::bytes(4))
            }
            "UUID" | "COLLATION" | "CHARSET" => Ok(Identity::text(SERVER_TEXT, 4)),
            "JSON_UNQUOTE" | "JSON_TYPE" | "JSON_QUOTE" | "JSON_PRETTY" => {
                Ok(Identity::text(JSON_TEXT, 4))
            }
            "JSON_EXTRACT"
            | "JSON_OBJECT"
            | "JSON_ARRAY"
            | "JSON_KEYS"
            | "JSON_SEARCH"
            | "JSON_MERGE_PATCH"
            | "JSON_MERGE_PRESERVE"
            | "JSON_SET"
            | "JSON_REMOVE"
            | "JSON_INSERT"
            | "JSON_REPLACE"
            | "JSON_ARRAY_APPEND" => Ok(Identity::text(JSON_TEXT, 2)),
            "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "SYSDATE"
            | "CURDATE" | "CURRENT_DATE" | "CURTIME" | "CURRENT_TIME" | "UTC_DATE" | "UTC_TIME"
            | "UTC_TIMESTAMP" | "DATE" | "TIME" | "LAST_DAY" | "MAKEDATE" | "MAKETIME"
            | "SEC_TO_TIME" | "STR_TO_DATE" | "TIMEDIFF" | "CONVERT_TZ" | "FROM_DAYS" => {
                Ok(Identity::of_class(Class::Temporal))
            }
            "TIMESTAMP" | "FROM_UNIXTIME" if arguments.len() == 1 => {
                Ok(Identity::of_class(Class::Temporal))
            }
            // A date stays a date; text that spells one comes back as text
            // of the connection.
            "DATE_ADD" | "DATE_SUB" | "ADDDATE" | "SUBDATE" | "ADDTIME" | "SUBTIME"
            | "TIMESTAMPADD" => {
                let subject = argument(if name == "TIMESTAMPADD" { 2 } else { 0 })?;
                let identity = self.identity(subject)?;
                match identity.class {
                    Class::Temporal => Ok(identity),
                    Class::Text => self.connection(whole),
                    _ => Err(self.unmodelled(whole)),
                }
            }
            // The format's rung under the connection's collation.
            "DATE_FORMAT" | "TIME_FORMAT" if arguments.len() == 2 => {
                let format = self.identity(arguments[1])?;
                if format.class != Class::Text || !matches!(format.coercibility, 2 | 4) {
                    return Err(self.unmodelled(whole));
                }
                let mut identity = self.connection(whole)?;
                identity.coercibility = format.coercibility;
                Ok(identity)
            }
            "ABS" | "ROUND" | "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" | "OCTET_LENGTH"
            | "BIT_LENGTH" | "ASCII" | "ORD" | "FIND_IN_SET" | "STRCMP" | "CRC32" | "BIT_COUNT"
            | "SIGN" | "FLOOR" | "CEIL" | "CEILING" | "TRUNCATE" | "MOD" | "POW" | "POWER"
            | "SQRT" | "EXP" | "LN" | "LOG" | "LOG2" | "LOG10" | "SIN" | "COS" | "TAN" | "COT"
            | "ASIN" | "ACOS" | "ATAN" | "ATAN2" | "DEGREES" | "RADIANS" | "PI" | "RAND"
            | "JSON_LENGTH" | "JSON_VALID" | "JSON_CONTAINS" | "JSON_DEPTH" | "JSON_OVERLAPS"
            | "ISNULL" | "TO_DAYS" | "TO_SECONDS" | "TIME_TO_SEC" | "PERIOD_ADD"
            | "PERIOD_DIFF" | "INET_ATON" | "IS_IPV4" | "IS_IPV6" | "IS_UUID" | "UUID_SHORT"
            | "MICROSECOND" | "SECOND" | "MINUTE" | "HOUR" | "DAY" | "DAYOFMONTH" | "DAYOFWEEK"
            | "DAYOFYEAR" | "WEEKDAY" | "WEEK" | "WEEKOFYEAR" | "YEARWEEK" | "MONTH"
            | "QUARTER" | "YEAR" | "DATEDIFF" | "UNIX_TIMESTAMP" | "TIMESTAMPDIFF"
            | "COERCIBILITY" => Ok(Identity::of_class(Class::Number)),
            "USER" | "CURRENT_USER" | "SESSION_USER" | "SYSTEM_USER" | "DATABASE" | "SCHEMA"
            | "VERSION" | "CURRENT_ROLE" | "ICU_VERSION" => {
                Err(self.refuse(whole, "the SYSCONST coercibility rung is not modelled"))
            }
            _ => Err(self.unmodelled(whole)),
        }
    }
}

/// The collation and coercibility of `expression` as `function` (`COLLATION`,
/// `CHARSET`, `COERCIBILITY`, `WEIGHT_STRING`) was asked for it.
///
/// # Errors
/// Refuses, naming the function and the expression, when the expression
/// does not bind or its collation is not modelled.
pub(super) fn identity(
    function: &str,
    expression: &Expr,
    tables: &[BoundTable],
    subqueries: Option<&SubqueryResolver<'_>>,
) -> Result<Identity, BindError> {
    let scope = Scope {
        name: function,
        tables,
        subqueries,
    };
    let identity = scope.identity(expression)?;
    // The whole expression has to bind as well: an argument the rule above
    // never reads may still name a column that does not exist.
    bind_expr_inner(expression, tables, &mut None, &mut None, subqueries)?;
    Ok(identity)
}
