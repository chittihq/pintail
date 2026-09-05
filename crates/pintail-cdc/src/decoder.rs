use chrono::{Datelike as _, Timelike as _, Utc};
use mysql_async::{
    Value as MysqlValue,
    binlog::{events::OptionalMetaExtractor, row::BinlogRow, value::BinlogValue},
    consts::ColumnType,
};
use pintail_probe::{SourceColumn, SourceTable};
use pintail_snapshot::map_mysql_value;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, Value};

use crate::CdcError;

/// How a row image's columns line up with the schema it is decoded against.
///
/// The two agree on width for every row a healthy stream sees, and `Positional`
/// is that case: column *i* of the image is column *i* of the schema, which is
/// the only reading available when `binlog_row_metadata` is MINIMAL.
///
/// They stop agreeing whenever the stream is behind a schema change - an ALTER
/// that landed while the stream was disconnected, or several that landed faster
/// than it caught up. The row images written before the change are historical
/// and narrower, and no amount of re-probing makes them wider. Under FULL
/// metadata the table map names its own columns, so those rows can still be
/// placed exactly: `ByName` carries, for each image position, the schema column
/// it holds. Columns the image predates are absent rather than guessed.
pub(crate) enum RowAlignment {
    Positional,
    ByName {
        /// Schema column index per image position; `None` where the image
        /// carries a column the schema no longer has.
        image_to_schema: Vec<Option<usize>>,
    },
}

/// What an image column with no column of that name in the schema means.
///
/// It is genuinely ambiguous, and only a re-probe separates the two readings:
/// either the schema is stale and has not learned the column yet, or the
/// column really was dropped and this image predates the drop. So the question
/// is asked twice. `Reject` is the first ask, against the schema in hand,
/// where an unplaceable column is the drift signal that triggers the probe.
/// `Ignore` is the second, against a schema just refreshed from the source: if
/// the column is still unknown there, it is gone from the table and the value
/// this image carries for it has nowhere to go.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownColumn {
    Reject,
    Ignore,
}

/// The binlog column type a probed declaration is written as.
///
/// Deliberately partial. Anything this is not certain of returns `None`, which
/// abandons type matching for the whole row rather than risking a confident
/// wrong answer - and an unrecognised declaration matches nothing anywhere, so
/// a gap here costs a resync, never a corrupted row.
fn binlog_column_type(column: &SourceColumn) -> Option<ColumnType> {
    // `get_column_type` resolves ENUM and SET from their metadata rather than
    // reporting the STRING they are stored as, so these compare directly.
    Some(match column.mysql_data_type.to_ascii_lowercase().as_str() {
        "tinyint" => ColumnType::MYSQL_TYPE_TINY,
        "smallint" => ColumnType::MYSQL_TYPE_SHORT,
        "mediumint" => ColumnType::MYSQL_TYPE_INT24,
        "int" | "integer" => ColumnType::MYSQL_TYPE_LONG,
        "bigint" => ColumnType::MYSQL_TYPE_LONGLONG,
        "float" => ColumnType::MYSQL_TYPE_FLOAT,
        "double" | "real" => ColumnType::MYSQL_TYPE_DOUBLE,
        "decimal" | "numeric" => ColumnType::MYSQL_TYPE_NEWDECIMAL,
        "year" => ColumnType::MYSQL_TYPE_YEAR,
        "date" => ColumnType::MYSQL_TYPE_DATE,
        "time" => ColumnType::MYSQL_TYPE_TIME2,
        "datetime" => ColumnType::MYSQL_TYPE_DATETIME2,
        "timestamp" => ColumnType::MYSQL_TYPE_TIMESTAMP2,
        "char" | "binary" => ColumnType::MYSQL_TYPE_STRING,
        "varchar" | "varbinary" => ColumnType::MYSQL_TYPE_VARCHAR,
        "tinytext" | "tinyblob" | "text" | "blob" | "mediumtext" | "mediumblob" | "longtext"
        | "longblob" => ColumnType::MYSQL_TYPE_BLOB,
        "enum" => ColumnType::MYSQL_TYPE_ENUM,
        "set" => ColumnType::MYSQL_TYPE_SET,
        "json" => ColumnType::MYSQL_TYPE_JSON,
        "bit" => ColumnType::MYSQL_TYPE_BIT,
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" => ColumnType::MYSQL_TYPE_GEOMETRY,
        _ => return None,
    })
}

/// Places a nameless row image on `schema` by matching column types, when
/// exactly one placement is possible.
///
/// MINIMAL metadata names no columns, but it still records each column's type,
/// and a row image is always an order-preserving subsequence of the schema it
/// predates: `ADD COLUMN` inserts, it never reorders what is already there. So
/// the question is how many ways the image's type sequence embeds in the
/// schema's. Exactly one means the placement is determined and the row can be
/// decoded; more than one means the drift is genuinely ambiguous - three
/// consecutive `INT` columns added mid-table look the same from any of three
/// positions - and the caller resyncs rather than picking.
///
/// Counting embeddings directly avoids enumerating them: `ways[i][j]` is the
/// number of ways the image's first `i` columns embed in the schema's first
/// `j`, saturating at two because "more than one" is all the caller needs.
fn embed_by_type(image: &[ColumnType], schema: &[ColumnType]) -> Option<Vec<usize>> {
    if image.len() > schema.len() {
        return None;
    }
    let mut ways = vec![vec![0_u8; schema.len() + 1]; image.len() + 1];
    for slot in &mut ways[0] {
        *slot = 1;
    }
    for taken in 1..=image.len() {
        for offered in 1..=schema.len() {
            let skipped = ways[taken][offered - 1];
            let matched = if image[taken - 1] == schema[offered - 1] {
                ways[taken - 1][offered - 1]
            } else {
                0
            };
            ways[taken][offered] = skipped.saturating_add(matched).min(2);
        }
    }
    if ways[image.len()][schema.len()] != 1 {
        return None;
    }
    // One embedding exists, so walking back from the end is deterministic:
    // at each step exactly one of "skip this schema column" and "match it"
    // leads anywhere.
    let mut placement = vec![0_usize; image.len()];
    let mut offered = schema.len();
    for taken in (1..=image.len()).rev() {
        while ways[taken][offered - 1] == ways[taken][offered] {
            offered -= 1;
        }
        placement[taken - 1] = offered - 1;
        offered -= 1;
    }
    Some(placement)
}

/// Placement for a row image exactly as wide as the table's physical columns
/// when the schema also keeps `VIRTUAL GENERATED` ones.
///
/// `MySQL` writes those columns into its row images, so a healthy stream is
/// positional and never reaches this. A server that leaves them out produces
/// an image one column narrower per virtual column; reading it positionally
/// over the physical columns alone is the only exact placement, and the
/// virtual columns decode as absent (NULL) rather than misaligning every
/// column after the first one.
fn physical_placement(table: &SourceTable, image_columns: usize) -> Option<Vec<Option<usize>>> {
    let physical = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !column.virtual_generated())
        .map(|(index, _)| Some(index))
        .collect::<Vec<_>>();
    (physical.len() != table.columns.len() && physical.len() == image_columns).then_some(physical)
}

impl RowAlignment {
    /// Works out how `table_map`'s row images map onto `table`.
    ///
    /// An error here means the row cannot be placed against this schema, which
    /// is the signal to re-probe and try again against a fresher one.
    pub(crate) fn resolve(
        table: &SourceTable,
        table_map: &mysql_async::binlog::events::TableMapEvent<'_>,
        unknown: UnknownColumn,
    ) -> Result<Self, CdcError> {
        let image_columns = usize::try_from(table_map.columns_count()).unwrap_or(usize::MAX);
        if image_columns == table.columns.len() {
            return Ok(Self::Positional);
        }
        if let Some(image_to_schema) = physical_placement(table, image_columns) {
            return Ok(Self::ByName { image_to_schema });
        }
        let metadata = OptionalMetaExtractor::new(table_map.iter_optional_meta())
            .map_err(|error| CdcError::Decode(format!("{}: {error}", table.name)))?;
        let names = metadata
            .iter_column_name()
            .map(|name| name.map(|name| name.name().into_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CdcError::Decode(format!("{}: {error}", table.name)))?;
        let image_to_schema = if names.len() == image_columns {
            names
                .iter()
                .map(|name| {
                    table
                        .columns
                        .iter()
                        .position(|column| column.name.eq_ignore_ascii_case(name))
                })
                .collect::<Vec<_>>()
        } else {
            // MINIMAL metadata: no names, so fall back to the types, which it
            // does record.
            let image_types = (0..image_columns)
                .map(|index| table_map.get_column_type(index).ok().flatten())
                .collect::<Option<Vec<_>>>();
            let schema_types = table
                .columns
                .iter()
                .map(binlog_column_type)
                .collect::<Option<Vec<_>>>();
            let placement = image_types
                .zip(schema_types)
                .and_then(|(image, schema)| embed_by_type(&image, &schema))
                .ok_or_else(|| {
                    CdcError::Decode(format!(
                        "{} row image contains {image_columns} columns against a {}-column \
                         schema, and binlog_row_metadata=MINIMAL names no columns to place them \
                         by; their types do not identify one placement either",
                        table.name,
                        table.columns.len()
                    ))
                })?;
            placement.into_iter().map(Some).collect::<Vec<_>>()
        };
        if unknown == UnknownColumn::Reject
            && let Some(orphan) = names
                .iter()
                .zip(&image_to_schema)
                .find_map(|(name, target)| target.is_none().then_some(name))
        {
            return Err(CdcError::Decode(format!(
                "{}.{orphan} is in the row image but not in the tracked schema",
                table.name
            )));
        }
        // A column the image predates takes the value MySQL gave it when the
        // ALTER ran, and NULL is the only one this can reconstruct without the
        // declared default. Refusing the others keeps the row honest: the
        // caller quarantines for resync rather than storing a wrong value.
        for (index, column) in table.columns.iter().enumerate() {
            if image_to_schema.contains(&Some(index)) {
                continue;
            }
            if !column.nullable {
                return Err(CdcError::Decode(format!(
                    "{}.{} is absent from a {image_columns}-column row image and is NOT NULL",
                    table.name, column.name
                )));
            }
            if table
                .key
                .columns
                .iter()
                .any(|key| key.eq_ignore_ascii_case(&column.name))
            {
                return Err(CdcError::Decode(format!(
                    "{}.{} is a key column absent from a {image_columns}-column row image",
                    table.name, column.name
                )));
            }
        }
        Ok(Self::ByName { image_to_schema })
    }
}

pub(crate) fn decode_row(
    table: &SourceTable,
    row: BinlogRow,
    alignment: &RowAlignment,
) -> Result<Vec<Value>, CdcError> {
    let image_to_schema = match alignment {
        RowAlignment::Positional => {
            if row.len() != table.columns.len() {
                return Err(CdcError::Decode(format!(
                    "{} row image contains {} columns; FULL metadata/image requires {}",
                    table.name,
                    row.len(),
                    table.columns.len()
                )));
            }
            return row
                .unwrap()
                .into_iter()
                .zip(&table.columns)
                .map(|(value, column)| decode_value(&table.name, column, value))
                .collect();
        }
        RowAlignment::ByName { image_to_schema } => image_to_schema,
    };
    if row.len() != image_to_schema.len() {
        return Err(CdcError::Decode(format!(
            "{} row image contains {} columns; its table map declared {}",
            table.name,
            row.len(),
            image_to_schema.len()
        )));
    }
    let mut values = vec![Value::Null; table.columns.len()];
    for (value, target) in row.unwrap().into_iter().zip(image_to_schema) {
        let Some(index) = *target else {
            continue;
        };
        values[index] = decode_value(&table.name, &table.columns[index], value)?;
    }
    Ok(values)
}

pub(crate) fn physical_key(table: &SourceTable, values: &[Value]) -> Result<PrimaryKey, CdcError> {
    if table.key.mode == KeyMode::AppendRowId {
        return Err(CdcError::Decode(format!(
            "{} has no stable source key",
            table.name
        )));
    }
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
                    CdcError::Decode(format!(
                        "{} key column {key} is absent from its probed schema",
                        table.name
                    ))
                })?;
            key_part(&values[index]).ok_or_else(|| {
                CdcError::Decode(format!("{}.{} key value is NULL", table.name, key))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    PrimaryKey::new(parts).map_err(CdcError::Schema)
}

pub(crate) fn insert_key(
    table: &SourceTable,
    values: &[Value],
    version: u64,
) -> Result<PrimaryKey, CdcError> {
    if table.key.mode == KeyMode::AppendRowId {
        return PrimaryKey::new(vec![KeyPart::UInt64(version)]).map_err(CdcError::Schema);
    }
    physical_key(table, values)
}

fn decode_value(
    table: &str,
    column: &SourceColumn,
    value: BinlogValue<'static>,
) -> Result<Value, CdcError> {
    let value = MysqlValue::try_from(value)
        .map_err(|error| CdcError::Decode(format!("{table}.{}: {error}", column.name)))?;
    let value = adapt_binlog_value(column, value)?;
    map_mysql_value(table, column, value).map_err(|error| CdcError::Decode(error.to_string()))
}

#[allow(clippy::too_many_lines)]
fn adapt_binlog_value(column: &SourceColumn, value: MysqlValue) -> Result<MysqlValue, CdcError> {
    if value == MysqlValue::NULL {
        return Ok(value);
    }
    let mysql_type = column.mysql_data_type.to_ascii_lowercase();
    // binlog_row_metadata=MINIMAL omits the SIGNEDNESS field, so the binlog
    // decoder yields every unsigned integer column as signed — negative once
    // the value crosses the signed midpoint. The probed declaration is the
    // authority: reinterpret the two's-complement bits at the column's width.
    // Under FULL metadata unsigned columns arrive as UInt and this never fires.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    if let MysqlValue::Int(signed) = value
        && column
            .mysql_column_type
            .to_ascii_lowercase()
            .contains("unsigned")
    {
        let reinterpreted = match mysql_type.as_str() {
            "tinyint" => Some(u64::from(signed as u8)),
            "smallint" => Some(u64::from(signed as u16)),
            "mediumint" => Some(u64::from(signed as u32 & 0x00FF_FFFF)),
            "int" | "integer" => Some(u64::from(signed as u32)),
            "bigint" => Some(signed as u64),
            _ => None,
        };
        if let Some(reinterpreted) = reinterpreted {
            return Ok(MysqlValue::UInt(reinterpreted));
        }
    }
    if mysql_type == "enum" {
        let index = numeric_index(&value).ok_or_else(|| {
            CdcError::Decode(format!("{}.{} ENUM index is invalid", "<row>", column.name))
        })?;
        let labels = declaration_labels(&column.mysql_column_type, "enum")?;
        let label = if index == 0 {
            ""
        } else {
            labels.get(index - 1).ok_or_else(|| {
                CdcError::Decode(format!(
                    "{} ENUM index {index} exceeds {} labels",
                    column.name,
                    labels.len()
                ))
            })?
        };
        return Ok(MysqlValue::Bytes(label.as_bytes().to_vec()));
    }
    if mysql_type == "set" {
        let bits = set_bits(&value).ok_or_else(|| {
            CdcError::Decode(format!("{}.{} SET bits are invalid", "<row>", column.name))
        })?;
        let labels = declaration_labels(&column.mysql_column_type, "set")?;
        if labels.len() > 64 {
            return Err(CdcError::Decode(format!(
                "{} SET declares more than 64 labels",
                column.name
            )));
        }
        let mut selected = Vec::new();
        for (index, label) in labels.iter().enumerate() {
            if bits & (1_u64 << index) != 0 {
                selected.push(label.as_str());
            }
        }
        if labels.len() < 64 && bits >> labels.len() != 0 {
            return Err(CdcError::Decode(format!(
                "{} SET bits exceed {} declared labels",
                column.name,
                labels.len()
            )));
        }
        return Ok(MysqlValue::Bytes(selected.join(",").into_bytes()));
    }
    if mysql_type == "timestamp"
        && let MysqlValue::Bytes(bytes) = &value
        && let Ok(raw) = std::str::from_utf8(bytes)
        && let Ok(seconds) = raw.split('.').next().unwrap_or(raw).parse::<i64>()
    {
        if seconds == 0 {
            return Ok(MysqlValue::Date(0, 0, 0, 0, 0, 0, 0));
        }
        let timestamp = chrono::DateTime::<Utc>::from_timestamp(seconds, 0).ok_or_else(|| {
            CdcError::Decode(format!("{} TIMESTAMP is out of range", column.name))
        })?;
        let micros = raw
            .split_once('.')
            .map_or("", |(_, fraction)| fraction)
            .chars()
            .take(6)
            .collect::<String>();
        let micros = if micros.is_empty() {
            0
        } else {
            format!("{micros:0<6}")
                .parse::<u32>()
                .map_err(|error| CdcError::Decode(error.to_string()))?
        };
        return Ok(MysqlValue::Date(
            u16::try_from(timestamp.year()).map_err(|error| CdcError::Decode(error.to_string()))?,
            u8::try_from(timestamp.month()).map_err(|error| CdcError::Decode(error.to_string()))?,
            u8::try_from(timestamp.day()).map_err(|error| CdcError::Decode(error.to_string()))?,
            u8::try_from(timestamp.hour()).map_err(|error| CdcError::Decode(error.to_string()))?,
            u8::try_from(timestamp.minute())
                .map_err(|error| CdcError::Decode(error.to_string()))?,
            u8::try_from(timestamp.second())
                .map_err(|error| CdcError::Decode(error.to_string()))?,
            micros,
        ));
    }
    if is_textual(column)
        && let MysqlValue::Bytes(bytes) = value
    {
        return Ok(MysqlValue::Bytes(
            transcode_text(column, &bytes)?.into_bytes(),
        ));
    }
    Ok(value)
}

fn numeric_index(value: &MysqlValue) -> Option<usize> {
    match value {
        MysqlValue::Int(value) => usize::try_from(*value).ok(),
        MysqlValue::UInt(value) => usize::try_from(*value).ok(),
        MysqlValue::Bytes(value) => std::str::from_utf8(value).ok()?.parse().ok(),
        _ => None,
    }
}

fn set_bits(value: &MysqlValue) -> Option<u64> {
    match value {
        MysqlValue::Int(value) => u64::try_from(*value).ok(),
        MysqlValue::UInt(value) => Some(*value),
        MysqlValue::Bytes(bytes) if bytes.len() <= 8 => {
            Some(bytes.iter().enumerate().fold(0_u64, |bits, (index, byte)| {
                bits | (u64::from(*byte) << (index * 8))
            }))
        }
        _ => None,
    }
}

fn declaration_labels(column_type: &str, kind: &str) -> Result<Vec<String>, CdcError> {
    pintail_types::declaration_labels(column_type, kind)
        .ok_or_else(|| CdcError::Decode(format!("cannot parse {kind} declaration {column_type}")))
}

fn is_textual(column: &SourceColumn) -> bool {
    matches!(
        column.mysql_data_type.to_ascii_lowercase().as_str(),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set"
    )
}

fn transcode_text(column: &SourceColumn, bytes: &[u8]) -> Result<String, CdcError> {
    match column.character_set.as_deref().map(str::to_ascii_lowercase) {
        Some(character_set)
            if character_set == "utf8mb4"
                || character_set == "utf8"
                || character_set == "utf8mb3"
                || character_set == "ascii" =>
        {
            String::from_utf8(bytes.to_vec())
                .map_err(|error| CdcError::Decode(format!("{}.{}: {error}", "<row>", column.name)))
        }
        Some(character_set) if character_set == "latin1" => {
            Ok(bytes.iter().map(|byte| cp1252_character(*byte)).collect())
        }
        None => String::from_utf8(bytes.to_vec())
            .map_err(|error| CdcError::Decode(format!("{}.{}: {error}", "<row>", column.name))),
        Some(character_set) => Err(CdcError::Decode(format!(
            "{} uses unsupported binlog character set {character_set}",
            column.name
        ))),
    }
}

fn cp1252_character(byte: u8) -> char {
    const EXTENDED: [char; 32] = [
        '€', '\u{0081}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{008d}', 'Ž',
        '\u{008f}', '\u{0090}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{009d}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9f => EXTENDED[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
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

#[cfg(test)]
mod tests {
    use super::{adapt_binlog_value, declaration_labels, embed_by_type, set_bits, transcode_text};
    use mysql_async::Value as MysqlValue;
    use mysql_async::consts::ColumnType;
    use pintail_probe::SourceColumn;
    use pintail_types::DataType;

    #[test]
    fn an_image_without_the_virtual_columns_reads_the_physical_ones_in_order() {
        let mut virtual_column = column("varchar", "varchar(64)");
        virtual_column.name = "contact_clean".to_owned();
        virtual_column.extra = "VIRTUAL GENERATED".to_owned();
        let mut id = column("bigint", "bigint");
        id.name = "id".to_owned();
        let mut contact = column("varchar", "varchar(64)");
        contact.name = "contact".to_owned();
        let table = pintail_probe::SourceTable {
            name: "contacts".to_owned(),
            engine: None,
            estimated_rows: None,
            rows_are_exact: false,
            columns: vec![id, virtual_column, contact],
            key: pintail_probe::SourceKey {
                mode: pintail_types::KeyMode::Primary,
                index_name: None,
                columns: vec!["id".to_owned()],
            },
            unique_keys: Vec::new(),
            requires_reconciliation: false,
            foreign_keys: Vec::new(),
            secondary_indexes: Vec::new(),
            warnings: Vec::new(),
        };
        // MySQL logs the virtual column: a full-width image is positional.
        assert_eq!(super::physical_placement(&table, 3), None);
        // A server that omits it: the two physical columns, in order.
        assert_eq!(
            super::physical_placement(&table, 2),
            Some(vec![Some(0), Some(2)])
        );
        // Anything else is a real drift and goes to the name/type placement.
        assert_eq!(super::physical_placement(&table, 4), None);
    }

    fn column(data_type: &str, column_type: &str) -> SourceColumn {
        SourceColumn {
            id: 1,
            name: "value".to_owned(),
            mysql_data_type: data_type.to_owned(),
            mysql_column_type: column_type.to_owned(),
            pintail_type: DataType::Utf8,
            nullable: true,
            character_set: Some("utf8mb4".to_owned()),
            collation: Some("utf8mb4_0900_ai_ci".to_owned()),
            generated_stored: false,
            generation_expression: String::new(),
            extra: String::new(),
            auto_increment: false,
            default_value: None,
            default_generated: false,
        }
    }

    #[test]
    fn maps_enum_indexes_and_set_masks_to_labels() {
        let enum_column = column("enum", "enum('alpha','βeta','it\\'s')");
        assert_eq!(
            adapt_binlog_value(&enum_column, MysqlValue::Int(2)).expect("enum"),
            MysqlValue::Bytes("βeta".as_bytes().to_vec())
        );
        assert_eq!(
            declaration_labels(&enum_column.mysql_column_type, "enum").expect("labels"),
            ["alpha", "βeta", "it's"]
        );
        let set_column = column("set", "set('red','green','blue')");
        assert_eq!(
            adapt_binlog_value(&set_column, MysqlValue::Bytes(vec![0b101])).expect("set"),
            MysqlValue::Bytes(b"red,blue".to_vec())
        );
        assert_eq!(set_bits(&MysqlValue::Bytes(vec![1, 1])), Some(257));
    }

    #[test]
    fn reinterprets_minimal_metadata_unsigned_integers() {
        // Under binlog_row_metadata=MINIMAL every unsigned column decodes as
        // signed; the probed declaration recovers the true value bit-exactly.
        let cases: [(&str, &str, i64, u64); 6] = [
            ("tinyint", "tinyint unsigned", -56, 200),
            ("smallint", "smallint unsigned", -1, 65_535),
            ("mediumint", "mediumint unsigned", -1, 16_777_215),
            ("int", "int unsigned", -1_294_967_296, 3_000_000_000),
            ("bigint", "bigint unsigned", -1, u64::MAX),
            (
                "bigint",
                "bigint unsigned",
                i64::MIN,
                9_223_372_036_854_775_808,
            ),
        ];
        for (data_type, column_type, signed, expected) in cases {
            let unsigned_column = column(data_type, column_type);
            assert_eq!(
                adapt_binlog_value(&unsigned_column, MysqlValue::Int(signed))
                    .expect("unsigned reinterpretation"),
                MysqlValue::UInt(expected),
                "{column_type} {signed}"
            );
        }
        // In-range positives normalize to UInt without changing value.
        let int_column = column("int", "int unsigned");
        assert_eq!(
            adapt_binlog_value(&int_column, MysqlValue::Int(42)).expect("in-range"),
            MysqlValue::UInt(42)
        );
        // Signed columns and FULL-metadata UInt values pass through untouched.
        let signed_column = column("int", "int");
        assert_eq!(
            adapt_binlog_value(&signed_column, MysqlValue::Int(-56)).expect("signed"),
            MysqlValue::Int(-56)
        );
        assert_eq!(
            adapt_binlog_value(&int_column, MysqlValue::UInt(3_000_000_000)).expect("full"),
            MysqlValue::UInt(3_000_000_000)
        );
    }

    #[test]
    fn transcodes_mysql_latin1_as_cp1252() {
        let mut latin = column("varchar", "varchar(32)");
        latin.character_set = Some("latin1".to_owned());
        assert_eq!(
            transcode_text(&latin, &[b'c', b'a', b'f', 0xe9]).expect("latin1"),
            "café"
        );
        assert_eq!(
            transcode_text(&latin, &[0x80]).expect("cp1252 extension"),
            "€"
        );
    }

    /// Exercises the placement `embed_by_type` exists for: a MINIMAL row image
    /// written before an `ADD COLUMN`, against the schema that followed it.
    ///
    /// These assert the property that matters - a placement is returned only
    /// when it is the ONLY one - because the failure mode here is not an error
    /// but a confidently wrong answer, which writes real values into the wrong
    /// columns and passes every later check.
    mod type_placement {
        use super::{ColumnType, embed_by_type};

        const INT: ColumnType = ColumnType::MYSQL_TYPE_LONG;
        const TEXT: ColumnType = ColumnType::MYSQL_TYPE_VARCHAR;
        const DATE: ColumnType = ColumnType::MYSQL_TYPE_DATE;
        const BIG: ColumnType = ColumnType::MYSQL_TYPE_LONGLONG;

        #[test]
        fn identical_sequences_place_one_to_one() {
            assert_eq!(
                embed_by_type(&[INT, TEXT, DATE], &[INT, TEXT, DATE]),
                Some(vec![0, 1, 2])
            );
        }

        #[test]
        fn a_column_appended_after_the_image_was_written() {
            // The production shape: ALTER ... ADD COLUMN, then the older rows.
            assert_eq!(
                embed_by_type(&[INT, TEXT], &[INT, TEXT, DATE]),
                Some(vec![0, 1])
            );
        }

        #[test]
        fn a_column_inserted_in_the_middle_is_found_by_type() {
            // `ADD COLUMN ... AFTER` is exactly the case positional decoding
            // gets wrong, and the one type matching earns its keep on.
            assert_eq!(
                embed_by_type(&[INT, DATE], &[INT, TEXT, DATE]),
                Some(vec![0, 2])
            );
        }

        #[test]
        fn several_columns_added_at_once_still_place() {
            assert_eq!(
                embed_by_type(&[INT, BIG], &[INT, TEXT, DATE, BIG]),
                Some(vec![0, 3])
            );
        }

        #[test]
        fn a_repeated_type_around_the_gap_is_refused() {
            // Two INTs against three: the missing one could be any of the
            // three, so there is no determined answer and none is invented.
            assert_eq!(embed_by_type(&[INT, INT], &[INT, INT, INT]), None);
        }

        #[test]
        fn an_ambiguous_insertion_point_is_refused() {
            // The TEXT could be either of the schema's two.
            assert_eq!(embed_by_type(&[INT, TEXT], &[INT, TEXT, TEXT]), None);
        }

        #[test]
        fn an_image_that_does_not_embed_at_all_is_refused() {
            assert_eq!(embed_by_type(&[INT, BIG], &[INT, TEXT, DATE]), None);
        }

        #[test]
        fn an_image_wider_than_the_schema_is_refused() {
            // A drop the schema has already adopted; the extra value has no
            // home and the caller decides what that means.
            assert_eq!(embed_by_type(&[INT, TEXT, DATE], &[INT, TEXT]), None);
        }

        #[test]
        fn placements_are_strictly_increasing() {
            // Order preservation is the assumption the whole method rests on:
            // ADD COLUMN inserts, it never reorders what is already there.
            let placement = embed_by_type(&[INT, TEXT, BIG], &[INT, DATE, TEXT, DATE, BIG])
                .expect("this image embeds exactly once");
            assert!(placement.windows(2).all(|pair| pair[0] < pair[1]));
            assert_eq!(placement, vec![0, 2, 4]);
        }
    }
}
