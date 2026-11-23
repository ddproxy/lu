use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::graph::{self, Step};
use crate::model::{Dashboard, Focus, Request, Snapshot, Update, ViewModel};
use crate::render::{self, LayoutInfo};
use crate::terminal::TerminalGuard;

/// Longest a key press waits before the screen is redrawn anyway.
const POLL: Duration = Duration::from_millis(100);
/// Crossterm reports clicks, not double clicks.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    TogglePause,
    MoveUp,
    MoveDown,
    ToggleFocus,
    ToggleFocusBack,
    MoveParent,
    MoveChild,
    Home,
    End,
    OpenLog,
    Escape,
    Drain,
    ToggleGraph,
    ToggleProcess,
}

pub fn action_for(event: KeyEvent) -> Option<Action> {
    // Some terminals also report key releases; acting on them would double every press.
    if event.kind != KeyEventKind::Press {
        return None;
    }
    match (event.code, event.modifiers) {
        (KeyCode::Char('q'), _) => Some(Action::Quit),
        (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::Quit)
        }
        (KeyCode::Esc, _) => Some(Action::Escape),
        (KeyCode::Char('p' | ' '), _) => Some(Action::TogglePause),
        (KeyCode::Up | KeyCode::Char('k'), _) => Some(Action::MoveUp),
        (KeyCode::Down | KeyCode::Char('j'), _) => Some(Action::MoveDown),
        (KeyCode::Left | KeyCode::Char('h'), _) => Some(Action::MoveParent),
        (KeyCode::Right | KeyCode::Char('l'), _) => Some(Action::MoveChild),
        (KeyCode::Home, _) => Some(Action::Home),
        (KeyCode::End, _) => Some(Action::End),
        (KeyCode::BackTab, _) => Some(Action::ToggleFocusBack),
        (KeyCode::Tab, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
            Some(Action::ToggleFocusBack)
        }
        (KeyCode::Tab, _) => Some(Action::ToggleFocus),
        (KeyCode::Enter, _) => Some(Action::OpenLog),
        (KeyCode::Char('d'), _) => Some(Action::Drain),
        (KeyCode::Char('g'), _) => Some(Action::ToggleGraph),
        (KeyCode::Char('x'), _) => Some(Action::ToggleProcess),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickTarget {
    Node(usize),
    JobRow(usize),
    GraphJob(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    SelectNode(usize),
    SelectJob(usize),
    SelectGraphJob(String),
    OpenJob(String, Focus),
    ScrollGraph(bool),
    ScrollTable(bool),
}

/// Remembers the last click so a second press on the same target can open its log.
#[derive(Default)]
struct MouseState {
    last: Option<(ClickTarget, Instant)>,
}

fn inside(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn target_at(x: u16, y: u16, layout: &LayoutInfo, view: &ViewModel) -> Option<ClickTarget> {
    if inside(layout.graph, x, y) {
        let local_x = usize::from(x - layout.graph.x);
        let local_y = usize::from(y - layout.graph.y);
        return graph::hit_test(&layout.rendered_graph, local_x, local_y)
            .map(|id| ClickTarget::GraphJob(id.to_owned()));
    }
    if inside(layout.nodes, x, y) {
        let row = usize::from(y - layout.nodes.y);
        return row
            .checked_sub(1)
            .filter(|&index| index < view.node_names.len())
            .map(ClickTarget::Node);
    }
    if inside(layout.jobs, x, y) {
        let row = usize::from(y - layout.jobs.y);
        return row
            .checked_sub(2)
            .filter(|&index| index < view.job_ids.len())
            .map(ClickTarget::JobRow);
    }
    None
}

impl MouseState {
    fn handle(
        &mut self,
        event: MouseEvent,
        layout: &LayoutInfo,
        view: &ViewModel,
        now: Instant,
    ) -> Option<MouseAction> {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Modified clicks belong to the terminal's own copy/paste bindings.
                if event
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    return None;
                }
                let target = target_at(event.column, event.row, layout, view)?;
                let repeated = self.last.as_ref().is_some_and(|(previous, at)| {
                    *previous == target && now.saturating_duration_since(*at) <= DOUBLE_CLICK
                });
                self.last = Some((target.clone(), now));
                match target {
                    ClickTarget::Node(index) => Some(MouseAction::SelectNode(index)),
                    ClickTarget::JobRow(index) => {
                        if repeated {
                            view.job_ids
                                .get(index)
                                .cloned()
                                .map(|id| MouseAction::OpenJob(id, Focus::Jobs))
                        } else {
                            Some(MouseAction::SelectJob(index))
                        }
                    }
                    ClickTarget::GraphJob(id) => {
                        if repeated {
                            Some(MouseAction::OpenJob(id, Focus::Graph))
                        } else {
                            Some(MouseAction::SelectGraphJob(id))
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = matches!(event.kind, MouseEventKind::ScrollDown);
                if inside(layout.graph, event.column, event.row) {
                    Some(MouseAction::ScrollGraph(down))
                } else if inside(layout.nodes, event.column, event.row)
                    || inside(layout.jobs, event.column, event.row)
                {
                    Some(MouseAction::ScrollTable(down))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Runs the dashboard until the user quits or `stop` is set, applying each refresh from `feed`.
/// Log and drain results arrive on `updates`; `request` hands work to the feeder without blocking.
/// The caller sets `stop` from a signal handler; the guard restores the terminal either way.
pub fn run_live(
    feed: &Receiver<Result<Snapshot, String>>,
    updates: &Receiver<Update>,
    request: &dyn Fn(Request),
    stop: &AtomicBool,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut dashboard = Dashboard::new();
    let mut layout = LayoutInfo::default();
    let mut mouse = MouseState::default();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(result) = feed.try_recv() {
            dashboard.apply(result, Instant::now());
        }
        while let Ok(update) = updates.try_recv() {
            dashboard.apply_update(update);
        }
        let width = terminal.size().map_or(200, |size| size.width);
        let view = dashboard.view_with_width(Instant::now(), width);
        terminal.draw(|frame| layout = render::draw(frame, &view))?;
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) => match action_for(key) {
                    Some(Action::Quit) => break,
                    Some(Action::Escape) => {
                        if dashboard.close_process_pane() {
                            request(Request::ProcessPane { target: None });
                        } else if !dashboard.close_log() {
                            break;
                        }
                    }
                    Some(Action::TogglePause) => dashboard.toggle_pause(Instant::now()),
                    Some(Action::MoveUp) => dashboard.move_selection(false),
                    Some(Action::MoveDown) => dashboard.move_selection(true),
                    Some(Action::MoveParent) => dashboard.move_graph(Step::Parent),
                    Some(Action::MoveChild) => dashboard.move_graph(Step::Child),
                    Some(Action::Home) => dashboard.move_graph(Step::First),
                    Some(Action::End) => dashboard.move_graph(Step::Last),
                    Some(Action::ToggleFocus) => dashboard.toggle_focus(),
                    Some(Action::ToggleFocusBack) => dashboard.toggle_focus_back(),
                    Some(Action::OpenLog) => {
                        if let Some(job_id) = dashboard.open_selected_log() {
                            request(Request::Log { job_id });
                        }
                    }
                    Some(Action::Drain) => {
                        if let Some(drain) = dashboard.request_drain(Instant::now()) {
                            request(Request::Drain {
                                node: drain.node,
                                drain: drain.drain,
                            });
                        }
                    }
                    Some(Action::ToggleGraph) => dashboard.toggle_graph(),
                    Some(Action::ToggleProcess) => {
                        if let Some(target) = dashboard.toggle_process_pane() {
                            request(Request::ProcessPane { target });
                        }
                    }
                    None => {}
                },
                Event::Mouse(mouse_event) => {
                    if let Some(action) = mouse.handle(mouse_event, &layout, &view, Instant::now())
                    {
                        match action {
                            MouseAction::SelectNode(index) => {
                                dashboard.select_node_at(index);
                            }
                            MouseAction::SelectJob(index) => {
                                dashboard.select_job_at(index, Focus::Jobs);
                            }
                            MouseAction::SelectGraphJob(id) => {
                                dashboard.select_job_id(&id, Focus::Graph);
                            }
                            MouseAction::OpenJob(id, focus) => {
                                if let Some(job_id) = dashboard.open_job_id(&id, focus) {
                                    request(Request::Log { job_id });
                                }
                            }
                            MouseAction::ScrollGraph(down) => dashboard.scroll_graph(down),
                            MouseAction::ScrollTable(down) => dashboard.scroll_table(down),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::model::{JobRow, NodeRow, Snapshot};

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn the_keys_map_to_their_actions() {
        let none = KeyModifiers::NONE;
        let cases = [
            (KeyCode::Char('q'), none, Some(Action::Quit)),
            (KeyCode::Esc, none, Some(Action::Escape)),
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                Some(Action::Quit),
            ),
            (KeyCode::Char('p'), none, Some(Action::TogglePause)),
            (KeyCode::Char(' '), none, Some(Action::TogglePause)),
            (KeyCode::Up, none, Some(Action::MoveUp)),
            (KeyCode::Char('k'), none, Some(Action::MoveUp)),
            (KeyCode::Down, none, Some(Action::MoveDown)),
            (KeyCode::Char('j'), none, Some(Action::MoveDown)),
            (KeyCode::Left, none, Some(Action::MoveParent)),
            (KeyCode::Char('h'), none, Some(Action::MoveParent)),
            (KeyCode::Right, none, Some(Action::MoveChild)),
            (KeyCode::Char('l'), none, Some(Action::MoveChild)),
            (KeyCode::Home, none, Some(Action::Home)),
            (KeyCode::End, none, Some(Action::End)),
            (KeyCode::Tab, none, Some(Action::ToggleFocus)),
            (
                KeyCode::Tab,
                KeyModifiers::SHIFT,
                Some(Action::ToggleFocusBack),
            ),
            (KeyCode::BackTab, none, Some(Action::ToggleFocusBack)),
            (KeyCode::Enter, none, Some(Action::OpenLog)),
            (KeyCode::Char('d'), none, Some(Action::Drain)),
            (KeyCode::Char('g'), none, Some(Action::ToggleGraph)),
            (KeyCode::Char('x'), none, Some(Action::ToggleProcess)),
            (KeyCode::Char('z'), none, None),
            (KeyCode::Char('c'), none, None),
        ];
        for (code, modifiers, expected) in cases {
            assert_eq!(
                action_for(press(code, modifiers)),
                expected,
                "{code:?} with {modifiers:?}"
            );
        }
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut release = press(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for(release), None);
    }

    fn dashboard() -> Dashboard {
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: vec![NodeRow {
                    name: "peer-a".to_owned(),
                    harness: "idle".to_owned(),
                    drained: false,
                    seen_secs_ago: Some(2),
                    providers: Vec::new(),
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
                }],
                jobs: vec![
                    JobRow {
                        id: "01PARENT".to_owned(),
                        title: "index".to_owned(),
                        state: "done".to_owned(),
                        node: None,
                        repo: "owner/name".to_owned(),
                        git_ref: "main".to_owned(),
                        attempt: 1,
                        age_secs: 1,
                        model: None,
                        note: String::new(),
                        after: Vec::new(),
                    },
                    JobRow {
                        id: "01CHILD".to_owned(),
                        title: "merge".to_owned(),
                        state: "running".to_owned(),
                        node: None,
                        repo: "owner/name".to_owned(),
                        git_ref: "main".to_owned(),
                        attempt: 1,
                        age_secs: 1,
                        model: None,
                        note: String::new(),
                        after: vec!["01PARENT".to_owned()],
                    },
                ],
            }),
            Instant::now(),
        );
        dashboard
    }

    fn view_and_layout(dashboard: &Dashboard) -> (ViewModel, LayoutInfo) {
        let view = dashboard.view(Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut layout = LayoutInfo::default();
        terminal
            .draw(|frame| layout = render::draw(frame, &view))
            .unwrap();
        (view, layout)
    }

    fn click(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_a_graph_node_selects_it_and_a_second_click_opens_its_log() {
        let dashboard = dashboard();
        let (view, layout) = view_and_layout(&dashboard);
        let node = layout.rendered_graph.nodes[0].clone();
        let x = layout.graph.x + node.x as u16;
        let y = layout.graph.y + node.y as u16;
        let now = Instant::now();
        let mut state = MouseState::default();
        assert_eq!(
            state.handle(
                click(MouseEventKind::Down(MouseButton::Left), x, y),
                &layout,
                &view,
                now
            ),
            Some(MouseAction::SelectGraphJob(node.id.clone()))
        );
        assert_eq!(
            state.handle(
                click(MouseEventKind::Down(MouseButton::Left), x, y),
                &layout,
                &view,
                now + Duration::from_millis(100)
            ),
            Some(MouseAction::OpenJob(node.id, Focus::Graph))
        );
    }

    #[test]
    fn clicking_table_rows_selects_and_double_clicking_a_job_opens_its_log() {
        let dashboard = dashboard();
        let (view, layout) = view_and_layout(&dashboard);
        let now = Instant::now();
        let mut state = MouseState::default();
        let node_click = click(
            MouseEventKind::Down(MouseButton::Left),
            layout.nodes.x + 1,
            layout.nodes.y + 1,
        );
        assert_eq!(
            state.handle(node_click, &layout, &view, now),
            Some(MouseAction::SelectNode(0))
        );
        let x = layout.jobs.x + 1;
        let y = layout.jobs.y + 2;
        assert_eq!(
            state.handle(
                click(MouseEventKind::Down(MouseButton::Left), x, y),
                &layout,
                &view,
                now
            ),
            Some(MouseAction::SelectJob(0))
        );
        assert_eq!(
            state.handle(
                click(MouseEventKind::Down(MouseButton::Left), x, y),
                &layout,
                &view,
                now + Duration::from_millis(100)
            ),
            Some(MouseAction::OpenJob("01PARENT".to_owned(), Focus::Jobs))
        );
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let dashboard = dashboard();
        let (view, layout) = view_and_layout(&dashboard);
        let mut state = MouseState::default();
        let now = Instant::now();
        let over_graph = click(
            MouseEventKind::ScrollDown,
            layout.graph.x + 1,
            layout.graph.y + 1,
        );
        assert_eq!(
            state.handle(over_graph, &layout, &view, now),
            Some(MouseAction::ScrollGraph(true))
        );
        let up = click(
            MouseEventKind::ScrollUp,
            layout.nodes.x + 1,
            layout.nodes.y + 1,
        );
        assert_eq!(
            state.handle(up, &layout, &view, now),
            Some(MouseAction::ScrollTable(false))
        );
    }

    #[test]
    fn clicks_on_empty_graph_cells_and_modified_clicks_do_nothing() {
        let dashboard = dashboard();
        let (view, layout) = view_and_layout(&dashboard);
        let mut state = MouseState::default();
        let now = Instant::now();
        let empty = click(
            MouseEventKind::Down(MouseButton::Left),
            layout.graph.x + 90,
            layout.graph.y,
        );
        assert_eq!(state.handle(empty, &layout, &view, now), None);
        let mut modified = click(
            MouseEventKind::Down(MouseButton::Left),
            layout.jobs.x + 1,
            layout.jobs.y + 2,
        );
        modified.modifiers = KeyModifiers::CONTROL;
        assert_eq!(state.handle(modified, &layout, &view, now), None);
    }
}
