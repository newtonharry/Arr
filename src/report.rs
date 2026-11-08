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

/// Arr's Sentry project (EU region). A DSN only lets reports in; it can't read anything.
const ARR_DSN: &str = "https://cf6596faade13d63657c233888c8464d@o4512395548884992.ingest.de.sentry.io/4512395555307600";
/// Longest message sent.
const MAX_MESSAGE: usize = 2000;

/// Where reports go: `ARR_SENTRY_DSN` when set (a self-hosted Sentry or GlitchTip; empty
/// turns reporting off), else Arr's project.
pub fn dsn() -> Option<String> {
    let d = std::env::var("ARR_SENTRY_DSN").unwrap_or_else(|_| ARR_DSN.to_string());
    Some(d.trim().to_string()).filter(|d| !d.is_empty())
}

/// Whether this build can send reports at all (so setup only asks when it can).
pub fn available() -> bool {
    dsn().is_some()
}

/// Reporting while held; dropping it sends what's queued (for up to two seconds).
pub struct Reporting(Option<sentry::ClientInitGuard>);

impl Reporting {
    /// Whether reports are actually being sent.
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }
}

/// Starts reporting for `component` ("server", "setup", "installer", ...) when the user
/// agreed and there's somewhere to report to. Panics are reported from then on too.
pub fn start(enabled: bool, component: &str) -> Reporting {
    if !enabled {
        return Reporting(None);
    }
    let Some(dsn) = dsn() else {
        return Reporting(None);
    };
    let mut options = sentry::ClientOptions::default();
    options.release = sentry::release_name!();
    options.send_default_pii = false;
    options.server_name = None;
    options.attach_stacktrace = false;
    options.before_send = Some(Arc::new(|e| Some(scrub_event(e))));
    let guard = sentry::init((dsn, options));
    if !guard.is_enabled() {
        return Reporting(None);
    }
    sentry::configure_scope(|s| s.set_tag("component", component));
    Reporting(Some(guard))
}

/// Reports an error that stopped something the user was doing. Does nothing unless
/// reporting was started.
pub fn error(component: &str, message: &str) {
    sentry::with_scope(
        |s| s.set_tag("component", component),
        || sentry::capture_message(&scrub(message), sentry::Level::Error),
    );
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
        // Paths: Windows (C:\…, \\server\…), home (~/…) and Unix (/…).
        (r(r#"\b[A-Za-z]:[\\/][^\s"'<>|]*"#), "<path>"),
        (r(r#"\\\\[^\s"'<>|]+"#), "<path>"),
        (r(r#"~/[^\s"'<>]*"#), "<path>"),
        (r(r#"(^|[\s(=\[,:])/[^\s"'<>]+"#), "$1<path>"),
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
