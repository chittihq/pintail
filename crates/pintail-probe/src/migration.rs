//! Whether a source column's declaration can change under a running mirror
//! without touching the rows already copied.
//!
//! `ALTER TABLE` reaches the replica as one DDL statement and nothing else:
//! the source rewrites every stored value the change affects, and emits no row
//! event for any of them. So the whole question a schema change asks is
//! whether the values it rewrote are the values it already had. When they are,
//! the mirror adopts the new declaration and keeps streaming; when they are
//! not, every untouched row on the replica is now stale and the table has to be
//! recopied.
//!
//! Deciding that from Pintail's mapped type alone is not enough, and that is
//! the reason this module exists. Whole families of change keep the mapped type
//! identical while rewriting values underneath it: a narrowing integer clips,
//! a shrinking string truncates, `DATETIME` becoming `TIMESTAMP` zeroes every
//! value outside the epoch window, a dropped `ENUM` member turns into the empty
//! label, a reordered `SET` re-renders, a changed generated expression
//! recomputes the column outright. Each one is a migration an operator runs
//! without a second thought, and each one leaves "same mapped type" reading
//! safe on a table that is now wrong.
//!
//! The rule here is therefore the conservative one: adopt in place only what
//! the source demonstrably leaves alone. A refusal is not a failure - it routes
//! the table through the resync that copies the rewritten values correctly -
//! so an unrecognised declaration refuses rather than guesses.

use crate::SourceColumn;

/// Why `refreshed` cannot be adopted onto rows copied under `previous`.
///
/// `None` means every value already stored stays exactly as it reads at the
/// source after the change.
pub(crate) fn unsafe_column_change(
    previous: &SourceColumn,
    refreshed: &SourceColumn,
) -> Option<String> {
    let reason = |detail: String| Some(format!("column {} {detail}", refreshed.name));

    // A generated column's values are produced by its expression, so a changed
    // expression rewrites every row without the source ever saying so. The
    // same is true of a column that starts or stops being generated.
    if let Some(detail) = generation_change(previous, refreshed) {
        return reason(detail);
    }
    // NOT NULL converts the NULLs already stored into the type's implicit
    // default; the replica's copies of those rows stay NULL.
    if previous.nullable && !refreshed.nullable {
        return reason("became NOT NULL, which rewrites the NULLs already stored".to_owned());
    }

    let before = Declaration::read(previous);
    let after = Declaration::read(refreshed);
    // Signedness is carried on the column type rather than the data type, so
    // an integer wears it in its family and a DECIMAL or a float does not.
    // MySQL converts a negative value to zero either way.
    if before.unsigned != after.unsigned {
        return reason(format!(
            "changed signedness from {} to {}, which converts every negative value the source \
             held to zero",
            previous.mysql_column_type, refreshed.mysql_column_type
        ));
    }
    if before.family != after.family {
        return reason(format!(
            "changed from {} to {}, which converts every stored value",
            previous.mysql_data_type, refreshed.mysql_data_type
        ));
    }
    if narrowed(&before, &after) {
        return reason(format!(
            "narrowed from {} to {}, which clips or truncates the values already stored",
            previous.mysql_column_type, refreshed.mysql_column_type
        ));
    }
    match before.family {
        Family::Enumeration => {
            // MySQL converts an ENUM by label, so reordering and appending
            // members leave every stored label as it was - only the ordinal
            // moves, and that is read from the refreshed declaration. A member
            // that disappears (dropped, or renamed, which is a drop and an add)
            // turns every row holding it into the empty label.
            let (before_labels, after_labels) = labels(previous, refreshed, "enum")?;
            let lost = before_labels
                .iter()
                .find(|label| !after_labels.contains(label))?;
            reason(format!(
                "no longer declares the ENUM member {lost}, so the source replaced it with the \
                 empty label in every row holding it"
            ))
        }
        Family::Selection => {
            // A SET renders its members in declaration order, so a reorder
            // rewrites the text of every row holding more than one member even
            // though the membership is untouched. Only an append is inert.
            let (before_labels, after_labels) = labels(previous, refreshed, "set")?;
            (!after_labels.starts_with(&before_labels)).then(|| {
                format!(
                    "column {} reordered or dropped SET members, which re-renders the values \
                     already stored",
                    refreshed.name
                )
            })
        }
        _ => None,
    }
}

/// Whether the refreshed declaration holds less than the previous one.
///
/// A decimal needs both of its halves compared, because they narrow
/// independently: `(10,2)` to `(12,1)` gains an integer digit and still rounds
/// away a fractional one. Everything else orders on a single capacity, and a
/// capacity either side leaves undeclared is no evidence of a narrowing - the
/// family check has already ruled out the conversions that rewrite values
/// whatever their width.
fn narrowed(before: &Declaration, after: &Declaration) -> bool {
    if before.family == Family::Decimal {
        let digits = |declaration: &Declaration| {
            let precision = declaration.numbers.first().copied().unwrap_or(10);
            let scale = declaration.numbers.get(1).copied().unwrap_or(0);
            (precision.saturating_sub(scale), scale)
        };
        let (before_integer, before_scale) = digits(before);
        let (after_integer, after_scale) = digits(after);
        return after_integer < before_integer || after_scale < before_scale;
    }
    matches!(
        (before.capacity, after.capacity),
        (Some(before), Some(after)) if after < before
    )
}

/// The previous and refreshed member lists, or `None` when either declaration
/// cannot be read - an unreadable list is no evidence of a change.
fn labels(
    previous: &SourceColumn,
    refreshed: &SourceColumn,
    kind: &str,
) -> Option<(Vec<String>, Vec<String>)> {
    Some((
        pintail_types::declaration_labels(&previous.mysql_column_type, kind)?,
        pintail_types::declaration_labels(&refreshed.mysql_column_type, kind)?,
    ))
}

fn generation_change(previous: &SourceColumn, refreshed: &SourceColumn) -> Option<String> {
    let was = is_generated(previous);
    let is = is_generated(refreshed);
    if was != is {
        return Some(if is {
            "became a generated column, so the source computed a value for every existing row"
                .to_owned()
        } else {
            "stopped being generated, so its values are no longer the ones the expression \
             produced"
                .to_owned()
        });
    }
    // A probe recorded before the expression was captured reads as empty, and
    // an empty expression on one side alone says nothing about a change.
    let before = previous.generation_expression.trim();
    let after = refreshed.generation_expression.trim();
    (was && !before.is_empty() && !after.is_empty() && before != after)
        .then(|| "changed its generated expression, which recomputes every existing row".to_owned())
}

fn is_generated(column: &SourceColumn) -> bool {
    column.generated_stored || column.virtual_generated()
}

/// The part of a column's declaration that decides whether a change to it
/// rewrites stored values.
struct Declaration {
    family: Family,
    /// How much the column can hold, in the unit its family truncates by:
    /// bytes for strings and binaries, bits for integers and `BIT`, digits of
    /// fractional seconds for temporals. Absent when the declaration carries no
    /// capacity, or none this can read - decimals, whose two halves narrow
    /// independently, are read from `numbers` instead.
    capacity: Option<u64>,
    /// The numbers the declaration spells out, in order.
    numbers: Vec<u64>,
    /// Whether the declaration is UNSIGNED.
    unsigned: bool,
}

/// Declarations that convert into one another without rewriting values belong
/// to one family; everything else is a conversion.
///
/// `DATETIME` and `TIMESTAMP` are deliberately separate despite mapping to the
/// same Pintail type: the source stores a `TIMESTAMP` as an epoch instant, and
/// the conversion was measured to zero every value outside that window -
/// `1960-01-01` and `2099-01-01` both became the zero date.
#[derive(Eq, PartialEq)]
enum Family {
    SignedInteger,
    UnsignedInteger,
    Decimal,
    /// `FLOAT` and `DOUBLE` are separate families: the source re-renders a
    /// widened `FLOAT` from its stored approximation, so `0.1` reads back as
    /// `0.10000000149011612`, and narrowing the other way rounds.
    Float,
    Double,
    Bit,
    Date,
    DateTime,
    Timestamp,
    Time,
    Year,
    /// `CHAR`, `VARCHAR` and the `TEXT` sizes: one lane of character data whose
    /// only difference is how much of it fits.
    Text,
    /// `BINARY`, `VARBINARY` and the `BLOB` sizes.
    Binary,
    Enumeration,
    Selection,
    Json,
    /// Anything else, kept apart by its own spelling so an unrecognised type
    /// compares equal to itself and to nothing else.
    Other(String),
}

impl Declaration {
    fn read(column: &SourceColumn) -> Self {
        let data_type = column.mysql_data_type.to_ascii_lowercase();
        let column_type = column.mysql_column_type.to_ascii_lowercase();
        let unsigned = column_type.contains("unsigned");
        let family = match data_type.as_str() {
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "bool"
            | "boolean" => {
                if unsigned {
                    Family::UnsignedInteger
                } else {
                    Family::SignedInteger
                }
            }
            "decimal" | "dec" | "numeric" | "fixed" => Family::Decimal,
            "float" => Family::Float,
            "double" | "double precision" | "real" => Family::Double,
            "bit" => Family::Bit,
            "date" => Family::Date,
            "datetime" => Family::DateTime,
            "timestamp" => Family::Timestamp,
            "time" => Family::Time,
            "year" => Family::Year,
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => Family::Text,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                Family::Binary
            }
            "enum" => Family::Enumeration,
            "set" => Family::Selection,
            "json" => Family::Json,
            other => Family::Other(other.to_owned()),
        };
        // Only a number is signed, and only its declaration says so. Reading
        // the word anywhere else would find it inside an ENUM member.
        let unsigned = unsigned
            && matches!(
                family,
                Family::SignedInteger
                    | Family::UnsignedInteger
                    | Family::Decimal
                    | Family::Float
                    | Family::Double
            );
        let numbers = declared_numbers(&column_type);
        let capacity = match family {
            Family::SignedInteger | Family::UnsignedInteger => integer_bits(&data_type),
            Family::Bit => Some(numbers.first().copied().unwrap_or(1)),
            Family::Text | Family::Binary => string_bytes(column, &data_type, &column_type),
            // Fractional-second precision is part of the type, and dropping it
            // rounds every stored value.
            Family::DateTime | Family::Timestamp | Family::Time => {
                Some(numbers.first().copied().unwrap_or(0))
            }
            _ => None,
        };
        Self {
            family,
            capacity,
            numbers,
            unsigned,
        }
    }
}

fn integer_bits(data_type: &str) -> Option<u64> {
    Some(match data_type {
        "bool" | "boolean" | "tinyint" => 8,
        "smallint" => 16,
        "mediumint" => 24,
        "int" | "integer" => 32,
        "bigint" => 64,
        _ => return None,
    })
}

/// How many bytes of text or binary the declaration holds.
///
/// `CHAR` and `VARCHAR` declare characters, the `TEXT` and `BLOB` sizes declare
/// bytes, and a change can move between the two - so both are measured in
/// bytes, the unit the source truncates by.
fn string_bytes(column: &SourceColumn, data_type: &str, column_type: &str) -> Option<u64> {
    let fixed = match data_type {
        "tinytext" | "tinyblob" => Some(255),
        "text" | "blob" => Some(65_535),
        "mediumtext" | "mediumblob" => Some(16_777_215),
        "longtext" | "longblob" => Some(4_294_967_295),
        _ => None,
    };
    if let Some(bytes) = fixed {
        return Some(bytes);
    }
    let characters = declared_numbers(column_type).first().copied()?;
    let per_character = match data_type {
        "binary" | "varbinary" => 1,
        _ => character_bytes(column.character_set.as_deref()),
    };
    Some(characters.saturating_mul(per_character))
}

/// Widest encoding of one character in a source character set.
///
/// An unrecognised set is read as the widest `MySQL` has, which keeps the
/// comparison conservative in the direction that matters: it never reports a
/// narrowing that is not one, and never hides a real narrowing to a set this
/// does know.
fn character_bytes(character_set: Option<&str>) -> u64 {
    match character_set.map(str::to_ascii_lowercase).as_deref() {
        Some("ascii" | "latin1" | "latin2" | "latin5" | "latin7" | "binary" | "cp1250"
        | "cp1251" | "cp1256" | "cp1257" | "cp850" | "cp852" | "cp866" | "dec8" | "greek"
        | "hebrew" | "hp8" | "keybcs2" | "koi8r" | "koi8u" | "macce" | "macroman" | "swe7"
        | "tis620" | "geostd8" | "armscii8") => 1,
        Some("ucs2" | "big5" | "cp932" | "euckr" | "gb2312" | "gbk" | "sjis" | "utf16le") => 2,
        Some("utf8" | "utf8mb3" | "ujis" | "eucjpms") => 3,
        _ => 4,
    }
}

/// The numbers inside a declaration's parentheses, in order: the `10` and `2`
/// of `decimal(10,2)`, the `64` of `varchar(64)`.
fn declared_numbers(column_type: &str) -> Vec<u64> {
    let Some(open) = column_type.find('(') else {
        return Vec::new();
    };
    let Some(close) = column_type[open..].find(')') else {
        return Vec::new();
    };
    column_type[open + 1..open + close]
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::unsafe_column_change;
    use crate::SourceColumn;
    use pintail_types::DataType;

    /// A column as the probe reports it. Every case below states only what its
    /// family needs; the rest is the same inert column each time.
    fn column(data_type: &str, column_type: &str) -> SourceColumn {
        SourceColumn {
            id: 1,
            name: "v".to_owned(),
            mysql_data_type: data_type.to_owned(),
            mysql_column_type: column_type.to_owned(),
            pintail_type: DataType::Int64,
            nullable: true,
            character_set: None,
            collation: None,
            generated_stored: false,
            generation_expression: String::new(),
            extra: String::new(),
            auto_increment: false,
            default_value: None,
            default_generated: false,
            ordinal: 1,
        }
    }

    fn text(data_type: &str, column_type: &str, character_set: &str) -> SourceColumn {
        SourceColumn {
            pintail_type: DataType::Utf8,
            character_set: Some(character_set.to_owned()),
            ..column(data_type, column_type)
        }
    }

    fn adopts(previous: &SourceColumn, refreshed: &SourceColumn) -> bool {
        unsafe_column_change(previous, refreshed).is_none()
    }

    /// Every case here was run against `MySQL` 8.4 first; the comment on each
    /// group is what the source actually did to the rows already stored.
    #[test]
    fn integer_width_widens_but_never_narrows() {
        // INT -> BIGINT left 2147483647 alone; BIGINT -> SMALLINT clipped
        // 100000 to 32767 and -100000 to -32768.
        assert!(adopts(&column("int", "int"), &column("bigint", "bigint")));
        assert!(!adopts(&column("bigint", "bigint"), &column("smallint", "smallint")));
        assert!(adopts(&column("tinyint", "tinyint(1)"), &column("int", "int")));
        // A display width is not a capacity.
        assert!(adopts(&column("int", "int(11)"), &column("int", "int(4)")));
    }

    #[test]
    fn signedness_is_a_conversion() {
        // INT -> INT UNSIGNED turned -5 into 0.
        assert!(!adopts(
            &column("int", "int"),
            &column("int", "int unsigned"),
        ));
        assert!(!adopts(
            &column("bigint", "bigint unsigned"),
            &column("bigint", "bigint"),
        ));
        assert!(adopts(
            &column("int", "int unsigned"),
            &column("bigint", "bigint unsigned"),
        ));
        // A DECIMAL wears UNSIGNED on the column type alone, so its family
        // cannot carry the change the way an integer's does.
        assert!(!adopts(
            &column("decimal", "decimal(10,2)"),
            &column("decimal", "decimal(10,2) unsigned"),
        ));
        assert!(!adopts(
            &column("double", "double unsigned"),
            &column("double", "double"),
        ));
        // The word inside an ENUM member is not a declaration of signedness.
        assert!(adopts(
            &column("enum", "enum('signed')"),
            &column("enum", "enum('signed','unsigned')"),
        ));
    }

    #[test]
    fn numeric_representation_never_crosses_families() {
        // DECIMAL(20,4) -> DOUBLE rendered 1234567890123456.1234 as
        // 1.234567890123456e15; FLOAT -> DOUBLE rendered 0.1 as
        // 0.10000000149011612.
        assert!(!adopts(
            &column("decimal", "decimal(20,4)"),
            &column("double", "double"),
        ));
        assert!(!adopts(&column("float", "float"), &column("double", "double")));
        assert!(!adopts(&column("double", "double"), &column("float", "float")));
        assert!(!adopts(&column("varchar", "varchar(16)"), &column("int", "int")));
        assert!(!adopts(&column("int", "int"), &column("varchar", "varchar(16)")));
    }

    #[test]
    fn decimal_digits_may_grow_and_never_shrink() {
        // (10,2) -> (14,4) re-rendered 12.34 as 12.3400, the same value;
        // (14,4) -> (10,1) rounded it to 12.3 and -99999999.99 to -100000000.0.
        assert!(adopts(
            &column("decimal", "decimal(10,2)"),
            &column("decimal", "decimal(14,4)"),
        ));
        assert!(!adopts(
            &column("decimal", "decimal(14,4)"),
            &column("decimal", "decimal(10,1)"),
        ));
        assert!(!adopts(
            &column("decimal", "decimal(10,2)"),
            &column("decimal", "decimal(10,1)"),
        ));
        // Both halves narrow on their own: this one gains integer digits while
        // rounding a fractional one away.
        assert!(!adopts(
            &column("decimal", "decimal(10,2)"),
            &column("decimal", "decimal(12,1)"),
        ));
    }

    #[test]
    fn string_capacity_grows_across_char_varchar_and_text() {
        // VARCHAR(64) -> VARCHAR(8) truncated a 26-character value to 8;
        // TEXT -> TINYTEXT cut 400 characters to 255.
        assert!(adopts(
            &text("varchar", "varchar(64)", "utf8mb4"),
            &text("text", "text", "utf8mb4"),
        ));
        assert!(adopts(
            &text("text", "text", "utf8mb4"),
            &text("longtext", "longtext", "utf8mb4"),
        ));
        assert!(!adopts(
            &text("varchar", "varchar(64)", "utf8mb4"),
            &text("varchar", "varchar(8)", "utf8mb4"),
        ));
        assert!(!adopts(
            &text("text", "text", "utf8mb4"),
            &text("tinytext", "tinytext", "utf8mb4"),
        ));
        assert!(!adopts(
            &text("char", "char(10)", "utf8mb4"),
            &text("char", "char(4)", "utf8mb4"),
        ));
    }

    #[test]
    fn a_wider_character_set_is_a_wider_column() {
        // latin1 -> utf8mb4 re-encoded 0xE9 as 0xC3A9: the same character, so
        // the decoded value the replica holds is unchanged.
        assert!(adopts(
            &text("varchar", "varchar(32)", "latin1"),
            &text("varchar", "varchar(32)", "utf8mb4"),
        ));
        // The reverse cannot hold every character the column could store.
        assert!(!adopts(
            &text("varchar", "varchar(32)", "utf8mb4"),
            &text("varchar", "varchar(32)", "latin1"),
        ));
        // A collation change alone leaves every stored value identical.
        assert!(adopts(
            &SourceColumn {
                collation: Some("utf8mb4_0900_ai_ci".to_owned()),
                ..text("varchar", "varchar(32)", "utf8mb4")
            },
            &SourceColumn {
                collation: Some("utf8mb4_bin".to_owned()),
                ..text("varchar", "varchar(32)", "utf8mb4")
            },
        ));
    }

    #[test]
    fn text_and_binary_are_different_lanes() {
        assert!(!adopts(
            &text("text", "text", "utf8mb4"),
            &column("blob", "blob"),
        ));
        assert!(!adopts(
            &text("varchar", "varchar(32)", "utf8mb4"),
            &column("varbinary", "varbinary(32)"),
        ));
        assert!(adopts(
            &column("varbinary", "varbinary(16)"),
            &column("varbinary", "varbinary(64)"),
        ));
        // VARBINARY(16) -> VARBINARY(4) cut 0011223344556677 to 00112233.
        assert!(!adopts(
            &column("varbinary", "varbinary(16)"),
            &column("varbinary", "varbinary(4)"),
        ));
    }

    #[test]
    fn temporal_flavours_are_conversions_even_at_one_mapped_type() {
        // DATETIME -> TIMESTAMP zeroed 1960-01-01 and 2099-01-01 outright, and
        // both declarations map to the same Pintail type - which is exactly why
        // the mapped type cannot decide this.
        assert!(!adopts(
            &column("datetime", "datetime"),
            &column("timestamp", "timestamp"),
        ));
        assert!(!adopts(
            &column("timestamp", "timestamp"),
            &column("datetime", "datetime"),
        ));
        assert!(!adopts(&column("datetime", "datetime"), &column("date", "date")));
        // DATETIME(6) -> DATETIME(0) rounded .654321 up to the next second.
        assert!(!adopts(
            &column("datetime", "datetime(6)"),
            &column("datetime", "datetime(0)"),
        ));
        assert!(adopts(
            &column("datetime", "datetime(0)"),
            &column("datetime", "datetime(6)"),
        ));
    }

    #[test]
    fn enum_members_may_be_appended_or_reordered_but_not_lost() {
        // MySQL converts an ENUM by label: reordering ('alpha','beta','gamma')
        // to ('gamma','beta','alpha') left every row's label alone and only
        // moved its ordinal. Dropping 'beta' turned those rows into ''; so did
        // renaming 'draft' to 'pending'.
        let three = column("enum", "enum('alpha','beta','gamma')");
        assert!(adopts(
            &column("enum", "enum('alpha','beta')"),
            &three,
        ));
        assert!(adopts(
            &three,
            &column("enum", "enum('gamma','beta','alpha')"),
        ));
        assert!(!adopts(&three, &column("enum", "enum('alpha','gamma')")));
        assert!(!adopts(
            &column("enum", "enum('draft','sent')"),
            &column("enum", "enum('pending','sent')"),
        ));
    }

    #[test]
    fn set_members_may_only_be_appended() {
        // A SET renders in declaration order: reordering ('a','b','c') to
        // ('c','b','a') turned the stored 'b,c' into 'c,b'. Appending 'c' left
        // 'a,b' exactly as it was.
        assert!(adopts(
            &column("set", "set('a','b')"),
            &column("set", "set('a','b','c')"),
        ));
        assert!(!adopts(
            &column("set", "set('a','b','c')"),
            &column("set", "set('c','b','a')"),
        ));
        assert!(!adopts(
            &column("set", "set('a','b','c')"),
            &column("set", "set('a','c')"),
        ));
    }

    #[test]
    fn a_generated_expression_rewrites_every_row() {
        // base * 2 became base * 100, and the source recomputed 20 and 40 into
        // 1000 and 2000 without emitting a single row event.
        let generated = |expression: &str| SourceColumn {
            generated_stored: true,
            generation_expression: expression.to_owned(),
            extra: "STORED GENERATED".to_owned(),
            ..column("int", "int")
        };
        assert!(adopts(&generated("(`base` * 2)"), &generated("(`base` * 2)")));
        assert!(!adopts(&generated("(`base` * 2)"), &generated("(`base` * 100)")));
        assert!(!adopts(&column("int", "int"), &generated("(`base` * 2)")));
        assert!(!adopts(&generated("(`base` * 2)"), &column("int", "int")));
        // A probe stored before expressions were captured reads as empty, and
        // says nothing about a change.
        assert!(adopts(&generated(""), &generated("(`base` * 2)")));
    }

    #[test]
    fn nullability_may_loosen_but_not_tighten() {
        // NULL -> NOT NULL replaced the stored NULL with 0.
        let nullable = column("int", "int");
        let required = SourceColumn {
            nullable: false,
            ..column("int", "int")
        };
        assert!(!adopts(&nullable, &required));
        assert!(adopts(&required, &nullable));
    }

    #[test]
    fn bit_width_widens_but_never_narrows() {
        // BIT(16) -> BIT(4) cut 65535 to 15.
        assert!(adopts(&column("bit", "bit(4)"), &column("bit", "bit(16)")));
        assert!(!adopts(&column("bit", "bit(16)"), &column("bit", "bit(4)")));
    }

    #[test]
    fn an_unrecognised_type_compares_only_against_itself() {
        assert!(adopts(
            &column("geometry", "geometry"),
            &column("geometry", "geometry"),
        ));
        assert!(!adopts(
            &column("geometry", "geometry"),
            &column("point", "point"),
        ));
    }
}
