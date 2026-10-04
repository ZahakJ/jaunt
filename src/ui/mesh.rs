//! Drawing the mesh: this machine as the hub on the left, a trunk, and one branch per
//! peer whose line style is its path. A relayed link visibly passes through the relay (a
//! waypoint on the line); an offline one is broken. Shared by the live view, `jaunt ls`
//! and the demo.

use super::canvas::Canvas;
use super::model::{Dir, MORPH_MS, Mesh, PeerView, Pulse, fmt_age, fmt_rtt};
use super::theme::{self, Col, Style, glyphs};
use crate::proto::PeerState;

pub struct TreeOpts {
    pub now_ms: u64,
    pub selected: Option<usize>,
    /// A blank row between branches.
    pub gap: bool,
    /// First peer shown (scrolling when there are more peers than rows).
    pub offset: usize,
}

pub fn link_col(state: PeerState) -> Col {
    match state {
        PeerState::Direct => theme::GREEN,
        PeerState::Relay => theme::AMBER,
        PeerState::Connecting | PeerState::Offline => theme::DIM,
    }
}

/// The relay waypoint's resting colour: a dim amber, so the line stays the subject.
pub fn waypoint_col() -> Col {
    theme::AMBER.mix(theme::DIM, 0.2)
}

fn line_glyph(state: PeerState) -> char {
    let g = glyphs();
    match state {
        PeerState::Direct => g.direct,
        PeerState::Relay => g.relay,
        PeerState::Connecting => g.connecting,
        PeerState::Offline => ' ',
    }
}

fn is_line(ch: char) -> bool {
    let g = glyphs();
    ch == g.direct || ch == g.relay || ch == g.connecting
}

/// The one-word path: kept in every mode and width, since without colour it is what
/// tells a direct link from a relayed one in words.
pub fn path_word(state: PeerState) -> &'static str {
    match state {
        PeerState::Direct => "direct",
        PeerState::Relay => "relayed",
        PeerState::Connecting => "dialing",
        PeerState::Offline => "offline",
    }
}

pub fn path_text(p: &PeerView) -> String {
    let base = path_word(p.state);
    match (&p.addr_kind, p.up()) {
        (Some(k), true) if !k.is_empty() && k != "relay" => format!("{base} {} {k}", glyphs().dot),
        _ => base.into(),
    }
}

/// The host-mode label, shortened for a column.
pub fn host_label(h: &str) -> String {
    match h {
        "will sleep on lid close" => "sleeps on lid close".into(),
        other => other.into(),
    }
}

/// The host-mode label for a narrow column.
fn host_short(h: &str) -> String {
    match h {
        "will sleep on lid close" => "lid sleeps".into(),
        "on battery" => "battery".into(),
        other => other.into(),
    }
}

pub fn host_col(h: &str) -> Col {
    if h.contains("battery") || h.contains("sleep") || h.contains("hot") {
        theme::AMBER
    } else if h.contains("awake") {
        theme::GREEN
    } else {
        theme::DIM
    }
}

pub fn sess_text(p: &PeerView) -> String {
    match p.sessions {
        Some(1) => "1 session".into(),
        Some(n) if n > 1 && p.up() => format!("{n} sessions"),
        _ => String::new(),
    }
}

fn age_text(p: &PeerView, now: u64) -> String {
    match (p.up(), p.since_ms, p.seen_ms) {
        (true, Some(s), _) => format!("up {}", fmt_age(now.saturating_sub(s))),
        (false, _, Some(s)) => format!("seen {} ago", fmt_age(now.saturating_sub(s))),
        _ => String::new(),
    }
}

/// The box-drawing joint for a trunk cell with lines going up/down/left/right.
fn joint(up: bool, down: bool, left: bool, right: bool) -> char {
    if theme::theme().ascii {
        return match (up || down, left || right) {
            (true, false) => '|',
            (false, true) => '-',
            _ => '+',
        };
    }
    match (up, down, left, right) {
        (false, false, _, _) => '─',
        (true, true, false, false) => '│',
        (false, true, false, true) => '┌',
        (true, false, false, true) => '└',
        (true, true, false, true) => '├',
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (false, true, true, true) => '┬',
        (true, false, true, true) => '┴',
        (false, true, true, false) => '┐',
        (true, false, true, false) => '┘',
        _ => '│',
    }
}

/// One latency scale shared by every link on screen, so links compare at a glance. It
/// starts at zero and is compressed (square root: "log-ish"), so 1 ms and 40 ms both
/// read and a link's own jitter still shows; the top is a robust maximum, so one outlier
/// does not flatten the rest (samples above it are drawn full height, marked clipped).
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    hi: f64,
}

impl Scale {
    /// The scale for these histories, kept at `prev` (the last one drawn) unless the
    /// data has moved well away from it.
    pub fn steady<'a>(hists: impl IntoIterator<Item = &'a [f64]>, prev: f64) -> Option<Scale> {
        let s = Scale::of(hists)?;
        if prev > 0.0 && s.hi <= prev * 1.25 && s.hi >= prev * 0.6 {
            return Some(Scale { hi: prev });
        }
        Some(s)
    }

    pub fn hi(&self) -> f64 {
        self.hi
    }

    pub fn of<'a>(hists: impl IntoIterator<Item = &'a [f64]>) -> Option<Scale> {
        let mut v: Vec<f64> = hists.into_iter().flatten().copied().filter(|x| x.is_finite() && *x > 0.0).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let lo = v[0].max(0.05);
        let p95 = v[(v.len() - 1) * 95 / 100];
        let max = v[v.len() - 1];
        // At least 4x the fastest sample, so a calm sub-millisecond mesh does not look wild.
        let hi = max.min(p95 * 2.0).max(lo * 4.0);
        Some(Scale { hi })
    }

    /// Level 0..=5 for `x` (7 when clipped at the top), and whether it was clipped.
    pub fn level(&self, x: f64) -> (usize, bool) {
        if x > self.hi * 1.001 {
            return (7, true);
        }
        // Up to ▆: the full block is kept for clipped spikes, and the graph keeps some
        // headroom so a slow link reads as a line of bars, not a slab.
        let t = (x.max(0.0) / self.hi).sqrt();
        ((t * 5.0).round().clamp(0.0, 5.0) as usize, false)
    }
}

/// `v` (one sample a second, the newest at wall-clock second `end_sec`) in `w` cells,
/// newest right. Each cell covers a fixed slice of wall-clock time and keeps its maximum,
/// so the graph scrolls one cell at a time instead of every bucket shifting each second.
/// A cell with no sample is `None`.
pub fn sparkline(v: &[f64], end_sec: u64, w: usize, s: &Scale) -> Vec<Option<(usize, bool)>> {
    if v.is_empty() || w == 0 {
        return Vec::new();
    }
    // Whole seconds per cell, rounded down, so the newest minute fills every cell.
    let per = (60 / w).max(1) as u64;
    let last_cell = end_sec / per;
    let mut out: Vec<Option<f64>> = vec![None; w];
    for (i, x) in v.iter().enumerate() {
        let sec = end_sec.saturating_sub((v.len() - 1 - i) as u64);
        let back = last_cell - sec / per;
        if back < w as u64 {
            let slot = &mut out[w - 1 - back as usize];
            *slot = Some(slot.map_or(*x, |y| y.max(*x)));
        }
    }
    let first = out.iter().position(Option::is_some).unwrap_or(w);
    out[first..].iter().map(|c| c.map(|x| s.level(x))).collect()
}

/// Sparkline cells wanted for a canvas `w` wide: 6 at 60 columns, 12 at 80, 20 at 120.
pub fn spark_target(w: u16) -> usize {
    ((w as i32 - 28) / 4).clamp(4, 20) as usize
}

/// Rows the tree needs for `n` peers.
pub fn tree_rows(n: usize, gap: bool) -> u16 {
    if n == 0 { 1 } else { (n * if gap { 2 } else { 1 } - usize::from(gap)) as u16 }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Path,
    Sess,
    Host,
    Age,
}

/// A right-hand column: its text per peer in a short and a long form.
struct Column {
    which: Which,
    short: Vec<String>,
    long: Vec<String>,
    /// 0 = not shown, 1 = short, 2 = long.
    pick: u8,
}

impl Column {
    fn width(&self, pick: u8) -> u16 {
        let v = match pick {
            0 => return 0,
            1 => &self.short,
            _ => &self.long,
        };
        v.iter().map(|s| s.chars().count()).max().unwrap_or(0) as u16
    }
    fn text(&self, k: usize) -> &str {
        match self.pick {
            1 => &self.short[k],
            _ => &self.long[k],
        }
    }
}

/// Where things go on a branch, the same for every row.
struct Geo {
    jx: u16,
    /// Every cell of the branch that carries line (not the label): what a sweep or a
    /// packet travels along, so it moves at an even pace and never hides under text.
    cells: Vec<u16>,
    /// Free line before the label (the waypoint sits in its middle).
    free: u16,
    spark: usize,
    label_x: u16,
    node_x: u16,
    wp_x: u16,
}

/// Draw the tree into `(x, y, w, h)`. Returns the y of each drawn peer row.
pub fn draw_tree(c: &mut Canvas, x: u16, y: u16, w: u16, h: u16, m: &Mesh, o: &TreeOpts) -> Vec<(usize, u16)> {
    let g = glyphs();
    let step = if o.gap { 2 } else { 1 };
    let fit = ((h as usize) + usize::from(o.gap)) / step;
    let peers: Vec<(usize, &PeerView)> = m.peers.iter().enumerate().skip(o.offset).take(fit.max(1)).collect();
    let n = peers.len();
    let hub_name = if m.me.is_empty() { "this machine".to_string() } else { m.me.clone() };
    let rows = tree_rows(n, o.gap).min(h.max(1));
    let hy = y + (rows.saturating_sub(1)) / 2;

    // Hub.
    let hx = x + 1;
    c.put(hx, hy, g.me, Style::fg(theme::accent(0.0)).bold());
    let after = c.text(hx + 2, hy, &hub_name, Style::fg(theme::WHITE).bold());
    if n == 0 {
        c.text(after + 2, hy, "alone: run 'jaunt link' to add a machine", Style::fg(theme::DIM));
        return Vec::new();
    }
    let jx = after + 2;
    c.hline(after + 1, jx, hy, g.stub, Style::fg(theme::FAINT));

    // Columns: the path word first (never dropped), then host state, sessions; long
    // forms and the age only when there is room.
    let name_w = peers.iter().map(|(_, p)| p.name.chars().count()).max().unwrap_or(4).clamp(4, 16) as u16;
    let per = |f: &dyn Fn(&PeerView) -> String| peers.iter().map(|(_, p)| f(p)).collect::<Vec<String>>();
    let host_of = |p: &PeerView| p.host.clone().unwrap_or_default();
    let mut cols = [
        Column { which: Which::Path, short: per(&|p| path_word(p.state).into()), long: per(&path_text), pick: 0 },
        Column { which: Which::Sess, short: per(&sess_text), long: per(&sess_text), pick: 0 },
        Column { which: Which::Host, short: per(&|p| host_short(&host_of(p))), long: per(&|p| host_label(&host_of(p))), pick: 0 },
        Column { which: Which::Age, short: per(&|p| age_text(p, o.now_ms)), long: per(&|p| age_text(p, o.now_ms)), pick: 0 },
    ];
    // The link's geometry depends on the width only (never on the text in the columns),
    // so nothing jumps sideways when a path or an address changes.
    let avail = (x + w).saturating_sub(jx + 1).saturating_sub(2 + name_w + 1);
    let mut spark = spark_target(w);
    let label_w = |s: usize| 7 + if s > 0 { s as u16 + 1 } else { 0 };
    let link = |s: usize, f: u16| f + 1 + label_w(s) + 1 + 2;
    let mut free = ((w as i32 - 30) / 4).clamp(9, 24) as u16;
    // Room for the path word ("relayed") is kept before the link takes its share.
    let path_need = 7 + 2;
    while link(spark, free) + path_need > avail && free > 9 {
        free -= 1;
    }
    while link(spark, free) + path_need > avail && spark > 4 {
        spark -= 1;
    }
    if link(spark, free) + path_need > avail {
        spark = 0;
        free = avail.saturating_sub(link(0, 0) + path_need).max(2);
    }
    let mut spare = avail.saturating_sub(link(spark, free));
    for (k, pick) in [(0, 1), (2, 1), (1, 2), (2, 2), (0, 2), (3, 2)] {
        let col = &mut cols[k];
        let want = col.width(pick);
        if want == 0 || col.pick >= pick {
            continue;
        }
        let cost = if col.pick == 0 { want + 2 } else { want.saturating_sub(col.width(col.pick)) };
        if cost <= spare {
            col.pick = pick;
            spare -= cost;
        }
    }
    let lw = label_w(spark);
    let label_x = jx + 2 + free;
    let node_x = jx + 1 + link(spark, free);
    let cells: Vec<u16> = (jx + 1..node_x).filter(|&cx| cx + 1 < label_x || cx > label_x + lw).collect();
    let geo = Geo {
        jx,
        cells,
        free,
        spark,
        label_x,
        node_x,
        wp_x: jx + 1 + free / 2,
    };
    let label_fits = free >= 2;
    let hists: Vec<Vec<f64>> = peers.iter().filter(|(_, p)| p.up()).map(|(_, p)| p.hist.iter().copied().collect()).collect();
    let scale = Scale::steady(hists.iter().map(|v| v.as_slice()), m.spark_hi.get());
    if let Some(sc) = &scale {
        m.spark_hi.set(sc.hi());
    }

    let first = y;
    let last = y + rows - 1;
    for row in first..=last {
        let peer_row = (row - first).is_multiple_of(step as u16);
        let ch = joint(row > first, row < last, row == hy, peer_row);
        c.put(jx, row, ch, Style::fg(theme::FAINT));
    }
    let mut drawn = Vec::new();
    let mut relay_labelled = false;
    for (k, (idx, p)) in peers.iter().enumerate() {
        let ry = y + (k * step) as u16;
        if ry > last {
            break;
        }
        drawn.push((*idx, ry));
        let lc = link_col(p.state);
        draw_line(c, &geo, ry, p, o.now_ms, (ry > first && o.gap).then(|| ry - 1));

        // The relay, named once, on the first relayed link: above it in the gap row, else
        // below it, else beside the waypoint.
        if p.state == PeerState::Relay && !relay_labelled && p.morph.is_none() {
            relay_labelled = true;
            let label = "relay";
            let lx = geo.wp_x.saturating_sub(2);
            let st = Style::fg(theme::AMBER.mix(theme::DIM, 0.5));
            if o.gap && ry > first {
                c.text(lx, ry - 1, label, st);
            } else if o.gap && ry < last {
                c.text(lx, ry + 1, label, st);
            } else if geo.free >= 14 {
                c.put(geo.wp_x + 1, ry, ' ', Style::PLAIN);
                let e = c.text(geo.wp_x + 2, ry, label, st);
                c.put(e, ry, ' ', Style::PLAIN);
            }
        }

        // The label on the line: latency and the last minute, on the shared scale.
        if p.up() && label_fits {
            let t = p.rtt_ms.map(fmt_rtt).unwrap_or_else(|| "- ms".into());
            let (num, unit) = t.split_once(' ').unwrap_or((&t, ""));
            let num_x = geo.label_x + 4u16.saturating_sub(num.len() as u16);
            let ax = c.text(num_x, ry, num, Style::fg(theme::WHITE).bold());
            c.text(ax + 1, ry, unit, Style::fg(theme::DIM));
            if geo.spark > 0
                && let Some(s) = &scale
            {
                let hist: Vec<f64> = p.hist.iter().copied().collect();
                let levels = sparkline(&hist, p.hist_sec, geo.spark, s);
                let sx = geo.label_x + 8 + (geo.spark - levels.len()) as u16;
                let last_i = levels.len().saturating_sub(1);
                for (i, lv) in levels.iter().enumerate() {
                    let Some((l, clipped)) = lv else { continue };
                    // Older samples recede; the newest is in full colour; a clipped spike
                    // stands out.
                    let age = 1.0 - i as f32 / last_i.max(1) as f32;
                    let shade = if *clipped {
                        lc.mix(theme::WHITE, 0.5)
                    } else if i == last_i {
                        lc
                    } else {
                        // In 16 colours the history is grey and only the newest is coloured.
                        let mut col = lc.mix(theme::FAINT, 0.45 + 0.25 * age);
                        col.ansi = theme::DIM.ansi;
                        col
                    };
                    c.put(sx + i as u16, ry, g.spark[*l], Style { bold: *clipped, ..Style::fg(shade) });
                }
            }
        } else if p.state == PeerState::Connecting && label_fits {
            let spin = g.spinner[((o.now_ms / 80) as usize) % g.spinner.len()];
            c.text(geo.label_x, ry, &format!("{spin} dialing"), Style::fg(theme::DIM));
        } else if p.state == PeerState::Offline && label_fits {
            // The path column already says offline; the break says since when.
            let t = match p.seen_ms {
                Some(s) => format!("seen {} ago", fmt_age(o.now_ms.saturating_sub(s))),
                None => "never seen".into(),
            };
            let room = geo.node_x.saturating_sub(geo.label_x + 3);
            if t.len() as u16 <= room {
                c.text(geo.label_x, ry, &t, Style::fg(theme::FAINT));
            }
        }

        for pulse in m.pulses.iter().filter(|q| q.peer.0 == p.id.0) {
            draw_pulse(c, &geo, ry, lc, p.state == PeerState::Relay, pulse, o.now_ms);
        }

        // Node and name.
        let flash = m.pulses.iter().any(|q| q.peer.0 == p.id.0 && arrived(q, o.now_ms));
        let (node, nst) = if p.up() {
            (g.up, Style::fg(if flash { theme::WHITE } else { lc }).bold())
        } else {
            (g.down, Style::fg(theme::DIM))
        };
        c.put(geo.node_x, ry, node, nst);
        let name_st = if p.up() { Style::fg(theme::WHITE).bold() } else { Style::fg(theme::DIM).bold() };
        let mut cx = c.text(geo.node_x + 2, ry, &p.name, name_st);
        cx = cx.max(geo.node_x + 2 + name_w);
        for col in &cols {
            if col.pick == 0 {
                continue;
            }
            let width = col.width(col.pick);
            let s = col.text(k);
            let st = match col.which {
                Which::Path => Style::fg(if p.up() { lc } else { theme::DIM }),
                Which::Sess => Style::fg(theme::WHITE),
                // An offline machine's host state is from its last ping: shown, not as news.
                Which::Host => Style::fg(if p.up() { host_col(s) } else { theme::DIM }),
                Which::Age => Style::fg(theme::DIM),
            };
            c.text_fit(cx + 2, ry, s, st, width, g.ellipsis);
            cx += width + 2;
        }

        if o.selected == Some(*idx) {
            for sx in (geo.node_x + 1)..(x + w) {
                c.style_at(sx, ry, |s| Style { sel: true, ..s });
            }
        }
    }
    drawn
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One branch's line: style by path, the relay waypoint on relayed links, a broken link
/// when offline, and a path change sweeping from here to there. `above` is a free row
/// above the branch (the waypoint lifts off into it on an upgrade).
fn draw_line(c: &mut Canvas, geo: &Geo, ry: u16, p: &PeerView, now: u64, above: Option<u16>) {
    let g = glyphs();
    let x0 = geo.jx + 1;
    if p.state == PeerState::Offline {
        // A link that is not there: two short stubs and nothing between.
        let st = Style::fg(theme::FAINT);
        c.hline(x0, x0 + 2, ry, g.stub, st);
        c.put(x0 + 2, ry, g.cut[0], st);
        c.put(geo.node_x - 2, ry, g.cut[1], st);
        c.put(geo.node_x - 1, ry, g.stub, st);
        return;
    }
    let style_of = |s: PeerState| -> (char, Style) {
        let lg = line_glyph(s);
        let col = if s == PeerState::Connecting { theme::FAINT } else { link_col(s) };
        (lg, Style::fg(col))
    };
    // A path change: the new style sweeps out from here; the old one is what is left.
    let n = geo.cells.len();
    let (done, from) = match p.morph {
        Some(mo) if now >= mo.start_ms && now < mo.start_ms + MORPH_MS && mo.from != p.state => {
            let e = ease((now - mo.start_ms) as f32 / MORPH_MS as f32);
            ((n as f32 * e).round() as usize, Some((mo.from, e)))
        }
        _ => (n, None),
    };
    let front = geo.cells.get(done).copied().unwrap_or(geo.node_x);
    for (i, &cx) in geo.cells.iter().enumerate() {
        let s = if i < done { p.state } else { from.map(|f| f.0).unwrap_or(p.state) };
        let (lg, st) = style_of(s);
        c.put(cx, ry, lg, st);
    }
    // The sweep's leading edge, bright.
    if from.is_some() && done > 0 {
        let (lg, _) = style_of(p.state);
        let mut col = link_col(p.state).mix(theme::WHITE, 0.6);
        col.ansi = 97;
        c.put(geo.cells[done - 1], ry, lg, Style::fg(col).bold());
    }
    // The waypoint: on the line while the relayed part reaches it; an upgrade lifts it off.
    let relayed_here = |s: PeerState| s == PeerState::Relay;
    let wp_state = if geo.wp_x < front { p.state } else { from.map(|f| f.0).unwrap_or(p.state) };
    if relayed_here(wp_state) {
        c.put(geo.wp_x, ry, g.waypoint, Style::fg(waypoint_col()).bold());
    } else if let Some((f, e)) = from
        && relayed_here(f)
        && let Some(ay) = above
    {
        // Lifted: hovering above the solid line, fading out.
        let at = geo.cells.iter().position(|&cx| cx == geo.wp_x).unwrap_or(0);
        let passed = at as f32 / n.max(1) as f32;
        let k = ((e - passed) / (1.0 - passed).max(0.05)).clamp(0.0, 1.0);
        if k < 0.95 && c.get(geo.wp_x, ay).is_some_and(|cell| cell.ch == ' ') {
            c.put(geo.wp_x, ay, g.waypoint, Style::fg(waypoint_col().mix(theme::FAINT, k)));
        }
    } else if let Some((f, e)) = from
        && relayed_here(p.state)
        && !relayed_here(f)
        && let Some(ay) = above
    {
        // A downgrade: the relay drops into the line as the dashes reach it.
        let _ = e;
        if c.get(geo.wp_x, ay).is_some_and(|cell| cell.ch == ' ') {
            c.put(geo.wp_x, ay, g.waypoint, Style::fg(waypoint_col().mix(theme::FAINT, 0.4)));
        }
    }
    if p.state == PeerState::Connecting {
        // A scan moving along the dotted line.
        let len = geo.free.max(1);
        let pos = ((now / 60) % len as u64) as u16;
        for d in 0..3u16 {
            let cx = x0 + (pos + d) % len;
            c.put(cx, ry, line_glyph(p.state), Style::fg(theme::DIM));
        }
    }
}

fn arrived(p: &Pulse, now: u64) -> bool {
    let t = now.saturating_sub(p.start_ms);
    match p.dir {
        Dir::Out => t >= p.dur_ms && t < p.dur_ms + 180,
        Dir::Bounce => t >= p.dur_ms / 2 && t < p.dur_ms / 2 + 180,
        Dir::In => false,
    }
}

/// A short comet along the branch: a bright head (an arrow in the direction of travel)
/// and a fading tail, drawn over line cells only, never over the label text. On a relayed
/// link the packet passes through the relay: the waypoint lights up as it goes by.
fn draw_pulse(c: &mut Canvas, geo: &Geo, y: u16, lc: Col, relayed: bool, p: &Pulse, now: u64) {
    let n = geo.cells.len();
    if now < p.start_ms || now >= p.start_ms + p.dur_ms || n < 2 {
        return;
    }
    let g = glyphs();
    let len = n as f32;
    let t = (now - p.start_ms) as f32 / p.dur_ms as f32;
    let (frac, outward) = match p.dir {
        Dir::Out => (t, true),
        Dir::In => (t, false),
        Dir::Bounce if t < 0.5 => (t * 2.0, true),
        Dir::Bounce => ((t - 0.5) * 2.0, false),
    };
    // Ease in-out so the pulse leaves and arrives gently.
    let e = ease(frac);
    let head = (e * (len - 1.0)).round() as i32;
    let head_i = if outward { head } else { n as i32 - 1 - head };
    let tail = if p.strong { 6 } else { 4 };
    let head_col = if p.strong { theme::CYAN.mix(theme::WHITE, 0.3) } else { theme::WHITE };
    for k in (0..=tail).rev() {
        let i = if outward { head_i - k } else { head_i + k };
        if i < 0 || i >= n as i32 {
            continue;
        }
        let cx = geo.cells[i as usize];
        let Some(cell) = c.get(cx, y) else { continue };
        let waypoint = relayed && cx == geo.wp_x && (cell.ch == g.waypoint || cell.ch == g.waypoint_lit);
        if !waypoint && !is_line(cell.ch) && !(k == 0 && (cell.ch == g.arrow || cell.ch == g.back)) {
            continue;
        }
        if waypoint {
            // The relay forwarding the packet.
            let mut col = theme::AMBER.mix(theme::WHITE, 0.5 - k as f32 * 0.08);
            col.ansi = if k <= 1 { 93 } else { 33 };
            c.put(cx, y, g.waypoint_lit, Style::fg(col).bold());
        } else if k == 0 {
            let mut col = head_col;
            col.ansi = 97;
            c.put(cx, y, if outward { g.arrow } else { g.back }, Style::fg(col).bold());
        } else {
            let mix = 0.85 * (1.0 - k as f32 / (tail as f32 + 1.0));
            let mut col = lc.mix(if p.strong { theme::CYAN } else { theme::WHITE }, mix);
            if k == 1 {
                col.ansi = 97;
            }
            c.put(cx, y, cell.ch, Style { bold: k <= 2, ..Style::fg(col) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_and_buckets() {
        let s = Scale::of([&[0.8f64; 60][..]]).expect("scale");
        assert!(sparkline(&[], 100, 8, &s).is_empty());
        let flat = sparkline(&[0.8; 60], 1000, 12, &s);
        assert_eq!(flat.len(), 12);
        assert!(flat.iter().all(|l| *l == flat[0] && l.is_some_and(|l| !l.1)));
        let v: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        let s = Scale::of([&v[..]]).expect("scale");
        let out: Vec<usize> = sparkline(&v, 1000, 6, &s).into_iter().flatten().map(|l| l.0).collect();
        assert!(out.len() >= 6);
        assert!(out.windows(2).all(|w| w[0] <= w[1]));
        // One more second: cells keep their contents (the graph scrolls, it does not reshuffle).
        let a = sparkline(&v[..59], 999, 12, &s);
        let b = sparkline(&v, 1000, 12, &s);
        assert_eq!((a.len(), b.len()), (12, 12));
        assert_eq!(a[1..], b[..11]);
    }

    #[test]
    fn shared_log_scale_reads_both_ends_and_clips_outliers() {
        let fast = [1.0f64; 30];
        let slow = [40.0f64; 30];
        let s = Scale::of([&fast[..], &slow[..]]).expect("scale");
        let (a, _) = s.level(1.0);
        let (b, _) = s.level(40.0);
        let (mid, _) = s.level(6.3);
        assert!(a < mid && mid < b, "{a} {mid} {b}");
        // One spike among calm samples: the rest keep their shape, the spike is clipped.
        let mut v = vec![1.0f64; 59];
        v.push(500.0);
        for (i, x) in v.iter_mut().enumerate().take(58) {
            *x += (i % 3) as f64 * 0.5;
        }
        let s = Scale::of([&v[..]]).expect("scale");
        assert_eq!(s.level(500.0), (7, true));
        assert!(s.level(2.0).0 > s.level(1.0).0, "{:?}", s.level(2.0));
    }

    #[test]
    fn spark_widths() {
        assert!(spark_target(58) >= 6);
        assert!(spark_target(77) >= 12);
        assert!(spark_target(118) >= 20);
    }

    #[test]
    fn joints() {
        assert_eq!(tree_rows(4, true), 7);
        assert_eq!(tree_rows(1, false), 1);
    }
}
