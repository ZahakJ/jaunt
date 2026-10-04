//! Small, bounded readers for sysfs / procfs attribute files.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Attribute files are tiny; anything longer is not what we expect.
const MAX_ATTR: u64 = 256;
/// Upper bound on directory entries looked at in one class directory.
pub const MAX_ENTRIES: usize = 128;

/// Where the guard reads the kernel's view of the machine. Tests point these at fake trees.
#[derive(Debug, Clone)]
pub struct Roots {
    pub sys: PathBuf,
    pub proc: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Roots { sys: PathBuf::from("/sys"), proc: PathBuf::from("/proc") }
    }
}

/// `Ok(None)` when the file does not exist; `Err` when it exists but cannot be read.
pub fn read_attr(path: &Path) -> Result<Option<String>, String> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut buf = Vec::new();
    f.take(MAX_ATTR).read_to_end(&mut buf).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&buf).trim().to_string()))
}

pub fn read_int(path: &Path) -> Result<Option<i64>, String> {
    match read_attr(path)? {
        None => Ok(None),
        Some(s) => s.parse::<i64>().map(Some).map_err(|_| format!("{} holds '{s}', not a number", path.display())),
    }
}

/// Sorted entries of a directory (at most `MAX_ENTRIES`). `Ok(None)` if it does not exist.
pub fn list_dir(dir: &Path) -> Result<Option<Vec<(String, PathBuf)>>, String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for ent in rd.take(MAX_ENTRIES) {
        let ent = ent.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        out.push((ent.file_name().to_string_lossy().into_owned(), ent.path()));
    }
    out.sort();
    Ok(Some(out))
}
