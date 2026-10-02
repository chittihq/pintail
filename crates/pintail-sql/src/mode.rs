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
        // One pass over the members: this runs several times for every
        // statement a connection sends, and asking the list once per flag
        // split it more than a dozen times each time.
        let mut mode = Self {
            ansi_quotes: false,
            real_as_float: false,
            permissive_grouping: true,
            pipes_as_concat: false,
            no_backslash_escapes: false,
            no_unsigned_subtraction: false,
            ignore_space: false,
            high_not_precedence: false,
            no_zero_date: false,
            no_zero_in_date: false,
            allow_invalid_dates: false,
            strict: false,
            time_truncate_fractional: false,
        };
        for member in value.split(',') {
            let member = member.trim();
            let is = |name: &str| member.eq_ignore_ascii_case(name);
            if is("ANSI") {
                mode.ansi_quotes = true;
                mode.real_as_float = true;
                mode.permissive_grouping = false;
                mode.pipes_as_concat = true;
                mode.ignore_space = true;
            } else if is("ANSI_QUOTES") {
                mode.ansi_quotes = true;
            } else if is("REAL_AS_FLOAT") {
                mode.real_as_float = true;
            } else if is("ONLY_FULL_GROUP_BY") {
                mode.permissive_grouping = false;
            } else if is("PIPES_AS_CONCAT") {
                mode.pipes_as_concat = true;
            } else if is("NO_BACKSLASH_ESCAPES") {
                mode.no_backslash_escapes = true;
            } else if is("NO_UNSIGNED_SUBTRACTION") {
                mode.no_unsigned_subtraction = true;
            } else if is("IGNORE_SPACE") {
                mode.ignore_space = true;
            } else if is("HIGH_NOT_PRECEDENCE") {
                mode.high_not_precedence = true;
            } else if is("NO_ZERO_DATE") {
                mode.no_zero_date = true;
            } else if is("NO_ZERO_IN_DATE") {
                mode.no_zero_in_date = true;
            } else if is("ALLOW_INVALID_DATES") {
                mode.allow_invalid_dates = true;
            } else if is("STRICT_TRANS_TABLES") || is("STRICT_ALL_TABLES") || is("TRADITIONAL") {
                mode.strict = true;
            } else if is("TIME_TRUNCATE_FRACTIONAL") {
                mode.time_truncate_fractional = true;
            }
        }
        mode
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
    fn every_member_sets_its_flag_however_the_list_is_written() {
        let none = ParseMode::from_sql_mode("");
        assert!(none.permissive_grouping && !none.strict && !none.ansi_quotes);
        for (member, set) in [
            (
                "ANSI_QUOTES",
                (|mode| mode.ansi_quotes) as fn(ParseMode) -> bool,
            ),
            ("REAL_AS_FLOAT", |mode| mode.real_as_float),
            ("ONLY_FULL_GROUP_BY", |mode| !mode.permissive_grouping),
            ("PIPES_AS_CONCAT", |mode| mode.pipes_as_concat),
            ("NO_BACKSLASH_ESCAPES", |mode| mode.no_backslash_escapes),
            ("NO_UNSIGNED_SUBTRACTION", |mode| {
                mode.no_unsigned_subtraction
            }),
            ("IGNORE_SPACE", |mode| mode.ignore_space),
            ("HIGH_NOT_PRECEDENCE", |mode| mode.high_not_precedence),
            ("NO_ZERO_DATE", |mode| mode.no_zero_date),
            ("NO_ZERO_IN_DATE", |mode| mode.no_zero_in_date),
            ("ALLOW_INVALID_DATES", |mode| mode.allow_invalid_dates),
            ("STRICT_TRANS_TABLES", |mode| mode.strict),
            ("STRICT_ALL_TABLES", |mode| mode.strict),
            ("TRADITIONAL", |mode| mode.strict),
            ("TIME_TRUNCATE_FRACTIONAL", |mode| {
                mode.time_truncate_fractional
            }),
        ] {
            assert!(!set(none), "{member} is off in an empty mode");
            for written in [
                member.to_owned(),
                member.to_ascii_lowercase(),
                format!("NO_ENGINE_SUBSTITUTION, {member} ,ERROR_FOR_DIVISION_BY_ZERO"),
            ] {
                let mode = ParseMode::from_sql_mode(&written);
                assert!(set(mode), "{written}");
                // And nothing else: every other member stays as it was.
                let alone = ParseMode::from_sql_mode(member);
                assert_eq!(mode, alone, "{written}");
            }
        }
        // A member is a whole name, never a part of one.
        assert_eq!(
            ParseMode::from_sql_mode("ANSI_QUOTESX,XPIPES_AS_CONCAT"),
            none
        );
        let ansi = ParseMode::from_sql_mode("ANSI");
        assert!(
            ansi.ansi_quotes
                && ansi.real_as_float
                && !ansi.permissive_grouping
                && ansi.pipes_as_concat
                && ansi.ignore_space
                && !ansi.strict
        );
    }

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
