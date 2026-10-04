//! Presentation: theme and glyphs, the mesh view, `ls`, the hop banners, the pairing
//! ritual and the demo. Contract for callers:
//! - `ls_plain` stays stable, tab-separated, escape-free (it is the piped format).
//! - the banner functions return one line, no trailing newline; callers add `\r\n` in raw mode.

pub(crate) mod bigtext;
mod canvas;
pub mod demo;
mod input;
pub mod live;
mod ls;
pub(crate) mod mesh;
mod model;
pub mod ritual;
pub mod theme;
pub mod tmux;
mod tui;
mod view;

use std::io::Write;

use crate::proto::{PathKind, PeerState, StatusSnapshot};
use theme::{Style, glyphs, paint};

fn fmt_rtt(ms: Option<f64>) -> String {
    match ms {
        Some(v) if v < 10.0 => format!("{v:.1}ms"),
        Some(v) => format!("{v:.0}ms"),
        None => "-".into(),
    }
}

/// `jaunt ls` when piped: one line per machine, this machine first:
/// `name  state  rtt  host  sessions  id` (tab-separated; `-` for unknown).
pub fn ls_plain(s: &StatusSnapshot, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{}\tself\t-\t-\t-\t{}", s.me.name, s.me.id)?;
    for p in &s.peers {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            p.name,
            p.state.as_str(),
            if matches!(p.state, PeerState::Direct | PeerState::Relay) { fmt_rtt(p.rtt_ms) } else { "-".into() },
            p.host.as_deref().map(canvas::clean).unwrap_or_else(|| "-".into()),
            p.sessions.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            p.id
        )?;
    }
    Ok(())
}

/// `jaunt ls` on a terminal: the mesh as a static block.
pub fn ls_pretty(s: &StatusSnapshot, out: &mut impl Write) -> std::io::Result<()> {
    let mut m = model::Mesh::default();
    m.apply_status(s);
    ls::print(&m, out)
}

fn path_style(path: PathKind) -> (char, Style) {
    let g = glyphs();
    match path {
        PathKind::Direct => (g.direct, Style::fg(theme::GREEN)),
        PathKind::Relay => (g.relay, Style::fg(theme::AMBER)),
        PathKind::Unknown => (g.connecting, Style::fg(theme::DIM)),
    }
}

/// On the way in: `forge ━━▸ fern · direct · 0.8 ms`.
pub fn hop_banner_in(me: &str, peer: &str, path: PathKind, rtt_ms: Option<f64>) -> String {
    let g = glyphs();
    let (line, st) = path_style(path);
    let dot = paint(&format!(" {} ", g.dot), Style::fg(theme::FAINT));
    let mut s = format!(
        "{} {}{} {}{dot}{}",
        paint(&canvas::clean(me), Style::fg(theme::DIM)),
        paint(&format!("{line}{line}"), st),
        paint(&g.arrow.to_string(), st.bold()),
        paint(&canvas::clean(peer), Style::fg(theme::accent(1.0)).bold()),
        paint(path.as_str(), st),
    );
    if let Some(v) = rtt_ms {
        s.push_str(&dot);
        s.push_str(&paint(&model::fmt_rtt(v), Style::fg(theme::WHITE)));
    }
    s
}

/// On the way out: `◂━━ back on forge · session kept on fern`.
pub fn hop_banner_out(me: &str, peer: &str, session_kept: bool) -> String {
    let g = glyphs();
    let head = format!(
        "{} back on {}",
        paint(&format!("{}{}{}", g.back, g.direct, g.direct), Style::fg(theme::accent(0.0))),
        paint(&canvas::clean(me), Style::fg(theme::accent(0.0)).bold())
    );
    if session_kept {
        let dot = paint(&format!(" {} ", g.dot), Style::fg(theme::FAINT));
        format!("{head}{dot}{}", paint(&format!("session kept on {}", canvas::clean(peer)), Style::fg(theme::DIM)))
    } else {
        head
    }
}

/// The single status line while the link is down.
/// H3: the session is up but nothing has come back for a while (a roam, a dead Wi-Fi).
pub fn stalled_line(peer: &str, quiet_s: u64) -> String {
    let g = glyphs();
    let dot = paint(&format!(" {} ", g.dot), Style::fg(theme::FAINT));
    format!(
        "{} {} {}{dot}{}{dot}{}",
        paint(&canvas::clean(peer), Style::fg(theme::WHITE).bold()),
        paint(&g.relay.to_string(), Style::fg(theme::AMBER)),
        paint(&format!("no answer for {quiet_s}s"), Style::fg(theme::AMBER).bold()),
        paint("waiting for the link", Style::fg(theme::DIM)),
        paint("~. to give up", Style::fg(theme::DIM)),
    )
}

pub fn link_lost_line(peer: &str, retry_in_s: u64) -> String {
    let g = glyphs();
    let dot = paint(&format!(" {} ", g.dot), Style::fg(theme::FAINT));
    format!(
        "{} {} {}{dot}{}{dot}{}",
        paint(&canvas::clean(peer), Style::fg(theme::WHITE).bold()),
        paint(&g.relay.to_string(), Style::fg(theme::AMBER)),
        paint("link lost", Style::fg(theme::AMBER).bold()),
        paint(&format!("retrying in {retry_in_s}s"), Style::fg(theme::DIM)),
        paint("~. to give up", Style::fg(theme::DIM)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners_carry_the_facts() {
        // Colour depends on the environment; the words must be there either way.
        let b = hop_banner_in("forge", "fern", PathKind::Direct, Some(0.84));
        assert!(b.contains("forge") && b.contains("fern") && b.contains("direct") && b.contains("0.8 ms"));
        let b = hop_banner_out("forge", "fern", true);
        assert!(b.contains("back on") && b.contains("session kept on"));
        assert!(link_lost_line("fern\x1b", 4).contains("fern?"));
    }
}
