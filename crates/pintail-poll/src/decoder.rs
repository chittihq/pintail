use mysql_async::Row;
use pintail_probe::SourceTable;
use pintail_snapshot::map_mysql_value;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, StoredRow, Value};

use crate::PollError;

pub(crate) fn decode_row(
    table: &SourceTable,
    row: Row,
    append_row_id: u64,
    version: u64,
    deleted: bool,
) -> Result<StoredRow, PollError> {
    if row.len() != table.columns.len() {
        return Err(PollError::Decode(format!(
            "{} source row contains {} values; expected {}",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    let values = row
        .unwrap()
        .into_iter()
        .zip(&table.columns)
        .map(|(value, column)| map_mysql_value(&table.name, column, value))
        .collect::<Result<Vec<_>, _>>()?;
    let key = if table.key.mode == KeyMode::AppendRowId {
        PrimaryKey::new(vec![KeyPart::UInt64(append_row_id)])?
    } else {
        physical_key(table, &values)?
    };
    Ok(StoredRow::new(key, values, version, deleted))
}

pub(crate) fn physical_key(table: &SourceTable, values: &[Value]) -> Result<PrimaryKey, PollError> {
    let parts = table
        .key
        .columns
        .iter()
        .map(|key| {
            let index = table
                .columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(key))
                .ok_or_else(|| {
                    PollError::Decode(format!(
                        "{} key column {key} is absent from its source schema",
                        table.name
                    ))
                })?;
            key_part(&values[index]).ok_or_else(|| {
                PollError::Decode(format!("{}.{} key value is NULL", table.name, key))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    PrimaryKey::new(parts).map_err(PollError::Schema)
}

pub(crate) fn source_projection(table: &SourceTable) -> String {
    // Geometry is fetched RAW, exactly like the snapshot path: the shared
    // map_mysql_value strips the 4-byte SRID from MySQL's internal format
    // to reach canonical WKB. Fetching ST_AsWKB here handed that mapper an
    // already-canonical value and the second strip corrupted every
    // reconciled geometry (#263: sakila.address lost its WKB header).
    table
        .columns
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn key_projection(table: &SourceTable) -> String {
    // Raw fetch for the same reason as source_projection: map_mysql_value
    // owns the SRID strip.
    table
        .key
        .columns
        .iter()
        .map(|key| quote_identifier(key))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn decode_key(table: &SourceTable, row: Row) -> Result<PrimaryKey, PollError> {
    if row.len() != table.key.columns.len() {
        return Err(PollError::Decode(format!(
            "{} source key contains {} values; expected {}",
            table.name,
            row.len(),
            table.key.columns.len()
        )));
    }
    let parts = row
        .unwrap()
        .into_iter()
        .zip(&table.key.columns)
        .map(|(value, key)| {
            let column = table
                .columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(key))
                .ok_or_else(|| {
                    PollError::Decode(format!(
                        "{} key column {key} is absent from its source schema",
                        table.name
                    ))
                })?;
            let value = map_mysql_value(&table.name, column, value)?;
            key_part(&value).ok_or_else(|| {
                PollError::Decode(format!("{}.{} key value is NULL", table.name, key))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    PrimaryKey::new(parts).map_err(PollError::Schema)
}

pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

fn key_part(value: &Value) -> Option<KeyPart> {
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(KeyPart::UInt64(u64::from(*value))),
        Value::Int64(value) => Some(KeyPart::Int64(*value)),
        Value::UInt64(value) => Some(KeyPart::UInt64(*value)),
        Value::Float64(value) => {
            let normalized = if value.get() == 0.0 { 0.0 } else { value.get() };
            Some(KeyPart::Utf8(normalized.to_string()))
        }
        Value::Utf8(value) | Value::Enum { label: value, .. } => Some(KeyPart::Utf8(value.clone())),
        Value::Binary(value) => Some(KeyPart::Binary(value.clone())),
    }
}
