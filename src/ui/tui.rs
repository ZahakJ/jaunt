//! The full-screen terminal: enter/leave (alternate screen, raw mode, key reader), one
//! frame per draw, and the key bindings shared by the live view and the demo.

use std::io::Write;

use super::canvas::{Canvas, Screen};
use super::input::{Key, Keys};
use super::model::Mesh;
use super::theme;
use super::view::{self, ViewState};
use crate::term::RawMode;

pub struct Tui {
    screen: Screen,
    on: Option<(RawMode, Keys)>,
}

pub enum Action {
    None,
    Quit,
    Hop(usize),
    Link,
}

impl Tui {
    pub fn new() -> Tui {
        Tui { screen: Screen::new(theme::theme().mode), on: None }
    }

    pub fn enter(&mut self) -> anyhow::Result<()> {
        if self.on.is_none() {
            let raw = RawMode::enable()?;
            self.screen.enter(&mut std::io::stdout())?;
            self.on = Some((raw, Keys::start()));
        }
        Ok(())
    }

    /// Give the terminal back (before a hop, the link ritual, or on exit).
    pub fn leave(&mut self) {
        if let Some((raw, mut keys)) = self.on.take() {
            keys.stop();
            let _ = self.screen.leave(&mut std::io::stdout());
            drop(raw);
        }
    }

    pub async fn key(&mut self) -> Option<Key> {
        match self.on.as_mut() {
            Some((_, k)) => k.rx.recv().await,
            None => std::future::pending().await,
        }
    }

    pub fn resized(&mut self) {
        self.screen.invalidate();
    }

    pub fn draw(&mut self, m: &Mesh, st: &mut ViewState, now_ms: u64) {
        if self.on.is_none() {
            return;
        }
        let (w, h) = crate::term::size();
        let mut c = Canvas::new(w, h);
        view::draw(&mut c, m, st, now_ms);
        let mut out = std::io::stdout().lock();
        let _ = self.screen.draw(&c, &mut out);
        let _ = out.flush();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.leave();
    }
}

pub fn handle_key(st: &mut ViewState, key: Key, n: usize) -> Action {
    if st.help {
        st.help = false;
        return match key {
            Key::CtrlC | Key::Char('q') => Action::Quit,
            _ => Action::None,
        };
    }
    match key {
        Key::Char('q') | Key::Esc | Key::CtrlC => Action::Quit,
        Key::Char('j') | Key::Down => {
            st.select(1, n);
            Action::None
        }
        Key::Char('k') | Key::Up => {
            st.select(-1, n);
            Action::None
        }
        Key::Char('g') => {
            st.selected = 0;
            Action::None
        }
        Key::Char('G') => {
            st.selected = n.saturating_sub(1);
            Action::None
        }
        Key::Enter if n > 0 => Action::Hop(st.selected),
        Key::Char('l') => Action::Link,
        Key::Char('?') | Key::Char('h') => {
            st.help = true;
            Action::None
        }
        _ => Action::None,
    }
}

/// Wait for one key on a cooked terminal (after a hop or the ritual).
pub async fn wait_key(prompt: &str) {
    let _ = write!(std::io::stdout(), "\r\n{}", theme::paint(prompt, theme::Style::fg(theme::DIM)));
    let _ = std::io::stdout().flush();
    if let Ok(_raw) = RawMode::enable() {
        let mut keys = Keys::start();
        let _ = keys.rx.recv().await;
        keys.stop();
    }
}
