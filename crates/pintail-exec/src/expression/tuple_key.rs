//! The key one row of a `COUNT(DISTINCT a, b, ...)` counts by.
//!
//! The binder spells the tuple as text every set already knows how to
//! hold: a JSON array of the components, NULL when any component is. That
//! is collision-free, and it builds, serializes and collates a JSON
//! document for every row.
//!
//! The key here says the same thing from the components' own types. An
//! integer is its digits; a DECIMAL, DATE or DATETIME column is its packed
//! units, the stand-in a single-column distinct set already uses for the
//! canonical text; text is quoted. Components are separated by a comma, so
//! two rows spell the same key exactly when every component is the same
//! value. The key is still text, so the set collates, merges and spills it
//! as it does any other, and a row read from a packed column and a row
//! read as a value spell the same key.

use std::fmt::Write as _;

use pintail_sql::{BinaryOp, BoundExpr, BoundExprKind, ScalarFunction};
use pintail_types::{DataType, Value};

use super::CompiledExpr;
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{ColumnVector, RecordBatch, TypedValues};
use crate::collation::Collation;
use crate::execution::distinct_keys::UnitKind;

/// The collation a set of tuple keys folds them under. It pads nothing and
/// folds a text component's case and accents; every other character a key
/// is spelled with - digits, the sign, the separators, the weights of a
/// component weighed by rules of its own - it tells apart.
pub(crate) const KEY_COLLATION: Collation = Collation::Utf8mb40900AiCi;

/// The most one component that is not text adds to a key: the digits of a
/// 128-bit integer with its sign and marks, or a calendar value written
/// some other way and quoted.
pub(crate) const SEGMENT_BYTES: usize = 64;

/// One component of a [`CompiledExpr::TupleKey`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TupleComponent {
    pub(crate) expr: CompiledExpr,
    /// The component's declared type, which a column evaluated for a whole
    /// batch is asked for.
    pub(crate) data_type: Option<DataType>,
    /// The packed units the component keys by, when it is a DECIMAL, DATE
    /// or DATETIME column.
    pub(crate) units: Option<UnitKind>,
    /// The collation a text component is weighed under before it is
    /// spelled: its own, when that is not the one the set folds its keys
    /// by. A `general_ci` or binary column pads and folds by its own rules
    /// whatever the rest of the tuple holds.
    pub(crate) weigh: Option<Collation>,
}

/// The `IS NULL` operands of a chain of `OR`s, in order; `false` for any
/// other condition.
fn null_tests<'a>(condition: &'a BoundExpr, operands: &mut Vec<&'a BoundExpr>) -> bool {
    match &condition.kind {
        BoundExprKind::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } => null_tests(left, operands) && null_tests(right, operands),
        BoundExprKind::IsNull {
            expr,
            negated: false,
        } => {
            operands.push(expr);
            true
        }
        _ => false,
    }
}

/// Whether a component of this shape spells a key that agrees with the
/// tuple's text form on which rows are the same.
///
/// Integers and text do, whatever computes them. A DECIMAL or a calendar
/// value does as a column, whose every value is written at the column's
/// own scale; a computed one may spell one value two ways, and stays with
/// the form that reads it.
fn keyed(component: &BoundExpr) -> bool {
    match component.data_type {
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
            | DataType::Utf8,
        ) => true,
        Some(data_type) => UnitKind::of_type(data_type).is_some() && stored(component),
        None => false,
    }
}

/// Whether `component` is a column, or a column as an intermediate result
/// holds it: read in the session's zone, or through the copy check. Either
/// writes each stored value one way, as the column itself does.
fn stored(component: &BoundExpr) -> bool {
    match &component.kind {
        BoundExprKind::Column(_) => true,
        BoundExprKind::Scalar {
            function: ScalarFunction::SessionTimestamp,
            args,
        } => args
            .first()
            .is_some_and(|source| matches!(source.kind, BoundExprKind::Column(_))),
        BoundExprKind::Scalar {
            function: ScalarFunction::Cast(target),
            args,
        } => matches!(
            args.as_slice(),
            [source, BoundExpr {
                kind: BoundExprKind::Literal(Value::UInt64(policy)),
                ..
            }] if policy & pintail_sql::CALENDAR_COPY_CHECK != 0
                && source.data_type == Some(*target)
                && stored(source)
        ),
        _ => false,
    }
}

/// The components of the tuple `expr` spells for a multi-argument
/// `COUNT(DISTINCT)`: the text of a JSON array of them, NULL when any is
/// NULL. `None` for any other expression, and for a tuple with a component
/// this key has no spelling for.
pub(crate) fn components(expr: &BoundExpr) -> Option<Vec<&BoundExpr>> {
    let BoundExprKind::Scalar {
        function: ScalarFunction::Cast(DataType::Utf8),
        args,
    } = &expr.kind
    else {
        return None;
    };
    let [choice] = args.as_slice() else {
        return None;
    };
    let BoundExprKind::Scalar {
        function: ScalarFunction::If,
        args,
    } = &choice.kind
    else {
        return None;
    };
    let [condition, null, tuple] = args.as_slice() else {
        return None;
    };
    if !matches!(null.kind, BoundExprKind::Literal(Value::Null)) {
        return None;
    }
    let BoundExprKind::Scalar {
        function: ScalarFunction::JsonArray,
        args: members,
    } = &tuple.kind
    else {
        return None;
    };
    let mut operands = Vec::with_capacity(members.len());
    if !null_tests(condition, &mut operands)
        || operands.len() != members.len()
        || operands.len() < 2
        || !operands
            .iter()
            .zip(members)
            .all(|(operand, member)| *operand == member && keyed(member))
    {
        return None;
    }
    Some(operands)
}

/// Appends an integer component.
#[inline]
pub(crate) fn write_int(key: &mut String, value: i128) {
    write!(key, "{value}").expect("a string takes every write");
}

/// Appends a component held as packed units, already divided to the key
/// its [`UnitKind`] gives them.
#[inline]
pub(crate) fn write_units(key: &mut String, units: i128) {
    key.push('#');
    write_int(key, units);
}

fn write_quoted(key: &mut String, text: &str) {
    key.push('"');
    for character in text.chars() {
        if matches!(character, '"' | '\\') {
            key.push('\\');
        }
        key.push(character);
    }
    key.push('"');
}

/// Appends a text component: quoted as it stands for the set to fold, or,
/// when the component folds by rules of its own, as the weights those
/// rules give it.
fn write_text(key: &mut String, text: &str, weigh: Option<Collation>) {
    match weigh {
        None => write_quoted(key, text),
        Some(collation) => {
            key.push('=');
            key.push_str(&crate::execution::normalized_collation_text(text, collation));
        }
    }
}

/// The units canonical `text` stands for, `None` for any other spelling:
/// what [`UnitKind::key_of_text`] answers, without writing the units back
/// out to compare. A calendar parser here reads digits at fixed places and
/// refuses a date no calendar holds, so text it reads is the text its units
/// print as exactly when it has the length they print at.
fn units_of_text(kind: UnitKind, text: &str) -> Option<i128> {
    match kind {
        UnitKind::Date => pintail_types::parse_date_units(text).map(i128::from),
        UnitKind::DateTime { fsp } => {
            let canonical = if fsp == 0 { 19 } else { 20 + usize::from(fsp) };
            (text.len() == canonical)
                .then(|| pintail_types::parse_datetime_units(text))
                .flatten()
                .map(|micros| kind.key_of_units(i128::from(micros)))
        }
        UnitKind::Decimal { .. } => kind.key_of_text(text),
    }
}

/// Appends a component that arrived as text: a calendar or decimal value's
/// canonical text as its units, anything else as the text it is.
fn write_read_text(
    key: &mut String,
    text: &str,
    units: Option<UnitKind>,
    weigh: Option<Collation>,
) {
    match units.and_then(|kind| units_of_text(kind, text)) {
        Some(units) => write_units(key, units),
        None => write_text(key, text, weigh),
    }
}

/// Appends a component read as a value, which is never NULL. A column's
/// canonical text is read back to the units a packed batch holds for it,
/// so both spell one key; text written any other way is a different value
/// to a set that compares text, and is quoted as it stands.
pub(crate) fn write_value(
    key: &mut String,
    value: &Value,
    units: Option<UnitKind>,
    weigh: Option<Collation>,
) {
    match value {
        Value::Int64(value) => write_int(key, i128::from(*value)),
        Value::UInt64(value) => write_int(key, i128::from(*value)),
        Value::Boolean(value) => key.push(if *value { 'T' } else { 'F' }),
        Value::Utf8(text) => write_read_text(key, text, units, weigh),
        Value::Enum { label, .. } => write_text(key, label, weigh),
        other => {
            key.push('~');
            write_quoted(key, &format!("{other:?}"));
        }
    }
}

impl CompiledExpr {
    /// The tuple key over `components`, compiled against `columns`, for a
    /// set that folds its keys under [`KEY_COLLATION`]. `collation` is the
    /// plan's, which a text component with none of its own compares by.
    pub(crate) fn tuple_key(
        components: &[&BoundExpr],
        columns: &[pintail_sql::BoundColumn],
        collation: Collation,
    ) -> Result<Self, crate::ExecError> {
        components
            .iter()
            .map(|component| {
                let own = component
                    .text_collation()
                    .and_then(Collation::from_mysql_name)
                    .unwrap_or(collation);
                Ok(TupleComponent {
                    expr: Self::compile(component, columns, collation)?,
                    data_type: component.data_type,
                    units: component.data_type.and_then(UnitKind::of_type),
                    weigh: (component.data_type == Some(DataType::Utf8) && own != KEY_COLLATION)
                        .then_some(own),
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self::TupleKey)
    }
}

/// One row's key, or NULL when any component is.
pub(super) fn evaluate(
    components: &[TupleComponent],
    batch: &RecordBatch,
    row: usize,
) -> Result<Value, crate::ExecError> {
    let mut key = String::new();
    for (index, component) in components.iter().enumerate() {
        let value = component.expr.evaluate(batch, row)?;
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }
        if index > 0 {
            key.push(',');
        }
        write_value(&mut key, &value, component.units, component.weigh);
    }
    Ok(Value::Utf8(key))
}

/// How one component is read for a batch.
enum Reader<'a> {
    /// A plain integer column: each row's integer.
    Ints(&'a TypedValues, &'a ValidityMask),
    /// A column packed as the units the component keys by.
    Units(&'a TypedValues, &'a ValidityMask, UnitKind),
    /// A column held as text that is no ENUM or SET: each row's bytes.
    Text(&'a StrColumn, &'a ValidityMask, &'a ColumnVector),
    /// Any other column: each row as a value.
    Values(&'a ColumnVector),
}

impl<'a> Reader<'a> {
    fn of(component: &TupleComponent, column: &'a ColumnVector) -> Self {
        let Some((typed, validity)) = column.typed() else {
            return Self::Values(column);
        };
        // Only a plain integer column reads as its integers: a YEAR or a
        // BIT stored as one keeps the value's own reading of it.
        let plain_integer = matches!(
            column.data_type(),
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
        );
        match (typed, component.units) {
            (TypedValues::Int64(_) | TypedValues::UInt64(_), _) if plain_integer => {
                Self::Ints(typed, validity)
            }
            (_, Some(kind)) if typed.unit_kind() == Some(kind) => {
                Self::Units(typed, validity, kind)
            }
            (TypedValues::Utf8(text), _)
                if text.declared_enum_labels().is_none()
                    && text.declared_set_members().is_none() =>
            {
                Self::Text(text, validity, column)
            }
            _ => Self::Values(column),
        }
    }

    /// Appends row `row`'s component: `Some(false)` when it is NULL, and
    /// `None` for a row outside the column.
    #[inline]
    fn spell(&self, key: &mut String, row: usize, component: &TupleComponent) -> Option<bool> {
        match self {
            Self::Ints(typed, validity) => {
                if !validity.is_valid(row) {
                    return Some(false);
                }
                write_int(key, typed.int_key_at(row)?);
            }
            Self::Units(typed, validity, kind) => {
                if !validity.is_valid(row) {
                    return Some(false);
                }
                write_units(key, kind.key_of_units(typed.units_at(row)?));
            }
            Self::Text(text, validity, column) => {
                if !validity.is_valid(row) {
                    return Some(false);
                }
                let read = text.views().get(row)?.with_bytes(text.heap(), |bytes| {
                    std::str::from_utf8(bytes).ok().map(|text| {
                        write_read_text(key, text, component.units, component.weigh);
                    })
                });
                // Bytes that are not text are the value's own to read.
                if read.is_none() {
                    let value = column.value_owned(row)?;
                    if matches!(value, Value::Null) {
                        return Some(false);
                    }
                    write_value(key, &value, component.units, component.weigh);
                }
            }
            Self::Values(column) => {
                let value = column.value_owned(row)?;
                if matches!(value, Value::Null) {
                    return Some(false);
                }
                write_value(key, &value, component.units, component.weigh);
            }
        }
        Some(true)
    }
}

/// The keys of `batch`'s selected rows as one text column: each component
/// is read once for the batch - a column as it is packed, an expression as
/// the column `read` evaluates it to - and every row spells its key from
/// them into one buffer, with no value built for a packed component. The
/// keys are the ones [`evaluate`] spells. `None` when a component has no
/// column for the batch.
pub(super) fn column(
    batch: &RecordBatch,
    components: &[TupleComponent],
    mut read: impl FnMut(&CompiledExpr, Option<DataType>) -> Option<ColumnVector>,
) -> Option<ColumnVector> {
    let columns = components
        .iter()
        .map(|component| match component.expr.column_index() {
            Some(column) => batch.column(column).cloned(),
            None => read(&component.expr, component.data_type),
        })
        .collect::<Option<Vec<_>>>()?;
    let readers = components
        .iter()
        .zip(&columns)
        .map(|(component, column)| Reader::of(component, column))
        .collect::<Vec<_>>();
    let rows = batch.row_count();
    let selection = batch.selection();
    let mut keys = StrColumn::default();
    let mut valid = vec![false; rows];
    let mut key = String::new();
    let mut spelled = 0_u64;
    for (row, valid) in valid.iter_mut().enumerate() {
        key.clear();
        if selection.is_selected(row) {
            spelled += 1;
            *valid = true;
            for (index, (component, reader)) in components.iter().zip(&readers).enumerate() {
                if index > 0 {
                    key.push(',');
                }
                if !reader.spell(&mut key, row, component)? {
                    *valid = false;
                    key.clear();
                    break;
                }
            }
        }
        keys.push(key.as_bytes());
    }
    crate::counters::count(|counters| {
        counters.tuple_keys_spelled = counters.tuple_keys_spelled.saturating_add(spelled);
    });
    Some(ColumnVector::from_typed(
        DataType::Utf8,
        TypedValues::Utf8(keys),
        ValidityMask::from_bools(&valid),
    ))
}

#[cfg(test)]
mod tests {
    use super::{UnitKind, Value, write_units, write_value};

    fn spelled(value: &Value, units: Option<UnitKind>) -> String {
        let mut key = String::new();
        write_value(&mut key, value, units, None);
        key
    }

    /// A value read as canonical text and the same value read as packed
    /// units spell one key; text written another way spells its own.
    #[test]
    fn canonical_text_spells_its_units() {
        let kind = UnitKind::DateTime { fsp: 0 };
        let micros = pintail_types::parse_datetime_units("2024-03-05 06:07:08").expect("datetime");
        let mut packed = String::new();
        write_units(&mut packed, kind.key_of_units(i128::from(micros)));
        assert_eq!(
            spelled(&Value::Utf8("2024-03-05 06:07:08".to_owned()), Some(kind)),
            packed
        );
        assert_eq!(
            spelled(&Value::Utf8("2024-3-5 06:07:08".to_owned()), Some(kind)),
            "\"2024-3-5 06:07:08\""
        );
        // Whatever the general reading calls canonical, this one does, and
        // nothing else: every length, the zero date, and dates no calendar
        // holds.
        for fsp in [0_u8, 3, 6] {
            let kind = UnitKind::DateTime { fsp };
            for text in [
                "2024-03-05 06:07:08",
                "2024-03-05 06:07:08.5",
                "2024-03-05 06:07:08.500",
                "2024-03-05 06:07:08.500000",
                "2024-03-05 06:07:08.123456",
                "0000-00-00 00:00:00",
                "0000-00-00 00:00:00.000",
                "0000-00-00 00:00:00.000000",
                "0000-01-01 00:00:00",
                "2024-02-30 00:00:00",
                "2024-03-05 6:07:08",
                "2024-03-05",
                "",
            ] {
                assert_eq!(
                    super::units_of_text(kind, text),
                    kind.key_of_text(text),
                    "{text:?} at {fsp}"
                );
            }
        }
        for text in ["2024-03-05", "0000-00-00", "2024-02-30", "2024-3-5", ""] {
            assert_eq!(
                super::units_of_text(UnitKind::Date, text),
                UnitKind::Date.key_of_text(text),
                "{text:?}"
            );
        }
        let scale = UnitKind::Decimal { scale: 2 };
        assert_eq!(spelled(&Value::Utf8("-1.50".to_owned()), Some(scale)), "#-150");
        assert_eq!(spelled(&Value::Utf8("-1.5".to_owned()), Some(scale)), "\"-1.5\"");
    }

    /// No two different values of one component spell the same key, and a
    /// quote inside text cannot end it.
    #[test]
    fn spellings_do_not_collide() {
        let all = [
            spelled(&Value::Int64(1), None),
            spelled(&Value::Int64(-1), None),
            spelled(&Value::UInt64(11), None),
            spelled(&Value::Boolean(true), None),
            spelled(&Value::Boolean(false), None),
            spelled(&Value::Utf8("1".to_owned()), None),
            spelled(&Value::Utf8("#1".to_owned()), None),
            spelled(&Value::Utf8("a\",\"b".to_owned()), None),
            spelled(&Value::Utf8("a\\".to_owned()), None),
            spelled(&Value::Utf8(String::new()), None),
            spelled(&Value::Utf8("2024-01-01".to_owned()), Some(UnitKind::Date)),
        ];
        for (index, left) in all.iter().enumerate() {
            for right in &all[index + 1..] {
                assert_ne!(left, right);
            }
        }
        assert_eq!(all[7], "\"a\\\",\\\"b\"");
    }
}
