//! The client's terminal: raw mode that is restored on every exit path (including a
//! panic), window size, and the OpenSSH-style `Enter ~ .` / `Enter ~ ?` escapes.

use std::sync::Mutex;

use rustix::termios::{OptionalActions, Termios};

static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Puts stdin in raw mode; restores the saved settings on drop.
pub struct RawMode(());

impl RawMode {
    pub fn enable() -> std::io::Result<RawMode> {
        let stdin = rustix::stdio::stdin();
        let orig = rustix::termios::tcgetattr(stdin)?;
        let mut raw = orig.clone();
        raw.make_raw();
        if let Ok(mut g) = SAVED.lock() {
            *g = Some(orig);
        }
        install_panic_hook();
        rustix::termios::tcsetattr(stdin, OptionalActions::Now, &raw)?;
        Ok(RawMode(()))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

/// Restore the terminal if raw mode is on. Safe to call more than once.
pub fn restore() {
    if let Ok(mut g) = SAVED.lock()
        && let Some(t) = g.take()
    {
        let _ = rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &t);
    }
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
    });
}

pub fn stdin_is_tty() -> bool {
    rustix::termios::isatty(rustix::stdio::stdin())
}

pub fn stdout_is_tty() -> bool {
    rustix::termios::isatty(rustix::stdio::stdout())
}

/// (cols, rows) of the terminal on stdin, or 80x24.
pub fn size() -> (u16, u16) {
    match rustix::termios::tcgetwinsize(rustix::stdio::stdin()) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => (w.ws_col, w.ws_row),
        _ => (80, 24),
    }
}

/// Copies stdin into a channel from a thread that polls with a timeout, so it can be
/// stopped (dropped) without leaving a read blocked on the terminal.
pub struct StdinPump {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StdinPump {
    pub fn start(tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> StdinPump {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            use rustix::event::{PollFd, PollFlags, Timespec, poll};
            let stdin = rustix::stdio::stdin();
            let timeout = Timespec { tv_sec: 0, tv_nsec: 50_000_000 };
            let mut buf = vec![0u8; 8192];
            while !flag.load(Ordering::Relaxed) {
                let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
                match poll(&mut fds, Some(&timeout)) {
                    Ok(0) | Err(rustix::io::Errno::INTR) => continue,
                    Ok(_) => {}
                    Err(_) => return,
                }
                match rustix::io::read(stdin, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        StdinPump { stop, thread: Some(thread) }
    }
}

impl Drop for StdinPump {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EscapeAction {
    Disconnect,
    Help,
}

/// Recognises `~.` and `~?` at the start of a line, and `~~` for a literal tilde.
#[derive(Debug)]
pub struct Escapes {
    at_line_start: bool,
    pending_tilde: bool,
}

impl Default for Escapes {
    fn default() -> Self {
        Escapes { at_line_start: true, pending_tilde: false }
    }
}

impl Escapes {
    /// Filter input bytes: returns what to send and, if one was typed, the escape action.
    /// Input after a `~.` is dropped.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Option<EscapeAction>) {
        let mut out = Vec::with_capacity(input.len());
        let mut action = None;
        for &b in input {
            if self.pending_tilde {
                self.pending_tilde = false;
                match b {
                    b'.' => return (out, Some(EscapeAction::Disconnect)),
                    b'?' => {
                        action = Some(EscapeAction::Help);
                        continue;
                    }
                    b'~' => {
                        out.push(b'~');
                        self.at_line_start = false;
                        continue;
                    }
                    _ => out.push(b'~'),
                }
            } else if self.at_line_start && b == b'~' {
                self.pending_tilde = true;
                continue;
            }
            out.push(b);
            self.at_line_start = b == b'\r' || b == b'\n';
        }
        (out, action)
    }
}

pub const ESCAPE_HELP: &str = "\r\nSupported escape sequences (after Enter):\r\n  ~.  disconnect (the session stays on the host)\r\n  ~?  this help\r\n  ~~  send a literal ~\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_at_line_start() {
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"~."), (vec![], Some(EscapeAction::Disconnect)));
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"ls\r~.junk"), (b"ls\r".to_vec(), Some(EscapeAction::Disconnect)));
    }

    #[test]
    fn tilde_mid_line_passes_through() {
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"a~.b"), (b"a~.b".to_vec(), None));
    }

    #[test]
    fn split_across_reads_and_literal() {
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"\r~"), (b"\r".to_vec(), None));
        assert_eq!(e.feed(b"."), (vec![], Some(EscapeAction::Disconnect)));
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"~~x"), (b"~x".to_vec(), None));
        let mut e = Escapes::default();
        assert_eq!(e.feed(b"~?"), (vec![], Some(EscapeAction::Help)));
        assert_eq!(e.feed(b"~x"), (b"~x".to_vec(), None));
    }
}
