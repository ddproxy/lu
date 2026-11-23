//! Pure, clock-free calendar and admission algebra for time-aware scheduling.
//!
//! Time is injected as Unix seconds; this crate performs no I/O and reads no clock. It contains no
//! Lu types so it can move to a shared library unchanged. Phase 1 supports UTC and fixed offsets,
//! weekly windows, one-off ranges and exceptions, and the `available`/`unavailable` gate kinds.

pub mod calendar;
pub mod error;
pub mod interval;
pub mod job;
pub mod schedule;
pub mod time;

pub use calendar::{
    valid_calendar_name, Calendar, CalendarState, DateRange, DefaultState, WeeklyWindow, WindowKind,
};
pub use error::{Result, ScheduleError};
pub use interval::{intersect, normalize, subtract, Budget, Interval};
pub use job::{
    Admissibility, Admission, AdmissionRequest, Admitted, BlockReason, CalendarStateRequirement,
    Constraint, DurationSecs, Fit, GateRef, JobSchedule, OnDeadline, TerminalAction,
};
pub use schedule::{
    queue_key, Attachment, Schedule, ScheduleDefaults, Scope, MAX_ATTACHMENTS, MAX_CALENDARS,
    MAX_CALENDARS_PER_ATTACHMENT, MAX_EXCEPTIONS_PER_CALENDAR, MAX_WINDOWS_PER_CALENDAR,
};
pub use time::{CivilDate, CivilInstant, ClockTime, Days, FixedOffset, UnixSecs, Weekday};

#[cfg(test)]
mod algebra_tests {
    use super::*;

    /// Small deterministic generator: no proptest dependency, but the same invariants are checked
    /// over a covering set of combinations.
    fn generated_windows() -> Vec<(u16, u16, &'static str)> {
        let starts = [0, 8 * 60, 22 * 60];
        let ends = [60, 17 * 60, 24 * 60, 6 * 60];
        let days = ["mon", "mon-fri", "sat,sun", "*"];
        let mut out = Vec::new();
        for from in starts {
            for to in ends {
                for day in days {
                    if from != to {
                        out.push((from, to, day));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn generated_windows_normalize_to_disjoint_sorted_intervals() {
        let mut count = 0;
        for (from, to, days) in generated_windows() {
            let calendar = Calendar {
                name: "generated".to_owned(),
                offset: FixedOffset::UTC,
                default: Some(DefaultState::Closed),
                windows: vec![WeeklyWindow {
                    id: None,
                    kind: WindowKind::Available,
                    from: ClockTime::from_minutes(from).unwrap(),
                    to: ClockTime::from_minutes(to).unwrap(),
                    days: Days::parse(days).unwrap(),
                }],
                ranges: Vec::new(),
                exceptions: Vec::new(),
            };
            let start = CivilInstant::parse("2026-11-23T00:00Z", FixedOffset::UTC)
                .unwrap()
                .to_unix();
            let end = start + 14 * 86_400;
            let intervals = calendar
                .intervals(start, end, WindowKind::Available, &mut Budget::new(10_000))
                .unwrap();
            assert!(intervals
                .windows(2)
                .all(|pair| pair[0].end <= pair[1].start));
            assert!(intervals.iter().all(|i| i.start >= start && i.end <= end));
            count += 1;
        }
        assert_eq!(count, 48);
    }

    #[test]
    fn intersection_and_subtraction_commute_over_a_small_table() {
        let sets = vec![
            vec![],
            vec![Interval { start: 0, end: 10 }],
            vec![
                Interval { start: 5, end: 20 },
                Interval { start: 30, end: 40 },
            ],
        ];
        for a in &sets {
            for b in &sets {
                assert_eq!(intersect(a, b), intersect(b, a));
                let a_sub_b = subtract(a, b);
                assert!(intersect(&a_sub_b, b).is_empty());
                assert_eq!(intersect(a, &subtract(a, &a_sub_b)), intersect(a, b));
            }
        }
    }

    #[test]
    fn stateless_evaluation_survives_restart_mid_window() {
        let calendar = Calendar {
            name: "nightly".to_owned(),
            offset: FixedOffset::UTC,
            default: Some(DefaultState::Closed),
            windows: vec![WeeklyWindow {
                id: None,
                kind: WindowKind::Available,
                from: ClockTime::parse("01:00").unwrap(),
                to: ClockTime::parse("05:00").unwrap(),
                days: Days::ALL,
            }],
            ranges: Vec::new(),
            exceptions: Vec::new(),
        };
        let at = CivilInstant::parse("2026-11-25T03:00Z", FixedOffset::UTC)
            .unwrap()
            .to_unix();
        let before = calendar
            .state_at(at, 86_400, &mut Budget::new(1_000))
            .unwrap();
        let after = calendar
            .state_at(at, 86_400, &mut Budget::new(1_000))
            .unwrap();
        assert_eq!(before, after);
        assert!(before.available);
    }
}
