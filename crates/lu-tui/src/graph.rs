//! Dependency graph for `lu top` and `lu graph`. Pure text and data: no terminal is touched.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use crate::model::Tone;
use crate::sanitize::printable;

pub use lu_proto::JobState as State;

/// Components shown by one graph.
pub const MAX_COMPONENTS: usize = 6;
/// Jobs shown by one graph.
pub const MAX_JOBS: usize = 40;

const GAP: usize = 6;
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
            Self::Draft => '▧',
            Self::Failed => '✗',
            Self::Cancelled | Self::Expired => '–',
        }
    }

    fn tone(self) -> Tone {
        match self {
            Self::Done => Tone::Done,
            Self::Running | Self::Leased => Tone::Good,
            Self::Blocked | Self::Draft => Tone::Blocked,
            Self::Queued | Self::Recovering | Self::NeedsInput => Tone::Warn,
            Self::Failed => Tone::Bad,
            Self::Cancelled | Self::Expired => Tone::Dim,
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

    /// The nodes that can be selected, in the order they are drawn.
    pub fn draw_order(&self) -> Vec<String> {
        render(self, usize::MAX, false)
            .nodes
            .into_iter()
            .map(|node| node.id)
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<&Node> {
        self.components
            .iter()
            .flat_map(|component| component.nodes.iter())
            .find(|node| node.id == id)
    }

    /// Moves from `id` in one graph direction; `None` means there is nowhere to go.
    pub fn step(&self, id: &str, step: Step) -> Option<String> {
        let order = self.draw_order();
        let at = order.iter().position(|candidate| candidate == id)?;
        match step {
            Step::First => return order.first().cloned(),
            Step::Last => return order.last().cloned(),
            Step::Parent => {
                return self
                    .find(id)?
                    .after
                    .iter()
                    .find(|parent| self.find(parent).is_some())
                    .cloned();
            }
            Step::Child => {
                return order
                    .iter()
                    .find(|candidate| {
                        self.find(candidate)
                            .is_some_and(|node| node.after.iter().any(|parent| parent == id))
                    })
                    .cloned();
            }
            Step::Previous | Step::Next => {}
        }
        let depth = self.find(id)?.depth;
        let component = self
            .components
            .iter()
            .position(|part| part.nodes.iter().any(|node| node.id == id))?;
        let previous = step == Step::Previous;
        let candidates = if previous {
            &order[..at]
        } else {
            &order[at + 1..]
        };
        let mut sibling = None;
        let mut other_component = None;
        let mut look = |candidate: &String| {
            let Some((index, node)) =
                self.components
                    .iter()
                    .enumerate()
                    .find_map(|(index, part)| {
                        part.nodes
                            .iter()
                            .find(|node| node.id == *candidate)
                            .map(|node| (index, node))
                    })
            else {
                return false;
            };
            if index == component {
                if node.depth == depth {
                    sibling = Some(candidate.clone());
                    return true;
                }
            } else if other_component.is_none() {
                other_component = Some(candidate.clone());
            }
            false
        };
        if previous {
            for candidate in candidates.iter().rev() {
                if look(candidate) {
                    break;
                }
            }
        } else {
            for candidate in candidates {
                if look(candidate) {
                    break;
                }
            }
        }
        sibling.or(other_component)
    }
}

/// A direction a selection key can move in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Parent,
    Child,
    Previous,
    Next,
    First,
    Last,
}

/// Where a selectable node's label sits in a rendered graph, in graph-relative cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRect {
    pub id: String,
    pub x: usize,
    pub y: usize,
    pub width: usize,
}

/// Text and the click targets that produced it, both relative to the graph's content area.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedGraph {
    pub lines: Vec<GraphLine>,
    pub nodes: Vec<NodeRect>,
}

impl Deref for RenderedGraph {
    type Target = [GraphLine];

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

/// Maps a cell in the graph's content area to the job drawn there. Stubs have no rectangle, so
/// they are never selectable.
pub fn hit_test(graph: &RenderedGraph, x: usize, y: usize) -> Option<&str> {
    graph
        .nodes
        .iter()
        .find(|node| y == node.y && x >= node.x && x < node.x.saturating_add(node.width))
        .map(|node| node.id.as_str())
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
}

/// The cut-off counts formatted for a panel title, for example `+3 jobs, 1 component`.
pub fn cutoff_text(graph: &Graph) -> Option<String> {
    let mut parts = Vec::new();
    if graph.hidden_jobs > 0 {
        parts.push(format!(
            "{} {}",
            graph.hidden_jobs,
            noun(graph.hidden_jobs, "job")
        ));
    }
    if graph.hidden_components > 0 {
        parts.push(format!(
            "{} {}",
            graph.hidden_components,
            noun(graph.hidden_components, "component")
        ));
    }
    (!parts.is_empty()).then(|| format!("+{}", parts.join(", ")))
}

fn noun(count: usize, singular: &str) -> String {
    if count == 1 {
        singular.to_owned()
    } else {
        format!("{singular}s")
    }
}

/// Lays out each component horizontally, newest first, and clips to `width`.
/// `titles` keeps the clipped task titles used by `lu graph`; the dashboard omits them.
pub fn render(graph: &Graph, width: usize, titles: bool) -> RenderedGraph {
    let width = width.max(20);
    // Each row keeps its own click targets until all rows are known, so a packed
    // row can shift the nodes that follow it without losing their positions.
    let mut rows: Vec<(GraphLine, Vec<NodeRect>)> = Vec::new();
    let mut packed: Option<(GraphLine, Vec<NodeRect>)> = None;
    for component in &graph.components {
        let rendered = render_component(component, width, titles);
        if is_single(component) && rendered.lines.len() == 1 {
            let line = &rendered.lines[0];
            let line_width = line.text().chars().count();
            if let Some((row, nodes)) = packed.as_mut() {
                if row.text().chars().count() + 2 + line_width <= width {
                    let shift = row.text().chars().count() + 2;
                    row.segments.push(Segment {
                        text: "  ".to_owned(),
                        tone: Tone::Plain,
                    });
                    row.segments.extend(line.segments.iter().cloned());
                    nodes.extend(rendered.nodes.iter().map(|node| NodeRect {
                        x: shift + node.x,
                        ..node.clone()
                    }));
                    continue;
                }
            }
            if let Some(row) = packed.take() {
                rows.push(row);
            }
            packed = Some((line.clone(), rendered.nodes));
        } else {
            if let Some(row) = packed.take() {
                rows.push(row);
            }
            for (index, line) in rendered.lines.into_iter().enumerate() {
                let nodes = rendered
                    .nodes
                    .iter()
                    .filter(|node| node.y == index)
                    .map(|node| NodeRect {
                        y: 0,
                        ..node.clone()
                    })
                    .collect();
                rows.push((line, nodes));
            }
        }
    }
    if let Some(row) = packed {
        rows.push(row);
    }

    let mut result = RenderedGraph::default();
    for (y, (line, nodes)) in rows.into_iter().enumerate() {
        result.lines.push(line);
        result
            .nodes
            .extend(nodes.into_iter().map(|node| NodeRect { y, ..node }));
    }
    if titles {
        if let Some(summary) = summary(graph) {
            result.lines.push(GraphLine {
                segments: vec![Segment {
                    text: summary,
                    tone: Tone::Dim,
                }],
            });
        }
    }
    result
}

fn is_single(component: &Component) -> bool {
    component.nodes.len() == 1 && component.stubs.is_empty() && component.nodes[0].after.is_empty()
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
    /// Loaded dependencies plus the ids of stubs naming this job.
    parents: Vec<String>,
}

struct Placed {
    row: usize,
    column: usize,
    label: String,
    tone: Tone,
    id: String,
    parents: Vec<String>,
}

fn render_component(component: &Component, width: usize, titles: bool) -> RenderedGraph {
    let base = usize::from(!component.stubs.is_empty());
    let mut draw: Vec<DrawNode> = component
        .nodes
        .iter()
        .enumerate()
        .map(|(order, node)| {
            let mut parents = node.after.clone();
            for stub in &component.stubs {
                if stub.to.iter().any(|to| to == &node.id) && !parents.contains(&stub.id) {
                    parents.push(stub.id.clone());
                }
            }
            DrawNode {
                id: node.id.clone(),
                label: label(node.state, &node.id, &node.title, titles),
                tone: node.state.tone(),
                column: base + node.depth,
                y: 0.0,
                order,
                parents,
            }
        })
        .collect();
    let by_id: HashMap<String, usize> = draw
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect();

    let stub_start = draw.len();
    for (stub_index, stub) in component.stubs.iter().enumerate() {
        draw.push(DrawNode {
            id: stub.id.clone(),
            label: format!("#{}…", short_id(&stub.id)),
            tone: Tone::Dim,
            column: 0,
            y: 0.0,
            order: stub_start + stub_index,
            parents: Vec::new(),
        });
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); draw.len()];
    for (index, node) in draw.iter().enumerate() {
        for parent in &node.parents {
            if let Some(&source) = by_id.get(parent) {
                children[source].push(index);
            }
        }
    }

    let max_depth = component
        .nodes
        .iter()
        .map(|node| node.depth)
        .max()
        .unwrap_or(0);
    let mut next_y = 0.0;
    for depth in 0..=max_depth {
        for (index, node) in component.nodes.iter().enumerate() {
            if node.depth != depth {
                continue;
            }
            let loaded: Vec<usize> = node
                .after
                .iter()
                .filter_map(|dependency| by_id.get(dependency).copied())
                .filter(|&source| component.nodes[source].depth < depth)
                .collect();
            draw[index].y = if loaded.is_empty() {
                let y = next_y;
                next_y += 1.0;
                y
            } else {
                loaded.iter().map(|&source| draw[source].y).sum::<f64>() / loaded.len() as f64
            };
        }
        next_y = next_y.max(
            draw.iter()
                .filter(|node| node.column == base + depth)
                .map(|node| node.y + 1.0)
                .fold(0.0, f64::max),
        );
    }

    for (stub_index, stub) in component.stubs.iter().enumerate() {
        let targets: Vec<f64> = stub
            .to
            .iter()
            .filter_map(|to| by_id.get(to))
            .map(|&index| draw[index].y)
            .collect();
        draw[stub_start + stub_index].y = if targets.is_empty() {
            0.0
        } else {
            targets.iter().sum::<f64>() / targets.len() as f64
        };
    }

    // A chain shares its parent's row; a real branch gives each child its own row.
    for (parent, kids) in children.iter().enumerate() {
        if kids.len() < 2 {
            continue;
        }
        for (index, &child) in kids.iter().enumerate() {
            let incoming = children
                .iter()
                .filter(|parents| parents.contains(&child))
                .count();
            if incoming == 1 {
                draw[child].y = draw[parent].y + 0.5 + index as f64;
            }
        }
    }

    let mut order: Vec<usize> = (0..draw.len()).collect();
    order.sort_by(|&left, &right| {
        let a = &draw[left];
        let b = &draw[right];
        a.y.partial_cmp(&b.y)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.order.cmp(&b.order))
    });
    let mut row_of = vec![0usize; draw.len()];
    let mut rows = 0usize;
    let mut last: Option<f64> = None;
    for &index in &order {
        if last != Some(draw[index].y) {
            rows += 1;
            last = Some(draw[index].y);
        }
        row_of[index] = rows - 1;
    }

    let placed: Vec<Placed> = draw
        .iter()
        .enumerate()
        .map(|(index, node)| Placed {
            row: row_of[index],
            column: node.column,
            label: node.label.clone(),
            tone: node.tone,
            id: node.id.clone(),
            parents: node.parents.clone(),
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

    let mut canvas = Canvas {
        glyphs: vec![vec![None; line_width]; rows],
        directions: vec![vec![0u8; line_width]; rows],
        tones: vec![vec![None; line_width]; rows],
    };
    for node in &placed {
        let start = starts[node.column];
        for (offset, character) in node.label.chars().enumerate() {
            if let Some(cell) = canvas.glyphs[node.row].get_mut(start + offset) {
                *cell = Some((character, node.tone));
            }
        }
    }

    for node in &placed {
        for parent in &node.parents {
            let Some(&source) = by_id.get(parent.as_str()) else {
                continue;
            };
            let source = &placed[source];
            if node.column <= source.column {
                continue;
            }
            for gap in source.column..node.column {
                draw_edge(&mut canvas, source, node, gap, &starts, &widths);
            }
        }
    }

    let nodes = order
        .iter()
        .filter(|&&index| index < stub_start)
        .map(|&index| {
            let node = &placed[index];
            NodeRect {
                id: node.id.clone(),
                x: starts[node.column],
                y: node.row,
                width: node.label.chars().count(),
            }
        })
        .collect();
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let cells: Vec<Option<(char, Tone)>> = (0..line_width)
            .map(|x| match canvas.glyphs[row][x] {
                Some(cell) => Some(cell),
                None if canvas.directions[row][x] != 0 => Some((
                    direction_char(canvas.directions[row][x]),
                    canvas.tones[row][x].unwrap_or(Tone::Plain),
                )),
                None => None,
            })
            .collect();
        let end = cells
            .iter()
            .rposition(|cell| cell.is_some())
            .map_or(0, |index| index + 1);
        if end == 0 {
            continue;
        }
        lines.push(GraphLine {
            segments: segments(cells[..end].to_vec()),
        });
    }
    RenderedGraph { lines, nodes }
}

struct Canvas {
    glyphs: Vec<Vec<Option<(char, Tone)>>>,
    directions: Vec<Vec<u8>>,
    tones: Vec<Vec<Option<Tone>>>,
}

/// Connects two columns within one gap: a trunk at `+2` carries the vertical
/// run, the arrow at `-2` leaves one blank cell before the next node.
fn draw_edge(
    canvas: &mut Canvas,
    source: &Placed,
    target: &Placed,
    gap: usize,
    starts: &[usize],
    widths: &[usize],
) {
    let gap_start = starts[gap] + widths[gap];
    let next = starts[gap + 1];
    let trunk = gap_start + 2;
    let arrow = next.saturating_sub(2);
    let tone = source.tone;
    let horizontal = DIR_LEFT | DIR_RIGHT;

    if source.row == target.row && gap + 1 == target.column {
        for x in (gap_start + 1)..arrow {
            add_direction(canvas, source.row, x, horizontal, tone);
        }
        if let Some(cell) = canvas.glyphs[source.row].get_mut(arrow) {
            *cell = Some(('▶', tone));
        }
        return;
    }

    for x in (gap_start + 1)..trunk {
        add_direction(canvas, source.row, x, horizontal, tone);
    }
    add_direction(canvas, source.row, trunk, DIR_LEFT, tone);
    let low = source.row.min(target.row);
    let high = source.row.max(target.row);
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
        if row == target.row {
            bits |= DIR_RIGHT;
        }
        add_direction(canvas, row, trunk, bits, tone);
    }
    if gap + 1 == target.column {
        for x in (trunk + 1)..arrow {
            add_direction(canvas, target.row, x, horizontal, tone);
        }
        if let Some(cell) = canvas.glyphs[target.row].get_mut(arrow) {
            *cell = Some(('▶', tone));
        }
    } else {
        for x in (trunk + 1)..next {
            add_direction(canvas, target.row, x, horizontal, tone);
        }
    }
}

const DIR_UP: u8 = 1;
const DIR_RIGHT: u8 = 2;
const DIR_DOWN: u8 = 4;
const DIR_LEFT: u8 = 8;

fn add_direction(canvas: &mut Canvas, row: usize, x: usize, bits: u8, tone: Tone) {
    let Some(cell) = canvas
        .directions
        .get_mut(row)
        .and_then(|row| row.get_mut(x))
    else {
        return;
    };
    *cell |= bits;
    if let Some(slot) = canvas.tones.get_mut(row).and_then(|row| row.get_mut(x)) {
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

fn label(state: State, id: &str, title: &str, titles: bool) -> String {
    let node = format!("{} {}", state.glyph(), short_id(id));
    if titles {
        format!("{node} {}", clip(title, TITLE_CHARS))
    } else {
        node
    }
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

    fn text(lines: &[GraphLine]) -> String {
        lines
            .iter()
            .map(GraphLine::text)
            .collect::<Vec<_>>()
            .join("\n")
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
    fn a_chain_is_drawn_on_one_row_parent_first() {
        let graph = Graph::build(&[
            job("CCCC", State::Blocked, &["BBBB"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        let lines = render(&graph, 80, false);
        assert_eq!(lines.len(), 1, "{:?}", text(&lines));
        assert_eq!(text(&lines), "✓ AAAA ───▶ ○ BBBB ───▶ ◌ CCCC");
    }

    #[test]
    fn a_fan_out_stacks_children_to_the_right() {
        let graph = Graph::build(&[
            job("BBBB", State::Queued, &["AAAA"]),
            job("CCCC", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        let lines = render(&graph, 80, false);
        let text = text(&lines);
        assert_eq!(lines.len(), 3, "{text}");
        assert!(text.contains("✓ AAAA ─┐"), "{text}");
        assert!(text.contains("├─▶ ○ BBBB"), "{text}");
        assert!(text.contains("└─▶ ○ CCCC"), "{text}");
        assert!(!text.contains('│'), "{text}");
    }

    #[test]
    fn a_fan_in_keeps_the_join_between_its_parents() {
        let graph = Graph::build(&[
            job("CCCC", State::Blocked, &["AAAA", "BBBB"]),
            job("BBBB", State::Queued, &[]),
            job("AAAA", State::Running, &[]),
        ]);
        let lines = render(&graph, 80, false);
        let text = text(&lines);
        assert_eq!(lines.len(), 3, "{text}");
        assert!(text.contains("○ BBBB ─┐"), "{text}");
        assert!(text.contains("├─▶ ◌ CCCC"), "{text}");
        assert!(text.contains("▶ AAAA ─┘"), "{text}");
    }

    #[test]
    fn an_unknown_dependency_is_a_stub_at_the_left_of_its_chain() {
        let graph = Graph::build(&[job("AAAA", State::Running, &["MISSING"])]);
        let component = &graph.components[0];
        assert_eq!(component.stubs.len(), 1);
        assert_eq!(component.stubs[0].id, "MISSING");
        assert_eq!(component.nodes[0].depth, 0);
        let lines = render(&graph, 80, false);
        assert_eq!(lines.len(), 1, "{:?}", text(&lines));
        assert_eq!(text(&lines), "#SSING… ───▶ ▶ AAAA");
        assert_eq!(lines[0].segments[0].tone, Tone::Dim);
    }

    #[test]
    fn independent_nodes_share_a_row_when_they_fit() {
        let graph = Graph::build(&[
            job("AAAA", State::Queued, &[]),
            job("BBBB", State::Queued, &[]),
            job("CCCC", State::Queued, &[]),
        ]);
        let lines = render(&graph, 20, false);
        assert_eq!(text(&lines), "○ AAAA  ○ BBBB\n○ CCCC");
        let narrow = render(&graph, 20, false);
        assert_eq!(text(&narrow), text(&lines));
    }

    #[test]
    fn titles_are_only_drawn_when_asked_for() {
        let graph = Graph::build(&[
            job("BBBB", State::Done, &[]),
            job("AAAA", State::Queued, &["BBBB"]),
        ]);
        let plain = text(&render(&graph, 100, false));
        assert!(plain.contains("✓ BBBB ───▶ ○ AAAA"), "{plain}");
        assert!(!plain.contains("task "), "{plain}");
        let titled = text(&render(&graph, 100, true));
        assert!(
            titled.contains("✓ BBBB task BBBB ───▶ ○ AAAA task AAAA"),
            "{titled}"
        );
    }

    #[test]
    fn narrow_widths_clip_without_panicking() {
        let graph = Graph::build(&[
            job("CCCC", State::Blocked, &["BBBB"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
            job("EEEE", State::Running, &["DDDD"]),
            job("DDDD", State::Done, &[]),
        ]);
        for width in [0, 20, 21, 30] {
            let lines = render(&graph, width, true);
            assert!(lines
                .iter()
                .all(|line| line.text().chars().count() <= 20.max(width)));
        }
        let long = Graph::build(&[Job {
            id: "AAAA".to_owned(),
            state: State::Queued,
            title: "x".repeat(80),
            after: Vec::new(),
        }]);
        let short = text(&render(&long, 20, true));
        assert!(short.chars().count() <= 20, "{short}");
        let titled = text(&render(&long, 50, true));
        assert!(
            titled.chars().count() <= 50 && titled.contains('…'),
            "{titled}"
        );
    }

    #[test]
    fn cutoff_text_counts_jobs_and_components() {
        let jobs: Vec<Job> = (0..8)
            .map(|index| Job {
                id: format!("J{index}"),
                state: State::Running,
                title: "task".to_owned(),
                after: Vec::new(),
            })
            .collect();
        let graph = Graph::build(&jobs);
        assert_eq!(
            cutoff_text(&graph).as_deref(),
            Some("+2 jobs, 2 components")
        );
        let text = text(&render(&graph, 100, true));
        assert!(
            text.contains("2 more jobs and 2 more components not shown"),
            "{text}"
        );
        let jobs: Vec<Job> = (0..7)
            .map(|index| Job {
                id: format!("J{index}"),
                state: State::Running,
                title: "task".to_owned(),
                after: Vec::new(),
            })
            .collect();
        assert_eq!(
            cutoff_text(&Graph::build(&jobs)).as_deref(),
            Some("+1 job, 1 component")
        );
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
        let titled = text(&render(&graph, 100, true));
        assert!(
            titled.contains("2 more jobs and 2 more components not shown"),
            "{titled}"
        );
        assert!(!text(&render(&graph, 100, false)).contains("not shown"));
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
        assert_eq!(cutoff_text(&graph).as_deref(), Some("+3 jobs"));
    }

    #[test]
    fn the_example_lays_out_two_dependencies_into_one_job() {
        let graph = Graph::build(&[
            job("01KE9AR", State::Done, &[]),
            job("01Z7W0A", State::Running, &[]),
            job("01H6CQM", State::Blocked, &["01KE9AR", "01Z7W0A"]),
        ]);
        let plain = text(&render(&graph, 100, false));
        assert!(plain.contains("✓ KE9AR"), "{plain}");
        assert!(plain.contains("▶ Z7W0A"), "{plain}");
        assert!(plain.contains("◌ H6CQM"), "{plain}");
        assert!(plain.contains('┐'), "{plain}");
        assert!(plain.contains("├─▶"), "{plain}");
        assert!(plain.contains('┘'), "{plain}");
        assert!(!plain.contains("task 01"), "{plain}");
        let titled = text(&render(&graph, 100, true));
        assert!(titled.contains("✓ KE9AR task 01KE9AR"), "{titled}");
        assert!(titled.contains("▶ Z7W0A task 01Z7W0A"), "{titled}");
        assert!(titled.contains("◌ H6CQM task 01H6CQM"), "{titled}");
    }

    #[test]
    fn hit_test_finds_every_node_and_ignores_edges_and_empty_cells() {
        let graph = Graph::build(&[
            job("CCCC", State::Queued, &["AAAA"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        let rendered = render(&graph, 80, false);
        assert_eq!(rendered.nodes.len(), 3);
        for node in &rendered.nodes {
            assert_eq!(hit_test(&rendered, node.x, node.y), Some(node.id.as_str()));
            assert_eq!(
                hit_test(&rendered, node.x + node.width - 1, node.y),
                Some(node.id.as_str())
            );
        }
        let parent = &rendered.nodes[0];
        assert_eq!(hit_test(&rendered, parent.x + parent.width, parent.y), None);
        assert_eq!(hit_test(&rendered, 0, rendered.lines.len() + 1), None);
        assert_eq!(hit_test(&rendered, usize::MAX, 0), None);
        assert_eq!(hit_test(&rendered, 0, usize::MAX), None);
    }

    #[test]
    fn hit_test_uses_the_positions_of_packed_single_nodes() {
        let graph = Graph::build(&[
            job("AAAA", State::Queued, &[]),
            job("BBBB", State::Queued, &[]),
            job("CCCC", State::Queued, &[]),
        ]);
        let rendered = render(&graph, 20, false);
        assert_eq!(text(&rendered), "○ AAAA  ○ BBBB\n○ CCCC");
        let ids: Vec<&str> = rendered.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["AAAA", "BBBB", "CCCC"]);
        assert_eq!(hit_test(&rendered, 8, 0), Some("BBBB"));
        assert_eq!(hit_test(&rendered, 2, 1), Some("CCCC"));
        assert_eq!(hit_test(&rendered, 7, 0), None);
    }

    #[test]
    fn stubs_are_not_hit_testable_or_selectable() {
        let graph = Graph::build(&[job("AAAA", State::Running, &["MISSING"])]);
        let rendered = render(&graph, 80, false);
        assert_eq!(rendered.nodes.len(), 1);
        assert_eq!(rendered.nodes[0].id, "AAAA");
        assert_eq!(hit_test(&rendered, 0, 0), None);
        assert_eq!(graph.step("MISSING", Step::Child), None);
        assert_eq!(graph.step("AAAA", Step::Parent), None);
    }

    #[test]
    fn navigation_follows_a_chain_and_stays_put_at_its_ends() {
        let graph = Graph::build(&[
            job("CCCC", State::Blocked, &["BBBB"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        assert_eq!(graph.step("CCCC", Step::Parent).as_deref(), Some("BBBB"));
        assert_eq!(graph.step("BBBB", Step::Child).as_deref(), Some("CCCC"));
        assert_eq!(graph.step("BBBB", Step::Previous), None);
        assert_eq!(graph.step("BBBB", Step::Next), None);
        assert_eq!(graph.step("BBBB", Step::First).as_deref(), Some("AAAA"));
        assert_eq!(graph.step("BBBB", Step::Last).as_deref(), Some("CCCC"));
        assert_eq!(graph.step("AAAA", Step::Parent), None);
        assert_eq!(graph.step("CCCC", Step::Child), None);
    }

    #[test]
    fn navigation_moves_between_siblings_then_components() {
        let graph = Graph::build(&[
            job("BBBB", State::Queued, &["AAAA"]),
            job("CCCC", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
            job("DDDD", State::Running, &[]),
        ]);
        assert_eq!(graph.step("BBBB", Step::Next).as_deref(), Some("CCCC"));
        assert_eq!(graph.step("CCCC", Step::Previous).as_deref(), Some("BBBB"));
        assert_eq!(graph.step("CCCC", Step::Next).as_deref(), Some("DDDD"));
        assert_eq!(graph.step("DDDD", Step::Previous).as_deref(), Some("CCCC"));
        assert_eq!(graph.step("BBBB", Step::Previous), None);
    }

    #[test]
    fn navigation_uses_the_first_parent_and_first_child_in_draw_order() {
        let parents = Graph::build(&[
            job("DDDD", State::Blocked, &["BBBB", "AAAA"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        assert_eq!(parents.step("DDDD", Step::Parent).as_deref(), Some("BBBB"));
        let children = Graph::build(&[
            job("CCCC", State::Queued, &["AAAA"]),
            job("BBBB", State::Queued, &["AAAA"]),
            job("AAAA", State::Done, &[]),
        ]);
        assert_eq!(children.step("AAAA", Step::Child).as_deref(), Some("CCCC"));
    }
}
