//! A small full-screen picker on the alternate screen. Two modes:
//! - fuzzy: type to filter, arrows to move, Tab to mark, Ctrl-A to mark all shown, Enter
//!   to take the marked rows (or the one under the cursor), Esc to cancel;
//! - hotkeys: each row has a key; pressing it takes that row at once.

use std::io::Write;

use anyhow::Result;

use super::input::{Key, Keys};
use super::theme::{self, Style, paint};

pub struct Row {
    /// Matched against the query.
    pub text: String,
    /// What is shown after the text, dimmed (not matched).
    pub detail: String,
}

pub const HOTKEYS: &[u8] = b"123456789abcdefghijklmnoprstuvwxyz";

/// Subsequence match, case-insensitive. Lower is better; `None` = no match.
pub fn score(query: &str, text: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut gaps = 0;
    let mut at = 0;
    let mut first = None;
    for q in query.to_lowercase().chars() {
        if q == ' ' {
            continue;
        }
        let found = text[at..].iter().position(|c| *c == q)?;
        if first.is_none() {
            first = Some(at + found);
        } else {
            gaps += found;
        }
        at += found + 1;
    }
    Some(gaps * 4 + first.unwrap_or(0))
}

fn filter(rows: &[Row], query: &str) -> Vec<usize> {
    let mut hits: Vec<(usize, usize)> = rows.iter().enumerate().filter_map(|(i, r)| score(query, &r.text).map(|s| (s, i))).collect();
    hits.sort();
    hits.into_iter().map(|(_, i)| i).collect()
}

fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Returns the chosen rows (empty if cancelled).
pub async fn pick(title: &str, rows: &[Row], hotkeys: bool) -> Result<Vec<usize>> {
    let _raw = crate::term::RawMode::enable()?;
    let mut out = std::io::stdout();
    write!(out, "\x1b[?1049h\x1b[?25l")?;
    let mut keys = Keys::start();
    let r = run(&mut out, &mut keys, title, rows, hotkeys).await;
    keys.stop();
    write!(out, "\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    r
}

async fn run(out: &mut impl Write, keys: &mut Keys, title: &str, rows: &[Row], hotkeys: bool) -> Result<Vec<usize>> {
    let mut query = String::new();
    let mut shown = filter(rows, "");
    let mut cursor = 0usize;
    let mut top = 0usize;
    let mut marked = vec![false; rows.len()];
    loop {
        let (cols, lines) = crate::term::size();
        let (w, h) = (cols as usize, (lines as usize).saturating_sub(3).max(1));
        cursor = cursor.min(shown.len().saturating_sub(1));
        if cursor < top {
            top = cursor;
        } else if cursor >= top + h {
            top = cursor + 1 - h;
        }
        let mut frame = String::from("\x1b[H\x1b[2J");
        let head = if hotkeys { format!("{title}  ") } else { format!("{title} › {query}") };
        frame.push_str(&paint(&clip(&head, w), Style::fg(theme::VIOLET).bold()));
        frame.push_str("\r\n");
        for (line, &i) in shown.iter().enumerate().skip(top).take(h) {
            let mark = if hotkeys {
                HOTKEYS.get(line).map(|k| format!("{} ", *k as char)).unwrap_or_else(|| "  ".into())
            } else if marked[i] {
                "● ".into()
            } else {
                "  ".into()
            };
            let text = clip(&rows[i].text, w.saturating_sub(3));
            let room = w.saturating_sub(3 + text.chars().count() + 2);
            let detail = if room > 8 { format!("  {}", clip(&rows[i].detail, room)) } else { String::new() };
            let sel = line == cursor;
            let style = Style { sel, ..Style::fg(theme::WHITE) };
            frame.push_str(&paint(&mark, Style::fg(theme::GREEN)));
            frame.push_str(&paint(&text, style));
            frame.push_str(&paint(&detail, Style { sel, ..Style::fg(theme::DIM) }));
            frame.push_str("\r\n");
        }
        let n_marked = marked.iter().filter(|m| **m).count();
        let help = if hotkeys {
            "key or Enter opens · Esc quits".to_string()
        } else {
            format!("{}/{} · Tab marks ({n_marked}) · Ctrl-A marks all · Enter opens · Esc quits", shown.len(), rows.len())
        };
        frame.push_str(&format!("\x1b[{};1H", lines));
        frame.push_str(&paint(&clip(&help, w), Style::fg(theme::DIM)));
        out.write_all(frame.as_bytes())?;
        out.flush()?;

        let Some(k) = keys.rx.recv().await else { return Ok(Vec::new()) };
        match k {
            Key::Esc | Key::CtrlC => return Ok(Vec::new()),
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(shown.len().saturating_sub(1)),
            Key::Enter => {
                let chosen: Vec<usize> = (0..rows.len()).filter(|i| marked[*i]).collect();
                if !chosen.is_empty() {
                    return Ok(chosen);
                }
                return Ok(shown.get(cursor).map(|i| vec![*i]).unwrap_or_default());
            }
            Key::Char(c) if hotkeys => {
                if let Some(line) = HOTKEYS.iter().position(|k| *k as char == c)
                    && let Some(i) = shown.get(top + line)
                {
                    return Ok(vec![*i]);
                }
                if c == 'q' {
                    return Ok(Vec::new());
                }
            }
            Key::Tab if !hotkeys => {
                if let Some(&i) = shown.get(cursor) {
                    marked[i] = !marked[i];
                    cursor = (cursor + 1).min(shown.len().saturating_sub(1));
                }
            }
            Key::All if !hotkeys => {
                let all = shown.iter().all(|i| marked[*i]);
                for &i in &shown {
                    marked[i] = !all;
                }
            }
            Key::Backspace if !hotkeys => {
                query.pop();
                shown = filter(rows, &query);
            }
            Key::Char(c) => {
                query.push(c);
                shown = filter(rows, &query);
                cursor = 0;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_order() {
        assert_eq!(score("", "x"), Some(0));
        assert!(score("gh", "GitHub").is_some());
        assert!(score("xyz", "GitHub").is_none());
        // Contiguous beats scattered.
        assert!(score("hub", "GitHub") < score("hub", "h-u-b"));
        let rows = ["rust docs", "github issues", "hacker news"].map(|t| Row { text: t.into(), detail: String::new() });
        assert_eq!(filter(&rows, "hn"), vec![2]);
        assert_eq!(filter(&rows, "is ue"), vec![1]);
    }
}
