//! Path confinement shared by import, scan and cleanup: client-to-local mapping, safe
//! torrent-relative names, symlink-free component checks and no-clobber directory creation.

use std::path::{Component, Path, PathBuf};

/// A client-side absolute path without `.`/`..`/empty components and without a trailing `/`.
pub fn normalize_client(p: &str) -> Option<String> {
    if !p.starts_with('/') || p.contains('\0') || p.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.iter().any(|s| *s == "." || *s == "..") {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// A relative path as `/`-separated text on every OS (Windows would use `\\`), which is
/// how relative paths are stored and compared. None unless every part is a plain name.
pub fn rel_string(rel: &Path) -> Option<String> {
    let mut parts = vec![];
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A local folder written as a download client path: `/`-separated and starting with `/`.
/// Unix paths are unchanged; on Windows `C:\\Users\\x` becomes `/C:/Users/x`.
pub fn client_form(local: &Path) -> String {
    let s = local.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    }
}

/// Maps a client path below `client_root` to the same location below `local_root`.
pub fn client_to_local(client_root: &str, local_root: &Path, client_path: &str) -> Option<PathBuf> {
    let root = normalize_client(client_root)?;
    let p = normalize_client(client_path)?;
    let rest = if root == "/" {
        p.trim_start_matches('/').to_string()
    } else if p == root {
        String::new()
    } else {
        p.strip_prefix(&format!("{root}/"))?.to_string()
    };
    let mut out = local_root.to_path_buf();
    if !rest.is_empty() {
        out.push(safe_rel(&rest)?);
    }
    Some(out)
}

/// A relative path made only of normal components (no absolute, `.`, `..`, empty or control
/// characters), as torrent file names must be.
pub fn safe_rel(name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.len() > 4096
        || name.starts_with('/')
        || name.chars().any(|c| c.is_control() || c == '\\')
    {
        return None;
    }
    let p = PathBuf::from(name);
    if p.components().all(|c| matches!(c, Component::Normal(_)))
        && name
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
    {
        Some(p)
    } else {
        None
    }
}

pub fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Fails if any existing component of `path` below `root` is a symlink, or `path` is not
/// below `root`. Missing trailing components are allowed.
pub fn check_no_symlinks(root: &Path, path: &Path) -> Result<(), String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| "path is outside its root".to_string())?;
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("path is not a plain path below its root".into());
    }
    let mut p = root.to_path_buf();
    for c in rel.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symbolic link", p.display()));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Creates `rel` directories below `root`, refusing symlinked or non-directory components.
pub fn ensure_dirs(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("folder is not a plain relative path".into());
    }
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => {}
        _ => {
            return Err(format!(
                "library root {} is not an accessible directory",
                root.display()
            ));
        }
    }
    let mut p = root.to_path_buf();
    for c in rel.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symbolic link", p.display()));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} exists and is not a directory", p.display())),
            Err(_) => std::fs::create_dir(&p)
                .map_err(|e| format!("cannot create {}: {}", p.display(), e.kind()))?,
        }
    }
    Ok(p)
}

/// `path` with every symbolic link in it resolved, for comparing where two folders really are
/// (available on every platform). The folder may not exist yet: the longest existing ancestor is
/// resolved and the components that do not exist are appended as written. Only a component that
/// is genuinely missing is tolerated; any other failure (no permission, a dangling link, a loop,
/// a file where a folder should be) is an error with a plain message, and the caller refuses.
pub fn resolved(path: &Path) -> Result<PathBuf, String> {
    let mut rest: Vec<std::ffi::OsString> = vec![];
    let mut cur = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(real) => {
                let mut out = real;
                out.extend(rest.iter().rev());
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Missing for real, not a link that points nowhere (that one exists as a link).
                if std::fs::symlink_metadata(&cur).is_ok() {
                    return Err(format!(
                        "{} is a link that points nowhere, so where it leads cannot be checked",
                        cur.display()
                    ));
                }
                match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
                    (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                        rest.push(name);
                        cur = parent.to_path_buf();
                    }
                    _ => {
                        return Err(format!(
                            "no part of {} can be looked at, so where it is cannot be checked",
                            path.display()
                        ));
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "cannot look at {} to see where it is ({}); not going on",
                    cur.display(),
                    e.kind()
                ));
            }
        }
    }
}

/// Whether two folders are the same place or one holds the other, by where they really are.
/// An error when either cannot be resolved (see `resolved`).
pub fn overlap(a: &Path, b: &Path) -> Result<bool, String> {
    let (a, b) = (resolved(a)?, resolved(b)?);
    Ok(a.starts_with(&b) || b.starts_with(&a))
}

/// `(free, total)` bytes of the filesystem holding `path`; free is what this process may use.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a zeroed out-parameter of the right type.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    #[allow(clippy::unnecessary_cast)] // c_ulong/fsblkcnt_t widths differ across targets
    let (free, total) = (
        (s.f_bavail as u64).saturating_mul(s.f_frsize as u64),
        (s.f_blocks as u64).saturating_mul(s.f_frsize as u64),
    );
    (rc == 0).then_some((free, total))
}

/// `(free, total)` bytes of the drive holding `path`; free is what this process may use.
#[cfg(windows)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: a NUL-terminated wide path and valid out-parameters; the last may be null.
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}

/// Free bytes available to this process on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    disk_space(path).map(|(free, _)| free)
}

/// Whether two existing paths are on the same drive (filesystem).
pub fn same_drive(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        let prefix = |p: &Path| match p.components().next() {
            Some(Component::Prefix(x)) => Some(x.as_os_str().to_ascii_lowercase()),
            _ => None,
        };
        let (x, y) = (prefix(a), prefix(b));
        x.is_some() && x == y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_and_names() {
        let local = Path::new("/srv/dl");
        assert_eq!(
            client_to_local("/downloads", local, "/downloads/").unwrap(),
            PathBuf::from("/srv/dl")
        );
        assert_eq!(
            client_to_local("/downloads", local, "/downloads/tv/x").unwrap(),
            PathBuf::from("/srv/dl/tv/x")
        );
        assert!(client_to_local("/downloads", local, "/downloads2/x").is_none());
        assert!(client_to_local("/downloads", local, "/downloads/../etc").is_none());
        assert!(client_to_local("/downloads", local, "relative").is_none());
        assert_eq!(
            safe_rel("Show/S01/a.mkv").unwrap(),
            PathBuf::from("Show/S01/a.mkv")
        );
        for bad in [
            "", "/abs", "../x", "a/../b", "a//b", "a/./b", "a\\b", "a\nb",
        ] {
            assert!(safe_rel(bad).is_none(), "{bad:?}");
        }
    }

    #[cfg(unix)] // makes symlinks
    #[test]
    fn symlinks_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::os::unix::fs::symlink(tmp.path(), root.join("link")).unwrap();
        assert!(check_no_symlinks(&root, &root.join("a/b.mkv")).is_ok());
        assert!(check_no_symlinks(&root, &root.join("link/b.mkv")).is_err());
        assert!(check_no_symlinks(&root, Path::new("/etc/passwd")).is_err());
        assert!(ensure_dirs(&root, Path::new("link/x")).is_err());
        assert_eq!(
            ensure_dirs(&root, Path::new("new/Season 1")).unwrap(),
            root.join("new/Season 1")
        );
        assert!(free_bytes(&root).is_some());
    }
}
