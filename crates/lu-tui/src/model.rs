//! State and view model for the dashboard; no terminal is touched, so it tests without a tty.

use std::time::{Duration, Instant};

use crate::graph::{self, Graph};
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
    /// The ids this job waits for.
    pub after: Vec<String>,
}

/// Everything one refresh brings back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub nodes: Vec<NodeRow>,
    pub jobs: Vec<JobRow>,
}

/// Which table the selection keys act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Nodes,
    Jobs,
}

#[derive(Debug, Clone, Copy, Default)]
struct Selection {
    focus: Focus,
    node: usize,
    job: usize,
}

impl Selection {
    fn clamp(&mut self, snapshot: &Snapshot) {
        self.node = self.node.min(snapshot.nodes.len().saturating_sub(1));
        self.job = self.job.min(snapshot.jobs.len().saturating_sub(1));
    }
}

/// The log tail shown under the tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPane {
    pub job_id: String,
    pub title: String,
    pub lines: Vec<Line>,
}

/// Work the UI asks the feeder to do; the screen keeps running while it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Log { job_id: String },
    Drain { node: String, drain: bool },
}

/// What the feeder sends back over the second channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Log {
        job_id: String,
        result: Result<String, String>,
    },
    Drain {
        node: String,
        drain: bool,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainRequest {
    pub node: String,
    pub drain: bool,
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
    /// A finished job: dim green.
    Done,
    /// A blocked job: orange.
    Blocked,
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
            queued: count(&["queued", "blocked", "recovering"]),
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

/// Lines kept in the log pane.
const LOG_LINES: usize = 20;

/// A drain or undrain must be confirmed within this window.
const DRAIN_CONFIRM: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
struct PendingDrain {
    node: String,
    drain: bool,
    at: Instant,
}

/// Pausing keeps a copy with its age, so a paused screen never looks live.
#[derive(Debug, Clone, Default)]
pub struct Dashboard {
    live: Snapshot,
    updated: Option<Instant>,
    error: Option<String>,
    frozen: Option<(Snapshot, Vec<Sample>, Instant)>,
    history: Vec<Sample>,
    updates: u64,
    selection: Selection,
    log: Option<LogPane>,
    pending: Option<PendingDrain>,
    notice: Option<(String, Tone)>,
    graph_hidden: bool,
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
                self.selection.clamp(&snapshot);
                self.live = snapshot;
                self.updated = Some(now);
                self.error = None;
                self.updates += 1;
            }
            Err(error) => self.error = Some(printable(&error)),
        }
    }

    /// Applies a feeder result; a stale log answer is ignored.
    pub fn apply_update(&mut self, update: Update) {
        match update {
            Update::Log { job_id, result } => self.apply_log(&job_id, result),
            Update::Drain {
                node,
                drain,
                result,
            } => self.apply_drain(&node, drain, result),
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

    fn current(&self) -> &Snapshot {
        match &self.frozen {
            Some((snapshot, _, _)) => snapshot,
            None => &self.live,
        }
    }

    /// Moves the highlighted row in the focused table.
    pub fn move_selection(&mut self, down: bool) {
        self.pending = None;
        let len = match self.selection.focus {
            Focus::Nodes => self.current().nodes.len(),
            Focus::Jobs => self.current().jobs.len(),
        };
        let index = match self.selection.focus {
            Focus::Nodes => &mut self.selection.node,
            Focus::Jobs => &mut self.selection.job,
        };
        if len == 0 {
            *index = 0;
        } else if down {
            *index = (*index + 1).min(len - 1);
        } else {
            *index = index.saturating_sub(1);
        }
    }

    /// Hides or shows the dependency graph under the job list.
    pub fn toggle_graph(&mut self) {
        self.graph_hidden = !self.graph_hidden;
    }

    /// Switches the arrows and `d` between the node table and the job list.
    pub fn toggle_focus(&mut self) {
        self.pending = None;
        self.selection.focus = match self.selection.focus {
            Focus::Nodes => Focus::Jobs,
            Focus::Jobs => Focus::Nodes,
        };
    }

    /// Opens the selected job's log pane and returns its id for the feeder to fetch.
    pub fn open_selected_log(&mut self) -> Option<String> {
        if self.selection.focus != Focus::Jobs {
            return None;
        }
        let job = self.current().jobs.get(self.selection.job)?.clone();
        let job_id = job.id.clone();
        self.log = Some(LogPane {
            job_id: job.id.clone(),
            title: printable(&job.title),
            lines: vec![Line::new("loading log…", Tone::Dim)],
        });
        self.pending = None;
        Some(job_id)
    }

    /// Closes the log pane; returns whether one was open.
    pub fn close_log(&mut self) -> bool {
        self.log.take().is_some()
    }

    fn apply_log(&mut self, job_id: &str, result: Result<String, String>) {
        let Some(log) = &mut self.log else {
            return;
        };
        if log.job_id != job_id {
            return;
        }
        match result {
            Ok(text) => {
                let mut lines: Vec<String> = text.lines().map(printable).collect();
                if lines.is_empty() {
                    lines.push("(no log yet)".to_owned());
                }
                let start = lines.len().saturating_sub(LOG_LINES);
                log.lines = lines[start..]
                    .iter()
                    .map(|line| Line::new(line.clone(), Tone::Plain))
                    .collect();
            }
            Err(error) => {
                log.lines = vec![Line::new(
                    format!("log unavailable: {}", printable(&error)),
                    Tone::Bad,
                )];
            }
        }
    }

    /// The first `d` on a node asks for a second; the second within the window returns the work.
    pub fn request_drain(&mut self, now: Instant) -> Option<DrainRequest> {
        if self.selection.focus != Focus::Nodes {
            return None;
        }
        let node = self.current().nodes.get(self.selection.node)?.clone();
        let drain = !node.drained;
        let confirmed = match self.pending.take() {
            Some(pending) => {
                pending.node == node.name
                    && pending.drain == drain
                    && now.saturating_duration_since(pending.at) <= DRAIN_CONFIRM
            }
            None => false,
        };
        if confirmed {
            self.notice = Some((
                format!(
                    "{} {}…",
                    if drain { "draining" } else { "undraining" },
                    printable(&node.name)
                ),
                Tone::Plain,
            ));
            Some(DrainRequest {
                node: node.name,
                drain,
            })
        } else {
            self.pending = Some(PendingDrain {
                node: node.name,
                drain,
                at: now,
            });
            self.notice = None;
            None
        }
    }

    fn apply_drain(&mut self, node: &str, drain: bool, result: Result<(), String>) {
        match result {
            Ok(()) => {
                for row in &mut self.live.nodes {
                    if row.name == node {
                        row.drained = drain;
                    }
                }
                if let Some((snapshot, _, _)) = &mut self.frozen {
                    for row in &mut snapshot.nodes {
                        if row.name == node {
                            row.drained = drain;
                        }
                    }
                }
                self.notice = Some((
                    format!(
                        "{} {}",
                        if drain { "drained" } else { "undrained" },
                        printable(node)
                    ),
                    Tone::Good,
                ));
            }
            Err(error) => {
                self.notice = Some((format!("drain failed: {}", printable(&error)), Tone::Bad));
            }
        }
    }

    fn notice_status(&self, now: Instant) -> Option<Status> {
        if let Some((text, tone)) = &self.notice {
            return Some(Status::Notice {
                text: text.clone(),
                tone: *tone,
            });
        }
        let pending = self.pending.as_ref()?;
        if now.saturating_duration_since(pending.at) > DRAIN_CONFIRM {
            return None;
        }
        Some(Status::Notice {
            text: format!(
                "press d again within {}s to {} {}",
                DRAIN_CONFIRM.as_secs(),
                if pending.drain { "drain" } else { "undrain" },
                printable(&pending.node)
            ),
            tone: Tone::Warn,
        })
    }

    pub fn view(&self, now: Instant) -> ViewModel {
        let (snapshot, history, live_status) = match &self.frozen {
            Some((snapshot, history, taken)) => (
                snapshot,
                history.as_slice(),
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
                (&self.live, self.history.as_slice(), status)
            }
        };
        let status = self.notice_status(now).unwrap_or(live_status);
        ViewModel::build(
            snapshot,
            history,
            status,
            self.selection,
            self.log.as_ref(),
            !self.graph_hidden,
        )
    }
}

enum Status {
    Waiting,
    Live { age: Duration, updates: u64 },
    Failing(String),
    Paused(Duration),
    Notice { text: String, tone: Tone },
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
    /// Index into `lines` of the highlighted row.
    pub selected_line: Option<usize>,
    pub log: Option<LogPane>,
    /// The dependency graph under the job list, when it has a component to show.
    pub graph: Option<Graph>,
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
        "done" => Tone::Done,
        "running" | "leased" => Tone::Good,
        "blocked" => Tone::Blocked,
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
    fn build(
        snapshot: &Snapshot,
        history: &[Sample],
        status: Status,
        selection: Selection,
        log: Option<&LogPane>,
        show_graph: bool,
    ) -> Self {
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

        let node_rows = snapshot.nodes.len().max(1);
        let selected_line = match selection.focus {
            Focus::Nodes if !snapshot.nodes.is_empty() => {
                Some(1 + selection.node.min(snapshot.nodes.len() - 1))
            }
            Focus::Jobs if !snapshot.jobs.is_empty() => {
                Some(3 + node_rows + selection.job.min(snapshot.jobs.len() - 1))
            }
            _ => None,
        };

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
            Status::Notice { text, tone } => (Line::new(clip(&text, 120), tone), false),
        };
        let graph = show_graph.then(|| {
            let jobs: Vec<graph::Job> = snapshot
                .jobs
                .iter()
                .map(|job| graph::Job {
                    id: job.id.clone(),
                    state: graph::State::from_name(&job.state),
                    title: job.title.clone(),
                    after: job.after.clone(),
                })
                .collect();
            graph::Graph::build(&jobs)
        });
        Self {
            title: "lu top".to_owned(),
            charts: charts(history),
            lines,
            status,
            footer: "↑↓/jk select   Tab focus   Enter log   Esc close   d drain   g graph   p pause   q quit"
                .to_owned(),
            paused,
            selected_line,
            log: log.cloned(),
            graph: graph.filter(|graph| !graph.is_empty()),
        }
    }

    /// The two tables, without the title, status, footer or charts; `--once` prints this.
    pub fn table_text(&self) -> String {
        let mut lines: Vec<String> = self.lines.iter().map(|line| line.text.clone()).collect();
        if let Some(graph) = &self.graph {
            let graph_lines: Vec<String> = graph::render(graph, 100)
                .iter()
                .map(graph::GraphLine::text)
                .collect();
            if !graph_lines.is_empty() {
                lines.push(String::new());
                lines.extend(graph_lines);
            }
        }
        lines.join("\n")
    }

    /// The whole screen as text, one line per row; tests assert against this.
    pub fn plain_text(&self) -> String {
        let mut lines = vec![self.title.clone()];
        lines.extend(self.lines.iter().map(|line| line.text.clone()));
        if let Some(log) = &self.log {
            lines.push(format!("log {}: {}", log.job_id, log.title));
            lines.extend(log.lines.iter().map(|line| line.text.clone()));
        }
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
            after: Vec::new(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            nodes: vec![
                node("ct-1", "busy", Some(3)),
                node("ct-2", "idle", Some(90)),
            ],
            jobs: vec![job("01ABC", "running"), job("01BLK", "blocked")],
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
        let queued = view
            .charts
            .iter()
            .find(|c| c.title == "queued jobs")
            .unwrap();
        assert_eq!(queued.latest, 1);
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

    #[test]
    fn the_selection_moves_in_each_table_and_tab_switches_between_them() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut snap = snapshot();
        snap.jobs.push(job("01DEF", "queued"));
        dashboard.apply(Ok(snap), t0);
        assert_eq!(dashboard.view(t0).selected_line, Some(1));
        dashboard.move_selection(true);
        assert_eq!(dashboard.view(t0).selected_line, Some(2));
        dashboard.move_selection(true);
        assert_eq!(dashboard.view(t0).selected_line, Some(2), "clamped");
        dashboard.toggle_focus();
        assert_eq!(dashboard.view(t0).selected_line, Some(5));
        dashboard.move_selection(true);
        assert_eq!(dashboard.view(t0).selected_line, Some(6));
        dashboard.move_selection(false);
        assert_eq!(dashboard.view(t0).selected_line, Some(5));
    }

    #[test]
    fn enter_opens_the_selected_jobs_log_and_keeps_the_last_twenty_lines() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        dashboard.toggle_focus();
        let id = dashboard.open_selected_log().unwrap();
        assert_eq!(id, "01ABC");
        assert!(dashboard.view(t0).plain_text().contains("loading log…"));
        let lines = (1..=25)
            .map(|n| format!("line {n}\u{1b}[0m"))
            .collect::<Vec<_>>()
            .join("\n");
        dashboard.apply_update(Update::Log {
            job_id: id,
            result: Ok(lines),
        });
        let view = dashboard.view(t0);
        let log = view.log.as_ref().unwrap();
        assert_eq!(log.lines.len(), 20);
        assert!(log.lines[0].text.contains("line 6"));
        assert!(view.plain_text().contains("line 25"));
        assert!(!view.plain_text().contains('\u{1b}'));
        assert!(dashboard.close_log());
        assert!(dashboard.view(t0).log.is_none());
    }

    #[test]
    fn a_late_log_for_another_job_is_ignored() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        dashboard.toggle_focus();
        dashboard.open_selected_log().unwrap();
        dashboard.apply_update(Update::Log {
            job_id: "01OTHER".to_owned(),
            result: Ok("secret".to_owned()),
        });
        let text = dashboard.view(t0).plain_text();
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("loading log…"), "{text}");
    }

    #[test]
    fn drain_needs_a_second_press_within_three_seconds() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        assert!(dashboard.request_drain(t0).is_none());
        assert!(dashboard.view(t0).status.text.contains("press d again"));
        assert!(dashboard.view(t0).status.text.contains("ct-1"));
        let request = dashboard
            .request_drain(t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(request.node, "ct-1");
        assert!(request.drain);
        dashboard.apply_update(Update::Drain {
            node: "ct-1".to_owned(),
            drain: true,
            result: Ok(()),
        });
        let view = dashboard.view(t0 + Duration::from_secs(2));
        assert!(view.status.text.contains("drained ct-1"));
        let row = view
            .lines
            .iter()
            .find(|line| line.text.starts_with("ct-1"))
            .unwrap();
        assert!(row.text.contains("drained"));
    }

    #[test]
    fn an_expired_drain_confirmation_must_be_asked_again() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        assert!(dashboard.request_drain(t0).is_none());
        assert!(
            dashboard
                .request_drain(t0 + Duration::from_secs(4))
                .is_none(),
            "the first prompt expired"
        );
        assert!(
            dashboard
                .request_drain(t0 + Duration::from_secs(5))
                .is_some(),
            "the second prompt was in time"
        );
    }

    #[test]
    fn once_printing_has_the_tables_but_no_chrome_or_charts() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(Ok(snapshot()), t0);
        let text = dashboard.view(t0).table_text();
        assert!(text.contains("NODE"));
        assert!(text.contains("PROVIDERS"));
        assert!(text.contains("JOB"));
        assert!(text.contains("01ABC"));
        assert!(!text.contains("lu top"));
        assert!(!text.contains("q quit"));
        assert!(!text.contains("running jobs"));
        assert!(!text.contains('\u{1b}'));
    }
}
