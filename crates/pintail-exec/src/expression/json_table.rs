//! Row production for the `JSON_TABLE` table function.
//!
//! A bound table function compiles once into paths and typed columns; each
//! document then expands into rows through [`JsonTableRows::expand`]. The
//! paths are the ones every JSON function walks, and a matched value is
//! stored into its column the way `MySQL` stores a JSON value into a field
//! of the declared type: a value the type cannot hold is an error, which
//! the column's `ON ERROR` clause turns into NULL, a default or a failed
//! statement.

use pintail_sql::{
    BoundJsonTable, JsonTableColumn, JsonTableFallback, JsonTableType, ScalarFunction,
};
use pintail_types::{DataType, Value};

use super::{
    JsonStep, collect_json_matches, evaluate_eager_scalar_inner, json_path_steps, mysql_json_text,
    scalar_string,
};
use crate::{ExecError, collation::Collation};

/// A compiled `JSON_TABLE`: what a document expands into.
pub(crate) struct JsonTableRows {
    root: Level,
    width: usize,
}

/// One column list and the path whose matches are its rows.
struct Level {
    path: Vec<JsonStep>,
    columns: Vec<Column>,
    nested: Vec<Self>,
    /// Row positions of every column of this list and the lists under it,
    /// reset to NULL once the list's rows for one parent row are out.
    outputs: Vec<usize>,
}

enum Column {
    Ordinality {
        output: usize,
    },
    Value {
        output: usize,
        name: String,
        target: JsonTableType,
        path: Vec<JsonStep>,
        on_empty: Fallback,
        on_error: Fallback,
    },
    Exists {
        output: usize,
        path: Vec<JsonStep>,
        present: Value,
        absent: Value,
    },
}

/// A fallback with its `DEFAULT` already stored as the column's type.
enum Fallback {
    Null,
    Error,
    Value(Value),
}

/// Why a JSON value could not be stored into a column.
#[derive(Clone, Copy)]
enum Refusal {
    /// An array or an object met a scalar column.
    NotScalar,
    /// The value is past what the column's type holds.
    OutOfRange,
    /// The value does not read as the column's type at all.
    Invalid(&'static str),
}

impl Refusal {
    fn error(self, column: &str, row: usize) -> ExecError {
        ExecError::JsonTable(match self {
            Self::NotScalar => format!(
                "Can't store an array or an object in the scalar JSON_TABLE column '{column}'"
            ),
            Self::OutOfRange => {
                format!("Value is out of range for JSON_TABLE's column '{column}'")
            }
            Self::Invalid(kind) => {
                format!("Invalid JSON value for CAST to {kind} from column {column} at row {row}")
            }
        })
    }
}

impl JsonTableRows {
    /// Compiles the bound table function: parses every path and stores
    /// every `DEFAULT`.
    ///
    /// # Errors
    ///
    /// A malformed path is `MySQL`'s invalid-path error. A `DEFAULT` that
    /// is not JSON text, or that its column cannot hold, fails the
    /// statement here, before any document is read, as it does in `MySQL`.
    pub(crate) fn compile(table: &BoundJsonTable) -> Result<Self, ExecError> {
        Ok(Self {
            root: Level::compile(&table.path, &table.columns)?,
            width: table.width,
        })
    }

    /// Expands one document into rows, handing each to `emit`.
    ///
    /// A NULL document has no rows.
    ///
    /// # Errors
    ///
    /// Text that is not JSON, a column's `ERROR ON EMPTY` or `ERROR ON
    /// ERROR`, and whatever `emit` returns.
    pub(crate) fn expand(
        &self,
        document: &Value,
        emit: &mut dyn FnMut(&[Value]) -> Result<(), ExecError>,
    ) -> Result<(), ExecError> {
        if matches!(document, Value::Null) {
            return Ok(());
        }
        let document = serde_json::from_str::<serde_json::Value>(&scalar_string(document)?)
            .map_err(|error| {
                ExecError::JsonTable(format!(
                    "Invalid JSON text in argument 1 to function json_table: \"{error}\""
                ))
            })?;
        let mut row = vec![Value::Null; self.width];
        self.root.expand(&document, &mut row, emit)?;
        Ok(())
    }
}

impl Level {
    fn compile(path: &str, columns: &[JsonTableColumn]) -> Result<Self, ExecError> {
        let mut level = Self {
            path: json_path_steps(path)?,
            columns: Vec::new(),
            nested: Vec::new(),
            outputs: Vec::new(),
        };
        for column in columns {
            match column {
                JsonTableColumn::Ordinality { output } => {
                    level.outputs.push(*output);
                    level.columns.push(Column::Ordinality { output: *output });
                }
                JsonTableColumn::Value {
                    output,
                    name,
                    target,
                    path,
                    on_empty,
                    on_error,
                } => {
                    level.outputs.push(*output);
                    level.columns.push(Column::Value {
                        output: *output,
                        name: name.clone(),
                        target: *target,
                        path: json_path_steps(path)?,
                        on_empty: Fallback::compile(on_empty, *target, name)?,
                        on_error: Fallback::compile(on_error, *target, name)?,
                    });
                }
                JsonTableColumn::Exists {
                    output,
                    name,
                    target,
                    path,
                } => {
                    let flag = |flag: u64| {
                        store(&serde_json::Value::from(flag), *target)
                            .map_err(|refusal| refusal.error(name, 1))
                    };
                    level.outputs.push(*output);
                    level.columns.push(Column::Exists {
                        output: *output,
                        path: json_path_steps(path)?,
                        present: flag(1)?,
                        absent: flag(0)?,
                    });
                }
                JsonTableColumn::Nested { path, columns } => {
                    let nested = Self::compile(path, columns)?;
                    level.outputs.extend_from_slice(&nested.outputs);
                    level.nested.push(nested);
                }
            }
        }
        Ok(level)
    }

    /// Emits this list's rows for one value of the enclosing list, and
    /// answers whether its path matched anything there.
    ///
    /// Sibling nested lists do not multiply each other: each contributes
    /// its own rows in turn, the others' columns NULL beside them, and a
    /// row whose nested lists all come up empty is emitted once with every
    /// nested column NULL.
    fn expand(
        &self,
        context: &serde_json::Value,
        row: &mut [Value],
        emit: &mut dyn FnMut(&[Value]) -> Result<(), ExecError>,
    ) -> Result<bool, ExecError> {
        let mut matches = Vec::new();
        collect_json_matches(context, &self.path, &mut matches);
        for (index, value) in matches.iter().enumerate() {
            for column in &self.columns {
                match column {
                    Column::Ordinality { output } => {
                        row[*output] = Value::Int64(i64::try_from(index + 1).unwrap_or(i64::MAX));
                    }
                    Column::Exists {
                        output,
                        path,
                        present,
                        absent,
                    } => {
                        let mut found = Vec::new();
                        collect_json_matches(value, path, &mut found);
                        row[*output] = if found.is_empty() { absent } else { present }.clone();
                    }
                    Column::Value {
                        output,
                        name,
                        target,
                        path,
                        on_empty,
                        on_error,
                    } => {
                        row[*output] =
                            column_value(value, path, *target, on_empty, on_error, name, index)?;
                    }
                }
            }
            if self.nested.is_empty() {
                emit(row)?;
                continue;
            }
            let mut produced = false;
            for nested in &self.nested {
                produced |= nested.expand(value, row, emit)?;
                for output in &nested.outputs {
                    row[*output] = Value::Null;
                }
            }
            if !produced {
                emit(row)?;
            }
        }
        Ok(!matches.is_empty())
    }
}

impl Fallback {
    fn compile(
        fallback: &JsonTableFallback,
        target: JsonTableType,
        column: &str,
    ) -> Result<Self, ExecError> {
        let text = match fallback {
            JsonTableFallback::Null => return Ok(Self::Null),
            JsonTableFallback::Error => return Ok(Self::Error),
            JsonTableFallback::Default(text) => text,
        };
        let value = serde_json::from_str::<serde_json::Value>(text).map_err(|error| {
            ExecError::JsonTable(format!(
                "Invalid JSON text in argument 1 to function JSON_TABLE: \"{error}\""
            ))
        })?;
        if !matches!(target, JsonTableType::Json) && (value.is_array() || value.is_object()) {
            return Err(ExecError::JsonTable(format!(
                "Invalid default value for '{column}'"
            )));
        }
        // A default of JSON null does not read as any scalar type.
        if value.is_null() && !matches!(target, JsonTableType::Json) {
            return Err(Refusal::Invalid(cast_name(target)).error(column, 1));
        }
        store(&value, target)
            .map(Self::Value)
            .map_err(|refusal| refusal.error(column, 1))
    }
}

/// The value of one `PATH` column for one row.
fn column_value(
    context: &serde_json::Value,
    path: &[JsonStep],
    target: JsonTableType,
    on_empty: &Fallback,
    on_error: &Fallback,
    name: &str,
    index: usize,
) -> Result<Value, ExecError> {
    let mut found = Vec::new();
    collect_json_matches(context, path, &mut found);
    let stored = match found.as_slice() {
        [] => {
            return match on_empty {
                Fallback::Null => Ok(Value::Null),
                Fallback::Value(value) => Ok(value.clone()),
                Fallback::Error => Err(ExecError::JsonTable(format!(
                    "Missing value for JSON_TABLE column '{name}'"
                ))),
            };
        }
        [value] => store(value, target),
        // Several matches are an array of them: a document for a JSON
        // column, and not a scalar for any other.
        several if matches!(target, JsonTableType::Json) => {
            return Ok(Value::Utf8(mysql_json_text(&serde_json::Value::Array(
                several.iter().map(|value| (*value).clone()).collect(),
            ))));
        }
        _ => Err(Refusal::NotScalar),
    };
    match (stored, on_error) {
        (Ok(value), _) => Ok(value),
        (Err(_), Fallback::Null) => Ok(Value::Null),
        (Err(_), Fallback::Value(value)) => Ok(value.clone()),
        (Err(refusal), Fallback::Error) => Err(refusal.error(name, index + 1)),
    }
}

const fn cast_name(target: JsonTableType) -> &'static str {
    match target {
        JsonTableType::Integer { .. } => "INTEGER",
        JsonTableType::Decimal { .. } => "DECIMAL",
        JsonTableType::Float | JsonTableType::Double => "DOUBLE",
        JsonTableType::Text { .. } => "CHAR",
        JsonTableType::Json => "JSON",
        JsonTableType::Date => "DATE",
        JsonTableType::DateTime { .. } => "DATETIME",
    }
}

/// Stores one JSON value as a column of the declared type. JSON null is
/// SQL NULL in every type.
fn store(value: &serde_json::Value, target: JsonTableType) -> Result<Value, Refusal> {
    use serde_json::Value as Json;
    if value.is_null() {
        return Ok(Value::Null);
    }
    if matches!(target, JsonTableType::Json) {
        return Ok(Value::Utf8(mysql_json_text(value)));
    }
    if value.is_array() || value.is_object() {
        return Err(Refusal::NotScalar);
    }
    match target {
        JsonTableType::Integer { min, max } => store_integer(value, min, max),
        JsonTableType::Decimal { precision, scale } => {
            let text = match value {
                Json::Bool(flag) => u8::from(*flag).to_string(),
                Json::Number(number) => match (number.as_u64(), number.as_i64()) {
                    (Some(unsigned), _) => unsigned.to_string(),
                    (_, Some(signed)) => signed.to_string(),
                    // The shortest text that reads back as the double, in
                    // plain digits: what the value was written as.
                    _ => number
                        .as_f64()
                        .map(|float| float.to_string())
                        .ok_or(Refusal::Invalid("DECIMAL"))?,
                },
                Json::String(text) => text.clone(),
                Json::Null | Json::Array(_) | Json::Object(_) => {
                    return Err(Refusal::NotScalar);
                }
            };
            store_decimal(&text, precision, scale).map(Value::Utf8)
        }
        JsonTableType::Float | JsonTableType::Double => {
            let float = match value {
                Json::Bool(flag) => f64::from(u8::from(*flag)),
                Json::Number(number) => number.as_f64().ok_or(Refusal::Invalid("DOUBLE"))?,
                Json::String(text) => text_double(text)?,
                Json::Null | Json::Array(_) | Json::Object(_) => {
                    return Err(Refusal::NotScalar);
                }
            };
            if matches!(target, JsonTableType::Float) {
                #[allow(clippy::cast_possible_truncation)]
                let narrowed = float as f32;
                if !narrowed.is_finite() {
                    return Err(Refusal::OutOfRange);
                }
                return Ok(Value::float64(f64::from(narrowed)));
            }
            Ok(Value::float64(float))
        }
        JsonTableType::Text {
            max_chars,
            max_bytes,
        } => {
            let text = match value {
                Json::String(text) => text.clone(),
                other => mysql_json_text(other),
            };
            let fits = |limit: Option<u64>, length: usize| {
                limit.is_none_or(|limit| u64::try_from(length).is_ok_and(|length| length <= limit))
            };
            if fits(max_bytes, text.len())
                && (max_chars.is_none() || fits(max_chars, text.chars().count()))
            {
                Ok(Value::Utf8(text))
            } else {
                Err(Refusal::OutOfRange)
            }
        }
        JsonTableType::Date => store_calendar(value, DataType::Date32),
        JsonTableType::DateTime { fsp } => store_calendar(value, DataType::DateTime64 { fsp }),
        JsonTableType::Json => Ok(Value::Utf8(mysql_json_text(value))),
    }
}

/// An integer column: a JSON integer inside the type's range, a double
/// rounded half to even, a boolean as 1 or 0, or a string that is nothing
/// but an optionally signed run of digits after any leading spaces.
///
/// A negative double or digit string stored into an unsigned column wraps
/// to the 64-bit value before the range test, which only `BIGINT UNSIGNED`
/// then passes; a negative JSON integer is out of range there. Both are
/// what `MySQL` 8.4 does.
fn store_integer(value: &serde_json::Value, min: i64, max: u64) -> Result<Value, Refusal> {
    use serde_json::Value as Json;
    let unsigned_column = min == 0;
    let in_range = |wide: i128| {
        if wide < i128::from(min) || wide > i128::from(max) {
            return Err(Refusal::OutOfRange);
        }
        if unsigned_column {
            u64::try_from(wide).map(Value::UInt64)
        } else {
            i64::try_from(wide).map(Value::Int64)
        }
        .map_err(|_| Refusal::OutOfRange)
    };
    let wrapped = |signed: i64| {
        if unsigned_column {
            i128::from(signed.cast_unsigned())
        } else {
            i128::from(signed)
        }
    };
    match value {
        Json::Bool(flag) => in_range(i128::from(*flag)),
        Json::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                return in_range(i128::from(unsigned));
            }
            if let Some(signed) = number.as_i64() {
                return in_range(i128::from(signed));
            }
            let rounded = number
                .as_f64()
                .ok_or(Refusal::OutOfRange)?
                .round_ties_even();
            if rounded < 0.0 {
                in_range(wrapped(whole_signed(rounded).ok_or(Refusal::OutOfRange)?))
            } else if unsigned_column {
                in_range(i128::from(
                    whole_unsigned(rounded).ok_or(Refusal::OutOfRange)?,
                ))
            } else {
                in_range(i128::from(
                    whole_signed(rounded).ok_or(Refusal::OutOfRange)?,
                ))
            }
        }
        Json::String(text) => {
            let text = text.trim_start_matches(' ');
            let (negative, digits) = match text.as_bytes().first() {
                Some(b'-') => (true, &text[1..]),
                Some(b'+') => (false, &text[1..]),
                _ => (false, text),
            };
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Refusal::Invalid("INTEGER"));
            }
            if negative {
                let signed = format!("-{digits}")
                    .parse::<i64>()
                    .map_err(|_| Refusal::Invalid("INTEGER"))?;
                in_range(wrapped(signed))
            } else {
                let unsigned = digits
                    .parse::<u64>()
                    .map_err(|_| Refusal::Invalid("INTEGER"))?;
                in_range(i128::from(unsigned))
            }
        }
        Json::Null | Json::Array(_) | Json::Object(_) => Err(Refusal::NotScalar),
    }
}

/// 2^63, the first double past the signed 64-bit range.
const SIGNED_END: f64 = 9_223_372_036_854_775_808.0;
/// 2^64, the first double past the unsigned 64-bit range.
const UNSIGNED_END: f64 = 18_446_744_073_709_551_616.0;

/// A whole double as a signed 64-bit integer, when it is one.
#[allow(clippy::cast_possible_truncation)]
fn whole_signed(whole: f64) -> Option<i64> {
    (-SIGNED_END..SIGNED_END)
        .contains(&whole)
        .then_some(whole as i64)
}

/// A whole non-negative double as an unsigned 64-bit integer, when it is
/// one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_unsigned(whole: f64) -> Option<u64> {
    (0.0..UNSIGNED_END).contains(&whole).then_some(whole as u64)
}

/// A `DOUBLE` read from a JSON string: a decimal number with an optional
/// exponent after any leading spaces, and nothing after it. The empty
/// string is zero.
fn text_double(text: &str) -> Result<f64, Refusal> {
    let text = text.trim_start_matches(' ');
    if text.is_empty() {
        return Ok(0.0);
    }
    if !is_decimal_number(text) {
        return Err(Refusal::Invalid("DOUBLE"));
    }
    match text.parse::<f64>() {
        Ok(float) if float.is_finite() => Ok(float),
        _ => Err(Refusal::Invalid("DOUBLE")),
    }
}

/// Whether `text` is `[+-] digits [. digits] [e [+-] digits]` with a digit
/// somewhere in the mantissa.
fn is_decimal_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits = |at: &mut usize| {
        let start = *at;
        while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        *at - start
    };
    let mut mantissa = digits(&mut at);
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        mantissa += digits(&mut at);
    }
    if mantissa == 0 {
        return false;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        at += usize::from(matches!(bytes.get(at), Some(b'+' | b'-')));
        if digits(&mut at) == 0 {
            return false;
        }
    }
    at == bytes.len()
}

/// A `DECIMAL(precision, scale)` from number text: rounded half away from
/// zero to the scale, and out of range when the integer part needs more
/// digits than the type has. Works on the digits themselves, so the 65
/// digits a DECIMAL may have need no wide arithmetic.
fn store_decimal(text: &str, precision: u8, scale: u8) -> Result<String, Refusal> {
    let text = text.trim_matches(' ');
    if !is_decimal_number(text) {
        return Err(Refusal::Invalid("DECIMAL"));
    }
    let (negative, unsigned) = match text.as_bytes()[0] {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (
            &unsigned[..at],
            unsigned[at + 1..]
                .parse::<i64>()
                .map_err(|_| Refusal::OutOfRange)?,
        ),
        None => (unsigned, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = Vec::with_capacity(whole.len() + fraction.len());
    digits.extend_from_slice(whole.as_bytes());
    digits.extend_from_slice(fraction.as_bytes());
    // Where the decimal point stands among `digits`, counted from the
    // left; it may fall before the first digit or after the last.
    let point = i64::try_from(whole.len())
        .unwrap_or(i64::MAX)
        .saturating_add(exponent);
    let zero = digits.iter().all(|digit| *digit == b'0');
    let scale = usize::from(scale);
    let integer_room = usize::from(precision).saturating_sub(scale);
    if zero || point < -(i64::try_from(scale).unwrap_or(i64::MAX)) {
        // Zero, or too small to round up to one unit of the scale.
        return Ok(format_decimal(false, b"0", &[], scale));
    }
    if point > 100 {
        return Err(Refusal::OutOfRange);
    }
    // Lay the digits out as an integer part and a fraction.
    let (mut integer, mut fraction): (Vec<u8>, Vec<u8>) = if point <= 0 {
        let mut fraction = vec![b'0'; usize::try_from(-point).unwrap_or(0)];
        fraction.extend_from_slice(&digits);
        (vec![b'0'], fraction)
    } else {
        let point = usize::try_from(point).unwrap_or(usize::MAX);
        if point >= digits.len() {
            digits.resize(point, b'0');
            (digits, Vec::new())
        } else {
            let fraction = digits.split_off(point);
            (digits, fraction)
        }
    };
    let round_up = fraction.get(scale).is_some_and(|digit| *digit >= b'5');
    fraction.truncate(scale);
    if round_up {
        let mut carry = true;
        for digit in fraction.iter_mut().rev().chain(integer.iter_mut().rev()) {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            integer.insert(0, b'1');
        }
    }
    let significant = integer
        .iter()
        .position(|digit| *digit != b'0')
        .map_or(&integer[integer.len() - 1..], |first| &integer[first..]);
    let whole_zero = *significant == *b"0";
    if !whole_zero && significant.len() > integer_room {
        return Err(Refusal::OutOfRange);
    }
    let nonzero = !whole_zero || fraction.iter().any(|digit| *digit != b'0');
    Ok(format_decimal(
        negative && nonzero,
        significant,
        &fraction,
        scale,
    ))
}

fn format_decimal(negative: bool, integer: &[u8], fraction: &[u8], scale: usize) -> String {
    let mut text = String::with_capacity(integer.len() + scale + 2);
    if negative {
        text.push('-');
    }
    text.extend(integer.iter().map(|digit| char::from(*digit)));
    if scale > 0 {
        text.push('.');
        text.extend(fraction.iter().map(|digit| char::from(*digit)));
        for _ in fraction.len()..scale {
            text.push('0');
        }
    }
    text
}

/// A `DATE` or `DATETIME` column: the string or integer read as a cast
/// reads it, refused when that has no answer or the answer is not a day of
/// the calendar - a zero month or day, or the thirtieth of February.
fn store_calendar(value: &serde_json::Value, target: DataType) -> Result<Value, Refusal> {
    let (operand, operand_type) = match value {
        serde_json::Value::String(text) => (Value::Utf8(text.clone()), DataType::Utf8),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(signed) => (Value::Int64(signed), DataType::Int64),
            None => return Err(Refusal::OutOfRange),
        },
        _ => return Err(Refusal::OutOfRange),
    };
    let cast = evaluate_eager_scalar_inner(
        ScalarFunction::Cast(target),
        &[operand],
        &[Some(operand_type)],
        None,
        Some(target),
        Collation::default(),
    );
    match cast {
        Ok(Value::Utf8(text)) => {
            let part = |range: std::ops::Range<usize>| text.get(range)?.parse::<u32>().ok();
            let day =
                part(0..4)
                    .zip(part(5..7))
                    .zip(part(8..10))
                    .and_then(|((year, month), day)| {
                        chrono::NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)
                    });
            if day.is_some() {
                Ok(Value::Utf8(text))
            } else {
                Err(Refusal::OutOfRange)
            }
        }
        _ => Err(Refusal::OutOfRange),
    }
}

#[cfg(test)]
mod tests {
    use super::{Refusal, store_decimal, store_integer, text_double};
    use pintail_types::Value;
    use serde_json::json;

    fn decimal(text: &str, precision: u8, scale: u8) -> Option<String> {
        store_decimal(text, precision, scale).ok()
    }

    #[test]
    fn decimals_round_half_away_from_zero_and_refuse_overflow() {
        assert_eq!(decimal("2.345", 5, 2).as_deref(), Some("2.35"));
        assert_eq!(decimal("2.335", 5, 2).as_deref(), Some("2.34"));
        assert_eq!(decimal("-0.005", 5, 2).as_deref(), Some("-0.01"));
        assert_eq!(decimal("-0.004", 5, 2).as_deref(), Some("0.00"));
        assert_eq!(decimal("1e-10", 5, 2).as_deref(), Some("0.00"));
        assert_eq!(decimal("999.994", 5, 2).as_deref(), Some("999.99"));
        assert_eq!(decimal("999.995", 5, 2), None);
        assert_eq!(decimal("1000", 5, 2), None);
        assert_eq!(decimal("999.994", 10, 0).as_deref(), Some("1000"));
        assert_eq!(decimal(" 1.5 ", 5, 2).as_deref(), Some("1.50"));
        assert_eq!(decimal("-.5", 10, 0).as_deref(), Some("-1"));
        assert_eq!(decimal("1.", 5, 2).as_deref(), Some("1.00"));
        assert_eq!(decimal(".5e1", 5, 2).as_deref(), Some("5.00"));
        assert_eq!(decimal("1e2", 5, 2).as_deref(), Some("100.00"));
        assert_eq!(decimal("0.125", 5, 2).as_deref(), Some("0.13"));
        assert_eq!(decimal("12345678901234567890", 20, 3), None);
        assert_eq!(decimal("1.5x", 5, 2), None);
        assert_eq!(decimal("", 5, 2), None);
    }

    #[test]
    fn integers_follow_the_column_range_and_the_text_form() {
        let signed = |value: serde_json::Value| store_integer(&value, -128, 127).ok();
        assert_eq!(signed(json!(1.5)), Some(Value::Int64(2)));
        assert_eq!(signed(json!(2.5)), Some(Value::Int64(2)));
        assert_eq!(signed(json!(-1.5)), Some(Value::Int64(-2)));
        assert_eq!(signed(json!(300)), None);
        assert_eq!(signed(json!(true)), Some(Value::Int64(1)));
        assert_eq!(signed(json!(" 5")), Some(Value::Int64(5)));
        assert_eq!(signed(json!("5 ")), None);
        assert_eq!(signed(json!("5.0")), None);
        assert_eq!(signed(json!("-0")), Some(Value::Int64(0)));
        let wide = |value: serde_json::Value| store_integer(&value, 0, u64::MAX).ok();
        assert_eq!(wide(json!(-1)), None);
        assert_eq!(wide(json!(-1.5)), Some(Value::UInt64(u64::MAX - 1)));
        assert_eq!(wide(json!("-5")), Some(Value::UInt64(u64::MAX - 4)));
        assert_eq!(wide(json!(-0.5)), Some(Value::UInt64(0)));
        let narrow = |value: serde_json::Value| store_integer(&value, 0, 255).ok();
        assert_eq!(narrow(json!(-1.5)), None);
        assert_eq!(narrow(json!("-5")), None);
    }

    #[test]
    fn doubles_read_a_whole_number_text_or_nothing() {
        assert!(matches!(text_double(""), Ok(float) if float.abs() < f64::EPSILON));
        assert!(matches!(text_double(" 1.5"), Ok(float) if (float - 1.5).abs() < f64::EPSILON));
        assert!(matches!(text_double("1.5 "), Err(Refusal::Invalid(_))));
        assert!(matches!(text_double("inf"), Err(Refusal::Invalid(_))));
        assert!(matches!(text_double("1e400"), Err(Refusal::Invalid(_))));
    }
}
