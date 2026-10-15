//! The machine's secret key: 32 raw bytes in a 0600 file. A key file that other users can
//! read is refused rather than silently fixed, so the user notices.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use anyhow::{Context, Result, bail};
use iroh::SecretKey;

use crate::paths::{Paths, private_dir};

/// Load the key, or `None` when there is none yet.
pub fn load(paths: &Paths) -> Result<Option<SecretKey>> {
    let path = paths.key_file();
    let mut f = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let meta = f.metadata()?;
    if meta.mode() & 0o077 != 0 {
        bail!(
            "the key file {} can be read by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            meta.mode() & 0o777,
            path.display()
        );
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        bail!("the key file {} belongs to another user", path.display());
    }
    let mut buf = Vec::with_capacity(33);
    (&mut f).take(33).read_to_end(&mut buf)?;
    let bytes: [u8; 32] = buf
        .try_into()
        .map_err(|_| anyhow::anyhow!("the key file {} is not 32 bytes long", path.display()))?;
    Ok(Some(SecretKey::from_bytes(&bytes)))
}

/// Load the key, generating and saving a new one if there is none.
pub fn load_or_create(paths: &Paths) -> Result<SecretKey> {
    if let Some(k) = load(paths)? {
        return Ok(k);
    }
    create(paths)
}

/// Generate a fresh key and write it with `create_new`, so an existing key is never overwritten.
pub fn create(paths: &Paths) -> Result<SecretKey> {
    private_dir(&paths.data_dir)?;
    let path = paths.key_file();
    let key = SecretKey::generate();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    f.write_all(&key.to_bytes())?;
    f.sync_all()?;
    Ok(key)
}

/// Remove the key file (used by `jaunt reset`).
pub fn remove(paths: &Paths) -> Result<()> {
    match fs::remove_file(paths.key_file()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn tmp_paths(tag: &str) -> Paths {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("target/tmp/unit-ident-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Paths::under(&dir)
    }

    #[test]
    fn create_load_roundtrip_and_mode() {
        let p = tmp_paths("rt");
        let k = load_or_create(&p).unwrap();
        let mode = fs::metadata(p.key_file()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let k2 = load(&p).unwrap().unwrap();
        assert_eq!(k.public().as_bytes(), k2.public().as_bytes());
        let _ = fs::remove_dir_all(p.jaunt_home.unwrap());
    }

    #[test]
    fn refuses_group_readable_key() {
        let p = tmp_paths("perm");
        load_or_create(&p).unwrap();
        fs::set_permissions(p.key_file(), fs::Permissions::from_mode(0o640)).unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        let _ = fs::remove_dir_all(p.jaunt_home.unwrap());
    }

    #[test]
    fn refuses_wrong_length() {
        let p = tmp_paths("len");
        private_dir(&p.data_dir).unwrap();
        fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(p.key_file()).unwrap().write_all(&[1u8; 31]).unwrap();
        assert!(load(&p).is_err());
        let _ = fs::remove_dir_all(p.jaunt_home.unwrap());
    }
}
