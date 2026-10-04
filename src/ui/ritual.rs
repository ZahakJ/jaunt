//! The pairing ritual on screen, for both roles: two nodes and the gap between them, the
//! code large and speakable, steps ticking, the link closing with a pulse, the sigil and
//! three words, the question, the fan-out. Off a terminal it prints plain lines instead.

use std::io::Write;
use std::time::Duration;

use tokio::sync::mpsc;

use super::bigtext;
use super::canvas::{Canvas, Inline, clean};
use super::input::{Key, Keys};
use super::theme::{self, Style, glyphs};
use crate::proto::{PairRole, PairStep, PairUpdate};
use crate::term::RawMode;

pub enum Input {
    Answer(bool),
    Cancel,
}

#[derive(Default, Clone)]
struct Steps {
    started: u64,
    rendezvous: Option<u64>,
    key: Option<u64>,
    sealed: Option<u64>,
    confirm: Option<u64>,
    linked: Option<u64>,
}

/// Everything the view has learned so far.
#[derive(Clone)]
pub struct State {
    pub role: PairRole,
    pub me: String,
    other: Option<String>,
    code: Option<String>,
    expires_at_ms: Option<u64>,
    relay: Option<String>,
    first_machine: bool,
    steps: Steps,
    sigil: Vec<String>,
    hue: u8,
    words: Vec<String>,
    here_ok: bool,
    there_ok: bool,
    asking: bool,
    members: usize,
    fanout: Vec<(String, bool)>,
    fanout_done: Option<(usize, usize)>,
    end: Option<(bool, String)>,
}

impl State {
    pub fn new(role: PairRole, me: &str, code: Option<String>, now_ms: u64) -> State {
        State {
            role,
            me: clean(me),
            other: None,
            code,
            expires_at_ms: None,
            relay: None,
            first_machine: false,
            steps: Steps { started: now_ms, ..Steps::default() },
            sigil: Vec::new(),
            hue: 0,
            words: Vec::new(),
            here_ok: false,
            there_ok: false,
            asking: false,
            members: 0,
            fanout: Vec::new(),
            fanout_done: None,
            end: None,
        }
    }

    pub fn apply(&mut self, u: &PairUpdate, now: u64, yes: bool) {
        let s = &mut self.steps;
        match &u.step {
            PairStep::Waiting { code, expires_at_ms, relays, first_machine } => {
                self.code = Some(clean(code));
                self.expires_at_ms = Some(*expires_at_ms);
                self.relay = relays.first().map(|r| clean(r));
                self.first_machine = *first_machine;
            }
            PairStep::Dialing { relays } => self.relay = relays.first().map(|r| clean(r)),
            PairStep::Rendezvous => s.rendezvous = Some(now),
            PairStep::KeyExchange => {
                s.rendezvous.get_or_insert(now);
                s.key = Some(now);
            }
            PairStep::Sealed { other } => {
                s.key.get_or_insert(now);
                s.sealed = Some(now);
                self.other = Some(clean(&other.name));
            }
            PairStep::Confirm { other, sigil, hue, words } => {
                s.sealed.get_or_insert(now);
                s.confirm = Some(now);
                self.other = Some(clean(&other.name));
                // 5 rows of 5 cells; anything else is not drawn.
                self.sigil = sigil.iter().take(5).map(|r| r.chars().take(5).collect()).collect();
                self.hue = *hue;
                self.words = words.iter().take(3).map(|w| clean(w)).collect();
                self.asking = !yes;
            }
            PairStep::Confirmed { here } => {
                if *here {
                    self.here_ok = true;
                    self.asking = false;
                } else {
                    self.there_ok = true;
                }
            }
            PairStep::Linked { other, members } => {
                s.linked = Some(now);
                self.other = Some(clean(&other.name));
                self.members = *members;
                self.here_ok = true;
                self.there_ok = true;
                self.asking = false;
            }
            PairStep::FanOut { machine, ok, .. } => self.fanout.push((clean(&machine.name), *ok)),
            PairStep::FanOutDone { told, not_yet } => self.fanout_done = Some((*told, *not_yet)),
            PairStep::Failed { reason } => self.end = Some((false, clean(reason))),
        }
    }

    pub fn finish(&mut self, ok: bool, msg: &str) {
        self.asking = false;
        if self.end.is_none() || ok {
            self.end = Some((ok, clean(msg)));
        }
    }

    /// The other side's label before its name is known.
    fn other_label(&self) -> String {
        match (&self.other, self.role) {
            (Some(n), _) => n.clone(),
            (None, PairRole::Inviter) => "new machine".into(),
            (None, PairRole::Joiner) => "inviter".into(),
        }
    }
}

/// How far the gap has closed (0..1) at `now`, eased between steps.
fn closure(s: &Steps, now: u64) -> f32 {
    let marks = [(s.rendezvous, 0.4), (s.key, 0.62), (s.sealed, 0.82), (s.linked, 1.0)];
    let mut target = 0.0;
    let mut since = s.started;
    let mut from = 0.0;
    for (t, v) in marks {
        if let Some(t) = t {
            from = target;
            target = v;
            since = t;
        }
    }
    let k = ((now.saturating_sub(since)) as f32 / 350.0).min(1.0);
    from + (target - from) * (k * (2.0 - k))
}

/// Draw the ritual at `now` into a canvas `w` wide.
pub fn draw(st: &State, w: u16, now: u64) -> Canvas {
    let g = glyphs();
    let w = w.clamp(40, 84);
    let mut c = Canvas::new(w, 40);
    let mut y = 1;

    // The two nodes and the gap.
    let left = st.me.clone();
    let right = st.other_label();
    let linked = st.steps.linked.is_some();
    let sealed = st.steps.sealed.is_some();
    let lx = 2;
    c.put(lx, y, g.me, Style::fg(theme::accent(0.0)).bold());
    let l_end = c.text(lx + 2, y, &left, Style::fg(theme::WHITE).bold());
    let r_len = right.chars().count() as u16;
    let rx = w.saturating_sub(r_len + 4);
    let (rnode, rst) = if sealed {
        (g.up, Style::fg(theme::accent(1.0)).bold())
    } else {
        (g.down, Style::fg(theme::DIM))
    };
    c.put(rx, y, rnode, rst);
    c.text(rx + 2, y, &right, if sealed { Style::fg(theme::WHITE).bold() } else { Style::fg(theme::DIM) });
    let a = l_end + 2;
    let b = rx.saturating_sub(2);
    if b > a + 4 {
        let span = (b - a) as f32;
        let k = closure(&st.steps, now);
        let half = (span * k / 2.0).round() as u16;
        let failed = matches!(st.end, Some((false, _)));
        for x in a..b {
            let t = (x - a) as f32 / span;
            let from_left = x < a + half;
            let from_right = x >= b - half;
            if from_left || from_right {
                let col = if failed { theme::RED } else { theme::accent(t) };
                let ch = if linked || from_left && from_right { g.direct } else { g.relay };
                c.put(x, y, ch, Style::fg(col));
            } else {
                // The gap: a faint dotted shimmer while waiting.
                let phase = ((now / 120) as u16 + x) % 6;
                if phase == 0 && st.end.is_none() {
                    c.put(x, y, g.dot, Style::fg(theme::DIM));
                } else if (x - a).is_multiple_of(3) {
                    c.put(x, y, g.dot, Style::fg(theme::FAINT));
                }
            }
        }
        // The link closing: one bright pulse from here to there.
        if let Some(t) = st.steps.linked {
            let e = now.saturating_sub(t) as f32 / 900.0;
            if e < 1.0 {
                let head = a + ((b - a - 1) as f32 * e) as u16;
                for k in 0..5u16 {
                    if head >= a + k {
                        let x = head - k;
                        let ch = if k == 0 { g.arrow } else { g.direct };
                        let mut col = theme::WHITE.mix(theme::CYAN, k as f32 / 5.0);
                        col.ansi = 97;
                        c.put(x, y, ch, Style::fg(col).bold());
                    }
                }
            }
        }
    }
    y += 2;

    if let Some((false, why)) = &st.end {
        c.put(2, y, g.cross, Style::fg(theme::RED).bold());
        c.text_fit(4, y, why, Style::fg(theme::RED), w - 6, g.ellipsis);
        y += 2;
    }

    // The code, large, until the two machines have found each other (inviter only).
    if st.role == PairRole::Inviter && st.steps.confirm.is_none() && st.end.is_none() {
        if let Some(code) = &st.code {
            let spaced = code.replace('-', " ");
            let big = if theme::theme().ascii { None } else { bigtext::render(&spaced, g.half) };
            match big {
                Some(rows) if (rows[0].len() as u16) + 6 <= w => {
                    let n = rows[0].len().max(2) - 1;
                    for (r, row) in rows.iter().enumerate() {
                        for (i, ch) in row.iter().enumerate() {
                            if *ch != ' ' {
                                c.put(3 + i as u16, y + r as u16, *ch, Style::fg(theme::accent(i as f32 / n as f32)).bold());
                            }
                        }
                    }
                    y += 4;
                }
                _ => {
                    let spaced: String = code.to_uppercase().chars().flat_map(|ch| [ch, ' ']).collect();
                    c.text(3, y, spaced.trim_end(), Style::fg(theme::accent(0.3)).bold());
                    y += 2;
                }
            }
            c.text(3, y, "on the new machine run", Style::fg(theme::DIM));
            y += 1;
            let head = format!("jaunt join {code}");
            let tail = st.relay.as_ref().map(|r| format!("--relay {r}"));
            let room = w.saturating_sub(6);
            let cmd_st = Style::fg(theme::WHITE).bold();
            c.put(3, y, g.arrow, Style::fg(theme::accent(0.5)));
            match tail {
                Some(t) if (head.len() + 1 + t.len()) as u16 > room => {
                    // Too wide for one line: a shell continuation, so it still pastes whole.
                    c.text_fit(5, y, &format!("{head} \\"), cmd_st, room, g.ellipsis);
                    y += 1;
                    c.text_fit(9, y, &t, cmd_st, w.saturating_sub(10), g.ellipsis);
                }
                Some(t) => {
                    c.text(5, y, &format!("{head} {t}"), cmd_st);
                }
                None => {
                    c.text_fit(5, y, &head, cmd_st, room, g.ellipsis);
                }
            }
            y += 1;
            if st.first_machine {
                c.text(3, y, "this is the first machine: the mesh starts here", Style::fg(theme::DIM));
                y += 1;
            }
            let left = st.expires_at_ms.map(|e| e.saturating_sub(now) / 1000).unwrap_or(300);
            let mut x = c.text(3, y, "works once", Style::fg(theme::DIM));
            x = c.text(x, y, &format!(" {} ", g.dot), Style::fg(theme::FAINT));
            let tone = if left < 60 { theme::AMBER } else { theme::DIM };
            c.text(x, y, &format!("expires in {}:{:02}", left / 60, left % 60), Style::fg(tone));
            y += 2;
        }
    } else if st.role == PairRole::Joiner
        && st.steps.confirm.is_none()
        && st.end.is_none()
        && let Some(code) = &st.code
    {
        let x = c.text(3, y, "code  ", Style::fg(theme::DIM));
        c.text(x, y, code, Style::fg(theme::WHITE).bold());
        y += 2;
    }

    // The steps.
    if st.steps.confirm.is_none() && st.end.is_none() {
        let spin = g.spinner[((now / 80) as usize) % g.spinner.len()];
        if st.steps.rendezvous.is_none() {
            let first = if st.role == PairRole::Inviter { "waiting for the other machine" } else { "looking for the inviter" };
            c.put(3, y, spin, Style::fg(theme::accent(0.6)));
            c.text(5, y, first, Style::fg(theme::WHITE));
            y += 2;
        } else {
            let rows: [(&str, Option<u64>); 3] = [
                ("rendezvous", st.steps.rendezvous),
                ("key exchange", st.steps.key),
                ("identities sealed", st.steps.sealed),
            ];
            let mut prev = st.steps.started;
            let mut prev_done = true;
            for (label, done) in rows {
                let (mark, mst, lst) = match (done, prev_done) {
                    (Some(_), _) => (g.check, Style::fg(theme::GREEN).bold(), Style::fg(theme::WHITE)),
                    (None, true) => (spin, Style::fg(theme::accent(0.6)), Style::fg(theme::WHITE)),
                    (None, false) => (g.dot, Style::fg(theme::FAINT), Style::fg(theme::DIM)),
                };
                c.put(3, y, mark, mst);
                let x = c.text(5, y, label, lst);
                if let Some(t) = done {
                    let dt = t.saturating_sub(prev) as f64 / 1000.0;
                    c.text(x.max(24), y, &format!("{dt:.1}s"), Style::fg(theme::FAINT));
                    prev = t;
                }
                prev_done = done.is_some();
                y += 1;
            }
            y += 1;
        }
        return trim(c, y);
    }

    // The sigil and the words.
    if !st.sigil.is_empty() && st.end.as_ref().is_none_or(|(ok, _)| *ok) {
        let sg = sigil(&st.sigil, st.hue, &st.words);
        let sx = 4;
        // Faint corners make it a badge rather than a blob.
        let [tl, tr, bl, br, _, _] = g.corner;
        let fr = Style::fg(theme::FAINT);
        c.put(sx - 2, y.saturating_sub(1), tl, fr);
        c.put(sx + 11, y.saturating_sub(1), tr, fr);
        c.put(sx - 2, y + 5, bl, fr);
        c.put(sx + 11, y + 5, br, fr);
        for (r, row) in sg.iter().enumerate() {
            for (i, cell) in row.iter().enumerate() {
                if let Some((fill, col)) = cell {
                    c.text(sx + i as u16 * 2, y + r as u16, fill, Style::fg(*col));
                }
            }
        }
        let col = sigil_tones(st.hue, &st.words)[1];
        let tx = sx + 15;
        let other = st.other_label();
        if st.steps.linked.is_none() {
            c.text(tx, y, &format!("compare with the screen of {other}"), Style::fg(theme::DIM));
        } else {
            c.text(tx, y, "both screens matched", Style::fg(theme::DIM));
        }
        let mut x = tx;
        for (i, wd) in st.words.iter().enumerate() {
            if i > 0 {
                x = c.text(x, y + 1, &format!(" {} ", g.dot), Style::fg(theme::FAINT));
            }
            x = c.text(x, y + 1, wd, Style::fg(col).bold());
        }
        // Who has confirmed, in words.
        let spin = g.spinner[((now / 80) as usize) % g.spinner.len()];
        let me = format!("{} (here)", st.me);
        let nw = me.chars().count().max(other.chars().count()) as u16;
        let who = [
            (me.as_str(), st.here_ok, if st.asking { "not yet: answer below" } else { "not yet" }),
            (other.as_str(), st.there_ok, "waiting for its answer"),
        ];
        for (k, (name, ok, wait)) in who.iter().enumerate() {
            let yy = y + 3 + k as u16;
            let (m, ms) = if *ok {
                (g.check, Style::fg(theme::GREEN).bold())
            } else if k == 1 && st.end.is_none() {
                (spin, Style::fg(theme::accent(0.6)))
            } else {
                (g.dot, Style::fg(theme::FAINT))
            };
            c.put(tx, yy, m, ms);
            c.text(tx + 2, yy, name, Style::fg(if *ok { theme::WHITE } else { theme::DIM }));
            let (t, ts) = if *ok { ("confirmed", Style::fg(theme::GREEN)) } else { (*wait, Style::fg(theme::DIM)) };
            c.text_fit(tx + 4 + nw, yy, t, ts, w.saturating_sub(tx + 5 + nw), g.ellipsis);
        }
        y += 6;
        if st.asking {
            let x = c.text(3, y, "Same sigil and words on both screens?  ", Style::fg(theme::WHITE).bold());
            let x = c.text(x, y, "y", Style::fg(theme::GREEN).bold());
            let x = c.text(x + 1, y, "yes   ", Style::fg(theme::DIM));
            let x = c.text(x, y, "n", Style::fg(theme::RED).bold());
            c.text(x + 1, y, "no", Style::fg(theme::DIM));
            y += 2;
        }
    }

    if st.steps.linked.is_some() {
        c.put(3, y, g.check, Style::fg(theme::GREEN).bold());
        let other = st.other_label();
        let msg = match st.role {
            PairRole::Inviter => format!("linked: {other} joined the mesh"),
            PairRole::Joiner => format!("linked through {other}"),
        };
        let x = c.text(5, y, &msg, Style::fg(theme::WHITE).bold());
        if st.members > 0 {
            c.text(x, y, &format!("  {} {} machines", g.dot, st.members), Style::fg(theme::DIM));
        }
        y += 1;
        if !st.fanout.is_empty() || st.fanout_done.is_some_and(|(a, b)| a + b > 0) {
            let mut x = c.text(5, y, "told  ", Style::fg(theme::DIM));
            for (name, ok) in &st.fanout {
                if x + name.len() as u16 + 4 > w {
                    break;
                }
                x = c.text(x, y, name, Style::fg(theme::WHITE));
                let (m, ms) = if *ok { (g.check, Style::fg(theme::GREEN).bold()) } else { (g.ellipsis, Style::fg(theme::AMBER)) };
                c.put(x + 1, y, m, ms);
                x += 4;
            }
            if st.fanout_done.is_none() {
                c.put(x, y, g.spinner[((now / 80) as usize) % g.spinner.len()], Style::fg(theme::accent(0.6)));
            }
            y += 1;
            if let Some((_, n)) = st.fanout_done
                && n > 0
            {
                c.text(5, y, &format!("{n} not reachable now; they learn at their next sync"), Style::fg(theme::DIM));
                y += 1;
            }
        }
        y += 1;
    }
    trim(c, y)
}

/// The sigil's three tones. Picked from what both machines already show (the hue hint
/// and the three words, all derived from the same hash), so they are identical on both
/// screens; this is only a choice of colours, not more of the check.
fn sigil_tones(hue: u8, words: &[String]) -> [theme::Col; 3] {
    let pal = theme::SIGIL;
    let a = hue as usize % pal.len();
    let sum: usize = words.iter().flat_map(|w| w.bytes()).map(usize::from).sum();
    // The second hue: any other one that is also distinct in 16 colours.
    let others: Vec<usize> = (0..pal.len()).filter(|&k| pal[k].ansi != pal[a].ansi).collect();
    let b = others.get(sum % others.len().max(1)).copied().unwrap_or(a);
    let mut core = pal[a].mix(theme::WHITE, 0.55);
    core.ansi = 97;
    [core, pal[a], pal[b]]
}

/// The sigil as cells: `(fill, colour)` per lit pixel. Tones go in rings from the
/// centre (the order of the rings is picked by the words), and each tone has its own
/// fill, so the pattern of tones is the same without colour.
fn sigil(rows: &[String], hue: u8, words: &[String]) -> Vec<Vec<Option<(&'static str, theme::Col)>>> {
    let g = glyphs();
    let mono = matches!(theme::theme().mode, theme::ColorMode::None | theme::ColorMode::Plain);
    let tones = sigil_tones(hue, words);
    let first = words.first().and_then(|w| w.bytes().next()).unwrap_or(0) as usize;
    let order = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]][first % 6];
    rows.iter()
        .enumerate()
        .map(|(r, row)| {
            row.chars()
                .enumerate()
                .map(|(i, cell)| {
                    (cell == '#').then(|| {
                        let ring = (r as i32 - 2).abs().max((i as i32 - 2).abs()) as usize;
                        let t = order[ring.min(2)];
                        // With colour the tones are colours; without, distinct fills.
                        let fill = if mono { g.sigil[t] } else { g.sigil[0] };
                        (fill, tones[t])
                    })
                })
                .collect()
        })
        .collect()
}

fn trim(c: Canvas, h: u16) -> Canvas {
    let mut out = Canvas::new(c.w, h.min(c.h));
    for y in 0..out.h {
        for x in 0..out.w {
            if let Some(cell) = c.get(x, y) {
                out.put(x, y, cell.ch, cell.st);
            }
        }
    }
    out
}

/// One step as plain lines (non-terminal output).
pub fn plain_lines(u: &PairUpdate) -> Vec<String> {
    let inviter = u.role == PairRole::Inviter;
    let ascii = theme::theme().ascii;
    match &u.step {
        PairStep::Waiting { code, relays, first_machine, .. } => {
            let mut v = Vec::new();
            if *first_machine {
                v.push("this is the first machine: the mesh starts here".into());
            }
            v.push(format!("code: {code}   (works once, for 5 minutes)"));
            let relay = relays.first().map(|r| format!(" --relay {r}")).unwrap_or_default();
            v.push(format!("on the new machine run:  jaunt join {code}{relay}"));
            v.push("waiting for the other machine...".into());
            v
        }
        PairStep::Dialing { relays } => vec![format!("looking for the inviting machine through {}", relays.join(", "))],
        PairStep::Rendezvous => vec!["rendezvous: the two machines are connected".into()],
        PairStep::KeyExchange => vec!["key exchange done".into()],
        PairStep::Sealed { other } => vec![format!("identities sealed: {} ({})", clean(&other.name), other.id.short())],
        PairStep::Confirm { other, sigil, words, .. } => {
            let (on, off) = if ascii { ("##", "  ") } else { ("██", "  ") };
            let mut v = vec![format!("compare with the screen of {}:", clean(&other.name)), String::new()];
            v.extend(sigil.iter().take(5).map(|row| {
                let cells: String = row.chars().take(5).map(|c| if c == '#' { on } else { off }).collect();
                format!("    {cells}")
            }));
            v.push(String::new());
            v.push(format!("    {}", words.iter().map(|w| clean(w)).collect::<Vec<_>>().join(" ")));
            v.push(String::new());
            v
        }
        PairStep::Confirmed { here: true } => vec!["confirmed here".into()],
        PairStep::Confirmed { here: false } => vec!["confirmed on the other machine".into()],
        PairStep::Linked { other, members } => {
            if inviter {
                vec![format!("linked: {} joined the mesh ({members} machines)", clean(&other.name))]
            } else {
                vec![format!("linked through {}: the mesh has {members} machines", clean(&other.name))]
            }
        }
        PairStep::FanOut { machine, ok: true, .. } => vec![format!("  told {}", clean(&machine.name))],
        PairStep::FanOut { machine, ok: false, detail } => vec![format!("  not yet told {}: {}", clean(&machine.name), clean(detail))],
        PairStep::FanOutDone { told, not_yet } => {
            if *told + *not_yet == 0 { vec![] } else { vec![format!("{told} told, {not_yet} not yet")] }
        }
        PairStep::Failed { .. } => vec![],
    }
}

fn ask_line() -> bool {
    print!("Same sigil and words on both screens? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

struct Fancy {
    out: Inline,
    keys: Keys,
    _raw: RawMode,
}

/// The ritual as the `link`/`join` client shows it.
pub struct Ritual {
    st: State,
    yes: bool,
    fancy: Option<Fancy>,
    answers: (mpsc::Sender<bool>, mpsc::Receiver<bool>),
}

fn now_ms() -> u64 {
    crate::roster::now_ms()
}

/// The last line off a terminal (or in a pane too narrow to draw in). The message can
/// quote the other machine (PAIR-1), so it is never printed raw.
fn plain_finish(ok: bool, msg: &str) -> String {
    let msg = crate::term::safe_text(msg, 2000, true);
    if ok { msg } else { format!("jaunt: {msg}") }
}

impl Ritual {
    /// The animated view when stdin and stdout are terminals; plain lines otherwise.
    pub fn start(role: PairRole, me: &str, code: Option<String>, yes: bool) -> Ritual {
        let tty = crate::term::stdout_is_tty() && crate::term::stdin_is_tty() && std::env::var("TERM").is_ok_and(|t| t != "dumb");
        let fancy = if tty && theme::width() >= 40 {
            match RawMode::enable() {
                Ok(raw) => Some(Fancy { out: Inline::new(theme::theme().mode), keys: Keys::start(), _raw: raw }),
                Err(_) => None,
            }
        } else {
            None
        };
        let mut r = Ritual { st: State::new(role, me, code, now_ms()), yes, fancy, answers: mpsc::channel(1) };
        if r.fancy.is_some() {
            let _ = std::io::stdout().write_all(b"\x1b[?25l");
            r.redraw();
        }
        r
    }

    fn redraw(&mut self) {
        if let Some(f) = &mut self.fancy {
            let c = draw(&self.st, theme::width().saturating_sub(1), now_ms());
            let _ = f.out.draw(&c, &mut std::io::stdout());
        }
    }

    pub fn step(&mut self, u: &PairUpdate) {
        self.st.apply(u, now_ms(), self.yes);
        if self.fancy.is_some() {
            self.redraw();
            return;
        }
        let mut out = std::io::stdout();
        for l in plain_lines(u) {
            let _ = writeln!(out, "{l}");
        }
        let _ = out.flush();
        if matches!(u.step, PairStep::Confirm { .. }) && !self.yes {
            let tx = self.answers.0.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(ask_line());
            });
        }
    }

    /// The next thing the human did. Redraws the animation while waiting.
    pub async fn input(&mut self) -> Input {
        let mut tick = tokio::time::interval(Duration::from_millis(66));
        loop {
            let Some(f) = self.fancy.as_mut() else {
                return match self.answers.1.recv().await {
                    Some(a) => Input::Answer(a),
                    None => std::future::pending().await,
                };
            };
            tokio::select! {
                k = f.keys.rx.recv() => match k {
                    Some(Key::CtrlC) | Some(Key::Char('q')) if !self.st.asking => return Input::Cancel,
                    Some(Key::CtrlC) => return Input::Cancel,
                    Some(Key::Char('y')) | Some(Key::Char('Y')) if self.st.asking => {
                        self.st.asking = false;
                        return Input::Answer(true);
                    }
                    Some(Key::Char('n')) | Some(Key::Char('N')) | Some(Key::Enter) | Some(Key::Esc) if self.st.asking => {
                        self.st.asking = false;
                        return Input::Answer(false);
                    }
                    Some(_) => {}
                    None => std::future::pending::<()>().await,
                },
                _ = tick.tick() => self.redraw(),
            }
        }
    }

    /// The end: let the closing pulse play out, draw the last frame, restore the terminal.
    pub async fn finish(mut self, ok: bool, msg: &str) {
        self.st.finish(ok, msg);
        if self.fancy.is_none() {
            let line = plain_finish(ok, msg);
            if ok {
                println!("{line}");
            } else {
                eprintln!("{line}");
            }
            return;
        }
        if let Some(t) = self.st.steps.linked {
            while now_ms() < t + 950 {
                self.redraw();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        self.redraw();
        if let Some(mut f) = self.fancy.take() {
            f.keys.stop();
            let mut out = std::io::stdout();
            let _ = f.out.finish(&mut out);
            let _ = out.write_all(b"\x1b[?25h");
            let _ = out.flush();
        }
    }
}

impl Drop for Ritual {
    fn drop(&mut self) {
        if self.fancy.is_some() {
            let _ = std::io::stdout().write_all(b"\r\n\x1b[?25h");
            let _ = std::io::stdout().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PAIR-1 / L9: the plain (non-tty, narrow pane) last line never carries a raw escape,
    /// even when the daemon's error quotes the other machine's abort reason.
    #[test]
    fn plain_finish_line_has_no_escapes() {
        let msg = "the other machine stopped: \x1b]52;c;Y3VybCBldmlsfHNo\x07\x1b]0;pwned\x07";
        let line = plain_finish(false, msg);
        assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
        assert!(line.starts_with("jaunt: the other machine stopped"));
        assert_eq!(plain_finish(true, "linked fern"), "linked fern");
    }
}
