//! Author-created test fixtures: real playable media generated with ffmpeg and `.torrent`
//! files with correct piece hashes, so a real qBittorrent can verify existing data and
//! complete without trackers. Used by tests, the sandbox harness and `arr fixture`.

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::path::Path;
use std::process::{Command, Stdio};

fn benc_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

fn benc_int(out: &mut Vec<u8>, i: u64) {
    out.push(b'i');
    out.extend_from_slice(i.to_string().as_bytes());
    out.push(b'e');
}

/// A generated torrent and its identity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TorrentFixture {
    pub name: String,
    pub info_hash: String,
    #[serde(skip)]
    pub torrent: Vec<u8>,
    pub magnet: String,
    pub files: Vec<(String, u64)>,
    pub size: u64,
}

/// Builds a v1 torrent named `name` over `files` (relative path, bytes). One file without
/// directories gives a single-file torrent; otherwise files live under `name/`.
pub fn build_torrent(name: &str, files: &[(String, Vec<u8>)]) -> TorrentFixture {
    const PIECE: usize = 16 * 1024;
    let data: Vec<u8> = files.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    let mut pieces = vec![];
    for chunk in data.chunks(PIECE) {
        pieces.extend_from_slice(&Sha1::digest(chunk));
    }
    let single = files.len() == 1 && !files[0].0.contains('/');
    // Keys in sorted order, as bencode requires.
    let mut info = vec![b'd'];
    if single {
        benc_bytes(&mut info, b"length");
        benc_int(&mut info, files[0].1.len() as u64);
        benc_bytes(&mut info, b"name");
        benc_bytes(&mut info, files[0].0.as_bytes());
    } else {
        benc_bytes(&mut info, b"files");
        info.push(b'l');
        for (path, bytes) in files {
            info.push(b'd');
            benc_bytes(&mut info, b"length");
            benc_int(&mut info, bytes.len() as u64);
            benc_bytes(&mut info, b"path");
            info.push(b'l');
            for part in path.split('/') {
                benc_bytes(&mut info, part.as_bytes());
            }
            info.extend_from_slice(b"ee");
        }
        info.push(b'e');
        benc_bytes(&mut info, b"name");
        benc_bytes(&mut info, name.as_bytes());
    }
    benc_bytes(&mut info, b"piece length");
    benc_int(&mut info, PIECE as u64);
    benc_bytes(&mut info, b"pieces");
    benc_bytes(&mut info, &pieces);
    info.push(b'e');
    let info_hash = hex::encode(Sha1::digest(&info));
    let mut torrent = vec![b'd'];
    benc_bytes(&mut torrent, b"info");
    torrent.extend_from_slice(&info);
    torrent.push(b'e');
    let display = if single {
        files[0].0.clone()
    } else {
        name.to_string()
    };
    TorrentFixture {
        magnet: format!(
            "magnet:?xt=urn:btih:{info_hash}&dn={}",
            display.replace(' ', ".")
        ),
        name: display,
        info_hash,
        torrent,
        files: files
            .iter()
            .map(|(p, b)| (p.clone(), b.len() as u64))
            .collect(),
        size: data.len() as u64,
    }
}

/// Generates a real playable 1920x1080 video (MPEG-4 Part 2 in Matroska, 1 frame per second,
/// solid colour): honest HD for import and quality checks, small and fast to make.
pub fn generate_mkv(ffmpeg: &Path, out: &Path, seconds: u32) -> Result<Vec<u8>> {
    generate_video(ffmpeg, out, seconds, 1920, 1080)
}

/// Generates a real playable video of the given size (1 fps solid colour, Matroska).
pub fn generate_video(
    ffmpeg: &Path,
    out: &Path,
    seconds: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    if seconds == 0 || seconds > 600 {
        bail!("fixture duration must be 1-600 seconds");
    }
    if !(16..=7680).contains(&width) || !(16..=4320).contains(&height) {
        bail!("fixture size must be 16x16 to 7680x4320");
    }
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "color=c=0x1f3d5c:s={width}x{height}:r=1:d={seconds}"
        ))
        .args(["-c:v", "mpeg4", "-q:v", "31", "-f", "matroska"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("cannot run {}", ffmpeg.display()))?;
    if !status.success() {
        bail!("ffmpeg failed to generate {}", out.display());
    }
    std::fs::read(out).with_context(|| format!("cannot read {}", out.display()))
}

/// Writes `files` under `dir` (creating parents) and returns the torrent over them.
pub fn write_release(
    dir: &Path,
    name: &str,
    files: &[(String, Vec<u8>)],
) -> Result<TorrentFixture> {
    let t = build_torrent(name, files);
    let single = files.len() == 1 && !files[0].0.contains('/');
    for (path, bytes) in files {
        let p = if single {
            dir.join(path)
        } else {
            dir.join(name).join(path)
        };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, bytes)?;
    }
    Ok(t)
}
