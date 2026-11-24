use std::cmp::Ordering;

use fw_tui::theme::Theme as WidgetTheme;
use fw_tui::widgets::{
    render_table_sorted, ColWidth, SortKind, TableColumn, TableRow, TableViewState,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Sparkline, Widget};
use ratatui::Frame;

use crate::graph::{self, NodeRect, RenderedGraph};
use crate::model::{Focus, Line as ModelLine, Tone, ViewModel};

/// Persistent table state: sorting and the scroll offset survive refreshes.
#[derive(Debug, Clone, Default)]
pub struct TableUi {
    pub nodes: TableViewState,
    pub jobs: TableViewState,
}

/// Where the mouse sees each pane; coordinates are terminal cells.
#[derive(Debug, Clone)]
pub struct LayoutInfo {
    /// Content area of the Nodes block, with its header on row zero.
    pub nodes: Rect,
    /// Content area of the Jobs block, with its header on row zero.
    pub jobs: Rect,
    /// Content area of the Graph block, matching the node rectangles in `rendered_graph`.
    pub graph: Rect,
    pub rendered_graph: RenderedGraph,
    /// Column metadata from the last frame, for click hit testing and sort keys.
    pub nodes_columns: Vec<TableColumn>,
    pub jobs_columns: Vec<TableColumn>,
    /// Displayed data-row order from the last frame; maps a clicked row to a model row.
    pub nodes_order: Vec<usize>,
    pub jobs_order: Vec<usize>,
}

impl Default for LayoutInfo {
    fn default() -> Self {
        Self {
            nodes: Rect::new(0, 0, 0, 0),
            jobs: Rect::new(0, 0, 0, 0),
            graph: Rect::new(0, 0, 0, 0),
            rendered_graph: RenderedGraph::default(),
            nodes_columns: Vec::new(),
            jobs_columns: Vec::new(),
            nodes_order: Vec::new(),
            jobs_order: Vec::new(),
        }
    }
}

/// Named badge colours. Foregrounds are chosen for contrast on both light and dark terminals.
mod badge {
    use super::Color;
    pub const AMBER: Color = Color::Rgb(255, 176, 0);
    pub const GREEN: Color = Color::Rgb(22, 163, 74);
    pub const RED: Color = Color::Rgb(190, 32, 32);
    pub const BLUE: Color = Color::Rgb(37, 74, 160);
    pub const ON_LIGHT: Color = Color::Black;
    pub const ON_DARK: Color = Color::White;
}

/// What the terminal can show. `NO_COLOR` turns colours off; without a 24-bit `COLORTERM` badges
/// use bold plus reverse video instead of background colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub truecolor: bool,
}

impl Theme {
    pub fn from_env() -> Self {
        let truecolor =
            std::env::var("COLORTERM").is_ok_and(|value| value == "truecolor" || value == "24bit");
        Self {
            color: std::env::var_os("NO_COLOR").is_none(),
            truecolor,
        }
    }

    pub fn plain() -> Self {
        Self {
            color: false,
            truecolor: false,
        }
    }

    pub fn standard() -> Self {
        Self {
            color: true,
            truecolor: false,
        }
    }

    pub fn truecolor() -> Self {
        Self {
            color: true,
            truecolor: true,
        }
    }
}

/// A filled badge: a real background in 24-bit, otherwise bold + reverse video.
fn filled_badge(theme: Theme, fg: Color, bg: Color, ansi: Color) -> Style {
    if !theme.color {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else if !theme.truecolor {
        Style::default()
            .fg(ansi)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(fg).bg(bg)
    }
}

/// A text badge without a background, reduced to bold when colours are unavailable.
fn text_badge(theme: Theme, color: Color, ansi: Color, modifiers: Modifier) -> Style {
    if !theme.color {
        Style::default().add_modifier(modifiers | Modifier::BOLD)
    } else {
        Style::default()
            .fg(if theme.truecolor { color } else { ansi })
            .add_modifier(modifiers)
    }
}

fn style_for(tone: Tone, theme: Theme) -> Style {
    match tone {
        Tone::Plain => Style::default(),
        Tone::Heading => Style::default().add_modifier(Modifier::BOLD),
        Tone::Good => {
            if theme.color {
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            }
        }
        Tone::Warn => text_badge(theme, Color::Yellow, Color::Yellow, Modifier::empty()),
        Tone::Bad => text_badge(theme, Color::Red, Color::LightRed, Modifier::empty()),
        Tone::Dim => Style::default().fg(if theme.color {
            Color::DarkGray
        } else {
            Color::Reset
        }),
        Tone::Done => text_badge(theme, Color::Green, Color::Green, Modifier::DIM),
        Tone::Blocked => text_badge(
            theme,
            Color::Rgb(255, 140, 0),
            Color::Yellow,
            Modifier::empty(),
        ),
        Tone::NodeBusy => filled_badge(theme, badge::ON_LIGHT, badge::AMBER, Color::Yellow),
        Tone::NodeIdle => text_badge(theme, Color::Green, Color::Green, Modifier::DIM),
        Tone::NodeDrained => filled_badge(theme, badge::ON_DARK, badge::BLUE, Color::Blue),
        Tone::NodeOffline => filled_badge(theme, badge::ON_DARK, badge::RED, Color::Red),
        Tone::JobRunning => filled_badge(theme, badge::ON_LIGHT, badge::GREEN, Color::Green),
        Tone::JobQueued => text_badge(theme, badge::AMBER, Color::Yellow, Modifier::empty()),
        Tone::JobFailed => filled_badge(theme, badge::ON_DARK, badge::RED, Color::Red),
        Tone::JobDone => text_badge(theme, Color::Green, Color::Green, Modifier::DIM),
        Tone::JobCancelled => text_badge(theme, Color::DarkGray, Color::DarkGray, Modifier::DIM),
    }
}

fn pane_line(line: &crate::model::Line, theme: Theme) -> Line<'static> {
    if line.segments.is_empty() {
        Line::from(Span::styled(line.text.clone(), style_for(line.tone, theme)))
    } else {
        Line::from(
            line.segments
                .iter()
                .map(|segment| Span::styled(segment.text.clone(), style_for(segment.tone, theme)))
                .collect::<Vec<_>>(),
        )
    }
}

// The widget takes plain cell strings; the surrounding style map is re-applied after drawing so
// state badges, version warnings and memory colours keep the same tokens as before.
fn widget_theme(theme: Theme) -> WidgetTheme {
    WidgetTheme {
        title: if theme.color {
            Style::default()
                .fg(Color::Reset)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        },
        label: Style::default(),
        dim: Style::default(),
        good: Style::default(),
        warn: Style::default(),
        bad: Style::default(),
        accent: Style::default(),
        selected_row: Style::default(),
        header_bar: Style::default().add_modifier(Modifier::BOLD),
        border: Style::default(),
        border_focused: Style::default(),
    }
}

fn state_rank(value: &str) -> u8 {
    match value.trim() {
        "running" | "leased" => 0,
        "queued" | "recovering" | "needs_input" | "stale" | "busy" => 1,
        "blocked" | "draft" | "drained" => 2,
        "done" => 3,
        "failed" | "offline" => 4,
        "cancelled" | "expired" | "deadline_missed" | "idle" => 5,
        _ => 6,
    }
}

fn compare_state(a: &str, b: &str) -> Ordering {
    state_rank(a)
        .cmp(&state_rank(b))
        .then_with(|| a.trim().cmp(b.trim()))
}

fn metric(value: &str) -> f64 {
    let trimmed = value.trim().replace(',', "");
    let (number, factor) = match trimmed.chars().last() {
        Some('k' | 'K') => (&trimmed[..trimmed.len() - 1], 1_000.0),
        Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 1_000_000.0),
        Some('g' | 'G') => (&trimmed[..trimmed.len() - 1], 1_000_000_000.0),
        Some('B' | '%') if trimmed.len() > 1 => (&trimmed[..trimmed.len() - 1], 1.0),
        _ => (trimmed.as_str(), 1.0),
    };
    number
        .parse::<f64>()
        .ok()
        .map(|value| value * factor)
        .unwrap_or(f64::MAX)
}

fn compound_part(value: &str, index: usize) -> &str {
    value.split('/').nth(index).unwrap_or("")
}

fn compare_compound(a: &str, b: &str) -> Ordering {
    metric(compound_part(a, 0))
        .partial_cmp(&metric(compound_part(b, 0)))
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            metric(compound_part(a, 1))
                .partial_cmp(&metric(compound_part(b, 1)))
                .unwrap_or(Ordering::Equal)
        })
}

fn node_columns(view: &ViewModel) -> Vec<TableColumn> {
    vec![
        TableColumn::sortable("NODE", ColWidth::Fixed(12), SortKind::Text),
        TableColumn::sortable(
            "VERSION",
            ColWidth::Fixed(view.node_version_width as u16),
            SortKind::Text,
        ),
        TableColumn::sortable(
            "STATE",
            ColWidth::Fixed(14),
            SortKind::Custom(compare_state),
        ),
        TableColumn::sortable("SEEN", ColWidth::Fixed(8), SortKind::Duration),
        TableColumn::sortable("LOAD", ColWidth::Fixed(6), SortKind::Number).right(),
        TableColumn::sortable("MEM", ColWidth::Fixed(5), SortKind::Number).right(),
        TableColumn::sortable(
            "WORKSPACES",
            ColWidth::Fixed(12),
            SortKind::Custom(compare_compound),
        )
        .right(),
        TableColumn::sortable(
            "WS TOTALS",
            ColWidth::Fixed(16),
            SortKind::Custom(compare_compound),
        )
        .right(),
        TableColumn::sortable("PROVIDERS", ColWidth::Flex(1), SortKind::Text),
    ]
}

fn column_min_width(column: &TableColumn) -> usize {
    match column.width {
        ColWidth::Fixed(width) | ColWidth::Min(width) => usize::from(width),
        ColWidth::Flex(_) => 0,
    }
}

/// Drops the lowest-priority trailing columns until the rest fit without clipping.
fn fit_columns(mut columns: Vec<TableColumn>, width: usize) -> Vec<TableColumn> {
    while columns.len() > 1 {
        let needed: usize = columns.iter().map(column_min_width).sum::<usize>() + columns.len() - 1;
        if needed <= width {
            break;
        }
        columns.pop();
    }
    columns
}

fn job_columns(view: &ViewModel) -> Vec<TableColumn> {
    let widths = view.job_columns;
    let mut columns = vec![
        TableColumn::sortable("JOB", ColWidth::Fixed(10), SortKind::Text),
        TableColumn::sortable(
            "STATE",
            ColWidth::Fixed(11),
            SortKind::Custom(compare_state),
        ),
        TableColumn::sortable("PRI", ColWidth::Fixed(3), SortKind::Number).right(),
        TableColumn::sortable("NODE", ColWidth::Fixed(9), SortKind::Text),
        TableColumn::sortable("REPO", ColWidth::Fixed(widths.repo as u16), SortKind::Text),
        TableColumn::sortable("TRY", ColWidth::Fixed(3), SortKind::Number).right(),
        TableColumn::sortable("AGE", ColWidth::Fixed(6), SortKind::Duration),
    ];
    if widths.title > 0 {
        columns.push(TableColumn::sortable(
            "TITLE",
            ColWidth::Fixed(widths.title as u16),
            SortKind::Text,
        ));
    }
    if widths.note > 0 {
        columns.push(TableColumn::sortable(
            "NOTE",
            ColWidth::Fixed(widths.note as u16),
            SortKind::Text,
        ));
    }
    columns
}

fn constraint(width: ColWidth) -> Constraint {
    match width {
        ColWidth::Fixed(width) => Constraint::Length(width),
        ColWidth::Min(width) => Constraint::Min(width),
        ColWidth::Flex(width) => Constraint::Fill(width.max(1)),
    }
}

fn column_rects(area: Rect, columns: &[TableColumn]) -> Vec<Rect> {
    if area.width == 0 {
        return Vec::new();
    }
    Layout::horizontal(
        columns
            .iter()
            .map(|column| constraint(column.width))
            .collect::<Vec<_>>(),
    )
    .spacing(1)
    .split(Rect::new(area.x, area.y, area.width, 1))
    .to_vec()
}

/// Splits a model line into the widget's cells; `gap` strips the extra space before PROVIDERS.
fn cells_from_line(text: &str, widths: &[usize], gap: bool) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    let mut cells = Vec::with_capacity(widths.len() + 1);
    for (index, &width) in widths.iter().enumerate() {
        let end = (at + width).min(chars.len());
        cells.push(chars[at..end].iter().collect());
        at = if index + 1 < widths.len() {
            (end + 1).min(chars.len())
        } else {
            end
        };
    }
    let rest: String = chars[at..].iter().collect();
    if gap {
        cells.push(rest.strip_prefix(' ').unwrap_or(&rest).to_owned());
    } else {
        cells.push(rest);
    }
    cells
}

fn table_rows(
    lines: &[ModelLine],
    widths: &[usize],
    keys: &[String],
    gap: bool,
    expected: usize,
) -> Vec<TableRow> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut cells = cells_from_line(&line.text, widths, gap);
            cells.resize(expected, String::new());
            let mut row = TableRow::new(cells);
            if let Some(key) = keys.get(index) {
                row = row.with_key(key.clone());
            }
            row
        })
        .collect()
}

/// The widget draws plain strings; Lu's badge colours are re-applied on top afterwards.
fn apply_line_styles(
    buffer: &mut Buffer,
    rects: &[Rect],
    origin_x: u16,
    line: &ModelLine,
    y: u16,
    selected: bool,
    theme: Theme,
) {
    let mut at = 0usize;
    for segment in &line.segments {
        for _ in segment.text.chars() {
            if let Some(rect) = rects.iter().find(|rect| {
                let start = usize::from(rect.x.saturating_sub(origin_x));
                at >= start && at < start.saturating_add(usize::from(rect.width))
            }) {
                let style = style_for(segment.tone, theme);
                let style = if selected {
                    style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    style
                };
                buffer[(rect.x, y)].set_style(style);
            }
            at += 1;
        }
    }
}

/// Draws one table through the shared widget and returns the visible rows in draw order.
/// Each returned value is an index into the caller's row and line slices.
fn draw_table(
    buffer: &mut Buffer,
    area: Rect,
    columns: &[TableColumn],
    rows: &[TableRow],
    lines: &[ModelLine],
    state: &mut TableViewState,
    theme: Theme,
) -> Vec<usize> {
    render_table_sorted(area, buffer, columns, rows, state, &widget_theme(theme));
    if rows.is_empty() || area.height < 2 {
        return Vec::new();
    }
    let order = state.sorted_indices(columns, rows);
    let start = state.offset.min(order.len() - 1);
    let end = start
        .saturating_add(usize::from(area.height - 2))
        .min(order.len());
    let visible = order[start..end].to_vec();
    let rects = column_rects(area, columns);
    for (position, &index) in visible.iter().enumerate() {
        if let Some(line) = lines.get(index) {
            let y = area.y + 1 + position as u16;
            apply_line_styles(
                &mut *buffer,
                &rects,
                area.x,
                line,
                y,
                index == state.selected,
                theme,
            );
        }
    }
    visible
}

const NODE_WIDTHS: [usize; 8] = [12, 10, 14, 8, 6, 5, 12, 16];

fn job_widths(view: &ViewModel) -> Vec<usize> {
    let widths = view.job_columns;
    let mut result = vec![10, 11, 3, 9, widths.repo, 3, 6];
    if widths.title > 0 {
        result.push(widths.title);
    }
    if widths.note > 0 {
        result.push(widths.note);
    }
    result
}

fn table_widths(columns: &[TableColumn], base: &[usize]) -> Vec<usize> {
    base[..base.len().min(columns.len())].to_vec()
}

fn node_table_rows(view: &ViewModel, columns: &[TableColumn]) -> Vec<TableRow> {
    let lines = &view.lines[1..1 + view.node_names.len()];
    let mut widths = NODE_WIDTHS.to_vec();
    widths[1] = view.node_version_width;
    table_rows(
        lines,
        &table_widths(columns, &widths),
        &view.node_names,
        true,
        columns.len(),
    )
}

fn job_table_rows(view: &ViewModel, columns: &[TableColumn]) -> Vec<TableRow> {
    let start = 3 + view.node_rows;
    let lines = &view.lines[start..start + view.job_ids.len()];
    table_rows(
        lines,
        &table_widths(columns, &job_widths(view)),
        &view.job_ids,
        false,
        columns.len(),
    )
}

/// Empty tables keep model-owned placeholder text, outside the widget's row list.
fn draw_placeholder(buffer: &mut Buffer, area: Rect, line: &ModelLine, theme: Theme) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    Paragraph::new(Text::from(vec![pane_line(line, theme)]))
        .render(Rect::new(area.x, area.y + 1, area.width, 1), buffer);
}

fn draw_process_pane(
    frame: &mut Frame,
    pane: &crate::model::ProcessPaneView,
    area: Rect,
    theme: Theme,
) {
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
                .style(Style::default().fg(if theme.color {
                    Color::Cyan
                } else {
                    Color::Reset
                })),
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
                .style(Style::default().fg(if theme.color {
                    Color::Magenta
                } else {
                    Color::Reset
                })),
            mem_area,
        );
    }
    let lines: Vec<Line> = pane
        .lines
        .iter()
        .map(|line| pane_line(line, theme))
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(pane.title.as_str())),
        body,
    );
}

/// Turns graph text into styled lines, reversing and bolding only the selected node's label so
/// its state colour and the edges around it stay visible.
fn graph_widget_lines(
    rendered: &RenderedGraph,
    selected: Option<&str>,
    theme: Theme,
) -> Vec<Line<'static>> {
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
                        style_for(segment.tone, theme)
                            .add_modifier(Modifier::REVERSED)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        style_for(segment.tone, theme)
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

const GAUGE_TITLES: [&str; 4] = ["RUN", "QUE", "BUSY", "PROV"];

fn trend(chart: &crate::model::Chart) -> &'static str {
    let Some((previous, latest)) = chart
        .values
        .len()
        .checked_sub(2)
        .and_then(|at| chart.values.get(at).zip(chart.values.last()))
    else {
        return "·";
    };
    match latest.cmp(previous) {
        std::cmp::Ordering::Greater => "↑",
        std::cmp::Ordering::Less => "↓",
        std::cmp::Ordering::Equal => "→",
    }
}

/// A one-row strip: value and direction instead of four flat filled blocks.
fn draw_gauges(frame: &mut Frame, charts: &[crate::model::Chart], area: Rect, theme: Theme) {
    if area.height == 0 || area.width == 0 || charts.is_empty() {
        return;
    }
    let separator = Span::styled(
        " │ ",
        if theme.color {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        },
    );
    let mut spans = Vec::new();
    for (index, chart) in charts.iter().take(GAUGE_TITLES.len()).enumerate() {
        if index > 0 {
            spans.push(separator.clone());
        }
        let color = match index {
            0 => Color::LightGreen,
            1 => Color::Yellow,
            2 => Color::Cyan,
            _ => Color::Magenta,
        };
        let style = if theme.color {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(
            format!("{} {} {}", GAUGE_TITLES[index], chart.latest, trend(chart)),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Lays out a view model with colours taken from the environment.
pub fn draw(frame: &mut Frame, view: &ViewModel) -> LayoutInfo {
    let mut tables = TableUi::default();
    draw_with_tables(frame, view, &mut tables)
}

/// Keeps sort and selection state across frames; used by the interactive event loop.
pub fn draw_with_tables(frame: &mut Frame, view: &ViewModel, tables: &mut TableUi) -> LayoutInfo {
    draw_with_theme_and_tables(frame, view, Theme::from_env(), tables)
}

/// Lays out a view model; all text decisions were made in the model.
pub fn draw_with_theme(frame: &mut Frame, view: &ViewModel, theme: Theme) -> LayoutInfo {
    let mut tables = TableUi::default();
    draw_with_theme_and_tables(frame, view, theme, &mut tables)
}

fn draw_with_theme_and_tables(
    frame: &mut Frame,
    view: &ViewModel,
    theme: Theme,
    tables: &mut TableUi,
) -> LayoutInfo {
    let area = frame.area();
    let gauge_height = u16::from(area.height >= 12 && !view.charts.is_empty());
    let [gauges, body, status, footer] = Layout::vertical([
        Constraint::Length(gauge_height),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_gauges(frame, &view.charts, gauges, theme);

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
    // Each widget reserves two rows for its header and scroll window, so the graph yields space
    // rather than pushing table rows off screen.
    let table_need = view.node_rows.min(6) + 4 + view.job_ids.len().min(5) + 4;
    let table_room = usize::from(body.height.saturating_sub(bottom_height))
        .saturating_sub(table_need.min(usize::from(u16::MAX)));
    let graph_height = rendered
        .as_ref()
        .filter(|graph| !graph.lines.is_empty())
        .map_or(0, |graph| {
            let cap = area.height / 3;
            let room = u16::try_from(table_room).unwrap_or(u16::MAX);
            ((graph.lines.len() + 2) as u16).max(5).min(cap).min(room)
        });
    let [table_area, graph_area, bottom_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(graph_height),
        Constraint::Length(bottom_height),
    ])
    .areas(body);

    let focus_border = if theme.color {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
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
    let node_height = u16::try_from(node_lines + 3)
        .unwrap_or(u16::MAX)
        .min(table_area.height);
    let [node_area, jobs_area] =
        Layout::vertical([Constraint::Length(node_height), Constraint::Min(0)]).areas(table_area);
    let nodes = nodes_block.inner(node_area);
    let jobs = jobs_block.inner(jobs_area);
    frame.render_widget(nodes_block, node_area);
    frame.render_widget(jobs_block, jobs_area);

    let nodes_columns = fit_columns(node_columns(view), nodes.width as usize);
    let jobs_columns = fit_columns(job_columns(view), jobs.width as usize);
    let node_rows = node_table_rows(view, &nodes_columns);
    let job_rows = job_table_rows(view, &jobs_columns);
    // The view model counts rows in data order; the widget keeps its own selection index.
    if view.focus == Focus::Nodes {
        if let Some(index) = view.selected_line.and_then(|line| line.checked_sub(1)) {
            if index < node_rows.len() {
                tables.nodes.selected = index;
            }
        }
    }
    if view.focus == Focus::Jobs {
        let first_job = 3 + view.node_rows;
        if let Some(index) = view
            .selected_line
            .and_then(|line| line.checked_sub(first_job))
        {
            if index < job_rows.len() {
                tables.jobs.selected = index;
            }
        }
    }
    let nodes_order = draw_table(
        frame.buffer_mut(),
        nodes,
        &nodes_columns,
        &node_rows,
        &view.lines[1..1 + node_rows.len()],
        &mut tables.nodes,
        theme,
    );
    let jobs_order = draw_table(
        frame.buffer_mut(),
        jobs,
        &jobs_columns,
        &job_rows,
        &view.lines[3 + view.node_rows..3 + view.node_rows + job_rows.len()],
        &mut tables.jobs,
        theme,
    );
    if view.node_names.is_empty() {
        if let Some(line) = view.lines.get(1) {
            draw_placeholder(frame.buffer_mut(), nodes, line, theme);
        }
    }
    if view.job_ids.is_empty() {
        if let Some(line) = view.lines.get(3 + view.node_rows) {
            draw_placeholder(frame.buffer_mut(), jobs, line, theme);
        }
    }
    // Short lists leave the bottom of the Jobs pane empty; fill it with fleet totals.
    if job_rows.len() + 4 <= usize::from(jobs.height) {
        let summary = Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                view.summary.clone(),
                if theme.color {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                },
            )),
        ]));
        summary.render(
            Rect::new(jobs.x, jobs.y + job_rows.len() as u16 + 2, jobs.width, 2),
            frame.buffer_mut(),
        );
    }

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
                    Line::from(Span::styled(text, style_for(detail.tone, theme)))
                        .alignment(Alignment::Left),
                );
            }
            graph_rect = block.inner(graph_area);
            let lines = graph_widget_lines(&shown_graph, view.selected_job.as_deref(), theme);
            frame.render_widget(Paragraph::new(Text::from(lines)).block(block), graph_area);
        }
    }

    if let Some(pane) = &view.process_pane {
        draw_process_pane(frame, pane, bottom_area, theme);
    } else if let Some(pane) = &view.cordon_pane {
        let lines: Vec<Line> = pane
            .lines
            .iter()
            .map(|line| pane_line(line, theme))
            .collect();
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
        let lines: Vec<Line> = log
            .lines
            .iter()
            .map(|line| pane_line(line, theme))
            .collect();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
            bottom_area,
        );
    }

    let status_style = style_for(view.status.tone, theme).add_modifier(if view.paused {
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
        nodes_columns,
        jobs_columns,
        nodes_order,
        jobs_order,
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
                    repo: "owner/name".to_owned(),
                    priority: 5,
                    git_ref: "main".to_owned(),
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
        let text = screen(&dashboard().view_with_width(Instant::now(), 90), 90, 12);
        assert!(text.contains("lu top"));
        assert!(text.contains("peer-a"));
        assert!(text.contains("01ABC"));
        assert!(text.contains("live, update 1"));
        assert!(text.contains("q quit"));
        assert!(text.contains("d drain"), "{text}");
        let view = dashboard().view(Instant::now());
        let mut focus = dashboard();
        focus.toggle_focus();
        let text = screen(&focus.view_with_width(Instant::now(), 90), 90, 12);
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
        let view = dashboard.view_with_width(Instant::now(), 120);
        let text = screen(&view, 120, 20);
        assert!(text.contains("!zen"), "{text}");
        assert!(text.contains("!openrouter~"), "{text}");
        assert!(text.contains("cordons peer-a"), "{text}");
        assert!(text.contains("! zen 2m left"), "{text}");
        assert!(text.contains("~ openrouter lifts on 2 healthy"), "{text}");

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw_with_theme(frame, &view, Theme::truecolor());
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
        let text = screen(
            &dashboard.view_with_width(t0 + Duration::from_secs(7), 90),
            90,
            12,
        );
        assert!(text.contains("paused: showing data from 7s ago"), "{text}");
    }

    #[test]
    fn a_tall_screen_draws_a_compact_gauge_strip() {
        let text = screen(&dashboard().view_with_width(Instant::now(), 100), 100, 30);
        assert!(text.contains("RUN 1"), "{text}");
        assert!(text.contains("PROV 0"), "{text}");
        assert!(!text.contains("running jobs"), "{text}");
        let strip = text.lines().next().unwrap();
        assert_eq!(strip.lines().count(), 1);
    }

    #[test]
    fn a_tall_screen_draws_the_dependency_graph_under_the_jobs() {
        let mut dashboard = Dashboard::new();
        let row = |id: &str, state: &str, title: &str, after: &[&str]| JobRow {
            id: id.to_owned(),
            title: title.to_owned(),
            state: state.to_owned(),
            node: None,
            repo: "owner/name".to_owned(),
            priority: 5,
            git_ref: "main".to_owned(),
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
        let text = screen(&dashboard.view_with_width(Instant::now(), 90), 90, 30);
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
        let hidden = screen(&dashboard.view_with_width(Instant::now(), 90), 90, 30);
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
                    repo: "owner/name".to_owned(),
                    priority: 5,
                    git_ref: "main".to_owned(),
                    attempt: 1,
                    age_secs: 1,
                    model: None,
                    note: String::new(),
                    after: Vec::new(),
                }],
            }),
            Instant::now(),
        );
        let text = screen(&dashboard.view_with_width(Instant::now(), 90), 90, 12);
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
        let text = screen(&dashboard.view_with_width(t0, 90), 90, 20);
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
                    repo: "owner/name".to_owned(),
                    priority: 5,
                    git_ref: "main".to_owned(),
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
        let text = screen(&dashboard.view_with_width(t0, 120), 120, 35);
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
            repo: "owner/name".to_owned(),
            priority: 5,
            git_ref: "main".to_owned(),
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
        let text = screen(&dashboard.view_with_width(Instant::now(), 60), 60, 30);
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
        let text = screen(&dashboard.view_with_width(Instant::now(), 60), 60, 24);
        assert!(text.contains("┌Graph +13 rows"), "{text}");
        let panel = text
            .lines()
            .skip_while(|line| !line.starts_with("┌Graph"))
            .take_while(|line| !line.starts_with("└"))
            .count();
        assert_eq!(panel, 6, "{text}");
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
                repo: "owner/name".to_owned(),
                priority: 5,
                git_ref: "main".to_owned(),
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
        let (layout, buffer) = draw_view(&view, 80, 30);
        let text = text_of(&buffer);
        let bottom = text
            .lines()
            .nth(usize::from(layout.graph.y + layout.graph.height))
            .expect("the graph has a bottom border")
            .to_owned();
        assert!(bottom.contains("01LONGJOBID123"), "{bottom}");
        assert!(bottom.contains("running"), "{bottom}");
        assert!(bottom.contains("owner/name"), "{bottom}");
        assert!(bottom.contains("main"), "{bottom}");
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
                repo: "owner/name".to_owned(),
                priority: 5,
                git_ref: "main".to_owned(),
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

    fn draw_view_with_theme(
        view: &ViewModel,
        width: u16,
        height: u16,
        theme: Theme,
    ) -> (LayoutInfo, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut layout = LayoutInfo::default();
        terminal
            .draw(|frame| layout = draw_with_theme(frame, view, theme))
            .unwrap();
        (layout, terminal.backend().buffer().clone())
    }

    fn draw_with_tables(
        view: &ViewModel,
        width: u16,
        height: u16,
        theme: Theme,
        tables: &mut TableUi,
    ) -> (LayoutInfo, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut layout = LayoutInfo::default();
        terminal
            .draw(|frame| layout = draw_with_theme_and_tables(frame, view, theme, tables))
            .unwrap();
        (layout, terminal.backend().buffer().clone())
    }

    fn sorting_dashboard() -> Dashboard {
        let mut dashboard = Dashboard::new();
        let job = |id: &str, state: &str, priority: u8, age_secs: u64| JobRow {
            id: id.to_owned(),
            state: state.to_owned(),
            priority,
            age_secs,
            ..job_row(id, &[])
        };
        dashboard.apply(
            Ok(Snapshot {
                nodes: Vec::new(),
                jobs: vec![
                    job("01ZZZ", "done", 1, 600),
                    job("01AAA", "running", 9, 60),
                    job("01MMM", "failed", 5, 300),
                ],
                controller_version: None,
                drafts: 0,
                oldest_draft_secs: 0,
            }),
            Instant::now(),
        );
        dashboard.toggle_focus();
        dashboard
    }

    fn row_text(text: &str, y: u16) -> &str {
        text.lines().nth(usize::from(y)).unwrap_or("")
    }

    #[test]
    fn job_columns_sort_by_number_state_and_duration() {
        let view = sorting_dashboard().view_with_width(Instant::now(), 120);
        for (column, expected) in [
            (2usize, vec![0usize, 2, 1]), // PRI: 1, 5, 9
            (1usize, vec![1, 0, 2]),      // STATE: running, done, failed
            (6usize, vec![1, 2, 0]),      // AGE: 1m, 5m, 10m
        ] {
            let mut tables = TableUi::default();
            tables.jobs.cycle_sort(column);
            let (layout, buffer) = draw_with_tables(&view, 120, 20, Theme::plain(), &mut tables);
            assert_eq!(layout.jobs_order, expected, "column {column}");
            let text = text_of(&buffer);
            let header = row_text(&text, layout.jobs.y);
            if column != 2 {
                assert!(header.contains("^"), "{header}");
            }
            let first = row_text(&text, layout.jobs.y + 1);
            let model_index = expected[0];
            assert!(first.contains(&view.job_ids[model_index]), "{first}");
        }
    }

    #[test]
    fn sorting_keeps_the_selection_on_the_same_job() {
        let mut dashboard = sorting_dashboard();
        assert!(dashboard.select_job_id("01MMM", Focus::Jobs));
        let view = dashboard.view_with_width(Instant::now(), 120);
        let mut tables = TableUi::default();
        tables.jobs.cycle_sort(2);
        let (layout, _) = draw_with_tables(&view, 120, 20, Theme::plain(), &mut tables);
        assert_eq!(view.job_ids[tables.jobs.selected], "01MMM");
        assert_eq!(
            layout
                .jobs_order
                .iter()
                .position(|&i| i == tables.jobs.selected),
            Some(1)
        );
    }

    #[test]
    fn node_and_job_badges_use_named_truecolor_tokens() {
        let cases = [
            (Tone::NodeBusy, badge::ON_LIGHT, Some(badge::AMBER)),
            (Tone::NodeIdle, Color::Green, None),
            (Tone::NodeDrained, badge::ON_DARK, Some(badge::BLUE)),
            (Tone::NodeOffline, badge::ON_DARK, Some(badge::RED)),
            (Tone::JobRunning, badge::ON_LIGHT, Some(badge::GREEN)),
            (Tone::JobQueued, badge::AMBER, None),
            (Tone::Blocked, Color::Rgb(255, 140, 0), None),
            (Tone::JobFailed, badge::ON_DARK, Some(badge::RED)),
            (Tone::JobDone, Color::Green, None),
            (Tone::JobCancelled, Color::DarkGray, None),
        ];
        for (tone, fg, bg) in cases {
            let style = style_for(tone, Theme::truecolor());
            assert_eq!(style.fg, Some(fg), "{tone:?}");
            assert_eq!(style.bg, bg, "{tone:?}");
        }
    }

    #[test]
    fn filled_badges_fall_back_to_bold_reverse_without_truecolor() {
        let filled = [
            (Tone::NodeBusy, Color::Yellow),
            (Tone::NodeDrained, Color::Blue),
            (Tone::NodeOffline, Color::Red),
            (Tone::JobRunning, Color::Green),
            (Tone::JobFailed, Color::Red),
        ];
        for (tone, fg) in filled {
            let style = style_for(tone, Theme::standard());
            assert_eq!(style.fg, Some(fg), "{tone:?}");
            assert_eq!(style.bg, None, "{tone:?}");
            assert!(
                style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::REVERSED),
                "{tone:?} {style:?}"
            );
        }
        let text = style_for(Tone::JobQueued, Theme::standard());
        assert_eq!(text.fg, Some(Color::Yellow));
        assert_eq!(text.bg, None);
        assert!(!text.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn no_color_badges_keep_state_distinctions_with_modifiers() {
        let filled = [
            Tone::NodeBusy,
            Tone::NodeDrained,
            Tone::NodeOffline,
            Tone::JobRunning,
            Tone::JobFailed,
        ];
        let text = [
            Tone::NodeIdle,
            Tone::JobQueued,
            Tone::Blocked,
            Tone::JobDone,
            Tone::JobCancelled,
        ];
        for tone in filled {
            let style = style_for(tone, Theme::plain());
            assert_eq!(style.fg, None, "{tone:?}");
            assert_eq!(style.bg, None, "{tone:?}");
            assert!(
                style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::REVERSED),
                "{tone:?} {style:?}"
            );
        }
        for tone in text {
            let style = style_for(tone, Theme::plain());
            assert_eq!(style.fg, None, "{tone:?}");
            assert_eq!(style.bg, None, "{tone:?}");
            assert!(style.add_modifier.contains(Modifier::BOLD), "{tone:?}");
            assert!(!style.add_modifier.contains(Modifier::REVERSED), "{tone:?}");
        }
    }

    #[test]
    fn selection_is_distinct_from_a_16_color_badge() {
        let t0 = Instant::now();
        let row = |id: &str, state: &str| JobRow {
            id: id.to_owned(),
            state: state.to_owned(),
            ..job_row(id, &[])
        };
        let mut dashboard = Dashboard::new();
        dashboard.apply(
            Ok(Snapshot {
                nodes: Vec::new(),
                jobs: vec![row("01A", "running"), row("01B", "failed")],
                controller_version: None,
                drafts: 0,
                oldest_draft_secs: 0,
            }),
            t0,
        );
        assert!(dashboard.select_job_id("01A", Focus::Jobs));
        let (layout, buffer) = draw_view_with_theme(
            &dashboard.view_with_width(t0, 100),
            100,
            20,
            Theme::standard(),
        );
        let state_x = layout.jobs.x + 11;
        let selected = &buffer[(state_x, layout.jobs.y + 1)];
        let other = &buffer[(state_x, layout.jobs.y + 2)];
        assert!(
            selected.modifier.contains(Modifier::UNDERLINED),
            "{selected:?}"
        );
        assert!(selected.modifier.contains(Modifier::BOLD), "{selected:?}");
        assert!(
            selected.modifier.contains(Modifier::REVERSED),
            "{selected:?}"
        );
        assert!(!other.modifier.contains(Modifier::UNDERLINED), "{other:?}");
        assert!(other.modifier.contains(Modifier::REVERSED), "{other:?}");
    }
}
