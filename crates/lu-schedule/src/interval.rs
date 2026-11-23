//! Half-open interval algebra over injected Unix time.

use crate::error::{Result, ScheduleError};

/// `[start, end)` in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    pub start: i64,
    pub end: i64,
}

impl Interval {
    pub fn new(start: i64, end: i64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn contains(self, at: i64) -> bool {
        self.start <= at && at < self.end
    }

    pub fn length(self) -> i64 {
        self.end - self.start
    }
}

/// A step budget prevents a malformed or enormous rule set from stalling dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    remaining: usize,
}

impl Budget {
    pub const fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    pub fn spend(&mut self, steps: usize) -> Result<()> {
        if steps > self.remaining {
            self.remaining = 0;
            return Err(ScheduleError::validation(
                "schedule evaluation step budget exceeded",
            ));
        }
        self.remaining -= steps;
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }
}

/// Sorts, merges overlaps and adjacent intervals, and drops empty ones.
pub fn normalize(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.sort_unstable_by_key(|i| (i.start, i.end));
    let mut out: Vec<Interval> = Vec::with_capacity(intervals.len());
    for interval in intervals {
        if interval.start >= interval.end {
            continue;
        }
        match out.last_mut() {
            Some(last) if interval.start <= last.end => {
                last.end = last.end.max(interval.end);
            }
            _ => out.push(interval),
        }
    }
    out
}

/// Intersection of two normalized interval lists.
pub fn intersect(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].start.max(b[j].start);
        let end = a[i].end.min(b[j].end);
        if start < end {
            out.push(Interval { start, end });
        }
        if a[i].end <= b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `a - b`, where both inputs are normalized.
pub fn subtract(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    for &interval in a {
        let mut cursor = interval.start;
        for &cut in b {
            if cut.end <= cursor || cut.start >= interval.end {
                continue;
            }
            if cut.start > cursor {
                out.push(Interval {
                    start: cursor,
                    end: cut.start.min(interval.end),
                });
            }
            cursor = cursor.max(cut.end);
            if cursor >= interval.end {
                break;
            }
        }
        if cursor < interval.end {
            out.push(Interval {
                start: cursor,
                end: interval.end,
            });
        }
    }
    out
}

/// The interval containing `at`, if any.
pub fn containing(intervals: &[Interval], at: i64) -> Option<Interval> {
    intervals
        .iter()
        .find(|interval| interval.contains(at))
        .copied()
}

/// The first boundary strictly after `at`; `None` means the state is constant for the horizon.
pub fn next_boundary(intervals: &[Interval], at: i64, horizon_end: i64) -> Option<i64> {
    let mut next = None;
    for interval in intervals {
        for boundary in [interval.start, interval.end] {
            if boundary > at && boundary < horizon_end {
                next = Some(next.map_or(boundary, |n: i64| n.min(boundary)));
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(start: i64, end: i64) -> Interval {
        Interval { start, end }
    }

    #[test]
    fn normalize_merges_adjacent_and_overlapping() {
        assert_eq!(
            normalize(vec![i(3, 5), i(0, 2), i(2, 4), i(9, 9)]),
            vec![i(0, 5)]
        );
    }

    #[test]
    fn intersection_and_subtraction() {
        let a = normalize(vec![i(0, 10), i(20, 30)]);
        let b = normalize(vec![i(5, 25)]);
        assert_eq!(intersect(&a, &b), vec![i(5, 10), i(20, 25)]);
        assert_eq!(subtract(&a, &b), vec![i(0, 5), i(25, 30)]);
    }

    #[test]
    fn budget_is_bounded() {
        let mut budget = Budget::new(3);
        assert!(budget.spend(2).is_ok());
        assert!(budget.spend(2).is_err());
    }
}
