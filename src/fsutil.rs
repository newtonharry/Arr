//! Owner-only file handling: private directories (0700), private files (0600), atomic
//! replacement, and bounded reads.

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Fails unless `path` is a real directory (not a symlink) with no group/other permissions.
pub fn check_private_dir(path: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; private directories must be real directories",
            path.display()
        );
    }
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {:o}; private directories must not be accessible to group/other (chmod 700)",
            path.display(),
            mode
        );
    }
    Ok(())
}

/// Creates `path` (and missing parents) with mode 0700 if absent, then checks it.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            // The umask may have narrowed the mode further; never widen it.
        }
        Err(e) => return Err(e).with_context(|| format!("cannot stat {}", path.display())),
    }
    check_private_dir(path)
}

/// Fails unless `path` is a regular file with no group/other permissions.
pub fn check_private_file(path: &Path) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {:o}; private files must not be readable by group/other (chmod 600)",
            path.display(),
            mode
        );
    }
    Ok(())
}

/// Creates a new file (fails if it exists) with mode 0600, writes, and fsyncs it.
pub fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot fsync {}", path.display()))
}

pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("system randomness unavailable: {e}"))?;
    Ok(hex::encode(buf))
}

/// Atomically replaces `path` with `bytes` (temp file in the same dir, fsync, rename, fsync dir).
pub fn atomic_replace_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp: PathBuf = dir.join(format!(".{name}.tmp-{}", random_hex(6)?));
    write_new_private_file(&tmp, bytes)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot replace {}", path.display()));
    }
    fsync_dir(dir)
}

/// Reads a whole file, refusing anything larger than `max` bytes.
pub fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = f.metadata()?.len();
    if len > max {
        bail!("{} is {} bytes; limit is {}", path.display(), len, max);
    }
    let mut buf = Vec::with_capacity(len as usize);
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!(
            "{} grew beyond the {} byte limit while reading",
            path.display(),
            max
        );
    }
    Ok(buf)
}

/// Reads a secret from an owner-only file. The value is trimmed and must be non-empty.
/// Errors name the file, never the contents.
pub fn read_secret_file(path: &Path) -> Result<String> {
    check_private_file(path)?;
    let bytes = read_bounded(path, 4096)?;
    let s = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
    let s = s.trim();
    if s.is_empty() {
        bail!("{} is empty", path.display());
    }
    if s.chars().any(|c| c.is_control()) {
        bail!("{} contains control characters", path.display());
    }
    Ok(s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_and_file_modes_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(check_private_dir(&dir).is_err());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(check_private_dir(&link).is_err());

        let f = dir.join("secret");
        write_new_private_file(&f, b"  s3cret-value \n").unwrap();
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_secret_file(&f).unwrap(), "s3cret-value");
        assert!(
            write_new_private_file(&f, b"x").is_err(),
            "create_new must not overwrite"
        );

        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_secret_file(&f).unwrap_err().to_string();
        assert!(err.contains("chmod 600"));
        assert!(
            !err.contains("s3cret"),
            "errors must not echo secret contents"
        );
    }

    #[test]
    fn atomic_replace_and_bounded_read() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pointer.json");
        atomic_replace_private(&p, b"one").unwrap();
        atomic_replace_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "no temp files left behind"
        );
        assert!(read_bounded(&p, 2).is_err());
        assert_eq!(read_bounded(&p, 3).unwrap(), b"two");
    }
}
