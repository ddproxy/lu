//! The schedule engine: compiled calendars, attachments and one admission function.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::calendar::{next_fit_start, Calendar, CalendarState, WindowKind};
use crate::error::{Result, ScheduleError};
use crate::interval::{next_boundary, Budget};
use crate::job::{
    Admissibility, Admission, AdmissionRequest, BlockReason, Blocked, Constraint, GateRef,
    JobSchedule, TerminalAction,
};

/// Phase 1 defaults. All durations are seconds and all values are injected from config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDefaults {
    /// How far `explain` and the admissibility check look for a future boundary.
    #[serde(default = "default_horizon")]
    pub horizon_secs: i64,
    /// Step budget for one evaluation. The controller applies it per poll, not per queued job.
    #[serde(default = "default_step_budget")]
    pub step_budget: usize,
    /// Time allowed for staging before the harness starts.
    #[serde(default = "default_staging")]
    pub staging_secs: i64,
    /// Time allowed for collection and finish work after the harness ends.
    #[serde(default = "default_finish")]
    pub finish_secs: i64,
    /// Age at which a job enters the starvation class that outranks all static priorities.
    #[serde(default = "default_starvation")]
    pub starvation_secs: i64,
}

fn default_horizon() -> i64 {
    366 * 86_400
}

fn default_step_budget() -> usize {
    50_000
}

fn default_staging() -> i64 {
    300
}

fn default_finish() -> i64 {
    60
}

fn default_starvation() -> i64 {
    3_600
}

impl Default for ScheduleDefaults {
    fn default() -> Self {
        Self {
            horizon_secs: default_horizon(),
            step_budget: default_step_budget(),
            staging_secs: default_staging(),
            finish_secs: default_finish(),
            starvation_secs: default_starvation(),
        }
    }
}

/// Scope of an attachment. `model` is included so a future provider/model gate needs no new wire
/// form; phase 1 attaches only global, group, node and provider calendars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Group,
    Node,
    Provider,
    Model,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Group => "group",
            Self::Node => "node",
            Self::Provider => "provider",
            Self::Model => "model",
        }
    }
}

/// One attachment shape for every scope. `global` takes no name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub calendars: Vec<String>,
}

/// A complete, validated scheduling configuration. It is pure data: no clock and no I/O.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    #[serde(default)]
    pub defaults: ScheduleDefaults,
    #[serde(default)]
    pub calendars: Vec<Calendar>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// The maximums from design section 3.5.
pub const MAX_CALENDARS: usize = 64;
pub const MAX_WINDOWS_PER_CALENDAR: usize = 64;
pub const MAX_EXCEPTIONS_PER_CALENDAR: usize = 366;
pub const MAX_ATTACHMENTS: usize = 64;
pub const MAX_CALENDARS_PER_ATTACHMENT: usize = 16;

impl Schedule {
    pub fn validate(&self) -> Result<()> {
        if self.defaults.horizon_secs <= 0 {
            return Err(ScheduleError::new(
                "schedule.horizon_secs",
                "must be positive",
            ));
        }
        if self.defaults.step_budget == 0 {
            return Err(ScheduleError::new(
                "schedule.step_budget",
                "must be positive",
            ));
        }
        if self.defaults.starvation_secs <= 0 {
            return Err(ScheduleError::new(
                "schedule.starvation_secs",
                "must be positive",
            ));
        }
        if self.defaults.staging_secs < 0 || self.defaults.finish_secs < 0 {
            return Err(ScheduleError::new(
                "schedule",
                "staging_secs and finish_secs must not be negative",
            ));
        }
        if self.calendars.len() > MAX_CALENDARS {
            return Err(ScheduleError::new(
                "schedule.calendars",
                format!("at most {MAX_CALENDARS} calendars are allowed"),
            ));
        }
        let mut names = BTreeSet::new();
        for (index, calendar) in self.calendars.iter().enumerate() {
            let path = format!("calendar[{index}]");
            calendar.validate().map_err(|e| {
                ScheduleError::new(&path, format!("{}: {}", calendar.name, e.reason()))
            })?;
            if !names.insert(calendar.name.as_str()) {
                return Err(ScheduleError::new(
                    &path,
                    format!("duplicate calendar name {}", calendar.name),
                ));
            }
            if calendar.windows.len() > MAX_WINDOWS_PER_CALENDAR {
                return Err(ScheduleError::new(
                    &path,
                    format!("at most {MAX_WINDOWS_PER_CALENDAR} windows per calendar"),
                ));
            }
            let exceptions = calendar.ranges.len() + calendar.exceptions.len();
            if exceptions > MAX_EXCEPTIONS_PER_CALENDAR {
                return Err(ScheduleError::new(
                    &path,
                    format!("at most {MAX_EXCEPTIONS_PER_CALENDAR} one-off ranges per calendar"),
                ));
            }
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(ScheduleError::new(
                "schedule.attachments",
                format!("at most {MAX_ATTACHMENTS} attachments are allowed"),
            ));
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            let path = format!("attach[{index}]");
            if attachment.scope == Scope::Global && attachment.name.is_some() {
                return Err(ScheduleError::new(
                    &path,
                    "a global attachment must not have a name",
                ));
            }
            if attachment.scope != Scope::Global
                && attachment.name.as_deref().unwrap_or("").is_empty()
            {
                return Err(ScheduleError::new(
                    &path,
                    format!("{} attachment requires a name", attachment.scope.as_str()),
                ));
            }
            if attachment.calendars.len() > MAX_CALENDARS_PER_ATTACHMENT {
                return Err(ScheduleError::new(
                    &path,
                    format!("at most {MAX_CALENDARS_PER_ATTACHMENT} calendars per attachment"),
                ));
            }
            for name in &attachment.calendars {
                if !names.contains(name.as_str()) {
                    return Err(ScheduleError::new(
                        &path,
                        format!("unknown calendar {name}"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn calendar(&self, name: &str) -> Option<&Calendar> {
        self.calendars.iter().find(|calendar| calendar.name == name)
    }

    /// Earliest next calendar boundary after `at`, shared by the wake timer and `lu schedule`.
    pub fn next_change(&self, at: i64) -> Option<i64> {
        let mut budget = Budget::new(self.defaults.step_budget);
        self.calendars
            .iter()
            .filter_map(|calendar| {
                calendar
                    .next_change(at, self.defaults.horizon_secs, &mut budget)
                    .ok()
                    .flatten()
            })
            .min()
    }

    /// Checks that a job's named calendars exist and its block is well formed.
    pub fn validate_job(&self, job: &JobSchedule, path: &str) -> Result<()> {
        job.validate(path)?;
        if let Some(name) = &job.window {
            if self.calendar(name).is_none() {
                return Err(ScheduleError::new(path, format!("unknown calendar {name}")));
            }
        }
        if let Some(requirement) = &job.requires_calendar_state {
            if self.calendar(&requirement.calendar).is_none() {
                return Err(ScheduleError::new(
                    path,
                    format!("unknown calendar {}", requirement.calendar),
                ));
            }
        }
        Ok(())
    }

    fn allowance(&self) -> i64 {
        self.defaults.staging_secs + self.defaults.finish_secs
    }

    fn fit_need(&self, job: &JobSchedule, timeout_secs: i64) -> i64 {
        match job.fit {
            crate::job::Fit::None => 0,
            crate::job::Fit::Timeout => timeout_secs.max(0) + self.allowance(),
            crate::job::Fit::Seconds(secs) => secs.max(0) + self.allowance(),
        }
    }

    /// The single admission function used by dispatch, waiting reasons and `explain`.
    ///
    /// It never searches for a future acceptable start; it only reports the next time the current
    /// blocker can change. `explain` and `admissible` do the wider search outside dispatch.
    pub fn admit(&self, request: &AdmissionRequest) -> Result<Admission> {
        self.validate()?;
        request.job.validate("schedule")?;
        let now = request.now;
        let horizon = self.defaults.horizon_secs;
        let mut reasons = Vec::new();
        let mut budget = Budget::new(self.defaults.step_budget);
        let mut next_change = None::<i64>;
        let mut record = |reason: BlockReason| {
            if let Some(until) = reason.until {
                next_change = Some(next_change.map_or(until, |n| n.min(until)));
            }
            reasons.push(reason);
        };

        if let Some(not_before) = request.job.not_before {
            if now < not_before {
                record(
                    BlockReason::new(Constraint::NotBefore, format!("not_before {not_before}"))
                        .until(Some(not_before)),
                );
            }
        }

        let deadline_missed = request.job.deadline.is_some_and(|deadline| now > deadline);
        if deadline_missed {
            match request.job.on_deadline {
                crate::job::OnDeadline::Expire | crate::job::OnDeadline::Cancel => {
                    record(BlockReason::new(Constraint::Deadline, "deadline missed").until(None));
                    return Ok(Admission::Blocked(Blocked {
                        reasons,
                        next_change,
                        terminal: Some(TerminalAction::DeadlineMissed),
                    }));
                }
                crate::job::OnDeadline::Run => {}
            }
        }
        let soft_enabled = !deadline_missed;

        let mut window_end = None::<i64>;
        let mut fit_gates = Vec::new();
        let mut gates = Vec::new();
        for attachment in &self.attachments {
            let matches = match attachment.scope {
                Scope::Global => true,
                Scope::Group => request
                    .group
                    .as_deref()
                    .is_some_and(|g| attachment.name.as_deref() == Some(g)),
                Scope::Node => request
                    .node
                    .as_deref()
                    .is_some_and(|n| attachment.name.as_deref() == Some(n)),
                Scope::Provider => request
                    .provider
                    .as_deref()
                    .is_some_and(|p| attachment.name.as_deref() == Some(p)),
                Scope::Model => request
                    .model
                    .as_deref()
                    .is_some_and(|m| attachment.name.as_deref() == Some(m)),
            };
            if !matches {
                continue;
            }
            for name in &attachment.calendars {
                gates.push((
                    attachment.scope,
                    attachment.name.clone(),
                    name.clone(),
                    WindowKind::Available,
                    None,
                ));
            }
        }
        if soft_enabled {
            if let Some(name) = &request.job.window {
                gates.push((
                    Scope::Global,
                    None,
                    name.clone(),
                    WindowKind::Available,
                    None,
                ));
            }
            if let Some(requirement) = &request.job.requires_calendar_state {
                gates.push((
                    Scope::Global,
                    None,
                    requirement.calendar.clone(),
                    requirement.state,
                    Some((requirement.lead.secs(), requirement.tail.secs())),
                ));
            }
        }

        for (scope, name, calendar_name, wanted, trim) in gates {
            let Some(calendar) = self.calendar(&calendar_name) else {
                record(
                    BlockReason::new(
                        Constraint::UnknownCalendar,
                        format!("unknown calendar {calendar_name}"),
                    )
                    .gate(GateRef {
                        scope: scope.as_str().to_owned(),
                        name,
                        calendar: calendar_name,
                    }),
                );
                continue;
            };
            let state = match calendar.state_at(now, horizon, &mut budget) {
                Ok(state) => state,
                Err(error) => {
                    record(BlockReason::new(Constraint::EvalBudget, error.reason()));
                    continue;
                }
            };
            let is_fit_gate = scope == Scope::Global && name.is_none();
            let constraint = match scope {
                Scope::Global if is_fit_gate && wanted == WindowKind::Available => {
                    if request.job.window.as_deref() == Some(calendar_name.as_str()) {
                        Constraint::Window
                    } else {
                        Constraint::RequiresState
                    }
                }
                Scope::Global => match wanted {
                    WindowKind::Available => Constraint::GateGlobal,
                    WindowKind::Unavailable => Constraint::RequiresState,
                },
                Scope::Group => Constraint::GateGroup,
                Scope::Node => Constraint::GateNode,
                Scope::Provider => Constraint::GateProvider,
                Scope::Model => Constraint::GateModel,
            };
            let gate = GateRef {
                scope: scope.as_str().to_owned(),
                name: name.clone(),
                calendar: calendar_name.clone(),
            };
            if !state.wanted(wanted) {
                record(
                    BlockReason::new(
                        constraint,
                        format!(
                            "{} is {}; wants {}",
                            calendar_name,
                            availability(state),
                            wanted.as_str()
                        ),
                    )
                    .gate(gate)
                    .rule(state.rule)
                    .until(Some(state.until)),
                );
                continue;
            }
            let mut gate_end = state.until;
            if let Some((lead, tail)) = trim {
                if now < state.since.saturating_add(lead) {
                    record(
                        BlockReason::new(
                            Constraint::RequiresState,
                            format!("{calendar_name} lead has not elapsed"),
                        )
                        .gate(gate)
                        .rule(state.rule)
                        .until(Some(state.since.saturating_add(lead))),
                    );
                    continue;
                }
                gate_end = gate_end.saturating_sub(tail).max(now + 1);
            }
            if is_fit_gate {
                fit_gates.push((calendar.clone(), gate_end, gate));
            }
            window_end = Some(window_end.map_or(gate_end, |end: i64| end.min(gate_end)));
        }

        if soft_enabled && !fit_gates.is_empty() {
            let need = self.fit_need(&request.job, request.timeout_secs);
            if need > 0 {
                let horizon_end = now.saturating_add(horizon);
                for (calendar, gate_end, gate) in &fit_gates {
                    if gate_end.saturating_sub(now) >= need {
                        continue;
                    }
                    let intervals = match calendar.intervals(
                        now,
                        horizon_end,
                        WindowKind::Available,
                        &mut budget,
                    ) {
                        Ok(intervals) => intervals,
                        Err(error) => {
                            record(BlockReason::new(Constraint::EvalBudget, error.reason()));
                            continue;
                        }
                    };
                    let candidate =
                        next_fit_start(&intervals, now.saturating_add(1), need, horizon_end)
                            .or_else(|| next_boundary(&intervals, now, horizon_end));
                    record(
                        BlockReason::new(
                            Constraint::WindowTooShort,
                            format!("{} has less than {need}s remaining", gate.calendar),
                        )
                        .gate(gate.clone())
                        .until(candidate),
                    );
                }
            }
        }

        if !reasons.is_empty() {
            return Ok(Admission::Blocked(Blocked {
                reasons,
                next_change,
                terminal: None,
            }));
        }
        Ok(Admission::Admitted(crate::job::Admitted {
            at: now,
            window_end,
        }))
    }

    /// Decides whether a job has at least one admissible start inside the horizon. Used at submit
    /// and in `lu check`; it is never called from dispatch.
    pub fn admissible(&self, request: &AdmissionRequest) -> Result<Admissibility> {
        self.validate()?;
        let horizon = self.defaults.horizon_secs;
        let start = request
            .now
            .max(request.job.not_before.unwrap_or(request.now));
        if let Some(deadline) = request.job.deadline {
            if deadline < start && request.job.on_deadline != crate::job::OnDeadline::Run {
                return Ok(Admissibility::Never {
                    reasons: vec![BlockReason::new(
                        Constraint::Deadline,
                        "deadline is before the earliest start",
                    )],
                });
            }
        }
        let mut candidates = BTreeSet::new();
        candidates.insert(start);
        if let Some(not_before) = request.job.not_before {
            candidates.insert(not_before);
        }
        let mut budget = Budget::new(self.defaults.step_budget.saturating_mul(4));
        let end = start.saturating_add(horizon);
        let mut names = Vec::new();
        for attachment in &self.attachments {
            let matches = match attachment.scope {
                Scope::Global => true,
                Scope::Group => request
                    .group
                    .as_deref()
                    .is_some_and(|g| attachment.name.as_deref() == Some(g)),
                Scope::Node => request
                    .node
                    .as_deref()
                    .is_some_and(|n| attachment.name.as_deref() == Some(n)),
                Scope::Provider => request
                    .provider
                    .as_deref()
                    .is_some_and(|p| attachment.name.as_deref() == Some(p)),
                Scope::Model => request
                    .model
                    .as_deref()
                    .is_some_and(|m| attachment.name.as_deref() == Some(m)),
            };
            if matches {
                names.extend(attachment.calendars.iter().cloned());
            }
        }
        if let Some(name) = &request.job.window {
            names.push(name.clone());
        }
        if let Some(requirement) = &request.job.requires_calendar_state {
            names.push(requirement.calendar.clone());
        }
        for name in names {
            if let Some(calendar) = self.calendar(&name) {
                let Boundaries { boundaries } =
                    calendar_boundaries(calendar, start, end, &mut budget)?;
                candidates.extend(boundaries);
                if let Some(requirement) = &request.job.requires_calendar_state {
                    if requirement.calendar == name {
                        let lead = requirement.lead.secs();
                        let boundaries: Vec<i64> = candidates.iter().copied().collect();
                        candidates.extend(boundaries.into_iter().map(|t| t.saturating_add(lead)));
                    }
                }
            } else {
                return Ok(Admissibility::Never {
                    reasons: vec![BlockReason::new(
                        Constraint::UnknownCalendar,
                        format!("unknown calendar {name}"),
                    )],
                });
            }
        }
        let mut first_reasons = Vec::new();
        for candidate in candidates {
            if candidate > end {
                continue;
            }
            if let Some(deadline) = request.job.deadline {
                if candidate > deadline && request.job.on_deadline != crate::job::OnDeadline::Run {
                    continue;
                }
            }
            let mut at = request.clone();
            at.now = candidate;
            at.eligible_since = candidate.min(request.eligible_since);
            match self.admit(&at)? {
                Admission::Admitted(_) => {
                    return Ok(Admissibility::Fits {
                        first_start: candidate,
                    })
                }
                Admission::Blocked(blocked) => {
                    if first_reasons.is_empty() {
                        first_reasons = blocked.reasons;
                    }
                }
            }
        }
        Ok(Admissibility::Never {
            reasons: if first_reasons.is_empty() {
                vec![BlockReason::new(
                    Constraint::Window,
                    "no admissible start within the schedule horizon",
                )]
            } else {
                first_reasons
            },
        })
    }
}

struct Boundaries {
    boundaries: Vec<i64>,
}

fn calendar_boundaries(
    calendar: &Calendar,
    start: i64,
    end: i64,
    budget: &mut Budget,
) -> Result<Boundaries> {
    let intervals = calendar.intervals(start, end, WindowKind::Available, budget)?;
    let mut boundaries = Vec::new();
    for interval in &intervals {
        boundaries.push(interval.start);
        boundaries.push(interval.end);
    }
    if let Some(next) = next_boundary(&intervals, start - 1, end) {
        boundaries.push(next);
    }
    Ok(Boundaries { boundaries })
}

fn availability(state: CalendarState) -> &'static str {
    if state.available {
        "available"
    } else {
        "unavailable"
    }
}

/// Deterministic total order. The starvation class outranks every static priority once the bound
/// is reached; within a class, effective priority, eligibility time and creation order decide.
pub fn queue_key(
    now: i64,
    eligible_since: i64,
    priority: u8,
    dispatch_seq: u64,
    starvation_secs: i64,
) -> (u8, u8, i64, u64) {
    let age = now.saturating_sub(eligible_since).max(0);
    let starved = age >= starvation_secs.max(1);
    let slide = starvation_secs.max(3) / 3;
    let boost = if starved {
        0
    } else {
        (age / slide).min(2) as u8
    };
    let effective = priority.saturating_add(boost).min(9);
    (u8::from(starved), effective, eligible_since, dispatch_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{DefaultState, WeeklyWindow};
    use crate::job::{AdmissionRequest, Fit, JobSchedule};
    use crate::time::{CivilInstant, ClockTime, Days, FixedOffset};

    fn nightly() -> Schedule {
        Schedule {
            defaults: ScheduleDefaults {
                horizon_secs: 30 * 86_400,
                step_budget: 10_000,
                staging_secs: 0,
                finish_secs: 0,
                starvation_secs: 3_600,
            },
            calendars: vec![Calendar {
                name: "nightly".to_owned(),
                offset: FixedOffset::UTC,
                default: Some(DefaultState::Closed),
                windows: vec![WeeklyWindow {
                    id: Some("night".to_owned()),
                    kind: WindowKind::Available,
                    from: ClockTime::parse("01:00").unwrap(),
                    to: ClockTime::parse("05:00").unwrap(),
                    days: Days::ALL,
                }],
                ranges: Vec::new(),
                exceptions: Vec::new(),
            }],
            attachments: Vec::new(),
        }
    }

    fn at(text: &str) -> i64 {
        CivilInstant::parse(text, FixedOffset::UTC)
            .unwrap()
            .to_unix()
    }

    #[test]
    fn waits_for_window_and_admits_at_boundary() {
        let schedule = nightly();
        let job = JobSchedule {
            window: Some("nightly".to_owned()),
            fit: Fit::None,
            ..JobSchedule::default()
        };
        let request = AdmissionRequest::new(at("2026-11-24T14:00Z"), job.clone(), 60);
        let blocked = schedule.admit(&request).unwrap();
        let blocked = match blocked {
            Admission::Blocked(blocked) => blocked,
            Admission::Admitted(_) => panic!("should wait"),
        };
        assert_eq!(blocked.reasons[0].constraint, Constraint::Window);
        assert_eq!(blocked.next_change, Some(at("2026-11-25T01:00Z")));
        let request = AdmissionRequest::new(at("2026-11-25T01:00Z"), job, 60);
        assert!(schedule.admit(&request).unwrap().is_admitted());
    }

    #[test]
    fn fit_includes_allowance() {
        let mut schedule = nightly();
        schedule.defaults.staging_secs = 600;
        schedule.defaults.finish_secs = 60;
        let job = JobSchedule {
            window: Some("nightly".to_owned()),
            ..JobSchedule::default()
        };
        let request = AdmissionRequest::new(at("2026-11-25T04:50Z"), job, 600);
        let blocked = match schedule.admit(&request).unwrap() {
            Admission::Blocked(blocked) => blocked,
            Admission::Admitted(_) => panic!("short window must be refused"),
        };
        assert!(blocked
            .reasons
            .iter()
            .any(|reason| reason.constraint == Constraint::WindowTooShort));
    }

    #[test]
    fn state_gate_lead_tail_and_fit() {
        let mut schedule = nightly();
        schedule.calendars[0].name = "outage".to_owned();
        schedule.calendars[0].default = Some(DefaultState::Open);
        schedule.calendars[0].windows[0].kind = WindowKind::Unavailable;
        schedule.calendars[0].windows[0].from = ClockTime::parse("22:00").unwrap();
        schedule.calendars[0].windows[0].to = ClockTime::parse("02:00").unwrap();
        let job = JobSchedule {
            requires_calendar_state: Some(crate::job::CalendarStateRequirement {
                calendar: "outage".to_owned(),
                state: WindowKind::Unavailable,
                lead: crate::job::DurationSecs::from_secs(120),
                tail: crate::job::DurationSecs::from_secs(300),
            }),
            fit: Fit::None,
            ..JobSchedule::default()
        };
        let early = AdmissionRequest::new(at("2026-11-25T22:01Z"), job.clone(), 60);
        let blocked = schedule.admit(&early).unwrap();
        assert_eq!(
            blocked.primary().map(|r| r.constraint),
            Some(Constraint::RequiresState)
        );
        assert_eq!(
            blocked.primary().and_then(|r| r.until),
            Some(at("2026-11-25T22:02Z"))
        );
        let after_lead = AdmissionRequest::new(at("2026-11-25T22:10Z"), job.clone(), 60);
        assert!(schedule.admit(&after_lead).unwrap().is_admitted());
        let inside = AdmissionRequest::new(at("2026-11-25T23:30Z"), job, 60);
        let result = schedule.admit(&inside).unwrap();
        assert!(result.is_admitted(), "{result:?}");
    }

    #[test]
    fn admissibility_refuses_empty_intersection() {
        let mut schedule = nightly();
        schedule.calendars.push(Calendar {
            name: "day".to_owned(),
            offset: FixedOffset::UTC,
            default: Some(DefaultState::Closed),
            windows: vec![WeeklyWindow {
                id: None,
                kind: WindowKind::Available,
                from: ClockTime::parse("09:00").unwrap(),
                to: ClockTime::parse("17:00").unwrap(),
                days: Days::ALL,
            }],
            ranges: Vec::new(),
            exceptions: Vec::new(),
        });
        let job = JobSchedule {
            window: Some("nightly".to_owned()),
            requires_calendar_state: Some(crate::job::CalendarStateRequirement {
                calendar: "day".to_owned(),
                state: WindowKind::Available,
                lead: crate::job::DurationSecs::default(),
                tail: crate::job::DurationSecs::default(),
            }),
            fit: Fit::None,
            ..JobSchedule::default()
        };
        let request = AdmissionRequest::new(at("2026-11-24T00:00Z"), job, 60);
        assert!(matches!(
            schedule.admissible(&request).unwrap(),
            Admissibility::Never { .. }
        ));
    }

    #[test]
    fn starvation_bound_outranks_default_priority() {
        let now = at("2026-11-24T12:00Z");
        let low = queue_key(now, now - 3_600, 0, 99, 3_600);
        let default = queue_key(now, now, 5, 1, 3_600);
        assert!(low > default);
        let fresh = queue_key(now, now, 0, 99, 3_600);
        assert!(fresh < default);
    }
}
