use anyhow::{Context, Result};
use arr::{api, config, fsutil, import, library, mcp, setup, store, wizard};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Parser)]
#[command(
    name = "arr",
    version,
    about = "Finds, downloads and imports movies and TV for Plex."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at startup; its size doesn't matter
enum Command {
    /// Create a private state directory, API token and config template (never overwrites).
    /// The template favours native indexers and the embedded downloader; pass
    /// --prowlarr-url for the legacy Prowlarr-backed layout.
    Init {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: std::net::SocketAddr,
        #[arg(long)]
        prowlarr_url: Option<String>,
    },
    /// Create (or replace) managed settings for a standalone deployment, with an empty
    /// catalog, from an optional setup document (TOML; otherwise the current setup's, or
    /// defaults). Validates the config without needing its secret files yet. Never enables
    /// execution.
    Setup {
        #[arg(long)]
        config: PathBuf,
        /// Setup document (see config/setup.example.toml). Default: the one the current
        /// setup used, or built-in defaults.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Validate and summarise without changing the settings.
        #[arg(long)]
        dry_run: bool,
    },
    /// Import a native Radarr/Sonarr/Prowlarr snapshot (offline; no network access).
    Import {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        /// When the snapshot was taken (RFC 3339). Defaults to the file's mtime.
        #[arg(long)]
        observed_at: Option<String>,
        /// Validate and summarise without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Read-only bootstrap of an existing Plex library into the managed catalog (exact
    /// provider ids plus validated local files; nothing monitored or searched). Runs bounded
    /// batches until each media kind is done; resumable. Run it before `serve`, or use
    /// POST /api/v1/setup/plex/bootstrap on a running server.
    Bootstrap {
        #[arg(long)]
        config: PathBuf,
        /// movies, tv or all (default: the ones [plex] has a library section for).
        #[arg(long)]
        media: Option<String>,
        #[arg(long)]
        restart: bool,
        #[arg(long)]
        dry_run: bool,
        /// Items per batch (default 50 movies or 5 shows; at most 200).
        #[arg(long)]
        batch: Option<u32>,
    },
    /// Check the config, secret file modes, state database and active import (no network;
    /// creates or updates the state database).
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// First-run setup in a web browser: prints a private link, then signs in to Plex,
    /// picks libraries and folders, writes the config and imports what Plex already has.
    /// Exits when setup is done (the installer then starts the service).
    Wizard {
        #[arg(long)]
        state_dir: PathBuf,
        /// Address the setup page listens on (the installer uses the SSH server address
        /// when you're connected over SSH).
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        /// Port for the setup page now and Arr's API afterwards.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        #[arg(long, default_value_t = 50413)]
        torrent_port: u16,
        /// Upload limit in KiB/s for all torrents (0 = none; at most 4194303, as `arr set`).
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=u64::from(u32::MAX / 1024)))]
        upload_limit_kib: u64,
        #[arg(long)]
        ffprobe: PathBuf,
        #[arg(long, hide = true, default_value = "https://plex.tv/")]
        plex_tv_url: reqwest::Url,
        #[arg(long, hide = true, default_value = "https://app.plex.tv/auth")]
        plex_auth_url: reqwest::Url,
        /// Don't look for a Plex container with `docker`.
        #[arg(long)]
        no_docker: bool,
        /// Also open the link in this computer's web browser.
        #[arg(long)]
        open: bool,
        /// Tests: a shell command that stands in for installing Plex as administrator.
        #[arg(long, hide = true)]
        plex_installer: Option<String>,
        /// Where Plex listens on this computer.
        #[arg(long, hide = true, default_value_t = 32400)]
        plex_port: u16,
        /// Start with error reports ticked (the installer's --error-reports).
        #[arg(long)]
        error_reports: bool,
        /// Tests: where TMDB's API is, for checking keys.
        #[arg(long, hide = true, default_value = arr::tmdbkey::API_URL)]
        tmdb_url: reqwest::Url,
    },
    /// Install Plex Media Server from Plex's package source, and make a media folder Plex and
    /// you can both use (Movies, TV Shows). Needs administrator rights; the setup page runs it
    /// through the system's password dialog.
    #[command(hide = true)]
    PlexInstall {
        /// Owner of the folders it creates, as UID:GID.
        #[arg(long)]
        owner: Option<String>,
        /// The media folder (default: /srv/media on Linux, your Media folder on Windows).
        #[arg(long)]
        media: Option<PathBuf>,
    },
    /// Sign in to Plex again after Plex stopped accepting Arr's sign-in: prints a plex.tv
    /// link, then replaces only the saved Plex token (the config isn't changed). With
    /// --check, only says whether Plex still accepts it (exit 1 if not, 2 if unreachable).
    PlexSignin {
        /// Arr's config.toml.
        #[arg(long)]
        config: PathBuf,
        /// Only check the saved sign-in; change nothing.
        #[arg(long)]
        check: bool,
        #[arg(long, hide = true, default_value = "https://plex.tv/")]
        plex_tv_url: reqwest::Url,
        #[arg(long, hide = true, default_value = "https://app.plex.tv/auth")]
        plex_auth_url: reqwest::Url,
    },
    /// Run the HTTP API (and, with [execution] enabled, the background worker).
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Send one error report, if this install's config allows it ([reporting] enabled; see
    /// docs/PRIVACY.md). The installers and updaters use it; it never fails.
    #[command(hide = true)]
    Report {
        #[arg(long)]
        config: PathBuf,
        /// What failed: installer, updater, ...
        #[arg(long)]
        component: String,
        #[arg(long)]
        message: String,
    },
    /// Change one setting of an existing install; restart Arr afterwards.
    ///
    /// Arr checks the change first (keys and Plex's address are tried for real) and
    /// changes nothing if it doesn't work.
    ///
    /// Settings:
    ///   tmdb-key, plex-token      a key or token, read from stdin
    ///   plex-url URL              Plex's address, like http://127.0.0.1:32400
    ///   downloading on|off
    ///   upload-limit-kib N        KiB/s for all torrents; 0 for no limit
    ///   torrent-port N            1 to 65535
    ///   error-reports on|off
    ///   api-token, mcp-token      make a new random token
    #[command(verbatim_doc_comment)]
    Set {
        /// The install's config.toml.
        #[arg(long)]
        config: PathBuf,
        /// The setting to change.
        name: String,
        /// Its new value.
        value: Option<String>,
    },
    /// The TMDB key (read from stdin): `arr set tmdb-key`, or with --check, only checks it.
    #[command(hide = true)]
    TmdbKey {
        #[arg(long, required_unless_present = "check", conflicts_with = "check")]
        config: Option<PathBuf>,
        /// Only check the key with TMDB; save nothing.
        #[arg(long)]
        check: bool,
        /// Tests: where TMDB's API is (with --check). `requires = "check"` can't work: a
        /// flag's default counts as present.
        #[arg(long, hide = true, conflicts_with = "config")]
        tmdb_url: Option<reqwest::Url>,
    },
    /// Serve MCP tools on stdin/stdout as a client of a running API (stdout carries only MCP).
    Mcp {
        /// Base URL of `arr serve` (http or https; no credentials, query or fragment).
        #[arg(long)]
        api_url: String,
        /// File holding the API bearer token (mode 600).
        #[arg(long)]
        token_file: PathBuf,
        /// Timeout for each API call, in seconds.
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
        timeout_secs: u64,
    },
    /// Generate an authored test release: real playable video files (ffmpeg testsrc) under
    /// OUT/<name>/ plus OUT/<name>.torrent with correct piece hashes. Never fetches anything.
    #[command(hide = true)]
    Fixture {
        #[arg(long)]
        out: PathBuf,
        /// Release (torrent) name, e.g. "Parity Feature 2026 1080p WEB-DL-UM".
        #[arg(long)]
        name: String,
        /// Video files inside the release, e.g. "Parity.Show.S01E01.1080p.WEB-DL-UM.mkv"
        /// (repeatable). Default: one file named after the release.
        #[arg(long = "file")]
        files: Vec<String>,
        #[arg(long, default_value_t = 60)]
        seconds: u32,
        #[arg(long, default_value = "/usr/bin/ffmpeg")]
        ffmpeg: PathBuf,
    },
}

fn fixture(
    out: PathBuf,
    name: String,
    files: Vec<String>,
    seconds: u32,
    ffmpeg: PathBuf,
) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        anyhow::bail!("--name must be a plain file name");
    }
    std::fs::create_dir_all(&out)?;
    let names = if files.is_empty() {
        vec![format!("{name}.mkv")]
    } else {
        files
    };
    let tmp = out.join(format!(".fixture-{}.mkv", fsutil::random_hex(4)?));
    let mut contents = vec![];
    for (i, f) in names.iter().enumerate() {
        if arr::paths::safe_rel(f).is_none() {
            anyhow::bail!("--file {f:?} must be a plain relative path");
        }
        // Distinct durations make every file (and so every piece) unique.
        let bytes = arr::fixture::generate_mkv(&ffmpeg, &tmp, (seconds + i as u32).min(600))?;
        contents.push((f.clone(), bytes));
    }
    let _ = std::fs::remove_file(&tmp);
    let multi = contents.len() > 1 || contents[0].0.contains('/');
    let t = arr::fixture::write_release(&out, &name, &contents)?;
    let torrent_path = out.join(format!("{name}.torrent"));
    std::fs::write(&torrent_path, &t.torrent)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "name": t.name,
            "info_hash": t.info_hash,
            "magnet": t.magnet,
            "torrent": torrent_path,
            "data_root": if multi { out.join(&name) } else { out.clone() },
            "files": t.files,
            "size": t.size,
            "note": "place the data where qBittorrent's save path maps (it then verifies the pieces and seeds); serve the .torrent from a mock Prowlarr download endpoint",
        }))?
    );
    Ok(())
}

fn init(
    state_dir: PathBuf,
    listen: std::net::SocketAddr,
    prowlarr_url: Option<String>,
) -> Result<()> {
    if let Some(u) = &prowlarr_url {
        config::parse_base_url(u, "--prowlarr-url")?;
    }
    // The template keeps server.allow_non_loopback off.
    if !listen.ip().is_loopback() {
        anyhow::bail!(
            "--listen must be a loopback address like 127.0.0.1:8787 (set server.allow_non_loopback in the config to expose the API)"
        );
    }
    fsutil::ensure_private_dir(&state_dir)?;
    let secrets = state_dir.join("secrets");
    fsutil::ensure_private_dir(&secrets)?;
    let mut created = vec![];
    let token = secrets.join("api_token");
    if !token.exists() {
        fsutil::write_new_private_file(&token, fsutil::random_hex(32)?.as_bytes())?;
        created.push(token.display().to_string());
    }
    let cfg = state_dir.join("config.toml");
    if !cfg.exists() {
        fsutil::write_new_private_file(
            &cfg,
            config::template(&listen.to_string(), prowlarr_url.as_deref()).as_bytes(),
        )?;
        created.push(cfg.display().to_string());
    }
    let next: Vec<&str> = if prowlarr_url.is_some() {
        vec![
            "write the Prowlarr API key to secrets/prowlarr_api_key (chmod 600)",
            "arr import --state-dir <STATE> --snapshot <native.json>",
            "arr serve --config <STATE>/config.toml",
        ]
    } else {
        vec![
            "edit <STATE>/config.toml: [library], [[indexer]], [embedded] (and optionally [tmdb], [tvmaze], [plex], [mcp]); secrets only as files under <STATE>/secrets (chmod 600)",
            "arr setup --config <STATE>/config.toml [--settings setup.toml]",
            "arr check --config <STATE>/config.toml",
            "arr serve --config <STATE>/config.toml",
            "see docs/STANDALONE.md for the full recipe",
        ]
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "state_dir": state_dir,
            "created": created,
            "next": next,
        }))?
    );
    Ok(())
}

async fn setup_cmd(config_path: PathBuf, settings: Option<PathBuf>, dry_run: bool) -> Result<()> {
    fsutil::check_private_file(&config_path)?;
    let text = String::from_utf8(fsutil::read_bounded(&config_path, 256 * 1024)?)
        .context("config is not UTF-8")?;
    let base = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let (cfg, pending) = config::parse_config(
        &text,
        &base,
        &config_path.display().to_string(),
        config::Secrets::Defer,
    )?;
    let state_dir = cfg.state_dir.clone();
    let active = import::load_active(&state_dir)?;
    let managed = active
        .as_ref()
        .filter(|l| l.manifest.files.contains_key(setup::SOURCE_FILE));
    let origin = active.as_ref().map(|_| {
        if managed.is_some() {
            "managed_setup"
        } else {
            "native_snapshot"
        }
    });
    let doc = match (&settings, managed) {
        (Some(p), _) => setup::read_doc(p)?,
        // Running setup again (say, after adding an indexer) keeps custom profiles and goals.
        (None, Some(l)) => setup::stored_doc(&state_dir, &l.manifest.import_id)?,
        (None, None) => setup::SetupDoc::default(),
    };
    let now = time::OffsetDateTime::now_utc();
    let prepared = setup::prepare(&doc, &cfg, now).map_err(|errs| {
        anyhow::anyhow!("the setup document is invalid:\n- {}", errs.join("\n- "))
    })?;
    let (index, grabs) = {
        let db = state_dir.join("state.sqlite");
        if db.exists() {
            let store = store::Store::open(&db)?;
            let mode = cfg.migration;
            (
                store.blocking(|c| library::load_index(c, mode))?,
                store.blocking(|c| setup::active_grabs(c))?,
            )
        } else {
            (
                arr::catalog::CatalogIndex::new(arr::catalog::Catalog {
                    schema_version: arr::catalog::CATALOG_SCHEMA_VERSION,
                    import_id: String::new(),
                    observed_at: String::new(),
                    movies: vec![],
                    series: vec![],
                }),
                vec![],
            )
        }
    };
    let conflicts = setup::replace_conflicts(origin, &prepared.normalized.settings, &index, &grabs);
    if !conflicts.is_empty() {
        anyhow::bail!("managed setup refused:\n- {}", conflicts.join("\n- "));
    }
    let mut out = if dry_run {
        setup::summary(&prepared, "dry_run", None)
    } else {
        let (status, dir) = import::write_and_activate(
            &state_dir,
            setup::SOURCE_FILE,
            &prepared.source,
            &prepared.normalized,
        )?;
        setup::summary(&prepared, status, Some(dir))
    };
    let ffprobe = match &cfg.library {
        Some(l) => match arr::probe::preflight(&l.ffprobe).await {
            Ok(v) => serde_json::json!({ "status": "ok", "version": v }),
            Err(e) => serde_json::json!({ "status": "failed", "message": e }),
        },
        None => serde_json::json!({ "status": "not_configured" }),
    };
    out["preflight"] = serde_json::json!({
        "secret_files_missing": pending,
        "ffprobe": ffprobe,
        "search_backend": cfg.search_backend(),
        "download_backend": cfg.download_backend(),
        "execution_enabled": cfg.execution_enabled,
    });
    out["next"] = serde_json::json!([
        "create any missing secret files (chmod 600) listed under preflight",
        "arr check --config <STATE>/config.toml",
        "arr serve --config <STATE>/config.toml (a running server picks up a new setup only after restart; POST /api/v1/setup activates without one)",
    ]);
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Reads a secret from stdin, so it stays out of the process list. At a terminal, asks for it.
fn read_secret(what: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read};
    let stdin = std::io::stdin();
    let mut s = String::new();
    if stdin.is_terminal() {
        eprintln!("Paste the {what}, then press Enter");
        stdin.lock().take(4096).read_line(&mut s)
    } else {
        stdin.lock().take(4096).read_to_string(&mut s)
    }
    .with_context(|| format!("couldn't read the {what}"))?;
    Ok(s.trim().to_string())
}

// The runtime is shut down (not `process::exit`), so kill_on_drop children stop; with a
// timeout, because the MCP adapter's stdin read can't be cancelled and would otherwise keep
// a failed `arr mcp` waiting for its client's next line.
fn main() -> std::process::ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: couldn't start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = match rt.block_on(run()) {
        Ok(code) => code,
        Err(e) => {
            // One line, so installers can show it as it is.
            eprintln!("Error: {}", arr::fsutil::one_line(&format!("{e:#}")));
            std::process::ExitCode::FAILURE
        }
    };
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    code
}

async fn run() -> Result<std::process::ExitCode> {
    let cli = Cli::parse();
    // Logs go to stderr; the MCP adapter stays quiet unless asked.
    let default_filter = match cli.command {
        Command::Mcp { .. }
        | Command::Wizard { .. }
        | Command::Report { .. }
        | Command::Set { .. }
        | Command::TmdbKey { .. }
        | Command::PlexSignin { .. } => "warn",
        _ => "info,arr=info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let result = match cli.command {
        Command::Init {
            state_dir,
            listen,
            prowlarr_url,
        } => init(state_dir, listen, prowlarr_url),
        Command::Import {
            state_dir,
            snapshot,
            observed_at,
            dry_run,
        } => {
            let observed_at = observed_at
                .map(|s| {
                    OffsetDateTime::parse(&s, &Rfc3339).context("--observed-at must be RFC 3339")
                })
                .transpose()?;
            let summary = tokio::task::spawn_blocking(move || {
                import::run_import(&import::ImportOptions {
                    state_dir,
                    snapshot,
                    observed_at,
                    dry_run,
                })
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Command::Setup {
            config,
            settings,
            dry_run,
        } => setup_cmd(config, settings, dry_run).await,
        Command::Bootstrap {
            config,
            media,
            restart,
            dry_run,
            batch,
        } => {
            let cfg = config::load(&config)?;
            let state = api::AppState::new(cfg)?;
            use arr::settings::MediaKind::{Movie, Series};
            let kinds: Vec<arr::settings::MediaKind> = match media.as_deref() {
                Some("all") => vec![Movie, Series],
                Some("movies") => vec![Movie],
                Some("tv") => vec![Series],
                Some(other) => anyhow::bail!("--media must be movies, tv or all (got {other:?})"),
                // The libraries this install has; an asked-for one that isn't there fails.
                None => {
                    let plex = state.cfg.plex.as_ref();
                    let kinds: Vec<_> = [
                        (Movie, plex.and_then(|p| p.movie_section)),
                        (Series, plex.and_then(|p| p.tv_section)),
                    ]
                    .into_iter()
                    .filter_map(|(m, s)| s.map(|_| m))
                    .collect();
                    if kinds.is_empty() {
                        anyhow::bail!(
                            "no Plex library is configured ([plex] movie_section or tv_section)"
                        );
                    }
                    kinds
                }
            };
            let mut out = vec![];
            for m in kinds {
                let mut first = restart;
                let mut last = None;
                for _ in 0..100_000 {
                    let run = arr::bootstrap::batch(&state, m, first, dry_run, batch)
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}: {}", e.code, e.message))?;
                    first = false;
                    eprintln!(
                        "{:?}: {} offset {}/{} titles +{} files +{}",
                        run.media,
                        run.status,
                        run.offset,
                        run.total.map_or_else(|| "?".to_string(), |t| t.to_string()),
                        run.counts.titles_created,
                        run.counts.files_recorded
                    );
                    let running = run.status == "running";
                    let stuck = last
                        .as_ref()
                        .is_some_and(|l: &arr::bootstrap::Run| l.offset == run.offset);
                    last = Some(run);
                    if !running || stuck {
                        break;
                    }
                }
                out.extend(last);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "runs": out }))?
            );
            let failed: Vec<String> = out
                .iter()
                .filter(|r| r.status != "done")
                .map(|r| {
                    let what = match r.media {
                        arr::settings::MediaKind::Movie => "movies",
                        arr::settings::MediaKind::Series => "TV",
                    };
                    let why = r.last_error.as_deref().unwrap_or("it didn't finish");
                    format!("{what}: {why}")
                })
                .collect();
            if !failed.is_empty() {
                anyhow::bail!("reading Plex's library failed ({})", failed.join("; "));
            }
            Ok(())
        }
        Command::Check { config } => {
            let cfg = config::load(&config)?;
            let ffprobe = match &cfg.library {
                Some(l) => Some(arr::probe::preflight(&l.ffprobe).await),
                None => None,
            };
            if cfg.execution_enabled
                && let Some(Err(e)) = &ffprobe
            {
                anyhow::bail!("media validation preflight failed: {e}");
            }
            let state = api::AppState::new(cfg)?;
            state.store.ping().await?;
            let index = state.index();
            let import = state.current().map(|l| {
                serde_json::json!({
                    "import_id": l.import_id,
                    "origin": l.origin,
                    "observed_at": l.observed_at,
                    "movies": index.catalog.movies.len(),
                    "series": index.catalog.series.len(),
                    "blocking_issues": l.settings.blocking_issues,
                })
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "config": "ok",
                    "listen": state.cfg.listen.to_string(),
                    "prowlarr_configured": state.cfg.prowlarr.is_some(),
                    "search_backend": state.cfg.search_backend(),
                    "native_indexers": state.cfg.indexers.iter().map(|i| serde_json::json!({ "id": i.id, "name": i.name, "kind": i.kind })).collect::<Vec<_>>(),
                    "tvmaze_enabled": state.tvmaze.is_some(),
                    "tmdb_configured": state.tmdb.is_some(),
                    "execution_enabled": state.cfg.execution_enabled,
                    "qbittorrent_configured": state.cfg.qbittorrent.is_some(),
                    "download_backend": state.cfg.download_backend(),
                    "library_configured": state.cfg.library.is_some(),
                    "plex_configured": state.plex.is_some(),
                    "mcp_enabled": state.cfg.mcp.is_some(),
                    "ffprobe": ffprobe.map(|r| match r {
                        Ok(v) => serde_json::json!({ "status": "ok", "version": v }),
                        Err(e) => serde_json::json!({ "status": "failed", "message": e }),
                    }),
                    "indexer_mapping": state.search.as_ref().and_then(|b| b.native()).and_then(|r| state.current().map(|l| r.mapping(&l.settings))),
                    "active_import": import,
                }))?
            );
            Ok(())
        }
        Command::Serve { config } => {
            let cfg = config::load(&config)?;
            let _reporting = arr::report::start(cfg.reporting, "server");
            let result = api::serve(cfg).await;
            if let Err(e) = &result {
                arr::report::error("server", &format!("Arr stopped: {e:#}"));
            }
            result
        }
        Command::Report {
            config,
            component,
            message,
        } => {
            let _reporting = arr::report::start(arr::report::enabled_in(&config), &component);
            arr::report::error(&component, &message);
            Ok(())
        }
        Command::Set {
            config,
            name,
            value,
        } => {
            let value = match arr::set::secret(&name)? {
                Some(what) if value.is_some() => {
                    anyhow::bail!("give the {what} on stdin, not on the command line")
                }
                Some(what) => Some(read_secret(what)?),
                None => value,
            };
            println!("{}", arr::set::run(&config, &name, value.as_deref()).await?);
            Ok(())
        }
        Command::TmdbKey {
            config,
            check: _,
            tmdb_url,
        } => {
            let tmdb_url = match tmdb_url {
                Some(u) => u,
                None => arr::tmdbkey::API_URL.parse()?,
            };
            let key = read_secret("TMDB key")?;
            match config {
                Some(config) => {
                    println!("{}", arr::set::run(&config, "tmdb-key", Some(&key)).await?)
                }
                None => {
                    arr::tmdbkey::verify(&tmdb_url, &key).await?;
                    println!("TMDB accepted that key.");
                }
            }
            Ok(())
        }
        Command::Wizard {
            state_dir,
            bind,
            port,
            torrent_port,
            upload_limit_kib,
            ffprobe,
            plex_tv_url,
            plex_auth_url,
            no_docker,
            open,
            plex_installer,
            plex_port,
            error_reports,
            tmdb_url,
        } => {
            wizard::run(wizard::Options {
                state_dir,
                bind,
                port,
                torrent_port,
                upload_limit_kib,
                ffprobe,
                arr: std::env::current_exe().context("can't find the arr binary")?,
                plex_tv: plex_tv_url,
                plex_auth: plex_auth_url,
                docker: !no_docker,
                open,
                plex_installer,
                plex_port,
                error_reports,
                tmdb_url,
            })
            .await
        }
        Command::PlexInstall { owner, media } => {
            let owner = match owner {
                Some(o) => {
                    let (u, g) = o.split_once(':').context("--owner is UID:GID")?;
                    Some((u.parse()?, g.parse()?))
                }
                None => None,
            };
            let media = media.unwrap_or_else(arr::plexinstall::default_media);
            arr::plexinstall::run(arr::plexinstall::Options { owner, media }).await
        }
        Command::PlexSignin {
            config,
            check,
            plex_tv_url,
            plex_auth_url,
        } => {
            // 0 accepted, 1 refused (or sign-in failed), 2 unreachable.
            let code = arr::plexsignin::main(arr::plexsignin::Options {
                config,
                check,
                plex_tv: plex_tv_url,
                plex_auth: plex_auth_url,
            })
            .await;
            return Ok(std::process::ExitCode::from(
                u8::try_from(code).unwrap_or(1),
            ));
        }
        Command::Mcp {
            api_url,
            token_file,
            timeout_secs,
        } => {
            mcp::run(mcp::Options {
                api_url,
                token_file,
                timeout: std::time::Duration::from_secs(timeout_secs),
            })
            .await
        }
        Command::Fixture {
            out,
            name,
            files,
            seconds,
            ffmpeg,
        } => fixture(out, name, files, seconds, ffmpeg),
    };
    result.map(|()| std::process::ExitCode::SUCCESS)
}
