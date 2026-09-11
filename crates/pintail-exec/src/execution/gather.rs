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
pub(super) fn plain_text(column: &ColumnVector) -> Option<(&StrColumn, &ValidityMask)> {
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
    if let Some(packed) = gather_packed(sources, picks, data_type) {
        return Ok(packed);
    }
    let values = picks
        .iter()
        .map(|&(source, row)| {
            sources[source as usize]
                .value_owned(row as usize)
                .ok_or(ExecError::InvalidBatch(
                    "a picked row is outside its column",
                ))
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
    picks: &[(u32, u32)],
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
    let valid = picks
        .iter()
        .map(|&(source, row)| typed(source).1.is_valid(row as usize))
        .collect::<Vec<_>>();
    let packed = match first {
        Packing::Signed => TypedValues::Int64(
            picks
                .iter()
                .map(|&(source, row)| match typed(source).0 {
                    TypedValues::Int64(values) => values[row as usize],
                    _ => 0,
                })
                .collect(),
        ),
        Packing::Unsigned => TypedValues::UInt64(
            picks
                .iter()
                .map(|&(source, row)| match typed(source).0 {
                    TypedValues::UInt64(values) => values[row as usize],
                    _ => 0,
                })
                .collect(),
        ),
        Packing::Decimal(scale) => TypedValues::Decimal128 {
            values: DecimalUnits::Wide(
                picks
                    .iter()
                    .map(|&(source, row)| match typed(source).0 {
                        TypedValues::Decimal128 { values, .. } => {
                            values.get(row as usize).unwrap_or(0)
                        }
                        _ => 0,
                    })
                    .collect(),
            ),
            scale,
            text: LazyText::decimal(scale),
        },
        Packing::Temporal => TypedValues::Temporal {
            units: picks
                .iter()
                .map(|&(source, row)| match typed(source).0 {
                    TypedValues::Temporal { units, .. } => units[row as usize],
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
            for &(source, row) in picks {
                match typed(source).0 {
                    TypedValues::Utf8(column) => column.views()[row as usize]
                        .with_bytes(column.heap(), |bytes| text.push(bytes)),
                    _ => text.push(&[]),
                }
            }
            TypedValues::Utf8(text)
        }
    };
    Some(ColumnVector::from_typed(
        data_type,
        packed,
        ValidityMask::from_bools(&valid),
    ))
}
