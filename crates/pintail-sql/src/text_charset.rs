//! SQL encoding identity travels with expressions, independently of the
//! Unicode value carrier. Only byte-observing operations materialize bytes.
use std::cell::Cell;

use pintail_types::{CharacterSet, DataType, Value};

use crate::{BoundExpr, BoundExprKind, ScalarFunction};

thread_local! {
    static CONNECTION: Cell<CharacterSet> = const { Cell::new(CharacterSet::Utf8Mb4) };
}

/// Install the connection's literal and generated-text encoding for binding.
pub fn set_session_character_set(charset: Option<CharacterSet>) {
    CONNECTION.set(charset.unwrap_or_default());
}

/// The encoding captured in expressions bound on the calling thread.
#[must_use]
pub fn session_character_set() -> CharacterSet {
    CONNECTION.get()
}

pub(crate) fn character_set(expression: &BoundExpr) -> CharacterSet {
    match &expression.kind {
        BoundExprKind::Scalar {
            function:
                ScalarFunction::TextCharset(charset, _)
                | ScalarFunction::RawText(charset, _)
                | ScalarFunction::DecodeText(charset),
            ..
        } => *charset,
        BoundExprKind::Scalar {
            function: ScalarFunction::Collate { .. },
            args,
        } => character_set(&args[0]),
        BoundExprKind::Column(column) => column
            .collation
            .as_deref()
            .and_then(|name| name.split('_').next())
            .and_then(CharacterSet::from_name)
            .unwrap_or_default(),
        _ => CharacterSet::Utf8Mb4,
    }
}

pub(crate) fn annotate(expression: BoundExpr, charset: CharacterSet) -> BoundExpr {
    if expression.data_type != Some(DataType::Utf8) || charset == CharacterSet::Utf8Mb4 {
        return expression;
    }
    let fallback = if charset == session_character_set() {
        crate::session_default_collation()
    } else {
        charset.default_collation()
    };
    let collation =
        crate::bound::NamedCollation::from_name(expression.text_collation().unwrap_or(fallback))
            .expect("text encoding has a supported comparison profile");
    if let BoundExprKind::Scalar { function, args } = &expression.kind {
        if matches!(
            function,
            ScalarFunction::Conv | ScalarFunction::Bin | ScalarFunction::Oct
        ) {
            let bytes = wrap(
                expression,
                ScalarFunction::Cast(DataType::Binary),
                DataType::Binary,
            );
            return wrap(
                bytes,
                ScalarFunction::RawText(charset, collation),
                DataType::Utf8,
            );
        }
        if matches!(function, ScalarFunction::Lower | ScalarFunction::Upper)
            && matches!(
                args[0].kind,
                BoundExprKind::Scalar {
                    function: ScalarFunction::RawText(_, _),
                    ..
                }
            )
        {
            let bytes = wrap(
                encoded(args[0].clone()),
                ScalarFunction::EncodedCase {
                    charset,
                    upper: *function == ScalarFunction::Upper,
                },
                DataType::Binary,
            );
            return wrap(
                bytes,
                ScalarFunction::RawText(charset, collation),
                DataType::Utf8,
            );
        }
        if matches!(function, ScalarFunction::Concat | ScalarFunction::ConcatWs)
            && args.iter().any(|argument| {
                matches!(
                    argument.kind,
                    BoundExprKind::Scalar {
                        function: ScalarFunction::RawText(_, _),
                        ..
                    }
                )
            })
        {
            let args = args
                .iter()
                .cloned()
                .map(|argument| {
                    if character_set(&argument) == charset {
                        encoded(argument)
                    } else {
                        wrap(
                            argument,
                            ScalarFunction::EncodeText(charset),
                            DataType::Binary,
                        )
                    }
                })
                .collect();
            let bytes = BoundExpr {
                nullable: expression.nullable,
                data_type: Some(DataType::Binary),
                kind: BoundExprKind::Scalar {
                    function: *function,
                    args,
                },
            };
            return wrap(
                bytes,
                ScalarFunction::RawText(charset, collation),
                DataType::Utf8,
            );
        }
    }
    wrap(
        expression,
        ScalarFunction::TextCharset(charset, collation),
        DataType::Utf8,
    )
}

pub(crate) fn wrap(
    expression: BoundExpr,
    function: ScalarFunction,
    data_type: DataType,
) -> BoundExpr {
    BoundExpr {
        nullable: expression.nullable,
        data_type: Some(data_type),
        kind: BoundExprKind::Scalar {
            function,
            args: vec![expression],
        },
    }
}

pub(crate) fn encoded(expression: BoundExpr) -> BoundExpr {
    if expression.data_type != Some(DataType::Utf8) {
        return expression;
    }
    let charset = character_set(&expression);
    if let BoundExprKind::Scalar {
        function: ScalarFunction::RawText(_, _),
        args,
    } = &expression.kind
    {
        return args[0].clone();
    }
    if let BoundExprKind::Scalar {
        function: ScalarFunction::DecodeText(decoded),
        args,
    } = &expression.kind
        && charset == *decoded
    {
        // An introducer preserves bytes even when they cannot be decoded to
        // Unicode. A byte consumer need not decode and then encode them.
        let mut converted = wrap(
            args[0].clone(),
            ScalarFunction::PadTextBytes(charset),
            DataType::Binary,
        );
        converted.nullable = true;
        return converted;
    }
    if charset == CharacterSet::Utf8Mb4 {
        return expression;
    }
    wrap(
        expression,
        ScalarFunction::EncodeText(charset),
        DataType::Binary,
    )
}

pub(crate) fn scalar_charset(function: ScalarFunction, args: &[BoundExpr]) -> CharacterSet {
    use ScalarFunction as F;
    let text = |index: usize| {
        args.get(index)
            .filter(|arg| arg.data_type == Some(DataType::Utf8))
    };
    let subject = match function {
        F::Lower
        | F::Upper
        | F::Trim
        | F::TrimPattern { .. }
        | F::Substring
        | F::Left
        | F::Right
        | F::Replace
        | F::Insert
        | F::Reverse
        | F::SubstringIndex
        | F::Repeat
        | F::Lpad
        | F::Rpad
        | F::Collate { .. }
        | F::NullIf => text(0),
        F::Concat | F::ConcatWs | F::Coalesce | F::Greatest { .. } | F::Least { .. } => args
            .iter()
            .find(|arg| arg.data_type == Some(DataType::Utf8)),
        F::If => text(1).or_else(|| text(2)),
        F::Elt => text(1),
        F::JsonUnquote | F::JsonQuote | F::JsonPretty | F::JsonType => {
            return CharacterSet::Utf8Mb4;
        }
        _ => None,
    };
    subject.map_or_else(session_character_set, character_set)
}

pub(crate) fn byte_arguments(function: &mut ScalarFunction, args: &mut [BoundExpr]) {
    use ScalarFunction as F;
    let Some(first) = args.first_mut() else {
        return;
    };
    if *function == F::Ord && first.data_type == Some(DataType::Utf8) {
        *function = F::EncodedOrd(character_set(first));
    } else if matches!(
        function,
        F::Cast(DataType::Binary)
            | F::DeclaredCast {
                target: DataType::Binary,
                ..
            }
            | F::Hex
            | F::Length
            | F::Ascii
            | F::Sha1
            | F::Sha2
            | F::Md5
            | F::Crc32
            | F::ToBase64
    ) {
        *first = encoded(first.clone());
    }
}

/// Constant discovery crosses encoding operations without discarding the
/// representation used by non-constant execution.
pub(crate) fn literal_value(expression: &BoundExpr) -> Option<std::borrow::Cow<'_, Value>> {
    use std::borrow::Cow;
    match &expression.kind {
        BoundExprKind::Literal(value) => Some(Cow::Borrowed(value)),
        BoundExprKind::Scalar {
            function: ScalarFunction::TextCharset(charset, _),
            args,
        } => {
            let value = literal_value(&args[0])?;
            let Value::Utf8(text) = value.as_ref() else {
                return None;
            };
            Some(Cow::Owned(Value::Utf8(
                charset.decode(&charset.encode(text))?,
            )))
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::Utf8Prefix,
            args,
        } => {
            let value = literal_value(&args[0])?;
            let Value::Binary(bytes) = value.as_ref() else {
                return None;
            };
            Some(Cow::Owned(Value::Binary(
                pintail_types::utf8_prefix(bytes).to_vec(),
            )))
        }
        BoundExprKind::Scalar {
            function: ScalarFunction::DecodeText(charset),
            args,
        } => {
            let value = literal_value(&args[0])?;
            let Value::Binary(bytes) = value.as_ref() else {
                return None;
            };
            Some(Cow::Owned(Value::Utf8(charset.decode(bytes)?)))
        }
        _ => None,
    }
}

pub(crate) fn binary_pair(left: BoundExpr, right: BoundExpr) -> (BoundExpr, BoundExpr) {
    if left.data_type == Some(DataType::Binary) || right.data_type == Some(DataType::Binary) {
        (encoded(left), encoded(right))
    } else {
        (left, right)
    }
}
