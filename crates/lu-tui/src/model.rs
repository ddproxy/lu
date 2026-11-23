//! State and view model for the dashboard; no terminal is touched, so it tests without a tty.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::graph::{self, Graph};
use crate::sanitize::printable;
use lu_proto::{HarnessState, JobState, ProviderState};

/// Reduces a source repository URL or absolute path to `owner/name`, dropping the scheme,
/// credentials, host, query, fragment and a trailing `.git`. Unusable values become `-`; none of
/// the discarded parts can reach the screen.
pub fn repo_label(repo: &str) -> String {
    let value = printable(repo).trim().to_owned();
    if value.is_empty() || value.len() > 2048 || value.contains(char::is_whitespace) {
        return "-".to_owned();
    }
    let value = value.split(['?', '#']).next().unwrap_or(&value);
    let path = match value.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme, "https" | "http" | "ssh" | "git") {
                return "-".to_owned();
            }
            let Some((_, path)) = rest.split_once('/') else {
                return "-".to_owned();
            };
            path
        }
        None => value,
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 {
        return "-".to_owned();
    }
    let owner = parts[parts.len() - 2];
    let name = parts[parts.len() - 1];
    if !valid_repo_part(owner) || !valid_repo_part(name) {
        return "-".to_owned();
    }
    format!("{owner}/{name}")
}

fn valid_repo_part(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

/// One provider on a node, already reduced to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCell {
    pub name: String,
    /// `ok`, `cooling`, `disabled` or `unknown`.
    pub state: String,
    /// Seconds until the soonest key is usable, when the node reports it.
    pub soonest_secs: Option<u64>,
}

/// A provider cordon as shown in the node table and the detail pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CordonCell {
    pub provider: String,
    /// `reiki` or `failure`.
    pub source: String,
    pub reason: String,
    /// None when the cordon lifts on healthy heartbeats instead of a deadline.
    pub remaining_secs: Option<u64>,
    pub lifts_after_healthy_heartbeats: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeRow {
    pub name: String,
    /// `idle`, `busy` or `-`.
    pub harness: String,
    pub drained: bool,
    /// Seconds since the node's last heartbeat, when it has sent one.
    pub seen_secs_ago: Option<u64>,
    pub providers: Vec<ProviderCell>,
    pub cordons: Vec<CordonCell>,
    pub version: Option<String>,
    pub load1: Option<f64>,
    pub mem_available_kb: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub stuck: u64,
    /// Bytes in workspaces currently on disk.
    pub workspace_bytes: u64,
    pub workspace_count: u64,
    pub workspace_reclaimed_bytes_total: u64,
    pub workspace_evictions_total: u64,
    pub workspace_delete_errors_total: u64,
    /// Free-form labels the node publishes, shown after its providers.
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: String,
    /// What the job is for, from its task's first line.
    pub title: String,
    pub state: String,
    pub node: Option<String>,
    /// Normalised `owner/name` of the job's source repository, never a URL.
    pub repo: String,
    /// Source ref as reported by the controller, shown with the repo in the detail line.
    pub git_ref: String,
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
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub nodes: Vec<NodeRow>,
    pub jobs: Vec<JobRow>,
    pub controller_version: Option<String>,
    /// Number of stored drafts.
    pub drafts: u64,
    /// Age of the oldest stored draft in seconds, zero when there are none.
    pub oldest_draft_secs: u64,
}

/// Which pane the selection keys act on. The Jobs table and the graph share one selected job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Nodes,
    Jobs,
    Graph,
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
    Log {
        job_id: String,
    },
    Drain {
        node: String,
        drain: bool,
    },
    /// Set or clear the process pane target; feeder refreshes stats every tick while set.
    ProcessPane {
        target: Option<ProcessPaneTarget>,
    },
}

/// The job whose node stats the process pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessPaneTarget {
    pub job_id: String,
    pub node: String,
}

/// One process in a job's tree, as shown in the process pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTreeRow {
    pub pid: u32,
    pub state: String,
    pub cpu_s: u64,
    pub rss_kb: u64,
    pub age_secs: u64,
    pub command: String,
}

/// Stats for the selected job, as shown in the process pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessJobView {
    pub output_idle_secs: u64,
    pub cpu_idle_secs: u64,
    pub uploader_behind: Option<String>,
    pub stuck: bool,
    pub processes: Vec<ProcessTreeRow>,
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
    ProcessStats {
        job_id: String,
        node: String,
        /// `Ok(None)` means the node sent no stats.
        result: Result<Option<ProcessJobView>, String>,
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
    /// Node busy: dark text on amber.
    NodeBusy,
    /// Node idle: dim or green foreground.
    NodeIdle,
    /// Node drained or draining: light text on blue.
    NodeDrained,
    /// Node stale, offline or stuck: light text on red.
    NodeOffline,
    /// Job running or leased: dark text on green.
    JobRunning,
    /// Job queued, recovering or waiting for input: amber foreground.
    JobQueued,
    /// Job failed: light text on red.
    JobFailed,
    /// Job done: dim green.
    JobDone,
    /// Job cancelled or expired: grey.
    JobCancelled,
}

/// One coloured span within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub tone: Tone,
}

impl Segment {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub tone: Tone,
    pub segments: Vec<Segment>,
}

impl Line {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        let text = text.into();
        Self {
            segments: vec![Segment::new(text.clone(), tone)],
            text,
            tone,
        }
    }

    fn rich(segments: Vec<Segment>) -> Self {
        let text = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        let tone = segments.first().map_or(Tone::Plain, |segment| segment.tone);
        Self {
            text,
            tone,
            segments,
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
        let count = |states: &[JobState]| {
            snapshot
                .jobs
                .iter()
                .filter(|job| {
                    job.state
                        .parse::<JobState>()
                        .is_ok_and(|state| states.contains(&state))
                })
                .count() as u64
        };
        Self {
            running: count(&[JobState::Running, JobState::Leased]),
            queued: count(&[JobState::Queued, JobState::Blocked, JobState::Recovering]),
            busy_nodes: snapshot
                .nodes
                .iter()
                .filter(|n| n.harness.parse::<HarnessState>() == Ok(HarnessState::Busy))
                .count() as u64,
            providers_ok: snapshot
                .nodes
                .iter()
                .flat_map(|n| &n.providers)
                .filter(|p| p.state.parse::<ProviderState>() == Ok(ProviderState::Ok))
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NodeHistory {
    load: Vec<u64>,
    mem_pct: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessPaneState {
    Loading,
    NoStats,
    Error(String),
    Stats(ProcessJobView),
}

#[derive(Debug, Clone)]
struct Frozen {
    snapshot: Snapshot,
    history: Vec<Sample>,
    node_history: HashMap<String, NodeHistory>,
    taken: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct Dashboard {
    live: Snapshot,
    updated: Option<Instant>,
    error: Option<String>,
    frozen: Option<Frozen>,
    history: Vec<Sample>,
    updates: u64,
    selection: Selection,
    log: Option<LogPane>,
    pending: Option<PendingDrain>,
    notice: Option<(String, Tone)>,
    graph_hidden: bool,
    graph_offset: usize,
    controller_version: Option<String>,
    node_history: HashMap<String, NodeHistory>,
    process_target: Option<ProcessPaneTarget>,
    process: Option<ProcessPaneState>,
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
                self.controller_version = snapshot.controller_version.clone();
                for node in &snapshot.nodes {
                    let entry = self.node_history.entry(node.name.clone()).or_default();
                    if let Some(load) = node.load1 {
                        entry.load.push((load.max(0.0) * 100.0).round() as u64);
                        if entry.load.len() > HISTORY {
                            entry.load.remove(0);
                        }
                    }
                    if let (Some(avail), Some(total)) = (node.mem_available_kb, node.mem_total_kb) {
                        if total > 0 {
                            let used = total.saturating_sub(avail);
                            let pct = (used.saturating_mul(100) / total).min(100);
                            entry.mem_pct.push(pct);
                            if entry.mem_pct.len() > HISTORY {
                                entry.mem_pct.remove(0);
                            }
                        }
                    }
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

    /// Applies a feeder result; stale answers for another target are ignored.
    pub fn apply_update(&mut self, update: Update) {
        match update {
            Update::Log { job_id, result } => self.apply_log(&job_id, result),
            Update::Drain {
                node,
                drain,
                result,
            } => self.apply_drain(&node, drain, result),
            Update::ProcessStats {
                job_id,
                node,
                result,
            } => self.apply_process_stats(&job_id, &node, result),
        }
    }

    /// Opens or closes the process pane. Returns the target to track when opened.
    pub fn toggle_process_pane(&mut self) -> Option<Option<ProcessPaneTarget>> {
        if self.process_target.is_some() {
            self.process_target = None;
            self.process = None;
            return Some(None);
        }
        if self.selection.focus != Focus::Jobs {
            return None;
        }
        let job = self.current().jobs.get(self.selection.job)?;
        if job.state.parse::<JobState>() != Ok(JobState::Running) {
            return None;
        }
        let node = job.node.as_ref()?.clone();
        let target = ProcessPaneTarget {
            job_id: job.id.clone(),
            node,
        };
        self.process_target = Some(target.clone());
        self.process = Some(ProcessPaneState::Loading);
        self.pending = None;
        Some(Some(target))
    }

    /// Closes the process pane; returns whether one was open.
    pub fn close_process_pane(&mut self) -> bool {
        if self.process_target.take().is_some() {
            self.process = None;
            true
        } else {
            false
        }
    }

    fn apply_process_stats(
        &mut self,
        job_id: &str,
        node: &str,
        result: Result<Option<ProcessJobView>, String>,
    ) {
        let Some(target) = &self.process_target else {
            return;
        };
        if target.job_id != job_id || target.node != node {
            return;
        }
        self.process = Some(match result {
            Ok(Some(stats)) => ProcessPaneState::Stats(stats),
            Ok(None) => ProcessPaneState::NoStats,
            Err(error) => ProcessPaneState::Error(printable(&error)),
        });
    }

    pub fn paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// Freezes a copy of the live data, or resumes when already paused.
    pub fn toggle_pause(&mut self, now: Instant) {
        self.frozen = match self.frozen.take() {
            Some(_) => None,
            None => Some(Frozen {
                snapshot: self.live.clone(),
                history: self.history.clone(),
                node_history: self.node_history.clone(),
                taken: now,
            }),
        };
    }

    fn current(&self) -> &Snapshot {
        match &self.frozen {
            Some(frozen) => &frozen.snapshot,
            None => &self.live,
        }
    }

    /// Moves the highlighted row. In the graph, Up/Down move between siblings and components.
    pub fn move_selection(&mut self, down: bool) {
        if self.selection.focus == Focus::Graph {
            self.move_graph(if down {
                graph::Step::Next
            } else {
                graph::Step::Previous
            });
            return;
        }
        self.pending = None;
        let len = match self.selection.focus {
            Focus::Nodes => self.current().nodes.len(),
            Focus::Jobs => self.current().jobs.len(),
            Focus::Graph => unreachable!("graph handled above"),
        };
        let index = match self.selection.focus {
            Focus::Nodes => &mut self.selection.node,
            Focus::Jobs => &mut self.selection.job,
            Focus::Graph => unreachable!("graph handled above"),
        };
        if len == 0 {
            *index = 0;
        } else if down {
            *index = (*index + 1).min(len - 1);
        } else {
            *index = index.saturating_sub(1);
        }
    }

    /// Moves the graph selection; keys that do not apply to the graph are ignored.
    pub fn move_graph(&mut self, step: graph::Step) {
        if self.selection.focus != Focus::Graph {
            return;
        }
        self.pending = None;
        let Some(graph) = self.visible_graph() else {
            return;
        };
        let Some(id) = self
            .current()
            .jobs
            .get(self.selection.job)
            .map(|job| job.id.clone())
        else {
            return;
        };
        let Some(next) = graph.step(&id, step) else {
            return;
        };
        if let Some(index) = self.current().jobs.iter().position(|job| job.id == next) {
            self.selection.job = index;
            self.graph_offset = 0;
        }
    }

    fn visible_graph(&self) -> Option<Graph> {
        if self.graph_hidden {
            return None;
        }
        let jobs: Vec<graph::Job> = self
            .current()
            .jobs
            .iter()
            .map(|job| graph::Job {
                id: job.id.clone(),
                state: job.state.parse().unwrap_or(graph::State::Queued),
                title: job.title.clone(),
                after: job.after.clone(),
            })
            .collect();
        let graph = Graph::build(&jobs);
        (!graph.is_empty()).then_some(graph)
    }

    /// Hides or shows the dependency graph under the job list.
    pub fn toggle_graph(&mut self) {
        self.graph_hidden = !self.graph_hidden;
    }

    /// Tab moves Nodes → Jobs → Graph; Shift+Tab moves the other way.
    pub fn toggle_focus(&mut self) {
        self.cycle_focus(false);
    }

    pub fn toggle_focus_back(&mut self) {
        self.cycle_focus(true);
    }

    fn cycle_focus(&mut self, back: bool) {
        self.pending = None;
        self.selection.focus = match (back, self.selection.focus) {
            (false, Focus::Nodes) | (true, Focus::Graph) => Focus::Jobs,
            (false, Focus::Jobs) | (true, Focus::Nodes) => Focus::Graph,
            (false, Focus::Graph) | (true, Focus::Jobs) => Focus::Nodes,
        };
    }

    /// Opens the selected job's log pane and returns its id for the feeder to fetch.
    pub fn open_selected_log(&mut self) -> Option<String> {
        if !matches!(self.selection.focus, Focus::Jobs | Focus::Graph) {
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

    /// Selects a node row clicked with the mouse.
    pub fn select_node_at(&mut self, index: usize) -> bool {
        if index >= self.current().nodes.len() {
            return false;
        }
        self.selection.node = index;
        self.selection.focus = Focus::Nodes;
        self.pending = None;
        true
    }

    /// Selects a job row or graph node. Both panes share `selection.job`.
    pub fn select_job_at(&mut self, index: usize, focus: Focus) -> bool {
        if index >= self.current().jobs.len() || focus == Focus::Nodes {
            return false;
        }
        self.selection.job = index;
        self.selection.focus = focus;
        self.pending = None;
        self.graph_offset = 0;
        true
    }

    pub fn select_job_id(&mut self, id: &str, focus: Focus) -> bool {
        let Some(index) = self.current().jobs.iter().position(|job| job.id == id) else {
            return false;
        };
        self.select_job_at(index, focus)
    }

    /// Selects a job and opens its log, as a double click does.
    pub fn open_job_id(&mut self, id: &str, focus: Focus) -> Option<String> {
        if !self.select_job_id(id, focus) {
            return None;
        }
        self.open_selected_log()
    }

    /// Wheel over the graph moves its window by three rows.
    pub fn scroll_graph(&mut self, down: bool) {
        if self.selection.focus != Focus::Graph {
            return;
        }
        let rows = self
            .visible_graph()
            .map(|graph| {
                graph::render(&graph, 100, false)
                    .lines
                    .len()
                    .saturating_sub(1)
            })
            .unwrap_or(0);
        self.graph_offset = if down {
            self.graph_offset.saturating_add(3).min(rows)
        } else {
            self.graph_offset.saturating_sub(3)
        };
    }

    /// Wheel over a table moves its selection by three rows.
    pub fn scroll_table(&mut self, down: bool) {
        if self.selection.focus == Focus::Graph {
            return;
        }
        self.pending = None;
        let (len, index) = match self.selection.focus {
            Focus::Nodes => (self.current().nodes.len(), &mut self.selection.node),
            Focus::Jobs => (self.current().jobs.len(), &mut self.selection.job),
            Focus::Graph => unreachable!("graph handled above"),
        };
        if len == 0 {
            *index = 0;
        } else if down {
            *index = (*index + 3).min(len - 1);
        } else {
            *index = index.saturating_sub(3);
        }
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
                if let Some(frozen) = &mut self.frozen {
                    for row in &mut frozen.snapshot.nodes {
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

    /// A wide view, used by `--once` and callers that have no terminal width.
    pub fn view(&self, now: Instant) -> ViewModel {
        self.view_with_width(now, 200)
    }

    /// The view for a terminal of `width` cells. Tables size their flexible columns to fit.
    pub fn view_with_width(&self, now: Instant, width: u16) -> ViewModel {
        let (snapshot, history, node_history, live_status) = match &self.frozen {
            Some(frozen) => (
                &frozen.snapshot,
                frozen.history.as_slice(),
                &frozen.node_history,
                Status::Paused(now.saturating_duration_since(frozen.taken)),
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
                (
                    &self.live,
                    self.history.as_slice(),
                    &self.node_history,
                    status,
                )
            }
        };
        let status = self.notice_status(now).unwrap_or(live_status);
        let controller_version = snapshot
            .controller_version
            .as_deref()
            .or(self.controller_version.as_deref());
        ViewModel::build(ViewBuild {
            snapshot,
            width: usize::from(width),
            history,
            node_history,
            controller_version,
            status,
            selection: self.selection,
            graph_offset: self.graph_offset,
            log: self.log.as_ref(),
            show_graph: !self.graph_hidden,
            process_target: self.process_target.as_ref(),
            process: self.process.as_ref(),
            cordon_pane: if self.selection.focus == Focus::Nodes {
                snapshot
                    .nodes
                    .get(self.selection.node)
                    .filter(|node| !node.cordons.is_empty())
                    .map(cordon_pane_view)
            } else {
                None
            },
        })
    }
}

enum Status {
    Waiting,
    Live { age: Duration, updates: u64 },
    Failing(String),
    Paused(Duration),
    Notice { text: String, tone: Tone },
}

/// The process navigator pane shown under the lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessPaneView {
    pub title: String,
    pub lines: Vec<Line>,
    pub load_spark: Vec<u64>,
    pub mem_spark: Vec<u64>,
}

/// What the screen shows; the renderer only lays the lines out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModel {
    pub title: String,
    pub charts: Vec<Chart>,
    pub lines: Vec<Line>,
    pub status: Line,
    pub footer: String,
    /// Fleet totals drawn under a short job list so the empty space carries information.
    pub summary: String,
    pub paused: bool,
    /// Index into `lines` of the highlighted row.
    pub selected_line: Option<usize>,
    pub focus: Focus,
    /// How many rows the node table occupies, for mouse hit testing.
    pub node_rows: usize,
    pub node_names: Vec<String>,
    pub job_ids: Vec<String>,
    /// The job selected in both the Jobs table and the graph.
    pub selected_job: Option<String>,
    /// Full id, state and title shown on the graph block's bottom border.
    pub graph_detail: Option<Line>,
    pub graph_offset: usize,
    pub log: Option<LogPane>,
    /// The dependency graph under the job list, when it has a component to show.
    pub graph: Option<Graph>,
    pub process_pane: Option<ProcessPaneView>,
    /// Cordons for the selected node, shown when no log or process pane is open.
    pub cordon_pane: Option<ProcessPaneView>,
}

struct ViewBuild<'a> {
    snapshot: &'a Snapshot,
    width: usize,
    history: &'a [Sample],
    node_history: &'a HashMap<String, NodeHistory>,
    controller_version: Option<&'a str>,
    status: Status,
    selection: Selection,
    graph_offset: usize,
    log: Option<&'a LogPane>,
    show_graph: bool,
    process_target: Option<&'a ProcessPaneTarget>,
    process: Option<&'a ProcessPaneState>,
    cordon_pane: Option<ProcessPaneView>,
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
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Width budget for the three flexible job columns. The mandatory columns (id, state, node,
/// repo, try and age) keep their minimums; NOTE gives way first, then TITLE, then REPO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JobColumns {
    repo: usize,
    title: usize,
    note: usize,
}

const JOB_FIXED: usize = 10 + 11 + 9 + 3 + 6 + 7; // id, state, node, try, age and six gaps
const REPO_MAX: usize = 14;
const REPO_MIN: usize = 6;
const TITLE_MAX: usize = 38;
const TITLE_MIN: usize = 8;
const NOTE_MAX: usize = 60;
const NOTE_MIN: usize = 6;

fn take_from(width: &mut usize, floor: usize, needed: usize) -> usize {
    let taken = needed.min(width.saturating_sub(floor));
    *width -= taken;
    taken
}

fn job_columns(width: usize) -> JobColumns {
    let budget = width.saturating_sub(JOB_FIXED);
    let mut columns = JobColumns {
        repo: REPO_MAX,
        title: TITLE_MAX,
        note: NOTE_MAX,
    };
    let mut over = (columns.repo + columns.title + columns.note).saturating_sub(budget);
    over -= take_from(&mut columns.note, NOTE_MIN, over);
    over -= take_from(&mut columns.title, TITLE_MIN, over);
    over -= take_from(&mut columns.repo, REPO_MIN, over);
    over -= take_from(&mut columns.note, 0, over);
    let _ = take_from(&mut columns.title, 0, over);
    columns
}

fn provider_text(cell: &ProviderCell, cordon: Option<&CordonCell>) -> (String, Tone) {
    let name = printable(&cell.name);
    let (text, tone) = match (cell.state.parse::<ProviderState>(), cell.soonest_secs) {
        (Ok(ProviderState::Ok), _) => (format!("{name} ok"), Tone::Good),
        (Ok(ProviderState::Cooling), Some(secs)) => {
            (format!("{name} cooling {}", short_age(secs)), Tone::Warn)
        }
        (Ok(ProviderState::Cooling), None) => (format!("{name} cooling"), Tone::Warn),
        (Ok(ProviderState::Disabled), _) => (format!("{name} disabled"), Tone::Bad),
        (Ok(state), _) => (format!("{name} {}", state.as_str()), Tone::Dim),
        (Err(_), _) => (format!("{name} {}", printable(&cell.state)), Tone::Dim),
    };
    match cordon {
        Some(c) if c.source == "reiki" => (format!("!{name}~"), Tone::Blocked),
        Some(_) => (format!("!{name}"), Tone::Blocked),
        None => (text, tone),
    }
}

/// A suffix for the title naming providers that queued jobs need but no usable node can offer:
/// ` · waiting for zen 14h54m (3 queued)`. Empty when nothing is queued or every provider has at
/// least one node that is ready.
fn waiting_on_providers(snapshot: &Snapshot) -> String {
    let queued = snapshot
        .jobs
        .iter()
        .filter(|job| job.state.parse::<JobState>() == Ok(JobState::Queued))
        .count();
    if queued == 0 {
        return String::new();
    }
    let mut names: Vec<&str> = Vec::new();
    for node in snapshot.nodes.iter().filter(|n| !n.drained) {
        for cell in &node.providers {
            if !names.contains(&cell.name.as_str()) {
                names.push(&cell.name);
            }
        }
    }
    let mut waiting = Vec::new();
    for name in names {
        let cells: Vec<&ProviderCell> = snapshot
            .nodes
            .iter()
            .filter(|n| !n.drained)
            .flat_map(|n| &n.providers)
            .filter(|c| c.name == name)
            .collect();
        let any_ok = cells
            .iter()
            .any(|c| c.state.parse::<ProviderState>() == Ok(ProviderState::Ok));
        if any_ok {
            continue;
        }
        let soonest = cells.iter().filter_map(|c| c.soonest_secs).min();
        waiting.push(match soonest {
            Some(secs) => format!("{} {}", printable(name), short_age(secs)),
            None => printable(name),
        });
    }
    if waiting.is_empty() {
        String::new()
    } else {
        format!(" · waiting for {} ({queued} queued)", waiting.join(", "))
    }
}

fn cordon_pane_view(node: &NodeRow) -> ProcessPaneView {
    let mut lines = Vec::new();
    for cordon in &node.cordons {
        let (mark, tone) = if cordon.source == "reiki" {
            ("~", Tone::Warn)
        } else {
            ("!", Tone::Blocked)
        };
        let state = match (cordon.remaining_secs, cordon.lifts_after_healthy_heartbeats) {
            (Some(secs), _) => format!("{} left", short_age(secs)),
            (None, Some(count)) => format!("lifts on {count} healthy"),
            (None, None) => "lifts on healthy".to_owned(),
        };
        lines.push(Line::rich(vec![
            Segment::new(format!("{mark} {} ", printable(&cordon.provider)), tone),
            Segment::new(state, tone),
        ]));
        lines.push(Line::new(
            format!("    {}", printable(&cordon.reason)),
            Tone::Dim,
        ));
    }
    ProcessPaneView {
        title: format!("cordons {}", printable(&node.name)),
        lines,
        load_spark: Vec::new(),
        mem_spark: Vec::new(),
    }
}

fn job_tone(state: &str) -> Tone {
    match state.parse::<JobState>() {
        Ok(JobState::Done) => Tone::Done,
        Ok(JobState::Running | JobState::Leased) => Tone::Good,
        Ok(JobState::Blocked | JobState::Draft) => Tone::Blocked,
        Ok(JobState::Queued | JobState::Recovering | JobState::NeedsInput) => Tone::Warn,
        Ok(JobState::Failed) => Tone::Bad,
        Ok(JobState::Cancelled | JobState::Expired) | Err(_) => Tone::Dim,
    }
}

/// The badge tone for the STATE cell; unlike `job_tone` it can carry a background.
fn job_badge_tone(state: &str) -> Tone {
    match state.parse::<JobState>() {
        Ok(JobState::Running | JobState::Leased) => Tone::JobRunning,
        Ok(JobState::Failed) => Tone::JobFailed,
        Ok(JobState::Done) => Tone::JobDone,
        Ok(JobState::Cancelled | JobState::Expired) | Err(_) => Tone::JobCancelled,
        Ok(JobState::Blocked | JobState::Draft) => Tone::Blocked,
        Ok(JobState::Queued | JobState::Recovering | JobState::NeedsInput) => Tone::JobQueued,
    }
}

/// The badge tone for a node's STATE cell. Staleness and stuck jobs turn it red, a drain turns it
/// blue, and an unknown harness stays plain rather than borrowing a badge.
fn node_badge_tone(node: &NodeRow) -> Tone {
    let stale = node.seen_secs_ago.is_none_or(|age| age > STALE_SECS);
    let harness = node.harness.as_str();
    if node.stuck > 0 || stale || harness == "offline" {
        Tone::NodeOffline
    } else if node.drained || harness == "draining" {
        Tone::NodeDrained
    } else if harness == "busy" {
        Tone::NodeBusy
    } else if harness == "idle" {
        Tone::NodeIdle
    } else {
        Tone::Plain
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

fn compact_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", bytes, UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

fn mem_used_pct(node: &NodeRow) -> Option<u64> {
    match (node.mem_available_kb, node.mem_total_kb) {
        (Some(avail), Some(total)) if total > 0 => {
            Some(total.saturating_sub(avail).saturating_mul(100) / total)
        }
        _ => None,
    }
}

fn mem_tone(pct: u64) -> Tone {
    if pct >= 95 {
        Tone::Bad
    } else if pct >= 80 {
        Tone::Warn
    } else {
        Tone::Good
    }
}

fn node_state_base(node: &NodeRow) -> (String, Tone) {
    let stale = node.seen_secs_ago.is_none_or(|age| age > STALE_SECS);
    if stale {
        ("stale".to_owned(), Tone::Bad)
    } else if node.drained {
        ("drained".to_owned(), Tone::Warn)
    } else {
        (printable(&node.harness), Tone::Plain)
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

fn pad_left(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        text.to_owned()
    } else {
        format!("{}{}", " ".repeat(width - count), text)
    }
}

fn node_line(node: &NodeRow, controller_version: Option<&str>) -> Line {
    let (base_state, base_tone) = node_state_base(node);
    let seen = node.seen_secs_ago.map_or("-".to_owned(), short_age);
    let version = node
        .version
        .as_deref()
        .map(printable)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "-".to_owned());
    let version_tone = match controller_version {
        Some(cv) => {
            if node.version.as_deref() != Some(cv) {
                Tone::Warn
            } else {
                Tone::Plain
            }
        }
        None => Tone::Plain,
    };
    let load_text = node
        .load1
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "-".to_owned());
    let mem_pct = mem_used_pct(node);
    let mem_text = mem_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "-".to_owned());
    let mem_text_tone = mem_pct.map(mem_tone).unwrap_or(Tone::Plain);
    let workspace_text = format!(
        "{}/{}",
        node.workspace_count,
        compact_bytes(node.workspace_bytes)
    );
    let workspace_total_text = format!(
        "{}/{}/{}",
        compact_bytes(node.workspace_reclaimed_bytes_total),
        node.workspace_evictions_total,
        node.workspace_delete_errors_total
    );

    let state_plain = if node.stuck > 0 {
        format!("{base_state} stuck")
    } else {
        base_state
    };
    // The whole padded cell carries the badge, so its background is one solid block.
    let state_badge = Segment::new(
        pad_right(&clip(&state_plain, 14), 14),
        node_badge_tone(node),
    );

    // Problems come first so a narrow terminal truncates the healthy providers, not the cooling
    // one an operator needs to see.
    let mut provider_cells: Vec<(String, Tone)> = node
        .providers
        .iter()
        .map(|cell| {
            let cordon = node.cordons.iter().find(|c| c.provider == cell.name);
            provider_text(cell, cordon)
        })
        .collect();
    provider_cells.sort_by_key(|(_, tone)| *tone == Tone::Good);
    let providers_plain = provider_cells
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("   ");

    let mut segments = vec![
        Segment::new(pad_right(&clip(&node.name, 12), 12), Tone::Plain),
        Segment::new(" ", Tone::Plain),
        Segment::new(pad_right(&clip(&version, 10), 10), version_tone),
        Segment::new(" ", Tone::Plain),
    ];
    // state column (14)
    segments.push(state_badge);
    segments.push(Segment::new(" ", Tone::Plain));
    segments.push(Segment::new(pad_right(&seen, 8), Tone::Plain));
    segments.push(Segment::new(" ", Tone::Plain));
    segments.push(Segment::new(pad_left(&load_text, 6), Tone::Plain));
    segments.push(Segment::new(" ", Tone::Plain));
    segments.push(Segment::new(pad_left(&mem_text, 5), mem_text_tone));
    segments.push(Segment::new(" ", Tone::Plain));
    segments.push(Segment::new(pad_left(&workspace_text, 12), Tone::Plain));
    segments.push(Segment::new(" ", Tone::Plain));
    segments.push(Segment::new(
        pad_left(&workspace_total_text, 16),
        Tone::Plain,
    ));
    segments.push(Segment::new("  ", Tone::Plain));
    if provider_cells.is_empty() {
        // nothing
    } else {
        for (i, (text, tone)) in provider_cells.iter().enumerate() {
            if i > 0 {
                segments.push(Segment::new("   ", Tone::Plain));
            }
            segments.push(Segment::new(text.clone(), *tone));
        }
    }
    if !node.labels.is_empty() {
        let joined = node
            .labels
            .iter()
            .map(|label| printable(label))
            .collect::<Vec<_>>()
            .join(",");
        segments.push(Segment::new("   ", Tone::Plain));
        segments.push(Segment::new(format!("[{}]", clip(&joined, 40)), Tone::Dim));
    }

    // row fallback tone: stale/drained bad/warn dominates; include mem/stuck/version/provider issues
    let mut row_tone = base_tone;
    if node.stuck > 0 {
        row_tone = Tone::Bad;
    }
    if let Some(pct) = mem_pct {
        row_tone = worst([row_tone, mem_tone(pct)].into_iter());
    }
    if version_tone == Tone::Warn {
        row_tone = worst([row_tone, Tone::Warn].into_iter());
    }
    if base_tone == Tone::Plain && !provider_cells.is_empty() {
        row_tone =
            worst(std::iter::once(row_tone).chain(provider_cells.iter().map(|(_, tone)| *tone)));
    }
    let mut line = Line::rich(segments);
    // ensure plain text matches expected formatting for providers spacing
    if !providers_plain.is_empty() {
        let expected_prefix = format!(
            "{:<12} {:<10} {:<14} {:<8} {:>6} {:>5} {:>12} {:>16}  ",
            clip(&node.name, 12),
            clip(&version, 10),
            clip(&state_plain, 14),
            pad_right(&seen, 8).trim_end(),
            load_text,
            mem_text,
            workspace_text,
            workspace_total_text
        );
        // rebuild plain text cleanly
        line.text = format!(
            "{:<12} {:<10} {:<14} {:<8} {:>6} {:>5} {:>12} {:>16}  {}",
            clip(&node.name, 12),
            clip(&version, 10),
            clip(&state_plain, 14),
            seen,
            load_text,
            mem_text,
            workspace_text,
            workspace_total_text,
            providers_plain
        );
        let _ = expected_prefix;
    } else {
        line.text = format!(
            "{:<12} {:<10} {:<14} {:<8} {:>6} {:>5} {:>12} {:>16}",
            clip(&node.name, 12),
            clip(&version, 10),
            clip(&state_plain, 14),
            seen,
            load_text,
            mem_text,
            workspace_text,
            workspace_total_text
        );
    }
    if !node.labels.is_empty() {
        let joined = node
            .labels
            .iter()
            .map(|label| printable(label))
            .collect::<Vec<_>>()
            .join(",");
        line.text.push_str(&format!("   [{}]", clip(&joined, 40)));
    }
    line.tone = row_tone;
    line
}

fn process_pane_view(
    target: &ProcessPaneTarget,
    process: Option<&ProcessPaneState>,
    node_history: &HashMap<String, NodeHistory>,
) -> ProcessPaneView {
    let title = format!(
        "process {} @ {}",
        printable(&target.job_id),
        printable(&target.node)
    );
    let history = node_history.get(&target.node).cloned().unwrap_or_default();
    let mut lines = Vec::new();
    match process {
        Some(ProcessPaneState::Stats(stats)) => {
            let counter_tone = if stats.stuck {
                Tone::Bad
            } else if stats.output_idle_secs > 120 && stats.cpu_idle_secs > 120 {
                Tone::Warn
            } else {
                Tone::Plain
            };
            let uploader = stats
                .uploader_behind
                .as_deref()
                .map(printable)
                .unwrap_or_else(|| "-".to_owned());
            lines.push(Line::new(
                format!(
                    "output idle {}s   cpu idle {}s   uploader behind {}",
                    stats.output_idle_secs, stats.cpu_idle_secs, uploader
                ),
                counter_tone,
            ));
            lines.push(Line::new(
                format!(
                    "{:>7} {:>5} {:>6} {:>7} {:>6}  COMMAND",
                    "PID", "STATE", "CPU_S", "RSS", "AGE"
                ),
                Tone::Heading,
            ));
            if stats.processes.is_empty() {
                lines.push(Line::new("no processes", Tone::Dim));
            }
            for (i, proc) in stats.processes.iter().enumerate() {
                let indent = "  ".repeat(i.min(6));
                let state = printable(&proc.state);
                let state_tone = if state == "D" || state == "Z" {
                    Tone::Bad
                } else {
                    Tone::Plain
                };
                let mut segments = vec![
                    Segment::new(format!("{:>7} ", proc.pid), Tone::Plain),
                    Segment::new(format!("{:>5}", state), state_tone),
                    Segment::new(
                        format!(
                            " {:>6} {:>7} {:>6}  {}{}",
                            proc.cpu_s,
                            proc.rss_kb,
                            short_age(proc.age_secs),
                            indent,
                            printable(&proc.command)
                        ),
                        Tone::Plain,
                    ),
                ];
                let mut line = Line::rich(std::mem::take(&mut segments));
                line.text = format!(
                    "{:>7} {:>5} {:>6} {:>7} {:>6}  {}{}",
                    proc.pid,
                    state,
                    proc.cpu_s,
                    proc.rss_kb,
                    short_age(proc.age_secs),
                    indent,
                    printable(&proc.command)
                );
                line.tone = if state_tone == Tone::Bad {
                    Tone::Bad
                } else {
                    Tone::Plain
                };
                lines.push(line);
            }
        }
        Some(ProcessPaneState::NoStats) => {
            lines.push(Line::new("no stats reported by node", Tone::Dim));
        }
        Some(ProcessPaneState::Error(error)) => {
            lines.push(Line::new(
                format!("stats unavailable: {}", clip(error, 120)),
                Tone::Bad,
            ));
        }
        Some(ProcessPaneState::Loading) | None => {
            lines.push(Line::new("loading stats…", Tone::Dim));
        }
    }
    ProcessPaneView {
        title,
        lines,
        load_spark: history.load,
        mem_spark: history.mem_pct,
    }
}

fn job_header(columns: JobColumns) -> Line {
    Line::new(
        format!(
            "{:<10} {:<11} {:<9} {:<repo$} {:>3} {:<6} {:<title$} {}",
            "JOB",
            "STATE",
            "NODE",
            "REPO",
            "TRY",
            "AGE",
            "TITLE",
            "NOTE",
            repo = columns.repo,
            title = columns.title,
        ),
        Tone::Heading,
    )
}

fn job_line(job: &JobRow, columns: JobColumns) -> Line {
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
    let state = clip(&job.state, 11);
    let mut segments = vec![
        Segment::new(pad_right(&short_id, 10), Tone::Plain),
        Segment::new(" ", Tone::Plain),
        Segment::new(pad_right(&state, 11), job_badge_tone(&job.state)),
        Segment::new(" ", Tone::Plain),
        Segment::new(pad_right(&node, 9), Tone::Plain),
        Segment::new(" ", Tone::Plain),
        Segment::new(
            pad_right(&clip(&repo_label(&job.repo), columns.repo), columns.repo),
            Tone::Dim,
        ),
        Segment::new(" ", Tone::Plain),
        Segment::new(format!("{:>3}", job.attempt), Tone::Plain),
        Segment::new(" ", Tone::Plain),
        Segment::new(pad_right(&short_age(job.age_secs), 6), Tone::Plain),
    ];
    if columns.title > 0 {
        segments.push(Segment::new(" ", Tone::Plain));
        segments.push(Segment::new(
            pad_right(&clip(&job.title, columns.title), columns.title),
            Tone::Plain,
        ));
    }
    if columns.note > 0 {
        segments.push(Segment::new(" ", Tone::Plain));
        segments.push(Segment::new(clip(note, columns.note), Tone::Plain));
    }
    let mut line = Line::rich(segments);
    line.tone = job_tone(&job.state);
    line
}

/// One line of fleet totals, drawn in the empty space under a short job list.
fn fleet_summary(snapshot: &Snapshot) -> String {
    let count = |state: JobState| {
        snapshot
            .jobs
            .iter()
            .filter(|job| job.state.parse::<JobState>() == Ok(state))
            .count()
    };
    let online = snapshot
        .nodes
        .iter()
        .filter(|node| node.seen_secs_ago.is_none_or(|age| age <= STALE_SECS))
        .count();
    let busy = snapshot
        .nodes
        .iter()
        .filter(|node| node.harness.parse::<HarnessState>() == Ok(HarnessState::Busy))
        .count();
    format!(
        "jobs {} · run {} · que {} · blk {} · done {} · fail {} · canc {} · busy {}/{}",
        snapshot.jobs.len(),
        count(JobState::Running) + count(JobState::Leased),
        count(JobState::Queued) + count(JobState::Recovering) + count(JobState::NeedsInput),
        count(JobState::Blocked) + count(JobState::Draft),
        count(JobState::Done),
        count(JobState::Failed),
        count(JobState::Cancelled) + count(JobState::Expired),
        busy,
        online
    )
}

impl ViewModel {
    fn build(args: ViewBuild<'_>) -> Self {
        let ViewBuild {
            snapshot,
            width,
            history,
            node_history,
            controller_version,
            status,
            selection,
            graph_offset,
            log,
            show_graph,
            process_target,
            process,
            cordon_pane,
        } = args;
        let mut lines = vec![Line::new(
            format!(
                "{:<12} {:<10} {:<14} {:<8} {:>6} {:>5} {:>12} {:>16}  PROVIDERS",
                "NODE", "VERSION", "STATE", "SEEN", "LOAD", "MEM", "WORKSPACES", "WS TOTALS"
            ),
            Tone::Heading,
        )];
        if snapshot.nodes.is_empty() {
            lines.push(Line::new("no nodes have reported", Tone::Dim));
        }
        for node in &snapshot.nodes {
            lines.push(node_line(node, controller_version));
        }
        lines.push(Line::new("", Tone::Plain));
        // The border costs two cells on each side; leave one spare for narrow panes.
        let columns = job_columns(width.saturating_sub(4));
        lines.push(job_header(columns));
        if snapshot.jobs.is_empty() {
            lines.push(Line::new("no jobs", Tone::Dim));
        }
        for job in &snapshot.jobs {
            lines.push(job_line(job, columns));
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
                    state: job.state.parse().unwrap_or(graph::State::Queued),
                    title: job.title.clone(),
                    after: job.after.clone(),
                })
                .collect();
            graph::Graph::build(&jobs)
        });
        let controller = controller_version
            .map(printable)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "-".to_owned());
        let drafts = if snapshot.drafts == 0 {
            "drafts 0".to_owned()
        } else {
            format!(
                "drafts {} oldest {}",
                snapshot.drafts,
                short_age(snapshot.oldest_draft_secs)
            )
        };
        let waiting = waiting_on_providers(snapshot);
        let title = format!(
            "lu top {} · controller {} · {drafts}{waiting}",
            env!("CARGO_PKG_VERSION"),
            controller
        );
        let process_pane =
            process_target.map(|target| process_pane_view(target, process, node_history));
        let selected_job = snapshot.jobs.get(selection.job);
        let graph_detail = selected_job.map(|job| {
            Line::new(
                format!(
                    "{}  {}  {}  {}  {}",
                    printable(&job.id),
                    printable(&job.state),
                    clip(&repo_label(&job.repo), 24),
                    clip(&printable(&job.git_ref), 24),
                    printable(&job.title)
                ),
                job_tone(&job.state),
            )
        });
        let footer = match selection.focus {
            Focus::Nodes => {
                "↑↓/jk node  Tab/⇧Tab focus  d drain  g graph  p pause  q quit"
            }
            Focus::Jobs => {
                "↑↓/jk job  Tab/⇧Tab focus  Enter log  x proc  Esc close  g graph  p pause  q quit"
            }
            Focus::Graph => {
                "h/l parent-child  j/k sibling  Home/End ends  Enter log  click select  wheel scroll  Tab/⇧Tab focus  q quit"
            }
        }
        .to_owned();
        Self {
            title,
            charts: charts(history),
            lines,
            status,
            footer,
            summary: fleet_summary(snapshot),
            paused,
            selected_line,
            focus: selection.focus,
            node_rows,
            node_names: snapshot
                .nodes
                .iter()
                .map(|node| node.name.clone())
                .collect(),
            job_ids: snapshot.jobs.iter().map(|job| job.id.clone()).collect(),
            selected_job: selected_job.map(|job| job.id.clone()),
            graph_detail,
            graph_offset,
            log: log.cloned(),
            graph: graph.filter(|graph| !graph.is_empty()),
            process_pane,
            cordon_pane,
        }
    }

    /// The two tables, without the title, status, footer or charts; `--once` prints this.
    pub fn table_text(&self) -> String {
        let mut lines: Vec<String> = self.lines.iter().map(|line| line.text.clone()).collect();
        lines.push(String::new());
        lines.push(self.summary.clone());
        if let Some(graph) = &self.graph {
            let graph_lines: Vec<String> = graph::render(graph, 100, false)
                .iter()
                .map(graph::GraphLine::text)
                .collect();
            if !graph_lines.is_empty() {
                lines.push(String::new());
                lines.extend(graph_lines);
            }
        }
        if let Some(pane) = &self.process_pane {
            lines.push(String::new());
            lines.push(pane.title.clone());
            lines.extend(pane.lines.iter().map(|line| line.text.clone()));
        }
        lines.join("\n")
    }

    /// The whole screen as text, one line per row; tests assert against this.
    pub fn plain_text(&self) -> String {
        let mut lines = vec![self.title.clone()];
        lines.extend(self.lines.iter().map(|line| line.text.clone()));
        lines.push(self.summary.clone());
        if let Some(log) = &self.log {
            lines.push(format!("log {}: {}", log.job_id, log.title));
            lines.extend(log.lines.iter().map(|line| line.text.clone()));
        }
        if let Some(pane) = &self.process_pane {
            lines.push(pane.title.clone());
            lines.extend(pane.lines.iter().map(|line| line.text.clone()));
        }
        if let Some(pane) = &self.cordon_pane {
            lines.push(pane.title.clone());
            lines.extend(pane.lines.iter().map(|line| line.text.clone()));
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
            cordons: Vec::new(),
            version: None,
            load1: None,
            mem_available_kb: None,
            mem_total_kb: None,
            stuck: 0,
            workspace_bytes: 0,
            workspace_count: 0,
            workspace_reclaimed_bytes_total: 0,
            workspace_evictions_total: 0,
            workspace_delete_errors_total: 0,
            labels: Vec::new(),
        }
    }

    fn job(id: &str, state: &str) -> JobRow {
        JobRow {
            id: id.to_owned(),
            title: "do the thing".to_owned(),
            state: state.to_owned(),
            node: Some("ct-1".to_owned()),
            repo: "owner/name".to_owned(),
            git_ref: "main".to_owned(),
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
            controller_version: None,
            drafts: 0,
            oldest_draft_secs: 0,
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
    fn cordons_mark_provider_chips_and_open_a_detail_pane() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut snap = snapshot();
        snap.nodes[0].cordons = vec![
            CordonCell {
                provider: "zen".to_owned(),
                source: "failure".to_owned(),
                reason: "provider timeout".to_owned(),
                remaining_secs: Some(120),
                lifts_after_healthy_heartbeats: None,
            },
            CordonCell {
                provider: "openrouter".to_owned(),
                source: "reiki".to_owned(),
                reason: "Reiki cooldown".to_owned(),
                remaining_secs: None,
                lifts_after_healthy_heartbeats: Some(2),
            },
        ];
        dashboard.apply(Ok(snap), t0);
        let view = dashboard.view(t0);
        let text = view.plain_text();
        assert!(text.contains("!zen"), "{text}");
        assert!(text.contains("!openrouter~"), "{text}");
        assert!(text.contains("cordons ct-1"), "{text}");
        assert!(text.contains("! zen 2m left"), "{text}");
        assert!(text.contains("~ openrouter lifts on 2 healthy"), "{text}");
        assert!(text.contains("provider timeout"), "{text}");

        let node_line = view
            .lines
            .iter()
            .find(|line| line.text.starts_with("ct-1"))
            .unwrap();
        let chip = node_line
            .segments
            .iter()
            .find(|segment| segment.text == "!zen")
            .unwrap();
        assert_eq!(chip.tone, Tone::Blocked);
        let reiki_chip = node_line
            .segments
            .iter()
            .find(|segment| segment.text == "!openrouter~")
            .unwrap();
        assert_eq!(reiki_chip.tone, Tone::Blocked);

        let pane = view.cordon_pane.as_ref().unwrap();
        let failure = pane
            .lines
            .iter()
            .find(|line| line.text.contains("zen"))
            .unwrap();
        assert_eq!(failure.tone, Tone::Blocked);
        let reiki = pane
            .lines
            .iter()
            .find(|line| line.text.contains("openrouter"))
            .unwrap();
        assert_eq!(reiki.tone, Tone::Warn);
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
        let text = dashboard.view(t0).plain_text();
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn an_empty_dashboard_renders_its_chrome() {
        let text = Dashboard::new().view(Instant::now()).plain_text();
        assert!(text.contains("no nodes have reported"));
        assert!(text.contains("no jobs"));
        assert!(text.contains("q quit"));
        assert!(text.contains("lu top"));
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
        assert!(text.contains("VERSION"));
        assert!(text.contains("LOAD"));
        assert!(text.contains("MEM"));
        assert!(!text.contains("lu top"));
        assert!(!text.contains("q quit"));
        assert!(!text.contains("running jobs"));
        assert!(!text.contains('\u{1b}'));
    }

    #[allow(clippy::too_many_arguments)]
    fn node_full(
        name: &str,
        harness: &str,
        seen: Option<u64>,
        version: Option<&str>,
        load1: Option<f64>,
        mem_available_kb: Option<u64>,
        mem_total_kb: Option<u64>,
        stuck: u64,
    ) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            harness: harness.to_owned(),
            drained: false,
            seen_secs_ago: seen,
            providers: Vec::new(),
            cordons: Vec::new(),
            version: version.map(str::to_owned),
            load1,
            mem_available_kb,
            mem_total_kb,
            stuck,
            workspace_bytes: 0,
            workspace_count: 0,
            workspace_reclaimed_bytes_total: 0,
            workspace_evictions_total: 0,
            workspace_delete_errors_total: 0,
            labels: Vec::new(),
        }
    }

    #[test]
    fn node_labels_show_after_the_providers_and_are_sanitised() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut node = node_full("n1", "idle", Some(1), None, None, None, None, 0);
        node.labels = vec!["rust".to_owned(), "gpu\u{1b}[31m".to_owned()];
        dashboard.apply(
            Ok(Snapshot {
                nodes: vec![node],
                jobs: Vec::new(),
                ..Snapshot::default()
            }),
            t0,
        );
        let text = dashboard.view(t0).plain_text();
        assert!(text.contains("[rust,gpu?[31m]"), "{text}");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn node_load_mem_thresholds_and_absent_values() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: vec![
                    node_full(
                        "n1",
                        "idle",
                        Some(1),
                        None,
                        Some(1.234),
                        Some(300),
                        Some(1000),
                        0,
                    ), // 70%
                    node_full("n2", "idle", Some(1), None, None, Some(200), Some(1000), 0), // 80%
                    node_full(
                        "n3",
                        "idle",
                        Some(1),
                        None,
                        Some(0.0),
                        Some(40),
                        Some(1000),
                        0,
                    ), // 96%
                    node_full("n4", "idle", Some(1), None, None, None, None, 0),
                ],
                jobs: Vec::new(),
            }),
            t0,
        );
        let view = dashboard.view(t0);
        let text = view.plain_text();
        assert!(text.contains("1.2"), "{text}");
        assert!(text.contains("70%"), "{text}");
        assert!(text.contains("80%"), "{text}");
        assert!(text.contains("96%"), "{text}");
        let n2 = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("n2"))
            .unwrap();
        assert!(
            n2.segments
                .iter()
                .any(|s| s.text.contains("80%") && s.tone == Tone::Warn),
            "{:?}",
            n2.segments
        );
        let n3 = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("n3"))
            .unwrap();
        assert!(
            n3.segments
                .iter()
                .any(|s| s.text.contains("96%") && s.tone == Tone::Bad),
            "{:?}",
            n3.segments
        );
        let n1 = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("n1"))
            .unwrap();
        assert!(
            n1.segments
                .iter()
                .any(|s| s.text.contains("70%") && s.tone == Tone::Good),
            "{:?}",
            n1.segments
        );
        let n4 = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("n4"))
            .unwrap();
        assert!(n4.text.contains("-"), "{text}");
    }

    #[test]
    fn stuck_node_shows_red_marker_after_state() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut row = node("ct-1", "busy", Some(1));
        row.stuck = 2;
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: vec![row],
                jobs: Vec::new(),
            }),
            t0,
        );
        let view = dashboard.view(t0);
        let line = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("ct-1"))
            .unwrap();
        assert!(line.text.contains("busy stuck"), "{:?}", line.text);
        assert!(
            line.segments
                .iter()
                .any(|s| s.text.trim() == "busy stuck" && s.tone == Tone::NodeOffline),
            "{:?}",
            line.segments
        );
    }

    #[test]
    fn node_version_mismatch_is_orange_and_title_shows_versions() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: Some("0.2.0".to_owned()),
                nodes: vec![
                    node_full("same", "idle", Some(1), Some("0.2.0"), None, None, None, 0),
                    node_full("old", "idle", Some(1), Some("0.1.0"), None, None, None, 0),
                    node_full("none", "idle", Some(1), None, None, None, None, 0),
                ],
                jobs: Vec::new(),
            }),
            t0,
        );
        let view = dashboard.view(t0);
        assert!(view.title.contains("lu top "), "{:?}", view.title);
        assert!(view.title.contains("controller 0.2.0"), "{:?}", view.title);
        let old = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("old"))
            .unwrap();
        assert!(
            old.segments
                .iter()
                .any(|s| s.text.trim() == "0.1.0" && s.tone == Tone::Warn),
            "{:?}",
            old.segments
        );
        let none = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("none"))
            .unwrap();
        assert!(
            none.segments
                .iter()
                .any(|s| s.text.trim() == "-" && s.tone == Tone::Warn),
            "{:?}",
            none.segments
        );
        let same = view
            .lines
            .iter()
            .find(|l| l.text.starts_with("same"))
            .unwrap();
        assert!(
            same.segments
                .iter()
                .any(|s| s.text.trim() == "0.2.0" && s.tone == Tone::Plain),
            "{:?}",
            same.segments
        );
    }

    #[test]
    fn per_node_history_is_capped_at_150_samples() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        for i in 0..160u64 {
            dashboard.apply(
                Ok(Snapshot {
                    drafts: 0,
                    oldest_draft_secs: 0,
                    controller_version: None,
                    nodes: vec![node_full(
                        "n1",
                        "idle",
                        Some(1),
                        None,
                        Some(i as f64 / 10.0),
                        Some(1000 - (i % 100)),
                        Some(1000),
                        0,
                    )],
                    jobs: Vec::new(),
                }),
                t0,
            );
        }
        // open process pane to expose spark data
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: vec![node_full(
                    "n1",
                    "idle",
                    Some(1),
                    None,
                    Some(1.0),
                    Some(500),
                    Some(1000),
                    0,
                )],
                jobs: vec![JobRow {
                    id: "01RUN".to_owned(),
                    title: "run".to_owned(),
                    state: "running".to_owned(),
                    node: Some("n1".to_owned()),
                    repo: "owner/name".to_owned(),
                    git_ref: "main".to_owned(),
                    attempt: 1,
                    age_secs: 1,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            t0,
        );
        dashboard.toggle_focus(); // jobs
        let opened = dashboard.toggle_process_pane();
        assert!(opened.is_some());
        let pane = dashboard.view(t0).process_pane.unwrap();
        assert_eq!(pane.load_spark.len(), 150, "load history capped");
        assert_eq!(pane.mem_spark.len(), 150, "mem history capped");
    }

    #[test]
    fn process_pane_shows_counters_tree_colours_and_no_stats() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: vec![node_full(
                    "n1",
                    "busy",
                    Some(1),
                    None,
                    Some(1.0),
                    Some(500),
                    Some(1000),
                    0,
                )],
                jobs: vec![JobRow {
                    id: "01RUN".to_owned(),
                    title: "run".to_owned(),
                    state: "running".to_owned(),
                    node: Some("n1".to_owned()),
                    repo: "owner/name".to_owned(),
                    git_ref: "main".to_owned(),
                    attempt: 1,
                    age_secs: 1,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            t0,
        );
        dashboard.toggle_focus();
        dashboard.toggle_process_pane();
        dashboard.apply_update(Update::ProcessStats {
            job_id: "01RUN".to_owned(),
            node: "n1".to_owned(),
            result: Ok(Some(ProcessJobView {
                output_idle_secs: 130,
                cpu_idle_secs: 140,
                uploader_behind: None,
                stuck: false,
                processes: vec![
                    ProcessTreeRow {
                        pid: 10,
                        state: "D".to_owned(),
                        cpu_s: 1,
                        rss_kb: 2,
                        age_secs: 3,
                        command: "cmd".to_owned(),
                    },
                    ProcessTreeRow {
                        pid: 11,
                        state: "Z".to_owned(),
                        cpu_s: 0,
                        rss_kb: 0,
                        age_secs: 4,
                        command: "zombie".to_owned(),
                    },
                ],
            })),
        });
        let view = dashboard.view(t0);
        let pane = view.process_pane.as_ref().unwrap();
        assert!(
            pane.lines[0].text.contains("output idle 130s"),
            "{:?}",
            pane.lines
        );
        assert!(
            pane.lines[0].text.contains("cpu idle 140s"),
            "{:?}",
            pane.lines
        );
        assert!(
            pane.lines[0].text.contains("uploader behind -"),
            "{:?}",
            pane.lines
        );
        assert_eq!(
            pane.lines[0].tone,
            Tone::Warn,
            "idle>120s and not stuck => orange/warn"
        );
        let drow = pane
            .lines
            .iter()
            .find(|l| l.text.contains("  10 ") || l.text.starts_with("     10"))
            .unwrap();
        assert!(
            drow.segments
                .iter()
                .any(|s| s.text.trim() == "D" && s.tone == Tone::Bad),
            "{:?}",
            drow.segments
        );
        let zrow = pane
            .lines
            .iter()
            .find(|l| l.text.contains("  11 ") || l.text.starts_with("     11"))
            .unwrap();
        assert!(
            zrow.segments
                .iter()
                .any(|s| s.text.trim() == "Z" && s.tone == Tone::Bad),
            "{:?}",
            zrow.segments
        );

        dashboard.apply_update(Update::ProcessStats {
            job_id: "01RUN".to_owned(),
            node: "n1".to_owned(),
            result: Ok(Some(ProcessJobView {
                output_idle_secs: 130,
                cpu_idle_secs: 140,
                uploader_behind: Some("5s".to_owned()),
                stuck: true,
                processes: Vec::new(),
            })),
        });
        let pane = dashboard.view(t0).process_pane.unwrap();
        assert_eq!(pane.lines[0].tone, Tone::Bad, "stuck => red");
        assert!(
            pane.lines[0].text.contains("uploader behind 5s"),
            "{:?}",
            pane.lines[0].text
        );

        dashboard.apply_update(Update::ProcessStats {
            job_id: "01RUN".to_owned(),
            node: "n1".to_owned(),
            result: Ok(None),
        });
        let pane = dashboard.view(t0).process_pane.unwrap();
        assert_eq!(pane.lines.len(), 1);
        assert_eq!(pane.lines[0].tone, Tone::Dim);
        assert!(
            pane.lines[0].text.contains("no stats"),
            "{:?}",
            pane.lines[0].text
        );
    }

    #[test]
    fn hostile_names_versions_and_process_fields_are_sanitized() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: Some("ctrl\u{1b}[31m".to_owned()),
                nodes: vec![node_full(
                    "node\u{1b}[31m",
                    "idle",
                    Some(1),
                    Some("v\u{202e}1"),
                    Some(1.0),
                    Some(500),
                    Some(1000),
                    0,
                )],
                jobs: vec![JobRow {
                    id: "01RUN".to_owned(),
                    title: "run".to_owned(),
                    state: "running".to_owned(),
                    node: Some("node\u{1b}[31m".to_owned()),
                    repo: "owner/name".to_owned(),
                    git_ref: "main".to_owned(),
                    attempt: 1,
                    age_secs: 1,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            t0,
        );
        dashboard.toggle_focus();
        dashboard.toggle_process_pane();
        dashboard.apply_update(Update::ProcessStats {
            job_id: "01RUN".to_owned(),
            node: "node\u{1b}[31m".to_owned(),
            result: Ok(Some(ProcessJobView {
                output_idle_secs: 1,
                cpu_idle_secs: 1,
                uploader_behind: Some("x\u{1b}[2J".to_owned()),
                stuck: false,
                processes: vec![ProcessTreeRow {
                    pid: 1,
                    state: "R\u{1b}[0m".to_owned(),
                    cpu_s: 1,
                    rss_kb: 1,
                    age_secs: 1,
                    command: "echo \u{2028}bad".to_owned(),
                }],
            })),
        });
        let view = dashboard.view(t0);
        let all = view.plain_text();
        assert!(!all.contains('\u{1b}'), "{all}");
        assert!(!all.contains('\u{202e}'), "{all}");
        assert!(!all.contains('\u{2028}'), "{all}");
    }

    fn chain() -> Dashboard {
        let mut jobs = snapshot().jobs;
        jobs[0].after.clear();
        jobs[1].after = vec!["01ABC".to_owned()];
        let mut third = job("01XYZ", "running");
        third.title = "a title long enough to need clipping in a narrow graph panel".to_owned();
        third.after = vec!["01BLK".to_owned()];
        jobs.push(third);
        let mut dashboard = Dashboard::new();
        dashboard.apply(Ok(Snapshot { jobs, ..snapshot() }), Instant::now());
        dashboard
    }

    #[derive(Debug, Clone, Copy)]
    enum Key {
        Up,
        Down,
        Left,
        Right,
        Home,
        End,
        Enter,
        Drain,
    }

    fn apply_key(dashboard: &mut Dashboard, key: Key) {
        match key {
            Key::Up => dashboard.move_selection(false),
            Key::Down => dashboard.move_selection(true),
            Key::Left => dashboard.move_graph(graph::Step::Parent),
            Key::Right => dashboard.move_graph(graph::Step::Child),
            Key::Home => dashboard.move_graph(graph::Step::First),
            Key::End => dashboard.move_graph(graph::Step::Last),
            Key::Enter => {
                dashboard.open_selected_log();
            }
            Key::Drain => {
                dashboard.request_drain(Instant::now());
            }
        }
    }

    #[test]
    fn keys_act_on_the_focused_pane() {
        let t0 = Instant::now();
        let cases: &[(Focus, &[Key], Option<&str>)] = &[
            (
                Focus::Nodes,
                &[Key::Down, Key::Up, Key::Right, Key::End, Key::Drain],
                Some("01ABC"),
            ),
            (
                Focus::Jobs,
                &[Key::Down, Key::Left, Key::End, Key::Enter],
                Some("01XYZ"),
            ),
            (
                Focus::Graph,
                &[Key::Right, Key::Right, Key::Right],
                Some("01XYZ"),
            ),
            (
                Focus::Graph,
                &[Key::End, Key::Left, Key::Right],
                Some("01XYZ"),
            ),
            (Focus::Graph, &[Key::Home, Key::Left], Some("01ABC")),
        ];
        for (focus, keys, expected_job) in cases {
            let mut dashboard = chain();
            dashboard.select_job_at(2, *focus);
            for key in *keys {
                apply_key(&mut dashboard, *key);
            }
            let view = dashboard.view(t0);
            assert_eq!(view.focus, *focus, "{focus:?} {keys:?}");
            assert_eq!(
                view.selected_job.as_deref(),
                *expected_job,
                "{focus:?} {keys:?}"
            );
        }

        let mut nodes = chain();
        assert_eq!(
            nodes.open_selected_log(),
            None,
            "Enter does nothing in Nodes"
        );
        assert_eq!(nodes.request_drain(t0), None, "first d only asks");
        assert_eq!(
            nodes.request_drain(t0).map(|drain| drain.node),
            Some("ct-1".to_owned())
        );
        let mut jobs = chain();
        jobs.toggle_focus();
        assert_eq!(jobs.request_drain(t0), None, "d does nothing in Jobs");
        assert_eq!(jobs.open_selected_log(), Some("01ABC".to_owned()));
        let mut graph = chain();
        graph.select_job_at(0, Focus::Graph);
        assert_eq!(graph.request_drain(t0), None, "d does nothing in Graph");
        assert_eq!(graph.open_selected_log(), Some("01ABC".to_owned()));
    }

    #[test]
    fn tab_cycles_focus_in_both_directions() {
        let mut dashboard = chain();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Nodes);
        dashboard.toggle_focus();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Jobs);
        dashboard.toggle_focus();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Graph);
        dashboard.toggle_focus();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Nodes);
        dashboard.toggle_focus_back();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Graph);
        dashboard.toggle_focus_back();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Jobs);
        dashboard.toggle_focus_back();
        assert_eq!(dashboard.view(Instant::now()).focus, Focus::Nodes);
    }

    #[test]
    fn the_jobs_table_and_the_graph_share_one_selected_job() {
        let t0 = Instant::now();
        let mut dashboard = chain();
        assert!(dashboard.select_job_id("01BLK", Focus::Jobs));
        let view = dashboard.view(t0);
        assert_eq!(view.selected_job.as_deref(), Some("01BLK"));
        assert_eq!(
            view.selected_line,
            Some(6),
            "second job row in the Jobs pane"
        );
        assert!(dashboard.select_job_id("01ABC", Focus::Graph));
        let view = dashboard.view(t0);
        assert_eq!(view.selected_job.as_deref(), Some("01ABC"));
        assert_eq!(view.selected_line, None, "graph focus has no table row");
        dashboard.toggle_focus_back();
        assert_eq!(dashboard.view(t0).selected_line, Some(5));
        assert!(!dashboard.select_job_id("MISSING", Focus::Graph));
        assert!(!dashboard.select_job_at(99, Focus::Jobs));
        assert_eq!(dashboard.view(t0).selected_job.as_deref(), Some("01ABC"));
    }

    fn navigation_graph() -> (Dashboard, Graph) {
        let jobs = vec![
            job("01A", "running"),
            JobRow {
                after: vec!["01A".to_owned()],
                ..job("01B", "running")
            },
            JobRow {
                after: vec!["01B".to_owned()],
                ..job("01C", "running")
            },
            JobRow {
                after: vec!["01B".to_owned()],
                ..job("01D", "running")
            },
            JobRow {
                after: vec!["01C".to_owned(), "01D".to_owned()],
                ..job("01E", "running")
            },
            JobRow {
                after: vec!["MISSING".to_owned()],
                ..job("01F", "running")
            },
        ];
        let graph = Graph::build(
            &jobs
                .iter()
                .map(|job| graph::Job {
                    id: job.id.clone(),
                    state: graph::State::Running,
                    title: job.title.clone(),
                    after: job.after.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                jobs,
                ..Snapshot::default()
            }),
            Instant::now(),
        );
        dashboard.select_job_at(0, Focus::Graph);
        (dashboard, graph)
    }

    fn selected(dashboard: &Dashboard) -> String {
        dashboard
            .view(Instant::now())
            .selected_job
            .expect("a selected job")
    }

    #[test]
    fn graph_navigation_covers_chains_fan_out_fan_in_and_stubs() {
        let (mut dashboard, graph) = navigation_graph();

        dashboard.move_graph(graph::Step::Child);
        assert_eq!(selected(&dashboard), "01B", "chain child");
        let child = graph.step("01B", graph::Step::Child).unwrap();
        dashboard.move_graph(graph::Step::Child);
        assert_eq!(selected(&dashboard), child, "first child of the fan-out");
        let parent = graph.step(&child, graph::Step::Parent).unwrap();
        dashboard.move_graph(graph::Step::Parent);
        assert_eq!(selected(&dashboard), parent);
        let sibling = graph.step(&child, graph::Step::Next).unwrap();
        dashboard.select_job_id(&child, Focus::Graph);
        dashboard.move_graph(graph::Step::Next);
        assert_eq!(
            selected(&dashboard),
            sibling,
            "next sibling at the same depth"
        );
        let back = graph.step(&sibling, graph::Step::Previous).unwrap();
        dashboard.move_graph(graph::Step::Previous);
        assert_eq!(selected(&dashboard), back);

        dashboard.select_job_id("01E", Focus::Graph);
        dashboard.move_graph(graph::Step::Parent);
        assert_eq!(
            selected(&dashboard),
            graph.step("01E", graph::Step::Parent).unwrap()
        );

        dashboard.select_job_id("01F", Focus::Graph);
        dashboard.move_graph(graph::Step::Parent);
        assert_eq!(selected(&dashboard), "01F", "a stub is not a parent");
        dashboard.move_graph(graph::Step::Child);
        assert_eq!(selected(&dashboard), "01F", "no children at an edge");

        let order = graph.draw_order();
        dashboard.move_graph(graph::Step::First);
        assert_eq!(selected(&dashboard), order.first().unwrap().as_str());
        dashboard.move_graph(graph::Step::Last);
        assert_eq!(selected(&dashboard), order.last().unwrap().as_str());
        dashboard.move_graph(graph::Step::Child);
        assert_eq!(selected(&dashboard), order.last().unwrap().as_str(), "edge");
    }

    #[test]
    fn the_graph_shows_the_selected_jobs_full_details_and_a_focus_specific_footer() {
        let t0 = Instant::now();
        let mut dashboard = chain();
        let nodes = dashboard.view(t0).footer.clone();
        dashboard.toggle_focus();
        let jobs = dashboard.view(t0).footer.clone();
        dashboard.toggle_focus();
        let graph = dashboard.view(t0).footer.clone();
        assert!(nodes.contains("d drain") && !nodes.contains("Enter log"));
        assert!(jobs.contains("Enter log") && jobs.contains("x proc"));
        assert!(graph.contains("h/l parent-child") && graph.contains("Home/End"));
        assert_ne!(nodes, jobs);
        assert_ne!(jobs, graph);

        dashboard.select_job_id("01XYZ", Focus::Graph);
        let view = dashboard.view(t0);
        let detail = view.graph_detail.expect("the selected job's detail");
        assert!(detail.text.contains("01XYZ"), "{detail:?}");
        assert!(detail.text.contains("running"), "{detail:?}");
        assert!(detail.text.contains("a title long enough"), "{detail:?}");
    }

    #[test]
    fn the_wheel_moves_only_the_focused_pane() {
        let (mut dashboard, _) = navigation_graph();
        dashboard.scroll_graph(true);
        assert_eq!(dashboard.view(Instant::now()).graph_offset, 3);
        dashboard.scroll_graph(false);
        assert_eq!(dashboard.view(Instant::now()).graph_offset, 0);
        dashboard.toggle_focus_back();
        dashboard.scroll_graph(true);
        assert_eq!(dashboard.view(Instant::now()).graph_offset, 0);
        dashboard.scroll_table(true);
        assert_eq!(dashboard.view(Instant::now()).selected_line, Some(7));
        dashboard.scroll_table(false);
        assert_eq!(dashboard.view(Instant::now()).selected_line, Some(4));
    }

    #[test]
    fn title_names_providers_that_queued_jobs_are_waiting_for() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut snap = snapshot();
        snap.jobs = vec![job("01Q", "queued"), job("01R", "queued")];
        dashboard.apply(Ok(snap), t0);
        let text = dashboard.view(t0).plain_text();
        assert!(text.contains("waiting for zen 2h00m (2 queued)"), "{text}");

        // A node that can run zen again clears the note, and so does an empty queue.
        let mut snap = snapshot();
        snap.jobs = vec![job("01Q", "queued")];
        snap.nodes[1].providers[0].state = "ok".to_owned();
        snap.nodes[1].providers[0].soonest_secs = None;
        let mut dashboard = Dashboard::new();
        dashboard.apply(Ok(snap), t0);
        assert!(!dashboard.view(t0).plain_text().contains("waiting for"));
        let mut dashboard = Dashboard::new();
        dashboard.apply(Ok(snapshot()), t0);
        assert!(!dashboard.view(t0).plain_text().contains("waiting for"));
    }

    #[test]
    fn problem_providers_sort_before_healthy_ones_in_the_node_row() {
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        let mut snap = snapshot();
        snap.nodes[0].providers = vec![
            ProviderCell {
                name: "alpha".to_owned(),
                state: "ok".to_owned(),
                soonest_secs: None,
            },
            ProviderCell {
                name: "zen".to_owned(),
                state: "cooling".to_owned(),
                soonest_secs: Some(60),
            },
        ];
        dashboard.apply(Ok(snap), t0);
        let text = dashboard.view(t0).plain_text();
        let zen = text.find("zen cooling").expect(&text);
        let alpha = text.find("alpha ok").expect(&text);
        assert!(zen < alpha, "{text}");
    }

    fn credential_shaped_source() -> (String, String, String, String) {
        let scheme = format!("ht{}", "tps");
        let user = "u".repeat(7);
        let secret = "p".repeat(9);
        let host = format!("git{}.example", "hub");
        let at = ['@'].iter().collect::<String>();
        let colon = [':'].iter().collect::<String>();
        let source = [
            scheme.as_str(),
            "://",
            &user,
            &colon,
            &secret,
            &at,
            &host,
            "/",
            "org",
            "/",
            "widgets",
            ".git",
            "?",
            "ref",
            "=",
            "main",
            "#",
            "section",
        ]
        .concat();
        (source, user, secret, host)
    }

    #[test]
    fn repo_labels_drop_runtime_built_credentials_and_extras() {
        let (source, user, secret, host) = credential_shaped_source();
        let label = repo_label(&source);
        assert_eq!(label, "org/widgets");
        assert!(!label.contains(&user));
        assert!(!label.contains(&secret));
        assert!(!label.contains(&host));
        assert_eq!(repo_label(""), "-");
        assert_eq!(repo_label("   "), "-");
        assert_eq!(repo_label("owner/../name"), "-");
        assert_eq!(repo_label(&format!("owner/{}", "x".repeat(257))), "-");
        assert_eq!(repo_label("ftp://host/owner/name"), "-");
    }

    #[test]
    fn rendered_repos_never_show_credentials_or_query_parts() {
        let (source, user, secret, host) = credential_shaped_source();
        let t0 = Instant::now();
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                jobs: vec![JobRow {
                    repo: source,
                    ..job("01REPO", "running")
                }],
                ..snapshot()
            }),
            t0,
        );
        let text = dashboard.view(t0).plain_text();
        assert!(text.contains("org/widgets"), "{text}");
        for hidden in [&user, &secret, &host] {
            assert!(
                !text.contains(hidden),
                "credential text leaked into the render"
            );
        }
        assert!(!text.contains("ref=main"), "{text}");
        assert!(!text.contains("section"), "{text}");
    }

    #[test]
    fn job_badge_tones_cover_every_state() {
        let cases = [
            (JobState::Draft.as_str(), Tone::Blocked),
            (JobState::Blocked.as_str(), Tone::Blocked),
            (JobState::Queued.as_str(), Tone::JobQueued),
            (JobState::Recovering.as_str(), Tone::JobQueued),
            (JobState::NeedsInput.as_str(), Tone::JobQueued),
            (JobState::Leased.as_str(), Tone::JobRunning),
            (JobState::Running.as_str(), Tone::JobRunning),
            (JobState::Done.as_str(), Tone::JobDone),
            (JobState::Failed.as_str(), Tone::JobFailed),
            (JobState::Cancelled.as_str(), Tone::JobCancelled),
            (JobState::Expired.as_str(), Tone::JobCancelled),
        ];
        for (state, expected) in cases {
            assert_eq!(job_badge_tone(state), expected, "{state}");
        }
        assert_eq!(job_badge_tone("unknown"), Tone::JobCancelled);
    }

    #[test]
    fn node_badge_tones_cover_busy_idle_drained_offline_stuck_and_unknown() {
        let busy = node("n1", "busy", Some(1));
        assert_eq!(node_badge_tone(&busy), Tone::NodeBusy);
        let idle = node("n1", "idle", Some(1));
        assert_eq!(node_badge_tone(&idle), Tone::NodeIdle);
        let drained = NodeRow {
            drained: true,
            ..node("n1", "idle", Some(1))
        };
        assert_eq!(node_badge_tone(&drained), Tone::NodeDrained);
        let draining = node("n1", "draining", Some(1));
        assert_eq!(node_badge_tone(&draining), Tone::NodeDrained);
        let offline = node("n1", "offline", Some(1));
        assert_eq!(node_badge_tone(&offline), Tone::NodeOffline);
        let stale = node("n1", "idle", Some(STALE_SECS + 1));
        assert_eq!(node_badge_tone(&stale), Tone::NodeOffline);
        let stuck = NodeRow {
            stuck: 1,
            ..node("n1", "idle", Some(1))
        };
        assert_eq!(node_badge_tone(&stuck), Tone::NodeOffline);
        let unknown = node("n1", "unknown", Some(1));
        assert_eq!(node_badge_tone(&unknown), Tone::Plain);
    }
}
