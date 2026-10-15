//! Where jaunt keeps its files. `JAUNT_HOME` relocates everything under one directory
//! (tests and containers use it); otherwise the XDG base directories decide.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Paths {
    /// Set when `JAUNT_HOME` is in effect (test / container mode).
    pub jaunt_home: Option<PathBuf>,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub systemd_user_dir: PathBuf,
}

fn abs_env(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    let p = PathBuf::from(v);
    // The XDG spec says relative values are invalid and must be ignored.
    p.is_absolute().then_some(p)
}

impl Paths {
    pub fn from_env() -> Result<Paths> {
        if let Some(h) = std::env::var_os("JAUNT_HOME").filter(|v| !v.is_empty()) {
            let home = PathBuf::from(h);
            // A relative JAUNT_HOME would mean a different home in every directory.
            if !home.is_absolute() {
                bail!("JAUNT_HOME must be an absolute path (it is {})", home.display());
            }
            return Ok(Paths::under(&home));
        }
        let home = abs_env("HOME").context("HOME is not set to an absolute path")?;
        let config_home = abs_env("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let data_home = abs_env("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let state_home = abs_env("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let state_dir = state_home.join("jaunt");
        let runtime_dir = match abs_env("XDG_RUNTIME_DIR") {
            Some(r) => r.join("jaunt"),
            None => state_dir.join("run"),
        };
        Ok(Paths {
            jaunt_home: None,
            config_dir: config_home.join("jaunt"),
            data_dir: data_home.join("jaunt"),
            state_dir,
            runtime_dir,
            systemd_user_dir: config_home.join("systemd/user"),
        })
    }

    pub fn under(home: &Path) -> Paths {
        Paths {
            jaunt_home: Some(home.to_path_buf()),
            config_dir: home.join("config"),
            data_dir: home.join("data"),
            state_dir: home.join("state"),
            runtime_dir: home.join("run"),
            systemd_user_dir: home.join("config/systemd/user"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn key_file(&self) -> PathBuf {
        self.data_dir.join("key")
    }
    pub fn roster_file(&self) -> PathBuf {
        self.data_dir.join("roster.json")
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("sock")
    }
    /// The single-instance lock lives next to the key: one daemon per identity, even
    /// when two contexts disagree about XDG_RUNTIME_DIR. It records the daemon's socket.
    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("daemon.lock")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }
    pub fn events_file(&self) -> PathBuf {
        self.state_dir.join("events.jsonl")
    }
    pub fn unit_file(&self) -> PathBuf {
        self.systemd_user_dir.join("jaunt.service")
    }
}

/// Create `dir` (and parents) and make sure the leaf is a real directory of this user,
/// mode 0700. A symlink or a directory owned by someone else is refused.
pub fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    check_owned_dir(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot set permissions on {}", dir.display()))?;
    Ok(())
}

/// `dir` is a directory (not a symlink to one) owned by this user.
pub fn check_owned_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(dir).with_context(|| format!("cannot look at {}", dir.display()))?;
    if m.file_type().is_symlink() {
        bail!("{} is a symbolic link; jaunt keeps its files only in a real directory of its own", dir.display());
    }
    if !m.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let me = rustix::process::getuid().as_raw();
    if m.uid() != me {
        bail!("{} belongs to another user (uid {}); refusing to use it", dir.display(), m.uid());
    }
    Ok(())
}

/// Write `bytes` to `path` atomically (temp file in the same directory, fsync, rename),
/// with mode 0600.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    private_dir(dir)?;
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    let res = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("cannot write {}", path.display()))
}

/// Read a whole file, refusing anything larger than `max` bytes.
pub fn read_capped(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!("{} is larger than {} bytes; refusing to read it", path.display(), max);
    }
    Ok(Some(buf))
}
