//! What the rows a table already holds read for a column added to it.
//!
//! An `ALTER TABLE ... ADD COLUMN` carries no row events: the source fills
//! the new column into every row it holds - with the column's default, the
//! type's implicit default for a `NOT NULL` column declared without one, or
//! the statement's own time for `CURRENT_TIMESTAMP` - and the stream never
//! sees those values. The mirror evolves the table in place when it can
//! reproduce that value exactly, recording it as the column's fill so every
//! row stored before the column existed reads it. When it cannot, the reason
//! comes back and the table is recopied instead.

use std::collections::BTreeMap;

use chrono::{Datelike as _, Timelike as _};
use mysql_async::Value as MysqlValue;
use pintail_probe::{SourceColumn, SourceTable};
use pintail_types::{DataType, Value};

use crate::ddl::{self, AddedColumn, DeclaredDefault};

/// When and where the statement that added the columns ran, from its binlog
/// event: the seconds of the event header, the microseconds the server
/// records when the statement read sub-second time, and the session time
/// zone it records when the statement converted a time through it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StatementClock {
    pub(crate) seconds: u32,
    pub(crate) micros: Option<u32>,
    pub(crate) time_zone: Option<String>,
}

/// What the fills of one statement are worked out from.
#[derive(Debug, Default)]
pub(crate) struct FillContext {
    /// Whether the source reports defaults in `MySQL`'s catalogue form.
    pub(crate) mysql_catalogue: bool,
    /// The statement's own column definitions, when the change came from a
    /// statement the stream read.
    pub(crate) declared: Option<Vec<AddedColumn>>,
    pub(crate) clock: Option<StatementClock>,
    /// Times the source converted through a named time zone, by lowercased
    /// column name: a `DATETIME` filled with the statement's time, or a
    /// `TIMESTAMP` literal read in the statement's zone.
    pub(crate) converted: BTreeMap<String, Result<MysqlValue, String>>,
}

impl FillContext {
    fn declaration(&self, name: &str) -> Option<&AddedColumn> {
        self.declared
            .as_ref()?
            .iter()
            .find(|declared| declared.name.eq_ignore_ascii_case(name))
    }
}

/// How an added column's fill was decided, for the log line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FillKind {
    /// Nullable with no default: the rows read NULL.
    Null,
    /// The column's literal default.
    Literal,
    /// The type's implicit default, for `NOT NULL` without a default.
    Implicit,
    /// The time the statement ran.
    StatementTime,
}

/// A conversion the source has to make before `column`'s fill is known: the
/// SQL and its parameters, `None` when there is none.
pub(crate) fn source_conversion(
    column: &SourceColumn,
    context: &FillContext,
) -> Option<(&'static str, Vec<MysqlValue>)> {
    let declared = context.declaration(&column.name)?;
    let clock = context.clock.as_ref()?;
    let zone = clock.time_zone.clone()?;
    let data_type = column.mysql_data_type.to_ascii_lowercase();
    if data_type == "datetime" && filled_with_statement_time(column, &declared.default) {
        let micros = i64::from(clock.seconds)
            .saturating_mul(1_000_000)
            .saturating_add(i64::from(clock.micros.unwrap_or(0)));
        return Some((
            "SELECT CONVERT_TZ(TIMESTAMPADD(MICROSECOND, ?, '1970-01-01 00:00:00'), '+00:00', ?)",
            vec![MysqlValue::Int(micros), MysqlValue::from(zone)],
        ));
    }
    if data_type == "timestamp"
        && let Some(literal) = timestamp_literal(column, &declared.default)
        && !literal.starts_with("0000-00-00")
    {
        return Some((
            "SELECT CONVERT_TZ(?, ?, '+00:00')",
            vec![MysqlValue::from(literal), MysqlValue::from(zone)],
        ));
    }
    None
}

/// Whether the rows already stored took the statement's time: the
/// statement says `CURRENT_TIMESTAMP`, or says something this does not read
/// and the catalogue says `CURRENT_TIMESTAMP`.
fn filled_with_statement_time(column: &SourceColumn, declared: &DeclaredDefault) -> bool {
    match declared {
        DeclaredDefault::CurrentTimestamp => true,
        DeclaredDefault::Other => current_timestamp_precision(column).is_some(),
        _ => false,
    }
}

/// The `TIMESTAMP` literal the statement's session read in its time zone:
/// the statement's own, or the catalogue's when the statement's is not one
/// this reads.
fn timestamp_literal<'a>(
    column: &'a SourceColumn,
    declared: &'a DeclaredDefault,
) -> Option<&'a str> {
    match declared {
        DeclaredDefault::Text(literal) => Some(literal.trim()),
        DeclaredDefault::Other if !column.default_generated => column.default_value.as_deref(),
        _ => None,
    }
}

/// Records the fill of every column `refreshed` adds to `previous`, or
/// returns why one of them cannot be reproduced and the table has to be
/// recopied instead.
pub(crate) fn resolve_added_fills(
    previous: &SourceTable,
    refreshed: &mut SourceTable,
    context: &FillContext,
) -> Result<Vec<(String, FillKind)>, String> {
    let mut decided = Vec::new();
    for column in &mut refreshed.columns {
        let known = previous
            .columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&column.name));
        if known {
            continue;
        }
        let (fill, kind) = added_column_fill(column, context).map_err(|reason| {
            format!(
                "column {} joined the schema with values the rows already copied need, and \
                 {reason}; the table is recopied instead of evolved in place",
                column.name
            )
        })?;
        column.absent_fill = fill;
        decided.push((column.name.clone(), kind));
    }
    Ok(decided)
}

fn added_column_fill(
    column: &SourceColumn,
    context: &FillContext,
) -> Result<(Option<Value>, FillKind), String> {
    if column.virtual_generated()
        || column.generated_stored
        || column.generation_captured && !column.generation_expression.is_empty()
    {
        return Err("it is a generated column, computed by the source for every row".to_owned());
    }
    if column.auto_increment {
        return Err("AUTO_INCREMENT numbered the rows the source already held".to_owned());
    }
    let has_values = column.default_value.is_some() || column.default_generated;
    if !context.mysql_catalogue {
        if has_values || !column.nullable {
            return Err(
                "this source reports defaults in a form the mirror does not read".to_owned(),
            );
        }
        return Ok((None, FillKind::Null));
    }
    // The statement is the record of what the rows were filled with; the
    // catalogue describes the column as it is NOW, which a later
    // `SET DEFAULT` the stream has not reached yet may already have changed.
    // So the statement decides wherever its text does.
    let Some(declared) = context.declaration(&column.name) else {
        return catalogue_fill(column, None, context);
    };
    match &declared.default {
        DeclaredDefault::Absent if column.nullable => Ok((None, FillKind::Null)),
        DeclaredDefault::Absent => {
            implicit_default(column).map(|value| (Some(value), FillKind::Implicit))
        }
        DeclaredDefault::Null => Ok((None, FillKind::Null)),
        DeclaredDefault::CurrentTimestamp => {
            let DataType::DateTime64 { fsp } = column.pintail_type else {
                return Err("its CURRENT_TIMESTAMP default is not on a date-time".to_owned());
            };
            statement_time(column, fsp, context).map(|value| (Some(value), FillKind::StatementTime))
        }
        DeclaredDefault::Number(literal) | DeclaredDefault::Text(literal) => {
            let data_type = column.mysql_data_type.to_ascii_lowercase();
            if data_type == "float" {
                return float_fill(column, literal);
            }
            if data_type == "timestamp" && !literal.trim().starts_with("0000-00-00") {
                return converted(column, context).map(|value| (Some(value), FillKind::Literal));
            }
            if let Some(value) = canonical_literal(column, literal) {
                return Ok((Some(value), FillKind::Literal));
            }
            // A literal the source normalised - rounded, re-cased, reordered -
            // is taken from the catalogue, as long as the catalogue still
            // shows the value the statement wrote.
            let (value, kind) = catalogue_fill(column, Some(declared), context)?;
            match (&value, column.default_value.as_deref()) {
                (Some(stored), Some(printed))
                    if !column.default_generated
                        && declared_agrees(column, &declared.default, printed, stored) =>
                {
                    Ok((value, kind))
                }
                _ => Err(
                    "the source's default is no longer the one the statement declared, and the \
                     statement's literal is not in the form the source stores"
                        .to_owned(),
                ),
            }
        }
        DeclaredDefault::Other => catalogue_fill(column, Some(declared), context),
    }
}

/// The fill the source's catalogue alone decides: its literal default, a
/// type's implicit one, or the statement's time for `CURRENT_TIMESTAMP`
/// when a statement is in hand.
fn catalogue_fill(
    column: &SourceColumn,
    declared: Option<&AddedColumn>,
    context: &FillContext,
) -> Result<(Option<Value>, FillKind), String> {
    if column.default_generated {
        let Some(fsp) = current_timestamp_precision(column) else {
            return Err(format!(
                "its default ({}) is an expression the source evaluated for each row",
                column.default_value.as_deref().unwrap_or("")
            ));
        };
        if declared.is_none() {
            return Err(
                "its default is the time of the statement that added it, which only that \
                 statement's binlog event records"
                    .to_owned(),
            );
        }
        return statement_time(column, fsp, context)
            .map(|value| (Some(value), FillKind::StatementTime));
    }
    let Some(text) = column.default_value.as_deref() else {
        if column.nullable {
            return Ok((None, FillKind::Null));
        }
        return implicit_default(column).map(|value| (Some(value), FillKind::Implicit));
    };
    let value = match column.mysql_data_type.to_ascii_lowercase().as_str() {
        // The catalogue prints a FLOAT default to six significant digits,
        // which names several single-precision values; only the literal the
        // statement wrote says which one was stored.
        "float" => {
            return Err(
                "its FLOAT default is printed by the source to fewer digits than it stores, and \
                 the statement does not say which value it is"
                    .to_owned(),
            );
        }
        // Read in the time zone of the session that ran the statement,
        // which only its binlog event records.
        "timestamp" if !text.starts_with("0000-00-00") => {
            if declared.is_none() {
                return Err(
                    "its TIMESTAMP default was read in a time zone only the statement's binlog \
                     event records"
                        .to_owned(),
                );
            }
            converted(column, context)?
        }
        _ => literal_value(column, text)?,
    };
    Ok((Some(value), FillKind::Literal))
}

/// A FLOAT default from the statement's literal: the source converts the
/// literal to a double and stores that as a single. A declared scale rounds
/// first, so a literal with more digits than the scale is not reproduced.
fn float_fill(column: &SourceColumn, literal: &str) -> Result<(Option<Value>, FillKind), String> {
    let literal = literal.trim();
    let written = literal
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("its FLOAT default {literal} is not a number"))?;
    if let Some(scale) = column
        .mysql_column_type
        .split_once(',')
        .and_then(|(_, scale)| {
            scale
                .trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse::<usize>()
                .ok()
        })
    {
        let digits = literal
            .split_once('.')
            .map_or(0, |(_, fraction)| fraction.len());
        if digits > scale || literal.contains(['e', 'E']) {
            return Err(format!(
                "its FLOAT default {literal} is rounded to the declared scale, which the mirror \
                 does not reproduce"
            ));
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    let single = written as f32;
    if !single.is_finite() {
        return Err(format!("its FLOAT default {literal} is out of range"));
    }
    Ok((Some(Value::float64(f64::from(single))), FillKind::Literal))
}

/// The value a literal the statement wrote is stored as, when the literal is
/// already in the form the source stores it in - no rounding, re-casing,
/// reordering or padding for the source to have done. `None` otherwise.
#[allow(clippy::too_many_lines)] // one arm per source type
fn canonical_literal(column: &SourceColumn, literal: &str) -> Option<Value> {
    let data_type = column.mysql_data_type.to_ascii_lowercase();
    let declared_length = || {
        column
            .mysql_column_type
            .split_once('(')
            .and_then(|(_, rest)| rest.split(')').next())
            .and_then(|length| length.trim().parse::<usize>().ok())
    };
    match data_type.as_str() {
        "char" | "varchar" => {
            let charset = column
                .character_set
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            let representable = match charset.as_str() {
                "utf8mb4" => true,
                "utf8mb3" | "utf8" => literal.chars().all(|character| character.len_utf8() < 4),
                _ => literal.is_ascii(),
            };
            let text = if data_type == "char" {
                literal.trim_end_matches(' ')
            } else {
                literal
            };
            (representable
                && declared_length().is_some_and(|length| literal.chars().count() <= length))
            .then(|| Value::Utf8(text.to_owned()))
        }
        "enum" => pintail_types::declaration_labels(&column.mysql_column_type, "enum")?
            .into_iter()
            .find(|label| label == literal)
            .map(Value::Utf8),
        "set" => {
            let members = pintail_types::declaration_labels(&column.mysql_column_type, "set")?;
            let written = if literal.is_empty() {
                Vec::new()
            } else {
                literal.split(',').collect::<Vec<_>>()
            };
            let mut chosen = vec![false; members.len()];
            for member in written {
                let position = members.iter().position(|declared| declared == member)?;
                if std::mem::replace(&mut chosen[position], true) {
                    return None;
                }
            }
            Some(Value::Utf8(
                members
                    .iter()
                    .zip(chosen)
                    .filter(|(_, chosen)| *chosen)
                    .map(|(member, _)| member.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ))
        }
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            let digits = literal.strip_prefix('-').unwrap_or(literal);
            let canonical = !digits.is_empty()
                && digits.bytes().all(|byte| byte.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0'))
                && literal != "-0";
            if !canonical {
                return None;
            }
            map(column, MysqlValue::Bytes(literal.as_bytes().to_vec())).ok()
        }
        "decimal" | "numeric" => {
            let DataType::Decimal { precision, scale } = column.pintail_type else {
                return None;
            };
            let unsigned = literal.strip_prefix('-').unwrap_or(literal);
            let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            let canonical = !whole.is_empty()
                && whole.bytes().all(|byte| byte.is_ascii_digit())
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && (whole == "0" || !whole.starts_with('0'))
                && fraction.len() == usize::from(scale)
                && (scale > 0 || !unsigned.contains('.'))
                && whole.len() <= usize::from(precision.saturating_sub(scale)).max(1)
                && !(literal.starts_with('-')
                    && whole
                        .bytes()
                        .chain(fraction.bytes())
                        .all(|byte| byte == b'0'));
            canonical.then(|| Value::Utf8(literal.to_owned()))
        }
        "double" | "real" => literal
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(Value::float64),
        "year" => (literal.len() == 4
            && literal.bytes().all(|byte| byte.is_ascii_digit())
            && literal
                .parse::<u64>()
                .is_ok_and(|year| year == 0 || (1901..=2155).contains(&year)))
        .then(|| map(column, MysqlValue::Bytes(literal.as_bytes().to_vec())).ok())
        .flatten(),
        "date" | "datetime" => {
            let value = map(column, MysqlValue::Bytes(literal.as_bytes().to_vec())).ok()?;
            let Value::Utf8(stored) = &value else {
                return None;
            };
            // Only a literal the stored text begins with exactly: one the
            // source pads with zeros, never one it reformats or rounds.
            (literal.len() >= 10 && stored.starts_with(literal)).then_some(value)
        }
        "time" => {
            let DataType::Time64 { fsp } = column.pintail_type else {
                return None;
            };
            let unsigned = literal.strip_prefix('-').unwrap_or(literal);
            let (clock, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            let parts = clock.split(':').collect::<Vec<_>>();
            let canonical = parts.len() == 3
                && (2..=3).contains(&parts[0].len())
                && parts[1].len() == 2
                && parts[2].len() == 2
                && parts
                    .iter()
                    .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
                && parts[1] < "60"
                && parts[2] < "60"
                && parts[0].parse::<u32>().is_ok_and(|hours| hours <= 838)
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && fraction.len() <= usize::from(fsp)
                && (fsp > 0 || !unsigned.contains('.'));
            if !canonical {
                return None;
            }
            let sign = if literal.starts_with('-') { "-" } else { "" };
            let text = if fsp > 0 {
                format!(
                    "{sign}{clock}.{fraction:0<width$}",
                    width = usize::from(fsp)
                )
            } else {
                format!("{sign}{clock}")
            };
            map(column, MysqlValue::Bytes(text.into_bytes())).ok()
        }
        "bit" => {
            let width = declared_length().unwrap_or(1);
            let bits = literal.parse::<u64>().ok()?;
            (width >= 64 || bits >> width == 0)
                .then(|| map(column, MysqlValue::UInt(bits)).ok())
                .flatten()
        }
        "binary" | "varbinary" => {
            let mut bytes = literal.as_bytes().to_vec();
            if declared_length().is_some_and(|length| bytes.len() > length) {
                return None;
            }
            if let Some(width) = crate::decoder::fixed_binary_width(column) {
                crate::decoder::pad_fixed_binary(&mut bytes, width);
            }
            map(column, MysqlValue::Bytes(bytes)).ok()
        }
        _ => None,
    }
}

/// The fractional precision of a `TIMESTAMP` or `DATETIME` column whose
/// default is the time the row was written.
fn current_timestamp_precision(column: &SourceColumn) -> Option<u8> {
    if !column.default_generated {
        return None;
    }
    let DataType::DateTime64 { fsp } = column.pintail_type else {
        return None;
    };
    let text = column.default_value.as_deref()?.trim().to_ascii_uppercase();
    let rest = text.strip_prefix("CURRENT_TIMESTAMP")?;
    let declared = if rest.is_empty() {
        0
    } else {
        rest.strip_prefix('(')?
            .strip_suffix(')')?
            .trim()
            .parse()
            .ok()?
    };
    (declared == fsp).then_some(fsp)
}

/// The time the statement ran, as the column stores it: a `TIMESTAMP` in
/// UTC, a `DATETIME` in the statement's session time zone, either cut (not
/// rounded) to the column's precision, as the source does.
fn statement_time(column: &SourceColumn, fsp: u8, context: &FillContext) -> Result<Value, String> {
    let Some(clock) = context.clock.as_ref() else {
        return Err(
            "its default is the time of the statement that added it, which only that \
             statement's binlog event records"
                .to_owned(),
        );
    };
    if fsp > 0 && clock.micros.is_none() {
        return Err(
            "its default is the statement's time to the fraction of a second, which the \
             statement's binlog event does not record"
                .to_owned(),
        );
    }
    if column.mysql_data_type.eq_ignore_ascii_case("timestamp") {
        let seconds = i64::from(clock.seconds);
        let instant = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
            .ok_or_else(|| "the statement's time is out of range".to_owned())?;
        let part = |value: u32| u8::try_from(value).map_err(|error| error.to_string());
        let parts = MysqlValue::Date(
            u16::try_from(instant.year()).map_err(|error| error.to_string())?,
            part(instant.month())?,
            part(instant.day())?,
            part(instant.hour())?,
            part(instant.minute())?,
            part(instant.second())?,
            if fsp == 0 {
                0
            } else {
                clock.micros.unwrap_or(0)
            },
        );
        return map(column, parts);
    }
    converted(column, context)
}

fn converted(column: &SourceColumn, context: &FillContext) -> Result<Value, String> {
    match context.converted.get(&column.name.to_ascii_lowercase()) {
        Some(Ok(MysqlValue::NULL)) => Err(
            "the source could not convert its default through the statement's time zone".to_owned(),
        ),
        Some(Ok(value)) => map(column, value.clone()),
        Some(Err(reason)) => Err(reason.clone()),
        None => Err(
            "its default depends on a session time zone the statement's binlog event does not \
             record"
                .to_owned(),
        ),
    }
}

fn map(column: &SourceColumn, value: MysqlValue) -> Result<Value, String> {
    let mapped = pintail_snapshot::map_mysql_value("", column, value)
        .map_err(|error| format!("its default does not convert: {error}"))?;
    if matches!(mapped, Value::Null) {
        return Err("its default does not convert to a stored value".to_owned());
    }
    Ok(mapped)
}

/// A literal default as the catalogue prints it, as the column stores it.
fn literal_value(column: &SourceColumn, text: &str) -> Result<Value, String> {
    match column.mysql_data_type.to_ascii_lowercase().as_str() {
        "bit" => {
            let digits = text
                .strip_prefix("b'")
                .and_then(|rest| rest.strip_suffix('\''))
                .ok_or_else(|| format!("its BIT default {text} is not a bit literal"))?;
            let bits = u64::from_str_radix(digits, 2)
                .map_err(|_| format!("its BIT default {text} is not a bit literal"))?;
            map(column, MysqlValue::UInt(bits))
        }
        "binary" | "varbinary" => {
            let mut bytes = if text.is_empty() {
                Vec::new()
            } else {
                text.strip_prefix("0x")
                    .and_then(hex_bytes)
                    .ok_or_else(|| format!("its binary default {text} is not a hex literal"))?
            };
            if let Some(width) = crate::decoder::fixed_binary_width(column) {
                crate::decoder::pad_fixed_binary(&mut bytes, width);
            }
            map(column, MysqlValue::Bytes(bytes))
        }
        "json" | "geometry" | "point" | "linestring" | "polygon" | "multipoint"
        | "multilinestring" | "multipolygon" | "geometrycollection" | "tinyblob" | "blob"
        | "mediumblob" | "longblob" | "tinytext" | "text" | "mediumtext" | "longtext" => Err(
            format!("its default {text} is of a type the source only defaults by expression"),
        ),
        _ => map(column, MysqlValue::Bytes(text.as_bytes().to_vec())),
    }
}

fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}

/// What a `NOT NULL` column added without a default holds in the rows the
/// source already had: the type's implicit default.
fn implicit_default(column: &SourceColumn) -> Result<Value, String> {
    let data_type = column.mysql_data_type.to_ascii_lowercase();
    let text = match data_type.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "float"
        | "double" | "real" | "year" => "0".to_owned(),
        "bit" => return map(column, MysqlValue::UInt(0)),
        "decimal" | "numeric" => match column.pintail_type {
            DataType::Decimal { scale: 0, .. } => "0".to_owned(),
            DataType::Decimal { scale, .. } => format!("0.{}", "0".repeat(usize::from(scale))),
            _ => return Err("its DECIMAL type is not mapped as a decimal".to_owned()),
        },
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "set" => {
            String::new()
        }
        "enum" => pintail_types::declaration_labels(&column.mysql_column_type, "enum")
            .and_then(|labels| labels.into_iter().next())
            .ok_or_else(|| "its ENUM declaration cannot be read".to_owned())?,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            let mut bytes = Vec::new();
            if let Some(width) = crate::decoder::fixed_binary_width(column) {
                crate::decoder::pad_fixed_binary(&mut bytes, width);
            }
            return map(column, MysqlValue::Bytes(bytes));
        }
        "date" => "0000-00-00".to_owned(),
        "datetime" | "timestamp" => "0000-00-00 00:00:00".to_owned(),
        "time" => match column.pintail_type {
            DataType::Time64 { fsp: 0 } => "00:00:00".to_owned(),
            DataType::Time64 { fsp } => format!("00:00:00.{}", "0".repeat(usize::from(fsp))),
            _ => return Err("its TIME type is not mapped as a time".to_owned()),
        },
        "json" => "null".to_owned(),
        other => {
            return Err(format!(
                "it is NOT NULL without a default, and the {other} type's implicit value is not \
                 one the mirror reproduces"
            ));
        }
    };
    map(column, MysqlValue::Bytes(text.into_bytes()))
}

/// Whether the default the statement declared is the one the catalogue
/// reports now. A form this cannot read is taken on the catalogue's word.
#[allow(clippy::float_cmp)] // the same number written two ways is exactly equal
fn declared_agrees(
    column: &SourceColumn,
    declared: &DeclaredDefault,
    printed: &str,
    value: &Value,
) -> bool {
    let written = match declared {
        DeclaredDefault::Other => return true,
        DeclaredDefault::Absent | DeclaredDefault::Null | DeclaredDefault::CurrentTimestamp => {
            return false;
        }
        DeclaredDefault::Number(written) | DeclaredDefault::Text(written) => written.as_str(),
    };
    if literal_value(column, written).is_ok_and(|candidate| &candidate == value) {
        return true;
    }
    let data_type = column.mysql_data_type.to_ascii_lowercase();
    match data_type.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal"
        | "numeric" | "double" | "real" | "year" => {
            matches!(
                (written.trim().parse::<f64>(), printed.trim().parse::<f64>()),
                (Ok(left), Ok(right)) if left == right
            )
        }
        "time" | "datetime" => fraction_rounds_to(written.trim(), printed),
        "char" => written.trim_end_matches(' ') == printed.trim_end_matches(' '),
        "enum" => written
            .trim_end_matches(' ')
            .eq_ignore_ascii_case(printed.trim_end_matches(' ')),
        "set" => {
            let members = |text: &str| {
                let mut members = text
                    .split(',')
                    .filter(|member| !member.is_empty())
                    .map(|member| member.trim_end_matches(' ').to_ascii_lowercase())
                    .collect::<Vec<_>>();
                members.sort();
                members.dedup();
                members
            };
            members(written) == members(printed)
        }
        "binary" | "varbinary" => {
            let mut bytes = written.as_bytes().to_vec();
            if let Some(width) = crate::decoder::fixed_binary_width(column) {
                crate::decoder::pad_fixed_binary(&mut bytes, width);
            }
            matches!(value, Value::Binary(stored) if *stored == bytes)
        }
        _ => false,
    }
}

/// Whether a time written with more (or fewer) fractional digits than the
/// column keeps is the printed one: the same up to the fraction, and the
/// fraction rounded half up - or padded - to the printed width. A rounding
/// that carries into the seconds is not followed, and reads as different.
fn fraction_rounds_to(written: &str, printed: &str) -> bool {
    let (written_head, written_fraction) = written.split_once('.').unwrap_or((written, ""));
    let (printed_head, printed_fraction) = printed.split_once('.').unwrap_or((printed, ""));
    if written_head != printed_head || !written_fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let width = printed_fraction.len();
    if written_fraction.len() <= width {
        return format!("{written_fraction:0<width$}") == printed_fraction;
    }
    let kept = &written_fraction[..width];
    let round_up = written_fraction.as_bytes()[width] >= b'5';
    let Ok(kept) = (if kept.is_empty() {
        Ok(0)
    } else {
        kept.parse::<u64>()
    }) else {
        return false;
    };
    let rounded = kept + u64::from(round_up);
    if width == 0 {
        return rounded == 0;
    }
    let digits = format!("{rounded:0width$}");
    digits.len() == width && digits == printed_fraction
}

/// The clock of a statement's binlog event.
pub(crate) fn statement_clock(
    header_seconds: u32,
    status: &mysql_common::binlog::events::StatusVars<'_>,
) -> StatementClock {
    use mysql_common::binlog::{consts::StatusVarKey, events::StatusVarVal};
    let mut clock = StatementClock {
        seconds: header_seconds,
        ..StatementClock::default()
    };
    if let Some(variable) = status.get_status_var(StatusVarKey::TimeZone)
        && let Ok(StatusVarVal::TimeZone(zone)) = variable.get_value()
    {
        let zone = zone.as_str();
        if !zone.is_empty() {
            clock.time_zone = Some(zone.into_owned());
        }
    }
    // Three bytes on the wire; the decoder reads four and refuses it, so
    // the raw bytes it hands back are read here.
    if let Some(variable) = status.get_status_var(StatusVarKey::Microseconds) {
        clock.micros = match variable.get_value() {
            Ok(StatusVarVal::Microseconds(micros)) => Some(micros),
            Err(&[low, middle, high]) => {
                Some(u32::from(low) | (u32::from(middle) << 8) | (u32::from(high) << 16))
            }
            _ => None,
        }
        .filter(|micros| *micros < 1_000_000);
    }
    clock
}

/// The columns `statement` adds, when it is an `ALTER TABLE` that does.
pub(crate) fn declared_columns(statement: &str) -> Option<Vec<AddedColumn>> {
    ddl::added_columns(statement)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(
        name: &str,
        data_type: &str,
        column_type: &str,
        default: Option<&str>,
    ) -> SourceColumn {
        let shape = column_type
            .split_once('(')
            .and_then(|(_, rest)| rest.split(')').next())
            .filter(|_| matches!(data_type, "decimal" | "datetime" | "timestamp" | "time"))
            .map(|shape| {
                shape
                    .split(',')
                    .map(|part| part.trim().parse::<u8>().expect("numeric shape"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let temporal = data_type != "decimal";
        let raw_type = pintail_probe::declared_column(&pintail_probe::DeclaredColumn {
            ordinal: 1,
            name,
            data_type,
            column_type,
            numeric_precision: shape.first().copied().filter(|_| !temporal),
            numeric_scale: shape.get(1).copied().filter(|_| !temporal),
            datetime_precision: shape.first().copied().filter(|_| temporal),
            nullable: true,
            collation: None,
        })
        .expect("declared column");
        SourceColumn {
            default_value: default.map(str::to_owned),
            ..raw_type
        }
    }

    fn fill(column: &SourceColumn) -> Result<Option<Value>, String> {
        added_column_fill(
            column,
            &FillContext {
                mysql_catalogue: true,
                ..FillContext::default()
            },
        )
        .map(|(value, _)| value)
    }

    #[test]
    fn literal_defaults_convert_as_the_source_stores_them() {
        let cases = [
            (
                "varchar",
                "varchar(10)",
                "all ",
                Value::Utf8("all ".to_owned()),
            ),
            ("char", "char(6)", "ab", Value::Utf8("ab".to_owned())),
            ("enum", "enum('x','y')", "y", Value::Utf8("y".to_owned())),
            (
                "set",
                "set('a','b','c')",
                "a,c",
                Value::Utf8("a,c".to_owned()),
            ),
            ("int", "int", "-5", Value::Int64(-5)),
            (
                "bigint",
                "bigint unsigned",
                "18446744073709551615",
                Value::UInt64(u64::MAX),
            ),
            ("bit", "bit(5)", "b'101'", Value::UInt64(5)),
            (
                "binary",
                "binary(4)",
                "0x6162",
                Value::Binary(vec![b'a', b'b', 0, 0]),
            ),
            ("varbinary", "varbinary(8)", "", Value::Binary(Vec::new())),
            (
                "date",
                "date",
                "0000-00-00",
                Value::Utf8("0000-00-00".to_owned()),
            ),
            ("year", "year", "0000", Value::UInt64(0)),
        ];
        for (data_type, column_type, default, expected) in cases {
            let column = column("c", data_type, column_type, Some(default));
            assert_eq!(fill(&column), Ok(Some(expected)), "{column_type} {default}");
        }
    }

    #[test]
    fn expressions_generated_and_timed_defaults_recopy() {
        let mut expression = column("c", "varchar", "varchar(40)", Some("uuid()"));
        expression.default_generated = true;
        assert!(fill(&expression).is_err());
        let mut stamped = column("c", "timestamp", "timestamp", Some("CURRENT_TIMESTAMP"));
        stamped.default_generated = true;
        // No statement in hand: the time it ran is unknown.
        assert!(fill(&stamped).is_err());
        let float = column("c", "float", "float", Some("1.1"));
        assert!(
            fill(&float).is_err(),
            "a FLOAT default needs the statement's literal"
        );
        let mut counted = column("c", "int", "int", None);
        counted.auto_increment = true;
        assert!(fill(&counted).is_err());
    }

    fn declared_fill(
        column: &SourceColumn,
        statement: &str,
        clock: Option<StatementClock>,
    ) -> Result<Option<Value>, String> {
        added_column_fill(
            column,
            &FillContext {
                mysql_catalogue: true,
                declared: declared_columns(statement),
                clock,
                converted: BTreeMap::new(),
            },
        )
        .map(|(value, _)| value)
    }

    #[test]
    fn the_statement_decides_over_a_default_changed_since() {
        // The catalogue already shows the default a later SET DEFAULT gave
        // the column; the rows the ADD filled hold the one it declared.
        let mut later = column("state", "varchar", "varchar(8)", Some("old"));
        later.nullable = false;
        let statement = "ALTER TABLE t ADD COLUMN state VARCHAR(8) NOT NULL DEFAULT 'new' AFTER d";
        assert_eq!(
            declared_fill(&later, statement, None),
            Ok(Some(Value::Utf8("new".to_owned())))
        );
        // A literal the source normalises is taken from the catalogue only
        // while the catalogue still agrees with it.
        let price = column("price", "decimal", "decimal(8,3)", Some("1.500"));
        let declared = "ALTER TABLE t ADD COLUMN price DECIMAL(8,3) DEFAULT 1.5";
        assert_eq!(
            declared_fill(&price, declared, None),
            Ok(Some(Value::Utf8("1.500".to_owned())))
        );
        let moved = column("price", "decimal", "decimal(8,3)", Some("2.000"));
        assert!(declared_fill(&moved, declared, None).is_err());
        // No DEFAULT clause: NULL, whatever the catalogue says now.
        let nullable = column("note", "varchar", "varchar(8)", Some("since"));
        assert_eq!(
            declared_fill(&nullable, "ALTER TABLE t ADD COLUMN note VARCHAR(8)", None),
            Ok(None)
        );
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn a_float_default_is_the_single_of_the_written_literal() {
        let weight = column("weight", "float", "float", Some("1.12346"));
        assert_eq!(
            declared_fill(
                &weight,
                "ALTER TABLE t ADD COLUMN weight FLOAT DEFAULT 1.123456789",
                None
            ),
            Ok(Some(Value::float64(f64::from(1.123_456_789_f64 as f32))))
        );
    }

    #[test]
    fn current_timestamp_takes_the_statement_time_cut_to_the_precision() {
        let mut stamped = column(
            "at",
            "timestamp",
            "timestamp(3)",
            Some("CURRENT_TIMESTAMP(3)"),
        );
        stamped.pintail_type = DataType::DateTime64 { fsp: 3 };
        stamped.default_generated = true;
        let statement = "ALTER TABLE t ADD COLUMN at TIMESTAMP(3) DEFAULT CURRENT_TIMESTAMP(3)";
        let clock = StatementClock {
            seconds: 1_791_197_086,
            micros: Some(522_924),
            time_zone: None,
        };
        assert_eq!(
            declared_fill(&stamped, statement, Some(clock.clone())),
            Ok(Some(Value::Utf8("2026-10-05 10:44:46.522".to_owned())))
        );
        // Sub-second precision the event does not carry is not guessed.
        let without = StatementClock {
            micros: None,
            ..clock
        };
        assert!(declared_fill(&stamped, statement, Some(without)).is_err());
    }

    #[test]
    fn the_statement_clock_reads_its_three_byte_microseconds_and_zone() {
        // Q_TIME_ZONE_CODE "+00:00", then Q_MICROSECONDS 522924.
        let raw = [
            5, 6, b'+', b'0', b'0', b':', b'0', b'0', 13, 0xAC, 0xFA, 0x07,
        ];
        let event = mysql_common::binlog::events::QueryEvent::new(&raw[..], &b""[..]);
        assert_eq!(
            statement_clock(1_791_197_086, event.status_vars()),
            StatementClock {
                seconds: 1_791_197_086,
                micros: Some(522_924),
                time_zone: Some("+00:00".to_owned()),
            }
        );
    }

    #[test]
    fn not_null_without_a_default_reads_the_implicit_value() {
        let mut enumerated = column("c", "enum", "enum('p','q')", None);
        enumerated.nullable = false;
        assert_eq!(fill(&enumerated), Ok(Some(Value::Utf8("p".to_owned()))));
        let mut number = column("c", "int", "int", None);
        number.nullable = false;
        assert_eq!(fill(&number), Ok(Some(Value::Int64(0))));
        let nullable = column("c", "int", "int", None);
        assert_eq!(fill(&nullable), Ok(None));
    }
}
