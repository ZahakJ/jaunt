//! Presentation seam. Stage 1 renders plain text only; the UI work (theme, glyphs, mesh
//! view, pretty `ls`, banners) replaces the bodies here without touching callers.
//!
//! Contract for callers:
//! - `ls_plain` must stay stable, tab-separated, escape-free (it is the piped format).
//! - the other functions return one line, no trailing newline; callers add `\r\n` in raw mode.

use std::io::Write;

use crate::proto::{PathKind, PeerState, StatusSnapshot};

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
            p.host.as_deref().unwrap_or("-"),
            p.sessions.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            p.id
        )?;
    }
    Ok(())
}

/// `jaunt ls` on a terminal. Placeholder: the UI stage draws the real block.
pub fn ls_pretty(s: &StatusSnapshot, out: &mut impl Write) -> std::io::Result<()> {
    ls_plain(s, out)
}

/// On the way in: `forge ━━▸ fern · direct · 0.8 ms`.
pub fn hop_banner_in(me: &str, peer: &str, path: PathKind, rtt_ms: Option<f64>) -> String {
    format!("{me} -> {peer} · {} · {}", path.as_str(), fmt_rtt(rtt_ms))
}

/// On the way out: `◂━━ back on forge · session kept on fern`.
pub fn hop_banner_out(me: &str, peer: &str, session_kept: bool) -> String {
    if session_kept { format!("<- back on {me} · session kept on {peer}") } else { format!("<- back on {me}") }
}

/// The single status line while the link is down.
pub fn link_lost_line(peer: &str, retry_in_s: u64) -> String {
    format!("{peer} ╌ link lost · retrying in {retry_in_s}s · ~. to give up")
}
