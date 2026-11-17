//! State and view model for the dashboard; no terminal is touched, so it tests without a tty.

use std::time::{Duration, Instant};

use crate::sanitize::printable;

/// One provider on a node, already reduced to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCell {
    pub name: String,
    /// `ok`, `cooling`, `disabled` or `unknown`.
    pub state: String,
    /// Seconds until the soonest key is usable, when the node reports it.
    pub soonest_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub name: String,
    /// `idle`, `busy` or `-`.
    pub harness: String,
    pub drained: bool,
    /// Seconds since the node's last heartbeat, when it has sent one.
    pub seen_secs_ago: Option<u64>,
    pub providers: Vec<ProviderCell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: String,
    /// What the job is for, from its task's first line.
    pub title: String,
    pub state: String,
    pub node: Option<String>,
    pub attempt: u32,
    /// Seconds since the job was created.
    pub age_secs: u64,
    pub model: Option<String>,
    /// The failure class or summary, shortened by the caller.
    pub note: String,
}

/// Everything one refresh brings back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub nodes: Vec<NodeRow>,
    pub jobs: Vec<JobRow>,
}

/// Line colour; the model decides it and the renderer maps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Heading,
    Good,
    Warn,
    Bad,
    Dim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub tone: Tone,
}

impl Line {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// One reading of the fleet, kept to draw the charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub running: u64,
    pub queued: u64,
    pub busy_nodes: u64,
    pub providers_ok: u64,
}

impl Sample {
    fn of(snapshot: &Snapshot) -> Self {
        let count = |states: &[&str]| {
            snapshot
                .jobs
                .iter()
                .filter(|job| states.contains(&job.state.as_str()))
                .count() as u64
        };
        Self {
            running: count(&["running", "leased"]),
            queued: count(&["queued", "recovering"]),
            busy_nodes: snapshot
                .nodes
                .iter()
                .filter(|n| n.harness == "busy")
                .count() as u64,
            providers_ok: snapshot
                .nodes
                .iter()
                .flat_map(|n| &n.providers)
                .filter(|p| p.state == "ok")
                .count() as u64,
        }
    }
}

/// Samples kept for the charts: about five minutes at the refresh rate.
const HISTORY: usize = 150;

/// One chart: a title, its recent values and the latest one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chart {
    pub title: String,
    pub values: Vec<u64>,
    pub latest: u64,
}

/// A node silent for longer than this is shown as stale.
const STALE_SECS: u64 = 60;

/// Pausing keeps a copy with its age, so a paused screen never looks live.
#[derive(Debug, Clone, Default)]
pub struct Dashboard {
    live: Snapshot,
    updated: Option<Instant>,
    error: Option<String>,
    frozen: Option<(Snapshot, Vec<Sample>, Instant)>,
    history: Vec<Sample>,
    updates: u64,
}

impl Dashboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// A failed refresh keeps the last good data and shows the error beside it.
    pub fn apply(&mut self, result: Result<Snapshot, String>, now: Instant) {
        match result {
            Ok(snapshot) => {
                self.history.push(Sample::of(&snapshot));
                if self.history.len() > HISTORY {
                    self.history.remove(0);
                }
                self.live = snapshot;
                self.updated = Some(now);
                self.error = None;
                self.updates += 1;
            }
            Err(error) => self.error = Some(printable(&error)),
        }
    }

    pub fn paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// Freezes a copy of the live data, or resumes when already paused.
    pub fn toggle_pause(&mut self, now: Instant) {
        self.frozen = match self.frozen.take() {
            Some(_) => None,
            None => Some((self.live.clone(), self.history.clone(), now)),
        };
    }

    pub fn view(&self, now: Instant) -> ViewModel {
        match &self.frozen {
            Some((snapshot, history, taken)) => ViewModel::build(
                snapshot,
                history,
                Status::Paused(now.saturating_duration_since(*taken)),
            ),
            None => {
                let status = match (&self.error, self.updated) {
                    (Some(error), _) => Status::Failing(error.clone()),
                    (None, Some(at)) => Status::Live {
                        age: now.saturating_duration_since(at),
                        updates: self.updates,
                    },
                    (None, None) => Status::Waiting,
                };
                ViewModel::build(&self.live, &self.history, status)
            }
        }
    }
}

enum Status {
    Waiting,
    Live { age: Duration, updates: u64 },
    Failing(String),
    Paused(Duration),
}

/// What the screen shows; the renderer only lays the lines out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModel {
    pub title: String,
    pub charts: Vec<Chart>,
    pub lines: Vec<Line>,
    pub status: Line,
    pub footer: String,
    pub paused: bool,
}

fn short_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d", secs / 86_400),
    }
}

fn clip(text: &str, width: usize) -> String {
    let text = printable(text);
    if text.chars().count() <= width {
        return text;
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn provider_text(cell: &ProviderCell) -> (String, Tone) {
    let name = printable(&cell.name);
    match (cell.state.as_str(), cell.soonest_secs) {
        ("ok", _) => (format!("{name} ok"), Tone::Good),
        ("cooling", Some(secs)) => (format!("{name} cooling {}", short_age(secs)), Tone::Warn),
        ("cooling", None) => (format!("{name} cooling"), Tone::Warn),
        ("disabled", _) => (format!("{name} disabled"), Tone::Bad),
        (other, _) => (format!("{name} {}", printable(other)), Tone::Dim),
    }
}

fn job_tone(state: &str) -> Tone {
    match state {
        "running" | "leased" => Tone::Good,
        "queued" | "recovering" | "needs_input" => Tone::Warn,
        "failed" => Tone::Bad,
        _ => Tone::Dim,
    }
}

fn worst(tones: impl Iterator<Item = Tone>) -> Tone {
    tones.fold(Tone::Good, |acc, tone| match (acc, tone) {
        (Tone::Bad, _) | (_, Tone::Bad) => Tone::Bad,
        (Tone::Warn, _) | (_, Tone::Warn) => Tone::Warn,
        (acc, _) => acc,
    })
}

fn charts(history: &[Sample]) -> Vec<Chart> {
    let series = |title: &str, pick: fn(&Sample) -> u64| {
        let values: Vec<u64> = history.iter().map(pick).collect();
        Chart {
            title: title.to_owned(),
            latest: values.last().copied().unwrap_or(0),
            values,
        }
    };
    vec![
        series("running jobs", |s| s.running),
        series("queued jobs", |s| s.queued),
        series("busy nodes", |s| s.busy_nodes),
        series("providers ok", |s| s.providers_ok),
    ]
}

impl ViewModel {
    fn build(snapshot: &Snapshot, history: &[Sample], status: Status) -> Self {
        let mut lines = vec![Line::new(
            format!("{:<12} {:<8} {:<8} PROVIDERS", "NODE", "STATE", "SEEN"),
            Tone::Heading,
        )];
        if snapshot.nodes.is_empty() {
            lines.push(Line::new("no nodes have reported", Tone::Dim));
        }
        for node in &snapshot.nodes {
            let stale = node.seen_secs_ago.is_none_or(|age| age > STALE_SECS);
            let (state, tone) = if stale {
                ("stale".to_owned(), Tone::Bad)
            } else if node.drained {
                ("drained".to_owned(), Tone::Warn)
            } else {
                (printable(&node.harness), Tone::Plain)
            };
            let seen = node.seen_secs_ago.map_or("-".to_owned(), short_age);
            let cells: Vec<(String, Tone)> = node.providers.iter().map(provider_text).collect();
            let providers: Vec<&str> = cells.iter().map(|(text, _)| text.as_str()).collect();
            let tone = if tone == Tone::Plain && !cells.is_empty() {
                worst(cells.iter().map(|(_, tone)| *tone))
            } else {
                tone
            };
            lines.push(Line::new(
                format!(
                    "{:<12} {:<8} {:<8} {}",
                    clip(&node.name, 12),
                    state,
                    seen,
                    providers.join("   ")
                ),
                tone,
            ));
        }
        lines.push(Line::new("", Tone::Plain));
        lines.push(Line::new(
            format!(
                "{:<10} {:<11} {:<9} {:>3} {:<6} {:<38} NOTE",
                "JOB", "STATE", "NODE", "TRY", "AGE", "TITLE"
            ),
            Tone::Heading,
        ));
        if snapshot.jobs.is_empty() {
            lines.push(Line::new("no jobs", Tone::Dim));
        }
        for job in &snapshot.jobs {
            let node = job.node.as_deref().map_or("-".to_owned(), |n| clip(n, 9));
            let id = printable(&job.id);
            let short_id: String = id
                .chars()
                .skip(id.chars().count().saturating_sub(10))
                .collect();
            let note = match (&job.model, job.note.is_empty()) {
                (Some(model), true) => model.as_str(),
                _ => job.note.as_str(),
            };
            lines.push(Line::new(
                format!(
                    "{:<10} {:<11} {:<9} {:>3} {:<6} {:<38} {}",
                    short_id,
                    clip(&job.state, 11),
                    node,
                    job.attempt,
                    short_age(job.age_secs),
                    clip(&job.title, 38),
                    clip(note, 60)
                ),
                job_tone(&job.state),
            ));
        }

        let (status, paused) = match status {
            Status::Waiting => (Line::new("connecting…", Tone::Dim), false),
            Status::Live { age, updates } => (
                Line::new(
                    format!("live, update {updates}, {}s ago", age.as_secs()),
                    Tone::Plain,
                ),
                false,
            ),
            Status::Failing(error) => (
                Line::new(
                    format!(
                        "refresh failed, showing the last data: {}",
                        clip(&error, 80)
                    ),
                    Tone::Bad,
                ),
                false,
            ),
            Status::Paused(age) => (
                Line::new(
                    format!("paused: showing data from {}s ago", age.as_secs()),
                    Tone::Warn,
                ),
                true,
            ),
        };
        Self {
            title: "lu top".to_owned(),
            charts: charts(history),
            lines,
            status,
            footer: "p pause/resume   q quit".to_owned(),
            paused,
        }
    }

    /// The whole screen as text, one line per row; tests assert against this.
    pub fn plain_text(&self) -> String {
        let mut lines = vec![self.title.clone()];
        lines.extend(self.lines.iter().map(|line| line.text.clone()));
        lines.push(self.status.text.clone());
        lines.push(self.footer.clone());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, harness: &str, seen: Option<u64>) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            harness: harness.to_owned(),
            drained: false,
            seen_secs_ago: seen,
            providers: vec![
                ProviderCell {
                    name: "zen".to_owned(),
                    state: "cooling".to_owned(),
                    soonest_secs: Some(7200),
                },
                ProviderCell {
                    name: "openrouter".to_owned(),
                    state: "ok".to_owned(),
                    soonest_secs: None,
                },
            ],
        }
    }

    fn job(id: &str, state: &str) -> JobRow {
        JobRow {
            id: id.to_owned(),
            title: "do the thing".to_owned(),
            state: state.to_owned(),
            node: Some("ct-1".to_owned()),
            attempt: 1,
            age_secs: 125,
            model: Some("m".to_owned()),
            note: String::new(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            nodes: vec![
                node("ct-1", "busy", Some(3)),
                node("ct-2", "idle", Some(90)),
            ],
            jobs: vec![job("01ABC", "running")],
        }
    }

    #[test]
    fn nodes_and_jobs_are_laid_out_with_providers_and_ages() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        let text = dashboard.view(t0).plain_text();
        assert!(text.contains("zen cooling 2h00m"), "{text}");
        assert!(text.contains("openrouter ok"), "{text}");
        assert!(text.contains("01ABC"), "{text}");
        assert!(text.contains("do the thing"), "{text}");
        assert!(text.contains("2m"), "{text}");
    }

    #[test]
    fn charts_track_the_recent_counts_and_freeze_on_pause() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        dashboard.apply(Ok(snapshot()), t0);
        let view = dashboard.view(t0);
        let running = view
            .charts
            .iter()
            .find(|c| c.title == "running jobs")
            .unwrap();
        assert_eq!(running.values, vec![1, 1]);
        assert_eq!(running.latest, 1);
        dashboard.toggle_pause(t0);
        dashboard.apply(Ok(snapshot()), t0);
        let paused = dashboard.view(t0);
        let running = paused
            .charts
            .iter()
            .find(|c| c.title == "running jobs")
            .unwrap();
        assert_eq!(running.values.len(), 2);
    }

    #[test]
    fn history_is_bounded() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        for _ in 0..(HISTORY + 20) {
            dashboard.apply(Ok(snapshot()), t0);
        }
        assert_eq!(dashboard.view(t0).charts[0].values.len(), HISTORY);
    }

    #[test]
    fn a_node_silent_for_a_minute_is_stale() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        let view = dashboard.view(t0);
        let ct2 = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("ct-2"))
            .unwrap();
        assert!(ct2.text.contains("stale"));
        assert_eq!(ct2.tone, Tone::Bad);
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_data_and_says_so() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        dashboard.apply(Err("connection refused".to_owned()), t0);
        let view = dashboard.view(t0);
        assert!(view.status.text.contains("refresh failed"));
        assert!(view.plain_text().contains("01ABC"));
        dashboard.apply(Ok(snapshot()), t0);
        assert!(dashboard.view(t0).status.text.starts_with("live"));
    }

    #[test]
    fn a_paused_view_keeps_the_data_from_the_moment_of_the_pause() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        dashboard.toggle_pause(t0);
        let mut changed = snapshot();
        changed.jobs.push(job("01NEW", "queued"));
        dashboard.apply(Ok(changed), t0);
        let later = dashboard.view(t0 + Duration::from_secs(10));
        assert!(later.paused);
        assert!(!later.plain_text().contains("01NEW"));
        assert!(later.status.text.contains("10s ago"));
        dashboard.toggle_pause(t0);
        assert!(dashboard.view(t0).plain_text().contains("01NEW"));
    }

    #[test]
    fn names_from_the_network_are_sanitized() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut snap = snapshot();
        snap.nodes[0].name = "peer\u{1b}[31m".to_owned();
        snap.jobs[0].note = "x\u{1b}[2J".to_owned();
        dashboard.apply(Ok(snap), t0);
        assert!(!dashboard.view(t0).plain_text().contains('\u{1b}'));
    }

    #[test]
    fn an_empty_dashboard_renders_its_chrome() {
        let text = Dashboard::new().view(Instant::now()).plain_text();
        assert!(text.contains("no nodes have reported"));
        assert!(text.contains("no jobs"));
        assert!(text.contains("q quit"));
    }
}
