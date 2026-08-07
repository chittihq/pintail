use chrono::{Datelike as _, Timelike as _, Utc};
use mysql_async::{
    Value as MysqlValue,
    binlog::{row::BinlogRow, value::BinlogValue},
};
use pintail_probe::{SourceColumn, SourceTable};
use pintail_snapshot::map_mysql_value;
use pintail_types::{KeyMode, KeyPart, PrimaryKey, Value};

use crate::CdcError;

pub(crate) fn decode_row(table: &SourceTable, row: BinlogRow) -> Result<Vec<Value>, CdcError> {
    if row.len() != table.columns.len() {
        return Err(CdcError::Decode(format!(
            "{} row image contains {} columns; FULL metadata/image requires {}",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    row.unwrap()
        .into_iter()
        .zip(&table.columns)
        .map(|(value, column)| decode_value(&table.name, column, value))
        .collect()
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
    let declaration = column_type.trim();
    let prefix = format!("{kind}(");
    if !declaration.to_ascii_lowercase().starts_with(&prefix) || !declaration.ends_with(')') {
        return Err(CdcError::Decode(format!(
            "cannot parse {kind} declaration {column_type}"
        )));
    }
    let body = &declaration[prefix.len()..declaration.len() - 1];
    let mut labels = Vec::new();
    let mut characters = body.chars().peekable();
    while characters.peek().is_some() {
        if characters.next() != Some('\'') {
            return Err(CdcError::Decode(format!(
                "cannot parse {kind} declaration {column_type}"
            )));
        }
        let mut label = String::new();
        loop {
            match characters.next() {
                Some('\\') => label.push(characters.next().ok_or_else(|| {
                    CdcError::Decode(format!("unterminated escape in {column_type}"))
                })?),
                Some('\'') if characters.peek() == Some(&'\'') => {
                    characters.next();
                    label.push('\'');
                }
                Some('\'') => break,
                Some(character) => label.push(character),
                None => {
                    return Err(CdcError::Decode(format!(
                        "unterminated label in {column_type}"
                    )));
                }
            }
        }
        labels.push(label);
        match characters.next() {
            Some(',') => {}
            None => break,
            _ => {
                return Err(CdcError::Decode(format!(
                    "cannot parse {kind} declaration {column_type}"
                )));
            }
        }
    }
    Ok(labels)
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
        Value::Utf8(value) => Some(KeyPart::Utf8(value.clone())),
        Value::Binary(value) => Some(KeyPart::Binary(value.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::{adapt_binlog_value, declaration_labels, set_bits, transcode_text};
    use mysql_async::Value as MysqlValue;
    use pintail_probe::SourceColumn;
    use pintail_types::DataType;

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
}
