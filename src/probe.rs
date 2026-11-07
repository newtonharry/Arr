//! Bounded media probe (native `enableMediaInfo`): a real `ffprobe` run that must read every
//! packet of the first real video stream.
//!
//! The file must be a regular, non-symlinked file inside an allowed root, and is passed as
//! `file:<canonical absolute path>`. ffprobe gets only the `file` protocol, a demuxer whitelist
//! of ordinary video containers (no playlists, concat or image sequences), no shell, an empty
//! environment, a null stdin, CPU/memory/file-size/descriptor rlimits, a wall-clock timeout
//! and capped output. Only exit 0 with no error output, a non-cover-art video stream with a
//! codec and dimensions, packets read and a positive duration is accepted.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Demuxers accepted (FFmpeg names): Matroska/WebM, MP4/MOV, AVI, MPEG-TS/PS, ASF/WMV.
pub const FORMAT_WHITELIST: &str = "matroska,mov,avi,mpegts,mpeg,asf";
const MAX_STDOUT: u64 = 1024 * 1024;
const MAX_STDERR: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Probe {
    pub format: String,
    pub duration_secs: f64,
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub packets: u64,
}

impl Probe {
    /// Compact evidence string stored with the file (`ok: ...`).
    pub fn evidence(&self) -> String {
        format!(
            "ok: {} {} {}x{} {:.1}s {} packets",
            self.format,
            self.video_codec,
            self.width,
            self.height,
            self.duration_secs,
            self.packets
        )
    }
}

/// Preflight for the media-validation dependency: runs `ffprobe -version` (bounded, empty
/// environment). Media validation is never skipped, so a missing or broken ffprobe is an
/// error that says how to fix it; nothing is downloaded.
pub async fn preflight(ffprobe: &Path) -> Result<String, String> {
    let fix = format!(
        "ffprobe is an external dependency: install FFmpeg (Debian/Ubuntu: apt install ffmpeg; Fedora: dnf install ffmpeg-free; Alpine: apk add ffmpeg) or set [library] ffprobe to its absolute path (docs/STANDALONE.md). Completed downloads are never imported without media validation (configured: {})",
        ffprobe.display()
    );
    let run = tokio::process::Command::new(ffprobe)
        .arg("-version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), run).await {
        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect()),
        Ok(Ok(o)) => Err(format!("ffprobe exited with {}: {fix}", o.status)),
        Ok(Err(e)) => Err(format!("ffprobe could not be run ({}): {fix}", e.kind())),
        Err(_) => Err(format!(
            "ffprobe -version did not finish within 10 s: {fix}"
        )),
    }
}

/// Resolves `file` to a canonical path inside one of `roots`, refusing symlinks anywhere below
/// the root, non-regular files and names that are not plain UTF-8.
pub fn validate(file: &Path, roots: &[&Path]) -> Result<PathBuf, String> {
    let text = file.to_str().ok_or("path is not UTF-8")?;
    if !file.is_absolute() || text.chars().any(char::is_control) {
        return Err("path must be absolute and printable".into());
    }
    let meta = std::fs::symlink_metadata(file).map_err(|e| format!("cannot stat: {}", e.kind()))?;
    if !meta.file_type().is_file() {
        return Err("not a regular file".into());
    }
    for root in roots {
        let Ok(rel) = file.strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("path must be a plain path below the root".into());
        }
        let croot = std::fs::canonicalize(root)
            .map_err(|e| format!("cannot resolve the root: {}", e.kind()))?;
        let canonical =
            std::fs::canonicalize(file).map_err(|e| format!("cannot resolve: {}", e.kind()))?;
        // Any symlink below the root makes the canonical path differ from root + relative path.
        if croot.join(rel) != canonical {
            return Err("path contains a symbolic link".into());
        }
        return Ok(canonical);
    }
    Err("file is outside the allowed roots".into())
}

/// Runs ffprobe on an already validated canonical path.
pub async fn run(ffprobe: &Path, file: &Path, timeout: Duration) -> Result<Probe, String> {
    let mut cmd = tokio::process::Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-hide_banner",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        FORMAT_WHITELIST,
        "-select_streams",
        "V:0",
        "-count_packets",
        "-show_entries",
        "format=format_name,duration:stream=codec_type,codec_name,width,height,nb_read_packets",
        "-of",
        "json",
    ])
    .arg(format!("file:{}", file.display()))
    .env_clear()
    // Distribution FFmpeg builds can link OpenBLAS, which starts one thread per CPU at load;
    // on many-core hosts that exhausts RLIMIT_AS below and ffprobe fails with EAGAIN.
    .env("OPENBLAS_NUM_THREADS", "1")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let cpu = timeout.as_secs() + 5;
    // SAFETY: only async-signal-safe `setrlimit` calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let set = |res, v: u64| {
                let lim = libc::rlimit {
                    rlim_cur: v as libc::rlim_t,
                    rlim_max: v as libc::rlim_t,
                };
                if libc::setrlimit(res, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            set(libc::RLIMIT_CPU, cpu)?;
            set(libc::RLIMIT_AS, 2 * 1024 * 1024 * 1024)?;
            set(libc::RLIMIT_FSIZE, 0)?;
            set(libc::RLIMIT_CORE, 0)?;
            set(libc::RLIMIT_NOFILE, 64)?;
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start ffprobe: {}", e.kind()))?;
    let mut stdout = child.stdout.take().expect("piped").take(MAX_STDOUT);
    let mut stderr = child.stderr.take().expect("piped").take(MAX_STDERR);
    let work = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a.and(b)
            .map_err(|e| format!("cannot read ffprobe output: {}", e.kind()))?;
        let status = child
            .wait()
            .await
            .map_err(|e| format!("ffprobe failed: {}", e.kind()))?;
        Ok::<_, String>((status, out, err))
    };
    let (status, out, err) = match tokio::time::timeout(timeout, work).await {
        Ok(r) => r?,
        Err(_) => return Err(format!("ffprobe exceeded {} s", timeout.as_secs())),
    };
    if !status.success() {
        return Err(format!(
            "unreadable media ({})",
            first_line(&err).unwrap_or("ffprobe failed")
        ));
    }
    if let Some(line) = first_line(&err) {
        return Err(format!("media errors while reading packets ({line})"));
    }
    parse(&out)
}

fn first_line(err: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(err)
        .ok()?
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    // ffprobe prefixes messages with the input URL; keep only the reason (160 characters,
    // never splitting a UTF-8 character).
    let r = s.rsplit(": ").next().unwrap_or(s);
    let end = r.char_indices().nth(160).map(|(i, _)| i).unwrap_or(r.len());
    Some(&r[..end])
}

fn parse(out: &[u8]) -> Result<Probe, String> {
    let v: serde_json::Value =
        serde_json::from_slice(out).map_err(|_| "ffprobe output is not JSON".to_string())?;
    let stream = v["streams"]
        .as_array()
        .and_then(|s| s.first())
        .ok_or("no video stream")?;
    if stream["codec_type"] != "video" {
        return Err("no video stream".into());
    }
    let codec = stream["codec_name"]
        .as_str()
        .filter(|c| !c.is_empty())
        .ok_or("video codec unknown")?;
    let dim = |k: &str| {
        stream[k]
            .as_u64()
            .filter(|d| *d > 0 && *d <= 16384)
            .map(|d| d as u32)
    };
    let (Some(width), Some(height)) = (dim("width"), dim("height")) else {
        return Err("video dimensions unknown".into());
    };
    let packets: u64 = stream["nb_read_packets"]
        .as_str()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    if packets == 0 {
        return Err("no video packets could be read".into());
    }
    let duration: f64 = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    if !(duration.is_finite() && duration > 0.0) {
        return Err("duration unknown".into());
    }
    Ok(Probe {
        format: v["format"]["format_name"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .take(40)
            .collect(),
        duration_secs: duration,
        video_codec: codec.chars().take(32).collect(),
        width,
        height,
        packets,
    })
}

/// Native sample detection thresholds (Sonarr/Radarr `DetectSample`): the minimum duration
/// for an item with the given runtime in minutes.
pub fn minimum_duration_secs(runtime_minutes: u32) -> u32 {
    match runtime_minutes {
        0..=3 => 15,
        4..=10 => 90,
        11..=30 => 300,
        _ => 600,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rejects() {
        let ok = br#"{"streams":[{"codec_name":"h264","codec_type":"video","width":1920,"height":1080,"nb_read_packets":"1440"}],"format":{"format_name":"matroska,webm","duration":"60.0"}}"#;
        let p = parse(ok).unwrap();
        assert_eq!((p.width, p.height, p.packets), (1920, 1080, 1440));
        assert!(parse(br#"{"streams":[],"format":{"duration":"1"}}"#).is_err());
        assert!(parse(br#"{"streams":[{"codec_name":"h264","codec_type":"video","width":0,"height":1080,"nb_read_packets":"1"}],"format":{"duration":"1"}}"#).is_err());
        assert!(parse(br#"{"streams":[{"codec_name":"h264","codec_type":"video","width":2,"height":2,"nb_read_packets":"0"}],"format":{"duration":"1"}}"#).is_err());
        assert!(parse(br#"{"streams":[{"codec_name":"h264","codec_type":"video","width":2,"height":2,"nb_read_packets":"3"}],"format":{"duration":"N/A"}}"#).is_err());
        assert_eq!(minimum_duration_secs(1), 15);
        // Long non-ASCII error output is truncated on a character boundary.
        let long = format!("file:/x/é.mkv: {}", "é".repeat(400));
        let line = first_line(long.as_bytes()).unwrap();
        assert_eq!(line.chars().count(), 160);
        assert!(first_line("x: ab€".repeat(100).as_bytes()).is_some());
        assert_eq!(minimum_duration_secs(45), 600);
    }
}
