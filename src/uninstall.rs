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

/// What jaunt itself writes in each of its directories. Uninstall deletes these names and
/// nothing else, then removes the directory only if that left it empty: a directory
/// that is not jaunt's alone (a `JAUNT_HOME` pointed at `$HOME`, say) keeps everything else.
pub const CONFIG_FILES: &[&str] = &["config.toml", "jaunt.tmux"];
pub const STATE_FILES: &[&str] = &[
    "daemon.log", "daemon.log.1", "events.jsonl", "events.jsonl.1", "tmux-hint-shown", "addrs.json", "tmux-owners.json",
];
pub const RUNTIME_FILES: &[&str] = &["sock", "daemon.lock"];
pub const DATA_FILES: &[&str] = &["key", "roster.json", "daemon.lock"];

/// Remove `files` (and jaunt's own temp files) from `dir`, then `dir` itself if empty.
fn remove_known(dir: &Path, files: &[&str], removed: &mut Vec<String>) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        // write_private_atomic's temp files: ".<name>.tmp.<pid>" for one of ours.
        let ours_tmp = name
            .strip_prefix('.')
            .and_then(|n| n.split_once(".tmp."))
            .is_some_and(|(base, pid)| files.contains(&base) && pid.bytes().all(|b| b.is_ascii_digit()));
        if !(files.contains(&name.as_str()) || ours_tmp) {
            continue;
        }
        let p = ent.path();
        // Never follow or remove a directory under one of these names.
        if std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(true) {
            continue;
        }
        remove_file(&p, removed)?;
    }
    if std::fs::remove_dir(dir).is_ok() {
        removed.push(dir.display().to_string());
    }
    Ok(())
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

    // The unit's directory first (it may sit inside config_dir under JAUNT_HOME).
    if paths.jaunt_home.is_some() {
        let _ = std::fs::remove_dir(&paths.systemd_user_dir);
        if let Some(p) = paths.systemd_user_dir.parent() {
            let _ = std::fs::remove_dir(p);
        }
    }
    remove_known(&paths.config_dir, CONFIG_FILES, &mut removed)?;
    remove_known(&paths.runtime_dir, RUNTIME_FILES, &mut removed)?;
    remove_known(&paths.state_dir, STATE_FILES, &mut removed)?;
    if delete_key {
        remove_known(&paths.data_dir, DATA_FILES, &mut removed)?;
    } else {
        // Only the lock: key and roster stay.
        remove_known(&paths.data_dir, &["daemon.lock"], &mut removed)?;
    }
    for (dir, what) in [(&paths.config_dir, "config"), (&paths.state_dir, "state"), (&paths.runtime_dir, "runtime")] {
        if dir.exists() {
            println!("left {} in place: it holds files that are not jaunt's ({what} directory)", dir.display());
        }
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
    // What the user installed as root stays until undone as root.
    for (file, script) in [
        ("/etc/polkit-1/rules.d/49-jaunt-host.rules", "host-polkit-rule.sh"),
        ("/etc/systemd/logind.conf.d/50-jaunt-lid-on-ac.conf", "logind-lid-on-ac.sh"),
    ] {
        if Path::new(file).exists() {
            println!(
                "{file} is still installed (it was added as root from jaunt's scripts/root-extras); \
                 undo it with: sudo sh scripts/root-extras/{script} undo   (or: sudo rm {file})"
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A JAUNT_HOME that is not jaunt's alone keeps everything that is not jaunt's.
    #[test]
    fn only_jaunts_own_files_go() {
        let home = std::env::temp_dir().join(format!("jaunt-uninst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let p = Paths::under(&home);
        for (dir, f) in [(&p.config_dir, "config.toml"), (&p.state_dir, "events.jsonl"), (&p.state_dir, ".addrs.json.tmp.123"), (&p.runtime_dir, "sock")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::create_dir_all(p.config_dir.join("someapp")).unwrap();
        std::fs::write(p.config_dir.join("someapp/settings.ini"), b"keep").unwrap();
        std::fs::write(p.state_dir.join("history"), b"keep").unwrap();
        std::fs::create_dir_all(p.data_dir.join("photos")).unwrap();
        std::fs::write(p.data_dir.join("photos/img.txt"), b"keep").unwrap();
        std::fs::write(p.data_dir.join("key"), b"k").unwrap();
        let mut removed = Vec::new();
        remove_known(&p.config_dir, CONFIG_FILES, &mut removed).unwrap();
        remove_known(&p.state_dir, STATE_FILES, &mut removed).unwrap();
        remove_known(&p.runtime_dir, RUNTIME_FILES, &mut removed).unwrap();
        remove_known(&p.data_dir, DATA_FILES, &mut removed).unwrap();
        assert!(!p.config_dir.join("config.toml").exists());
        assert!(!p.state_dir.join("events.jsonl").exists());
        assert!(!p.state_dir.join(".addrs.json.tmp.123").exists());
        assert!(!p.runtime_dir.exists(), "an empty jaunt directory goes");
        assert!(!p.data_dir.join("key").exists());
        assert_eq!(std::fs::read(p.config_dir.join("someapp/settings.ini")).unwrap(), b"keep");
        assert_eq!(std::fs::read(p.state_dir.join("history")).unwrap(), b"keep");
        assert_eq!(std::fs::read(p.data_dir.join("photos/img.txt")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&home);
    }
}
