//! Colour capability, palette, glyph sets and terminal width: decided once per process.
//!
//! `JAUNT_COLOR=truecolor|16|none` forces a colour mode (captures use it); otherwise
//! `--no-color`, `NO_COLOR`, a non-TTY stdout or `TERM=dumb` give none, `COLORTERM`
//! truecolor/24bit gives truecolor, anything else the 16 ANSI colours.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    True,
    Ansi16,
    /// No colour; bold and reverse still mark structure (NO_COLOR on a terminal).
    None,
    /// Not a terminal: no escape codes at all.
    Plain,
}

/// A colour: the truecolor value and the 16-colour SGR code used instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Col {
    pub rgb: (u8, u8, u8),
    pub ansi: u8,
}

const fn col(r: u8, g: u8, b: u8, ansi: u8) -> Col {
    Col { rgb: (r, g, b), ansi }
}

// The palette of design §11.
pub const VIOLET: Col = col(0xb4, 0xa0, 0xff, 95);
pub const CYAN: Col = col(0x6f, 0xe3, 0xe1, 96);
pub const GREEN: Col = col(0x8e, 0xe6, 0xa0, 92);
pub const AMBER: Col = col(0xf2, 0xc9, 0x7d, 33);
pub const RED: Col = col(0xf2, 0x8b, 0x9a, 91);
pub const DIM: Col = col(0x6c, 0x70, 0x86, 90);
/// Fainter than DIM, for structure (trunk, rules). Same 16-colour code as DIM.
pub const FAINT: Col = col(0x55, 0x58, 0x6e, 90);
pub const WHITE: Col = col(0xf5, 0xf3, 0xff, 97);
/// The selection bar's background (the only background jaunt ever sets).
pub const SEL_BG: (u8, u8, u8) = (0x2e, 0x29, 0x48);

/// Colours for the sigil, indexed by the pairing `hue` hint (0..6).
pub const SIGIL: [Col; 7] = [VIOLET, CYAN, GREEN, AMBER, RED, col(0x89, 0xb4, 0xfa, 94), col(0xf5, 0xc2, 0xe7, 95)];

impl Col {
    /// Mix towards `other` by `t` (0..1); keeps this colour's 16-colour code.
    pub fn mix(self, other: Col, t: f32) -> Col {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Col { rgb: (m(self.rgb.0, other.rgb.0), m(self.rgb.1, other.rgb.1), m(self.rgb.2, other.rgb.2)), ansi: self.ansi }
    }
}

/// The accent gradient (violet → cyan) at `t` in 0..1.
pub fn accent(t: f32) -> Col {
    let mut c = VIOLET.mix(CYAN, t);
    c.ansi = if t < 0.5 { VIOLET.ansi } else { CYAN.ansi };
    c
}

pub struct Glyphs {
    pub me: char,
    pub up: char,
    pub down: char,
    pub direct: char,
    pub relay: char,
    pub connecting: char,
    pub stub: char,
    pub rule: char,
    pub arrow: char,
    pub back: char,
    pub dot: char,
    pub check: char,
    pub cross: char,
    pub ellipsis: char,
    pub enter: &'static str,
    pub updown: &'static str,
    pub spark: [char; 8],
    pub spinner: &'static [char],
    pub half: [char; 3],
    pub corner: [char; 6],
    /// The relay as a waypoint on a relayed link, and lit while a packet passes it.
    pub waypoint: char,
    pub waypoint_lit: char,
    /// The two ends of a broken (offline) link.
    pub cut: [char; 2],
    /// Sigil pixel fills, one per tone (distinct even without colour).
    pub sigil: [&'static str; 3],
    /// One second of a path strip with no sample.
    pub gap: char,
}

const UNICODE: Glyphs = Glyphs {
    me: '◆',
    up: '●',
    down: '○',
    direct: '━',
    relay: '╌',
    connecting: '┄',
    stub: '─',
    rule: '─',
    arrow: '▸',
    back: '◂',
    dot: '·',
    check: '✓',
    cross: '✗',
    ellipsis: '…',
    enter: "⏎",
    updown: "↑↓",
    spark: ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'],
    spinner: &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'],
    half: ['▀', '▄', '█'],
    corner: ['╭', '╮', '╰', '╯', '─', '│'],
    waypoint: '◇',
    waypoint_lit: '◆',
    cut: ['╴', '╶'],
    sigil: ["██", "▓▓", "▒▒"],
    gap: '·',
};

const ASCII: Glyphs = Glyphs {
    me: '*',
    up: 'o',
    down: '.',
    direct: '=',
    relay: '-',
    connecting: '.',
    stub: '-',
    rule: '-',
    arrow: '>',
    back: '<',
    dot: '.',
    check: '+',
    cross: 'x',
    ellipsis: '~',
    enter: "enter",
    updown: "j/k",
    spark: ['_', '.', ',', ':', '-', '=', '+', '#'],
    spinner: &['|', '/', '-', '\\'],
    half: ['"', '.', '#'],
    corner: ['+', '+', '+', '+', '-', '|'],
    waypoint: '@',
    waypoint_lit: '@',
    cut: ['-', '-'],
    sigil: ["##", "%%", "++"],
    gap: '.',
};

pub struct Theme {
    pub mode: ColorMode,
    pub g: &'static Glyphs,
    pub ascii: bool,
}

static NO_COLOR_FLAG: OnceLock<bool> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

/// Record `--no-color` before the first call to [`theme`].
pub fn set_no_color_flag(v: bool) {
    let _ = NO_COLOR_FLAG.set(v);
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn detect_mode() -> ColorMode {
    match std::env::var("JAUNT_COLOR").ok().as_deref() {
        Some("truecolor") | Some("24bit") => return ColorMode::True,
        Some("16") => return ColorMode::Ansi16,
        Some("none") | Some("0") => return ColorMode::None,
        _ => {}
    }
    if !crate::term::stdout_is_tty() || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return ColorMode::Plain;
    }
    if NO_COLOR_FLAG.get().copied().unwrap_or(false) || env_set("NO_COLOR") {
        return ColorMode::None;
    }
    match std::env::var("COLORTERM").ok().as_deref() {
        Some("truecolor") | Some("24bit") => ColorMode::True,
        _ => ColorMode::Ansi16,
    }
}

/// The first non-empty of LC_ALL, LC_CTYPE, LANG names a UTF-8 locale.
fn utf8_locale() -> bool {
    for k in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(v) = std::env::var(k)
            && !v.is_empty()
        {
            let v = v.to_ascii_lowercase();
            return v.contains("utf-8") || v.contains("utf8");
        }
    }
    false
}

pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        let ascii = (env_set("JAUNT_ASCII") && std::env::var("JAUNT_ASCII").is_ok_and(|v| v != "0")) || !utf8_locale();
        Theme { mode: detect_mode(), g: if ascii { &ASCII } else { &UNICODE }, ascii }
    })
}

pub fn glyphs() -> &'static Glyphs {
    theme().g
}

/// Columns of the terminal on stdout (then stdin), else `COLUMNS`, else 80.
pub fn width() -> u16 {
    for fd in [rustix::stdio::stdout(), rustix::stdio::stdin()] {
        if let Ok(w) = rustix::termios::tcgetwinsize(fd)
            && w.ws_col > 0
        {
            return w.ws_col;
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|&c| c > 0).unwrap_or(80)
}

/// Text style of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Col>,
    pub bold: bool,
    pub reverse: bool,
    pub sel: bool,
}

impl Style {
    pub const PLAIN: Style = Style { fg: None, bold: false, reverse: false, sel: false };
    pub const fn fg(c: Col) -> Style {
        Style { fg: Some(c), bold: false, reverse: false, sel: false }
    }
    pub const fn bold(mut self) -> Style {
        self.bold = true;
        self
    }
}

/// The SGR sequence that sets `s` from a reset state (empty for the plain style).
pub fn sgr(s: Style, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if s.bold {
        parts.push("1".into());
    }
    match (mode, s.fg) {
        (ColorMode::True, Some(c)) => parts.push(format!("38;2;{};{};{}", c.rgb.0, c.rgb.1, c.rgb.2)),
        (ColorMode::Ansi16, Some(c)) => parts.push(c.ansi.to_string()),
        _ => {}
    }
    if s.sel {
        match mode {
            ColorMode::True => parts.push(format!("48;2;{};{};{}", SEL_BG.0, SEL_BG.1, SEL_BG.2)),
            ColorMode::Ansi16 => parts.push("100".into()),
            _ => parts.push("7".into()),
        }
    } else if s.reverse {
        parts.push("7".into());
    }
    if parts.is_empty() { String::new() } else { format!("\x1b[{}m", parts.join(";")) }
}

/// `text` in style `s` for a one-line message (resets after).
pub fn paint(text: &str, s: Style) -> String {
    let p = sgr(s, theme().mode);
    if p.is_empty() { text.to_string() } else { format!("{p}{text}\x1b[0m") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_modes() {
        let s = Style::fg(GREEN).bold();
        assert_eq!(sgr(s, ColorMode::True), "\x1b[1;38;2;142;230;160m");
        assert_eq!(sgr(s, ColorMode::Ansi16), "\x1b[1;92m");
        assert_eq!(sgr(s, ColorMode::None), "\x1b[1m");
        assert_eq!(sgr(Style::fg(DIM), ColorMode::None), "");
        assert_eq!(sgr(s, ColorMode::Plain), "");
    }

    #[test]
    fn gradient_ends() {
        assert_eq!(accent(0.0).rgb, VIOLET.rgb);
        assert_eq!(accent(1.0).rgb, CYAN.rgb);
    }
}
