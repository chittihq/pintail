//! Statement boundaries for text-protocol requests.

/// Returns the first nonempty statement and its remaining request bytes.
/// Quotes and comments shield semicolons. Parsing is deferred until execution
/// so an error in a later statement does not prevent earlier statements from
/// running. The caller supplies the current session's escape mode each time.
#[must_use]
pub fn first_statement(sql: &[u8], mode: crate::ParseMode) -> Option<(&[u8], &[u8])> {
    let mut at = 0;
    let mut start = 0;
    let mut significant = false;
    while at < sql.len() {
        match sql[at] {
            byte if byte.is_ascii_whitespace() => at += 1,
            b';' => {
                if significant {
                    return Some((&sql[start..at], &sql[at + 1..]));
                }
                at += 1;
                start = at;
            }
            b'#' => {
                while at < sql.len() && sql[at] != b'\n' {
                    at += 1;
                }
            }
            b'-' if sql.get(at + 1) == Some(&b'-')
                && sql.get(at + 2).is_none_or(u8::is_ascii_whitespace) =>
            {
                while at < sql.len() && sql[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if sql.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < sql.len() && &sql[at..at + 2] != b"*/" {
                    at += 1;
                }
                if at + 1 >= sql.len() {
                    return Some((&sql[start..], &[]));
                }
                at += 2;
            }
            quote @ (b'\'' | b'"' | b'`') => {
                significant = true;
                at += 1;
                while at < sql.len() {
                    if sql[at] == b'\\'
                        && !mode.no_backslash_escapes
                        && quote != b'`'
                        && !(quote == b'"' && mode.ansi_quotes)
                    {
                        at = (at + 2).min(sql.len());
                    } else if sql[at] == quote {
                        at += 1;
                        if sql.get(at) == Some(&quote) {
                            at += 1;
                        } else {
                            break;
                        }
                    } else {
                        at += 1;
                    }
                }
            }
            _ => {
                significant = true;
                at += 1;
            }
        }
    }
    significant.then_some((&sql[start..], &[]))
}

#[cfg(test)]
mod tests {
    use super::first_statement;

    #[test]
    fn semicolons_in_quotes_and_comments_do_not_split_statements() {
        let sql = b"SELECT ';', \";\", `a;b`, 'it''s;ok' /* ; */; -- ;\n SELECT 2; # tail";
        let (first, rest) = first_statement(sql, crate::ParseMode::default()).unwrap();
        assert!(first.ends_with(b"/* ; */"));
        let (second, rest) = first_statement(rest, crate::ParseMode::default()).unwrap();
        assert!(second.ends_with(b"SELECT 2"));
        assert!(first_statement(rest, crate::ParseMode::default()).is_none());
        assert!(first_statement(b"; /* empty */ ;", crate::ParseMode::default()).is_none());
    }

    #[test]
    fn later_malformed_statements_are_left_for_the_parser() {
        let (first, rest) = first_statement(
            b"SET @x=1; SELECT 'unterminated",
            crate::ParseMode::default(),
        )
        .unwrap();
        assert_eq!(first, b"SET @x=1");
        assert_eq!(
            first_statement(rest, crate::ParseMode::default())
                .unwrap()
                .0,
            b" SELECT 'unterminated"
        );
        let sql = b"SELECT 'x\\'; SELECT 2;";
        assert!(
            first_statement(sql, crate::ParseMode::default())
                .unwrap()
                .1
                .is_empty()
        );
        assert_eq!(
            first_statement(
                sql,
                crate::ParseMode {
                    no_backslash_escapes: true,
                    ..Default::default()
                }
            )
            .unwrap()
            .1,
            b" SELECT 2;"
        );
    }
}
