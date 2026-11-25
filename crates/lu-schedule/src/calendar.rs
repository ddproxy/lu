//! Named calendars with weekly and one-off rules.
//!
//! Phase 1 has no timezone database: every calendar uses UTC or a fixed offset. Windows are
//! half-open and `days` names the local start day. A window whose end time is less than its start
//! time crosses midnight; equal endpoints are rejected.

use serde::{Deserialize, Serialize};

use crate::error::{Result, ScheduleError};
use crate::interval::{containing, next_boundary, normalize, subtract, Budget, Interval};
use crate::time::{CivilInstant, ClockTime, Days, FixedOffset, UnixSecs, Weekday};

/// The two gate kinds supported in phase 1. Unavailable always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowKind {
    Available,
    #[default]
    Unavailable,
}

impl WindowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The state of a calendar with no matching available rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultState {
    #[default]
    Open,
    Closed,
}

/// A recurring local-time window. `id` is carried in explanations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: WindowKind,
    pub from: ClockTime,
    pub to: ClockTime,
    #[serde(default)]
    pub days: Days,
}

impl WeeklyWindow {
    pub fn validate(&self, path: &str) -> Result<()> {
        if self.from == self.to {
            return Err(ScheduleError::new(
                path,
                "from and to must differ; use 24:00 as an end to close at midnight",
            ));
        }
        if self.days.is_empty() {
            return Err(ScheduleError::new(path, "days must not be empty"));
        }
        Ok(())
    }

    /// Expands the rule into UTC instants over `[start, end)`.
    pub fn intervals(
        &self,
        start: UnixSecs,
        end: UnixSecs,
        offset: FixedOffset,
        budget: &mut Budget,
    ) -> Result<Vec<Interval>> {
        if end <= start {
            return Ok(Vec::new());
        }
        let first_day = CivilInstant::from_unix(start, offset)
            .date
            .days_since_epoch()
            - 1;
        let last_day = CivilInstant::from_unix(end - 1, offset)
            .date
            .days_since_epoch()
            + 1;
        let crosses = self.to <= self.from;
        let mut out = Vec::new();
        for day in first_day..=last_day {
            let date = crate::time::CivilDate::from_days_since_epoch(day);
            if !self.days.contains(date.weekday()) {
                continue;
            }
            budget.spend(1)?;
            let begin = CivilInstant {
                date,
                time: self.from,
                offset,
            }
            .to_unix();
            let finish_date = if crosses {
                crate::time::CivilDate::from_days_since_epoch(day + 1)
            } else {
                date
            };
            let finish = CivilInstant {
                date: finish_date,
                time: self.to,
                offset,
            }
            .to_unix();
            let window_start = begin.max(start);
            let window_end = finish.min(end);
            if window_start < window_end {
                out.push(Interval {
                    start: window_start,
                    end: window_end,
                });
            }
        }
        Ok(out)
    }
}

/// A one-off span. Both endpoints are calendar-local unless they carry an explicit offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DateRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: WindowKind,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DateRange {
    fn endpoints(&self, offset: FixedOffset, path: &str) -> Result<(UnixSecs, UnixSecs)> {
        let from = CivilInstant::parse(&self.from, offset)
            .map_err(|e| ScheduleError::new(path, e.reason()))?
            .to_unix();
        let to = CivilInstant::parse(&self.to, offset)
            .map_err(|e| ScheduleError::new(path, e.reason()))?
            .to_unix();
        if to <= from {
            return Err(ScheduleError::new(
                path,
                "one-off range end must be after its start",
            ));
        }
        Ok((from, to))
    }

    fn intervals(
        &self,
        start: UnixSecs,
        end: UnixSecs,
        offset: FixedOffset,
        path: &str,
        budget: &mut Budget,
    ) -> Result<Vec<Interval>> {
        budget.spend(1)?;
        let (from, to) = self.endpoints(offset, path)?;
        let start = from.max(start);
        let end = to.min(end);
        Ok((start < end)
            .then_some(vec![Interval { start, end }])
            .unwrap_or_default())
    }
}

/// The observable state of a calendar over a half-open interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarState {
    /// `true` means available; `false` means unavailable.
    pub available: bool,
    /// Start of the current availability state.
    pub since: UnixSecs,
    /// End of the current availability state.
    pub until: UnixSecs,
    /// The rule that decided the current state, for explanations.
    pub rule: &'static str,
}

impl CalendarState {
    pub fn wanted(&self, wanted: WindowKind) -> bool {
        match wanted {
            WindowKind::Available => self.available,
            WindowKind::Unavailable => !self.available,
        }
    }
}

/// A named, immutable set of rules. Evaluations are pure: the same `(calendar, now)` always gives
/// the same answer, which is what makes a restart mid-window safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calendar {
    pub name: String,
    #[serde(default)]
    pub offset: FixedOffset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultState>,
    #[serde(default)]
    pub windows: Vec<WeeklyWindow>,
    #[serde(default)]
    pub ranges: Vec<DateRange>,
    #[serde(default)]
    pub exceptions: Vec<DateRange>,
}

impl Calendar {
    pub fn validate(&self) -> Result<()> {
        if !valid_calendar_name(&self.name) {
            return Err(ScheduleError::new(
                "calendar.name",
                "must be 1..64 characters of [A-Za-z0-9._-]",
            ));
        }
        for (index, window) in self.windows.iter().enumerate() {
            window.validate(&format!("calendar.{} windows[{index}]", self.name))?;
        }
        for (field, ranges) in [("ranges", &self.ranges), ("exceptions", &self.exceptions)] {
            for (index, range) in ranges.iter().enumerate() {
                let path = format!("calendar.{} {field}[{index}]", self.name);
                range
                    .endpoints(self.offset, &path)
                    .map_err(|e| ScheduleError::new(path.clone(), e.reason()))?;
            }
        }
        Ok(())
    }

    fn default_open(&self) -> bool {
        match self.default {
            Some(DefaultState::Open) => true,
            Some(DefaultState::Closed) => false,
            None => {
                !self.windows.iter().any(|w| w.kind == WindowKind::Available)
                    && !self.ranges.iter().any(|r| r.kind == WindowKind::Available)
                    && !self
                        .exceptions
                        .iter()
                        .any(|r| r.kind == WindowKind::Available)
            }
        }
    }

    /// Normalized intervals of `kind` over `[start, end)`. Unavailable wins over available.
    pub fn intervals(
        &self,
        start: UnixSecs,
        end: UnixSecs,
        kind: WindowKind,
        budget: &mut Budget,
    ) -> Result<Vec<Interval>> {
        self.validate()?;
        if end <= start {
            return Ok(Vec::new());
        }
        let mut available = Vec::new();
        let mut unavailable = Vec::new();
        if self.default_open() {
            available.push(Interval { start, end });
        }
        for (index, window) in self.windows.iter().enumerate() {
            let mut steps = Budget::new(budget.remaining());
            let intervals = window
                .intervals(start, end, self.offset, &mut steps)
                .map_err(|e| {
                    ScheduleError::new(
                        format!("calendar.{} windows[{index}]", self.name),
                        e.reason(),
                    )
                })?;
            budget.spend(budget.remaining().saturating_sub(steps.remaining()))?;
            if window.kind == WindowKind::Available {
                available.extend(intervals);
            } else {
                unavailable.extend(intervals);
            }
        }
        for (field, ranges) in [("ranges", &self.ranges), ("exceptions", &self.exceptions)] {
            for (index, range) in ranges.iter().enumerate() {
                let mut steps = Budget::new(budget.remaining());
                let intervals = range
                    .intervals(
                        start,
                        end,
                        self.offset,
                        &format!("{field}[{index}]"),
                        &mut steps,
                    )
                    .map_err(|e| {
                        ScheduleError::new(
                            format!("calendar.{} {field}[{index}]", self.name),
                            e.reason(),
                        )
                    })?;
                budget.spend(budget.remaining().saturating_sub(steps.remaining()))?;
                if range.kind == WindowKind::Available {
                    available.extend(intervals);
                } else {
                    unavailable.extend(intervals);
                }
            }
        }
        let available = normalize(available);
        let unavailable = normalize(unavailable);
        // An exception replaces the base rules for its span. Removing the span from both sides
        // then adding the exception's own kind gives that behaviour for ranges and exceptions
        // alike, with unavailable applied last.
        let mut exception_spans = Vec::new();
        for range in &self.exceptions {
            let (from, to) = range.endpoints(self.offset, "calendar.exception")?;
            exception_spans.push(Interval {
                start: from,
                end: to,
            });
        }
        let exception_spans = normalize(exception_spans);
        let available = subtract(&subtract(&available, &exception_spans), &unavailable);
        let exception_available = self
            .exceptions
            .iter()
            .filter(|r| r.kind == WindowKind::Available)
            .flat_map(|r| {
                let (from, to) = r
                    .endpoints(self.offset, "calendar.exception")
                    .unwrap_or((0, 0));
                [Interval {
                    start: from,
                    end: to,
                }]
            })
            .collect();
        let mut available = normalize([available, normalize(exception_available)].concat());
        // One-off unavailable ranges and exception windows win last.
        let mut blocked = unavailable;
        blocked.extend(
            self.exceptions
                .iter()
                .filter(|r| r.kind == WindowKind::Unavailable)
                .filter_map(|r| {
                    let (from, to) = r.endpoints(self.offset, "calendar.exception").ok()?;
                    Some(Interval {
                        start: from,
                        end: to,
                    })
                }),
        );
        let blocked = normalize(blocked);
        if kind == WindowKind::Unavailable {
            return Ok(blocked);
        }
        available = subtract(&available, &blocked);
        Ok(available)
    }

    /// State at an injected wall-clock instant. `horizon` bounds the returned interval.
    pub fn state_at(
        &self,
        at: UnixSecs,
        horizon: UnixSecs,
        budget: &mut Budget,
    ) -> Result<CalendarState> {
        let end = at.saturating_add(horizon.max(1));
        // A small lookback keeps `since` meaningful when `at` is already inside a window; lead
        // calculations depend on the real begin, not the first boundary after `at`.
        let lookback = at.saturating_sub(2 * 86_400);
        let available = self.intervals(lookback, end, WindowKind::Available, budget)?;
        let unavailable = self.intervals(lookback, end, WindowKind::Unavailable, budget)?;
        let (available_now, since, until) = match containing(&available, at) {
            Some(current) => (true, current.start, current.end),
            None => {
                let gaps = complement(&available, lookback, end);
                let current = containing(&gaps, at).ok_or_else(|| {
                    ScheduleError::validation("calendar evaluation outside horizon")
                })?;
                (false, current.start, current.end)
            }
        };
        let rule = if containing(&unavailable, at).is_some() {
            "unavailable"
        } else if available_now {
            "available"
        } else {
            "default"
        };
        Ok(CalendarState {
            available: available_now,
            since,
            until,
            rule,
        })
    }

    /// The next boundary strictly after `at`, within the horizon.
    pub fn next_change(
        &self,
        at: UnixSecs,
        horizon: UnixSecs,
        budget: &mut Budget,
    ) -> Result<Option<UnixSecs>> {
        let end = at.saturating_add(horizon.max(1));
        let intervals = self.intervals(at, end, WindowKind::Available, budget)?;
        Ok(next_boundary(&intervals, at, end))
    }
}

/// Complement of normalized intervals over `[start, end)`.
fn complement(intervals: &[Interval], start: i64, end: i64) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut cursor = start;
    for interval in intervals {
        if interval.end <= start || interval.start >= end {
            continue;
        }
        if interval.start > cursor {
            out.push(Interval {
                start: cursor,
                end: interval.start.min(end),
            });
        }
        cursor = cursor.max(interval.end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        out.push(Interval { start: cursor, end });
    }
    normalize(out)
}

pub fn valid_calendar_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Returns the earliest calendar-local start at which `need` seconds remain before a close.
pub fn next_fit_start(
    intervals: &[Interval],
    from: UnixSecs,
    need: i64,
    horizon_end: UnixSecs,
) -> Option<UnixSecs> {
    intervals
        .iter()
        .filter(|i| i.start >= from && i.start < horizon_end && i.length() >= need)
        .map(|i| i.start)
        .min()
}

/// Exposed for config/API views that want to render a calendar's local weekday.
pub fn weekday_at(unix: UnixSecs, offset: FixedOffset) -> Weekday {
    CivilInstant::from_unix(unix, offset).date.weekday()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ClockTime;

    fn window(kind: WindowKind, from: &str, to: &str, days: &str) -> WeeklyWindow {
        WeeklyWindow {
            id: Some(format!("{kind:?}-{from}")),
            kind,
            from: ClockTime::parse(from).unwrap(),
            to: ClockTime::parse(to).unwrap(),
            days: Days::parse(days).unwrap(),
        }
    }

    #[test]
    fn former_dates_field_is_not_an_alias_for_ranges() {
        let calendar: Calendar = serde_json::from_str(r#"{"name":"office","ranges":[]}"#).unwrap();
        assert!(calendar.ranges.is_empty());
        let error =
            serde_json::from_str::<Calendar>(r#"{"name":"office","dates":[]}"#).unwrap_err();
        assert!(error.to_string().contains("dates"), "{error}");
    }

    #[test]
    fn rejects_equal_endpoints() {
        let cal = Calendar {
            name: "bad".to_owned(),
            offset: FixedOffset::UTC,
            default: None,
            windows: vec![window(WindowKind::Available, "08:00", "08:00", "*")],
            ranges: Vec::new(),
            exceptions: Vec::new(),
        };
        assert!(cal.validate().is_err());
    }

    #[test]
    fn crossing_midnight_uses_start_day() {
        let cal = Calendar {
            name: "nightly".to_owned(),
            offset: FixedOffset::UTC,
            default: None,
            windows: vec![window(WindowKind::Available, "22:00", "06:00", "fri")],
            ranges: Vec::new(),
            exceptions: Vec::new(),
        };
        let mut budget = Budget::new(100);
        // 2026-11-27 is a Friday; UTC day boundary is 2026-11-28T00:00Z.
        let friday = CivilInstant::parse("2026-11-27T22:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let saturday_end = CivilInstant::parse("2026-11-28T06:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let intervals = cal
            .intervals(
                friday - 1,
                saturday_end + 1,
                WindowKind::Available,
                &mut budget,
            )
            .unwrap();
        assert_eq!(
            intervals,
            vec![Interval {
                start: friday,
                end: saturday_end
            }]
        );
        let monday = CivilInstant::parse("2026-11-30T22:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        assert!(cal
            .intervals(
                monday,
                monday + 1,
                WindowKind::Available,
                &mut Budget::new(100)
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unavailable_wins_and_exceptions_replace_base() {
        let cal = Calendar {
            name: "maintenance".to_owned(),
            offset: FixedOffset::parse("+02:00").unwrap(),
            default: Some(DefaultState::Open),
            windows: vec![
                window(WindowKind::Available, "09:00", "17:00", "mon-fri"),
                window(WindowKind::Unavailable, "12:00", "13:00", "*"),
            ],
            ranges: vec![DateRange {
                id: None,
                kind: WindowKind::Unavailable,
                from: "2026-12-01T09:00".to_owned(),
                to: "2026-12-01T12:00".to_owned(),
                reason: None,
            }],
            exceptions: vec![DateRange {
                id: Some("holiday".to_owned()),
                kind: WindowKind::Unavailable,
                from: "2026-12-25".to_owned(),
                to: "2026-12-26".to_owned(),
                reason: None,
            }],
        };
        let mut budget = Budget::new(1000);
        let at = CivilInstant::parse("2026-12-01T12:30+02:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let state = cal.state_at(at, 86_400, &mut budget).unwrap();
        assert!(!state.available);
        assert_eq!(state.rule, "unavailable");
        let holiday = CivilInstant::parse("2026-12-25T10:00+02:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        assert!(
            !cal.state_at(holiday, 86_400, &mut budget)
                .unwrap()
                .available
        );
    }

    #[test]
    fn full_day_windows_at_fixed_offset() {
        let cal = Calendar {
            name: "weekend".to_owned(),
            offset: FixedOffset::parse("-05:00").unwrap(),
            default: None,
            windows: vec![window(WindowKind::Available, "00:00", "24:00", "sat,sun")],
            ranges: Vec::new(),
            exceptions: Vec::new(),
        };
        let saturday = CivilInstant::parse("2026-11-28T00:00-05:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let monday = CivilInstant::parse("2026-11-30T00:00-05:00", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let intervals = cal
            .intervals(
                saturday,
                monday,
                WindowKind::Available,
                &mut Budget::new(100),
            )
            .unwrap();
        assert_eq!(
            intervals,
            vec![Interval {
                start: saturday,
                end: monday
            }]
        );
    }
}
