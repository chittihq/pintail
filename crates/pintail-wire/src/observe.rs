//! What the wire endpoint records about the sessions it serves.
//!
//! Before this it recorded almost nothing: one debug line when a connection
//! closed and one error line when it failed. A client could authenticate with
//! a valid key, read every row of every table in its scope, and disconnect,
//! leaving no record of who connected or what they ran. The HTTP surface has
//! logged method, path, status and duration per request for a while; the
//! `MySQL` surface had nothing comparable, which matters more now that the port
//! is reachable from outside the host.
//!
//! # Why statements are normalized
//!
//! A statement carries its literals, and a literal is a row value:
//! `WHERE email = 'someone@example.com'` puts a real person's address into
//! whatever consumes the log. The crate's own rule is that no log line carries
//! row values, and a query log is the most tempting place to break it.
//!
//! So `info` records the *shape* - literals replaced, so two queries differing
//! only in their constants read identically - which answers "what is this
//! client doing" and "which query is slow" without exporting data. The full
//! statement is emitted at `debug`, where an operator has deliberately turned
//! on verbose logging for a diagnosis and accepted what that means.

/// Replaces literal values in a statement with `?`.
///
/// Deliberately a lexical pass rather than a parse: it runs on every query, it
/// must never fail, and it must never be the reason a query is refused. An
/// unparseable statement still yields a usable shape.
#[must_use]
pub(crate) fn digest(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            // String literals, single or double quoted, with escapes and
            // doubled-quote escaping both consumed.
            quote @ ('\'' | '"') => {
                out.push('?');
                let mut escaped = false;
                while let Some(inner) = chars.next() {
                    if escaped {
                        escaped = false;
                    } else if inner == '\\' {
                        escaped = true;
                    } else if inner == quote {
                        // A doubled quote continues the same literal.
                        if chars.peek() == Some(&quote) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            // Numbers, including decimals and exponents. Only when they start
            // a token, so identifiers like `orders_2023` keep their name.
            digit if digit.is_ascii_digit() && !ends_with_identifier_char(&out) => {
                out.push('?');
                while chars
                    .peek()
                    .is_some_and(|next| next.is_ascii_digit() || matches!(next, '.' | 'e' | 'E'))
                {
                    chars.next();
                }
            }
            other => out.push(other),
        }
    }
    // Collapsed so formatting differences do not read as different queries.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ends_with_identifier_char(out: &str) -> bool {
    out.chars()
        .next_back()
        .is_some_and(|last| last.is_alphanumeric() || last == '_' || last == '.')
}

/// Truncates a digest so one pathological statement cannot dominate a log.
#[must_use]
pub(crate) fn truncated(digest: &str, limit: usize) -> String {
    if digest.chars().count() <= limit {
        return digest.to_owned();
    }
    let kept: String = digest.chars().take(limit).collect();
    format!("{kept}… (+{} chars)", digest.chars().count() - limit)
}

#[cfg(test)]
mod tests {
    use super::{digest, truncated};

    #[test]
    fn literals_are_replaced_so_row_values_never_reach_a_log() {
        assert_eq!(
            digest("SELECT * FROM users WHERE email = 'someone@example.com' AND age > 30"),
            "SELECT * FROM users WHERE email = ? AND age > ?",
        );
    }

    #[test]
    fn two_queries_differing_only_in_constants_share_a_shape() {
        assert_eq!(
            digest("SELECT a FROM t WHERE id = 1"),
            digest("SELECT a FROM t WHERE id = 99999"),
        );
    }

    #[test]
    fn identifiers_containing_digits_keep_their_name() {
        // orders_2023 is a table, not a literal; losing its name would make
        // the log useless for finding which table is slow.
        assert_eq!(
            digest("SELECT x FROM orders_2023"),
            "SELECT x FROM orders_2023"
        );
    }

    #[test]
    fn escaped_and_doubled_quotes_do_not_end_a_literal_early() {
        // If the scan ended at the inner quote, the rest of the address would
        // be emitted as if it were SQL.
        assert_eq!(digest("SELECT 'O''Brien' , 'a\\'b'"), "SELECT ? , ?");
    }

    #[test]
    fn whitespace_is_collapsed_so_formatting_is_not_a_new_query() {
        assert_eq!(digest("SELECT  a\n  FROM   t"), "SELECT a FROM t");
    }

    #[test]
    fn a_long_statement_is_truncated_with_its_size_named() {
        let long = digest(&format!("SELECT {}", "a,".repeat(200)));
        let short = truncated(&long, 40);
        assert!(short.len() < long.len());
        assert!(short.contains('+'), "the reader is told how much was cut");
    }
}
