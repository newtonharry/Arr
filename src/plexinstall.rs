//! `arr plex-install`: installs Plex Media Server from Plex's official package source and
//! makes a media folder that Plex and you can both use. The setup page runs it as
//! administrator through the system's password dialog (`pkexec`); it does only this, and
//! prints `step: ...` lines for the page to show.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const KEY_URL: &str = "https://downloads.plex.tv/plex-keys/PlexSign.v2.key";
const APT_KEY: &str = "/usr/share/keyrings/plexmediaserver.v2.asc";
const APT_LIST: &str = "/etc/apt/sources.list.d/plex.list";
const DNF_REPO: &str = "/etc/yum.repos.d/plex.repo";

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

/// The package manager this installer knows how to use here, if any.
pub fn package_manager() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    ["apt-get", "dnf"].into_iter().find(|p| {
        ["/usr/bin", "/bin", "/usr/sbin"]
            .iter()
            .any(|d| Path::new(d).join(p).is_file())
    })
}

pub async fn run(opts: Options) -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        bail!("arr plex-install installs system software: run it as administrator (sudo)");
    }
    let Some(pm) = package_manager() else {
        bail!(
            "this computer has neither apt nor dnf; install Plex from https://www.plex.tv/media-server-downloads/"
        );
    };
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
