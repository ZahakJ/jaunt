//! Keys from a raw-mode stdin, read on a thread that polls with a timeout, so it can be
//! stopped before a hop takes over stdin (a blocked `read` would steal the first key).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    CtrlC,
    Backspace,
    Tab,
    /// Ctrl-A
    All,
    Char(char),
}

pub struct Keys {
    pub rx: mpsc::Receiver<Key>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Keys {
    pub fn start() -> Keys {
        let (tx, rx) = mpsc::channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || read_loop(tx, s));
        Keys { rx, stop, thread: Some(thread) }
    }

    /// Stop the reader and wait for it (at most one poll interval).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_loop(tx: mpsc::Sender<Key>, stop: Arc<AtomicBool>) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdin = rustix::stdio::stdin();
    let timeout = Timespec { tv_sec: 0, tv_nsec: 50_000_000 };
    let mut buf = [0u8; 64];
    while !stop.load(Ordering::Relaxed) {
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
            && !fds[0].revents().contains(PollFlags::IN)
        {
            return;
        }
        let n = match rustix::io::read(stdin, &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for k in parse(&buf[..n]) {
            if tx.blocking_send(k).is_err() {
                return;
            }
        }
    }
}

/// Decode one read's worth of bytes. Unknown sequences are dropped.
pub fn parse(b: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            0x1b if i + 2 < b.len() && (b[i + 1] == b'[' || b[i + 1] == b'O') => {
                match b[i + 2] {
                    b'A' => keys.push(Key::Up),
                    b'B' => keys.push(Key::Down),
                    _ => {}
                }
                // Skip the rest of a CSI sequence (parameters end at a byte in 0x40..0x7e).
                let mut j = i + 2;
                while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            0x1b => keys.push(Key::Esc),
            b'\r' | b'\n' => keys.push(Key::Enter),
            3 => keys.push(Key::CtrlC),
            0x7f | 0x08 => keys.push(Key::Backspace),
            b'\t' => keys.push(Key::Tab),
            1 => keys.push(Key::All),
            c if c.is_ascii_graphic() || c == b' ' => keys.push(Key::Char(c as char)),
            _ => {}
        }
        i += 1;
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(parse(b"\x1b[Ak\x1bOB\r"), vec![Key::Up, Key::Char('k'), Key::Down, Key::Enter]);
        assert_eq!(parse(b"\x1b"), vec![Key::Esc]);
        assert_eq!(parse(b"\x1b[1;5C\x03"), vec![Key::CtrlC]);
        assert_eq!(parse(b"\x00\x7f\t\x01"), vec![Key::Backspace, Key::Tab, Key::All]);
    }
}
