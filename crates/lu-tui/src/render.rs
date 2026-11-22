use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Sparkline};
use ratatui::Frame;

use crate::graph::{self, NodeRect, RenderedGraph};
use crate::model::{Focus, Tone, ViewModel};

/// Where the mouse sees each pane; coordinates are terminal cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutInfo {
    /// Content area of the Nodes block, with its header on row zero.
    pub nodes: Rect,
    /// Content area of the Jobs block, with a blank row, then its header.
    pub jobs: Rect,
    /// Content area of the Graph block, matching the node rectangles in `rendered_graph`.
    pub graph: Rect,
    pub rendered_graph: RenderedGraph,
}

impl Default for LayoutInfo {
    fn default() -> Self {
        Self {
            nodes: Rect::new(0, 0, 0, 0),
            jobs: Rect::new(0, 0, 0, 0),
            graph: Rect::new(0, 0, 0, 0),
            rendered_graph: RenderedGraph::default(),
        }
    }
}

fn style_for(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::default(),
        Tone::Heading => Style::default().add_modifier(Modifier::BOLD),
        Tone::Good => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
        Tone::Warn => Style::default().fg(Color::Yellow),
        Tone::Bad => Style::default().fg(Color::Red),
        Tone::Dim => Style::default().fg(Color::DarkGray),
        Tone::Done => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::DIM),
        Tone::Blocked => Style::default().fg(Color::Rgb(255, 140, 0)),
    }
}

fn model_line(line: &crate::model::Line, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = if line.segments.is_empty() {
        vec![Span::styled(line.text.clone(), style_for(line.tone))]
    } else {
        line.segments
            .iter()
            .map(|segment| Span::styled(segment.text.clone(), style_for(segment.tone)))
            .collect()
    };
    if selected {
        spans = spans
            .into_iter()
            .map(|span| {
                Span::styled(
                    span.content.into_owned(),
                    span.style.add_modifier(Modifier::REVERSED),
                )
            })
            .collect();
    }
    Line::from(spans)
}

fn pane_line(line: &crate::model::Line) -> Line<'static> {
    if line.segments.is_empty() {
        Line::from(Span::styled(line.text.clone(), style_for(line.tone)))
    } else {
        Line::from(
            line.segments
                .iter()
                .map(|segment| Span::styled(segment.text.clone(), style_for(segment.tone)))
                .collect::<Vec<_>>(),
        )
    }
}

fn draw_process_pane(frame: &mut Frame, pane: &crate::model::ProcessPaneView, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let spark_h = if area.height >= 8 { 3 } else { 0 };
    let [sparks, body] =
        Layout::vertical([Constraint::Length(spark_h), Constraint::Min(0)]).areas(area);
    if spark_h > 0 {
        let [load_area, mem_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(sparks);
        let load_width = load_area.width.saturating_sub(2) as usize;
        let load_skip = pane.load_spark.len().saturating_sub(load_width);
        let load_data: Vec<u64> = pane.load_spark.iter().skip(load_skip).copied().collect();
        let load_title = format!(
            "load {}",
            load_data
                .last()
                .map(|v| format!("{:.2}", *v as f64 / 100.0))
                .unwrap_or_else(|| "-".to_owned())
        );
        frame.render_widget(
            Sparkline::default()
                .block(Block::bordered().title(load_title))
                .data(&load_data)
                .style(Style::default().fg(Color::Cyan)),
            load_area,
        );
        let mem_width = mem_area.width.saturating_sub(2) as usize;
        let mem_skip = pane.mem_spark.len().saturating_sub(mem_width);
        let mem_data: Vec<u64> = pane.mem_spark.iter().skip(mem_skip).copied().collect();
        let mem_title = format!("mem {}%", mem_data.last().copied().unwrap_or(0));
        frame.render_widget(
            Sparkline::default()
                .block(Block::bordered().title(mem_title))
                .data(&mem_data)
                .style(Style::default().fg(Color::Magenta)),
            mem_area,
        );
    }
    let lines: Vec<Line> = pane.lines.iter().map(pane_line).collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(pane.title.as_str())),
        body,
    );
}

/// Turns graph text into styled lines, reversing and bolding only the selected node's label so
/// its state colour and the edges around it stay visible.
fn graph_widget_lines(rendered: &RenderedGraph, selected: Option<&str>) -> Vec<Line<'static>> {
    rendered
        .lines
        .iter()
        .enumerate()
        .map(|(y, line)| {
            let selected_node = selected.and_then(|id| {
                rendered
                    .nodes
                    .iter()
                    .find(|node| node.id == id && node.y == y)
            });
            let mut spans = Vec::new();
            let mut x = 0usize;
            for segment in &line.segments {
                let cells: Vec<char> = segment.text.chars().collect();
                let mut start = 0usize;
                while start < cells.len() {
                    let inside = |at: usize| {
                        selected_node.is_some_and(|node| at >= node.x && at < node.x + node.width)
                    };
                    let selected_cell = inside(x + start);
                    let mut end = start + 1;
                    while end < cells.len() && inside(x + end) == selected_cell {
                        end += 1;
                    }
                    let text: String = cells[start..end].iter().collect();
                    let style = if selected_cell {
                        style_for(segment.tone)
                            .add_modifier(Modifier::REVERSED)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        style_for(segment.tone)
                    };
                    spans.push(Span::styled(text, style));
                    start = end;
                }
                x += cells.len();
            }
            Line::from(spans)
        })
        .collect()
}

/// Lays out a view model; all text decisions were made in the model.
pub fn draw(frame: &mut Frame, view: &ViewModel) -> LayoutInfo {
    let area = frame.area();
    let chart_height = if area.height >= 24 { 5 } else { 0 };
    let [charts, body, status, footer] = Layout::vertical([
        Constraint::Length(chart_height),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    if chart_height > 0 && !view.charts.is_empty() {
        let cells = Layout::horizontal(vec![
            Constraint::Ratio(1, view.charts.len() as u32);
            view.charts.len()
        ])
        .split(charts);
        for (chart, cell) in view.charts.iter().zip(cells.iter()) {
            let width = cell.width.saturating_sub(2) as usize;
            let skip = chart.values.len().saturating_sub(width);
            let data: Vec<u64> = chart.values[skip..].to_vec();
            let title = format!("{} {}", chart.title, chart.latest);
            let sparkline = Sparkline::default()
                .block(Block::bordered().title(title))
                .data(&data)
                .style(Style::default().fg(Color::Cyan));
            frame.render_widget(sparkline, *cell);
        }
    }

    let process_height = view.process_pane.as_ref().map_or(0, |pane| {
        let spark_h = if body.height >= 10 { 3 } else { 0 };
        (spark_h + pane.lines.len() as u16 + 2).min(body.height.saturating_sub(1))
    });
    let detail_height = view
        .cordon_pane
        .as_ref()
        .map_or(0, |pane| pane.lines.len() as u16 + 2);
    let log_height = if process_height > 0 || detail_height > 0 {
        0
    } else {
        view.log
            .as_ref()
            .map_or(0, |log| log.lines.len() as u16 + 2)
            .min(body.height.saturating_sub(1))
    };
    let bottom_height = if process_height > 0 {
        process_height
    } else if detail_height > 0 {
        detail_height.min(body.height.saturating_sub(1))
    } else {
        log_height
    };
    let graph_view = (area.height >= 24).then_some(view.graph.as_ref()).flatten();
    let rendered = graph_view.map(|graph| graph::render(graph, area.width as usize, false));
    // The panel never pushes the other panes: it gets at most a third of the screen,
    // with three content rows and a border whenever there is a graph to show.
    let graph_height = rendered
        .as_ref()
        .filter(|graph| !graph.lines.is_empty())
        .map_or(0, |graph| {
            let cap = area.height / 3;
            let room = body.height.saturating_sub(bottom_height).saturating_sub(1);
            ((graph.lines.len() + 2) as u16).max(5).min(cap).min(room)
        });
    let [table_area, graph_area, bottom_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(graph_height),
        Constraint::Length(bottom_height),
    ])
    .areas(body);

    let focus_border = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let nodes_block =
        Block::bordered()
            .title(view.title.as_str())
            .border_style(if view.focus == Focus::Nodes {
                focus_border
            } else {
                Style::default()
            });
    let jobs_block = Block::bordered()
        .title("Jobs")
        .border_style(if view.focus == Focus::Jobs {
            focus_border
        } else {
            Style::default()
        });
    let node_lines = 1 + view.node_rows;
    let node_height = u16::try_from(node_lines + 2)
        .unwrap_or(u16::MAX)
        .min(table_area.height);
    let [node_area, jobs_area] =
        Layout::vertical([Constraint::Length(node_height), Constraint::Min(0)]).areas(table_area);
    let node_widget: Vec<Line> = view
        .lines
        .iter()
        .take(node_lines)
        .enumerate()
        .map(|(index, line)| model_line(line, view.selected_line == Some(index)))
        .collect();
    let job_widget: Vec<Line> = view
        .lines
        .iter()
        .skip(node_lines)
        .enumerate()
        .map(|(index, line)| model_line(line, view.selected_line == Some(node_lines + index)))
        .collect();
    let nodes = nodes_block.inner(node_area);
    let jobs = jobs_block.inner(jobs_area);
    frame.render_widget(
        Paragraph::new(Text::from(node_widget)).block(nodes_block),
        node_area,
    );
    frame.render_widget(
        Paragraph::new(Text::from(job_widget)).block(jobs_block),
        jobs_area,
    );

    let mut graph_rect = Rect::new(0, 0, 0, 0);
    let mut shown_graph = RenderedGraph::default();
    if graph_height > 0 {
        if let Some(full) = &rendered {
            let content = graph_height.saturating_sub(2) as usize;
            let offset = view
                .graph_offset
                .min(full.lines.len().saturating_sub(content));
            let lines = full
                .lines
                .iter()
                .skip(offset)
                .take(content)
                .cloned()
                .collect();
            let nodes = full
                .nodes
                .iter()
                .filter(|node| node.y >= offset && node.y < offset + content)
                .map(|node| NodeRect {
                    y: node.y - offset,
                    ..node.clone()
                })
                .collect();
            shown_graph = RenderedGraph { lines, nodes };

            let hidden = full.lines.len().saturating_sub(content);
            let mut title = "Graph".to_owned();
            let mut notes = Vec::new();
            if let Some(view) = graph_view {
                if let Some(cutoff) = graph::cutoff_text(view) {
                    notes.push(cutoff);
                }
            }
            if hidden > 0 {
                notes.push(format!("+{hidden} rows"));
            }
            if !notes.is_empty() {
                title.push(' ');
                title.push_str(&notes.join(", "));
            }
            let mut block =
                Block::bordered()
                    .title(title)
                    .border_style(if view.focus == Focus::Graph {
                        focus_border
                    } else {
                        Style::default()
                    });
            if let Some(detail) = &view.graph_detail {
                let width = graph_area.width.saturating_sub(2) as usize;
                let text: String = detail.text.chars().take(width).collect();
                block = block.title_bottom(
                    Line::from(Span::styled(text, style_for(detail.tone)))
                        .alignment(Alignment::Left),
                );
            }
            graph_rect = block.inner(graph_area);
            let lines = graph_widget_lines(&shown_graph, view.selected_job.as_deref());
            frame.render_widget(Paragraph::new(Text::from(lines)).block(block), graph_area);
        }
    }

    if let Some(pane) = &view.process_pane {
        draw_process_pane(frame, pane, bottom_area);
    } else if let Some(pane) = &view.cordon_pane {
        let lines: Vec<Line> = pane.lines.iter().map(pane_line).collect();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().title(pane.title.as_str())),
            bottom_area,
        );
    } else if let Some(log) = &view.log {
        let title = if log.title.is_empty() {
            format!("log {}", log.job_id)
        } else {
            format!("log {}: {}", log.job_id, log.title)
        };
        let lines: Vec<Line> = log.lines.iter().map(pane_line).collect();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
            bottom_area,
        );
    }

    let status_style = style_for(view.status.tone).add_modifier(if view.paused {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    frame.render_widget(
        Paragraph::new(view.status.text.as_str()).style(status_style),
        status,
    );
    frame.render_widget(Paragraph::new(view.footer.as_str()), footer);
    LayoutInfo {
        nodes,
        jobs,
        graph: graph_rect,
        rendered_graph: shown_graph,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use super::*;
    use crate::model::{CordonCell, Dashboard, JobRow, NodeRow, ProviderCell, Snapshot, Update};

    fn screen(view: &ViewModel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, view);
            })
            .unwrap();
        text_of(terminal.backend().buffer())
    }

    fn text_of(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_colored_cell(buffer: &Buffer, symbol: char, color: Color) -> bool {
        let area = buffer.area;
        (area.y..area.y + area.height).any(|y| {
            (area.x..area.x + area.width).any(|x| {
                buffer[(x, y)].symbol() == symbol.to_string() && buffer[(x, y)].fg == color
            })
        })
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
                jobs: vec![JobRow {
                    id: "01ABC".to_owned(),
                    title: "do the thing".to_owned(),
                    state: "running".to_owned(),
                    node: Some("peer-a".to_owned()),
                    attempt: 1,
                    age_secs: 5,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            Instant::now(),
        );
        dashboard
    }

    #[test]
    fn a_live_screen_draws_nodes_jobs_status_and_footer() {
        let text = screen(&dashboard().view(Instant::now()), 90, 12);
        assert!(text.contains("lu top"));
        assert!(text.contains("peer-a"));
        assert!(text.contains("01ABC"));
        assert!(text.contains("live, update 1"));
        assert!(text.contains("q quit"));
        assert!(text.contains("d drain"), "{text}");
        let view = dashboard().view(Instant::now());
        let mut focus = dashboard();
        focus.toggle_focus();
        let text = screen(&focus.view(Instant::now()), 90, 12);
        assert!(text.contains("x proc"), "{text}");
        assert!(!view.footer.contains("x proc"));
    }

    #[test]
    fn cordons_draw_orange_chips_and_distinct_detail_text() {
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
                    providers: vec![
                        ProviderCell {
                            name: "zen".to_owned(),
                            state: "ok".to_owned(),
                            soonest_secs: None,
                        },
                        ProviderCell {
                            name: "openrouter".to_owned(),
                            state: "ok".to_owned(),
                            soonest_secs: None,
                        },
                    ],
                    cordons: vec![
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
                    ],
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
                jobs: Vec::new(),
            }),
            Instant::now(),
        );
        let view = dashboard.view(Instant::now());
        let text = screen(&view, 120, 20);
        assert!(text.contains("!zen"), "{text}");
        assert!(text.contains("!openrouter~"), "{text}");
        assert!(text.contains("cordons peer-a"), "{text}");
        assert!(text.contains("! zen 2m left"), "{text}");
        assert!(text.contains("~ openrouter lifts on 2 healthy"), "{text}");

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &view);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(has_colored_cell(buffer, '!', Color::Rgb(255, 140, 0)));
        assert!(has_colored_cell(buffer, '~', Color::Yellow));
    }

    #[test]
    fn a_paused_screen_says_how_old_its_data_is() {
        let mut dashboard = dashboard();
        let t0 = Instant::now();
        dashboard.toggle_pause(t0);
        let text = screen(&dashboard.view(t0 + Duration::from_secs(7)), 90, 12);
        assert!(text.contains("paused: showing data from 7s ago"), "{text}");
    }

    #[test]
    fn a_tall_screen_draws_the_charts() {
        let text = screen(&dashboard().view(Instant::now()), 100, 30);
        assert!(text.contains("running jobs 1"), "{text}");
        assert!(text.contains("providers ok 0"), "{text}");
    }

    #[test]
    fn a_tall_screen_draws_the_dependency_graph_under_the_jobs() {
        let mut dashboard = Dashboard::new();
        let row = |id: &str, state: &str, title: &str, after: &[&str]| JobRow {
            id: id.to_owned(),
            title: title.to_owned(),
            state: state.to_owned(),
            node: None,
            attempt: 1,
            age_secs: 1,
            model: None,
            note: String::new(),
            after: after.iter().map(|id| (*id).to_owned()).collect(),
        };
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs: vec![
                    row(
                        "01H6CQM",
                        "blocked",
                        "merge results",
                        &["01KE9AR", "01Z7W0A"],
                    ),
                    row("01KE9AR", "done", "index", &[]),
                    row("01Z7W0A", "running", "tests", &[]),
                ],
            }),
            Instant::now(),
        );
        let text = screen(&dashboard.view(Instant::now()), 90, 30);
        assert!(text.contains("┌Graph"), "{text}");
        let row = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no graph row with {needle} in {text}"))
                .to_owned()
        };
        let first = row("KE9AR ─┐");
        assert!(first.contains("✓ KE9AR"), "{first}");
        assert!(!first.contains("index"), "{first}");
        let join = row("├─▶ ◌ H6CQM");
        assert!(!join.contains("merge results"), "{join}");
        let second = row("Z7W0A ─┘");
        assert!(second.contains("▶ Z7W0A"), "{second}");
        assert!(!second.contains("tests"), "{second}");

        dashboard.toggle_graph();
        let hidden = screen(&dashboard.view(Instant::now()), 90, 30);
        assert!(!hidden.contains("├─▶"), "{hidden}");
    }

    #[test]
    fn a_short_screen_hides_the_dependency_graph() {
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs: vec![JobRow {
                    id: "01ABC".to_owned(),
                    title: "do the thing".to_owned(),
                    state: "running".to_owned(),
                    node: None,
                    attempt: 1,
                    age_secs: 1,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            Instant::now(),
        );
        let text = screen(&dashboard.view(Instant::now()), 90, 12);
        assert!(!text.contains('▶'), "{text}");
    }

    #[test]
    fn a_tiny_terminal_draws_without_panicking() {
        let view = dashboard().view(Instant::now());
        let _ = screen(&view, 1, 1);
        let _ = screen(&view, 0, 0);
        let _ = screen(&view, 12, 2);
    }

    #[test]
    fn a_log_pane_is_drawn_under_the_tables() {
        let mut dashboard = dashboard();
        let t0 = Instant::now();
        dashboard.toggle_focus();
        let id = dashboard.open_selected_log().unwrap();
        dashboard.apply_update(Update::Log {
            job_id: id,
            result: Ok("first\nlast".to_owned()),
        });
        let text = screen(&dashboard.view(t0), 90, 20);
        assert!(text.contains("log 01ABC"), "{text}");
        assert!(text.contains("last"), "{text}");
    }

    #[test]
    fn a_tall_screen_draws_the_process_pane_with_sparklines() {
        use crate::model::{ProcessJobView, ProcessTreeRow};
        let mut dashboard = Dashboard::new();
        let t0 = Instant::now();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: Some("0.1.10".to_owned()),
                nodes: vec![NodeRow {
                    name: "peer-a".to_owned(),
                    harness: "busy".to_owned(),
                    drained: false,
                    seen_secs_ago: Some(1),
                    providers: Vec::new(),
                    cordons: Vec::new(),
                    version: Some("0.1.10".to_owned()),
                    load1: Some(1.23),
                    mem_available_kb: Some(200),
                    mem_total_kb: Some(1000),
                    stuck: 0,
                    workspace_bytes: 0,
                    workspace_count: 0,
                    workspace_reclaimed_bytes_total: 0,
                    workspace_evictions_total: 0,
                    workspace_delete_errors_total: 0,
                    labels: Vec::new(),
                }],
                jobs: vec![JobRow {
                    id: "01RUN".to_owned(),
                    title: "do the thing".to_owned(),
                    state: "running".to_owned(),
                    node: Some("peer-a".to_owned()),
                    attempt: 1,
                    age_secs: 5,
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
            node: "peer-a".to_owned(),
            result: Ok(Some(ProcessJobView {
                output_idle_secs: 130,
                cpu_idle_secs: 140,
                uploader_behind: None,
                stuck: false,
                processes: vec![ProcessTreeRow {
                    pid: 4242,
                    state: "S".to_owned(),
                    cpu_s: 7,
                    rss_kb: 1024,
                    age_secs: 90,
                    command: "opencode run".to_owned(),
                }],
            })),
        });
        let text = screen(&dashboard.view(t0), 120, 35);
        assert!(text.contains("process 01RUN @ peer-a"), "{text}");
        assert!(text.contains("output idle 130s"), "{text}");
        assert!(text.contains("cpu idle 140s"), "{text}");
        assert!(text.contains("uploader behind -"), "{text}");
        assert!(text.contains("PID"), "{text}");
        assert!(text.contains("4242"), "{text}");
        assert!(text.contains("opencode run"), "{text}");
        assert!(text.contains("load"), "{text}");
        assert!(text.contains("mem"), "{text}");
    }

    fn job_row(id: &str, after: &[&str]) -> JobRow {
        JobRow {
            id: id.to_owned(),
            title: "a long task title that must not appear in the graph".to_owned(),
            state: "queued".to_owned(),
            node: None,
            attempt: 1,
            age_secs: 1,
            model: None,
            note: String::new(),
            after: after.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn the_graph_panel_titles_the_counts_when_jobs_are_left_out() {
        let mut dashboard = Dashboard::new();
        let jobs: Vec<JobRow> = (0..10)
            .map(|index| job_row(&format!("01JOB{index}"), &[]))
            .collect();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs,
            }),
            Instant::now(),
        );
        let text = screen(&dashboard.view(Instant::now()), 60, 30);
        assert!(text.contains("┌Graph +4 jobs, 4 components"), "{text}");
        assert!(!text.contains("not shown"), "{text}");
    }

    #[test]
    fn the_graph_panel_clips_tall_graphs_and_says_how_many_rows_are_hidden() {
        let mut dashboard = Dashboard::new();
        let mut jobs = Vec::new();
        for component in 0..6 {
            let parent = format!("01P{component}");
            let first = format!("01A{component}");
            let second = format!("01B{component}");
            jobs.push(job_row(&first, &[&parent]));
            jobs.push(job_row(&second, &[&parent]));
            jobs.push(job_row(&parent, &[]));
        }
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs,
            }),
            Instant::now(),
        );
        let text = screen(&dashboard.view(Instant::now()), 60, 24);
        assert!(text.contains("┌Graph +12 rows"), "{text}");
        let panel = text
            .lines()
            .skip_while(|line| !line.starts_with("┌Graph"))
            .take_while(|line| !line.starts_with("└"))
            .count();
        assert_eq!(panel, 7, "{text}");
        assert!(text.lines().any(|line| line.contains("├─▶")), "{text}");
    }

    fn draw_view(view: &ViewModel, width: u16, height: u16) -> (LayoutInfo, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut layout = LayoutInfo::default();
        terminal.draw(|frame| layout = draw(frame, view)).unwrap();
        (layout, terminal.backend().buffer().clone())
    }

    fn graph_dashboard(ids: &[&str]) -> Dashboard {
        let jobs = ids
            .iter()
            .map(|id| JobRow {
                id: (*id).to_owned(),
                title: "a title much longer than the graph panel can show".to_owned(),
                state: "running".to_owned(),
                node: None,
                attempt: 1,
                age_secs: 1,
                model: None,
                note: String::new(),
                after: Vec::new(),
            })
            .collect();
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs,
            }),
            Instant::now(),
        );
        dashboard
    }

    #[test]
    fn the_focused_pane_has_a_highlighted_border() {
        let mut dashboard = graph_dashboard(&["01AAA", "01BBB"]);
        let (layout, buffer) = draw_view(&dashboard.view(Instant::now()), 100, 30);
        let border_y = layout.graph.y.saturating_sub(1);
        assert_ne!(buffer[(layout.graph.x, border_y)].fg, Color::Cyan);

        dashboard.toggle_focus();
        dashboard.toggle_focus();
        let view = dashboard.view(Instant::now());
        let (layout, buffer) = draw_view(&view, 100, 30);
        let cell = &buffer[(layout.graph.x, layout.graph.y.saturating_sub(1))];
        assert_eq!(cell.fg, Color::Cyan, "{cell:?}");
        assert!(cell.modifier.contains(Modifier::BOLD), "{cell:?}");
        assert_eq!(view.focus, Focus::Graph);
    }

    #[test]
    fn the_graph_border_shows_the_selected_jobs_full_details() {
        let mut dashboard = graph_dashboard(&["01LONGJOBID123"]);
        dashboard.toggle_focus();
        dashboard.toggle_focus();
        let view = dashboard.view(Instant::now());
        let (layout, buffer) = draw_view(&view, 40, 30);
        let text = text_of(&buffer);
        let bottom = text
            .lines()
            .nth(usize::from(layout.graph.y + layout.graph.height))
            .expect("the graph has a bottom border")
            .to_owned();
        assert!(bottom.contains("01LONGJOBID123"), "{bottom}");
        assert!(bottom.contains("running"), "{bottom}");
        assert!(bottom.contains("a title much"), "{bottom}");
        assert!(!bottom.contains("panel can show"), "{bottom}");
    }

    #[test]
    fn the_selected_node_is_reversed_and_bold_and_wheel_scrolling_moves_the_window() {
        let ids: Vec<String> = (0..32).map(|index| format!("01{index:03}")).collect();
        let jobs = ids
            .iter()
            .enumerate()
            .map(|(index, id)| JobRow {
                id: id.clone(),
                title: "a fan-out job".to_owned(),
                state: "running".to_owned(),
                node: None,
                attempt: 1,
                age_secs: 1,
                model: None,
                note: String::new(),
                after: if index == 0 {
                    Vec::new()
                } else {
                    vec!["01000".to_owned()]
                },
            })
            .collect();
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                drafts: 0,
                oldest_draft_secs: 0,
                controller_version: None,
                nodes: Vec::new(),
                jobs,
            }),
            Instant::now(),
        );
        dashboard.select_job_id("01000", Focus::Graph);
        let view = dashboard.view(Instant::now());
        let (layout, buffer) = draw_view(&view, 100, 30);
        let node = layout
            .rendered_graph
            .nodes
            .iter()
            .find(|node| node.id == "01000")
            .expect("the selected node is drawn")
            .clone();
        let cell = &buffer[(
            layout.graph.x + node.x as u16,
            layout.graph.y + node.y as u16,
        )];
        assert!(cell.modifier.contains(Modifier::REVERSED), "{cell:?}");
        assert!(cell.modifier.contains(Modifier::BOLD), "{cell:?}");
        let first = layout.rendered_graph.nodes.first().unwrap().id.clone();

        dashboard.scroll_graph(true);
        let view = dashboard.view(Instant::now());
        let (scrolled, _) = draw_view(&view, 100, 30);
        assert_eq!(view.graph_offset, 3);
        assert_ne!(scrolled.rendered_graph.nodes.first().unwrap().id, first);
        assert!(scrolled.rendered_graph.nodes.iter().all(|node| node.y < 8));
        assert!(
            scrolled
                .rendered_graph
                .nodes
                .iter()
                .all(|node| node.id != "01000"),
            "a selected job outside the window has no graph highlight"
        );
    }
}
