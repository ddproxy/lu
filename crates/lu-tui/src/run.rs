use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::model::{Dashboard, Request, Snapshot, Update};
use crate::render;
use crate::terminal::TerminalGuard;

/// Longest a key press waits before the screen is redrawn anyway.
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    TogglePause,
    MoveUp,
    MoveDown,
    ToggleFocus,
    OpenLog,
    Escape,
    Drain,
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
        (KeyCode::Tab, _) => Some(Action::ToggleFocus),
        (KeyCode::Enter, _) => Some(Action::OpenLog),
        (KeyCode::Char('d'), _) => Some(Action::Drain),
        _ => None,
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
    while !stop.load(Ordering::Relaxed) {
        while let Ok(result) = feed.try_recv() {
            dashboard.apply(result, Instant::now());
        }
        while let Ok(update) = updates.try_recv() {
            dashboard.apply_update(update);
        }
        terminal.draw(|frame| render::draw(frame, &dashboard.view(Instant::now())))?;
        if event::poll(POLL)? {
            if let Event::Key(key) = event::read()? {
                match action_for(key) {
                    Some(Action::Quit) => break,
                    Some(Action::Escape) => {
                        if !dashboard.close_log() {
                            break;
                        }
                    }
                    Some(Action::TogglePause) => dashboard.toggle_pause(Instant::now()),
                    Some(Action::MoveUp) => dashboard.move_selection(false),
                    Some(Action::MoveDown) => dashboard.move_selection(true),
                    Some(Action::ToggleFocus) => dashboard.toggle_focus(),
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
                    None => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;

    use super::*;

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
        assert_eq!(
            action_for(press(KeyCode::Char('q'), none)),
            Some(Action::Quit)
        );
        assert_eq!(action_for(press(KeyCode::Esc, none)), Some(Action::Escape));
        assert_eq!(
            action_for(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('p'), none)),
            Some(Action::TogglePause)
        );
        assert_eq!(
            action_for(press(KeyCode::Char(' '), none)),
            Some(Action::TogglePause)
        );
        assert_eq!(action_for(press(KeyCode::Up, none)), Some(Action::MoveUp));
        assert_eq!(
            action_for(press(KeyCode::Char('k'), none)),
            Some(Action::MoveUp)
        );
        assert_eq!(
            action_for(press(KeyCode::Down, none)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('j'), none)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            action_for(press(KeyCode::Tab, none)),
            Some(Action::ToggleFocus)
        );
        assert_eq!(
            action_for(press(KeyCode::Enter, none)),
            Some(Action::OpenLog)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('d'), none)),
            Some(Action::Drain)
        );
        assert_eq!(action_for(press(KeyCode::Char('x'), none)), None);
        assert_eq!(
            action_for(press(KeyCode::Char('c'), none)),
            None,
            "plain c is not quit"
        );
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut release = press(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for(release), None);
    }
}
