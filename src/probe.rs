//! Bounded media probe (native `enableMediaInfo`): a real `ffprobe` run that must read every
//! packet of the first real video stream.
//!
//! The file must be a regular, non-symlinked file inside an allowed root, and is passed as
//! `file:<canonical absolute path>`. ffprobe gets only the `file` protocol, a demuxer whitelist
//! of ordinary video containers (no playlists, concat or image sequences), no shell, an empty
//! environment, a null stdin, CPU/memory/file-size/descriptor rlimits, a wall-clock timeout
//! and capped output. On Windows a job object stands in for the rlimits: CPU time, memory,
//! one process (no children), killed with the job; it has no file-size or descriptor caps. Only exit 0 with no error output, a non-cover-art video stream with a
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
    let mut cmd = tokio::process::Command::new(ffprobe);
    cmd.arg("-version").env_clear();
    keep_system_root(&mut cmd);
    // Arr in the background has no console: ffprobe mustn't open one.
    #[cfg(windows)]
    cmd.creation_flags(job::CREATE_NO_WINDOW);
    let run = cmd
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
    .env("OPENBLAS_NUM_THREADS", "1");
    keep_system_root(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let cpu = timeout.as_secs() + 5;
    // SAFETY: only async-signal-safe `setrlimit` calls between fork and exec.
    #[cfg(unix)]
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
    // Windows: started suspended and without a console, put in its job, then let run, so
    // the limits hold from its first instruction.
    #[cfg(windows)]
    cmd.creation_flags(job::CREATE_NO_WINDOW | job::CREATE_SUSPENDED);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start ffprobe: {}", e.kind()))?;
    #[cfg(windows)]
    let _job = {
        let job = job::confine(&child, cpu).map_err(|e| format!("cannot limit ffprobe: {e}"))?;
        job::resume(child.id().unwrap_or(0)).map_err(|e| format!("cannot start ffprobe: {e}"))?;
        job
    };
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

/// Windows programs can fail to start without `SystemRoot`, even with nothing else set.
fn keep_system_root(cmd: &mut tokio::process::Command) {
    if cfg!(windows)
        && let Some(v) = std::env::var_os("SystemRoot")
    {
        cmd.env("SystemRoot", v);
    }
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub const CREATE_SUSPENDED: u32 = 0x0000_0004;
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Lets a process started with CREATE_SUSPENDED run (its only thread, in practice).
    pub fn resume(pid: u32) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };
        // SAFETY: a snapshot of all threads; closed below.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: plain-data struct; dwSize set as the API requires.
        let mut te: THREADENTRY32 = unsafe { std::mem::zeroed() };
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        // SAFETY: a valid snapshot and entry for the whole walk.
        let mut more = unsafe { Thread32First(snap, &mut te) } != 0;
        while more {
            if te.th32OwnerProcessID == pid {
                // SAFETY: a thread id from the snapshot; the handle is closed right away.
                let h = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID) };
                if !h.is_null() {
                    if unsafe { ResumeThread(h) } != u32::MAX {
                        resumed += 1;
                    }
                    unsafe { CloseHandle(h) };
                }
            }
            more = unsafe { Thread32Next(snap, &mut te) } != 0;
        }
        unsafe { CloseHandle(snap) };
        if resumed == 0 {
            return Err(std::io::Error::other("its thread couldn't be started"));
        }
        Ok(())
    }

    /// A job object; closing it kills whatever is still in it.
    pub struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: a handle this struct owns, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: a kernel handle, usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    /// Puts `child` in a new job: `cpu_secs` of CPU time, 2 GiB of memory, no child processes.
    pub fn confine(child: &tokio::process::Child, cpu_secs: u64) -> std::io::Result<Job> {
        let last = std::io::Error::last_os_error;
        let process = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("ffprobe already exited"))?;
        // SAFETY: no security attributes and no name; the handle is checked below.
        let job = Job(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
        if job.0.is_null() {
            return Err(last());
        }
        // SAFETY: plain-data struct, all zeroes is a valid "no limits" value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let basic = &mut info.BasicLimitInformation;
        basic.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        basic.PerProcessUserTimeLimit = (cpu_secs as i64).saturating_mul(10_000_000); // 100 ns
        basic.ActiveProcessLimit = 1;
        info.ProcessMemoryLimit = 2 * 1024 * 1024 * 1024;
        // SAFETY: a valid job handle and a pointer to a struct of the size passed.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last());
        }
        // SAFETY: both handles are open; the child's stays valid while `child` lives.
        if unsafe { AssignProcessToJobObject(job.0, process as HANDLE) } == 0 {
            return Err(last());
        }
        Ok(job)
    }
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
