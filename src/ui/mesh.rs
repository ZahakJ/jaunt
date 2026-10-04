//! Drawing the mesh: this machine as the hub on the left, a trunk, and one branch per
//! peer whose line style is its path. Shared by the live view, `jaunt ls` and the demo.

use super::canvas::Canvas;
use super::model::{Dir, Mesh, PeerView, Pulse, fmt_age, fmt_rtt};
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

/// Which of the right-hand columns fit.
#[derive(Clone, Copy)]
struct Cols {
    path: u16,
    host: u16,
    sess: u16,
    age: u16,
}

pub fn link_col(state: PeerState) -> Col {
    match state {
        PeerState::Direct => theme::GREEN,
        PeerState::Relay => theme::AMBER,
        PeerState::Connecting | PeerState::Offline => theme::DIM,
    }
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

pub fn path_text(p: &PeerView) -> String {
    let base = match p.state {
        PeerState::Direct => "direct",
        PeerState::Relay => "relayed",
        PeerState::Connecting => "dialing",
        PeerState::Offline => "offline",
    };
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

pub fn host_col(h: &str) -> Col {
    if h.contains("battery") || h.contains("sleep") || h.contains("hot") {
        theme::AMBER
    } else if h.contains("awake") {
        theme::GREEN
    } else {
        theme::DIM
    }
}

fn sess_text(p: &PeerView) -> String {
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

/// A sparkline of `v` in `w` cells: buckets keep their maximum, scaled to the window's
/// spread (with a floor, so jitter on a sub-millisecond link does not look wild).
pub fn sparkline(v: &[f64], w: usize) -> Vec<usize> {
    if v.is_empty() || w == 0 {
        return Vec::new();
    }
    let n = v.len().min(w);
    let per = v.len() as f64 / n as f64;
    let buckets: Vec<f64> = (0..n)
        .map(|i| {
            let a = (i as f64 * per) as usize;
            let b = (((i + 1) as f64 * per) as usize).clamp(a + 1, v.len());
            v[a..b].iter().copied().fold(f64::MIN, f64::max)
        })
        .collect();
    let lo = buckets.iter().copied().fold(f64::MAX, f64::min);
    let hi = buckets.iter().copied().fold(f64::MIN, f64::max);
    let span = (hi - lo).max(hi * 0.5).max(0.05);
    buckets.iter().map(|x| (((x - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize).collect()
}

/// Rows the tree needs for `n` peers.
pub fn tree_rows(n: usize, gap: bool) -> u16 {
    if n == 0 { 1 } else { (n * if gap { 2 } else { 1 } - usize::from(gap)) as u16 }
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

    // Right-hand columns, dropped from the least important when space is short.
    let name_w = peers.iter().map(|(_, p)| p.name.chars().count()).max().unwrap_or(4).clamp(4, 16) as u16;
    let width_of = |f: &dyn Fn(&PeerView) -> String| peers.iter().map(|(_, p)| f(p).chars().count()).max().unwrap_or(0) as u16;
    let full = Cols {
        path: width_of(&path_text),
        host: width_of(&|p: &PeerView| p.host.as_deref().map(host_label).unwrap_or_default()).min(24),
        sess: width_of(&sess_text),
        age: width_of(&|p: &PeerView| age_text(p, o.now_ms)),
    };
    // The link gets a fair share of the width first; then columns join in order of
    // importance while they fit; whatever is left lengthens the link (up to a cap).
    let total = (x + w).saturating_sub(jx + 1).saturating_sub(2 + name_w + 1);
    let mut len = (w * 3 / 10).clamp(18, 40).min(total);
    let mut spare = total - len;
    let mut cols = Cols { path: 0, host: 0, sess: 0, age: 0 };
    for k in 0..4 {
        let (want, slot) = match k {
            0 => (full.host, &mut cols.host),
            1 => (full.sess, &mut cols.sess),
            2 => (full.path, &mut cols.path),
            _ => (full.age, &mut cols.age),
        };
        if want > 0 && want + 2 <= spare {
            *slot = want;
            spare -= want + 2;
        }
    }
    len = (len + spare).clamp(6, 40);
    let node_x = jx + 1 + len;
    // The label ("27 ms ▁▂▁▃") sits near the node; the line runs free before it, so a
    // pulse is visible along most of the link.
    let spark_w = ((len as i32 - 22) / 2).clamp(0, 12) as usize;
    let spark_w = if spark_w < 5 { 0 } else { spark_w };
    let label_w = 8 + if spark_w > 0 { spark_w as u16 + 1 } else { 0 };
    let lx = node_x.saturating_sub(3 + label_w);
    let label_fits = lx >= jx + 4;

    let first = y;
    let last = y + rows - 1;
    let mut drawn = Vec::new();
    for row in first..=last {
        let peer_row = (row - first).is_multiple_of(step as u16);
        let ch = joint(row > first, row < last, row == hy, peer_row);
        c.put(jx, row, ch, Style::fg(theme::FAINT));
    }
    for (k, (idx, p)) in peers.iter().enumerate() {
        let ry = y + (k * step) as u16;
        if ry > last {
            break;
        }
        drawn.push((*idx, ry));
        let lc = link_col(p.state);
        let lg = line_glyph(p.state);
        let lstyle = Style::fg(if p.state == PeerState::Connecting { theme::FAINT } else { lc });
        c.hline(jx + 1, node_x, ry, lg, lstyle);

        // The label on the line: latency and the last minute.
        if p.up() && label_fits {
            let t = p.rtt_ms.map(fmt_rtt).unwrap_or_else(|| "- ms".into());
            let (num, unit) = t.split_once(' ').unwrap_or((&t, ""));
            c.hline(lx, lx + label_w + 1, ry, ' ', Style::PLAIN);
            let num_x = lx + 1 + 4u16.saturating_sub(num.len() as u16);
            let ax = c.text(num_x, ry, num, Style::fg(theme::WHITE).bold());
            c.text(ax + 1, ry, unit, Style::fg(theme::DIM));
            if spark_w > 0 {
                let hist: Vec<f64> = p.hist.iter().copied().collect();
                let levels = sparkline(&hist, spark_w);
                let sx = lx + 9 + (spark_w - levels.len()) as u16;
                for (i, l) in levels.iter().enumerate() {
                    let shade = lc.mix(theme::FAINT, 0.4 - *l as f32 * 0.05);
                    c.put(sx + i as u16, ry, g.spark[*l], Style::fg(shade));
                }
            }
        } else if p.state == PeerState::Connecting && label_fits {
            let spin = g.spinner[((o.now_ms / 80) as usize) % g.spinner.len()];
            c.hline(lx, lx + label_w + 1, ry, ' ', Style::PLAIN);
            c.text(lx + 1, ry, &format!("{spin} dialing"), Style::fg(theme::DIM));
            // A scan moving along the dotted line.
            let pos = ((o.now_ms / 60) % len as u64) as u16;
            for d in 0..3u16 {
                let cx = jx + 1 + (pos + d) % len;
                if c.get(cx, ry).is_some_and(|cell| cell.ch == lg) {
                    c.put(cx, ry, lg, Style::fg(theme::DIM));
                }
            }
        } else if p.state == PeerState::Offline && label_fits {
            c.text(lx + 1 + 8 + spark_w as u16 - 7, ry, "offline", Style::fg(theme::FAINT));
        }

        for pulse in m.pulses.iter().filter(|q| q.peer.0 == p.id.0) {
            draw_pulse(c, jx + 1, node_x, ry, lg, lc, pulse, o.now_ms);
        }

        // Node and name.
        let flash = m.pulses.iter().any(|q| q.peer.0 == p.id.0 && arrived(q, o.now_ms));
        let (node, nst) = if p.up() {
            (g.up, Style::fg(if flash { theme::WHITE } else { lc }).bold())
        } else {
            (g.down, Style::fg(theme::DIM))
        };
        c.put(node_x, ry, node, nst);
        let name_st = if p.up() { Style::fg(theme::WHITE).bold() } else { Style::fg(theme::DIM).bold() };
        let mut cx = c.text(node_x + 2, ry, &p.name, name_st);
        cx = cx.max(node_x + 2 + name_w);
        let mut col = |c: &mut Canvas, width: u16, s: &str, st: Style| {
            if width > 0 {
                c.text_fit(cx + 2, ry, s, st, width, glyphs().ellipsis);
                cx += width + 2;
            }
        };
        col(c, cols.path, &path_text(p), Style::fg(if p.up() { lc } else { theme::DIM }));
        col(c, cols.sess, &sess_text(p), Style::fg(theme::WHITE));
        let host = p.host.as_deref().map(host_label).unwrap_or_default();
        // An offline machine's host state is from its last ping: shown, but not as news.
        col(c, cols.host, &host, Style::fg(if p.up() { host_col(&host) } else { theme::DIM }));
        col(c, cols.age, &age_text(p, o.now_ms), Style::fg(theme::DIM));

        if o.selected == Some(*idx) {
            for sx in (node_x + 1)..(x + w) {
                c.style_at(sx, ry, |s| Style { sel: true, ..s });
            }
        }
    }
    drawn
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
/// and a fading tail, drawn over line cells only, never over the label text.
#[allow(clippy::too_many_arguments)]
fn draw_pulse(c: &mut Canvas, x0: u16, x1: u16, y: u16, lg: char, lc: Col, p: &Pulse, now: u64) {
    if now < p.start_ms || now >= p.start_ms + p.dur_ms || x1 <= x0 + 1 || lg == ' ' {
        return;
    }
    let g = glyphs();
    let len = (x1 - x0) as f32;
    let t = (now - p.start_ms) as f32 / p.dur_ms as f32;
    let (frac, outward) = match p.dir {
        Dir::Out => (t, true),
        Dir::In => (t, false),
        Dir::Bounce if t < 0.5 => (t * 2.0, true),
        Dir::Bounce => ((t - 0.5) * 2.0, false),
    };
    // Ease in-out so the pulse leaves and arrives gently.
    let e = frac * frac * (3.0 - 2.0 * frac);
    let head = (e * (len - 1.0)).round() as i32;
    let head_x = if outward { x0 as i32 + head } else { x1 as i32 - 1 - head };
    let tail = if p.strong { 6 } else { 4 };
    let head_col = if p.strong { theme::CYAN.mix(theme::WHITE, 0.3) } else { theme::WHITE };
    for k in (0..=tail).rev() {
        let cx = if outward { head_x - k } else { head_x + k };
        if cx < x0 as i32 || cx >= x1 as i32 {
            continue;
        }
        let cx = cx as u16;
        let Some(cell) = c.get(cx, y) else { continue };
        if cell.ch != lg && !(k == 0 && (cell.ch == g.arrow || cell.ch == g.back)) {
            continue;
        }
        if k == 0 {
            let mut col = head_col;
            col.ansi = 97;
            c.put(cx, y, if outward { g.arrow } else { g.back }, Style::fg(col).bold());
        } else {
            let mix = 0.85 * (1.0 - k as f32 / (tail as f32 + 1.0));
            let mut col = lc.mix(if p.strong { theme::CYAN } else { theme::WHITE }, mix);
            if k == 1 {
                col.ansi = 97;
            }
            c.put(cx, y, lg, Style { bold: k <= 2, ..Style::fg(col) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_and_buckets() {
        assert_eq!(sparkline(&[], 8), Vec::<usize>::new());
        let flat = sparkline(&[0.8; 60], 8);
        assert_eq!(flat.len(), 8);
        assert!(flat.iter().all(|&l| l == 0));
        let v: Vec<f64> = (0..60).map(|i| i as f64).collect();
        let s = sparkline(&v, 6);
        assert_eq!(s.len(), 6);
        assert_eq!(*s.last().unwrap_or(&0), 7);
        assert!(s.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn joints() {
        assert_eq!(tree_rows(4, true), 7);
        assert_eq!(tree_rows(1, false), 1);
    }
}
