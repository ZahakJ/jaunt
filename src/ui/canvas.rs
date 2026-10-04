//! A grid of styled cells, and the two ways it reaches the terminal: as plain lines
//! (static output) or as a diff against the previous frame (the live views, no flicker).

use std::io::Write;

use super::theme::{ColorMode, Style, sgr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub st: Style,
}

const BLANK: Cell = Cell { ch: ' ', st: Style::PLAIN };

#[derive(Clone)]
pub struct Canvas {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
}

/// Strings that came from the network or the daemon: printable ASCII only, so every
/// character is exactly one column and no escape sequence can reach the terminal.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).collect()
}

impl Canvas {
    pub fn new(w: u16, h: u16) -> Canvas {
        Canvas { w, h, cells: vec![BLANK; w as usize * h as usize] }
    }

    pub fn get(&self, x: u16, y: u16) -> Option<Cell> {
        (x < self.w && y < self.h).then(|| self.cells[y as usize * self.w as usize + x as usize])
    }

    pub fn put(&mut self, x: u16, y: u16, ch: char, st: Style) {
        if x < self.w && y < self.h {
            let control = (ch as u32) < 0x20 || ch == '\x7f';
            self.cells[y as usize * self.w as usize + x as usize] = Cell { ch: if control { '?' } else { ch }, st };
        }
    }

    /// Restyle a cell without changing its character.
    pub fn style_at(&mut self, x: u16, y: u16, f: impl Fn(Style) -> Style) {
        if x < self.w && y < self.h {
            let c = &mut self.cells[y as usize * self.w as usize + x as usize];
            c.st = f(c.st);
        }
    }

    /// Write `s` from `x`, clipped at `x + max` and at the right edge; returns the next x.
    pub fn text_max(&mut self, x: u16, y: u16, s: &str, st: Style, max: u16) -> u16 {
        let mut cx = x;
        for ch in s.chars() {
            if cx >= self.w || cx - x >= max {
                break;
            }
            self.put(cx, y, ch, st);
            cx += 1;
        }
        cx
    }

    pub fn text(&mut self, x: u16, y: u16, s: &str, st: Style) -> u16 {
        self.text_max(x, y, s, st, u16::MAX)
    }

    /// Like [`text`], but ends with `…` when `s` does not fit in `max` columns.
    pub fn text_fit(&mut self, x: u16, y: u16, s: &str, st: Style, max: u16, ellipsis: char) -> u16 {
        let n = s.chars().count();
        if n <= max as usize {
            return self.text(x, y, s, st);
        }
        if max == 0 {
            return x;
        }
        let head: String = s.chars().take(max as usize - 1).collect();
        let nx = self.text(x, y, &head, st);
        self.put(nx, y, ellipsis, st);
        nx + 1
    }

    pub fn hline(&mut self, x0: u16, x1: u16, y: u16, ch: char, st: Style) {
        for x in x0..x1 {
            self.put(x, y, ch, st);
        }
    }

    /// Each row as a string with SGR codes, trailing blanks dropped.
    pub fn lines(&self, mode: ColorMode) -> Vec<String> {
        (0..self.h).map(|y| self.row_string(y, 0, self.w, mode, true)).collect()
    }

    fn row_string(&self, y: u16, x0: u16, x1: u16, mode: ColorMode, trim: bool) -> String {
        let row = &self.cells[y as usize * self.w as usize..(y as usize + 1) * self.w as usize];
        let mut end = x1 as usize;
        if trim {
            while end > x0 as usize && row[end - 1] == BLANK {
                end -= 1;
            }
        }
        let mut out = String::new();
        let mut cur = String::new();
        for c in &row[x0 as usize..end] {
            let want = sgr(c.st, mode);
            if want != cur {
                if !cur.is_empty() {
                    out.push_str("\x1b[0m");
                }
                out.push_str(&want);
                cur = want;
            }
            out.push(c.ch);
        }
        if !cur.is_empty() {
            out.push_str("\x1b[0m");
        }
        out
    }
}

/// Full-screen output on the alternate screen: only changed cells are written, inside
/// a synchronized update, so frames never tear or flicker.
pub struct Screen {
    prev: Option<Canvas>,
    mode: ColorMode,
}

impl Screen {
    pub fn new(mode: ColorMode) -> Screen {
        Screen { prev: None, mode }
    }

    pub fn enter(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        self.prev = None;
        out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J")?;
        out.flush()
    }

    pub fn leave(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        self.prev = None;
        out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
        out.flush()
    }

    /// Forget the previous frame (after a resize): the next draw repaints everything.
    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    pub fn draw(&mut self, c: &Canvas, out: &mut impl Write) -> std::io::Result<()> {
        let mut buf = String::from("\x1b[?2026h");
        let full = self.prev.as_ref().is_none_or(|p| p.w != c.w || p.h != c.h);
        if full {
            buf.push_str("\x1b[0m\x1b[2J");
        }
        for y in 0..c.h {
            let (mut first, mut last) = (None, 0);
            for x in 0..c.w {
                let changed = full || self.prev.as_ref().and_then(|p| p.get(x, y)) != c.get(x, y);
                if changed {
                    first.get_or_insert(x);
                    last = x;
                }
            }
            if let Some(f) = first {
                buf.push_str(&format!("\x1b[{};{}H", y + 1, f + 1));
                buf.push_str(&c.row_string(y, f, last + 1, self.mode, false));
            }
        }
        buf.push_str("\x1b[?2026l");
        out.write_all(buf.as_bytes())?;
        out.flush()?;
        self.prev = Some(c.clone());
        Ok(())
    }
}

/// A block of lines redrawn in place below the cursor (the pairing ritual): no
/// alternate screen, so the result stays in the scrollback.
pub struct Inline {
    lines: u16,
    mode: ColorMode,
}

impl Inline {
    pub fn new(mode: ColorMode) -> Inline {
        Inline { lines: 0, mode }
    }

    pub fn draw(&mut self, c: &Canvas, out: &mut impl Write) -> std::io::Result<()> {
        let mut buf = String::from("\x1b[?2026h\r");
        if self.lines > 1 {
            buf.push_str(&format!("\x1b[{}A", self.lines - 1));
        }
        for (i, l) in c.lines(self.mode).iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(l);
            buf.push_str("\x1b[K");
        }
        // Lines left over from a taller previous frame.
        for _ in c.h..self.lines {
            buf.push_str("\r\n\x1b[2K");
        }
        if self.lines > c.h {
            buf.push_str(&format!("\x1b[{}A", self.lines - c.h));
        }
        buf.push_str("\x1b[?2026l");
        out.write_all(buf.as_bytes())?;
        out.flush()?;
        self.lines = c.h;
        Ok(())
    }

    /// Leave the cursor on a fresh line below the block.
    pub fn finish(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.lines > 0 {
            out.write_all(b"\r\n")?;
        }
        self.lines = 0;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{GREEN, Style};

    #[test]
    fn lines_trim_and_reset() {
        let mut c = Canvas::new(10, 2);
        c.text(1, 0, "ab", Style::fg(GREEN));
        let l = c.lines(ColorMode::Ansi16);
        assert_eq!(l[0], " \x1b[92mab\x1b[0m");
        assert_eq!(l[1], "");
        assert_eq!(c.lines(ColorMode::None)[0], " ab");
    }

    #[test]
    fn clipping_and_cleaning() {
        let mut c = Canvas::new(4, 1);
        let x = c.text(2, 0, "abcdef", Style::PLAIN);
        assert_eq!(x, 4);
        c.text(0, 0, "\x1b", Style::PLAIN);
        assert_eq!(c.get(0, 0).map(|c| c.ch), Some('?'));
        assert_eq!(clean("fe\x1b[2Jrn\u{202e}"), "fe?[2Jrn?");
        let mut c = Canvas::new(10, 1);
        c.text_fit(0, 0, "abcdefgh", Style::PLAIN, 5, '~');
        assert_eq!(c.lines(ColorMode::None)[0], "abcd~");
    }
}
