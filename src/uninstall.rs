//! `jaunt uninstall`: stop the service, remove the unit, the binary, the config and the
//! state; ask before deleting the key and roster (the machine's identity in the mesh).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::paths::Paths;
use crate::{ipc, systemd, term};

/// Where install.sh puts the binary: `$JAUNT_HOME/bin` in test mode, else `~/.local/bin`.
fn bin_dir(paths: &Paths) -> Option<PathBuf> {
    if let Some(h) = &paths.jaunt_home {
        return Some(h.join("bin"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute())?;
    Some(home.join(".local/bin"))
}

fn remove_dir(dir: &Path, removed: &mut Vec<String>) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            removed.push(dir.display().to_string());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", dir.display())),
    }
}

fn remove_file(f: &Path, removed: &mut Vec<String>) -> Result<()> {
    match std::fs::remove_file(f) {
        Ok(()) => {
            removed.push(f.display().to_string());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", f.display())),
    }
}

fn ask(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// `delete_identity`: Some(true) = --yes (delete without asking), Some(false) = --keep-key,
/// None = ask on a terminal, keep otherwise.
pub async fn run(paths: &Paths, delete_identity: Option<bool>) -> Result<i32> {
    // Learn the name and whether there is a mesh before the daemon goes away.
    // (Only ask a running daemon: a status call would otherwise start one.)
    let snapshot = match ipc::connect_existing(paths).await {
        Some(_) => crate::status(paths).await.ok(),
        None => None,
    };
    // peers: None when unknown (daemon not running).
    let (name, peers) = match snapshot {
        Some(s) => (s.me.name.clone(), Some(s.peers.len())),
        None => (Config::load(paths).ok().and_then(|c| c.name).unwrap_or_else(crate::config::default_name), None),
    };

    let mut removed = Vec::new();
    if systemd::unit_installed(paths)
        && let Err(e) = systemd::down_unit()
    {
        println!("could not stop the service through systemd ({e:#}); stopping the daemon directly");
    }
    if crate::stop_daemon(paths).await.context("cannot stop the daemon; nothing was removed")? {
        println!("daemon stopped");
    }

    let unit = paths.unit_file();
    remove_file(&unit, &mut removed)?;
    if paths.jaunt_home.is_none() && removed.iter().any(|r| r == &unit.display().to_string()) {
        let _ = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    }

    let has_identity = paths.key_file().exists() || paths.roster_file().exists();
    let delete_key = match delete_identity {
        Some(d) => d,
        None if has_identity && term::stdin_is_tty() => ask(&format!(
            "Delete this machine's key and roster in {}? Without them it cannot rejoin as the same machine.",
            paths.data_dir.display()
        ))?,
        None => false,
    };

    remove_dir(&paths.config_dir, &mut removed)?;
    remove_dir(&paths.state_dir, &mut removed)?;
    remove_dir(&paths.runtime_dir, &mut removed)?;
    if delete_key {
        remove_dir(&paths.data_dir, &mut removed)?;
    }

    // The binary last: only the one install.sh put in place, and only if it is this one.
    if let Some(dir) = bin_dir(paths) {
        let installed = dir.join("jaunt");
        let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
        let target = installed.canonicalize().ok();
        if target.is_some() && target == me {
            remove_file(&installed, &mut removed)?;
        } else if installed.exists() {
            println!("left {} in place: it is not the binary running this command", installed.display());
        } else if let Some(me) = me {
            println!("left {} in place: install.sh did not put it there", me.display());
        }
    }
    if let Some(h) = &paths.jaunt_home {
        let _ = std::fs::remove_dir(h.join("bin"));
        let _ = std::fs::remove_dir(h);
    }

    for r in &removed {
        println!("removed {r}");
    }
    if has_identity && !delete_key {
        println!("kept the key and roster in {} (delete that directory to finish)", paths.data_dir.display());
    }
    println!("tmux sessions on this machine are left running; end them with tmux if you want");
    println!("if you sourced jaunt.tmux from your tmux config, remove that line");
    if peers.map_or(has_identity, |n| n > 0) {
        println!("on another machine in the mesh, revoke this one: jaunt unlink {name}");
    }
    Ok(0)
}

/// Refuse obviously wrong combinations early.
pub fn check_flags(yes: bool, keep_key: bool) -> Result<Option<bool>> {
    match (yes, keep_key) {
        (true, true) => bail!("--yes and --keep-key contradict each other"),
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}
