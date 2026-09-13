//! Format-directed temporal parsing keeps incomplete dates until SQL mode
//! decides whether they are permitted; civil-date parsing cannot do that.
use chrono::{Datelike, Duration, NaiveDate};
use pintail_types::DataType;

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const DAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

#[derive(Default)]
struct Parts {
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micros: u32,
    twelve_hour: bool,
    afternoon: bool,
    year_day: Option<u32>,
    weekday: Option<u32>,
    week: Option<(char, u32)>,
    week_year: Option<(char, u32)>,
}

pub(super) fn parse(
    text: &str,
    format: &str,
    data_type: Option<DataType>,
    policy: u64,
) -> Option<String> {
    let mut parts = Parts::default();
    parts.consume(&mut text.trim_start(), format)?;
    parts.finish_date()?;
    if parts.twelve_hour {
        if !(1..=12).contains(&parts.hour) {
            return None;
        }
        parts.hour = parts.hour % 12 + u32::from(parts.afternoon) * 12;
    }
    if parts.hour > 23 || parts.minute > 59 || parts.second > 59 {
        return None;
    }
    let time_only = matches!(data_type, Some(DataType::Time64 { .. }));
    if !time_only
        && ((policy & 1 != 0 && (parts.year == 0 || parts.month == 0 || parts.day == 0))
            || (policy & 2 != 0 && parts.year != 0 && (parts.month == 0 || parts.day == 0)))
    {
        return None;
    }
    let date = format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day);
    if matches!(data_type, Some(DataType::Date32)) {
        return Some(date);
    }
    let fsp = match data_type {
        Some(DataType::DateTime64 { fsp } | DataType::Time64 { fsp }) => fsp,
        _ => 0,
    };
    if time_only {
        parts.hour += parts.day * 24;
    }
    let mut time = format!("{:02}:{:02}:{:02}", parts.hour, parts.minute, parts.second);
    if fsp > 0 {
        time.push('.');
        time.push_str(&format!("{:06}", parts.micros)[..usize::from(fsp)]);
    }
    Some(if time_only {
        time
    } else {
        format!("{date} {time}")
    })
}

impl Parts {
    fn consume(&mut self, input: &mut &str, format: &str) -> Option<()> {
        let mut pattern = format.chars();
        while let Some(character) = pattern.next() {
            *input = input.trim_start();
            // Exhausted input leaves the remaining fields at zero, even
            // when a literal separator or a fraction follows in the format.
            if input.is_empty() {
                break;
            }
            if character.is_whitespace() {
                continue;
            }
            if character != '%' {
                *input = input.strip_prefix(character)?;
                continue;
            }
            match pattern.next()? {
                specifier @ ('Y' | 'y') => {
                    let limit = if specifier == 'y' { 2 } else { 4 };
                    let (value, digits) = number(input, limit)?;
                    self.year = if digits <= 2 {
                        short_year(value)
                    } else {
                        value
                    };
                }
                'm' | 'c' => self.month = number(input, 2)?.0,
                'd' | 'e' => self.day = number(input, 2)?.0,
                'D' => {
                    self.day = number(input, 2)?.0;
                    let length = input
                        .bytes()
                        .take(2)
                        .take_while(u8::is_ascii_alphabetic)
                        .count();
                    *input = &input[length..];
                }
                'H' | 'k' => self.hour = number(input, 2)?.0,
                'h' | 'I' | 'l' => {
                    self.twelve_hour = true;
                    self.hour = number(input, 2)?.0;
                }
                'i' => self.minute = number(input, 2)?.0,
                's' | 'S' => self.second = number(input, 2)?.0,
                'f' => {
                    let (value, digits) = number(input, 6)?;
                    self.micros = value * 10_u32.pow(6 - digits);
                }
                'p' => {
                    if !self.twelve_hour {
                        return None;
                    }
                    let meridian = input.get(..2)?;
                    if meridian.eq_ignore_ascii_case("pm") {
                        self.afternoon = true;
                    } else if !meridian.eq_ignore_ascii_case("am") {
                        return None;
                    }
                    *input = &input[2..];
                }
                'M' => self.month = named(input, &MONTHS, false)? + 1,
                'b' => self.month = named(input, &MONTHS, true)? + 1,
                'W' => self.weekday = Some(named(input, &DAYS, false)?),
                'a' => self.weekday = Some(named(input, &DAYS, true)?),
                'w' => {
                    let day = number(input, 1)?.0;
                    if day > 6 {
                        return None;
                    }
                    self.weekday = Some(day);
                }
                'j' => self.year_day = Some(number(input, 3)?.0),
                mode @ ('U' | 'u' | 'V' | 'v') => self.week = Some((mode, number(input, 2)?.0)),
                mode @ ('X' | 'x') => self.week_year = Some((mode, number(input, 4)?.0)),
                'r' => self.consume(input, "%h:%i:%s %p")?,
                'T' => self.consume(input, "%H:%i:%s")?,
                '#' => skip(input, u8::is_ascii_digit),
                '@' => skip(input, u8::is_ascii_alphabetic),
                '.' => skip(input, u8::is_ascii_punctuation),
                '%' => *input = input.strip_prefix('%')?,
                _ => return None,
            }
        }
        Some(())
    }

    fn finish_date(&mut self) -> Option<()> {
        if self.year > 9999 || self.month > 12 || self.day > 31 {
            return None;
        }
        let year = i32::try_from(self.year).ok()?;
        if let Some(ordinal) = self.year_day
            && self.year != 0
        {
            self.set_date(NaiveDate::from_yo_opt(year, ordinal)?)?;
        }
        if let Some((mode, week)) = self.week {
            let weekday = self.weekday?;
            let strict = matches!(mode, 'V' | 'v');
            if week > 53 || (strict && week == 0) {
                return None;
            }
            let year = match (mode, self.week_year) {
                ('V', Some(('X', year))) | ('v', Some(('x', year))) => i32::try_from(year).ok()?,
                ('U' | 'u', None) => year,
                _ => return None,
            };
            let sunday = matches!(mode, 'U' | 'V');
            let anchor = NaiveDate::from_ymd_opt(year, 1, if sunday { 1 } else { 4 })?;
            let offset = if sunday {
                i64::from((7 - anchor.weekday().num_days_from_sunday()) % 7)
            } else {
                -i64::from(anchor.weekday().num_days_from_monday())
            };
            let day = if sunday { weekday } else { (weekday + 6) % 7 };
            let date = anchor.checked_add_signed(Duration::days(
                offset + (i64::from(week) - 1) * 7 + i64::from(day),
            ))?;
            self.set_date(date)?;
        }
        if self.month != 0 && self.day != 0 {
            // Year zero is not a leap year in this calendar.
            NaiveDate::from_ymd_opt(i32::try_from(self.year.max(1)).ok()?, self.month, self.day)?;
        }
        Some(())
    }

    fn set_date(&mut self, date: NaiveDate) -> Option<()> {
        self.year = u32::try_from(date.year())
            .ok()
            .filter(|year| *year <= 9999)?;
        self.month = date.month();
        self.day = date.day();
        Some(())
    }
}

fn number(input: &mut &str, limit: usize) -> Option<(u32, u32)> {
    let digits = input
        .bytes()
        .take(limit)
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    let value = input[..digits].parse().ok()?;
    *input = &input[digits..];
    Some((value, u32::try_from(digits).ok()?))
}

fn short_year(year: u32) -> u32 {
    year + if year <= 69 { 2000 } else { 1900 }
}

fn named(input: &mut &str, names: &[&str], abbreviated: bool) -> Option<u32> {
    let length = input.bytes().take_while(u8::is_ascii_alphabetic).count();
    if length == 0 {
        return None;
    }
    let name = input[..length].to_ascii_lowercase();
    let mut matches = names.iter().enumerate().filter(|(_, candidate)| {
        let candidate = if abbreviated {
            &candidate[..3]
        } else {
            candidate
        };
        candidate.starts_with(&name)
    });
    let (index, _) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    *input = &input[length..];
    u32::try_from(index).ok()
}

fn skip(input: &mut &str, predicate: fn(&u8) -> bool) {
    let length = input.bytes().take_while(predicate).count();
    *input = &input[length..];
}
