use mysql_async::{Conn, prelude::Queryable as _};
use pintail_probe::{SourceColumn, SourceTable};
use pintail_types::{KeyPart, StoredRow, Value};

use crate::{PollError, decoder::quote_identifier};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceChunk {
    pub(crate) chunk_id: String,
    pub(crate) offset: usize,
    pub(crate) source_count: u64,
    pub(crate) source_checksum: String,
}

pub(crate) async fn source_chunks(
    connection: &mut Conn,
    database: &str,
    table: &SourceTable,
    chunk_rows: usize,
) -> Result<Vec<SourceChunk>, PollError> {
    let mut chunks = Vec::new();
    let mut offset = 0_usize;
    loop {
        let row_expression = source_row_expression(table);
        let order = table
            .key
            .columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(*), \
                    COALESCE(BIT_XOR(__pintail_crc),0), \
                    CAST(COALESCE(SUM(__pintail_crc),0) AS UNSIGNED) \
             FROM (\
               SELECT CAST(CRC32({row_expression}) AS UNSIGNED) AS __pintail_crc \
               FROM {}.{} ORDER BY {order} LIMIT {chunk_rows} OFFSET {offset}\
             ) AS __pintail_chunk",
            quote_identifier(database),
            quote_identifier(&table.name),
        );
        let (source_count, xor, sum): (u64, u64, u64) = connection
            .query_first(sql)
            .await?
            .ok_or_else(|| PollError::Decode("source checksum returned no row".to_owned()))?;
        chunks.push(SourceChunk {
            chunk_id: chunks.len().to_string(),
            offset,
            source_count,
            source_checksum: format!("{xor:08x}:{sum:016x}"),
        });
        if source_count
            < u64::try_from(chunk_rows).map_err(|error| PollError::Decode(error.to_string()))?
        {
            break;
        }
        offset = offset
            .checked_add(chunk_rows)
            .ok_or_else(|| PollError::Decode("checksum offset exceeds usize".to_owned()))?;
    }
    Ok(chunks)
}

pub(crate) fn replica_checksum(rows: &[StoredRow]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash_bytes(
        &mut hash,
        &u64::try_from(rows.len()).unwrap_or(u64::MAX).to_le_bytes(),
    );
    for row in rows {
        hash_key(&mut hash, row.key().parts());
        for value in row.values() {
            hash_value(&mut hash, value);
        }
    }
    format!("{hash:016x}")
}

fn source_row_expression(table: &SourceTable) -> String {
    let parts = table
        .columns
        .iter()
        .map(source_value_expression)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "''".to_owned()
    } else {
        format!("CONCAT({})", parts.join(","))
    }
}

fn source_value_expression(column: &SourceColumn) -> String {
    let identifier = quote_identifier(&column.name);
    let value = if is_geometry(&column.mysql_data_type) {
        format!("ST_AsWKB({identifier})")
    } else if is_binary(&column.mysql_data_type) {
        identifier.clone()
    } else {
        format!("CONVERT({identifier} USING utf8mb4)")
    };
    format!(
        "IF({identifier} IS NULL,'N',CONCAT('V',LPAD(OCTET_LENGTH({value}),16,'0'),':',HEX({value})))"
    )
}

fn hash_key(hash: &mut u64, parts: &[KeyPart]) {
    hash_bytes(
        hash,
        &u64::try_from(parts.len()).unwrap_or(u64::MAX).to_le_bytes(),
    );
    for part in parts {
        match part {
            KeyPart::Int64(value) => {
                hash_bytes(hash, b"i");
                hash_bytes(hash, &value.to_le_bytes());
            }
            KeyPart::UInt64(value) => {
                hash_bytes(hash, b"u");
                hash_bytes(hash, &value.to_le_bytes());
            }
            KeyPart::Utf8(value) => {
                hash_bytes(hash, b"s");
                hash_sized(hash, value.as_bytes());
            }
            KeyPart::Binary(value) => {
                hash_bytes(hash, b"b");
                hash_sized(hash, value);
            }
        }
    }
}

fn hash_value(hash: &mut u64, value: &Value) {
    match value {
        Value::Null => hash_bytes(hash, b"n"),
        Value::Boolean(value) => hash_bytes(hash, &[b't', u8::from(*value)]),
        Value::Int64(value) => {
            hash_bytes(hash, b"i");
            hash_bytes(hash, &value.to_le_bytes());
        }
        Value::UInt64(value) => {
            hash_bytes(hash, b"u");
            hash_bytes(hash, &value.to_le_bytes());
        }
        Value::Float64(value) => {
            hash_bytes(hash, b"f");
            hash_bytes(hash, &value.to_bits().to_le_bytes());
        }
        Value::Utf8(value) | Value::Enum { label: value, .. } => {
            hash_bytes(hash, b"s");
            hash_sized(hash, value.as_bytes());
        }
        Value::Binary(value) => {
            hash_bytes(hash, b"b");
            hash_sized(hash, value);
        }
    }
}

fn hash_sized(hash: &mut u64, bytes: &[u8]) {
    hash_bytes(
        hash,
        &u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes(),
    );
    hash_bytes(hash, bytes);
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn is_binary(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit"
    )
}

fn is_geometry(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "geometry"
            | "point"
            | "linestring"
            | "polygon"
            | "multipoint"
            | "multilinestring"
            | "multipolygon"
            | "geometrycollection"
    )
}

#[cfg(test)]
mod tests {
    use pintail_types::{KeyPart, PrimaryKey, StoredRow, Value};

    use super::replica_checksum;

    #[test]
    fn replica_checksum_is_order_and_value_sensitive() {
        let row = |key: u64, value: &str| {
            StoredRow::new(
                PrimaryKey::new(vec![KeyPart::UInt64(key)]).unwrap(),
                vec![Value::Utf8(value.to_owned())],
                1,
                false,
            )
        };
        assert_eq!(
            replica_checksum(&[row(1, "a"), row(2, "b")]),
            replica_checksum(&[row(1, "a"), row(2, "b")])
        );
        assert_ne!(
            replica_checksum(&[row(1, "a"), row(2, "b")]),
            replica_checksum(&[row(2, "b"), row(1, "a")])
        );
        assert_ne!(
            replica_checksum(&[row(1, "a")]),
            replica_checksum(&[row(1, "changed")])
        );
    }
}
