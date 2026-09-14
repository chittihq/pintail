//! Session lexical options, scoped to synchronous parsing work.
use std::cell::Cell;

/// What `sql_mode` a `MySQL` 8.4 server hands a connection that never sets one.
///
/// This is the default in the literal sense: it is what a statement runs under
/// unless a session says otherwise, so it belongs beside `ParseMode` rather
/// than at one entry point. The wire path had it and the HTTP query path did
/// not, which let the same statement answer two ways depending on which door
/// it came in: `STR_TO_DATE('201506', '%Y%m')` is NULL under `NO_ZERO_IN_DATE`
/// and `2015-06-00` without it, and the HTTP path was answering the latter.
pub const DEFAULT_SQL_MODE: &str = "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,\
NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION";

/// The supported parsing flags in a session's SQL mode.
// Each flag mirrors one independent `sql_mode` member.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParseMode {
    /// Double quotes delimit identifiers instead of strings.
    pub ansi_quotes: bool,
    /// REAL declarations and casts use single precision.
    pub real_as_float: bool,
    /// Ungrouped columns may read a representative row when full grouping is disabled.
    pub permissive_grouping: bool,
    /// Double pipes concatenate strings instead of evaluating OR.
    pub pipes_as_concat: bool,
    /// Backslashes in strings remain literal characters.
    pub no_backslash_escapes: bool,
    /// Subtraction is signed even over unsigned operands.
    pub no_unsigned_subtraction: bool,
    /// Function-name lexing consumes whitespace after identifier tokens.
    pub ignore_space: bool,
    /// NOT binds with unary operators rather than comparisons.
    pub high_not_precedence: bool,
    /// Date parsing rejects any zero date component.
    pub no_zero_date: bool,
    /// Date parsing rejects zero month/day when the year is nonzero.
    pub no_zero_in_date: bool,
    /// Calendar casts accept day-of-month combinations outside the civil calendar.
    pub allow_invalid_dates: bool,
    /// Invalid character conversion returns NULL instead of a valid prefix.
    pub strict: bool,
    /// Temporal conversions discard excess fractional digits instead of rounding.
    pub time_truncate_fractional: bool,
}

impl Default for ParseMode {
    /// A server's own default mode, not an empty one. Deriving every field
    /// from `false` reads as neutral and is not: it is the permissive mode,
    /// which `MySQL` stopped shipping long ago. `permissive_grouping` already
    /// had to be written inverted to compensate - the tell that the neutral
    /// reading was wrong - and each remaining flag had the same gap, silently,
    /// on every path that never installed a session mode.
    fn default() -> Self {
        Self::from_sql_mode(DEFAULT_SQL_MODE)
    }
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
            ansi_quotes: has("ANSI_QUOTES") || has("ANSI"),
            real_as_float: has("REAL_AS_FLOAT") || has("ANSI"),
            permissive_grouping: !has("ONLY_FULL_GROUP_BY") && !has("ANSI"),
            pipes_as_concat: has("PIPES_AS_CONCAT") || has("ANSI"),
            no_backslash_escapes: has("NO_BACKSLASH_ESCAPES"),
            no_unsigned_subtraction: has("NO_UNSIGNED_SUBTRACTION"),
            ignore_space: has("IGNORE_SPACE") || has("ANSI"),
            high_not_precedence: has("HIGH_NOT_PRECEDENCE"),
            no_zero_date: has("NO_ZERO_DATE"),
            no_zero_in_date: has("NO_ZERO_IN_DATE"),
            allow_invalid_dates: has("ALLOW_INVALID_DATES"),
            strict: has("STRICT_TRANS_TABLES") || has("STRICT_ALL_TABLES") || has("TRADITIONAL"),
            time_truncate_fractional: has("TIME_TRUNCATE_FRACTIONAL"),
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
