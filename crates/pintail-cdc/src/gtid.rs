use std::{collections::BTreeMap, fmt::Write as _, str::FromStr};

use mysql_async::Sid;

use crate::CdcError;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MysqlGtidSet {
    entries: BTreeMap<String, Vec<(u64, u64)>>,
}

impl MysqlGtidSet {
    pub(crate) fn parse(value: &str) -> Result<Self, CdcError> {
        let mut set = Self::default();
        for raw_sid in value
            .split(',')
            .map(str::trim)
            .filter(|sid| !sid.is_empty())
        {
            let mut parts = raw_sid.split(':');
            let uuid = parts
                .next()
                .ok_or_else(|| CdcError::InvalidCheckpoint("GTID SID is empty".to_owned()))?;
            let first = parts.next().ok_or_else(|| {
                CdcError::InvalidCheckpoint(format!("GTID SID has no intervals: {raw_sid}"))
            })?;
            let (key, first_interval) = if first.as_bytes().first().is_some_and(u8::is_ascii_digit)
            {
                (uuid.to_owned(), Some(first))
            } else {
                let interval = parts.next().ok_or_else(|| {
                    CdcError::InvalidCheckpoint(format!(
                        "tagged GTID SID has no intervals: {raw_sid}"
                    ))
                })?;
                (format!("{uuid}:{first}"), Some(interval))
            };
            let intervals = first_interval.into_iter().chain(parts).map(parse_interval);
            for interval in intervals {
                let (start, end) = interval?;
                set.insert_interval(&key, start, end);
            }
        }
        Ok(set)
    }

    pub(crate) fn add_event(
        &mut self,
        sid: [u8; 16],
        tag: Option<&str>,
        sequence: u64,
    ) -> Result<(), CdcError> {
        if sequence == 0 {
            return Err(CdcError::Decode(
                "GTID sequence zero is not a valid committed transaction".to_owned(),
            ));
        }
        let mut key = format_uuid(sid);
        if let Some(tag) = tag {
            write!(key, ":{tag}").expect("writing to a string cannot fail");
        }
        self.insert_interval(&key, sequence, sequence);
        Ok(())
    }

    pub(crate) fn to_sids(&self) -> Result<Vec<Sid<'static>>, CdcError> {
        self.entries
            .iter()
            .map(|(key, intervals)| {
                let value = format_entry(key, intervals);
                Sid::from_str(&value).map_err(|error| {
                    CdcError::InvalidCheckpoint(format!("cannot encode GTID SID {value}: {error}"))
                })
            })
            .collect()
    }

    fn insert_interval(&mut self, key: &str, start: u64, end: u64) {
        let intervals = self.entries.entry(key.to_owned()).or_default();
        intervals.push((start, end));
        intervals.sort_unstable();
        let mut merged = Vec::<(u64, u64)>::with_capacity(intervals.len());
        for (start, end) in intervals.drain(..) {
            if let Some((_, previous_end)) = merged.last_mut()
                && start <= previous_end.saturating_add(1)
            {
                *previous_end = (*previous_end).max(end);
            } else {
                merged.push((start, end));
            }
        }
        *intervals = merged;
    }
}

impl std::fmt::Display for MysqlGtidSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, (key, intervals)) in self.entries.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(&format_entry(key, intervals))?;
        }
        Ok(())
    }
}

fn parse_interval(value: &str) -> Result<(u64, u64), CdcError> {
    let (start, end) = value
        .split_once('-')
        .map_or((value, value), |(start, end)| (start, end));
    let start = start.parse::<u64>().map_err(|error| {
        CdcError::InvalidCheckpoint(format!("invalid GTID interval {value}: {error}"))
    })?;
    let end = end.parse::<u64>().map_err(|error| {
        CdcError::InvalidCheckpoint(format!("invalid GTID interval {value}: {error}"))
    })?;
    if start == 0 || end < start {
        return Err(CdcError::InvalidCheckpoint(format!(
            "invalid GTID interval {value}"
        )));
    }
    Ok((start, end))
}

fn format_entry(key: &str, intervals: &[(u64, u64)]) -> String {
    let mut value = key.to_owned();
    for (start, end) in intervals {
        write!(value, ":{start}").expect("writing to a string cannot fail");
        if end != start {
            write!(value, "-{end}").expect("writing to a string cannot fail");
        }
    }
    value
}

fn format_uuid(bytes: [u8; 16]) -> String {
    let mut output = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            output.push('-');
        }
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::MysqlGtidSet;

    #[test]
    fn parses_merges_and_encodes_mysql_gtid_sets() {
        let mut set = MysqlGtidSet::parse(
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:1-3:5,\
             bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb:blue:7",
        )
        .expect("parse GTID set");
        set.add_event([0xaa; 16], None, 4).expect("merge event");
        set.add_event([0xaa; 16], None, 5).expect("existing event");
        assert_eq!(
            set.to_string(),
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:1-5,\
             bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb:blue:7"
        );
        assert_eq!(set.to_sids().expect("wire SIDs").len(), 2);
    }
}
