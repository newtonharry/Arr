//! Opt-in error reports (docs/PRIVACY.md).
//!
//! Off unless the user said yes during setup (`[reporting] enabled = true`). Reports go to
//! Arr's Sentry project, or wherever `ARR_SENTRY_DSN` points (a self-hosted Sentry or
//! GlitchTip). Every event is scrubbed here, before it leaves the computer: no host name,
//! user, request, breadcrumbs or extra data, and messages lose URLs, emails, paths, IP
//! addresses, host names, quoted names, tokens and `key=value` secrets.

use regex::Regex;
use sentry::protocol::Event;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Arr's Sentry project (EU region). A DSN only lets reports in; it can't read anything.
const ARR_DSN: &str = "https://cf6596faade13d63657c233888c8464d@o4512395548884992.ingest.de.sentry.io/4512395555307600";
/// Longest message sent.
const MAX_MESSAGE: usize = 2000;

/// Where reports go: `ARR_SENTRY_DSN` when set (a self-hosted Sentry or GlitchTip; empty
/// turns reporting off, as does one that isn't a DSN), else Arr's project.
pub fn dsn() -> Option<sentry::types::Dsn> {
    parse_dsn(&std::env::var("ARR_SENTRY_DSN").unwrap_or_else(|_| ARR_DSN.to_string()))
}

fn parse_dsn(d: &str) -> Option<sentry::types::Dsn> {
    d.trim().parse().ok()
}

/// Whether this build can send reports at all (so setup only asks when it can).
pub fn available() -> bool {
    dsn().is_some()
}

/// Reporting while held; dropping it sends what's queued (for up to two seconds).
pub struct Reporting(Option<Arc<sentry::Client>>);

impl Drop for Reporting {
    fn drop(&mut self) {
        if let Some(c) = &self.0 {
            c.close(Some(Duration::from_secs(2)));
        }
    }
}

impl Reporting {
    /// Whether reports are actually being sent.
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }
}

/// Starts reporting for `component` ("server", "setup", "installer", ...) when the user
/// agreed and there's somewhere to report to. Panics are reported from then on too.
pub fn start(enabled: bool, component: &str) -> Reporting {
    match dsn() {
        Some(dsn) if enabled => start_to(dsn, component),
        _ => Reporting(None),
    }
}

fn start_to(dsn: sentry::types::Dsn, component: &str) -> Reporting {
    let mut options = sentry::ClientOptions::default();
    options.release = sentry::release_name!();
    options.send_default_pii = false;
    options.server_name = None;
    options.attach_stacktrace = false;
    options.dsn = Some(dsn);
    options.before_send = Some(Arc::new(|e| Some(scrub_event(e))));
    // Bound to the main hub, not this thread's as `sentry::init` does: the setup page
    // turns reporting on from one worker thread and fails on another.
    let client = Arc::new(sentry::Client::from(sentry::apply_defaults(options)));
    if !client.is_enabled() {
        return Reporting(None);
    }
    let hub = sentry::Hub::main();
    hub.bind_client(Some(client.clone()));
    hub.configure_scope(|s| s.set_tag("component", component));
    report_panics();
    Reporting(Some(client))
}

/// Panics are reported through the main hub as well; sentry's own panic hook uses the
/// panicking thread's hub, which may predate reporting.
fn report_panics() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let next = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let hub = sentry::Hub::main();
            if let Some(client) = hub.client().filter(|c| c.is_enabled()) {
                hub.capture_event(panic_event(info));
                client.flush(Some(Duration::from_secs(2)));
            }
            next(info);
        }));
    });
}

#[allow(clippy::field_reassign_with_default)] // Event is non_exhaustive: no struct literal
fn panic_event(info: &std::panic::PanicHookInfo<'_>) -> Event<'static> {
    let p = info.payload();
    let message = p
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<Any>");
    let x = sentry::protocol::Exception {
        ty: "panic".into(),
        value: Some(message.to_string()),
        stacktrace: sentry::integrations::backtrace::current_stacktrace(),
        mechanism: Some(sentry::protocol::Mechanism {
            ty: "panic".into(),
            handled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut e = Event::default();
    e.exception = vec![x].into();
    e.level = sentry::Level::Fatal;
    e
}

/// Reports an error that stopped something the user was doing. Does nothing unless
/// reporting was started.
pub fn error(component: &str, message: &str) {
    let hub = sentry::Hub::main();
    hub.with_scope(
        |s| s.set_tag("component", component),
        || hub.capture_message(&scrub(message), sentry::Level::Error),
    );
}

/// Error detail shown to the user but left out of reports: another program's output, or
/// an import error naming a title. Wrap it, and add the reportable part as context.
#[derive(Debug)]
pub struct Private(pub String);

impl std::fmt::Display for Private {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Private {}

/// An error chain as `{:#}` shows it, without its [`Private`] parts.
pub fn reportable(e: &anyhow::Error) -> String {
    e.chain()
        .filter(|c| !c.is::<Private>())
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

/// For `arr report`: whether a config file says yes, read without its secrets (a broken
/// install may have none yet).
pub fn enabled_in(config: &std::path::Path) -> bool {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .and_then(|t| t.get("reporting")?.get("enabled")?.as_bool())
        .unwrap_or(false)
}

fn scrub_event(mut e: Event<'static>) -> Event<'static> {
    e.server_name = None;
    e.user = None;
    e.request = None;
    e.breadcrumbs = Default::default();
    e.extra.clear();
    e.modules.clear();
    if let Some(m) = e.message.as_mut() {
        *m = scrub(m);
    }
    if let Some(l) = e.logentry.as_mut() {
        l.message = scrub(&l.message);
        l.params.clear();
    }
    for x in e.exception.values.iter_mut() {
        if let Some(v) = x.value.as_mut() {
            *v = scrub(v);
        }
    }
    // Device context can carry the computer's name; the OS and Rust contexts are enough.
    e.contexts.retain(|k, _| k == "os" || k == "rust");
    e
}

static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(p).expect("valid pattern");
    vec![
        // Credentials first, while their keys are still there to see.
        (r(r"(?i)\b(bearer|basic)\s+[^\s,;]+"), "$1 <secret>"),
        (
            r(
                r#"(?i)\b(token|secret|password|passwd|pwd|api[_-]?key|authorization|cookie|code)(\s*[=:]\s*)[^\s,;&"']+"#,
            ),
            "$1$2<secret>",
        ),
        (r(r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s"'<>]+"#), "<url>"),
        (
            r(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+"),
            "<email>",
        ),
        // Quoted names: titles, file names, folder names.
        (r(r#""[^"\n]*""#), "\"…\""),
        (r(r"“[^”\n]*”|‘[^’\n]*’"), "“…”"),
        (r(r"(^|[\s(:=\[,])'[^'\n]*'"), "$1'…'"),
        // Paths: Windows (C:\…, \\server\…), home (~/…) and Unix (/…). Names in them can
        // hold spaces and apostrophes, so a path runs to the next separator: ": ", "; ",
        // ", ", a double quote, a line end or the end.
        (
            r(r#"\b[A-Za-z]:[\\/][^\n"]*?(: |; |, |"|\n|$)"#),
            "<path>$1",
        ),
        (r(r#"\\\\[^\n"]+?(: |; |, |"|\n|$)"#), "<path>$1"),
        (r(r#"~/[^\n"]*?(: |; |, |"|\n|$)"#), "<path>$1"),
        (
            r(r#"(^|[\s(=\[,:])/[^\n"]+?(: |; |, |"|\n|$)"#),
            "$1<path>$2",
        ),
        (r(r"\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b"), "<ip>"),
        (r(r"(?i)\b[0-9a-f]{0,4}(:[0-9a-f]{0,4}){2,7}\b"), "<ip>"),
        (
            r(
                r"(?i)\b[a-z0-9-]+(\.[a-z0-9-]+)*\.(app|dev|io|com|net|org|tv|me|co|local|lan|home|arpa|internal)\b",
            ),
            "<host>",
        ),
        // Long unbroken runs: tokens, keys, machine ids, hashes.
        (r(r"\b[A-Za-z0-9_\-]{20,}\b"), "<token>"),
    ]
});

/// A message with everything personal or secret replaced (see the module docs).
pub fn scrub(text: &str) -> String {
    let mut s: String = text.chars().take(MAX_MESSAGE).collect();
    for (re, with) in RULES.iter() {
        s = re.replace_all(&s, *with).into_owned();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_personal_or_secret_survives() {
        let secrets = [
            "2abcDEF_ngrokTokenValue123456",
            "GOCSPX-supersecretvalue",
            "harry@example.com",
            "quiet-river-1234.ngrok-free.app",
            "192.168.0.183",
            "/home/harry/Movies",
            "C:\\Users\\Harry\\Media",
            "The Matrix (1999)",
            "hunter2",
            "plexTokenAbcdef0123456789",
            "fe80::1ff:fe23:4567:890a",
        ];
        let message = "import failed for \"The Matrix (1999)\" in /home/harry/Movies/The Matrix (1999) \
            and C:\\Users\\Harry\\Media\\x.mkv; Authorization: Bearer plexTokenAbcdef0123456789; \
            password=hunter2; token=2abcDEF_ngrokTokenValue123456 secret: GOCSPX-supersecretvalue \
            mail harry@example.com via https://quiet-river-1234.ngrok-free.app/mcp?code=abc \
            and quiet-river-1234.ngrok-free.app at 192.168.0.183:32400 or fe80::1ff:fe23:4567:890a";
        let out = scrub(message);
        for s in secrets {
            assert!(!out.contains(s), "{s:?} survived in {out:?}");
        }
        // The shape of the error stays readable.
        assert!(out.starts_with("import failed for"), "{out}");
    }

    #[test]
    fn paths_go_whole_even_with_spaces() {
        assert_eq!(
            scrub(r"can't read C:\Users\Harry Potter\Media\config.toml: access denied"),
            "can't read <path>: access denied"
        );
        assert_eq!(
            scrub("can't create /srv/media/Movies/The Matrix (1999)/Harry's cut.mkv: no space"),
            "can't create <path>: no space"
        );
        assert_eq!(scrub("in ~/My Movies"), "in <path>");
        assert_eq!(
            scrub(r"on \\nas\Media Share\TV; retrying"),
            "on <path>; retrying"
        );
    }

    #[test]
    fn private_detail_stays_out_of_reports() {
        // How the setup page's Plex import fails (wizard.rs), with bootstrap.rs's message.
        let e = anyhow::Error::new(Private(
            "The Matrix: tmdb connection failed; the run resumes at this item".into(),
        ))
        .context("the Plex import didn't finish");
        assert!(
            format!("{e:#}")
                .ends_with(": The Matrix: tmdb connection failed; the run resumes at this item")
        );
        assert_eq!(reportable(&e), "the Plex import didn't finish");
    }

    /// The setup page turns reporting on from one worker thread and fails on another,
    /// whose hub already exists.
    #[test]
    fn a_failure_on_another_thread_is_reported() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (got_tx, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = vec![];
                let mut chunk = [0; 4096];
                // Headers, then as much body as they announce.
                while let Ok(n @ 1..) = s.read(&mut chunk) {
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_lowercase();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}",
                );
                let _ = got_tx.send(String::from_utf8_lossy(&buf).into_owned());
            }
        });
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _ = sentry::Hub::current(); // this thread's hub, made before reporting is on
            wait.recv().unwrap();
            error("setup", "failed on another thread");
            panic!("panicked on another thread");
        });
        let dsn = parse_dsn(&format!("http://key@{addr}/1")).unwrap();
        let reporting = start_to(dsn, "setup");
        go.send(()).unwrap();
        assert!(worker.join().is_err());
        drop(reporting);
        let mut bodies = String::new();
        for _ in 0..2 {
            bodies += &got
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("not everything was sent: {bodies}"));
        }
        assert!(bodies.contains("failed on another thread"), "{bodies}");
        assert!(bodies.contains("panicked on another thread"), "{bodies}");
    }

    #[test]
    fn a_bad_dsn_turns_reporting_off() {
        assert!(parse_dsn(ARR_DSN).is_some());
        assert!(parse_dsn("http://key@127.0.0.1:9/1").is_some());
        for bad in ["", "  ", "not a dsn", "https://sentry.io/1"] {
            assert!(parse_dsn(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn ordinary_wording_is_kept() {
        let m = "the Plex import didn't finish; run this again to continue it";
        assert_eq!(scrub(m), m);
        assert_eq!(
            scrub("arr check rejected the config"),
            "arr check rejected the config"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // Event is non_exhaustive: no struct literal
    fn events_lose_identifying_fields() {
        let mut e = Event::default();
        e.message = Some("failed at /srv/media/Movies".into());
        e.server_name = Some("harrys-laptop".into());
        e.contexts.insert(
            "device".into(),
            sentry::protocol::Context::Other(Default::default()),
        );
        e.contexts.insert(
            "os".into(),
            sentry::protocol::Context::Other(Default::default()),
        );
        let e = scrub_event(e);
        assert_eq!(e.server_name, None);
        assert_eq!(e.message.as_deref(), Some("failed at <path>"));
        assert!(!e.contexts.contains_key("device") && e.contexts.contains_key("os"));
    }

    #[test]
    fn the_config_switch_is_read_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(&p, "[server]\nlisten = \"127.0.0.1:1\"\n").unwrap();
        assert!(!enabled_in(&p));
        std::fs::write(
            &p,
            "[server]\nlisten = \"x\"\n[reporting]\nenabled = true\n",
        )
        .unwrap();
        assert!(enabled_in(&p));
        assert!(!enabled_in(&tmp.path().join("missing.toml")));
    }
}
