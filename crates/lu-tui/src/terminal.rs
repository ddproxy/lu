//! Terminal setup that is always undone: when the guard drops (including while unwinding), when
//! any thread panics, and never twice. Mouse capture is scoped the same way, so copy and paste in
//! other applications keep working as soon as the dashboard exits.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

/// Holds the alternate screen, raw mode and mouse capture. Dropping it restores the terminal.
pub struct TerminalGuard {
    _private: (),
}

fn enter_session(out: &mut impl Write, enable_raw: fn() -> io::Result<()>) -> io::Result<()> {
    execute!(out, EnterAlternateScreen)?;
    if let Err(error) = execute!(out, EnableMouseCapture) {
        let _ = execute!(out, LeaveAlternateScreen);
        return Err(error);
    }
    if let Err(error) = enable_raw() {
        let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
        return Err(error);
    }
    Ok(())
}

fn leave_session(out: &mut impl Write, disable_raw: fn() -> io::Result<()>) {
    let _ = disable_raw();
    let _ = execute!(out, LeaveAlternateScreen, Show, DisableMouseCapture);
    let _ = out.flush();
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        // A panic prints its message to the screen the user is looking at. Restore first, so the
        // message lands on the normal screen where it can be read; that also releases the mouse.
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        ACTIVE.store(true, Ordering::SeqCst);
        if let Err(error) = enter_session(&mut io::stdout(), terminal::enable_raw_mode) {
            restore();
            return Err(error);
        }
        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Leaves raw mode, the alternate screen and mouse capture. Does nothing unless a guard is active,
/// so the panic hook and the guard can both call it.
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        leave_session(&mut io::stdout(), terminal::disable_raw_mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_RAW: AtomicBool = AtomicBool::new(false);

    fn raw_on() -> io::Result<()> {
        TEST_RAW.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn raw_off() -> io::Result<()> {
        TEST_RAW.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn raw_fails() -> io::Result<()> {
        Err(io::Error::other("no tty"))
    }

    fn as_text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("crossterm writes escape sequences as UTF-8")
    }

    #[test]
    fn setup_and_restore_are_symmetric() {
        let mut bytes = Vec::new();
        enter_session(&mut bytes, raw_on).expect("setup succeeds");
        assert!(TEST_RAW.load(Ordering::SeqCst));
        leave_session(&mut bytes, raw_off);
        assert!(!TEST_RAW.load(Ordering::SeqCst));
        let text = as_text(&bytes);
        assert!(text.contains("\u{1b}[?1049h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1000h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1003h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1006h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1049l"), "{text:?}");
        assert!(text.contains("\u{1b}[?1000l"), "{text:?}");
        assert!(text.contains("\u{1b}[?1003l"), "{text:?}");
        assert!(text.contains("\u{1b}[?1006l"), "{text:?}");
        assert!(text.contains("\u{1b}[?25h"), "{text:?}");
    }

    #[test]
    fn a_failed_setup_undoes_the_screen_and_mouse() {
        let mut bytes = Vec::new();
        assert!(enter_session(&mut bytes, raw_fails).is_err());
        let text = as_text(&bytes);
        assert!(text.contains("\u{1b}[?1049h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1049l"), "{text:?}");
        assert!(text.contains("\u{1b}[?1000h"), "{text:?}");
        assert!(text.contains("\u{1b}[?1000l"), "{text:?}");
    }
}
