//! Civil time values used by calendars.
//!
//! This module contains no clock reads. Every value is explicit; the caller injects wall time as
//! Unix seconds. Phase 1 supports UTC and fixed offsets only.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, ScheduleError};

/// Seconds since the Unix epoch, UTC. Aliased to make signatures readable.
pub type UnixSecs = i64;

/// A fixed offset from UTC, in minutes. No zone database or DST rules are part of phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FixedOffset {
    minutes: i16,
}

impl FixedOffset {
    pub const UTC: Self = Self { minutes: 0 };
    pub const MAX_MINUTES: i16 = 14 * 60;

    pub const fn from_minutes(minutes: i16) -> Option<Self> {
        if minutes.abs() > Self::MAX_MINUTES {
            None
        } else {
            Some(Self { minutes })
        }
    }

    pub const fn minutes(self) -> i16 {
        self.minutes
    }

    pub const fn is_utc(self) -> bool {
        self.minutes == 0
    }

    /// Parses `Z`, `UTC`, `+HH:MM`, `-HH:MM`, `+HHMM`, or a bare minute count.
    pub fn parse(value: &str) -> Result<Self> {
        let text = value.trim();
        if text.eq_ignore_ascii_case("z") || text.eq_ignore_ascii_case("utc") {
            return Ok(Self::UTC);
        }
        if let Ok(minutes) = text.parse::<i16>() {
            return Self::from_minutes(minutes).ok_or_else(|| {
                ScheduleError::validation(format!("offset {text} is outside -14:00..=+14:00"))
            });
        }
        let (sign, body) = match text.as_bytes().first() {
            Some(b'+') => (1i16, &text[1..]),
            Some(b'-') => (-1i16, &text[1..]),
            _ => {
                return Err(ScheduleError::validation(format!(
                    "invalid fixed offset {text:?}; expected Z, UTC, +HH:MM or -HH:MM"
                )))
            }
        };
        let (hour, minute) = match body.split_once(':') {
            Some((h, m)) => (h, m),
            None if body.len() == 4 => (&body[..2], &body[2..]),
            _ => {
                return Err(ScheduleError::validation(format!(
                    "invalid fixed offset {text:?}; expected +HH:MM or -HH:MM"
                )))
            }
        };
        let hour = hour
            .parse::<i16>()
            .map_err(|_| ScheduleError::validation(format!("invalid fixed offset {text:?}")))?;
        let minute = minute
            .parse::<i16>()
            .map_err(|_| ScheduleError::validation(format!("invalid fixed offset {text:?}")))?;
        if hour > 14 || !(0..=59).contains(&minute) || (hour == 14 && minute != 0) {
            return Err(ScheduleError::validation(format!(
                "offset {text} is outside -14:00..=+14:00"
            )));
        }
        let minutes = sign * (hour * 60 + minute);
        Ok(Self::from_minutes(minutes).expect("range checked above"))
    }

    /// Canonical form used in config and JSON.
    pub fn as_str(self) -> String {
        if self.minutes == 0 {
            return "Z".to_owned();
        }
        let sign = if self.minutes < 0 { '-' } else { '+' };
        let abs = self.minutes.unsigned_abs();
        format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
    }
}

impl fmt::Display for FixedOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl Serialize for FixedOffset {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for FixedOffset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

/// A day of the week. Wire spellings are `mon`..`sun` and their full names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    pub const ALL: [Self; 7] = [
        Self::Mon,
        Self::Tue,
        Self::Wed,
        Self::Thu,
        Self::Fri,
        Self::Sat,
        Self::Sun,
    ];

    /// Day index with Monday = 0, matching the Unix epoch's Thursday = 3.
    pub const fn index(self) -> i64 {
        match self {
            Self::Mon => 0,
            Self::Tue => 1,
            Self::Wed => 2,
            Self::Thu => 3,
            Self::Fri => 4,
            Self::Sat => 5,
            Self::Sun => 6,
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        let text = value.trim().to_ascii_lowercase();
        match text.as_str() {
            "mon" | "monday" => Ok(Self::Mon),
            "tue" | "tues" | "tuesday" => Ok(Self::Tue),
            "wed" | "wednesday" => Ok(Self::Wed),
            "thu" | "thur" | "thurs" | "thursday" => Ok(Self::Thu),
            "fri" | "friday" => Ok(Self::Fri),
            "sat" | "saturday" => Ok(Self::Sat),
            "sun" | "sunday" => Ok(Self::Sun),
            _ => Err(ScheduleError::validation(format!(
                "invalid weekday {value:?}"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of weekdays. Accepts `*`, `mon-fri`, comma-separated lists, and JSON/TOML arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Days {
    bits: u8,
}

impl Days {
    pub const ALL: Self = Self { bits: 0x7f };

    pub fn from_days(days: impl IntoIterator<Item = Weekday>) -> Self {
        let mut bits = 0;
        for day in days {
            bits |= 1 << day.index() as u8;
        }
        Self { bits }
    }

    pub fn contains(self, day: Weekday) -> bool {
        self.bits & (1 << day.index() as u8) != 0
    }

    pub fn is_empty(self) -> bool {
        self.bits == 0
    }

    pub fn list(self) -> Vec<Weekday> {
        Weekday::ALL
            .into_iter()
            .filter(|d| self.contains(*d))
            .collect()
    }

    pub fn parse(value: &str) -> Result<Self> {
        let text = value.trim();
        if text == "*" {
            return Ok(Self::ALL);
        }
        let mut bits = 0;
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(ScheduleError::validation("empty day in `days`"));
            }
            match part.split_once('-') {
                Some((start, end)) => {
                    let start = Weekday::parse(start)?;
                    let end = Weekday::parse(end)?;
                    if start.index() > end.index() {
                        return Err(ScheduleError::validation(format!(
                            "day range {part} runs backwards"
                        )));
                    }
                    for index in start.index()..=end.index() {
                        if let Some(day) = Weekday::ALL.iter().find(|d| d.index() == index) {
                            bits |= 1 << day.index() as u8;
                        }
                    }
                }
                None => {
                    let day = Weekday::parse(part)?;
                    bits |= 1 << day.index() as u8;
                }
            }
        }
        Ok(Self { bits })
    }
}

impl Default for Days {
    fn default() -> Self {
        Self::ALL
    }
}

impl fmt::Display for Days {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::ALL {
            return f.write_str("*");
        }
        let list = self
            .list()
            .iter()
            .map(|day| day.as_str())
            .collect::<Vec<_>>()
            .join(",");
        f.write_str(&list)
    }
}

impl Serialize for Days {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Days {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            List(Vec<String>),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Text(text) => Self::parse(&text).map_err(de::Error::custom),
            Repr::List(list) => {
                let mut days = Vec::new();
                for item in list {
                    days.push(Weekday::parse(&item).map_err(de::Error::custom)?);
                }
                Ok(Self::from_days(days))
            }
        }
    }
}

/// A local wall-clock time in minutes from midnight. `24:00` is allowed as an end boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClockTime {
    minutes: u16,
}

impl ClockTime {
    pub const MIDNIGHT: Self = Self { minutes: 0 };
    pub const END_OF_DAY: Self = Self { minutes: 24 * 60 };

    pub const fn from_minutes(minutes: u16) -> Option<Self> {
        if minutes > 24 * 60 {
            None
        } else {
            Some(Self { minutes })
        }
    }

    pub const fn minutes(self) -> u16 {
        self.minutes
    }

    pub fn parse(value: &str) -> Result<Self> {
        let text = value.trim();
        let (hour, minute) = text.split_once(':').ok_or_else(|| {
            ScheduleError::validation(format!("invalid time {text:?}; expected HH:MM"))
        })?;
        let hour = hour
            .parse::<u16>()
            .map_err(|_| ScheduleError::validation(format!("invalid time {text:?}")))?;
        let minute = minute
            .parse::<u16>()
            .map_err(|_| ScheduleError::validation(format!("invalid time {text:?}")))?;
        if hour > 24 || minute > 59 || (hour == 24 && minute != 0) {
            return Err(ScheduleError::validation(format!(
                "time {text:?} is outside 00:00..=24:00"
            )));
        }
        Ok(Self {
            minutes: hour * 60 + minute,
        })
    }

    pub fn as_str(self) -> String {
        format!("{:02}:{:02}", self.minutes / 60, self.minutes % 60)
    }
}

impl Default for ClockTime {
    fn default() -> Self {
        Self::MIDNIGHT
    }
}

impl fmt::Display for ClockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl Serialize for ClockTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for ClockTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// A civil date in a calendar's fixed offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CivilDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl CivilDate {
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<&str> = value.trim().split('-').collect();
        if parts.len() != 3 || parts[0].is_empty() || parts[1].len() != 2 || parts[2].len() != 2 {
            return Err(ScheduleError::validation(format!(
                "invalid date {value:?}; expected YYYY-MM-DD"
            )));
        }
        let year = parts[0]
            .parse::<i64>()
            .map_err(|_| ScheduleError::validation(format!("invalid date {value:?}")))?;
        let month = parts[1]
            .parse::<u32>()
            .map_err(|_| ScheduleError::validation(format!("invalid date {value:?}")))?;
        let day = parts[2]
            .parse::<u32>()
            .map_err(|_| ScheduleError::validation(format!("invalid date {value:?}")))?;
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(ScheduleError::validation(format!(
                "invalid date {value:?}; not a calendar day"
            )));
        }
        Ok(Self { year, month, day })
    }

    /// Days since 1970-01-01. Uses Howard Hinnant's civil-from-days inverse.
    pub fn days_since_epoch(self) -> i64 {
        let year = self.year - if self.month <= 2 { 1 } else { 0 };
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let yoe = year - era * 400;
        let mp = self.month as i64 + if self.month > 2 { -3 } else { 9 };
        let doy = (153 * mp + 2) / 5 + self.day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn from_days_since_epoch(days: i64) -> Self {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = (mp + if mp < 10 { 3 } else { -9 }) as u32;
        if month <= 2 {
            year += 1;
        }
        Self { year, month, day }
    }

    pub fn weekday(self) -> Weekday {
        // 1970-01-01 was a Thursday (index 3).
        let index = self.days_since_epoch().rem_euclid(7) + 3;
        Weekday::ALL[(index % 7) as usize]
    }

    pub fn as_str(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// A calendar-local instant: a date, a local time, and the fixed offset used to reach Unix time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilInstant {
    pub date: CivilDate,
    pub time: ClockTime,
    pub offset: FixedOffset,
}

impl CivilInstant {
    pub fn to_unix(self) -> UnixSecs {
        self.date.days_since_epoch() * 86_400 + self.time.minutes() as i64 * 60
            - self.offset.minutes() as i64 * 60
    }

    pub fn from_unix(unix: UnixSecs, offset: FixedOffset) -> Self {
        let local = unix + offset.minutes() as i64 * 60;
        let days = local.div_euclid(86_400);
        let seconds = local.rem_euclid(86_400);
        Self {
            date: CivilDate::from_days_since_epoch(days),
            time: ClockTime::from_minutes((seconds / 60) as u16).expect("under 1440"),
            offset,
        }
    }

    /// Parses `YYYY-MM-DD` (midnight) or `YYYY-MM-DDTHH:MM[:SS][Z|±HH:MM]`.
    /// An explicit offset wins; otherwise the calendar's offset is used.
    pub fn parse(value: &str, default_offset: FixedOffset) -> Result<Self> {
        let text = value.trim();
        if text.len() == 10 {
            let date = CivilDate::parse(text)?;
            return Ok(Self {
                date,
                time: ClockTime::MIDNIGHT,
                offset: default_offset,
            });
        }
        let (date_part, rest) = text.split_once('T').ok_or_else(|| {
            ScheduleError::validation(format!(
                "invalid instant {value:?}; expected YYYY-MM-DD or YYYY-MM-DDTHH:MM"
            ))
        })?;
        let date = CivilDate::parse(date_part)?;
        let (time_part, offset) = if let Some(stripped) = rest.strip_suffix('Z') {
            (stripped, FixedOffset::UTC)
        } else if let Some(index) = rest.find(['+', '-']) {
            (&rest[..index], FixedOffset::parse(&rest[index..])?)
        } else {
            (rest, default_offset)
        };
        let mut pieces = time_part.split(':');
        let hour = pieces.next().and_then(|s| s.parse::<u16>().ok());
        let minute = pieces.next().and_then(|s| s.parse::<u16>().ok());
        let second = pieces.next().unwrap_or("00");
        let second = second
            .parse::<u16>()
            .map_err(|_| ScheduleError::validation(format!("invalid instant {value:?}")))?;
        if pieces.next().is_some() || second > 59 {
            return Err(ScheduleError::validation(format!(
                "invalid instant {value:?}"
            )));
        }
        let (hour, minute) = match (hour, minute) {
            (Some(h), Some(m)) if h <= 23 && m <= 59 => (h, m),
            _ => {
                return Err(ScheduleError::validation(format!(
                    "invalid instant {value:?}"
                )))
            }
        };
        Ok(Self {
            date,
            time: ClockTime::from_minutes(hour * 60 + minute).expect("range checked"),
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_roundtrip() {
        for text in ["Z", "UTC", "+02:00", "-05:30", "+0530"] {
            let offset = FixedOffset::parse(text).unwrap();
            assert_eq!(FixedOffset::parse(&offset.as_str()).unwrap(), offset);
        }
        assert!(FixedOffset::parse("+15:00").is_err());
        assert!(FixedOffset::parse("nowhere").is_err());
    }

    #[test]
    fn window_days_and_midnight() {
        assert!(Days::parse("mon-fri").unwrap().contains(Weekday::Fri));
        assert!(!Days::parse("mon-fri").unwrap().contains(Weekday::Sat));
        assert_eq!(Days::parse("*").unwrap(), Days::ALL);
        assert_eq!(ClockTime::parse("24:00").unwrap(), ClockTime::END_OF_DAY);
        assert!(ClockTime::parse("24:01").is_err());
    }

    #[test]
    fn civil_instants_are_reversible() {
        let instant = CivilInstant::parse("2026-12-05T22:00-05:00", FixedOffset::UTC).unwrap();
        assert_eq!(instant.to_unix(), 1_796_526_000);
        let back = CivilInstant::from_unix(instant.to_unix(), instant.offset);
        assert_eq!(back.date, instant.date);
        assert_eq!(back.time, instant.time);
        assert_eq!(
            CivilDate::parse("2024-02-29").unwrap().weekday(),
            Weekday::Thu
        );
    }
}
