//! Session lexical options, scoped to synchronous parsing work.
use std::cell::Cell;

/// The supported parsing flags in a session's SQL mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParseMode {
    /// Double quotes delimit identifiers instead of strings.
    pub ansi_quotes: bool,
    /// Double pipes concatenate strings instead of evaluating OR.
    pub pipes_as_concat: bool,
    /// Backslashes in strings remain literal characters.
    pub no_backslash_escapes: bool,
}

impl ParseMode {
    /// Extracts lexical flags from a comma-separated SQL mode value.
    #[must_use]
    pub fn from_sql_mode(value: &str) -> Self {
        let has = |name: &str| {
            value
                .split(',')
                .any(|mode| mode.trim().eq_ignore_ascii_case(name))
        };
        Self {
            ansi_quotes: has("ANSI_QUOTES"),
            pipes_as_concat: has("PIPES_AS_CONCAT"),
            no_backslash_escapes: has("NO_BACKSLASH_ESCAPES"),
        }
    }
}

thread_local! {
    static MODE: Cell<ParseMode> = Cell::new(ParseMode::default());
}

/// Returns the lexical mode of this synchronous parsing scope.
#[must_use]
pub fn session_parse_mode() -> ParseMode {
    MODE.get()
}

/// Runs synchronous work with a parsing mode, restoring it even after a panic.
pub fn with_parse_mode<T>(mode: ParseMode, work: impl FnOnce() -> T) -> T {
    struct Restore(ParseMode);
    impl Drop for Restore {
        fn drop(&mut self) {
            MODE.set(self.0);
        }
    }
    let _restore = Restore(MODE.replace(mode));
    work()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_statement;

    #[test]
    fn lexical_flags_are_scoped_and_preserve_operator_precedence() {
        let mode = ParseMode::from_sql_mode("ansi_quotes,pipes_as_concat,no_backslash_escapes");
        with_parse_mode(mode, || {
            let statement = parse_statement(r#"SELECT "name", 2 * 3 || 4, 'a\nb'"#).unwrap();
            let text = statement.to_string();
            assert!(text.contains(r#""name""#));
            assert!(text.contains("||"));
            assert!(text.contains(r"a\nb"));
            assert_eq!(session_parse_mode(), mode);
        });
        assert_eq!(session_parse_mode(), ParseMode::default());
        assert!(
            parse_statement("SELECT 0 || 1")
                .unwrap()
                .to_string()
                .contains("OR")
        );
    }
}
