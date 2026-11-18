use ratatui::layout::{Constraint, Layout};
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
    let log_height = view
        .log
        .as_ref()
        .map_or(0, |log| log.lines.len() as u16 + 2)
        .min(body.height.saturating_sub(1));
    let graph = (area.height >= 24)
        .then_some(view.graph.as_ref())
        .flatten()
        .map(|graph| graph::render(graph, area.width as usize));
    let graph_height = graph
        .as_ref()
        .map_or(0, |lines| lines.len() as u16)
        .min(body.height.saturating_sub(log_height).saturating_sub(3));
    let [table_area, graph_area, log_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(graph_height),
        Constraint::Length(log_height),
    ])
    .areas(body);
    let lines: Vec<Line> = view
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut style = style_for(line.tone);
            if view.selected_line == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(line.text.as_str()).style(style)
        })
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
    if let Some(log) = &view.log {
        let title = if log.title.is_empty() {
            format!("log {}", log.job_id)
        } else {
            format!("log {}: {}", log.job_id, log.title)
        };
        let lines: Vec<Line> = log
            .lines
            .iter()
            .map(|line| Line::from(line.text.as_str()).style(style_for(line.tone)))
            .collect();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
            log_area,
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
                nodes: vec![NodeRow {
                    name: "peer-a".to_owned(),
                    harness: "idle".to_owned(),
                    drained: false,
                    seen_secs_ago: Some(2),
                    providers: Vec::new(),
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
}
