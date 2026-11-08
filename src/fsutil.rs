//! Owner-only file handling: private directories (0700), private files (0600), atomic
//! replacement, and bounded reads.
//!
//! On Windows there are no mode bits: privacy comes from the folder's access list. The
//! state folder lives in the user's profile, which only that user (and administrators) can
//! open, and `install.ps1` also strips inherited access from it.

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
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
    #[cfg(unix)]
    {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} has mode {:o}; private directories must not be accessible to group/other (chmod 700)",
                path.display(),
                mode
            );
        }
    }
    Ok(())
}

/// `text` on one line (line breaks become spaces), so installers can show an error as it is.
pub fn one_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Creates `path` (and missing parents) with mode 0700 if absent, then checks it.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = fs::DirBuilder::new();
            #[cfg(unix)]
            b.mode(0o700);
            b.recursive(true)
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
    #[cfg(unix)]
    {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} has mode {:o}; private files must not be readable by group/other (chmod 600)",
                path.display(),
                mode
            );
        }
    }
    Ok(())
}

/// Creates a new file (fails if it exists) with mode 0600, writes, and fsyncs it.
pub fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Makes a rename or new file in `path` durable. Windows can't open a folder this way, and
/// NTFS journals renames, so there it does nothing.
pub fn fsync_dir(path: &Path) -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    File::open(path)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot fsync {}", path.display()))
}

/// A file's (device, inode) without following a final symlink. On Windows: the volume
/// serial number and the NTFS file index, which identify a file the same way.
pub fn file_id(path: &Path) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(path)?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            GetFileInformationByHandle,
        };
        // Metadata only (no read access), folders too, and the link itself, not its target.
        let f = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid open handle and a zeroed out-parameter of the right type.
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ))
    }
}

/// Last-modified time in whole seconds since 1970, as `stat`'s `st_mtime`.
pub fn mtime(m: &fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.mtime()
    }
    #[cfg(not(unix))]
    {
        match m
            .modified()
            .map(|t| t.duration_since(std::time::UNIX_EPOCH))
        {
            Ok(Ok(d)) => d.as_secs() as i64,
            Ok(Err(e)) => -(e.duration().as_secs_f64().ceil() as i64),
            Err(_) => 0,
        }
    }
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
    // Run as root (`sudo arr set`, an installer), keep the file its owner's, so the service
    // can still read it.
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions and can't fail.
    if unsafe { libc::geteuid() } == 0
        && let Err(e) =
            owner(path, dir).and_then(|(u, g)| std::os::unix::fs::chown(&tmp, Some(u), Some(g)))
    {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot set the owner of {}", path.display()));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot replace {}", path.display()));
    }
    fsync_dir(dir)
}

/// Who owns `path` (uid, gid), or the folder `dir` it will go in when it's new.
#[cfg(unix)]
fn owner(path: &Path, dir: &Path) -> std::io::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let m = match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::metadata(dir)?,
        m => m?,
    };
    Ok((m.uid(), m.gid()))
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
    fn errors_are_flattened_to_one_line() {
        assert_eq!(
            one_line("bad config: TOML parse error\r\n  |\n1 | a =\n  |    ^\n"),
            "bad config: TOML parse error | 1 | a = |    ^"
        );
    }

    #[cfg(unix)]
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
    fn file_ids_tell_files_apart_and_follow_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, b"1").unwrap();
        fs::write(&b, b"2").unwrap();
        let id = file_id(&a).unwrap();
        assert_ne!(id, file_id(&b).unwrap());
        assert_eq!(id.0, file_id(&b).unwrap().0, "same volume");
        let c = tmp.path().join("c");
        fs::rename(&a, &c).unwrap();
        assert_eq!(file_id(&c).unwrap(), id);
        assert!(file_id(&a).is_err());
        assert!(file_id(tmp.path()).is_ok(), "folders have ids too");
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_owner_and_a_new_one_gets_the_folders() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let ids = |p: &Path| {
            let m = fs::metadata(p).unwrap();
            (m.uid(), m.gid())
        };
        let new = tmp.path().join("new");
        assert_eq!(owner(&new, tmp.path()).unwrap(), ids(tmp.path()));
        // A file the folder's owner didn't make: root's /etc/hostname stands in.
        let theirs = Path::new("/etc/hostname");
        if theirs.exists() {
            assert_eq!(owner(theirs, tmp.path()).unwrap(), ids(theirs));
        }
        assert!(owner(&tmp.path().join("no/such"), &tmp.path().join("no")).is_err());
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
