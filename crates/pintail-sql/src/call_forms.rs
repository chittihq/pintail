//! Respell call and cast forms the grammar does not read, before parsing.
//!
//! Each form here is another way to write something the grammar already
//! reads, so the tokens are rearranged into that spelling and keep their
//! spans:
//!
//! - `CHAR(n, ... USING cs)` is `CONVERT(CHAR(n, ...) USING cs)`, or the
//!   arguments converted one by one where `cs` has fixed wide units;
//! - `CAST(x AS CHAR(n) CHARACTER SET cs)` is `LEFT(CONVERT(x USING cs), n)`,
//!   with `CHARSET`, `ASCII`, `UNICODE`, `BYTE`, `NCHAR` and `NATIONAL CHAR`
//!   naming a character set the same way, a trailing `BINARY` choosing that
//!   set's binary collation, and `CONVERT(x, CHAR ...)` taking every suffix
//!   the cast does;
//! - `CAST(x AS SIGNED INT)` is `CAST(x AS SIGNED)`;
//! - `TRIM(BOTH FROM x)` trims spaces;
//! - `TABLE t` is `SELECT * FROM t`;
//! - `column -> 'path'` and `column ->> 'path'` are one operand, so they are
//!   wrapped in parentheses: the grammar reads the arrow looser than the
//!   operators around it;
//! - `MID(s FROM n FOR m)` is `SUBSTRING(s FROM n FOR m)`;
//! - `UTC_DATE`, `UTC_TIME` and `UTC_TIMESTAMP` written bare are the calls
//!   with no arguments;
//! - `x REGEXP NULL` is `x REGEXP (NULL)`;
//! - `WEIGHT_STRING(x AS CHAR(n))` becomes a call under an internal name,
//!   which the binder answers for by the name the statement wrote.
use std::collections::BTreeMap;

use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::interval::{significant, word_token};

/// The names `WEIGHT_STRING(x AS CHAR(n))` and `WEIGHT_STRING(x AS
/// BINARY(n))` are parsed under: the value, then the length.
pub(crate) const WEIGHT_STRING_CHAR: &str = "PINTAIL_WEIGHT_STRING_CHAR";
pub(crate) const WEIGHT_STRING_BINARY: &str = "PINTAIL_WEIGHT_STRING_BINARY";

/// Token changes against one token list, by position in that list.
#[derive(Default)]
pub(crate) struct Edits {
    before: BTreeMap<usize, Vec<Token>>,
    replace: BTreeMap<usize, Vec<Token>>,
    after: BTreeMap<usize, Vec<Token>>,
}

impl Edits {
    pub(crate) fn is_empty(&self) -> bool {
        self.before.is_empty() && self.replace.is_empty() && self.after.is_empty()
    }

    pub(crate) fn before(&mut self, at: usize, tokens: impl IntoIterator<Item = Token>) {
        self.before.entry(at).or_default().extend(tokens);
    }

    pub(crate) fn after(&mut self, at: usize, tokens: impl IntoIterator<Item = Token>) {
        self.after.entry(at).or_default().extend(tokens);
    }

    pub(crate) fn replace(&mut self, at: usize, tokens: impl IntoIterator<Item = Token>) {
        self.replace.insert(at, tokens.into_iter().collect());
    }

    pub(crate) fn remove(&mut self, at: usize) {
        self.replace.insert(at, Vec::new());
    }

    /// Every added token takes the span of the token it was placed at.
    pub(crate) fn apply(mut self, tokens: &mut Vec<TokenWithSpan>) {
        let original = std::mem::take(tokens);
        for (index, token) in original.into_iter().enumerate() {
            let span = token.span;
            let place = |list: Option<Vec<Token>>, tokens: &mut Vec<TokenWithSpan>| {
                for added in list.into_iter().flatten() {
                    tokens.push(TokenWithSpan::new(added, span));
                }
            };
            place(self.before.remove(&index), tokens);
            match self.replace.remove(&index) {
                Some(replacement) => place(Some(replacement), tokens),
                None => tokens.push(token),
            }
            place(self.after.remove(&index), tokens);
        }
    }
}

pub(crate) fn is_word(token: &Token, name: &str) -> bool {
    matches!(token, Token::Word(word)
        if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(name))
}

fn is_any_word(token: &Token, names: &[&str]) -> bool {
    names.iter().any(|name| is_word(token, name))
}

/// The position in `significant` of the parenthesis closing the one at
/// `open`.
fn closing(tokens: &[TokenWithSpan], significant: &[usize], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (at, &index) in significant.iter().enumerate().skip(open) {
        match tokens[index].token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The last position directly inside the parentheses `open..close` whose
/// token satisfies `wanted`.
fn last_inside(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    open: usize,
    close: usize,
    wanted: impl Fn(&Token) -> bool,
) -> Option<usize> {
    let mut depth = 0_usize;
    let mut found = None;
    for at in open + 1..close {
        let token = &tokens[significant[at]].token;
        match token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && wanted(token) => found = Some(at),
            _ => {}
        }
    }
    found
}

pub(crate) fn rewrite(tokens: &mut Vec<TokenWithSpan>) {
    const TRIGGERS: [&str; 6] = ["CHAR", "NCHAR", "CAST", "CONVERT", "TRIM", "TABLE"];
    const WEIGHT: &str = "WEIGHT_STRING";
    if !tokens.iter().any(|token| {
        is_any_word(&token.token, &TRIGGERS)
            || is_word(&token.token, WEIGHT)
            || is_any_word(&token.token, &UTC_CLOCKS)
            || is_word(&token.token, "MID")
            || is_any_word(&token.token, &["REGEXP", "RLIKE"])
            || matches!(token.token, Token::Arrow | Token::LongArrow)
    }) {
        return;
    }
    // One form at a time: an edit moves every later position, and a cast
    // may sit inside another.
    for _ in 0..256 {
        let significant = significant(tokens);
        let Some(edits) = (0..significant.len()).find_map(|at| {
            let mut edits = Edits::default();
            call_form(tokens, &significant, at, &mut edits);
            (!edits.is_empty()).then_some(edits)
        }) else {
            return;
        };
        edits.apply(tokens);
    }
}

fn call_form(tokens: &[TokenWithSpan], significant: &[usize], at: usize, edits: &mut Edits) {
    let token = |position: usize| significant.get(position).map(|&index| &tokens[index].token);
    let Some(head) = token(at) else {
        return;
    };
    if is_word(head, "TABLE") {
        table_statement(tokens, significant, at, edits);
        return;
    }
    if matches!(head, Token::Arrow | Token::LongArrow) {
        json_path(tokens, significant, at, edits);
        return;
    }
    if is_any_word(head, &["REGEXP", "RLIKE"]) {
        // A bare NULL pattern is not read after these two; in parentheses
        // it is.
        if token(at + 1).is_some_and(|pattern| is_word(pattern, "NULL")) {
            edits.before(significant[at + 1], [Token::LParen]);
            edits.after(significant[at + 1], [Token::RParen]);
        }
        return;
    }
    if is_any_word(head, &UTC_CLOCKS) {
        // Reserved words, so a bare one is never a column.
        if token(at + 1) != Some(&Token::LParen)
            && at.checked_sub(1).and_then(token) != Some(&Token::Period)
        {
            edits.after(significant[at], [Token::LParen, Token::RParen]);
        }
        return;
    }
    if token(at + 1) != Some(&Token::LParen) {
        return;
    }
    let Some(close) = closing(tokens, significant, at + 1) else {
        return;
    };
    if is_word(head, "MID") {
        if last_inside(tokens, significant, at + 1, close, |inner| {
            is_word(inner, "FROM")
        })
        .is_some()
        {
            edits.replace(significant[at], [word_token("SUBSTRING")]);
        }
    } else if is_word(head, "WEIGHT_STRING") {
        weight_string(tokens, significant, at, close, edits);
    } else if is_word(head, "CHAR") {
        char_using(tokens, significant, at, close, edits);
    } else if is_word(head, "CAST") || is_word(head, "CONVERT") {
        cast_type(tokens, significant, at, close, edits);
    } else if is_word(head, "TRIM")
        && token(at + 2).is_some_and(|side| is_any_word(side, &["BOTH", "LEADING", "TRAILING"]))
        && token(at + 3).is_some_and(|from| is_word(from, "FROM"))
    {
        edits.before(
            significant[at + 3],
            [Token::SingleQuotedString(" ".to_owned())],
        );
    }
}

/// The clock functions the grammar reads only with parentheses.
const UTC_CLOCKS: [&str; 3] = ["UTC_DATE", "UTC_TIME", "UTC_TIMESTAMP"];

/// `column -> 'path'` or `column ->> 'path'` at the arrow `at`: the column
/// may be qualified and the path is a string literal, which is all `MySQL`
/// accepts on either side. Already inside its own parentheses it is left
/// alone, which is also what ends the rewriting.
fn json_path(tokens: &[TokenWithSpan], significant: &[usize], at: usize, edits: &mut Edits) {
    let token = |position: usize| significant.get(position).map(|&index| &tokens[index].token);
    let is_name = |position: usize| matches!(token(position), Some(Token::Word(_)));
    if !matches!(
        token(at + 1),
        Some(Token::SingleQuotedString(_) | Token::DoubleQuotedString(_))
    ) {
        return;
    }
    let Some(mut start) = at.checked_sub(1).filter(|&name| is_name(name)) else {
        return;
    };
    while start >= 2 && token(start - 1) == Some(&Token::Period) && is_name(start - 2) {
        start -= 2;
    }
    let wrapped = start.checked_sub(1).and_then(token) == Some(&Token::LParen)
        && token(at + 2) == Some(&Token::RParen);
    if !wrapped {
        edits.before(significant[start], [Token::LParen]);
        edits.after(significant[at + 1], [Token::RParen]);
    }
}

/// `TABLE t` where a query may start: first in the statement, directly
/// inside parentheses, or after a set operator.
fn table_statement(tokens: &[TokenWithSpan], significant: &[usize], at: usize, edits: &mut Edits) {
    let token = |position: usize| significant.get(position).map(|&index| &tokens[index].token);
    let starts = match at.checked_sub(1).and_then(token) {
        None | Some(Token::SemiColon | Token::LParen) => true,
        Some(previous) => {
            is_any_word(previous, &["UNION", "EXCEPT", "INTERSECT"])
                || (is_any_word(previous, &["ALL", "DISTINCT"])
                    && at
                        .checked_sub(2)
                        .and_then(token)
                        .is_some_and(|set| is_any_word(set, &["UNION", "EXCEPT", "INTERSECT"])))
        }
    };
    if starts
        && matches!(token(at + 1), Some(Token::Word(_)))
        && token(at + 2) != Some(&Token::LParen)
    {
        edits.replace(
            significant[at],
            [word_token("SELECT"), Token::Mul, word_token("FROM")],
        );
    }
}

fn weight_string(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    at: usize,
    close: usize,
    edits: &mut Edits,
) {
    let token = |position: usize| &tokens[significant[position]].token;
    let Some(split) = last_inside(tokens, significant, at + 1, close, |token| {
        is_word(token, "AS")
    }) else {
        return;
    };
    // AS, the type, an opening parenthesis, the length, its closing one.
    if split + 5 != close
        || token(split + 2) != &Token::LParen
        || !matches!(token(split + 3), Token::Number(..))
    {
        return;
    }
    let name = if is_word(token(split + 1), "CHAR") {
        WEIGHT_STRING_CHAR
    } else if is_word(token(split + 1), "BINARY") {
        WEIGHT_STRING_BINARY
    } else {
        return;
    };
    edits.replace(significant[at], [word_token(name)]);
    edits.replace(significant[split], [Token::Comma]);
    for position in [split + 1, split + 2, split + 4] {
        edits.remove(significant[position]);
    }
}

fn char_using(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    at: usize,
    close: usize,
    edits: &mut Edits,
) {
    let Some(using) = last_inside(tokens, significant, at + 1, close, |token| {
        is_word(token, "USING")
    }) else {
        return;
    };
    if using + 2 != close {
        return;
    }
    let Token::Word(charset) = &tokens[significant[using + 1]].token else {
        return;
    };
    let charset = charset.value.to_ascii_lowercase();
    let mut commas = Vec::new();
    let mut depth = 0_usize;
    for &index in &significant[at + 2..using] {
        match tokens[index].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => commas.push(index),
            _ => {}
        }
    }
    let open = [word_token("CONVERT"), Token::LParen];
    // In a character set of fixed two- or four-byte units every argument
    // fills whole units of its own, so each is converted apart and the
    // results joined. Units of a surrogate pair may come from two
    // arguments, which is left to the grammar to refuse.
    let apart = !commas.is_empty() && matches!(charset.as_str(), "ucs2" | "utf32");
    if !commas.is_empty() && matches!(charset.as_str(), "utf16" | "utf16le") {
        return;
    }
    if apart {
        edits.before(
            significant[at],
            [word_token("CONCAT"), Token::LParen]
                .into_iter()
                .chain(open.clone()),
        );
        for comma in commas {
            edits.replace(
                comma,
                [
                    Token::RParen,
                    word_token("USING"),
                    word_token(&charset),
                    Token::RParen,
                    Token::Comma,
                ]
                .into_iter()
                .chain(open.clone())
                .chain([word_token("CHAR"), Token::LParen]),
            );
        }
        edits.after(significant[close], [Token::RParen]);
    } else {
        edits.before(significant[at], open);
    }
    edits.before(significant[using], [Token::RParen]);
}

/// The character type of a cast: `[NATIONAL] CHAR [(n)]` or `NCHAR [(n)]`
/// and what follows it.
struct CharacterType {
    /// Position of the length, when one is written.
    length: Option<usize>,
    charset: Option<String>,
    binary: bool,
}

fn character_type(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    start: usize,
    close: usize,
) -> Option<CharacterType> {
    let token = |position: usize| &tokens[significant[position]].token;
    let mut at = start;
    let mut parsed = CharacterType {
        length: None,
        charset: None,
        binary: false,
    };
    if at < close && is_word(token(at), "NATIONAL") {
        parsed.charset = Some("utf8mb3".to_owned());
        at += 1;
    }
    if at >= close {
        return None;
    }
    if is_word(token(at), "NCHAR") && parsed.charset.is_none() {
        parsed.charset = Some("utf8mb3".to_owned());
    } else if !is_word(token(at), "CHAR") {
        return None;
    }
    at += 1;
    if at + 2 < close
        && token(at) == &Token::LParen
        && matches!(token(at + 1), Token::Number(..))
        && token(at + 2) == &Token::RParen
    {
        parsed.length = Some(at + 1);
        at += 3;
    }
    let plain = parsed.charset.is_none();
    while at < close {
        let word = token(at);
        let named = if is_word(word, "CHARSET") {
            at += 1;
            true
        } else if is_any_word(word, &["CHARACTER", "CHAR"])
            && at + 1 < close
            && is_word(token(at + 1), "SET")
        {
            at += 2;
            true
        } else {
            false
        };
        if named {
            let Token::Word(name) = (at < close).then(|| token(at))? else {
                return None;
            };
            parsed.charset = Some(name.value.to_ascii_lowercase());
        } else if is_word(word, "ASCII") {
            parsed.charset = Some("latin1".to_owned());
        } else if is_word(word, "UNICODE") {
            parsed.charset = Some("ucs2".to_owned());
        } else if is_word(word, "BYTE") {
            parsed.charset = Some("binary".to_owned());
        } else if is_word(word, "BINARY") {
            parsed.binary = true;
        } else {
            return None;
        }
        at += 1;
    }
    (!plain || parsed.charset.is_some() || parsed.binary).then_some(parsed)
}

fn cast_type(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    at: usize,
    close: usize,
    edits: &mut Edits,
) {
    let token = |position: usize| &tokens[significant[position]].token;
    let cast = is_word(token(at), "CAST");
    let Some(split) = last_inside(tokens, significant, at + 1, close, |token| {
        if cast {
            is_word(token, "AS")
        } else {
            matches!(token, Token::Comma) || is_word(token, "USING")
        }
    }) else {
        return;
    };
    if is_word(token(split), "USING") || split == at + 2 {
        return;
    }
    if split + 3 == close
        && is_any_word(token(split + 1), &["SIGNED", "UNSIGNED"])
        && is_any_word(token(split + 2), &["INT", "INTEGER"])
    {
        edits.remove(significant[split + 2]);
        return;
    }
    let Some(parsed) = character_type(tokens, significant, split + 1, close) else {
        return;
    };
    let length = parsed.length.map(|position| token(position).clone());
    let sized = |name: &str| {
        let mut written = vec![word_token(name)];
        if let Some(length) = length.clone() {
            written.extend([Token::LParen, length, Token::RParen]);
        }
        written
    };
    // The type is rewritten at its first token; the rest of it goes.
    for &index in &significant[split + 2..close] {
        edits.remove(index);
    }
    let type_at = significant[split + 1];
    let collation = match parsed.charset.as_deref() {
        // A character string of bytes is a byte string, padded to its length.
        Some("binary") => {
            edits.replace(type_at, sized("BINARY"));
            return;
        }
        Some(charset) => {
            let using = [word_token("USING"), word_token(charset)];
            if parsed.length.is_some() {
                edits.replace(significant[at], [word_token("LEFT")]);
                edits.after(significant[at + 1], [word_token("CONVERT"), Token::LParen]);
                edits.replace(
                    significant[split],
                    using.into_iter().chain([Token::RParen, Token::Comma]),
                );
                edits.replace(type_at, length);
            } else {
                edits.replace(significant[at], [word_token("CONVERT")]);
                edits.replace(significant[split], using);
                edits.remove(type_at);
            }
            format!("{charset}_bin")
        }
        None => {
            edits.replace(type_at, sized("CHAR"));
            let default = crate::text_charset::session_character_set().default_collation();
            format!("{}_bin", default.split('_').next().unwrap_or(default))
        }
    };
    if parsed.binary {
        edits.after(
            significant[close],
            [word_token("COLLATE"), word_token(&collation)],
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_statement;

    fn respelled(sql: &str) -> String {
        parse_statement(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .to_string()
    }

    #[test]
    fn json_paths_mid_and_bare_clocks_are_respelled() {
        for (sql, expected) in [
            ("SELECT 1 + j->'$.a'", "SELECT 1 + (j -> '$.a')"),
            (
                "SELECT t.j->>'$.a' LIKE 'p%'",
                "SELECT (t.j ->> '$.a') LIKE 'p%'",
            ),
            ("SELECT (j->'$.a') IS NULL", "SELECT (j -> '$.a') IS NULL"),
            ("SELECT f(j->'$.a')", "SELECT f(j -> '$.a')"),
            (
                "SELECT MID(a FROM 2 FOR 3)",
                "SELECT SUBSTRING(a FROM 2 FOR 3)",
            ),
            ("SELECT MID(a, 2, 3)", "SELECT MID(a, 2, 3)"),
            ("SELECT a REGEXP NULL", "SELECT a REGEXP (NULL)"),
            ("SELECT a NOT RLIKE NULL, 1", "SELECT a NOT RLIKE (NULL), 1"),
            (
                "SELECT UTC_DATE, utc_time, UTC_TIMESTAMP()",
                "SELECT UTC_DATE(), utc_time(), UTC_TIMESTAMP()",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }

    #[test]
    fn character_casts_read_as_conversions() {
        for (sql, expected) in [
            (
                "SELECT CAST(a AS CHAR CHARACTER SET latin1)",
                "SELECT CONVERT(a USING latin1)",
            ),
            (
                "SELECT cast(a as char(3) charset utf8mb4)",
                "SELECT LEFT(CONVERT(a USING utf8mb4), 3)",
            ),
            (
                "SELECT CAST(f(a, b) AS CHAR(2) ASCII)",
                "SELECT LEFT(CONVERT(f(a, b) USING latin1), 2)",
            ),
            (
                "SELECT CONVERT(a, CHAR(5) UNICODE)",
                "SELECT LEFT(CONVERT(a USING ucs2), 5)",
            ),
            (
                "SELECT CAST(a AS NATIONAL CHAR)",
                "SELECT CONVERT(a USING utf8mb3)",
            ),
            (
                "SELECT CAST(a AS NCHAR(4))",
                "SELECT LEFT(CONVERT(a USING utf8mb3), 4)",
            ),
            (
                "SELECT CAST(a AS CHAR(2) CHARSET binary)",
                "SELECT CAST(a AS BINARY(2))",
            ),
            ("SELECT CAST(a AS CHAR BYTE)", "SELECT CAST(a AS BINARY)"),
            (
                "SELECT CAST(a AS CHAR(2) BINARY) = 'x'",
                "SELECT CAST(a AS CHAR(2)) COLLATE utf8mb4_bin = 'x'",
            ),
            (
                "SELECT CAST(a AS CHAR CHARACTER SET latin1 BINARY)",
                "SELECT CONVERT(a USING latin1) COLLATE latin1_bin",
            ),
            (
                "SELECT CAST(CAST(a AS CHAR CHARSET latin1) AS CHAR(1) CHARSET utf8mb4)",
                "SELECT LEFT(CONVERT(CONVERT(a USING latin1) USING utf8mb4), 1)",
            ),
            (
                "SELECT CAST(a AS SIGNED INT), CAST(a AS UNSIGNED INTEGER), CONVERT(a, SIGNED INT)",
                "SELECT CAST(a AS SIGNED), CAST(a AS UNSIGNED), CONVERT(a, SIGNED)",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }

    #[test]
    fn plain_casts_and_types_are_left_alone() {
        for sql in [
            "SELECT CAST(a AS CHAR(3)), CAST(a AS CHAR), CONVERT(a, CHAR(2)), CONVERT(a USING latin1)",
            "SELECT CAST(a AS DECIMAL(5,2)), CAST(a AS BINARY(2)), CAST(a AS SIGNED)",
            "SELECT CHAR(65, 66), TRIM(BOTH 'x' FROM a), TRIM(a)",
        ] {
            assert_eq!(respelled(sql), sql, "{sql}");
        }
        assert!(parse_statement("CREATE TABLE t (a CHAR(3) CHARACTER SET latin1)").is_ok());
        assert!(parse_statement("DROP TABLE t").is_ok());
    }

    #[test]
    fn other_forms_take_their_plain_spelling() {
        for (sql, expected) in [
            (
                "SELECT CHAR(65, a USING latin1)",
                "SELECT CONVERT(CHAR(65, a) USING latin1)",
            ),
            (
                "SELECT CHAR(65, f(a, 1) USING ucs2), CHAR(a USING utf32)",
                "SELECT CONCAT(CONVERT(CHAR(65) USING ucs2), CONVERT(CHAR(f(a, 1)) USING ucs2)), CONVERT(CHAR(a) USING utf32)",
            ),
            (
                "SELECT TRIM(LEADING FROM a), trim(both from b)",
                "SELECT TRIM(LEADING ' ' FROM a), TRIM(BOTH ' ' FROM b)",
            ),
            ("TABLE t", "SELECT * FROM t"),
            (
                "SELECT a FROM (TABLE t) AS d WHERE a IN (TABLE u)",
                "SELECT a FROM (SELECT * FROM t) AS d WHERE a IN (SELECT * FROM u)",
            ),
            (
                "SELECT 1 UNION ALL TABLE t ORDER BY 1",
                "SELECT 1 UNION ALL SELECT * FROM t ORDER BY 1",
            ),
            (
                "SELECT WEIGHT_STRING(a AS CHAR(3)), weight_string(cast(a as char) as binary(2))",
                "SELECT PINTAIL_WEIGHT_STRING_CHAR(a, 3), PINTAIL_WEIGHT_STRING_BINARY(CAST(a AS CHAR), 2)",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }
}
