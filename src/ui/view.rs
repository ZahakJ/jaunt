//! The full-screen mesh view, as one frame: header, mesh, selected machine, event log,
//! key bar, and the help overlay. Pure drawing; `live.rs` and `demo.rs` drive it.

use super::canvas::Canvas;
use super::mesh::{TreeOpts, draw_tree, host_col, host_label, link_col, path_word, sess_text, tree_rows};
use super::model::{LineKind, Mesh, PeerView, Tone, fmt_age, fmt_ago, fmt_rtt};
use super::theme::{self, Col, Style, glyphs};
use crate::proto::{PathKind, PeerState};

#[derive(Default)]
pub struct ViewState {
    pub selected: usize,
    pub offset: usize,
    pub help: bool,
    /// A message for the key bar row, with when it goes away (ms).
    pub toast: Option<(String, Tone, u64)>,
    /// "live", "demo" or "pick".
    pub tag: &'static str,
    /// When the daemon last sent something (the tag's dot brightens; no blinking).
    pub beat_ms: u64,
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
pub fn header(c: &mut Canvas, y: u16, m: &Mesh, tag: &str, beat_ms: u64, now_ms: u64) {
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
            c.put(ex + 2, y, g.up, Style::fg(tag_col(tag, beat_ms, now_ms)));
            c.text(ex + 3, y, &format!(" {tag}"), Style::fg(theme::DIM));
        }
    }
    let room = rx.saturating_sub(x + 4);
    if room < 7 {
        return;
    }
    let rx0 = x + 3;
    c.text(rx0, y, "relay", Style::fg(theme::DIM));
    let (dot, dst, txt) = relay_state(m, room.saturating_sub(9));
    c.put(rx0 + 6, y, dot, dst);
    c.text(rx0 + 8, y, &txt, Style::fg(theme::DIM));
}

/// The tag's dot: steady, brighter for a moment when the daemon sent something.
fn tag_col(tag: &str, beat_ms: u64, now_ms: u64) -> Col {
    let fresh = tag != "live" && tag != "pick" || now_ms.saturating_sub(beat_ms) < 400;
    if fresh { theme::accent(1.0) } else { theme::accent(1.0).mix(theme::DIM, 0.45) }
}

/// The relay's dot and its host name if that fits in `room` whole (never cut mid-word).
fn relay_state(m: &Mesh, room: u16) -> (char, Style, String) {
    let g = glyphs();
    let fit = |cands: Vec<String>| cands.into_iter().find(|t| t.chars().count() as u16 <= room).unwrap_or_default();
    match (m.relay_host(), m.relay_connected) {
        (Some(h), up) => {
            if up {
                (g.up, Style::fg(theme::GREEN), fit(vec![h.clone()]))
            } else {
                (g.down, Style::fg(theme::RED), fit(vec![format!("{h} unreachable"), "unreachable".into()]))
            }
        }
        (None, _) => (g.down, Style::fg(theme::AMBER), fit(vec!["no relay".into()])),
    }
}

/// The tall header: the wordmark in large type, facts stacked beside it. Returns rows used.
fn header_big(c: &mut Canvas, m: &Mesh, tag: &str, beat_ms: u64, now_ms: u64) -> Option<u16> {
    let g = glyphs();
    let rows = super::bigtext::wordmark(g.half);
    let n = rows[0].len().max(2) - 1;
    for (r, row) in rows.iter().enumerate() {
        for (i, ch) in row.iter().enumerate() {
            if *ch != ' ' {
                // A smooth gradient; in 16 colours the switch falls between letters (7 columns each).
                let mut col = theme::accent(i as f32 / n as f32);
                col.ansi = theme::accent((i / 7) as f32 / 4.0).ansi;
                c.put(2 + i as u16, 1 + r as u16, *ch, Style::fg(col).bold());
            }
        }
    }
    let x = 2 + rows[0].len() as u16 + 4;
    // Line 1: this machine.
    c.put(x, 1, g.me, Style::fg(theme::accent(0.0)).bold());
    let e = c.text(x + 2, 1, &m.me, Style::fg(theme::WHITE).bold());
    if let Some(h) = &m.me_host {
        c.text(e + 2, 1, &format!("{} host {h}", g.dot), Style::fg(super::mesh::host_col(h)));
    }
    // Line 2: relay.
    let e = c.text(x + 2, 2, "relay ", Style::fg(theme::DIM));
    let (dot, dst, txt) = relay_state(m, c.w.saturating_sub(e + 2));
    c.put(x, 2, dot, dst);
    c.text(e, 2, &txt, Style::fg(theme::DIM));
    // Line 3: counts.
    let up = m.up_count();
    let direct = m.peers.iter().filter(|p| p.state == PeerState::Direct).count();
    let relayed = m.peers.iter().filter(|p| p.state == PeerState::Relay).count();
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
        let tx = c.w.saturating_sub(tag.len() as u16 + 4);
        c.put(tx, 1, g.up, Style::fg(tag_col(tag, beat_ms, now_ms)));
        c.text(tx + 2, 1, tag, Style::fg(theme::DIM));
    }
    Some(5)
}

/// A faint rule with a small dim title: ` ── events ──────`.
pub fn rule(c: &mut Canvas, y: u16, title: &str) {
    rule_at(c, 1, c.w.saturating_sub(1), y, title, Style::fg(theme::DIM));
}

fn rule_at(c: &mut Canvas, x0: u16, x1: u16, y: u16, title: &str, st: Style) {
    let g = glyphs();
    c.hline(x0, x1, y, g.rule, Style::fg(theme::FAINT));
    if !title.is_empty() {
        c.put(x0 + 2, y, ' ', Style::PLAIN);
        let e = c.text_max(x0 + 3, y, title, st, x1.saturating_sub(x0 + 5));
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
        if room < 4 || (t.chars().count() as u16) > room {
            // Whole facts only: an address cut in half is worse than none.
            break;
        }
        x = c.text(x, y, t, *st);
    }
}

fn events(c: &mut Canvas, x1: u16, y0: u16, rows: u16, m: &Mesh, now_ms: u64) {
    let g = glyphs();
    let name_w = m.log.iter().take(rows as usize).map(|l| l.who().len()).chain(m.peers.iter().map(|p| p.name.len())).max().unwrap_or(4).clamp(4, 12) as u16;
    for (i, l) in m.log.iter().take(rows as usize).enumerate() {
        let y = y0 + i as u16;
        let age = fmt_age(now_ms.saturating_sub(l.t_ms));
        let fade = i as f32 / rows.max(1) as f32;
        // Routine lines (sync in step, a link still settling) are drawn quietly.
        let quiet = matches!(l.kind, LineKind::Routine) || l.tone == Tone::Quiet;
        let ax = 6u16.saturating_sub(age.len() as u16);
        c.text(ax, y, &age, Style::fg(theme::DIM));
        c.put(8, y, if quiet { g.dot } else { g.up }, Style::fg(l.tone.col().mix(theme::FAINT, fade * 0.6)));
        c.text_max(10, y, &l.verb, Style::fg(if quiet { theme::DIM } else { l.tone.col() }), 10);
        let mut x = 21;
        let who = l.who();
        if !who.is_empty() {
            let st = if quiet { Style::fg(theme::DIM) } else { Style::fg(theme::WHITE).bold() };
            c.text_fit(x, y, &who, st, name_w, g.ellipsis);
        }
        x += name_w + 2;
        let times = if l.count > 1 { format!(" {}{}", if theme::theme().ascii { 'x' } else { '×' }, l.count) } else { String::new() };
        let tw = times.chars().count() as u16;
        for (t, tone) in &l.detail {
            let room = x1.saturating_sub(x + 1 + tw);
            if room == 0 {
                break;
            }
            let st = if *tone == Tone::Quiet || quiet && *tone != Tone::Relay { Style::fg(theme::DIM) } else { Style::fg(tone.col()) };
            let nx = c.text_words(x, y, t, st, room, g.ellipsis);
            if nx == x || nx < x + t.chars().count() as u16 {
                x = nx;
                break;
            }
            x = nx;
        }
        if !times.is_empty() && x + tw < x1 {
            c.text(x, y, &times, Style::fg(theme::accent(0.8).mix(theme::DIM, 0.4)));
        }
    }
    if m.log.is_empty() && rows > 0 {
        c.text(4, y0, "quiet so far", Style::fg(theme::FAINT));
    }
}

/// The selected machine in full: what the snapshot and the event stream say, nothing
/// more. Returns the rows used.
fn panel(c: &mut Canvas, x: u16, y: u16, w: u16, rows: u16, p: &PeerView, now_ms: u64) -> u16 {
    let g = glyphs();
    let lab = |c: &mut Canvas, yy: u16, t: &str| c.text(x, yy, t, Style::fg(theme::DIM));
    let vx = x + 10;
    let x1 = x + w;
    let dot = format!(" {} ", g.dot);
    let mut used = 0u16;
    let row = |c: &mut Canvas, label: &str, parts: Vec<(String, Style)>, used: &mut u16| {
        if *used >= rows {
            return;
        }
        let yy = y + *used;
        lab(c, yy, label);
        let mut cx = vx;
        for (i, (t, st)) in parts.iter().enumerate() {
            if t.is_empty() {
                continue;
            }
            // A part starting with a space joins the previous one (no separator).
            let joined = t.starts_with(' ') || parts.get(i.wrapping_sub(1)).is_some_and(|(p, _)| p.ends_with(' '));
            let sep = if i > 0 && !joined { dot.as_str() } else { "" };
            let need = (sep.chars().count() + t.chars().count()) as u16;
            if cx + need > x1 {
                break;
            }
            cx = c.text(cx, yy, sep, Style::fg(theme::FAINT));
            cx = c.text(cx, yy, t, *st);
        }
        *used += 1;
    };
    let lc = link_col(p.state);
    let dim = Style::fg(theme::DIM);

    // Path now.
    let mut path = vec![(path_word(p.state).to_string(), Style::fg(if p.up() { lc } else { theme::DIM }).bold())];
    if p.up() {
        if let Some(k) = p.addr_kind.as_deref().filter(|k| !k.is_empty() && *k != "relay") {
            path.push((k.to_string(), Style::fg(lc)));
        }
        if let Some(a) = p.addr.as_deref() {
            path.push((format!("  {a}"), dim));
        }
    }
    row(c, "path", path, &mut used);

    // The last minute, one cell per second, in the link's own line style.
    if !p.hist_path.is_empty() && used < rows {
        let yy = y + used;
        lab(c, yy, "60s");
        let room = x1.saturating_sub(vx + 4) as usize;
        let n = p.hist_path.len().min(room);
        let mut cx = vx;
        for _ in n..room.min(60) {
            c.put(cx, yy, g.gap, Style::fg(theme::FAINT));
            cx += 1;
        }
        for k in p.hist_path.iter().skip(p.hist_path.len() - n) {
            let (ch, col) = match k {
                PathKind::Direct => (g.direct, theme::GREEN),
                PathKind::Relay => (g.relay, theme::AMBER),
                PathKind::Unknown => (g.gap, theme::FAINT),
            };
            c.put(cx, yy, ch, Style::fg(col));
            cx += 1;
        }
        c.text(cx + 1, yy, "now", Style::fg(theme::FAINT));
        used += 1;
    }

    // Latency over that minute.
    if p.up() && !p.hist.is_empty() {
        let v: Vec<f64> = p.hist.iter().copied().collect();
        let min = v.iter().copied().fold(f64::MAX, f64::min);
        let max = v.iter().copied().fold(f64::MIN, f64::max);
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        let num = |v: f64| fmt_rtt(v).trim_end_matches(" ms").to_string();
        let mut parts = vec![(p.rtt_ms.map(fmt_rtt).unwrap_or_else(|| "-".into()), Style::fg(theme::WHITE).bold())];
        for (k, val) in [("min", min), ("avg", avg), ("max", max)] {
            parts.push((format!("{k} {}", num(val)), dim));
        }
        row(c, "latency", parts, &mut used);
    }

    // Path changes and the handshake, from the event stream.
    if let Some((t, from, to)) = p.changes.back() {
        let col = |k: &PathKind| match k {
            PathKind::Direct => theme::GREEN,
            PathKind::Relay => theme::AMBER,
            PathKind::Unknown => theme::DIM,
        };
        let parts = vec![
            (from.as_str().to_string(), Style::fg(col(from))),
            (format!(" {} ", g.arrow), Style::fg(theme::FAINT)),
            (to.as_str().to_string(), Style::fg(col(to))),
            (fmt_ago(now_ms.saturating_sub(*t)), dim),
        ];
        row(c, "changed", parts, &mut used);
    }
    if let Some((t, incoming, path)) = p.handshake {
        let parts = vec![
            ((if incoming { "incoming" } else { "outgoing" }).to_string(), Style::fg(theme::WHITE)),
            (path.as_str().to_string(), dim),
            (fmt_ago(now_ms.saturating_sub(t)), dim),
        ];
        row(c, "handshake", parts, &mut used);
    }

    // The machine.
    let mut link = Vec::new();
    match (p.up(), p.since_ms, p.seen_ms) {
        (true, Some(s), _) => link.push((format!("up {}", fmt_age(now_ms.saturating_sub(s))), Style::fg(theme::WHITE))),
        (false, _, Some(s)) => link.push((format!("seen {}", fmt_ago(now_ms.saturating_sub(s))), dim)),
        _ => {}
    }
    if let Some(v) = &p.version {
        link.push((format!("v{v}"), dim));
    }
    if !link.is_empty() {
        row(c, "link", link, &mut used);
    }
    let mut host = Vec::new();
    if let Some(h) = &p.host {
        host.push((host_label(h), Style::fg(if p.up() { host_col(h) } else { theme::DIM })));
    }
    let s = sess_text(p);
    if !s.is_empty() {
        host.push((s, Style::fg(theme::WHITE)));
    }
    if !host.is_empty() {
        row(c, "host", host, &mut used);
    }
    row(c, "id", vec![(p.id.short(), dim)], &mut used);
    if let Some(e) = &p.error {
        row(c, "error", vec![(e.clone(), Style::fg(theme::RED))], &mut used);
    }
    used
}

/// Rows `panel` would use for `p`.
fn panel_rows(p: &PeerView) -> u16 {
    let mut n = 2; // path, id
    n += u16::from(!p.hist_path.is_empty());
    n += u16::from(p.up() && !p.hist.is_empty());
    n += u16::from(!p.changes.is_empty());
    n += u16::from(p.handshake.is_some());
    n += u16::from(p.since_ms.is_some() || p.seen_ms.is_some() || p.version.is_some());
    n += u16::from(p.host.is_some() || !sess_text(p).is_empty());
    n += u16::from(p.error.is_some());
    n
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
        (g.direct.to_string().repeat(5), "direct path", Style::fg(theme::GREEN)),
        (format!("{0}{0}{1}{0}{0}", g.relay, g.waypoint), "through the relay", Style::fg(theme::AMBER)),
        (format!("{0}{1}  {2}{0}{3}", g.stub, g.cut[0], g.cut[1], g.down), "offline: no link", Style::fg(theme::DIM)),
        (format!("{}{}{}", g.direct, g.direct, g.arrow), "traffic, ping, handshake", Style::fg(theme::WHITE)),
        (format!("{}{}{}{}", g.spark[1], g.spark[4], g.spark[2], g.spark[3]), "latency, last minute", Style::fg(theme::GREEN)),
        (String::new(), "  one scale for all links", Style::PLAIN),
    ];
    let bw = 44u16.min(c.w.saturating_sub(2));
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
    let top = match big.then(|| header_big(c, m, st.tag, st.beat_ms, now_ms)).flatten() {
        Some(rows) => rows,
        None => {
            header(c, 0, m, st.tag, st.beat_ms, now_ms);
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
    let lower = keys_y.saturating_sub(dy + 1);
    let sel = m.peers.get(st.selected);
    let want = sel.map(panel_rows).unwrap_or(0);
    let title = |p: &PeerView| p.name.clone();
    let tstyle = |p: &PeerView| Style::fg(link_col(p.state)).bold();
    if let Some(p) = sel.filter(|_| w >= 104 && lower >= 8) {
        // Wide: the events on the left, the selected machine on the right.
        let pw = 46u16;
        let px = w - pw - 1;
        rule_at(c, 1, px - 2, dy, "events", Style::fg(theme::DIM));
        rule_at(c, px - 1, w - 1, dy, &title(p), tstyle(p));
        events(c, px - 3, dy + 1, keys_y.saturating_sub(dy + 1), m, now_ms);
        panel(c, px + 1, dy + 1, pw - 2, keys_y.saturating_sub(dy + 1), p, now_ms);
    } else if let Some(p) = sel.filter(|_| h >= 30 && lower >= want + 6) {
        // Tall: the selected machine in full, then the events.
        rule_at(c, 1, w - 1, dy, &title(p), tstyle(p));
        let used = panel(c, 3, dy + 1, w - 5, want, p, now_ms);
        let ey = dy + 1 + used + 1;
        rule(c, ey, "events");
        events(c, w, ey + 1, keys_y.saturating_sub(ey + 1), m, now_ms);
    } else {
        detail(c, dy, m, st.selected, now_ms);
        let ey = dy + 2;
        rule(c, ey, "events");
        events(c, w, ey + 1, keys_y.saturating_sub(ey + 1), m, now_ms);
    }
    keybar(c, keys_y, st, now_ms);
    if st.help {
        help(c, now_ms);
    }
}
