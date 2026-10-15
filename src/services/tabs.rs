//! `Tabs`: the host's open browser tabs, read (never written) from the session files the
//! browsers keep on disk. Firefox and its forks: `recovery.jsonlz4` (mozLz4: an LZ4 block
//! behind a magic). Chromium, Chrome, Brave, Edge, Vivaldi: the newest `Session_*` file,
//! an append-only log of SNSS commands. Only http(s) tabs are listed.
//! Client: `jaunt tabs <machine>` picks some and opens them here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};

use crate::proto::Tab;

const MAX_FILE: u64 = 64 << 20;
const MAX_TABS: usize = 2000;
/// A session file not written for this long belongs to a browser nobody uses here.
const STALE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Every tab of every browser profile found under `home`, newest browser first.
pub fn read_all(home: &Path) -> Vec<Tab> {
    let mut found: Vec<(SystemTime, Vec<Tab>)> = Vec::new();
    for (name, root) in firefox_roots(home) {
        for (mtime, file) in firefox_sessions(&root) {
            if let Ok(tabs) = std::fs::read(&file).map_err(anyhow::Error::from).and_then(|b| firefox_tabs(&b, name)) {
                found.push((mtime, tabs));
            }
        }
    }
    for (name, root) in chromium_roots(home) {
        for (mtime, file) in chromium_sessions(&root) {
            if let Ok(b) = std::fs::read(&file) {
                found.push((mtime, snss_tabs(&b, name)));
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut seen = HashSet::new();
    found
        .into_iter()
        .flat_map(|(_, t)| t)
        .filter(|t| seen.insert((t.browser.clone(), t.url.clone())))
        .take(MAX_TABS)
        .collect()
}

fn firefox_roots(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mac = home.join("Library/Application Support");
    vec![
        ("firefox", home.join(".mozilla/firefox")),
        ("firefox", home.join(".var/app/org.mozilla.firefox/.mozilla/firefox")),
        ("firefox", home.join("snap/firefox/common/.mozilla/firefox")),
        ("firefox", mac.join("Firefox/Profiles")),
        ("librewolf", home.join(".librewolf")),
        ("librewolf", home.join(".var/app/io.gitlab.librewolf-community/.librewolf")),
        ("waterfox", home.join(".waterfox")),
        ("waterfox", home.join(".var/app/net.waterfox.waterfox/.waterfox")),
        ("zen", home.join(".zen")),
        ("floorp", home.join(".floorp")),
    ]
}

fn chromium_roots(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mac = home.join("Library/Application Support");
    let flat = home.join(".var/app");
    vec![
        ("chromium", home.join(".config/chromium")),
        ("chromium", flat.join("org.chromium.Chromium/config/chromium")),
        ("chromium", flat.join("io.github.ungoogled_software.ungoogled_chromium/config/chromium")),
        ("chromium", mac.join("Chromium")),
        ("chrome", home.join(".config/google-chrome")),
        ("chrome", flat.join("com.google.Chrome/config/google-chrome")),
        ("chrome", mac.join("Google/Chrome")),
        ("brave", home.join(".config/BraveSoftware/Brave-Browser")),
        ("brave", flat.join("com.brave.Browser/config/BraveSoftware/Brave-Browser")),
        ("brave", mac.join("BraveSoftware/Brave-Browser")),
        ("edge", home.join(".config/microsoft-edge")),
        ("edge", mac.join("Microsoft Edge")),
        ("vivaldi", home.join(".config/vivaldi")),
        ("vivaldi", mac.join("Vivaldi")),
    ]
}

fn fresh(p: &Path) -> Option<SystemTime> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok()?;
    (m.is_file() && m.len() <= MAX_FILE && t.elapsed().map(|e| e < STALE).unwrap_or(true)).then_some(t)
}

fn subdirs(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

/// Per profile, the newer of the running browser's recovery file and the one written at exit.
fn firefox_sessions(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    subdirs(root)
        .into_iter()
        .filter_map(|p| {
            [p.join("sessionstore-backups/recovery.jsonlz4"), p.join("sessionstore.jsonlz4")]
                .into_iter()
                .filter_map(|f| Some((fresh(&f)?, f)))
                .max_by_key(|(t, _)| *t)
        })
        .collect()
}

/// Per profile, the newest `Session_*` (or the older `Current Session`).
fn chromium_sessions(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    subdirs(root)
        .into_iter()
        .filter_map(|p| {
            let dir = p.join("Sessions");
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|d| d.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.retain(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Session_")));
            files.push(p.join("Current Session"));
            files.into_iter().filter_map(|f| Some((fresh(&f)?, f))).max_by_key(|(t, _)| *t)
        })
        .collect()
}

fn web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn tab(browser: &str, title: &str, url: &str) -> Option<Tab> {
    (web(url) && url.len() <= crate::desktop::MAX_URL).then(|| Tab {
        browser: browser.into(),
        title: crate::desktop::clean_line(title, 300),
        url: url.into(),
    })
}

// ---------------------------------------------------------------- Firefox

/// An LZ4 block (no frame), as used inside mozLz4.
pub fn lz4_block(src: &[u8], size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut i = 0;
    let byte = |i: &mut usize| -> Result<u8> {
        let b = *src.get(*i).ok_or_else(|| anyhow::anyhow!("truncated LZ4 block"))?;
        *i += 1;
        Ok(b)
    };
    let length = |i: &mut usize, base: usize| -> Result<usize> {
        let mut n = base;
        if base == 15 {
            loop {
                let b = byte(i)?;
                n += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    };
    while i < src.len() {
        let token = byte(&mut i)?;
        let lit = length(&mut i, (token >> 4) as usize)?;
        let end = i.checked_add(lit).filter(|e| *e <= src.len()).ok_or_else(|| anyhow::anyhow!("truncated LZ4 literals"))?;
        out.extend_from_slice(&src[i..end]);
        i = end;
        if i == src.len() {
            break;
        }
        let off = byte(&mut i)? as usize | (byte(&mut i)? as usize) << 8;
        let len = length(&mut i, (token & 15) as usize)? + 4;
        if off == 0 || off > out.len() || out.len() + len > size {
            bail!("corrupt LZ4 block");
        }
        let start = out.len() - off;
        for k in 0..len {
            out.push(out[start + k]);
        }
    }
    if out.len() > size {
        bail!("LZ4 block larger than announced");
    }
    Ok(out)
}

pub fn mozlz4(b: &[u8]) -> Result<Vec<u8>> {
    if b.len() < 12 || &b[..8] != b"mozLz40\0" {
        bail!("not a mozLz4 file");
    }
    let size = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
    if size > (256 << 20) {
        bail!("mozLz4 file too large");
    }
    lz4_block(&b[12..], size)
}

pub fn firefox_tabs(file: &[u8], browser: &str) -> Result<Vec<Tab>> {
    let json: serde_json::Value = serde_json::from_slice(&mozlz4(file)?)?;
    let mut tabs = Vec::new();
    for w in json["windows"].as_array().into_iter().flatten() {
        for t in w["tabs"].as_array().into_iter().flatten() {
            let entries = t["entries"].as_array();
            let n = entries.map(|e| e.len()).unwrap_or(0);
            if n == 0 {
                continue;
            }
            // `index` is 1-based; missing means the last entry.
            let idx = t["index"].as_u64().map(|i| (i as usize).clamp(1, n) - 1).unwrap_or(n - 1);
            let e = &entries.unwrap()[idx];
            if let Some(t) = tab(browser, e["title"].as_str().unwrap_or(""), e["url"].as_str().unwrap_or("")) {
                tabs.push(t);
            }
        }
    }
    Ok(tabs)
}

// ---------------------------------------------------------------- Chromium (SNSS)

const SET_TAB_WINDOW: u8 = 0;
const SET_TAB_INDEX_IN_WINDOW: u8 = 2;
const UPDATE_TAB_NAVIGATION: u8 = 6;
const SET_SELECTED_NAVIGATION_INDEX: u8 = 7;
const TAB_CLOSED: u8 = 16;
const WINDOW_CLOSED: u8 = 17;

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A `base::Pickle`: u32 payload size, then 4-byte aligned fields.
struct Pickle<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Pickle<'a> {
    fn new(b: &'a [u8]) -> Pickle<'a> {
        Pickle { b, at: 4 }
    }
    fn int(&mut self) -> Option<i32> {
        let v = i32_at(self.b, self.at)?;
        self.at += 4;
        Some(v)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n.div_ceil(4) * 4;
        Some(s)
    }
    fn string(&mut self) -> Option<String> {
        let n = usize::try_from(self.int()?).ok()?;
        Some(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }
    fn string16(&mut self) -> Option<String> {
        let n = usize::try_from(self.int()?).ok()?;
        let raw = self.bytes(n.checked_mul(2)?)?;
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

#[derive(Default)]
struct SnssTab {
    window: i32,
    order: i32,
    selected: Option<i32>,
    navs: BTreeMap<i32, (String, String)>,
}

pub fn snss_tabs(b: &[u8], browser: &str) -> Vec<Tab> {
    if b.len() < 8 || &b[..4] != b"SNSS" {
        return Vec::new();
    }
    let mut tabs: HashMap<i32, SnssTab> = HashMap::new();
    let mut closed_windows = HashSet::new();
    let mut at = 8;
    while let Some(size) = b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize) {
        at += 2;
        let Some(cmd) = b.get(at..at + size) else { break };
        at += size;
        let (Some(&id), body) = (cmd.first(), cmd.get(1..).unwrap_or(&[])) else { continue };
        match id {
            SET_TAB_WINDOW => {
                if let (Some(w), Some(t)) = (i32_at(body, 0), i32_at(body, 4)) {
                    tabs.entry(t).or_default().window = w;
                }
            }
            SET_TAB_INDEX_IN_WINDOW => {
                if let (Some(t), Some(i)) = (i32_at(body, 0), i32_at(body, 4)) {
                    tabs.entry(t).or_default().order = i;
                }
            }
            UPDATE_TAB_NAVIGATION => {
                let mut p = Pickle::new(body);
                if let (Some(t), Some(i), Some(url), Some(title)) = (p.int(), p.int(), p.string(), p.string16()) {
                    tabs.entry(t).or_default().navs.insert(i, (url, title));
                }
            }
            SET_SELECTED_NAVIGATION_INDEX => {
                if let (Some(t), Some(i)) = (i32_at(body, 0), i32_at(body, 4)) {
                    tabs.entry(t).or_default().selected = Some(i);
                }
            }
            TAB_CLOSED => {
                if let Some(t) = i32_at(body, 0) {
                    tabs.remove(&t);
                }
            }
            WINDOW_CLOSED => {
                if let Some(w) = i32_at(body, 0) {
                    closed_windows.insert(w);
                }
            }
            _ => {}
        }
    }
    let mut open: Vec<SnssTab> = tabs.into_values().filter(|t| !closed_windows.contains(&t.window)).collect();
    open.sort_by_key(|t| (t.window, t.order));
    open.into_iter()
        .filter_map(|t| {
            let (url, title) = t.selected.and_then(|s| t.navs.get(&s)).or_else(|| t.navs.values().next_back())?;
            tab(browser, title, url)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal-only block, plus one with an overlapping match ("abcabcabca").
    #[test]
    fn lz4_blocks() {
        assert_eq!(lz4_block(b"\x50hello", 5).unwrap(), b"hello");
        let block = [0x36, b'a', b'b', b'c', 3, 0, 0x00];
        // token 0x36: 3 literals, match length 6+4=10 at offset 3, then an empty last sequence.
        assert_eq!(lz4_block(&block, 13).unwrap(), b"abcabcabcabca");
        assert!(lz4_block(&[0x10, b'a', 5, 0], 100).is_err(), "offset past the start");
        assert!(lz4_block(&[0xf0], 100).is_err(), "truncated length");
    }

    fn mozlz4_of(json: &str) -> Vec<u8> {
        // Literal-only LZ4: token 0xf0 with an extended length.
        let mut b = b"mozLz40\0".to_vec();
        b.extend_from_slice(&(json.len() as u32).to_le_bytes());
        let mut n = json.len() - 15;
        b.push(0xf0);
        while n >= 255 {
            b.push(255);
            n -= 255;
        }
        b.push(n as u8);
        b.extend_from_slice(json.as_bytes());
        b
    }

    #[test]
    fn firefox_session() {
        let json = r#"{"windows":[{"tabs":[
            {"entries":[{"url":"https://a.org/","title":"A"},{"url":"https://b.org/","title":"B"}],"index":1},
            {"entries":[{"url":"about:config","title":"x"}],"index":1},
            {"entries":[{"url":"https://c.org/","title":"C\u001b[2J"}]}
        ]}]}"#;
        let tabs = firefox_tabs(&mozlz4_of(json), "firefox").unwrap();
        let urls: Vec<&str> = tabs.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, ["https://a.org/", "https://c.org/"]);
        assert_eq!(tabs[1].title, "C [2J", "no escape sequences reach a terminal");
        assert!(firefox_tabs(b"not lz4", "firefox").is_err());
    }

    fn cmd(id: u8, body: &[u8]) -> Vec<u8> {
        let mut c = ((body.len() + 1) as u16).to_le_bytes().to_vec();
        c.push(id);
        c.extend_from_slice(body);
        c
    }

    fn ints(v: &[i32]) -> Vec<u8> {
        v.iter().flat_map(|i| i.to_le_bytes()).collect()
    }

    fn nav(tab: i32, index: i32, url: &str, title: &str) -> Vec<u8> {
        let mut p = ints(&[0, tab, index, url.len() as i32]);
        p.extend_from_slice(url.as_bytes());
        p.resize(p.len().div_ceil(4) * 4, 0);
        let t: Vec<u16> = title.encode_utf16().collect();
        p.extend(ints(&[t.len() as i32]));
        p.extend(t.iter().flat_map(|u| u.to_le_bytes()));
        p.resize(p.len().div_ceil(4) * 4, 0);
        let size = (p.len() - 4) as u32;
        p[..4].copy_from_slice(&size.to_le_bytes());
        p
    }

    #[test]
    fn chromium_session() {
        let mut f = b"SNSS".to_vec();
        f.extend(ints(&[3]));
        for c in [
            cmd(SET_TAB_WINDOW, &ints(&[1, 10])),
            cmd(SET_TAB_WINDOW, &ints(&[1, 11])),
            cmd(SET_TAB_WINDOW, &ints(&[2, 12])),
            cmd(SET_TAB_INDEX_IN_WINDOW, &ints(&[10, 1])),
            cmd(SET_TAB_INDEX_IN_WINDOW, &ints(&[11, 0])),
            cmd(UPDATE_TAB_NAVIGATION, &nav(10, 0, "https://old.org/", "Old")),
            cmd(UPDATE_TAB_NAVIGATION, &nav(10, 1, "https://new.org/", "Nëw")),
            cmd(SET_SELECTED_NAVIGATION_INDEX, &ints(&[10, 1])),
            cmd(UPDATE_TAB_NAVIGATION, &nav(11, 0, "https://first.org/", "First")),
            cmd(UPDATE_TAB_NAVIGATION, &nav(12, 0, "https://gone-window.org/", "W")),
            cmd(WINDOW_CLOSED, &ints(&[2, 0, 0])),
            cmd(SET_TAB_WINDOW, &ints(&[1, 13])),
            cmd(UPDATE_TAB_NAVIGATION, &nav(13, 0, "https://closed.org/", "C")),
            cmd(TAB_CLOSED, &ints(&[13, 0, 0])),
        ] {
            f.extend(c);
        }
        f.extend_from_slice(&[200, 0, 6]); // a truncated last command is ignored
        let tabs = snss_tabs(&f, "brave");
        let got: Vec<(&str, &str)> = tabs.iter().map(|t| (t.url.as_str(), t.title.as_str())).collect();
        assert_eq!(got, [("https://first.org/", "First"), ("https://new.org/", "Nëw")]);
        assert!(snss_tabs(b"SNSX\0\0\0\0", "x").is_empty());
    }
}
