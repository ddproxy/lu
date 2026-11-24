#![cfg(feature = "tui")]

//! Golden renders of the whole TestBackend buffer at two widths and on the NO_COLOR path.
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p lu-tui --features tui --test golden`.

use std::time::Instant;

use lu_tui::model::{Dashboard, JobRow, NodeRow, ProviderCell, Snapshot};
use lu_tui::render::{draw_with_theme, Theme};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

fn node(name: &str, harness: &str, seen: Option<u64>, drained: bool, stuck: u64) -> NodeRow {
    NodeRow {
        name: name.to_owned(),
        harness: harness.to_owned(),
        drained,
        seen_secs_ago: seen,
        providers: vec![ProviderCell {
            name: "alpha".to_owned(),
            state: "ok".to_owned(),
            soonest_secs: None,
        }],
        cordons: Vec::new(),
        version: None,
        load1: Some(0.5),
        mem_available_kb: Some(800),
        mem_total_kb: Some(1000),
        stuck,
        workspace_bytes: 1024,
        workspace_count: 1,
        workspace_reclaimed_bytes_total: 0,
        workspace_evictions_total: 0,
        workspace_delete_errors_total: 0,
        labels: Vec::new(),
    }
}

fn job(id: &str, state: &str, repo: &str, title: &str, note: &str) -> JobRow {
    JobRow {
        id: id.to_owned(),
        title: title.to_owned(),
        state: state.to_owned(),
        node: Some("peer-a".to_owned()),
        repo: repo.to_owned(),
        priority: 5,
        git_ref: "main".to_owned(),
        attempt: 1,
        age_secs: 125,
        model: None,
        note: note.to_owned(),
        after: Vec::new(),
    }
}

fn fixture() -> Dashboard {
    let mut dashboard = Dashboard::new();
    dashboard.apply(
        Ok(Snapshot {
            nodes: vec![
                node("peer-a", "busy", Some(1), false, 0),
                node("peer-b", "idle", Some(2), false, 0),
                node("peer-c", "idle", Some(3), true, 0),
                node("peer-d", "offline", None, false, 2),
            ],
            jobs: vec![
                job("01AAAA", "running", "alpha/one", "a short title", ""),
                job(
                    "01BBBB",
                    "queued",
                    "beta/two",
                    "a title long enough to need clipping on a narrow terminal",
                    "a note long enough to need clipping too",
                ),
                job("01CCCC", "failed", "gamma/three", "broken", "timeout"),
                job("01DDDD", "done", "delta/four", "finished", "no changes"),
                job("01EEEE", "cancelled", "epsilon/five", "stopped", ""),
            ],
            controller_version: None,
            drafts: 0,
            oldest_draft_secs: 0,
        }),
        Instant::now(),
    );
    dashboard
}

fn render(width: u16, height: u16, theme: Theme) -> (Buffer, String) {
    let t0 = Instant::now();
    let view = fixture().view_with_width(t0, width);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            draw_with_theme(frame, &view, theme);
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = (0..height)
        .map(|y| {
            let line: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The title carries the crate version; keep the golden files stable across releases.
    let text = text.replace(env!("CARGO_PKG_VERSION"), "X.Y.Z");
    (buffer, format!("{text}\n"))
}

fn check(name: &str, width: u16, height: u16, theme: Theme, expected: &str) {
    let (_, text) = render(width, height, theme);
    let path = format!("{}/tests/golden/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &text).expect("write golden file");
        return;
    }
    assert_eq!(text, expected, "golden render for {name}");
}

#[test]
fn wide_terminal_render_is_stable() {
    check(
        "wide_200",
        200,
        24,
        Theme::truecolor(),
        include_str!("golden/wide_200.txt"),
    );
}

#[test]
fn narrow_terminal_render_is_stable() {
    check(
        "narrow_80",
        80,
        24,
        Theme::truecolor(),
        include_str!("golden/narrow_80.txt"),
    );
}

#[test]
fn no_color_render_is_stable_and_has_no_colors() {
    let (buffer, text) = render(80, 24, Theme::plain());
    let path = format!(
        "{}/tests/golden/no_color_80.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &text).expect("write golden file");
        return;
    }
    assert_eq!(text, include_str!("golden/no_color_80.txt"));
    let area = buffer.area;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &buffer[(x, y)];
            assert_eq!(cell.fg, ratatui::style::Color::Reset, "cell at {x},{y}");
            assert_eq!(cell.bg, ratatui::style::Color::Reset, "cell at {x},{y}");
        }
    }
}
