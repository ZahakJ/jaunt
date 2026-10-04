//! `jaunt ls` on a terminal: the mesh as a static block in the same visual language as
//! the live view, sized to the terminal and printed once.

use std::io::Write;

use super::canvas::Canvas;
use super::mesh::{TreeOpts, draw_tree, tree_rows};
use super::model::Mesh;
use super::theme::{self, Style, glyphs};
use super::view::header;
use crate::proto::PeerState;

pub fn render(m: &Mesh, width: u16, now_ms: u64) -> Canvas {
    let g = glyphs();
    let w = width.clamp(40, 110);
    let gap = m.peers.len() <= 8;
    let rows = tree_rows(m.peers.len(), gap);
    let mut c = Canvas::new(w, rows + 5);
    header(&mut c, 0, m, "", 0, now_ms);
    let opts = TreeOpts { now_ms, selected: None, gap, offset: 0 };
    draw_tree(&mut c, 1, 2, w - 2, rows, m, &opts);

    let count = |s: PeerState| m.peers.iter().filter(|p| p.state == s).count();
    let parts = [
        (count(PeerState::Direct), "direct", theme::GREEN),
        (count(PeerState::Relay), "relayed", theme::AMBER),
        (count(PeerState::Connecting), "dialing", theme::DIM),
        (count(PeerState::Offline), "offline", theme::DIM),
        (m.revoked, "revoked", theme::RED),
    ];
    let y = rows + 3;
    let mut x = 2;
    let mut first = true;
    for (n, label, col) in parts {
        if n == 0 {
            continue;
        }
        if !first {
            x = c.text(x, y, &format!(" {} ", g.dot), Style::fg(theme::FAINT));
        }
        first = false;
        x = c.text(x, y, &n.to_string(), Style::fg(col).bold());
        x = c.text(x + 1, y, label, Style::fg(theme::DIM));
    }
    c
}

pub fn print(m: &Mesh, out: &mut impl Write) -> std::io::Result<()> {
    let c = render(m, theme::width().saturating_sub(1), crate::roster::now_ms());
    for l in c.lines(theme::theme().mode) {
        writeln!(out, "{l}")?;
    }
    Ok(())
}
