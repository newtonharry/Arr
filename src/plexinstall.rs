//! `arr plex-install`: installs Plex Media Server from Plex's official package source and
//! makes a media folder that Plex and you can both use. On Linux the setup page runs it as
//! administrator through the system's password dialog (`pkexec`); on Windows it runs as you
//! and Plex's own installer asks Windows for permission. It does only this, and prints
//! `step: ...` lines for the page to show.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const KEY_URL: &str = "https://downloads.plex.tv/plex-keys/PlexSign.v2.key";
const APT_KEY: &str = "/usr/share/keyrings/plexmediaserver.v2.asc";
const APT_LIST: &str = "/etc/apt/sources.list.d/plex.list";
const DNF_REPO: &str = "/etc/yum.repos.d/plex.repo";
/// Plex's machine-readable list of public builds (URL and SHA-1 per platform).
#[cfg(windows)]
const DOWNLOADS_JSON: &str = "https://plex.tv/api/downloads/5.json";

pub struct Options {
    /// Who owns the media folders Arr creates (the user running the setup page).
    pub owner: Option<(u32, u32)>,
    /// The media folder: `Movies` and `TV Shows` are made inside it.
    pub media: PathBuf,
}

fn step(s: &str) {
    println!("step: {s}");
    let _ = std::io::stdout().flush();
}

/// The package manager this installer knows how to use here, if any ("windows": Plex's
/// own installer).
pub fn package_manager() -> Option<&'static str> {
    if cfg!(windows) {
        return Some("windows");
    }
    if !cfg!(target_os = "linux") {
        return None;
    }
    ["apt-get", "dnf"].into_iter().find(|p| {
        ["/usr/bin", "/bin", "/usr/sbin"]
            .iter()
            .any(|d| Path::new(d).join(p).is_file())
    })
}

/// Where a Plex installed by Arr keeps movies and shows: /srv/media on Linux (Plex runs as
/// its own user there), your own Media folder on Windows (Plex runs as you).
pub fn default_media() -> PathBuf {
    if cfg!(windows) {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        return home.unwrap_or_else(|| PathBuf::from("C:\\")).join("Media");
    }
    PathBuf::from("/srv/media")
}

pub async fn run(opts: Options) -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        bail!("arr plex-install installs system software: run it as administrator (sudo)");
    }
    // The installer runs with a private umask (077); the media folders must be readable by
    // Plex's own user, so this process uses the usual 022 whatever it inherited.
    #[cfg(unix)]
    // SAFETY: umask only sets this process's file-creation mask.
    unsafe {
        libc::umask(0o022);
    }
    let Some(pm) = package_manager() else {
        bail!(
            "this computer has neither apt nor dnf; install Plex from https://www.plex.tv/media-server-downloads/"
        );
    };
    #[cfg(windows)]
    if pm == "windows" {
        install_windows().await?;
        step("Making your media folder");
        for d in [
            opts.media.clone(),
            opts.media.join("Movies"),
            opts.media.join("TV Shows"),
        ] {
            make_shared_dir(&d, opts.owner)?;
        }
        return Ok(());
    }
    step("Adding Plex's package source");
    match pm {
        "apt-get" => {
            let key = reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?
                .get(KEY_URL)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .context("couldn't download Plex's signing key")?
                .text()
                .await?;
            if !key.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----") {
                bail!("Plex's signing key didn't download correctly");
            }
            write_public(Path::new(APT_KEY), key.as_bytes())?;
            write_public(
                Path::new(APT_LIST),
                format!("deb [signed-by={APT_KEY}] https://repo.plex.tv/deb/ public main\n")
                    .as_bytes(),
            )?;
            step("Downloading Plex Media Server");
            // Only Plex's list: a broken source elsewhere on this computer can't stop it.
            run_cmd(
                "apt-get",
                &[
                    "update",
                    "-o",
                    &format!("Dir::Etc::sourcelist={APT_LIST}"),
                    "-o",
                    "Dir::Etc::sourceparts=-",
                    "-o",
                    "APT::Get::List-Cleanup=0",
                ],
            )?;
            step("Installing Plex Media Server");
            run_cmd("apt-get", &["install", "-y", "plexmediaserver"])?;
        }
        _ => {
            write_public(
                Path::new(DNF_REPO),
                b"[PlexTv]\nname=Plex Media Server\nbaseurl=https://repo.plex.tv/rpm/\nenabled=1\ngpgcheck=1\nrepo_gpgcheck=1\ngpgkey=https://downloads.plex.tv/plex-keys/PlexSign.v2.key\n",
            )?;
            step("Installing Plex Media Server");
            run_cmd("dnf", &["install", "-y", "plexmediaserver"])?;
        }
    }
    step("Making your media folder");
    for d in [
        opts.media.clone(),
        opts.media.join("Movies"),
        opts.media.join("TV Shows"),
    ] {
        make_shared_dir(&d, opts.owner)?;
    }
    step("Starting Plex");
    // The package starts it already; this makes sure it also starts with the computer.
    let _ = run_cmd("systemctl", &["enable", "--now", "plexmediaserver"]);
    Ok(())
}

/// Downloads Plex's Windows installer from Plex's list, checks it, runs it silently (it asks
/// Windows for permission itself) and makes sure Plex is running afterwards.
#[cfg(windows)]
async fn install_windows() -> Result<()> {
    use sha1::{Digest, Sha1};
    use std::process::{Command, Stdio};
    step("Downloading Plex Media Server");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()?;
    let list: serde_json::Value = http
        .get(DOWNLOADS_JSON)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("couldn't reach plex.tv for the Plex installer")?
        .json()
        .await
        .context("plex.tv's download list didn't read correctly")?;
    let release = list["computer"]["Windows"]["releases"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["build"] == "windows-x86_64"))
        .context("plex.tv's download list has no 64-bit Windows build")?;
    let (Some(url), Some(sum)) = (release["url"].as_str(), release["checksum"].as_str()) else {
        bail!("plex.tv's download list has no installer address");
    };
    if !url.starts_with("https://downloads.plex.tv/") {
        bail!("plex.tv pointed somewhere unexpected for the installer: {url}");
    }
    let bytes = http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("couldn't download the Plex installer")?
        .bytes()
        .await
        .context("couldn't download the Plex installer")?;
    if hex::encode(Sha1::digest(&bytes)) != sum.to_ascii_lowercase() {
        bail!(
            "the Plex installer didn't download correctly (its checksum doesn't match); try again"
        );
    }
    let exe = std::env::temp_dir().join(format!(
        "PlexMediaServer-{}.exe",
        crate::fsutil::random_hex(4)?
    ));
    std::fs::write(&exe, &bytes).with_context(|| format!("cannot write {}", exe.display()))?;
    step("Installing Plex Media Server (Windows asks for permission: choose Yes)");
    let args = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"];
    // Nothing of this process is handed down: Plex, started by its installer, must not
    // hold the setup page's pipe open.
    let quiet = |c: &mut Command| {
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    };
    let mut c = Command::new(&exe);
    c.args(args);
    quiet(&mut c);
    let status = match c.status() {
        // The installer must be started elevated: ask Windows to.
        Err(e) if e.raw_os_error() == Some(740) => {
            let path = exe.to_string_lossy().replace('\'', "''");
            let mut ps = Command::new("powershell.exe");
            ps.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$p = Start-Process -FilePath '{path}' -ArgumentList '{}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
                    args.join("','")
                ),
            ]);
            quiet(&mut ps);
            ps.status()
        }
        other => other,
    };
    let _ = std::fs::remove_file(&exe);
    let status = status.context("couldn't start the Plex installer")?;
    if !status.success() {
        bail!(
            "the Plex installer stopped ({status}); if Windows asked for permission, choose Yes, then try again"
        );
    }
    step("Starting Plex");
    for _ in 0..10 {
        let up = http.get("http://127.0.0.1:32400/identity").send().await;
        if up.is_ok_and(|r| r.status().is_success()) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // The installer didn't start it: start it as you, as signing in to Windows does.
    let program = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Program Files"))
        .join("Plex\\Plex Media Server\\Plex Media Server.exe");
    let mut c = Command::new(&program);
    quiet(&mut c);
    c.spawn().with_context(|| {
        format!(
            "Plex is installed but couldn't be started ({})",
            program.display()
        )
    })?;
    Ok(())
}

fn write_public(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// Creates `dir` readable by everyone (Plex runs as its own user) and owned by `owner`, so
/// Arr can write to it. A folder that already exists is left exactly as it is.
fn make_shared_dir(dir: &Path, owner: Option<(u32, u32)>) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_ok() {
        return Ok(());
    }
    // Made with its final mode in one step, and handed over without following a link, so
    // nothing swapped in at this path meanwhile gets root's chmod or chown.
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o755);
    b.create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::lchown(dir, Some(uid), Some(gid))
            .with_context(|| format!("cannot give {} to you", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = owner;
    Ok(())
}

/// Runs a package-manager command; its own output goes to stderr, so a failure can be shown.
fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::null())
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!(
            "{program} {} failed ({status})",
            args.first().unwrap_or(&"")
        );
    }
    Ok(())
}
