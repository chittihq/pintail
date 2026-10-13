//! Lower compound intervals before grammar parsing, retaining spans.
//!
//! A literal quantity folds to one amount of a single unit. Any other
//! quantity - a column, a function call, arithmetic - is wrapped in the
//! internal function that reads its text the same way at run time, so
//! `INTERVAL CONCAT(h, ':', m) HOUR_MINUTE` becomes
//! `INTERVAL PINTAIL_INTERVAL_HOUR_MINUTE(CONCAT(h, ':', m)) SECOND`.
use sqlparser::tokenizer::{Token, TokenWithSpan, Word};

/// A compound interval qualifier: the fields its quantity is written in.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CompoundUnit {
    /// `YEAR_MONTH`, in months.
    YearMonth,
    /// `DAY_HOUR`, in seconds.
    DayHour,
    /// `DAY_MINUTE`, in seconds.
    DayMinute,
    /// `DAY_SECOND`, in seconds.
    DaySecond,
    /// `HOUR_MINUTE`, in seconds.
    HourMinute,
    /// `HOUR_SECOND`, in seconds.
    HourSecond,
    /// `MINUTE_SECOND`, in seconds.
    MinuteSecond,
    /// `DAY_MICROSECOND`, in microseconds.
    DayMicrosecond,
    /// `HOUR_MICROSECOND`, in microseconds.
    HourMicrosecond,
    /// `MINUTE_MICROSECOND`, in microseconds.
    MinuteMicrosecond,
    /// `SECOND_MICROSECOND`, in microseconds.
    SecondMicrosecond,
    /// Plain `MICROSECOND`: one field that counts microseconds one by one.
    /// The binder builds it over a whole-number amount; it has no qualifier
    /// spelling of its own and is never lowered from tokens.
    Microsecond,
}

const UNITS: [(&str, CompoundUnit); 11] = [
    ("YEAR_MONTH", CompoundUnit::YearMonth),
    ("DAY_HOUR", CompoundUnit::DayHour),
    ("DAY_MINUTE", CompoundUnit::DayMinute),
    ("DAY_SECOND", CompoundUnit::DaySecond),
    ("HOUR_MINUTE", CompoundUnit::HourMinute),
    ("HOUR_SECOND", CompoundUnit::HourSecond),
    ("MINUTE_SECOND", CompoundUnit::MinuteSecond),
    ("DAY_MICROSECOND", CompoundUnit::DayMicrosecond),
    ("HOUR_MICROSECOND", CompoundUnit::HourMicrosecond),
    ("MINUTE_MICROSECOND", CompoundUnit::MinuteMicrosecond),
    ("SECOND_MICROSECOND", CompoundUnit::SecondMicrosecond),
];

/// The internal function a non-literal quantity is wrapped in.
const FUNCTION_PREFIX: &str = "PINTAIL_INTERVAL_";

impl CompoundUnit {
    fn parse(qualifier: &str) -> Option<Self> {
        UNITS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(qualifier))
            .map(|(_, unit)| *unit)
    }

    /// The unit an internal function name stands for, when it is one.
    #[must_use]
    pub fn from_function_name(name: &str) -> Option<Self> {
        let prefix = name.get(..FUNCTION_PREFIX.len())?;
        if !prefix.eq_ignore_ascii_case(FUNCTION_PREFIX) {
            return None;
        }
        Self::parse(&name[FUNCTION_PREFIX.len()..])
    }

    fn name(self) -> &'static str {
        UNITS
            .iter()
            .find(|(_, unit)| *unit == self)
            .map_or("", |(name, _)| name)
    }

    /// The single unit a quantity of this qualifier is counted in. A
    /// microsecond qualifier counts seconds with a six-digit fraction, which
    /// is also what gives the result its six fractional digits.
    fn single_unit(self) -> &'static str {
        match self {
            Self::YearMonth => "MONTH",
            _ => "SECOND",
        }
    }

    /// Whether the qualifier's last field counts microseconds.
    #[must_use]
    pub fn counts_microseconds(self) -> bool {
        matches!(
            self,
            Self::DayMicrosecond
                | Self::HourMicrosecond
                | Self::MinuteMicrosecond
                | Self::SecondMicrosecond
                | Self::Microsecond
        )
    }

    /// A quantity from [`Self::quantity`] as the amount of the single unit:
    /// the count itself, or seconds with six fractional digits when the
    /// qualifier counts microseconds.
    #[must_use]
    pub fn render(self, quantity: i64) -> String {
        if !self.counts_microseconds() {
            return quantity.to_string();
        }
        let sign = if quantity < 0 { "-" } else { "" };
        let magnitude = quantity.unsigned_abs();
        format!(
            "{sign}{}.{:06}",
            magnitude / 1_000_000,
            magnitude % 1_000_000
        )
    }

    /// Each field's worth in the single unit, most significant first.
    fn weights(self) -> &'static [i64] {
        match self {
            Self::YearMonth => &[12, 1],
            Self::DayHour => &[86_400, 3_600],
            Self::DayMinute => &[86_400, 3_600, 60],
            Self::DaySecond => &[86_400, 3_600, 60, 1],
            Self::HourMinute => &[3_600, 60],
            Self::HourSecond => &[3_600, 60, 1],
            Self::MinuteSecond => &[60, 1],
            Self::DayMicrosecond => &[86_400_000_000, 3_600_000_000, 60_000_000, 1_000_000, 1],
            Self::HourMicrosecond => &[3_600_000_000, 60_000_000, 1_000_000, 1],
            Self::MinuteMicrosecond => &[60_000_000, 1_000_000, 1],
            Self::SecondMicrosecond => &[1_000_000, 1],
            Self::Microsecond => &[1],
        }
    }

    /// The amount a quantity written as `text` counts, in [`Self::single_unit`].
    ///
    /// Fields are digit runs separated by punctuation, aligned to the
    /// qualifier's least significant field; missing fields are leading
    /// zeroes and text with no digits is zero. A dot is another separator
    /// rather than an implicit fraction, except that a microsecond field is
    /// the digits of a fraction of a second: `5` is half a second, and only
    /// a field of six digits or more counts microseconds one by one. A plain
    /// `MICROSECOND` amount has no seconds field to be a fraction of and
    /// counts as written. More fields than the qualifier has is `None`,
    /// which `MySQL` answers as NULL.
    #[must_use]
    pub fn quantity(self, text: &str) -> Option<i64> {
        let weights = self.weights();
        let text = text.trim_start();
        let negative = text.starts_with('-');
        let fields = text
            .split(|ch: char| !ch.is_ascii_digit())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() > weights.len() {
            return None;
        }
        let fraction = self.counts_microseconds() && weights.len() > 1;
        let mut total = 0_i64;
        for (field, weight) in fields.iter().zip(&weights[weights.len() - fields.len()..]) {
            let mut value = field.parse::<i64>().ok()?;
            if fraction && *weight == 1 && field.len() < 6 {
                value = value.checked_mul(10_i64.pow(6 - u32::try_from(field.len()).ok()?))?;
            }
            total = total.checked_add(value.checked_mul(*weight)?)?;
        }
        if negative {
            total.checked_neg()
        } else {
            Some(total)
        }
    }
}

pub(crate) fn rewrite(tokens: &mut Vec<TokenWithSpan>) {
    name_range_function(tokens);
    fold_literals(tokens);
    wrap_expressions(tokens);
}

/// The name `INTERVAL(N, N1, N2, ...)` is parsed under.
pub(crate) const RANGE_FUNCTION: &str = "PINTAIL_RANGE_OF";

/// `INTERVAL(N, N1, N2, ...)` is a function - the number of listed values
/// not above `N` - and shares its first word with the interval of date
/// arithmetic. A comma directly inside the parentheses tells them apart:
/// `INTERVAL (1) DAY` is arithmetic, `INTERVAL(1, 2)` the function. The
/// function is renamed here so the grammar reads an ordinary call; the
/// token keeps its span, so the statement's text still names it.
fn name_range_function(tokens: &mut [TokenWithSpan]) {
    // Most statements have no INTERVAL at all, and pay only this look.
    if !tokens.iter().any(|token| is_interval(&token.token)) {
        return;
    }
    let significant = significant(tokens);
    for (at, &keyword) in significant.iter().enumerate() {
        if !is_interval(&tokens[keyword].token)
            || significant
                .get(at + 1)
                .is_none_or(|&next| tokens[next].token != Token::LParen)
        {
            continue;
        }
        let mut depth = 0_usize;
        let mut list = false;
        let mut closed = false;
        for &index in &significant[at + 1..] {
            match tokens[index].token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                Token::Comma if depth == 1 => list = true,
                _ => {}
            }
        }
        if list && closed {
            tokens[keyword].token = word_token(RANGE_FUNCTION);
        }
    }
}

fn significant(tokens: &[TokenWithSpan]) -> Vec<usize> {
    tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            (!matches!(token.token, Token::Whitespace(_))).then_some(index)
        })
        .collect()
}

fn is_interval(token: &Token) -> bool {
    matches!(token, Token::Word(word)
        if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("INTERVAL"))
}

fn compound_unit(token: &Token) -> Option<CompoundUnit> {
    match token {
        Token::Word(word) if word.quote_style.is_none() => CompoundUnit::parse(&word.value),
        _ => None,
    }
}

/// `INTERVAL <literal> <compound qualifier>` becomes one amount of a unit.
fn fold_literals(tokens: &mut [TokenWithSpan]) {
    for indexes in significant(tokens).windows(3) {
        let [keyword, literal, qualifier] = indexes else {
            continue;
        };
        if !is_interval(&tokens[*keyword].token) {
            continue;
        }
        let Some(unit) = compound_unit(&tokens[*qualifier].token) else {
            continue;
        };
        let value = match &tokens[*literal].token {
            Token::SingleQuotedString(value)
            | Token::DoubleQuotedString(value)
            | Token::Number(value, _) => unit.quantity(value),
            Token::Word(word) if word.value.eq_ignore_ascii_case("NULL") => None,
            _ => continue,
        };
        tokens[*literal].token = value.map_or_else(
            || word_token("NULL"),
            |value| Token::Number(unit.render(value), false),
        );
        tokens[*qualifier].token = word_token(unit.single_unit());
    }
}

/// `INTERVAL <expression> <compound qualifier>`, the expression anything
/// but a single literal, reads its quantity at run time.
fn wrap_expressions(tokens: &mut Vec<TokenWithSpan>) {
    let significant = significant(tokens);
    // (interval keyword, qualifier) token positions
    let mut wraps = Vec::new();
    for (at, &keyword) in significant.iter().enumerate() {
        if !is_interval(&tokens[keyword].token) {
            continue;
        }
        let mut depth = 0_usize;
        for &index in &significant[at + 1..] {
            match &tokens[index].token {
                Token::LParen => depth += 1,
                Token::RParen | Token::Comma | Token::SemiColon | Token::EOF if depth == 0 => break,
                Token::RParen => depth -= 1,
                token if depth == 0 => {
                    if let Some(unit) = compound_unit(token) {
                        wraps.push((keyword, index, unit));
                        break;
                    }
                    // A plain unit ends an ordinary interval.
                    if matches!(token, Token::Word(word) if word.quote_style.is_none()
                        && is_single_unit(&word.value))
                    {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    if wraps.is_empty() {
        return;
    }
    let original = std::mem::take(tokens);
    let mut wraps = wraps.into_iter().peekable();
    for (index, token) in original.into_iter().enumerate() {
        match wraps.peek() {
            Some(&(keyword, _, unit)) if index == keyword => {
                let span = token.span;
                tokens.push(token);
                tokens.push(TokenWithSpan::new(
                    word_token(&format!("{FUNCTION_PREFIX}{}", unit.name())),
                    span,
                ));
                tokens.push(TokenWithSpan::new(Token::LParen, span));
            }
            Some(&(_, qualifier, unit)) if index == qualifier => {
                tokens.push(TokenWithSpan::new(Token::RParen, token.span));
                tokens.push(TokenWithSpan::new(
                    word_token(unit.single_unit()),
                    token.span,
                ));
                wraps.next();
            }
            _ => tokens.push(token),
        }
    }
}

fn is_single_unit(word: &str) -> bool {
    [
        "MICROSECOND",
        "SECOND",
        "MINUTE",
        "HOUR",
        "DAY",
        "WEEK",
        "MONTH",
        "QUARTER",
        "YEAR",
    ]
    .iter()
    .any(|unit| unit.eq_ignore_ascii_case(word))
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

#[cfg(test)]
mod tests {
    use super::CompoundUnit;
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
            ("SECOND_MICROSECOND", "1.5", 1_500_000),
            ("SECOND_MICROSECOND", "5", 500_000),
            ("SECOND_MICROSECOND", "1.000005", 1_000_005),
            ("SECOND_MICROSECOND", "1.1234567", 2_234_567),
            ("SECOND_MICROSECOND", "-1.5", -1_500_000),
            ("MINUTE_MICROSECOND", "1:2", 1_200_000),
            ("HOUR_MICROSECOND", "1:2:3.4", 3_723_400_000),
            ("DAY_MICROSECOND", "1 2:3:4.5", 93_784_500_000),
        ] {
            let parsed = CompoundUnit::parse(unit).unwrap();
            assert_eq!(parsed.quantity(literal), Some(expected), "{unit} {literal}");
            parse_statement(&format!(
                "SELECT DATE_ADD('2024-01-01', INTERVAL '{literal}' {unit})"
            ))
            .unwrap();
        }
        let day_second = CompoundUnit::parse("DAY_SECOND").unwrap();
        assert_eq!(day_second.quantity("1 2:3:4.5"), None);
        assert_eq!(day_second.quantity("999999999999999999999999"), None);
    }

    #[test]
    fn rewriting_respects_comments_strings_and_original_locations() {
        let sql = "SELECT 'INTERVAL 1 YEAR_MONTH',\n DATE_ADD('2024-01-01', INTERVAL /* units */ '1-2' YEAR_MONTH) AS shifted";
        let parsed = parse_statement(sql).unwrap().to_string();
        assert!(parsed.contains("'INTERVAL 1 YEAR_MONTH'"));
        assert!(parsed.contains("INTERVAL 14 MONTH"));
        assert!(parse_statement("SELECT DATE_ADD('2024-01-01', INTERVAL NULL DAY_SECOND)").is_ok());
    }

    #[test]
    fn expression_quantities_read_their_text_at_run_time() {
        let parsed = parse_statement(
            "SELECT DATE_ADD(d, INTERVAL CONCAT(h, ':', m) HOUR_MINUTE), \
             d + INTERVAL -1 DAY_HOUR, DATE_SUB(d, INTERVAL x.y SECOND_MICROSECOND), \
             DATE_ADD(d, INTERVAL 2 DAY) FROM x",
        )
        .unwrap()
        .to_string();
        assert!(
            parsed.contains("INTERVAL PINTAIL_INTERVAL_HOUR_MINUTE(CONCAT(h, ':', m)) SECOND"),
            "{parsed}"
        );
        assert!(
            parsed.contains("INTERVAL PINTAIL_INTERVAL_DAY_HOUR(-1) SECOND"),
            "{parsed}"
        );
        assert!(
            parsed.contains("INTERVAL PINTAIL_INTERVAL_SECOND_MICROSECOND(x.y) SECOND"),
            "{parsed}"
        );
        assert!(parsed.contains("INTERVAL 2 DAY"), "{parsed}");
        assert_eq!(
            CompoundUnit::from_function_name("pintail_interval_day_hour"),
            Some(CompoundUnit::DayHour)
        );
        assert_eq!(
            CompoundUnit::from_function_name("PINTAIL_INTERVAL_DAY"),
            None
        );
    }
}
