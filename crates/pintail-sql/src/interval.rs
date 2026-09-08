//! Lower literal compound intervals before grammar parsing, retaining spans.
use sqlparser::tokenizer::{Token, TokenWithSpan, Word};

pub(crate) fn rewrite(tokens: &mut [TokenWithSpan]) {
    let significant = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            (!matches!(token.token, Token::Whitespace(_))).then_some(index)
        })
        .collect::<Vec<_>>();
    for indexes in significant.windows(3) {
        let [keyword, literal, qualifier] = indexes else {
            continue;
        };
        if !matches!(&tokens[*keyword].token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("INTERVAL"))
        {
            continue;
        }
        let Token::Word(word) = &tokens[*qualifier].token else {
            continue;
        };
        if word.quote_style.is_some() {
            continue;
        }
        let Some((weights, unit)) = qualifier_weights(&word.value) else {
            continue;
        };
        let value = match &tokens[*literal].token {
            Token::SingleQuotedString(value)
            | Token::DoubleQuotedString(value)
            | Token::Number(value, _) => quantity(value, weights),
            Token::Word(word) if word.value.eq_ignore_ascii_case("NULL") => None,
            _ => continue,
        };
        tokens[*literal].token = value.map_or_else(
            || word_token("NULL"),
            |value| Token::Number(value.to_string(), false),
        );
        tokens[*qualifier].token = word_token(unit);
    }
}

fn word_token(value: &str) -> Token {
    Token::Word(Word {
        value: value.to_owned(),
        quote_style: None,
        keyword: sqlparser::keywords::ALL_KEYWORDS
            .binary_search(&value)
            .map_or(sqlparser::keywords::Keyword::NoKeyword, |index| {
                sqlparser::keywords::ALL_KEYWORDS_INDEX[index]
            }),
    })
}

fn qualifier_weights(unit: &str) -> Option<(&'static [i64], &'static str)> {
    match unit.to_ascii_uppercase().as_str() {
        "YEAR_MONTH" => Some((&[12, 1], "MONTH")),
        "DAY_HOUR" => Some((&[86400, 3600], "SECOND")),
        "DAY_MINUTE" => Some((&[86400, 3600, 60], "SECOND")),
        "DAY_SECOND" => Some((&[86400, 3600, 60, 1], "SECOND")),
        "HOUR_MINUTE" => Some((&[3600, 60], "SECOND")),
        "HOUR_SECOND" => Some((&[3600, 60, 1], "SECOND")),
        "MINUTE_SECOND" => Some((&[60, 1], "SECOND")),
        _ => None,
    }
}

fn quantity(literal: &str, weights: &[i64]) -> Option<i64> {
    let literal = literal.trim_start();
    let negative = literal.starts_with('-');
    // Fields are digit runs separated by punctuation. Missing fields are
    // leading zeroes. A dot is another separator for these non-microsecond
    // qualifiers, rather than an implicit fractional-second extension.
    let fields = literal
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() > weights.len() {
        return None;
    }
    let mut total = 0_i64;
    for (field, weight) in fields.iter().zip(&weights[weights.len() - fields.len()..]) {
        total = total.checked_add(field.parse::<i64>().ok()?.checked_mul(*weight)?)?;
    }
    if negative {
        total.checked_neg()
    } else {
        Some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::{qualifier_weights, quantity};
    use crate::parse_statement;

    #[test]
    fn literal_fields_align_right_and_keep_the_sign() {
        for (unit, literal, expected) in [
            ("YEAR_MONTH", "1-2", 14),
            ("YEAR_MONTH", "-1-2", -14),
            ("DAY_SECOND", "3 4:00:00", 273_600),
            ("DAY_SECOND", "1:10", 70),
            ("DAY_HOUR", "2", 7200),
            ("DAY_MINUTE", "2:3", 7380),
            ("HOUR_MINUTE", "+1:2", 3720),
            ("HOUR_SECOND", "1:2:3", 3723),
            ("MINUTE_SECOND", "1.2", 62),
            ("YEAR_MONTH", "", 0),
        ] {
            let (weights, _) = qualifier_weights(unit).unwrap();
            assert_eq!(quantity(literal, weights), Some(expected));
            parse_statement(&format!(
                "SELECT DATE_ADD('2024-01-01', INTERVAL '{literal}' {unit})"
            ))
            .unwrap();
        }
        let (weights, _) = qualifier_weights("DAY_SECOND").unwrap();
        assert_eq!(quantity("1 2:3:4.5", weights), None);
        assert_eq!(quantity("999999999999999999999999", weights), None);
    }

    #[test]
    fn rewriting_respects_comments_strings_and_original_locations() {
        let sql = "SELECT 'INTERVAL 1 YEAR_MONTH',\n DATE_ADD('2024-01-01', INTERVAL /* units */ '1-2' YEAR_MONTH) AS shifted";
        let parsed = parse_statement(sql).unwrap().to_string();
        assert!(parsed.contains("'INTERVAL 1 YEAR_MONTH'"));
        assert!(parsed.contains("INTERVAL 14 MONTH"));
        assert!(parse_statement("SELECT DATE_ADD('2024-01-01', INTERVAL NULL DAY_SECOND)").is_ok());
    }
}
