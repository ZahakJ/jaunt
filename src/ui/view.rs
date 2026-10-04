//! The full-screen mesh view, as one frame: header, mesh, selected machine, event log,
//! key bar, and the help overlay. Pure drawing; `live.rs` and `demo.rs` drive it.

use super::canvas::Canvas;
use super::mesh::{TreeOpts, draw_tree, link_col, tree_rows};
use super::model::{Mesh, Tone, fmt_age};
use super::theme::{self, Style, glyphs};

#[derive(Default)]
pub struct ViewState {
    pub selected: usize,
    pub offset: usize,
    pub help: bool,
    /// A message for the key bar row, with when it goes away (ms).
    pub toast: Option<(String, Tone, u64)>,
    /// "live", "demo" or "pick".
    pub tag: &'static str,
}

impl ViewState {
    pub fn select(&mut self, delta: i32, n: usize) {
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(n as i32) as usize;
    }
}

/// The header: wordmark, this machine, relay state; counts on the right.
pub fn header(c: &mut Canvas, y: u16, m: &Mesh, tag: &str, now_ms: u64) {
    let g = glyphs();
    let mut x = 1;
    for (i, ch) in "jaunt".chars().enumerate() {
        c.put(x, y, ch, Style::fg(theme::accent(i as f32 / 4.0)).bold());
        x += 1;
    }
    x += 1;
    for (i, ch) in [g.direct, g.direct, g.arrow].iter().enumerate() {
        c.put(x, y, *ch, Style::fg(theme::accent(0.4 + i as f32 * 0.3)));
        x += 1;
    }
    x = c.text(x + 1, y, &m.me, Style::fg(theme::WHITE).bold());

    // Right side first, so the relay text knows how much room it has.
    let up = m.up_count();
    let total = m.peers.len();
    let right = format!("{up}/{total} up");
    let tag_w = if tag.is_empty() { 0 } else { tag.chars().count() as u16 + 3 };
    let rx = c.w.saturating_sub(right.chars().count() as u16 + 1 + tag_w);
    if rx > x + 4 {
        let mut ex = c.text(rx, y, &format!("{up}"), Style::fg(if up > 0 { theme::GREEN } else { theme::DIM }).bold());
        ex = c.text(ex, y, &format!("/{total} up"), Style::fg(theme::DIM));
        if !tag.is_empty() {
            let on = (now_ms / 1000).is_multiple_of(2) || tag != "live";
            c.put(ex + 2, y, if on { g.up } else { g.down }, Style::fg(theme::accent(1.0)));
            c.text(ex + 3, y, &format!(" {tag}"), Style::fg(theme::DIM));
        }
    }
    let room = rx.saturating_sub(x + 4);
    if room < 8 {
        return;
    }
    let (dot, dst, txt) = match (m.relay_host(), m.relay_connected) {
        (Some(h), true) => (g.up, Style::fg(theme::GREEN), h),
        (Some(h), false) => (g.down, Style::fg(theme::RED), format!("{h} unreachable")),
        (None, _) => (g.down, Style::fg(theme::AMBER), "no relay".into()),
    };
    let rx0 = x + 3;
    c.text(rx0, y, "relay", Style::fg(theme::DIM));
    c.put(rx0 + 6, y, dot, dst);
    c.text_fit(rx0 + 8, y, &txt, Style::fg(theme::DIM), room.saturating_sub(10), g.ellipsis);
}

/// The tall header: the wordmark in large type, facts stacked beside it. Returns rows used.
fn header_big(c: &mut Canvas, m: &Mesh, tag: &str, now_ms: u64) -> Option<u16> {
    let g = glyphs();
    let rows = super::bigtext::render("JAUNT", g.half)?;
    let n = rows[0].len().max(2) - 1;
    for (r, row) in rows.iter().enumerate() {
        for (i, ch) in row.iter().enumerate() {
            if *ch != ' ' {
                c.put(2 + i as u16, 1 + r as u16, *ch, Style::fg(theme::accent(i as f32 / n as f32)).bold());
            }
        }
    }
    let x = 2 + rows[0].len() as u16 + 4;
    // Line 1: this machine.
    c.put(x, 1, g.me, Style::fg(theme::accent(0.0)).bold());
    let mut e = c.text(x + 2, 1, &m.me, Style::fg(theme::WHITE).bold());
    if let Some(h) = &m.me_host {
        e = c.text(e + 2, 1, &format!("{} host {h}", g.dot), Style::fg(super::mesh::host_col(h)));
    }
    let _ = e;
    // Line 2: relay.
    let (dot, dst, txt) = match (m.relay_host(), m.relay_connected) {
        (Some(h), true) => (g.up, Style::fg(theme::GREEN), h),
        (Some(h), false) => (g.down, Style::fg(theme::RED), format!("{h} unreachable")),
        (None, _) => (g.down, Style::fg(theme::AMBER), "no relay".into()),
    };
    c.put(x, 2, dot, dst);
    let e = c.text(x + 2, 2, "relay ", Style::fg(theme::DIM));
    c.text_fit(e, 2, &txt, Style::fg(theme::DIM), c.w.saturating_sub(e + 2), g.ellipsis);
    // Line 3: counts.
    let up = m.up_count();
    let direct = m.peers.iter().filter(|p| p.state == crate::proto::PeerState::Direct).count();
    let relayed = m.peers.iter().filter(|p| p.state == crate::proto::PeerState::Relay).count();
    let mut e = c.text(x, 3, &up.to_string(), Style::fg(if up > 0 { theme::GREEN } else { theme::DIM }).bold());
    e = c.text(e, 3, &format!(" of {} up", m.peers.len()), Style::fg(theme::DIM));
    for (n, label, col) in [(direct, "direct", theme::GREEN), (relayed, "relayed", theme::AMBER)] {
        if n > 0 {
            e = c.text(e, 3, &format!("  {}  ", g.dot), Style::fg(theme::FAINT));
            e = c.text(e, 3, &n.to_string(), Style::fg(col).bold());
            e = c.text(e + 1, 3, label, Style::fg(theme::DIM));
        }
    }
    if !tag.is_empty() {
        let on = (now_ms / 1000).is_multiple_of(2) || tag != "live";
        let tx = c.w.saturating_sub(tag.len() as u16 + 4);
        c.put(tx, 1, if on { g.up } else { g.down }, Style::fg(theme::accent(1.0)));
        c.text(tx + 2, 1, tag, Style::fg(theme::DIM));
    }
    Some(5)
}

/// A faint rule with a small dim title: ` ── events ──────`.
pub fn rule(c: &mut Canvas, y: u16, title: &str) {
    let g = glyphs();
    c.hline(1, c.w.saturating_sub(1), y, g.rule, Style::fg(theme::FAINT));
    if !title.is_empty() {
        c.put(3, y, ' ', Style::PLAIN);
        let e = c.text(4, y, title, Style::fg(theme::DIM));
        c.put(e, y, ' ', Style::PLAIN);
    }
}

fn detail(c: &mut Canvas, y: u16, m: &Mesh, sel: usize, now_ms: u64) {
    let g = glyphs();
    let Some(p) = m.peers.get(sel) else { return };
    let dot = format!(" {} ", g.dot);
    let mut parts: Vec<(String, Style)> = vec![(p.id.short(), Style::fg(theme::DIM))];
    if let Some(a) = &p.addr {
        parts.push((a.clone(), Style::fg(theme::DIM)));
    }
    if let Some(v) = &p.version {
        parts.push((format!("v{v}"), Style::fg(theme::DIM)));
    }
    if let Some(s) = p.since_ms.filter(|_| p.up()) {
        parts.push((format!("linked {}", fmt_age(now_ms.saturating_sub(s))), Style::fg(theme::DIM)));
    }
    if let Some(e) = &p.error {
        parts.push((e.clone(), Style::fg(theme::RED)));
    }
    let mut x = c.text(3, y, &p.name, Style::fg(link_col(p.state)).bold());
    x += 1;
    for (i, (t, st)) in parts.iter().enumerate() {
        if i > 0 {
            x = c.text(x, y, &dot, Style::fg(theme::FAINT));
        } else {
            x += 1;
        }
        let room = c.w.saturating_sub(x + 1);
        if room < 4 {
            break;
        }
        x = c.text_fit(x, y, t, *st, room, g.ellipsis);
    }
}

fn events(c: &mut Canvas, y0: u16, rows: u16, m: &Mesh, now_ms: u64) {
    let g = glyphs();
    let name_w = m.peers.iter().map(|p| p.name.len()).max().unwrap_or(4).clamp(4, 12) as u16;
    for (i, l) in m.log.iter().take(rows as usize).enumerate() {
        let y = y0 + i as u16;
        let age = fmt_age(now_ms.saturating_sub(l.t_ms));
        let fade = i as f32 / rows.max(1) as f32;
        let ax = 6u16.saturating_sub(age.len() as u16);
        c.text(ax, y, &age, Style::fg(theme::DIM));
        c.put(8, y, g.up, Style::fg(l.tone.col().mix(theme::FAINT, fade * 0.6)));
        c.text_max(10, y, &l.verb, Style::fg(l.tone.col()), 10);
        let mut x = 21;
        if let Some(p) = &l.peer {
            c.text_max(x, y, p, Style::fg(theme::WHITE).bold(), name_w);
        }
        x += name_w + 2;
        for (t, tone) in &l.detail {
            let room = c.w.saturating_sub(x + 1);
            if room == 0 {
                break;
            }
            let st = if *tone == Tone::Quiet { Style::fg(theme::DIM) } else { Style::fg(tone.col()) };
            x = c.text_fit(x, y, t, st, room, g.ellipsis);
        }
    }
    if m.log.is_empty() && rows > 0 {
        c.text(4, y0, "quiet so far", Style::fg(theme::FAINT));
    }
}

fn keybar(c: &mut Canvas, y: u16, st: &ViewState, now_ms: u64) {
    let g = glyphs();
    if let Some((msg, tone, until)) = &st.toast
        && now_ms < *until
    {
        c.text_fit(2, y, msg, Style::fg(tone.col()), c.w.saturating_sub(3), g.ellipsis);
        return;
    }
    let hop = if st.tag == "pick" { "open" } else { "hop" };
    let keys: [(&str, &str); 5] = [(g.updown, "select"), (g.enter, hop), ("l", "link"), ("?", "help"), ("q", "quit")];
    let mut x = 2;
    for (k, label) in keys {
        let need = (k.chars().count() + label.len() + 3) as u16;
        if x + need > c.w {
            break;
        }
        x = c.text(x, y, k, Style::fg(theme::accent(0.8)).bold());
        x = c.text(x + 1, y, label, Style::fg(theme::DIM));
        x += 3;
    }
}

fn help(c: &mut Canvas, now_ms: u64) {
    let g = glyphs();
    let lines: Vec<(String, &str, Style)> = vec![
        (format!("{} j k", g.updown), "choose a machine", Style::fg(theme::accent(0.8)).bold()),
        (g.enter.to_string(), "hop: its tmux session", Style::fg(theme::accent(0.8)).bold()),
        ("l".into(), "link a new machine", Style::fg(theme::accent(0.8)).bold()),
        ("q".into(), "quit", Style::fg(theme::accent(0.8)).bold()),
        (String::new(), "", Style::PLAIN),
        (g.direct.to_string().repeat(4), "direct path", Style::fg(theme::GREEN)),
        (g.relay.to_string().repeat(4), "through the relay", Style::fg(theme::AMBER)),
        (format!("   {}", g.down), "offline", Style::fg(theme::DIM)),
        (format!("{}{}{}", g.direct, g.direct, g.arrow), "traffic, ping, handshake", Style::fg(theme::WHITE)),
        (format!("{}{}{}", g.spark[1], g.spark[4], g.spark[2]), "latency, last minute", Style::fg(theme::GREEN)),
    ];
    let bw = 40u16.min(c.w.saturating_sub(2));
    let bh = (lines.len() as u16 + 4).min(c.h.saturating_sub(2));
    if bw < 20 || bh < 6 {
        return;
    }
    let x0 = (c.w - bw) / 2;
    let y0 = (c.h - bh) / 2;
    let [tl, tr, bl, br, hz, vt] = g.corner;
    let frame = Style::fg(theme::accent(0.5));
    for y in y0..y0 + bh {
        for x in x0..x0 + bw {
            c.put(x, y, ' ', Style::PLAIN);
        }
        c.put(x0, y, vt, frame);
        c.put(x0 + bw - 1, y, vt, frame);
    }
    c.hline(x0 + 1, x0 + bw - 1, y0, hz, frame);
    c.hline(x0 + 1, x0 + bw - 1, y0 + bh - 1, hz, frame);
    c.put(x0, y0, tl, frame);
    c.put(x0 + bw - 1, y0, tr, frame);
    c.put(x0, y0 + bh - 1, bl, frame);
    c.put(x0 + bw - 1, y0 + bh - 1, br, frame);
    let t = c.text(x0 + 2, y0, " jaunt ", Style::fg(theme::accent(0.0)).bold());
    let _ = t;
    for (i, (k, label, st)) in lines.iter().enumerate() {
        let y = y0 + 2 + i as u16;
        if y >= y0 + bh - 1 {
            break;
        }
        c.text(x0 + 3, y, k, *st);
        c.text_max(x0 + 12, y, label, Style::fg(theme::DIM), bw.saturating_sub(14));
    }
    let _ = now_ms;
}

/// Draw one frame of the mesh view.
pub fn draw(c: &mut Canvas, m: &Mesh, st: &mut ViewState, now_ms: u64) {
    let (w, h) = (c.w, c.h);
    if w < 30 || h < 8 {
        c.text(0, 0, "jaunt: window too small", Style::fg(theme::DIM));
        return;
    }
    let n = m.peers.len();
    if st.selected >= n {
        st.selected = n.saturating_sub(1);
    }
    let big = h >= 30 && w >= 90 && !theme::theme().ascii;
    let top = match big.then(|| header_big(c, m, st.tag, now_ms)).flatten() {
        Some(rows) => rows,
        None => {
            header(c, 0, m, st.tag, now_ms);
            1
        }
    };
    let keys_y = h - 1;

    // Height budget: header, rule 1, mesh, detail 2, events rule 1 + at least 3 lines, keys 1.
    let fixed = top + 1 + 2 + 1 + 3 + 1;
    let room = h.saturating_sub(fixed);
    let gap = tree_rows(n, true) + 2 <= room;
    let pad = if tree_rows(n, gap) + 2 <= room { 1 } else { 0 };
    let tree_h = tree_rows(n, gap).min(room.saturating_sub(2 * pad).max(1));
    let step = if gap { 2 } else { 1 };
    let fit = ((tree_h as usize + usize::from(gap)) / step).max(1);
    if st.selected < st.offset {
        st.offset = st.selected;
    } else if st.selected >= st.offset + fit {
        st.offset = st.selected + 1 - fit;
    }

    rule(c, top, "mesh");
    let ty = top + 1 + pad;
    let opts = TreeOpts { now_ms, selected: (n > 0).then_some(st.selected), gap, offset: st.offset };
    draw_tree(c, 1, ty, w - 2, tree_h, m, &opts);
    let (up, down) = if theme::theme().ascii { ('^', 'v') } else { ('↑', '↓') };
    if st.offset > 0 {
        c.put(w - 3, ty, up, Style::fg(theme::DIM));
    }
    if st.offset + fit < n {
        c.put(w - 3, ty + tree_h - 1, down, Style::fg(theme::DIM));
    }
    let dy = ty + tree_h + pad;
    detail(c, dy, m, st.selected, now_ms);
    let ey = dy + 2;
    rule(c, ey, "events");
    events(c, ey + 1, keys_y.saturating_sub(ey + 1), m, now_ms);
    keybar(c, keys_y, st, now_ms);
    if st.help {
        help(c, now_ms);
    }
}
