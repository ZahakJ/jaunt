//! The local desktop: open a URL in the browser, show a notification, read and write the
//! clipboard. The daemon may run without the graphical session's environment (started by
//! hand, or by a user manager that was never told), so the display, Wayland socket and
//! session bus are found in the runtime directory when the environment lacks them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_URL: usize = 8192;
pub const MAX_CLIP: usize = 512 * 1024;
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 2000;

/// Only web URLs cross between machines: no `file:`, `javascript:` or custom schemes.
pub fn check_url(url: &str) -> Result<(), String> {
    if url.len() > MAX_URL {
        return Err("the URL is too long".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("the URL contains spaces or control characters".into());
    }
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) || url.len() < 10 {
        return Err("only http:// and https:// URLs are opened".into());
    }
    Ok(())
}

/// Printable text only, at most `max` characters; tabs and newlines become spaces.
pub fn clean_line(s: &str, max: usize) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect::<String>().trim().to_string()
}

/// Display variables a GUI helper needs, from the environment or the runtime directory.
pub fn session_env() -> Vec<(&'static str, String)> {
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut env = Vec::new();
    let wayland = std::env::var("WAYLAND_DISPLAY").ok().filter(|s| !s.is_empty()).or_else(|| run.as_deref().and_then(find_wayland));
    if let Some(w) = wayland {
        env.push(("WAYLAND_DISPLAY", w));
    }
    let x = std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()).or_else(|| find_x_display(Path::new("/tmp/.X11-unix")));
    if let Some(d) = x {
        env.push(("DISPLAY", d));
    }
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|s| !s.is_empty()).or_else(|| {
        let p = run.as_deref()?.join("bus");
        p.exists().then(|| format!("unix:path={}", p.display()))
    });
    if let Some(b) = bus {
        env.push(("DBUS_SESSION_BUS_ADDRESS", b));
    }
    env
}

/// `wayland-<n>` sockets only (not `wayland-1.lock` or other programs' `wayland-1-x.sock`).
fn find_wayland(run: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(run)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.strip_prefix("wayland-").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    names.sort();
    names.into_iter().next()
}

fn find_x_display(dir: &Path) -> Option<String> {
    let mut n: Vec<u32> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok()?.strip_prefix('X')?.parse().ok())
        .collect();
    n.sort();
    n.first().map(|n| format!(":{n}"))
}

fn helper(prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.envs(session_env()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    c
}

async fn run(mut c: Command, what: &str) -> Result<()> {
    let out = tokio::time::timeout(HELPER_TIMEOUT, c.output())
        .await
        .with_context(|| format!("{what} did not finish within 5 s"))?
        .with_context(|| format!("cannot run {what}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{what} failed: {}", clean_line(&err, 200));
    }
    Ok(())
}

pub fn found(prog: &str) -> bool {
    crate::shell::which(prog).is_some()
}

/// Open a web URL in this machine's browser.
pub async fn open_url(url: &str) -> Result<()> {
    check_url(url).map_err(anyhow::Error::msg)?;
    let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut c = helper(prog);
    c.arg(url);
    // xdg-open may wait for the browser it started; a browser already running returns.
    match tokio::time::timeout(HELPER_TIMEOUT, async { c.spawn()?.wait().await }).await {
        Ok(Ok(s)) if !s.success() => bail!("{prog} could not open the URL"),
        Ok(Err(e)) => bail!("cannot run {prog}: {e}"),
        _ => Ok(()),
    }
}

pub async fn notify(title: &str, body: &str) -> Result<()> {
    let title = clean_line(title, MAX_TITLE);
    let body: String = body.chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_BODY).collect();
    if cfg!(target_os = "macos") {
        let mut c = helper("osascript");
        c.args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"]);
        c.arg(&title).arg(&body);
        return run(c, "osascript").await;
    }
    let mut c = helper("notify-send");
    c.args(["-a", "jaunt", "--"]).arg(&title).arg(&body);
    run(c, "notify-send").await
}

/// The clipboard tools on this machine: (read, write) argv.
fn clip_tools() -> Option<(&'static [&'static str], &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        return Some((&["pbpaste"], &["pbcopy"]));
    }
    let wayland = session_env().iter().any(|(k, _)| *k == "WAYLAND_DISPLAY");
    if wayland && found("wl-paste") && found("wl-copy") {
        return Some((&["wl-paste", "--no-newline", "--type", "text"], &["wl-copy", "--type", "text/plain;charset=utf-8"]));
    }
    if found("xclip") {
        return Some((&["xclip", "-selection", "clipboard", "-out"], &["xclip", "-selection", "clipboard", "-in"]));
    }
    if found("xsel") {
        return Some((&["xsel", "--clipboard", "--output"], &["xsel", "--clipboard", "--input"]));
    }
    None
}

pub fn clip_tool_name() -> Option<&'static str> {
    clip_tools().map(|(r, _)| r[0])
}

/// The clipboard's text; `None` when it holds no text.
pub async fn clip_get() -> Result<Option<String>> {
    let Some((read, _)) = clip_tools() else { bail!("no clipboard tool here (wl-clipboard, xclip or xsel)") };
    let mut c = helper(read[0]);
    c.args(&read[1..]).stdout(Stdio::piped());
    let out = tokio::time::timeout(HELPER_TIMEOUT, c.output()).await.context("the clipboard did not answer within 5 s")??;
    if !out.status.success() || out.stdout.len() > MAX_CLIP {
        return Ok(None);
    }
    Ok(String::from_utf8(out.stdout).ok())
}

pub async fn clip_set(text: &str) -> Result<()> {
    if text.len() > MAX_CLIP {
        bail!("the clipboard text is larger than {} KiB", MAX_CLIP / 1024);
    }
    let Some((_, write)) = clip_tools() else { bail!("no clipboard tool here (wl-clipboard, xclip or xsel)") };
    let mut c = helper(write[0]);
    // wl-copy and xclip stay behind to serve the selection: their stdout must not be ours.
    c.args(&write[1..]).stdin(Stdio::piped()).kill_on_drop(false);
    let mut child = c.spawn().with_context(|| format!("cannot run {}", write[0]))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);
    match tokio::time::timeout(HELPER_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) if s.success() => Ok(()),
        Ok(Ok(_)) => bail!("{} failed", write[0]),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => bail!("{} did not finish within 5 s", write[0]),
    }
}

/// Shims for sessions jaunt starts: `xdg-open` first on PATH and `jaunt-browser` as
/// BROWSER, both `jaunt xdg-open`: web URLs go to the machine hopped in from, anything
/// else to the real `xdg-open`.
pub fn shim_dir(paths: &crate::paths::Paths) -> PathBuf {
    paths.state_dir.join("bin")
}

const SHIMS: [&str; 2] = ["xdg-open", "jaunt-browser"];

/// The environment of a new session: like an ssh login, no display of this machine (so
/// `xdg-open` falls back to BROWSER instead of opening a window here), and the shims.
/// Empty when `bridge_urls = false` or the shims cannot be written.
pub fn bridge_env(paths: &crate::paths::Paths, on: bool) -> Vec<(&'static str, String)> {
    if !on {
        return Vec::new();
    }
    let dir = shim_dir(paths);
    let Ok(exe) = std::env::current_exe() else { return Vec::new() };
    let Some(exe) = exe.to_str().filter(|e| !e.contains('\'') && !e.ends_with(" (deleted)")) else { return Vec::new() };
    let script = format!("#!/bin/sh\nexec '{exe}' xdg-open \"$@\"\n");
    for name in SHIMS {
        let shim = dir.join(name);
        if std::fs::read_to_string(&shim).ok().as_deref() != Some(&script) {
            use std::os::unix::fs::PermissionsExt;
            let ok = std::fs::create_dir_all(&dir).is_ok()
                && std::fs::write(&shim, &script).is_ok()
                && std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).is_ok();
            if !ok {
                return Vec::new();
            }
        }
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let mut env = vec![("BROWSER", dir.join("jaunt-browser").display().to_string()), ("PATH", format!("{}:{path}", dir.display()))];
    for k in ["DISPLAY", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"] {
        env.push((k, String::new()));
    }
    env
}

/// The real `xdg-open`: the first on PATH that is not the shim.
pub fn real_xdg_open(paths: &crate::paths::Paths) -> Option<PathBuf> {
    let skip = shim_dir(paths);
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.is_absolute() && *d != skip)
        .map(|d| d.join("xdg-open"))
        .find(|f| f.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls() {
        assert!(check_url("https://example.com/a?b=c#d").is_ok());
        assert!(check_url("HTTP://127.0.0.1:7707/").is_ok());
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "http://a b", "https://x\n", "ftp://x.org", "http://", "-https://x.org"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        assert!(check_url(&format!("https://x.org/{}", "a".repeat(MAX_URL))).is_err());
    }

    #[test]
    fn display_discovery() {
        let d = std::env::temp_dir().join(format!("jaunt-desk-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for n in ["wayland-1.lock", "wayland-1-awww-daemon.sock", "wayland-1", "wayland-3", "bus", "X1_", "X1", "X0"] {
            std::fs::write(d.join(n), "").unwrap();
        }
        assert_eq!(find_wayland(&d).as_deref(), Some("wayland-1"));
        assert_eq!(find_x_display(&d).as_deref(), Some(":0"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn lines_are_cleaned() {
        assert_eq!(clean_line("a\tb\x1b[31m\n", 100), "a b [31m");
        assert_eq!(clean_line("abcdef", 3), "abc");
    }
}
