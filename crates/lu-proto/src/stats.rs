//! Shared view types for node contribution statistics.
//!
//! The controller's store computes these values once; the operator API, the CLI and `/metrics`
//! all read the same shape. Only fixed state and failure-class names and configured node names
//! may become metric labels, so repository text never reaches a Prometheus label.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{FailureClass, JobState, NodeName};

/// Accepted values of the `by` selector.
pub const STATS_GROUPS: [&str; 3] = ["node", "repo", "state"];
/// The only states counted as terminal jobs in the report.
pub const TERMINAL_STATES: [JobState; 4] = [
    JobState::Done,
    JobState::Failed,
    JobState::Cancelled,
    JobState::Expired,
];
/// Fixed histogram bucket upper bounds in seconds, used by both the JSON report and `/metrics`.
pub const STATS_BUCKETS_SECS: [u64; 8] = [1, 5, 15, 60, 300, 900, 3600, 21600];
/// Upper-bound labels in the order Prometheus expects, including `+Inf`.
pub const STATS_BUCKET_LABELS: [&str; 9] =
    ["1", "5", "15", "60", "300", "900", "3600", "21600", "+Inf"];
/// Maximum number of repositories in each top-repository list.
pub const TOP_REPOSITORIES: usize = 10;

/// How the CLI groups its table. The JSON response always carries the full report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatsGroup {
    #[default]
    Node,
    Repo,
    State,
}

impl StatsGroup {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Repo => "repo",
            Self::State => "state",
        }
    }
}

/// Why a `by` value was not recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseStatsGroupError;

impl fmt::Display for ParseStatsGroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("by must be node, repo or state")
    }
}

impl std::error::Error for ParseStatsGroupError {}

impl FromStr for StatsGroup {
    type Err = ParseStatsGroupError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "node" => Ok(Self::Node),
            "repo" => Ok(Self::Repo),
            "state" => Ok(Self::State),
            _ => Err(ParseStatsGroupError),
        }
    }
}

/// Cumulative histogram counts for one duration, with fixed bucket boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationSummary {
    /// Number of jobs with both timestamps present.
    pub count: u64,
    /// Sum of all samples in whole seconds, for the histogram's `_sum` series.
    pub sum_seconds: u64,
    /// Nearest-rank 50th percentile in whole seconds; `null` when there are no samples.
    pub median_seconds: Option<u64>,
    /// Nearest-rank 90th percentile in whole seconds; `null` when there are no samples.
    pub p90_seconds: Option<u64>,
    /// Cumulative sample counts, keyed by the fixed `le` labels.
    pub buckets: BTreeMap<String, u64>,
}

impl DurationSummary {
    /// A zero-filled summary with every fixed bucket present.
    pub fn empty() -> Self {
        Self {
            count: 0,
            sum_seconds: 0,
            median_seconds: None,
            p90_seconds: None,
            buckets: STATS_BUCKET_LABELS
                .iter()
                .map(|label| ((*label).to_owned(), 0))
                .collect(),
        }
    }
}

/// One normalised repository and the number of terminal jobs attributed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContribution {
    /// `host/owner/name`, never a raw URL.
    pub repository: String,
    pub jobs: u64,
}

/// The same contribution fields for the fleet and for one configured node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsAggregate {
    /// Terminal job counts. All four fixed keys are always present.
    pub states: BTreeMap<String, u64>,
    /// Final failure classes of `failed` jobs. All five fixed keys are always present.
    pub failures: BTreeMap<String, u64>,
    /// Sum of `attempt - 1` over counted terminal jobs.
    pub retries: u64,
    /// Wall-clock `finished_at - started_at`, summed over jobs with both timestamps.
    pub busy_seconds: u64,
    pub runtime: DurationSummary,
    /// `started_at - created_at` over jobs with both timestamps.
    pub queue_wait: DurationSummary,
    /// At most ten normalised repositories, ordered by jobs then repository name.
    pub top_repositories: Vec<RepoContribution>,
    /// Newest terminal `finished_at` in this scope, or `null` when there is none.
    pub last_finished_at: Option<i64>,
}

impl StatsAggregate {
    /// A zero-filled aggregate with all fixed keys and buckets present.
    pub fn empty() -> Self {
        let states = TERMINAL_STATES
            .iter()
            .map(|state| (state.as_str().to_owned(), 0))
            .collect();
        let failures = FailureClass::ALL
            .iter()
            .map(|class| (class.as_str().to_owned(), 0))
            .collect();
        Self {
            states,
            failures,
            retries: 0,
            busy_seconds: 0,
            runtime: DurationSummary::empty(),
            queue_wait: DurationSummary::empty(),
            top_repositories: Vec::new(),
            last_finished_at: None,
        }
    }
}

/// One row of the `nodes` array. `node` is always a configured node name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeContribution {
    pub node: NodeName,
    #[serde(flatten)]
    pub stats: StatsAggregate,
}

/// Complete response of `GET /v1/stats` and `lu stats --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsReport {
    pub by: StatsGroup,
    /// Inclusive lower bound in Unix seconds; `null` means all retained history.
    pub since: Option<i64>,
    /// Node filter from the request, if any.
    pub node: Option<NodeName>,
    pub fleet: StatsAggregate,
    pub nodes: Vec<NodeContribution>,
}

/// Store query behind both `GET /v1/stats` and `/metrics`.
///
/// `nodes` is the configured node list. Supplying it keeps Prometheus label sets bounded to
/// configured names; an empty list lets the store derive rows from jobs (useful for tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatsQuery {
    /// How the caller wants the report grouped in a table. JSON always carries every group.
    pub by: StatsGroup,
    /// Inclusive lower bound on `finished_at`, or `None` for all retained jobs.
    pub since: Option<i64>,
    /// When set, only this node's jobs contribute to the fleet and node sections.
    pub node: Option<NodeName>,
    /// Configured node names, in any order. Zero-valued rows are emitted for every entry.
    pub nodes: Vec<NodeName>,
}

/// Parses `--since`/`?since=`: Unix seconds, a UTC date, or a compact duration before now.
///
/// Durations are sequences such as `30m`, `1h30m` or `7d` using `s`, `m`, `h`, `d` and `w`.
pub fn parse_since(text: &str, now: i64) -> Result<i64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("since must not be empty".to_owned());
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text
            .parse::<i64>()
            .map_err(|_| "since timestamp is out of range".to_owned());
    }
    if text.len() == 10 || text.len() == 20 {
        if let Ok(stamp) = parse_utc_timestamp(text) {
            return Ok(stamp);
        }
    }
    parse_duration(text).map(|secs| now.saturating_sub(secs as i64))
}

fn parse_duration(text: &str) -> Result<u64, String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut total: u64 = 0;
    let mut saw_unit = false;
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(format!("invalid since duration {text:?}"));
        }
        let number = text[start..index]
            .parse::<u64>()
            .map_err(|_| "since duration is out of range".to_owned())?;
        let unit = if index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            match byte {
                b's' => 1,
                b'm' => 60,
                b'h' => 3_600,
                b'd' => 86_400,
                b'w' => 604_800,
                _ => return Err(format!("invalid since duration {text:?}")),
            }
        } else {
            return Err(format!("invalid since duration {text:?}"));
        };
        total = total
            .checked_add(
                number
                    .checked_mul(unit)
                    .ok_or("since duration is out of range")?,
            )
            .ok_or("since duration is out of range")?;
        saw_unit = true;
    }
    if !saw_unit {
        return Err(format!("invalid since duration {text:?}"));
    }
    Ok(total)
}

fn parse_utc_timestamp(text: &str) -> Result<i64, String> {
    let date = &text[..10];
    let bytes = date.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err("invalid UTC date".to_owned());
    }
    let year = number(&date[0..4])? as i64;
    let month = number(&date[5..7])? as i64;
    let day = number(&date[8..10])? as i64;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err("invalid UTC date".to_owned());
    }
    let (hour, minute, second) = if text.len() == 20 {
        let time = &text[10..];
        let b = time.as_bytes();
        if b[0] != b'T' || b[3] != b':' || b[6] != b':' || b[9] != b'Z' {
            return Err("invalid UTC timestamp".to_owned());
        }
        (
            number(&time[1..3])? as i64,
            number(&time[4..6])? as i64,
            number(&time[7..9])? as i64,
        )
    } else {
        (0, 0, 0)
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err("invalid UTC timestamp".to_owned());
    }
    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn number(text: &str) -> Result<u32, String> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("invalid UTC timestamp".to_owned());
    }
    text.parse::<u32>()
        .map_err(|_| "invalid UTC timestamp".to_owned())
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Reduces a repository string to `host/owner/name`, or returns `None` when it cannot be
/// normalised. Credentials, query strings, fragments and a `.git` suffix are dropped; local
/// paths and strings with unexpected characters are not reported.
pub fn normalize_repository(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 512 {
        return None;
    }
    let (authority, path) = if let Some((_, rest)) = raw.split_once("://") {
        let rest = rest.split(['?', '#']).next()?;
        rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p))
    } else if let Some((user_host, path)) = raw.split_once('@') {
        let (host, scp_path) = path.split_once(':').unwrap_or((user_host, path));
        let _ = scp_path;
        if let Some((_, after_colon)) = path.split_once(':') {
            (host, after_colon)
        } else {
            (host, scp_path)
        }
    } else {
        let rest = raw.split(['?', '#']).next()?;
        rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p))
    };
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.to_ascii_lowercase();
    if host.is_empty() || host.len() > 128 {
        return None;
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        return None;
    }
    // A bare local path has no host; only host-like names and URLs are reported.
    if !host.contains(['.', ':']) && !raw.contains("://") && !raw.contains('@') {
        return None;
    }
    let path = path.trim_end_matches('/');
    let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let name = parts.pop()?;
    let owner = parts.pop()?;
    if !parts.is_empty() {
        // Keep exactly host/owner/name; ignore deeper groups and subdirectories.
    }
    let name = name
        .strip_suffix(".git")
        .or_else(|| name.strip_suffix(".GIT"))
        .unwrap_or(name);
    if name.is_empty() || owner.is_empty() || owner.len() > 128 || name.len() > 128 {
        return None;
    }
    for part in [owner, name] {
        if !part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return None;
        }
    }
    Some(format!("{host}/{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_round_trip() {
        for group in STATS_GROUPS {
            let parsed: StatsGroup = group.parse().unwrap();
            assert_eq!(parsed.as_str(), group);
        }
        assert!("other".parse::<StatsGroup>().is_err());
    }

    #[test]
    fn since_accepts_unix_seconds_dates_and_durations() {
        assert_eq!(
            parse_since("1700000000", 1_800_000_000).unwrap(),
            1_700_000_000
        );
        assert_eq!(parse_since("1970-01-01", 100).unwrap(), 0);
        assert_eq!(parse_since("1970-01-01T00:00:01Z", 100).unwrap(), 1);
        assert_eq!(parse_since("1h30m", 10_000).unwrap(), 4_600);
        assert_eq!(parse_since("2d", 500_000).unwrap(), 500_000 - 172_800);
        assert_eq!(parse_since("0s", 10).unwrap(), 10);
        assert_eq!(parse_since("2024-02-29", 10).unwrap(), 1_709_164_800);
    }

    #[test]
    fn since_rejects_garbage() {
        for value in ["", "1x", "1hfoo", "2024-13-01", "2024-02-30", "-5m", "abc"] {
            assert!(parse_since(value, 100).is_err(), "{value}");
        }
    }

    #[test]
    fn repository_is_normalised_and_credentials_are_dropped() {
        let credential = ["s3", "cr3t"].join("");
        let raw = format!("https://user:{credential}@git.example.test:8443/team/app.git?x=1#frag");
        assert_eq!(
            normalize_repository(&raw).as_deref(),
            Some("git.example.test:8443/team/app")
        );
        assert_eq!(
            normalize_repository("git@git.example.test:team/app.git").as_deref(),
            Some("git.example.test/team/app")
        );
        assert_eq!(
            normalize_repository("https://git.example.test/team/app/").as_deref(),
            Some("git.example.test/team/app")
        );
        assert_eq!(normalize_repository("/srv/local/repo"), None);
        assert_eq!(normalize_repository("https://git.example.test/app"), None);
    }

    #[test]
    fn fixed_keys_are_always_present() {
        let empty = StatsAggregate::empty();
        assert_eq!(empty.states.len(), 4);
        assert_eq!(empty.failures.len(), 5);
        assert_eq!(empty.runtime.buckets.len(), STATS_BUCKET_LABELS.len());
        assert!(empty.runtime.median_seconds.is_none());
    }
}
