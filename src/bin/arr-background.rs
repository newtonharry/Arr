//! Runs Arr without a console window, for the Windows tasks `install.ps1` registers:
//!
//! - `arr-background --config <config.toml>` is `arr serve`, logging to `arr.log` beside the
//!   config (the "Arr" task, started at log-on);
//! - `arr-background --hidden <program> [args...]` runs a program without a window and exits
//!   with its exit code (the daily "Arr update" task runs PowerShell this way).
//!
//! On Windows this is a GUI program, so starting it opens no window; elsewhere it behaves
//! the same but has no use, since `arr serve` runs under systemd or launchd.
#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// The log is set aside once it grows past this, keeping one older copy.
const MAX_LOG: u64 = 10 * 1024 * 1024;

fn main() -> Result<()> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        Some("--config") if args.len() == 2 => serve(PathBuf::from(&args[1])),
        Some("--hidden") if args.len() >= 2 => hidden(&args[1..]),
        _ => bail!("usage: arr-background --config <config.toml> | --hidden <program> [args...]"),
    }
}

fn serve(config: PathBuf) -> Result<()> {
    let log = config.with_file_name("arr.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&log, config.with_file_name("arr.log.old"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("cannot open {}", log.display()))?;
    {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let env = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,arr=info".into());
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_filter(env),
            )
            .with(arr::logbuf::layer().with_filter(tracing_subscriber::filter::LevelFilter::WARN))
            .init();
    }
    // Only a scheduled task starts this program, and it starts it again if it exits with an
    // error (a changed setting from the web app ends it that way on purpose).
    arr::lifecycle::mark_supervised();
    arr::lifecycle::set_config_path(&config);
    let result = tokio::runtime::Runtime::new()?.block_on(async {
        let cfg = arr::config::load(&config)?;
        let _reporting = arr::report::start(cfg.reporting, "server");
        let result = arr::api::serve(cfg).await;
        if let Err(e) = &result {
            arr::report::error("server", &format!("Arr stopped: {e:#}"));
        }
        result
    });
    if let Err(e) = &result {
        tracing::error!("Arr stopped: {e:#}");
    }
    result
}

fn hidden(cmd: &[std::ffi::OsString]) -> Result<()> {
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let status = c
        .status()
        .with_context(|| format!("cannot run {}", cmd[0].to_string_lossy()))?;
    std::process::exit(status.code().unwrap_or(1));
}
