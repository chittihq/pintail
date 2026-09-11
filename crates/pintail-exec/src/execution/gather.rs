//! Output columns gathered from rows of kept columns.
//!
//! An operator that picks rows - a sort placing them in order, a join
//! pairing them - builds each output column from the columns it read. Where
//! every source packs the column alike, the packed units are copied; any
//! other column is gathered as the values the operator would have built
//! rows from, so the output is the same either way.

use pintail_types::DataType;

use super::ExecError;
use crate::array::{StrColumn, ValidityMask};
use crate::batch::{ColumnVector, DecimalUnits, LazyText, TypedValues};

/// Plain text: no ENUM or SET labels, which order and compare by their
/// declarations.
pub(crate) fn plain_text(column: &ColumnVector) -> Option<(&StrColumn, &ValidityMask)> {
    if column.data_type() != DataType::Utf8 {
        return None;
    }
    match column.typed()? {
        (TypedValues::Utf8(text), validity)
            if text.declared_enum_labels().is_none() && text.declared_set_members().is_none() =>
        {
            Some((text, validity))
        }
        _ => None,
    }
}

/// Whether `column` is packed in a form [`gather`] copies units from, with
/// text, where it has any, derived from those units.
pub(crate) fn packed(column: &ColumnVector) -> bool {
    packing(column).is_some()
}

/// The column of `data_type` holding, in order, row `row` of
/// `sources[source]` for each `(source, row)` of `picks`.
pub(crate) fn gather(
    sources: &[&ColumnVector],
    picks: &[(u32, u32)],
    data_type: DataType,
) -> Result<ColumnVector, ExecError> {
    gather_with(sources, picks.len(), |index| Some(picks[index]), data_type)
}

/// [`gather`] where a `None` pick is a NULL: the column a join pairs its
/// unmatched rows with.
pub(crate) fn gather_optional(
    sources: &[&ColumnVector],
    picks: &[Option<(u32, u32)>],
    data_type: DataType,
) -> Result<ColumnVector, ExecError> {
    gather_with(sources, picks.len(), |index| picks[index], data_type)
}

fn gather_with(
    sources: &[&ColumnVector],
    len: usize,
    pick: impl Fn(usize) -> Option<(u32, u32)>,
    data_type: DataType,
) -> Result<ColumnVector, ExecError> {
    if let Some(packed) = gather_packed(sources, len, &pick, data_type) {
        return Ok(packed);
    }
    let values =
        (0..len)
            .map(|index| match pick(index) {
                None => Ok(pintail_types::Value::Null),
                Some((source, row)) => sources[source as usize].value_owned(row as usize).ok_or(
                    ExecError::InvalidBatch("a picked row is outside its column"),
                ),
            })
            .collect::<Result<Vec<_>, _>>()?;
    Ok(ColumnVector::new(data_type, values)?)
}

/// The packed form every source shares for one column.
enum Packing {
    Signed,
    Unsigned,
    Decimal(u8),
    Temporal,
    Text,
}

fn packing(column: &ColumnVector) -> Option<Packing> {
    let (typed, _) = column.typed()?;
    Some(match typed {
        TypedValues::Int64(_) => Packing::Signed,
        TypedValues::UInt64(_) => Packing::Unsigned,
        TypedValues::Decimal128 { scale, text, .. } if text.derived() => Packing::Decimal(*scale),
        TypedValues::Temporal { text, .. } if text.derived() => Packing::Temporal,
        TypedValues::Utf8(_) if plain_text(column).is_some() => Packing::Text,
        _ => return None,
    })
}

fn gather_packed(
    sources: &[&ColumnVector],
    len: usize,
    pick: &impl Fn(usize) -> Option<(u32, u32)>,
    data_type: DataType,
) -> Option<ColumnVector> {
    let first = packing(sources.first()?)?;
    let alike = sources.iter().all(|column| {
        column.data_type() == data_type
            && match (packing(column), &first) {
                (Some(Packing::Decimal(scale)), Packing::Decimal(first)) => scale == *first,
                (Some(packing), first) => {
                    std::mem::discriminant(&packing) == std::mem::discriminant(first)
                }
                (None, _) => false,
            }
    });
    if !alike {
        return None;
    }
    let typed = |source: u32| sources[source as usize].typed().expect("packed column");
    // Each pick's packed column and row; `None` for a NULL pick.
    let at = |index: usize| pick(index).map(|(source, row)| (typed(source).0, row as usize));
    let valid = (0..len)
        .map(|index| {
            pick(index).is_some_and(|(source, row)| typed(source).1.is_valid(row as usize))
        })
        .collect::<Vec<_>>();
    let units = match first {
        Packing::Signed => TypedValues::Int64(
            (0..len)
                .map(|index| match at(index) {
                    Some((TypedValues::Int64(values), row)) => values[row],
                    _ => 0,
                })
                .collect(),
        ),
        Packing::Unsigned => TypedValues::UInt64(
            (0..len)
                .map(|index| match at(index) {
                    Some((TypedValues::UInt64(values), row)) => values[row],
                    _ => 0,
                })
                .collect(),
        ),
        Packing::Decimal(scale) => TypedValues::Decimal128 {
            values: DecimalUnits::Wide(
                (0..len)
                    .map(|index| match at(index) {
                        Some((TypedValues::Decimal128 { values, .. }, row)) => {
                            values.get(row).unwrap_or(0)
                        }
                        _ => 0,
                    })
                    .collect(),
            ),
            scale,
            text: LazyText::decimal(scale),
        },
        Packing::Temporal => TypedValues::Temporal {
            units: (0..len)
                .map(|index| match at(index) {
                    Some((TypedValues::Temporal { units, .. }, row)) => units[row],
                    _ => 0,
                })
                .collect(),
            text: match data_type {
                DataType::DateTime64 { fsp } => LazyText::datetime(fsp),
                _ => LazyText::date(),
            },
        },
        Packing::Text => {
            let mut text = StrColumn::default();
            for index in 0..len {
                match at(index) {
                    Some((TypedValues::Utf8(column), row)) => {
                        column.views()[row].with_bytes(column.heap(), |bytes| text.push(bytes));
                    }
                    _ => text.push(&[]),
                }
            }
            TypedValues::Utf8(text)
        }
    };
    Some(ColumnVector::from_typed(
        data_type,
        units,
        ValidityMask::from_bools(&valid),
    ))
}
