use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Sparkline};
use ratatui::Frame;

use crate::graph;
use crate::model::{Tone, ViewModel};

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

fn graph_line(line: &graph::GraphLine) -> Line<'static> {
    Line::from(
        line.segments
            .iter()
            .map(|segment| Span::styled(segment.text.clone(), style_for(segment.tone)))
            .collect::<Vec<_>>(),
    )
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

/// Lays out a view model; all text decisions were made in the model.
pub fn draw(frame: &mut Frame, view: &ViewModel) {
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
    let log_height = if process_height > 0 {
        0
    } else {
        view.log
            .as_ref()
            .map_or(0, |log| log.lines.len() as u16 + 2)
            .min(body.height.saturating_sub(1))
    };
    let bottom_height = if process_height > 0 {
        process_height
    } else {
        log_height
    };
    let graph = (area.height >= 24)
        .then_some(view.graph.as_ref())
        .flatten()
        .map(|graph| graph::render(graph, area.width as usize));
    let graph_height = graph
        .as_ref()
        .map_or(0, |lines| lines.len() as u16)
        .min(body.height.saturating_sub(bottom_height).saturating_sub(3));
    let [table_area, graph_area, bottom_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(graph_height),
        Constraint::Length(bottom_height),
    ])
    .areas(body);

    let lines: Vec<Line> = view
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| model_line(line, view.selected_line == Some(index)))
        .collect();
    let block = Block::bordered().title(view.title.as_str());
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), table_area);

    if graph_height > 0 {
        if let Some(lines) = graph {
            let lines: Vec<Line> = lines
                .iter()
                .take(graph_height as usize)
                .map(graph_line)
                .collect();
            frame.render_widget(Paragraph::new(Text::from(lines)), graph_area);
        }
    }

    if let Some(pane) = &view.process_pane {
        draw_process_pane(frame, pane, bottom_area);
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
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use super::*;
    use crate::model::{Dashboard, JobRow, NodeRow, Snapshot, Update};

    fn screen(view: &ViewModel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, view)).unwrap();
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

    fn dashboard() -> Dashboard {
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                controller_version: None,
                nodes: vec![NodeRow {
                    name: "peer-a".to_owned(),
                    harness: "idle".to_owned(),
                    drained: false,
                    seen_secs_ago: Some(2),
                    providers: Vec::new(),
                    version: None,
                    load1: None,
                    mem_available_kb: None,
                    mem_total_kb: None,
                    stuck: 0,
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
        assert!(text.contains("x proc"));
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
        assert!(text.contains("✓ KE9AR index"), "{text}");
        assert!(text.contains("▶ Z7W0A tests"), "{text}");
        assert!(text.contains("◌ H6CQM merge results"), "{text}");
        assert!(text.contains('┐'), "{text}");
        assert!(text.contains("├──▶"), "{text}");
        assert!(text.contains('┘'), "{text}");

        dashboard.toggle_graph();
        let hidden = screen(&dashboard.view(Instant::now()), 90, 30);
        assert!(!hidden.contains("├──▶"), "{hidden}");
    }

    #[test]
    fn a_short_screen_hides_the_dependency_graph() {
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
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
                controller_version: Some("0.1.10".to_owned()),
                nodes: vec![NodeRow {
                    name: "peer-a".to_owned(),
                    harness: "busy".to_owned(),
                    drained: false,
                    seen_secs_ago: Some(1),
                    providers: Vec::new(),
                    version: Some("0.1.10".to_owned()),
                    load1: Some(1.23),
                    mem_available_kb: Some(200),
                    mem_total_kb: Some(1000),
                    stuck: 0,
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
}
