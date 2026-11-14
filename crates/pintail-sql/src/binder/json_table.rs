//! Binding of the `JSON_TABLE` table function.
//!
//! `JSON_TABLE(document, path COLUMNS (...)) AS alias` is a relation whose
//! rows are computed from a JSON document. The document may be a constant
//! or read columns of relations written before it in the same `FROM`, which
//! makes the function implicitly lateral: it is evaluated once per row of
//! what precedes it. The binder resolves the document against that scope,
//! turns the column list into typed output columns, and leaves the paths
//! and `DEFAULT` texts as written for the executor to compile.

use std::sync::Arc;

use pintail_catalog::{DatabaseId, TableId};
use pintail_types::DataType;
use sqlparser::ast::{
    CharacterLength, DataType as SqlDataType, ExactNumberInfo, Expr, JsonTableColumn as SqlColumn,
    JsonTableColumnErrorHandling, TableFactor, Value as SqlValue, ValueWithSpan,
};

use super::{BindError, Binder, bind_expr, expression_scope};
use crate::bound::{
    BoundColumn, BoundExpr, BoundExprKind, BoundJsonTable, BoundProjection, BoundQuery, BoundTable,
    DEFAULT_TEXT_COLLATION, JsonTableColumn, JsonTableFallback, JsonTableType,
};

/// A refusal that names the table function, so the reader knows which part
/// of the statement it is about.
fn refuse(reason: impl std::fmt::Display) -> BindError {
    BindError::UnsupportedTableFactor(format!("JSON_TABLE: {reason}"))
}

impl Binder<'_> {
    /// Binds a `JSON_TABLE` factor. `preceding` holds the relations written
    /// before it in the same `FROM`, which its document may read.
    pub(super) fn bind_json_table(
        &self,
        factor: &TableFactor,
        preceding: &[BoundTable],
    ) -> Result<BoundTable, BindError> {
        let TableFactor::JsonTable {
            json_expr,
            json_path,
            columns,
            alias,
        } = factor
        else {
            return Err(BindError::UnsupportedTableFactor(factor.to_string()));
        };
        let alias = alias
            .as_ref()
            .ok_or_else(|| refuse("Every table function must have an alias"))?;
        if !alias.columns.is_empty() || alias.at.is_some() {
            return Err(refuse("a column list after the alias is not supported"));
        }
        let relation_name = alias.name.value.clone();

        let document = self.bind_document(json_expr, preceding)?;
        let path = path_text(json_path)?;

        let mut outputs = Vec::new();
        let columns = bind_columns(columns, &mut outputs)?;
        for (index, (name, _)) in outputs.iter().enumerate() {
            if outputs[..index]
                .iter()
                .any(|(earlier, _)| earlier.eq_ignore_ascii_case(name))
            {
                return Err(refuse(format_args!("Duplicate column name '{name}'")));
            }
        }

        let table_id = self.next_derived_id.get();
        self.next_derived_id.set(table_id.saturating_sub(1));
        let table_id = TableId::new(table_id);
        let database_id = DatabaseId::new(u64::MAX);
        let bound_columns = outputs
            .iter()
            .enumerate()
            .map(|(index, (name, data_type))| BoundColumn {
                database_id,
                table_id,
                column_id: u32::try_from(index + 1).unwrap_or(u32::MAX - 3),
                relation_name: relation_name.clone(),
                name: name.clone(),
                data_type: *data_type,
                // A nested list's columns are NULL on the rows of its
                // siblings, and a value column is NULL wherever its path has
                // no storable match.
                nullable: true,
                collation: None,
                enum_labels: None,
                geometry: false,
                timestamp: false,
                binary_width: None,
                bit_width: None,
                float_decimals: None,
                outer: false,
                using_shadowed: false,
                declared_set: false,
            })
            .collect::<Vec<_>>();

        let placeholder = placeholder_query(&bound_columns);

        Ok(BoundTable {
            database_id,
            table_id,
            database_name: String::new(),
            table_name: relation_name.clone(),
            relation_name,
            schema_version: 0,
            columns: bound_columns,
            row_count: None,
            estimated_rows: None,
            key_column_ids: Vec::new(),
            column_statistics: None,
            input: Some(Box::new(placeholder)),
            lateral: false,
            json_table: Some(Arc::new(BoundJsonTable {
                document,
                path,
                columns,
                width: outputs.len(),
            })),
        })
    }

    /// The document expression, resolved against the relations before the
    /// table function. A column of an enclosing query is refused: a
    /// subquery's `JSON_TABLE` would have to be recomputed for each outer
    /// row, and that path does not carry a table function.
    fn bind_document(&self, expr: &Expr, preceding: &[BoundTable]) -> Result<BoundExpr, BindError> {
        let scope = expression_scope(preceding, &self.outer_tables);
        let document = bind_expr(expr, &scope, None).map_err(|error| match error {
            BindError::UnsupportedSubquery(_) => refuse("a subquery as the document"),
            other => other,
        })?;
        if reads_enclosing_query(&document) {
            return Err(refuse(
                "a document that reads a column of an enclosing query is not supported",
            ));
        }
        match document.data_type {
            None | Some(DataType::Utf8 | DataType::Json) => Ok(document),
            Some(_) => Err(refuse(
                "Invalid data type for JSON data in argument 1 to function json_table; \
                 a JSON string or JSON type is required.",
            )),
        }
    }
}

fn reads_enclosing_query(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Column(column) => column.outer,
        BoundExprKind::PreparedIn { expr, .. }
        | BoundExprKind::Unary { expr, .. }
        | BoundExprKind::IsNull { expr, .. } => reads_enclosing_query(expr),
        BoundExprKind::Binary { left, right, .. } => {
            reads_enclosing_query(left) || reads_enclosing_query(right)
        }
        BoundExprKind::Scalar { args, .. } => args.iter().any(reads_enclosing_query),
        // bind_expr had no subquery resolver, so none of these was built.
        BoundExprKind::InSubquery { .. }
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::ExistsSubquery { .. } => true,
        BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_)
        | BoundExprKind::Window(_)
        | BoundExprKind::Literal(_) => false,
    }
}

fn path_text(path: &ValueWithSpan) -> Result<String, BindError> {
    match &path.value {
        SqlValue::SingleQuotedString(text) | SqlValue::DoubleQuotedString(text) => Ok(text.clone()),
        other => Err(refuse(format_args!(
            "a path must be a string literal, not {other}"
        ))),
    }
}

/// Binds one column list, appending each output column's name and type to
/// `outputs` in the order `MySQL` lays a row out: as written, a nested
/// list's columns where the `NESTED` clause stands.
fn bind_columns(
    columns: &[SqlColumn],
    outputs: &mut Vec<(String, DataType)>,
) -> Result<Vec<JsonTableColumn>, BindError> {
    columns
        .iter()
        .map(|column| match column {
            SqlColumn::ForOrdinality(name) => {
                outputs.push((name.value.clone(), DataType::Int64));
                Ok(JsonTableColumn::Ordinality {
                    output: outputs.len() - 1,
                })
            }
            SqlColumn::Nested(nested) => Ok(JsonTableColumn::Nested {
                path: path_text(&nested.path)?,
                columns: bind_columns(&nested.columns, outputs)?,
            }),
            SqlColumn::Named(named) => {
                let name = named.name.value.clone();
                let target = column_type(&named.r#type).ok_or_else(|| {
                    refuse(format_args!(
                        "column '{name}' has type {}, which is not supported",
                        named.r#type
                    ))
                })?;
                let path = path_text(&named.path)?;
                outputs.push((name.clone(), target.data_type()));
                let output = outputs.len() - 1;
                if named.exists {
                    if matches!(
                        target,
                        JsonTableType::Json | JsonTableType::Date | JsonTableType::DateTime { .. }
                    ) {
                        return Err(refuse(format_args!(
                            "EXISTS column '{name}' has type {}, which is not supported",
                            named.r#type
                        )));
                    }
                    return Ok(JsonTableColumn::Exists {
                        output,
                        name,
                        target,
                        path,
                    });
                }
                Ok(JsonTableColumn::Value {
                    output,
                    target,
                    path,
                    on_empty: fallback(named.on_empty.as_ref())?,
                    on_error: fallback(named.on_error.as_ref())?,
                    name,
                })
            }
        })
        .collect()
}

fn fallback(clause: Option<&JsonTableColumnErrorHandling>) -> Result<JsonTableFallback, BindError> {
    match clause {
        None | Some(JsonTableColumnErrorHandling::Null) => Ok(JsonTableFallback::Null),
        Some(JsonTableColumnErrorHandling::Error) => Ok(JsonTableFallback::Error),
        Some(JsonTableColumnErrorHandling::Default(value)) => match &value.value {
            SqlValue::SingleQuotedString(text) | SqlValue::DoubleQuotedString(text) => {
                Ok(JsonTableFallback::Default(text.clone()))
            }
            _ => Err(refuse(
                "non-string DEFAULT value for a column in a JSON_TABLE expression",
            )),
        },
    }
}

/// The declared column types a JSON value can be stored into. Anything
/// else - binary strings, `BIT`, `ENUM`, `SET`, `TIME`, `YEAR`, `TIMESTAMP`,
/// spatial types - is refused rather than approximated.
#[allow(clippy::too_many_lines)] // one arm per spelling of a type
fn column_type(data_type: &SqlDataType) -> Option<JsonTableType> {
    let signed = |bits: u32| JsonTableType::Integer {
        min: -(1_i64 << (bits - 1)),
        max: (1_u64 << (bits - 1)) - 1,
    };
    let unsigned = |bits: u32| JsonTableType::Integer {
        min: 0,
        max: (1_u64 << bits) - 1,
    };
    let characters = |length: Option<&CharacterLength>, default: Option<u64>| match length {
        Some(CharacterLength::IntegerLength { length, .. }) => Some(JsonTableType::Text {
            max_chars: Some(*length),
            max_bytes: None,
        }),
        Some(CharacterLength::Max) => None,
        None => default.map(|length| JsonTableType::Text {
            max_chars: Some(length),
            max_bytes: None,
        }),
    };
    let bytes = |limit: u64| JsonTableType::Text {
        max_chars: None,
        max_bytes: Some(limit),
    };
    Some(match data_type {
        SqlDataType::TinyInt(_) | SqlDataType::Bool | SqlDataType::Boolean => signed(8),
        SqlDataType::TinyIntUnsigned(_) | SqlDataType::UTinyInt => unsigned(8),
        SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => signed(16),
        SqlDataType::SmallIntUnsigned(_)
        | SqlDataType::Int2Unsigned(_)
        | SqlDataType::USmallInt => unsigned(16),
        SqlDataType::MediumInt(_) => signed(24),
        SqlDataType::MediumIntUnsigned(_) => unsigned(24),
        SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::Int4(_) => signed(32),
        SqlDataType::IntUnsigned(_)
        | SqlDataType::IntegerUnsigned(_)
        | SqlDataType::Int4Unsigned(_) => unsigned(32),
        SqlDataType::BigInt(_) | SqlDataType::Int8(_) => JsonTableType::Integer {
            min: i64::MIN,
            max: i64::MAX.cast_unsigned(),
        },
        SqlDataType::BigIntUnsigned(_) | SqlDataType::Int8Unsigned(_) | SqlDataType::UBigInt => {
            JsonTableType::Integer {
                min: 0,
                max: u64::MAX,
            }
        }
        SqlDataType::Decimal(info) | SqlDataType::Numeric(info) | SqlDataType::Dec(info) => {
            let (precision, scale) = match info {
                ExactNumberInfo::None => (10, 0),
                ExactNumberInfo::Precision(precision) => (*precision, 0),
                ExactNumberInfo::PrecisionAndScale(precision, scale) => (*precision, *scale),
            };
            let precision = u8::try_from(precision).ok()?;
            let scale = u8::try_from(scale).ok()?;
            if precision == 0 || precision > 65 || scale > 30 || scale > precision {
                return None;
            }
            JsonTableType::Decimal { precision, scale }
        }
        SqlDataType::Float(ExactNumberInfo::None | ExactNumberInfo::Precision(0..=24)) => {
            JsonTableType::Float
        }
        SqlDataType::Float(ExactNumberInfo::Precision(25..=53))
        | SqlDataType::Double(ExactNumberInfo::None)
        | SqlDataType::DoublePrecision
        | SqlDataType::Float8 => JsonTableType::Double,
        SqlDataType::Real => {
            if crate::session_parse_mode().real_as_float {
                JsonTableType::Float
            } else {
                JsonTableType::Double
            }
        }
        SqlDataType::Char(length) | SqlDataType::Character(length) => {
            characters(length.as_ref(), Some(1))?
        }
        SqlDataType::Varchar(length)
        | SqlDataType::Nvarchar(length)
        | SqlDataType::CharVarying(length)
        | SqlDataType::CharacterVarying(length) => characters(length.as_ref(), None)?,
        SqlDataType::TinyText => bytes(255),
        SqlDataType::Text => bytes(65_535),
        SqlDataType::MediumText => bytes(16_777_215),
        SqlDataType::LongText => bytes(4_294_967_295),
        SqlDataType::JSON => JsonTableType::Json,
        SqlDataType::Date => JsonTableType::Date,
        SqlDataType::Datetime(fsp) => JsonTableType::DateTime {
            fsp: u8::try_from(fsp.unwrap_or(0))
                .ok()
                .filter(|fsp| *fsp <= 6)?,
        },
        _ => return None,
    })
}

/// The query a table function's relation carries in place of an input.
///
/// The relation is not a stored table, and its rows are not a query's
/// either. The placeholder names the relation's own columns, so result
/// metadata reads their declared types through it and a plan that ever
/// tried to run it would fail on a column nothing produces instead of
/// answering.
fn placeholder_query(columns: &[BoundColumn]) -> BoundQuery {
    BoundQuery {
        text_collation: DEFAULT_TEXT_COLLATION,
        from: Vec::new(),
        tables: Vec::new(),
        projection: columns
            .iter()
            .cloned()
            .map(|column| BoundProjection {
                name: column.name.clone(),
                expr: BoundExpr::column(column),
            })
            .collect(),
        filter: None,
        group_by: Vec::new(),
        aggregates: Vec::new(),
        windows: Vec::new(),
        having: None,
        distinct: false,
        order_by: Vec::new(),
        hidden_sort_columns: 0,
        union_all: Vec::new(),
        union_distinct: false,
        set_ops: Vec::new(),
        limit: None,
        recursive: None,
        outer_set: None,
        outer_set_refusal: None,
    }
}
