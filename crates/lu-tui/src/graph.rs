//! Dependency graph for `lu top` and `lu graph`. Pure text and data: no terminal is touched.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::model::Tone;
use crate::sanitize::printable;

pub use lu_proto::JobState as State;

/// Components shown by one graph.
pub const MAX_COMPONENTS: usize = 6;
/// Jobs shown by one graph.
pub const MAX_JOBS: usize = 40;

const GAP: usize = 9;
const SHORT_ID: usize = 5;
const TITLE_CHARS: usize = 30;

/// Presentation rules the graph adds to a wire [`State`].
pub trait StateStyle {
    fn glyph(self) -> char;
    fn tone(self) -> Tone;
    /// Finished jobs do not keep a component on screen.
    fn unfinished(self) -> bool;
}

impl StateStyle for State {
    fn glyph(self) -> char {
        match self {
            Self::Done => '✓',
            Self::Running | Self::Leased => '▶',
            Self::Blocked => '◌',
            Self::Queued | Self::Recovering | Self::NeedsInput => '○',
            Self::Failed => '✗',
            Self::Cancelled => '–',
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Done => Tone::Done,
            Self::Running | Self::Leased => Tone::Good,
            Self::Blocked => Tone::Blocked,
            Self::Queued | Self::Recovering | Self::NeedsInput => Tone::Warn,
            Self::Failed => Tone::Bad,
            Self::Cancelled => Tone::Dim,
        }
    }

    fn unfinished(self) -> bool {
        !self.is_terminal()
    }
}

/// The part of a job the graph needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub state: State,
    pub title: String,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub state: State,
    pub title: String,
    /// 0 without a dependency in the fetched set, otherwise one past its deepest dependency.
    pub depth: usize,
    pub after: Vec<String>,
}

/// A dependency that is not in the fetched job list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    pub id: String,
    /// The jobs in this component that named the missing dependency.
    pub to: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Newest first, as the controller lists jobs.
    pub nodes: Vec<Node>,
    pub stubs: Vec<Stub>,
}

impl Component {
    pub fn unfinished(&self) -> bool {
        self.nodes.iter().any(|node| node.state.unfinished())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub components: Vec<Component>,
    /// Components left out by the cap.
    pub hidden_components: usize,
    /// Jobs left out by the cap.
    pub hidden_jobs: usize,
}

impl Graph {
    /// Builds the components that contain at least one unfinished job, newest first.
    pub fn build(jobs: &[Job]) -> Self {
        let mut components = all_components(jobs);
        components.retain(Component::unfinished);
        cap(components)
    }

    /// The component containing `job_id`, even if every job in it is finished.
    pub fn for_job(jobs: &[Job], job_id: &str) -> Option<Self> {
        let component = all_components(jobs)
            .into_iter()
            .find(|component| component.nodes.iter().any(|node| node.id == job_id))?;
        Some(cap(vec![component]))
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

fn all_components(jobs: &[Job]) -> Vec<Component> {
    let by_id: HashMap<&str, usize> = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| (job.id.as_str(), index))
        .collect();
    let mut neighbours = vec![Vec::new(); jobs.len()];
    for (index, job) in jobs.iter().enumerate() {
        for dependency in &job.after {
            if let Some(&other) = by_id.get(dependency.as_str()) {
                neighbours[index].push(other);
                neighbours[other].push(index);
            }
        }
    }

    let mut seen = vec![false; jobs.len()];
    let mut memo = vec![None; jobs.len()];
    let mut components = Vec::new();
    for start in 0..jobs.len() {
        if seen[start] {
            continue;
        }
        let mut members = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(index) = stack.pop() {
            members.push(index);
            for &next in &neighbours[index] {
                if !seen[next] {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
        // The controller lists newest first; keep that order inside the component too.
        members.sort_unstable();

        let mut visiting = vec![false; jobs.len()];
        let nodes = members
            .iter()
            .map(|&index| Node {
                id: jobs[index].id.clone(),
                state: jobs[index].state,
                title: jobs[index].title.clone(),
                depth: depth_of(index, jobs, &by_id, &mut memo, &mut visiting),
                after: jobs[index].after.clone(),
            })
            .collect::<Vec<_>>();

        let mut stubs: Vec<Stub> = Vec::new();
        for &index in &members {
            for dependency in &jobs[index].after {
                if by_id.contains_key(dependency.as_str()) {
                    continue;
                }
                match stubs.iter_mut().find(|stub| stub.id == *dependency) {
                    Some(stub) => {
                        if !stub.to.contains(&jobs[index].id) {
                            stub.to.push(jobs[index].id.clone());
                        }
                    }
                    None => stubs.push(Stub {
                        id: dependency.clone(),
                        to: vec![jobs[index].id.clone()],
                    }),
                }
            }
        }
        components.push(Component { nodes, stubs });
    }
    components
}

fn depth_of(
    index: usize,
    jobs: &[Job],
    by_id: &HashMap<&str, usize>,
    memo: &mut [Option<usize>],
    visiting: &mut [bool],
) -> usize {
    if let Some(depth) = memo[index] {
        return depth;
    }
    if visiting[index] {
        // A dependency cycle is invalid at the controller, but never recurse forever.
        return 0;
    }
    visiting[index] = true;
    let mut deepest = None;
    for dependency in &jobs[index].after {
        if let Some(&other) = by_id.get(dependency.as_str()) {
            if other == index {
                continue;
            }
            let depth = depth_of(other, jobs, by_id, memo, visiting);
            deepest = Some(deepest.map_or(depth, |found: usize| found.max(depth)));
        }
    }
    visiting[index] = false;
    let depth = deepest.map_or(0, |found| found + 1);
    memo[index] = Some(depth);
    depth
}

fn cap(components: Vec<Component>) -> Graph {
    let total_jobs: usize = components
        .iter()
        .map(|component| component.nodes.len())
        .sum();
    let total_components = components.len();
    let mut kept = Vec::new();
    let mut jobs = 0;
    for mut component in components {
        if kept.len() >= MAX_COMPONENTS || jobs >= MAX_JOBS {
            break;
        }
        if jobs + component.nodes.len() <= MAX_JOBS {
            jobs += component.nodes.len();
            kept.push(component);
            continue;
        }
        let room = MAX_JOBS - jobs;
        component.nodes.truncate(room);
        let ids: HashSet<&str> = component
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        component
            .stubs
            .retain(|stub| stub.to.iter().any(|to| ids.contains(to.as_str())));
        jobs += component.nodes.len();
        kept.push(component);
        break;
    }
    Graph {
        hidden_components: total_components.saturating_sub(kept.len()),
        hidden_jobs: total_jobs.saturating_sub(jobs),
        components: kept,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub tone: Tone,
}

/// One line of a rendered graph. A row can mix tones, because an edge takes its
/// dependency's tone while the box on that row has its own state's tone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphLine {
    pub segments: Vec<Segment>,
}

impl GraphLine {
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    fn blank() -> Self {
        Self::default()
    }
}

/// Lays out every component left to right by depth, one row per node, and clips to `width`.
pub fn render(graph: &Graph, width: usize) -> Vec<GraphLine> {
    let width = width.max(20);
    let mut lines = Vec::new();
    for (index, component) in graph.components.iter().enumerate() {
        if index > 0 {
            lines.push(GraphLine::blank());
        }
        lines.extend(render_component(component, width));
    }
    if let Some(summary) = summary(graph) {
        lines.push(GraphLine {
            segments: vec![Segment {
                text: summary,
                tone: Tone::Dim,
            }],
        });
    }
    lines
}

fn summary(graph: &Graph) -> Option<String> {
    let job = plural(graph.hidden_jobs, "job");
    let component = plural(graph.hidden_components, "component");
    match (graph.hidden_jobs, graph.hidden_components) {
        (0, 0) => None,
        (0, _) => Some(format!("… {component} not shown")),
        (_, 0) => Some(format!("… {job} not shown")),
        (_, _) => Some(format!("… {job} and {component} not shown")),
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 more {noun}")
    } else {
        format!("{count} more {noun}s")
    }
}

struct DrawNode {
    id: String,
    label: String,
    tone: Tone,
    column: usize,
    y: f64,
    order: usize,
    after: Vec<String>,
}

struct Placed {
    row: usize,
    column: usize,
    label: String,
    tone: Tone,
    id: String,
    after: Vec<String>,
}

fn render_component(component: &Component, width: usize) -> Vec<GraphLine> {
    let base = usize::from(!component.stubs.is_empty());
    let mut draw: Vec<DrawNode> = component
        .nodes
        .iter()
        .enumerate()
        .map(|(order, node)| DrawNode {
            id: node.id.clone(),
            label: label(node.state, &node.id, &node.title),
            tone: node.state.tone(),
            column: base + node.depth,
            y: 0.0,
            order,
            after: node.after.clone(),
        })
        .collect();
    let by_id: HashMap<String, usize> = draw
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect();

    let mut next_y = 0.0;
    let max_depth = component
        .nodes
        .iter()
        .map(|node| node.depth)
        .max()
        .unwrap_or(0);
    for depth in 0..=max_depth {
        for (index, node) in component.nodes.iter().enumerate() {
            if node.depth != depth {
                continue;
            }
            if depth == 0 {
                draw[index].y = next_y;
                next_y += 1.0;
                continue;
            }
            let mut total = 0.0;
            let mut count = 0;
            for dependency in &node.after {
                if let Some(&other) = by_id.get(dependency) {
                    if component.nodes[other].depth < depth {
                        total += draw[other].y;
                        count += 1;
                    }
                }
            }
            if count == 0 {
                draw[index].y = next_y;
                next_y += 1.0;
            } else {
                draw[index].y = total / f64::from(count);
            }
        }
        next_y = next_y.max(
            draw.iter()
                .filter(|node| node.column == base + depth)
                .map(|node| node.y + 1.0)
                .fold(0.0, f64::max),
        );
    }

    let order = draw.len();
    for (stub_index, stub) in component.stubs.iter().enumerate() {
        let mut total = 0.0;
        let mut count = 0;
        for to in &stub.to {
            if let Some(&index) = by_id.get(to) {
                total += draw[index].y;
                count += 1;
            }
        }
        draw.push(DrawNode {
            id: stub.id.clone(),
            label: format!(" #{}…", short_id(&stub.id)),
            tone: Tone::Dim,
            column: 0,
            y: if count == 0 {
                0.0
            } else {
                total / f64::from(count)
            },
            order: order + stub_index,
            after: Vec::new(),
        });
    }

    let mut indices: Vec<usize> = (0..draw.len()).collect();
    indices.sort_by(|&left, &right| {
        let a = &draw[left];
        let b = &draw[right];
        a.y.partial_cmp(&b.y)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.order.cmp(&b.order))
    });
    let rows = indices.len();
    let placed: Vec<Placed> = indices
        .into_iter()
        .enumerate()
        .map(|(row, index)| {
            let node = &draw[index];
            Placed {
                row,
                column: node.column,
                label: node.label.clone(),
                tone: node.tone,
                id: node.id.clone(),
                after: node.after.clone(),
            }
        })
        .collect();
    let by_id: HashMap<&str, usize> = placed
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();

    let max_column = placed.iter().map(|node| node.column).max().unwrap_or(0);
    let mut widths = vec![1usize; max_column + 1];
    for node in &placed {
        widths[node.column] = widths[node.column].max(node.label.chars().count());
    }
    let mut starts = vec![0usize; max_column + 1];
    for column in 1..=max_column {
        starts[column] = starts[column - 1] + widths[column - 1] + GAP;
    }
    let natural = starts[max_column] + widths[max_column];
    let line_width = natural.min(width);

    let mut glyphs = vec![vec![None; line_width]; rows];
    for node in &placed {
        let start = starts[node.column];
        for (offset, character) in node.label.chars().enumerate() {
            if let Some(cell) = glyphs[node.row].get_mut(start + offset) {
                *cell = Some((character, node.tone));
            }
        }
    }

    let mut directions = vec![vec![0u8; line_width]; rows];
    let mut tones: Vec<Vec<Option<Tone>>> = vec![vec![None; line_width]; rows];
    for node in &placed {
        if node.column == 0 && node.after.is_empty() {
            continue;
        }
        for dependency in &node.after {
            let Some(&source) = by_id.get(dependency.as_str()) else {
                continue;
            };
            let source = &placed[source];
            if node.column <= source.column {
                continue;
            }
            let tone = source.tone;
            let low = source.row.min(node.row);
            let high = source.row.max(node.row);
            for gap in source.column..node.column {
                let gap_start = starts[gap] + widths[gap];
                let trunk = gap_start + 5;
                let next = if gap < max_column {
                    starts[gap + 1]
                } else {
                    natural
                };
                let arrow = next.saturating_sub(1);
                for x in (gap_start + 2)..trunk {
                    add_direction(
                        &mut directions,
                        &mut tones,
                        source.row,
                        x,
                        DIR_LEFT | DIR_RIGHT,
                        tone,
                    );
                }
                for x in (trunk + 1)..arrow {
                    add_direction(
                        &mut directions,
                        &mut tones,
                        node.row,
                        x,
                        DIR_LEFT | DIR_RIGHT,
                        tone,
                    );
                }
                for row in low..=high {
                    let mut bits = 0;
                    if row > low {
                        bits |= DIR_UP;
                    }
                    if row < high {
                        bits |= DIR_DOWN;
                    }
                    if row == source.row {
                        bits |= DIR_LEFT;
                    }
                    if row == node.row {
                        bits |= DIR_RIGHT;
                    }
                    add_direction(&mut directions, &mut tones, row, trunk, bits, tone);
                }
                if gap + 1 == node.column {
                    if let Some(cell) = glyphs[node.row].get_mut(arrow) {
                        *cell = Some(('▶', tone));
                    }
                }
            }
        }
    }

    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut cells: Vec<Option<(char, Tone)>> = Vec::with_capacity(line_width);
        for x in 0..line_width {
            let cell = match glyphs[row][x] {
                Some(cell) => Some(cell),
                None if directions[row][x] != 0 => Some((
                    direction_char(directions[row][x]),
                    tones[row][x].unwrap_or(Tone::Plain),
                )),
                None => None,
            };
            cells.push(cell);
        }
        while matches!(cells.last(), Some(None)) {
            cells.pop();
        }
        if cells.is_empty() {
            continue;
        }
        lines.push(GraphLine {
            segments: segments(cells),
        });
    }
    lines
}

const DIR_UP: u8 = 1;
const DIR_RIGHT: u8 = 2;
const DIR_DOWN: u8 = 4;
const DIR_LEFT: u8 = 8;

fn add_direction(
    directions: &mut [Vec<u8>],
    tones: &mut [Vec<Option<Tone>>],
    row: usize,
    x: usize,
    bits: u8,
    tone: Tone,
) {
    let Some(cell) = directions.get_mut(row).and_then(|row| row.get_mut(x)) else {
        return;
    };
    *cell |= bits;
    if let Some(slot) = tones.get_mut(row).and_then(|row| row.get_mut(x)) {
        *slot = Some(tone);
    }
}

fn direction_char(bits: u8) -> char {
    const HORIZONTAL: u8 = DIR_LEFT | DIR_RIGHT;
    const VERTICAL: u8 = DIR_UP | DIR_DOWN;
    const DOWN_RIGHT: u8 = DIR_RIGHT | DIR_DOWN;
    const DOWN_LEFT: u8 = DIR_LEFT | DIR_DOWN;
    const UP_RIGHT: u8 = DIR_RIGHT | DIR_UP;
    const UP_LEFT: u8 = DIR_LEFT | DIR_UP;
    const DOWN_LEFT_RIGHT: u8 = DIR_LEFT | DIR_RIGHT | DIR_DOWN;
    const UP_LEFT_RIGHT: u8 = DIR_LEFT | DIR_RIGHT | DIR_UP;
    const UP_DOWN_RIGHT: u8 = DIR_RIGHT | DIR_UP | DIR_DOWN;
    const UP_DOWN_LEFT: u8 = DIR_LEFT | DIR_UP | DIR_DOWN;
    const CROSS: u8 = DIR_LEFT | DIR_RIGHT | DIR_UP | DIR_DOWN;
    match bits {
        HORIZONTAL => '─',
        VERTICAL => '│',
        DOWN_RIGHT => '┌',
        DOWN_LEFT => '┐',
        UP_RIGHT => '└',
        UP_LEFT => '┘',
        DOWN_LEFT_RIGHT => '┬',
        UP_LEFT_RIGHT => '┴',
        UP_DOWN_RIGHT => '├',
        UP_DOWN_LEFT => '┤',
        CROSS => '┼',
        _ => '─',
    }
}

fn segments(cells: Vec<Option<(char, Tone)>>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for cell in cells {
        let (character, tone) = cell.unwrap_or((' ', Tone::Plain));
        match out.last_mut() {
            Some(last) if last.tone == tone => last.text.push(character),
            _ => out.push(Segment {
                text: character.to_string(),
                tone,
            }),
        }
    }
    out
}

fn label(state: State, id: &str, title: &str) -> String {
    format!(
        " {} {} {}",
        state.glyph(),
        short_id(id),
        clip(title, TITLE_CHARS)
    )
}

fn short_id(id: &str) -> String {
    let id = printable(id);
    id.chars()
        .skip(id.chars().count().saturating_sub(SHORT_ID))
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    let text = printable(text);
    if text.chars().count() <= width {
        return text;
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, state: State, after: &[&str]) -> Job {
        Job {
            id: id.to_owned(),
            state,
            title: format!("task {id}"),
            after: after.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn depths(graph: &Graph) -> HashMap<String, usize> {
        graph
            .components
            .iter()
            .flat_map(|component| &component.nodes)
            .map(|node| (node.id.clone(), node.depth))
            .collect()
    }

    #[test]
    fn a_chain_is_layered_by_its_dependency() {
        let graph = Graph::build(&[
            job("C", State::Blocked, &["B"]),
            job("B", State::Queued, &["A"]),
            job("A", State::Done, &[]),
        ]);
        assert_eq!(
            depths(&graph),
            HashMap::from([
                ("A".to_owned(), 0),
                ("B".to_owned(), 1),
                ("C".to_owned(), 2),
            ])
        );
    }

    #[test]
    fn a_diamond_layers_both_sides_and_the_join() {
        let graph = Graph::build(&[
            job("D", State::Blocked, &["B", "C"]),
            job("C", State::Running, &["A"]),
            job("B", State::Running, &["A"]),
            job("A", State::Done, &[]),
        ]);
        assert_eq!(
            depths(&graph),
            HashMap::from([
                ("A".to_owned(), 0),
                ("B".to_owned(), 1),
                ("C".to_owned(), 1),
                ("D".to_owned(), 2),
            ])
        );
    }

    #[test]
    fn a_fan_in_waits_for_the_deepest_dependency() {
        let graph = Graph::build(&[
            job("C", State::Blocked, &["A", "B"]),
            job("B", State::Queued, &[]),
            job("A", State::Running, &[]),
        ]);
        let depths = depths(&graph);
        assert_eq!(depths["A"], 0);
        assert_eq!(depths["B"], 0);
        assert_eq!(depths["C"], 1);
    }

    #[test]
    fn disconnected_components_are_separate_and_newest_first() {
        let graph = Graph::build(&[
            job("B", State::Running, &["A"]),
            job("D", State::Queued, &[]),
            job("A", State::Done, &[]),
            job("C", State::Running, &[]),
        ]);
        assert_eq!(graph.components.len(), 3);
        assert_eq!(graph.components[0].nodes[0].id, "B");
        assert_eq!(graph.components[1].nodes[0].id, "D");
        assert_eq!(graph.components[2].nodes[0].id, "C");
    }

    #[test]
    fn a_component_with_only_finished_jobs_is_dropped() {
        let graph = Graph::build(&[
            job("B", State::Done, &["A"]),
            job("A", State::Failed, &[]),
            job("C", State::Running, &[]),
        ]);
        assert_eq!(graph.components.len(), 1);
        assert_eq!(graph.components[0].nodes[0].id, "C");
    }

    #[test]
    fn an_unknown_dependency_is_a_stub() {
        let graph = Graph::build(&[job("A", State::Running, &["MISSING"])]);
        let component = &graph.components[0];
        assert_eq!(component.stubs.len(), 1);
        assert_eq!(component.stubs[0].id, "MISSING");
        assert_eq!(component.nodes[0].depth, 0);
        let text: String = render(&graph, 100)
            .iter()
            .map(GraphLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("#SSING…"), "{text}");
    }

    #[test]
    fn the_component_cap_says_how_many_were_left_out() {
        let jobs: Vec<Job> = (0..8)
            .map(|index| Job {
                id: format!("J{index}"),
                state: State::Running,
                title: "task".to_owned(),
                after: Vec::new(),
            })
            .collect();
        let graph = Graph::build(&jobs);
        assert_eq!(graph.components.len(), MAX_COMPONENTS);
        assert_eq!(graph.hidden_components, 2);
        assert_eq!(graph.hidden_jobs, 2);
        let text: String = render(&graph, 100)
            .iter()
            .map(GraphLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("2 more jobs and 2 more components not shown"),
            "{text}"
        );
    }

    #[test]
    fn the_job_cap_says_how_many_were_left_out() {
        // One long chain, so the component cap cannot hide any of these jobs.
        let jobs: Vec<Job> = (0..(MAX_JOBS + 3))
            .map(|index| Job {
                id: format!("J{index}"),
                state: State::Running,
                title: "task".to_owned(),
                after: if index == 0 {
                    Vec::new()
                } else {
                    vec![format!("J{}", index - 1)]
                },
            })
            .collect();
        let graph = Graph::build(&jobs);
        let shown: usize = graph
            .components
            .iter()
            .map(|component| component.nodes.len())
            .sum();
        assert_eq!(shown, MAX_JOBS);
        assert_eq!(graph.hidden_jobs, 3);
        assert_eq!(graph.hidden_components, 0);
    }

    #[test]
    fn the_example_lays_out_two_dependencies_into_one_job() {
        let graph = Graph::build(&[
            job("01KE9AR", State::Done, &[]),
            job("01Z7W0A", State::Running, &[]),
            job("01H6CQM", State::Blocked, &["01KE9AR", "01Z7W0A"]),
        ]);
        let text = render(&graph, 100)
            .iter()
            .map(GraphLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("✓ KE9AR task 01KE9AR"), "{text}");
        assert!(text.contains("▶ Z7W0A task 01Z7W0A"), "{text}");
        assert!(text.contains("◌ H6CQM task 01H6CQM"), "{text}");
        assert!(text.contains('┐'), "{text}");
        assert!(text.contains("├──▶"), "{text}");
        assert!(text.contains('┘'), "{text}");
    }
}
