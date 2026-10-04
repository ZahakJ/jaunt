//! `jaunt tmux-setup`: write the tmux snippet into jaunt's config directory and say the
//! one line to add. It edits no tmux configuration itself.

use anyhow::{Context, Result};

use super::theme::{self, Style};
use crate::paths::Paths;

const SNIPPET: &str = include_str!("../../contrib/tmux/jaunt.tmux");

/// Use this binary's absolute path in the popup when it is a plain path, so the popup
/// works even when `jaunt` is not on tmux's PATH.
fn snippet() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| p.to_str().map(str::to_string));
    match exe {
        Some(p) if p.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b)) => {
            SNIPPET.replace("\"jaunt pick\"", &format!("\"{p} pick\""))
        }
        _ => SNIPPET.to_string(),
    }
}

pub fn setup(paths: &Paths) -> Result<i32> {
    std::fs::create_dir_all(&paths.config_dir).with_context(|| format!("cannot create {}", paths.config_dir.display()))?;
    let file = paths.config_dir.join("jaunt.tmux");
    let tmp = file.with_extension("tmux.tmp");
    std::fs::write(&tmp, snippet()).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &file)?;
    let shown = match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(h) if file.starts_with(&h) && paths.jaunt_home.is_none() => format!("~/{}", file.strip_prefix(&h)?.display()),
        _ => file.display().to_string(),
    };
    let g = theme::glyphs();
    println!("{} wrote {shown}", theme::paint(&g.check.to_string(), Style::fg(theme::GREEN).bold()));
    println!();
    println!("Add this line to your tmux.conf (rihla users: ~/.config/rihla/tmux/local.conf):");
    println!();
    println!("    {}", theme::paint(&format!("source-file {shown}"), Style::fg(theme::WHITE).bold()));
    println!();
    println!("then reload tmux. {} prefix J  the mesh in a popup; Enter opens a window per machine", g.dot);
    println!("                 {} F12       keys go to the remote tmux; F12 again to come back", g.dot);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_binds_only_what_it_says() {
        let s = SNIPPET;
        assert!(s.contains("bind-key J display-popup"));
        assert!(s.contains("F12"));
        for bad in ["C-h", "C-j", "C-k", "C-l", "set-option -g prefix", "set -g prefix"] {
            assert!(!s.contains(bad), "snippet must not touch {bad}");
        }
    }
}
