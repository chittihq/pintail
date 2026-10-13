//! A `MODIFY` or `CHANGE` whose new declaration permits a rewrite of stored
//! values, decided from the rows the mirror holds.
//!
//! The source rewrites what the narrower declaration cannot hold and sends
//! no row event for any of it, so the declaration alone has to be read as
//! if it rewrote something, and the table recopied. But the stream stores
//! every change before a schema statement before it applies the statement:
//! at that moment the mirror's rows are the source's rows as the statement
//! found them. If none of them is a NULL under the new `NOT NULL`, outside
//! the narrower integer, longer than the shorter string or a member the
//! `ENUM` dropped, the statement rewrote nothing, and the table is adopted
//! in place.
//!
//! That holds only for the declaration the statement itself wrote. A probe
//! shows the source as it is now, which may be several statements later,
//! and the rows written in between were never under this statement's eyes.
//! So the statement's own text is read, the probe has to still show exactly
//! what it declared, and a statement that narrowed a column the source has
//! since changed again recopies the table.

use pintail_probe::{SourceColumn, SourceTable, StoredValueProof};
use pintail_store::{StoreError, TableStore};
use pintail_types::Value;

use crate::ddl::{self, ModifiedColumn};

/// Memory one streamed chunk of the stored values may hold.
const PROOF_SCAN_MEMORY: usize = 64 * 1024 * 1024;

/// The catalogue's data type and column type for a declared type, where the
/// declaration alone decides them.
fn catalogue_terms(declared_type: &str) -> Option<(String, String)> {
    if let Some(terms) = ddl::declared_terms(declared_type) {
        return Some((terms.data_type, terms.column_type));
    }
    let lower = declared_type.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        pintail_types::declaration_labels(declared_type.trim(), "enum")?;
        return Some(("enum".to_owned(), declared_type.trim().to_owned()));
    }
    if matches!(
        lower.as_str(),
        "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
    ) {
        return Some((lower.clone(), lower));
    }
    let (word, rest) = lower.split_once('(')?;
    let length = rest.strip_suffix(')')?.trim().parse::<u32>().ok()?;
    matches!(word, "varchar" | "char" | "varbinary" | "binary")
        .then(|| (word.to_owned(), format!("{word}({length})")))
}

/// The tracked column as `declared` redeclares it, or `None` for a type
/// whose catalogue entry the statement alone does not decide.
fn redeclared_column(
    tracked: &SourceColumn,
    declared: &ModifiedColumn,
    key: bool,
) -> Option<SourceColumn> {
    let (data_type, column_type) = catalogue_terms(&declared.declared_type)?;
    Some(SourceColumn {
        mysql_data_type: data_type,
        mysql_column_type: column_type,
        // A key column is NOT NULL whatever its redeclaration leaves out.
        nullable: !(declared.not_null || (key && !tracked.nullable)),
        ..tracked.clone()
    })
}

/// Whether two declarations of a column are the same one: the type as the
/// catalogue spells it, apart from an integer's display width, and the
/// nullability.
fn same_declaration(left: &SourceColumn, right: &SourceColumn) -> bool {
    let data_type = left.mysql_data_type.to_ascii_lowercase();
    if data_type != right.mysql_data_type.to_ascii_lowercase() || left.nullable != right.nullable {
        return false;
    }
    let column_type = |column: &SourceColumn| column.mysql_column_type.to_ascii_lowercase();
    match data_type.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" => {
            column_type(left).contains("unsigned") == column_type(right).contains("unsigned")
        }
        "enum" => {
            let labels = |column: &SourceColumn| {
                pintail_types::declaration_labels(&column.mysql_column_type, "enum")
            };
            labels(left).is_some() && labels(left) == labels(right)
        }
        _ => column_type(left) == column_type(right),
    }
}

fn named<'a>(table: &'a SourceTable, name: &str) -> Option<&'a SourceColumn> {
    table
        .columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
}

/// Decides a redeclaration of tracked columns.
///
/// `probed` is the source's table as it is now and `proofs` the columns of
/// it whose declaration permits a rewrite of values stored under `tracked`.
/// `declared` is what the statement's own text redeclares, when that can be
/// read. `rows_as_of_statement` is whether the store holds the table as the
/// statement found it, which a table being copied does not.
///
/// `Ok` adopts the probed table in place. `Err` is why it is recopied.
pub(crate) fn decide(
    tracked: &SourceTable,
    store: &TableStore,
    probed: &SourceTable,
    proofs: &[StoredValueProof],
    declared: Option<&[ModifiedColumn]>,
    rows_as_of_statement: bool,
) -> Result<(), String> {
    let is_key = |name: &str| {
        tracked
            .key
            .columns
            .iter()
            .any(|key| key.eq_ignore_ascii_case(name))
    };
    let redeclared = |name: &str| {
        let declared = declared?
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))?;
        redeclared_column(named(tracked, name)?, declared, is_key(name))
    };
    let still_shown = |name: &str, redeclared: &SourceColumn| {
        named(probed, name).is_some_and(|probed| same_declaration(redeclared, probed))
    };
    // A statement that narrowed a column the source no longer shows that
    // way: the probe compares equal or wider, and the rows this statement
    // rewrote would be kept as they were.
    for column in declared.unwrap_or_default() {
        let (Some(tracked_column), Some(redeclared)) =
            (named(tracked, &column.name), redeclared(&column.name))
        else {
            continue;
        };
        if let Some(reason) = pintail_probe::declaration_rewrites(tracked_column, &redeclared)
            && !still_shown(&column.name, &redeclared)
        {
            return Err(format!(
                "{reason}; the source has changed the column again since this statement, so \
                 the rows it rewrote cannot be told from the mirror's"
            ));
        }
    }
    let Some(first) = proofs.first() else {
        return Ok(());
    };
    if !rows_as_of_statement {
        return Err(first.reason.clone());
    }
    // Every narrowing the probe shows has to be this statement's own.
    for proof in proofs {
        if !redeclared(&proof.column).is_some_and(|column| still_shown(&proof.column, &column)) {
            return Err(proof.reason.clone());
        }
    }
    match rewritten_column(store, proofs) {
        Ok(None) => Ok(()),
        Ok(Some(proof)) => Err(format!("{}; the mirror holds such a value", proof.reason)),
        Err(error) => Err(format!(
            "{}; the stored values could not be read: {error}",
            first.reason
        )),
    }
}

/// The first column holding a value its new declaration rewrote, reading
/// only the columns in question, a bounded chunk at a time.
fn rewritten_column<'a>(
    store: &TableStore,
    proofs: &'a [StoredValueProof],
) -> Result<Option<&'a StoredValueProof>, StoreError> {
    let snapshot = store.snapshot();
    let Some((first, last)) = snapshot.key_bounds() else {
        return Ok(None);
    };
    let column_ids = proofs
        .iter()
        .map(|proof| proof.column_id)
        .collect::<Vec<_>>();
    let rewritten = |row: &[Value]| {
        proofs
            .iter()
            .zip(row)
            .find(|(proof, value)| !proof.checks.iter().all(|check| check.admits(value)))
            .map(|(proof, _)| proof)
    };
    if let Some(mut stream) = snapshot.scan_projected_range_stream(&first, &last, &column_ids)? {
        while let Some(chunk) = stream.next_chunk(PROOF_SCAN_MEMORY)? {
            if let Some(proof) = chunk.rows().iter().find_map(|row| rewritten(row)) {
                return Ok(Some(proof));
            }
        }
        return Ok(None);
    }
    let scan = snapshot.scan_projected_range(&first, &last, &column_ids)?;
    Ok(scan.rows().iter().find_map(|row| rewritten(row.values())))
}

#[cfg(test)]
mod tests {
    use super::{catalogue_terms, redeclared_column, same_declaration};
    use crate::ddl::modified_columns;
    use pintail_probe::{DeclaredColumn, SourceColumn, declared_column};

    fn column(data_type: &str, column_type: &str, nullable: bool) -> SourceColumn {
        declared_column(&DeclaredColumn {
            ordinal: 1,
            name: "v",
            data_type,
            column_type,
            numeric_precision: None,
            numeric_scale: None,
            datetime_precision: None,
            nullable,
            collation: None,
        })
        .expect("a declared column")
    }

    #[test]
    fn a_statement_is_read_for_the_columns_it_redeclares() {
        let columns = modified_columns(
            "ALTER TABLE t MODIFY v SMALLINT NOT NULL DEFAULT 0 COMMENT 'x', \
             CHANGE COLUMN w w VARCHAR(8) CHARACTER SET utf8mb4 NULL, \
             MODIFY e ENUM('a', 'it''s')",
        )
        .expect("readable");
        let read = columns
            .iter()
            .map(|column| {
                (
                    column.name.as_str(),
                    catalogue_terms(&column.declared_type),
                    column.not_null,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(read[0].0, "v");
        assert_eq!(
            read[0].1,
            Some(("smallint".to_owned(), "smallint".to_owned()))
        );
        assert!(read[0].2);
        assert_eq!(read[1].0, "w");
        assert_eq!(
            read[1].1,
            Some(("varchar".to_owned(), "varchar(8)".to_owned()))
        );
        assert!(!read[1].2);
        assert_eq!(
            read[2].1.as_ref().map(|terms| terms.0.as_str()),
            Some("enum")
        );
        // A generated value, a key or anything else that decides stored
        // values leaves the statement unread.
        assert!(modified_columns("ALTER TABLE t MODIFY v INT AS (w + 1) STORED").is_none());
        assert!(modified_columns("ALTER TABLE t MODIFY v INT PRIMARY KEY").is_none());
        // A type the statement alone does not decide.
        assert_eq!(catalogue_terms("GEOMETRY"), None);
        assert_eq!(catalogue_terms("TIMESTAMP"), None);
        assert_eq!(catalogue_terms("TEXT(300)"), None);
        assert_eq!(
            catalogue_terms("TINYTEXT"),
            Some(("tinytext".to_owned(), "tinytext".to_owned()))
        );
    }

    #[test]
    fn a_redeclaration_is_compared_as_the_catalogue_spells_it() {
        let declared = |statement: &str| {
            modified_columns(statement)
                .expect("readable")
                .pop()
                .expect("one column")
        };
        let tracked = column("bigint", "bigint", true);
        let narrowed = redeclared_column(
            &tracked,
            &declared("ALTER TABLE t MODIFY v SMALLINT NULL"),
            false,
        )
        .expect("decided by the statement");
        assert!(same_declaration(
            &narrowed,
            &column("smallint", "smallint", true)
        ));
        // An older catalogue prints a display width.
        assert!(same_declaration(
            &narrowed,
            &column("smallint", "smallint(6)", true)
        ));
        assert!(!same_declaration(&narrowed, &column("int", "int", true)));
        assert!(!same_declaration(
            &narrowed,
            &column("smallint", "smallint", false)
        ));
        assert!(!same_declaration(
            &narrowed,
            &column("smallint", "smallint unsigned", true)
        ));
        // A key column stays NOT NULL when its redeclaration does not say.
        let key = redeclared_column(
            &column("bigint", "bigint", false),
            &declared("ALTER TABLE t MODIFY v INT"),
            true,
        )
        .expect("decided by the statement");
        assert!(!key.nullable);
        let labels = redeclared_column(
            &column("enum", "enum('a','b','c')", true),
            &declared("ALTER TABLE t MODIFY v ENUM('a', 'c')"),
            false,
        )
        .expect("decided by the statement");
        assert!(same_declaration(
            &labels,
            &column("enum", "enum('a','c')", true)
        ));
        assert!(!same_declaration(
            &labels,
            &column("enum", "enum('a','b','c')", true)
        ));
    }
}
