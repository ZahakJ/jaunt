//! Filesystem side of `jaunt cp`: walking a source tree, and writing a received tree into
//! a destination without ever writing outside it.
//!
//! Entry paths are relative, `/`-separated, made of plain components only ("" is the root).
//! Before anything is written, every directory between the copy's root and the entry is
//! checked with lstat to be a real directory (not a symlink), so a received symlink can
//! never redirect a later entry out of the destination.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MAX_PATH: usize = 4096;
pub const MAX_COMPONENT: usize = 255;
pub const MAX_DEPTH: usize = 128;
pub const MAX_ENTRIES: u64 = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    /// Permission bits only (setuid, setgid and sticky are never carried).
    pub mode: u32,
    #[serde(default)]
    pub size: u64,
    /// Seconds since the epoch (files only).
    #[serde(default)]
    pub mtime: Option<i64>,
    /// Symlinks only.
    #[serde(default)]
    pub target: Option<String>,
}

/// One thing the sender could not send (it says why; the copy goes on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

pub struct Plan {
    /// The name the tree gets inside a destination directory.
    pub name: String,
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skipped>,
    pub files: u64,
    pub bytes: u64,
}

/// Split and check an entry path. `""` is the root.
pub fn components(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if path.len() > MAX_PATH {
        bail!("a path in the copy is longer than {MAX_PATH} bytes");
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_DEPTH {
        bail!("the copy is nested deeper than {MAX_DEPTH} directories");
    }
    for p in &parts {
        if p.is_empty() || *p == "." || *p == ".." || p.len() > MAX_COMPONENT || p.contains('\0') {
            bail!("refusing the path '{}': it could point outside the destination", path.escape_debug());
        }
    }
    Ok(parts)
}

fn valid_name(name: &str) -> bool {
    components(name).map(|c| c.len() == 1).unwrap_or(false)
}

fn mtime_of(m: &fs::Metadata) -> Option<i64> {
    Some(m.mtime())
}

/// Walk `src` (following it if it is itself a symlink; links inside are kept as links).
pub fn plan(src: &Path) -> Result<Plan> {
    let meta = fs::metadata(src).with_context(|| format!("cannot read {}", src.display()))?;
    let name = match src.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => fs::canonicalize(src)?.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    if !valid_name(&name) {
        bail!("cannot copy {}: it has no usable name", src.display());
    }
    let mut p = Plan { name, root: src.to_path_buf(), entries: Vec::new(), skipped: Vec::new(), files: 0, bytes: 0 };
    if meta.is_dir() {
        p.entries.push(Entry { path: String::new(), kind: Kind::Dir, mode: meta.mode() & 0o777, size: 0, mtime: None, target: None });
        walk(src, "", 1, &mut p)?;
    } else if meta.is_file() {
        p.files = 1;
        p.bytes = meta.len();
        p.entries.push(Entry { path: String::new(), kind: Kind::File, mode: meta.mode() & 0o777, size: meta.len(), mtime: mtime_of(&meta), target: None });
    } else {
        bail!("{} is not a regular file or directory", src.display());
    }
    Ok(p)
}

fn walk(dir: &Path, rel: &str, depth: usize, p: &mut Plan) -> Result<()> {
    let mut names: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect(),
        Err(e) => {
            p.skipped.push(Skipped { path: rel.to_string(), reason: format!("cannot list it: {e}") });
            return Ok(());
        }
    };
    names.sort();
    for os in names {
        let child_rel_display = format!("{rel}{}{}", if rel.is_empty() { "" } else { "/" }, os.to_string_lossy());
        let Some(name) = os.to_str() else {
            p.skipped.push(Skipped { path: child_rel_display, reason: "the name is not UTF-8".into() });
            continue;
        };
        let child_rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        if components(&child_rel).is_err() {
            p.skipped.push(Skipped { path: child_rel, reason: "path too long or too deep".into() });
            continue;
        }
        if p.entries.len() as u64 >= MAX_ENTRIES {
            bail!("more than {MAX_ENTRIES} entries; copy it in parts");
        }
        let abs = dir.join(&os);
        let m = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) => {
                p.skipped.push(Skipped { path: child_rel, reason: e.to_string() });
                continue;
            }
        };
        let ft = m.file_type();
        if ft.is_dir() {
            p.entries.push(Entry { path: child_rel.clone(), kind: Kind::Dir, mode: m.mode() & 0o777, size: 0, mtime: None, target: None });
            if depth + 1 > MAX_DEPTH {
                p.skipped.push(Skipped { path: child_rel, reason: "nested too deep".into() });
                continue;
            }
            walk(&abs, &child_rel, depth + 1, p)?;
        } else if ft.is_file() {
            p.files += 1;
            p.bytes += m.len();
            p.entries.push(Entry { path: child_rel, kind: Kind::File, mode: m.mode() & 0o777, size: m.len(), mtime: mtime_of(&m), target: None });
        } else if ft.is_symlink() {
            match fs::read_link(&abs).ok().and_then(|t| t.to_str().map(str::to_string)) {
                Some(t) if t.len() <= MAX_PATH && !t.contains('\0') => {
                    p.entries.push(Entry { path: child_rel, kind: Kind::Symlink, mode: 0o777, size: 0, mtime: None, target: Some(t) })
                }
                _ => p.skipped.push(Skipped { path: child_rel, reason: "unreadable or non-UTF-8 link target".into() }),
            }
        } else {
            p.skipped.push(Skipped { path: child_rel, reason: "not a file, directory or symlink".into() });
        }
    }
    Ok(())
}

/// Path of an entry on the sending side.
pub fn source_path(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() { root.to_path_buf() } else { root.join(rel) }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes a received tree under `target`, refusing anything that would land elsewhere.
pub struct Writer {
    dest: PathBuf,
    target: Option<PathBuf>,
    /// Directories whose final mode is applied at the end (so a read-only directory can
    /// still be filled first), deepest last.
    dirs: Vec<(PathBuf, u32)>,
    /// The name the receiver asked for (a pull of `machine:dir/x` expects `x`), if known.
    expect: Option<String>,
    pub files: u64,
    pub bytes: u64,
}

/// A file being received: data goes to a temp file next to it, renamed into place at the end.
pub struct Incoming {
    pub tmp: PathBuf,
    pub dest: PathBuf,
    pub file: fs::File,
    pub left: u64,
    pub mode: u32,
    pub mtime: Option<i64>,
}

impl Writer {
    /// `dest`: where the user asked the tree to go. If it is an existing directory the
    /// tree goes inside it under the sender's name, otherwise it becomes `dest`.
    pub fn new(dest: PathBuf) -> Writer {
        Writer { dest, target: None, dirs: Vec::new(), expect: None, files: 0, bytes: 0 }
    }

    /// Refuse a tree the sender names differently from what was asked for, so a peer
    /// cannot answer a pull of `x` with, say, `.bashrc`.
    pub fn expect_name(&mut self, name: Option<String>) {
        self.expect = name;
    }

    pub fn set_name(&mut self, name: &str) -> Result<()> {
        if !valid_name(name) {
            bail!("the sender named the copy '{}', which is not a plain file name", name.escape_debug());
        }
        if let Some(want) = &self.expect
            && want != name
        {
            bail!("asked for '{want}' but the sender sent '{}'", name.escape_debug());
        }
        let t = match fs::metadata(&self.dest) {
            Ok(m) if m.is_dir() => self.dest.join(name),
            _ => self.dest.clone(),
        };
        self.target = Some(t);
        Ok(())
    }

    #[cfg(test)]
    pub fn target(&self) -> Option<&Path> {
        self.target.as_deref()
    }

    /// Resolve an entry to its path, checking every directory between the copy's root and
    /// it. The root itself must be (for a non-root entry) a real directory as well.
    fn place(&self, rel: &str) -> Result<PathBuf> {
        let Some(target) = &self.target else { bail!("the sender did not start with a plan") };
        let parts = components(rel)?;
        let mut cur = target.clone();
        for part in parts {
            let m = fs::symlink_metadata(&cur).with_context(|| format!("{} is missing", cur.display()))?;
            if !m.file_type().is_dir() {
                bail!("{} is not a directory (a symlink is never followed here)", cur.display());
            }
            cur.push(part);
        }
        Ok(cur)
    }

    pub fn dir(&mut self, e: &Entry) -> Result<()> {
        let path = self.place(&e.path)?;
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(m) if m.file_type().is_symlink() => bail!("{} is a symbolic link; not copying a directory through it", path.display()),
            Ok(_) => bail!("{} exists and is not a directory", path.display()),
            Err(_) => {
                fs::DirBuilder::new().mode(0o700).create(&path).with_context(|| format!("cannot create {}", path.display()))?;
            }
        }
        if self.dirs.len() as u64 >= MAX_ENTRIES {
            bail!("too many directories");
        }
        self.dirs.push((path, e.mode & 0o777));
        Ok(())
    }

    pub fn symlink(&mut self, e: &Entry) -> Result<()> {
        let path = self.place(&e.path)?;
        let Some(t) = e.target.as_deref().filter(|t| !t.is_empty() && t.len() <= MAX_PATH && !t.contains('\0')) else {
            bail!("a symbolic link in the copy has no usable target");
        };
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_dir() => bail!("{} is a directory; not replacing it with a link", path.display()),
            Ok(_) => fs::remove_file(&path)?,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(t, &path).with_context(|| format!("cannot create the link {}", path.display()))
    }

    pub fn open_file(&mut self, e: &Entry) -> Result<Incoming> {
        let path = self.place(&e.path)?;
        if let Ok(m) = fs::symlink_metadata(&path)
            && m.file_type().is_dir()
        {
            bail!("{} is a directory; not replacing it with a file", path.display());
        }
        let parent = path.parent().context("no parent directory")?;
        let tmp = parent.join(format!(".jaunt-cp.{}.{}.tmp", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot write in {}", parent.display()))?;
        Ok(Incoming { tmp, dest: path, file, left: e.size, mode: e.mode & 0o777, mtime: e.mtime })
    }

    /// Mode, mtime, then rename over the destination (a symlink there is replaced, not followed).
    pub fn close_file(&mut self, f: Incoming, size: u64) -> Result<()> {
        let res = (|| -> Result<()> {
            f.file.set_permissions(fs::Permissions::from_mode(f.mode))?;
            if let Some(t) = f.mtime
                && t >= 0
            {
                let _ = f.file.set_modified(UNIX_EPOCH + Duration::from_secs(t as u64));
            }
            drop(f.file);
            fs::rename(&f.tmp, &f.dest).with_context(|| format!("cannot write {}", f.dest.display()))
        })();
        if res.is_err() {
            let _ = fs::remove_file(&f.tmp);
        }
        res?;
        self.files += 1;
        self.bytes += size;
        Ok(())
    }

    /// Apply directory modes, deepest first.
    pub fn finish(&mut self) -> Result<()> {
        for (path, mode) in self.dirs.drain(..).rev() {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).with_context(|| format!("cannot set the mode of {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp"));
        let d = base.join(format!("tree-{tag}-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rejects_traversal() {
        for bad in ["..", "a/../b", "/etc", "a//b", "./a", "a/.", "a/", "x\0y"] {
            assert!(components(bad).is_err(), "{bad}");
        }
        assert_eq!(components("a/b c/d").unwrap(), vec!["a", "b c", "d"]);
        assert!(components("").unwrap().is_empty());
        assert!(components(&"a/".repeat(MAX_DEPTH + 1)[..]).is_err());
        assert!(!valid_name("a/b") && !valid_name("..") && !valid_name("") && valid_name("x.txt"));
    }

    #[test]
    fn symlink_in_tree_cannot_redirect_writes() {
        let base = tmpdir("redirect");
        let outside = base.join("outside");
        fs::create_dir(&outside).unwrap();
        let dest = base.join("dest");
        fs::create_dir(&dest).unwrap();
        let mut w = Writer::new(dest.clone());
        w.set_name("tree").unwrap();
        w.dir(&Entry { path: "".into(), kind: Kind::Dir, mode: 0o755, size: 0, mtime: None, target: None }).unwrap();
        // A link pointing outside, then a file "through" it.
        let link = Entry { path: "esc".into(), kind: Kind::Symlink, mode: 0o777, size: 0, mtime: None, target: Some(outside.display().to_string()) };
        w.symlink(&link).unwrap();
        let through = Entry { path: "esc/pwned".into(), kind: Kind::File, mode: 0o644, size: 1, mtime: None, target: None };
        assert!(w.open_file(&through).is_err());
        assert!(w.dir(&Entry { path: "esc/d".into(), kind: Kind::Dir, mode: 0o755, size: 0, mtime: None, target: None }).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none(), "nothing may appear outside");
        // An existing symlink at the root target is not written through either.
        let dest2 = base.join("dest2");
        fs::create_dir(&dest2).unwrap();
        std::os::unix::fs::symlink(&outside, dest2.join("tree")).unwrap();
        let mut w2 = Writer::new(dest2);
        w2.set_name("tree").unwrap();
        assert!(w2.dir(&Entry { path: "".into(), kind: Kind::Dir, mode: 0o755, size: 0, mtime: None, target: None }).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn roundtrip_keeps_modes_and_links() {
        let base = tmpdir("roundtrip");
        let src = base.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::File::create(src.join("run.sh")).unwrap().write_all(b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::File::create(src.join("sub/data")).unwrap().write_all(&[7u8; 1000]).unwrap();
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o500)).unwrap();
        std::os::unix::fs::symlink("run.sh", src.join("link")).unwrap();
        let plan = plan(&src).unwrap();
        assert_eq!(plan.name, "src");
        assert_eq!(plan.files, 2);
        assert_eq!(plan.bytes, 1018);
        let dest = base.join("out");
        fs::create_dir(&dest).unwrap();
        let mut w = Writer::new(dest.clone());
        w.set_name(&plan.name).unwrap();
        for e in &plan.entries {
            match e.kind {
                Kind::Dir => w.dir(e).unwrap(),
                Kind::Symlink => w.symlink(e).unwrap(),
                Kind::File => {
                    let data = fs::read(source_path(&plan.root, &e.path)).unwrap();
                    let mut f = w.open_file(e).unwrap();
                    f.file.write_all(&data).unwrap();
                    w.close_file(f, data.len() as u64).unwrap();
                }
            }
        }
        w.finish().unwrap();
        let got = dest.join("src");
        assert_eq!(fs::read(got.join("sub/data")).unwrap(), vec![7u8; 1000]);
        assert_eq!(fs::metadata(got.join("run.sh")).unwrap().mode() & 0o777, 0o750);
        assert_eq!(fs::metadata(got.join("sub")).unwrap().mode() & 0o777, 0o500);
        assert_eq!(fs::read_link(got.join("link")).unwrap(), PathBuf::from("run.sh"));
        for d in [src.join("sub"), got.join("sub")] {
            fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn file_into_new_name_and_into_dir() {
        let base = tmpdir("names");
        let mut w = Writer::new(base.join("renamed.txt"));
        w.set_name("orig.txt").unwrap();
        assert_eq!(w.target().unwrap(), base.join("renamed.txt"));
        let mut w = Writer::new(base.clone());
        w.set_name("orig.txt").unwrap();
        assert_eq!(w.target().unwrap(), base.join("orig.txt"));
        assert!(Writer::new(base.clone()).set_name("../x").is_err());
        fs::remove_dir_all(&base).unwrap();
    }
}
