//! Job-side scheduling constraints. These types deliberately contain no Lu types.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::calendar::WindowKind;
use crate::error::{Result, ScheduleError};

/// A duration on the wire: an integer number of seconds or `Ns`, `Nm`, `Nh`, `Nd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DurationSecs {
    secs: i64,
}

impl DurationSecs {
    pub const fn from_secs(secs: i64) -> Self {
        Self { secs }
    }

    pub const fn secs(self) -> i64 {
        self.secs
    }

    pub fn parse(value: &str) -> Result<Self> {
        let text = value.trim();
        if let Ok(secs) = text.parse::<i64>() {
            return Ok(Self::from_secs(secs));
        }
        let (number, unit) = text.split_at(text.len().saturating_sub(1));
        let number = number
            .parse::<i64>()
            .map_err(|_| ScheduleError::validation(format!("invalid duration {value:?}")))?;
        let multiplier = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => {
                return Err(ScheduleError::validation(format!(
                    "invalid duration {value:?}"
                )))
            }
        };
        Ok(Self::from_secs(number.checked_mul(multiplier).ok_or_else(
            || ScheduleError::validation(format!("duration {value:?} overflows")),
        )?))
    }
}

impl fmt::Display for DurationSecs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.secs)
    }
}

impl Serialize for DurationSecs {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.secs)
    }
}

impl<'de> Deserialize<'de> for DurationSecs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(i64),
            Text(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Number(secs) => Ok(Self::from_secs(secs)),
            Repr::Text(text) => Self::parse(&text).map_err(de::Error::custom),
        }
    }
}

/// What happens when a job has passed its start deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnDeadline {
    #[default]
    Expire,
    /// Ignore the job's soft gates (window, state and fit); hard scope gates still apply.
    Run,
    /// End the job as cancelled with `deadline_missed`.
    Cancel,
}

impl OnDeadline {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expire => "expire",
            Self::Run => "run",
            Self::Cancel => "cancel",
        }
    }
}

/// How long a state-gated window must remain open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Require the job's timeout plus the configured staging and finish allowance.
    #[default]
    Timeout,
    /// Do not require the window to outlast the run.
    None,
    /// Require this many seconds plus the configured allowance.
    Seconds(i64),
}

impl Fit {
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::None => "none",
            Self::Seconds(_) => "duration",
        }
    }
}

impl fmt::Display for Fit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("timeout"),
            Self::None => f.write_str("none"),
            Self::Seconds(secs) => write!(f, "{secs}s"),
        }
    }
}

impl Serialize for Fit {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Timeout => serializer.serialize_str("timeout"),
            Self::None => serializer.serialize_str("none"),
            Self::Seconds(secs) => serializer.serialize_i64(*secs),
        }
    }
}

impl<'de> Deserialize<'de> for Fit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Named(String),
            Number(i64),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Named(text) => match text.as_str() {
                "timeout" => Ok(Self::Timeout),
                "none" => Ok(Self::None),
                other => DurationSecs::parse(other)
                    .map(|d| Self::Seconds(d.secs()))
                    .map_err(de::Error::custom),
            },
            Repr::Number(secs) => Ok(Self::Seconds(secs)),
        }
    }
}

/// Wait until a named calendar enters the wanted state. `lead` and `tail` trim the usable part of
/// a state interval: the gate is `[begin + lead, end - tail)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarStateRequirement {
    pub calendar: String,
    pub state: WindowKind,
    #[serde(default)]
    pub lead: DurationSecs,
    #[serde(default)]
    pub tail: DurationSecs,
}

/// The complete scheduling block stored in a job's `spec_json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSchedule {
    /// Earliest allowed start, Unix seconds UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    /// Latest allowed start, Unix seconds UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<i64>,
    #[serde(default, skip_serializing_if = "is_default_on_deadline")]
    pub on_deadline: OnDeadline,
    /// Name of a calendar that gates starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_calendar_state: Option<CalendarStateRequirement>,
    #[serde(default, skip_serializing_if = "is_default_fit")]
    pub fit: Fit,
}

fn is_default_on_deadline(value: &OnDeadline) -> bool {
    *value == OnDeadline::default()
}

fn is_default_fit(value: &Fit) -> bool {
    *value == Fit::default()
}

impl JobSchedule {
    pub fn validate(&self, path: &str) -> Result<()> {
        if let Some(not_before) = self.not_before {
            if not_before <= 0 {
                return Err(ScheduleError::new(
                    path,
                    "not_before must be a positive Unix time",
                ));
            }
        }
        if let Some(deadline) = self.deadline {
            if deadline <= 0 {
                return Err(ScheduleError::new(
                    path,
                    "deadline must be a positive Unix time",
                ));
            }
        }
        if let (Some(not_before), Some(deadline)) = (self.not_before, self.deadline) {
            if deadline < not_before {
                return Err(ScheduleError::new(path, "deadline is before not_before"));
            }
        }
        if let Some(window) = &self.window {
            if !crate::calendar::valid_calendar_name(window) {
                return Err(ScheduleError::new(
                    path,
                    "window is not a valid calendar name",
                ));
            }
        }
        if let Some(requirement) = &self.requires_calendar_state {
            if !crate::calendar::valid_calendar_name(&requirement.calendar) {
                return Err(ScheduleError::new(
                    path,
                    "requires_calendar_state.calendar is not a valid calendar name",
                ));
            }
            if requirement.lead.secs() < 0 || requirement.tail.secs() < 0 {
                return Err(ScheduleError::new(
                    path,
                    "requires_calendar_state lead and tail must not be negative",
                ));
            }
        }
        match self.fit {
            Fit::Seconds(secs) if secs < 0 => {
                Err(ScheduleError::new(path, "fit must not be negative"))
            }
            _ => Ok(()),
        }
    }

    pub fn has_constraints(&self) -> bool {
        self.not_before.is_some()
            || self.deadline.is_some()
            || self.window.is_some()
            || self.requires_calendar_state.is_some()
            || !self.fit.is_none()
    }
}

/// A closed set of blocker codes used by CLI, API and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    NotBefore,
    Deadline,
    Window,
    RequiresState,
    WindowTooShort,
    GateGlobal,
    GateGroup,
    GateNode,
    GateProvider,
    GateModel,
    UnknownCalendar,
    EvalBudget,
    Clock,
}

impl Constraint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotBefore => "not_before",
            Self::Deadline => "deadline",
            Self::Window => "window",
            Self::RequiresState => "requires_state",
            Self::WindowTooShort => "window_too_short",
            Self::GateGlobal => "gate_global",
            Self::GateGroup => "gate_group",
            Self::GateNode => "gate_node",
            Self::GateProvider => "gate_provider",
            Self::GateModel => "gate_model",
            Self::UnknownCalendar => "unknown_calendar",
            Self::EvalBudget => "eval_budget",
            Self::Clock => "clock",
        }
    }
}

/// Which gate produced a reason. `scope` is one of `global`, `group`, `node`, `provider`, `model`,
/// `window`, or `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRef {
    pub scope: String,
    pub name: Option<String>,
    pub calendar: String,
}

/// One structured blocker. `until` names the next time this constraint can change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockReason {
    pub constraint: Constraint,
    pub message: String,
    pub gate: Option<GateRef>,
    pub rule: Option<String>,
    pub until: Option<i64>,
}

impl BlockReason {
    pub fn new(constraint: Constraint, message: impl Into<String>) -> Self {
        Self {
            constraint,
            message: message.into(),
            gate: None,
            rule: None,
            until: None,
        }
    }

    pub fn until(mut self, until: Option<i64>) -> Self {
        self.until = until;
        self
    }

    pub fn gate(mut self, gate: GateRef) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn rule(mut self, rule: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admitted {
    pub at: i64,
    /// Earliest close among the gates that were admitted.
    pub window_end: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub reasons: Vec<BlockReason>,
    /// The first time any blocker may change, across all reasons.
    pub next_change: Option<i64>,
    /// Set when the deadline must end the job instead of waiting.
    pub terminal: Option<TerminalAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    DeadlineMissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Admitted(Admitted),
    Blocked(Blocked),
}

impl Admission {
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted(_))
    }

    pub fn primary(&self) -> Option<&BlockReason> {
        match self {
            Self::Admitted(_) => None,
            Self::Blocked(blocked) => blocked.reasons.first(),
        }
    }
}

/// Everything the evaluator needs. `group` may be the parent's group for a scoped child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRequest {
    pub now: i64,
    pub eligible_since: i64,
    pub timeout_secs: i64,
    pub job: JobSchedule,
    pub node: Option<String>,
    pub group: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl AdmissionRequest {
    pub fn new(now: i64, job: JobSchedule, timeout_secs: i64) -> Self {
        Self {
            now,
            eligible_since: now,
            timeout_secs,
            job,
            node: None,
            group: None,
            provider: None,
            model: None,
        }
    }
}

/// Result of the submit-time / `lu check` "can this ever fit" check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admissibility {
    Fits { first_start: i64 },
    Never { reasons: Vec<BlockReason> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_wire_formats() {
        assert_eq!(DurationSecs::parse("2m").unwrap().secs(), 120);
        assert_eq!(DurationSecs::parse("90").unwrap().secs(), 90);
        assert!(DurationSecs::parse("2x").is_err());
        let fit: Fit = serde_json::from_str("\"timeout\"").unwrap();
        assert_eq!(fit, Fit::Timeout);
        let fit: Fit = serde_json::from_str("\"5m\"").unwrap();
        assert_eq!(fit, Fit::Seconds(300));
        assert_eq!(serde_json::to_string(&fit).unwrap(), "300");
    }

    #[test]
    fn job_schedule_rejects_unknown_fields() {
        assert!(serde_json::from_str::<JobSchedule>(r#"{"later":1}"#).is_err());
        let job: JobSchedule = serde_json::from_str(r#"{"not_before":10,"deadline":20}"#).unwrap();
        assert!(job.validate("schedule").is_ok());
        assert!(
            serde_json::from_str::<JobSchedule>(r#"{"deadline":5,"not_before":10}"#)
                .unwrap()
                .validate("schedule")
                .is_err()
        );
    }
}
