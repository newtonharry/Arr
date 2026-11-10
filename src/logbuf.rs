//! The last few warnings and errors Arr logged, kept in memory for the web app's Health
//! screen. Linux logs go to the journal and not to a file, so there is nothing to read back:
//! a tracing layer keeps them instead. Each one is scrubbed with the scrubber error reports
//! use (names, paths, addresses and tokens removed; docs/PRIVACY.md) before it is stored,
//! and the list is capped, so nothing identifying is ever held or served.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Events kept.
pub const CAPACITY: usize = 50;
/// Longest message kept, in characters.
const MAX_MESSAGE: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Seconds since the Unix epoch.
    pub at: i64,
    /// `warning` or `error`.
    pub level: &'static str,
    pub component: String,
    pub message: String,
}

static RING: Mutex<VecDeque<Problem>> = Mutex::new(VecDeque::new());

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Stores one event, scrubbed. `target` is the module that logged it.
pub fn record(at: i64, level: &Level, target: &str, text: &str) {
    let level = match *level {
        Level::ERROR => "error",
        Level::WARN => "warning",
        _ => return,
    };
    let message: String = crate::report::scrub(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_MESSAGE)
        .collect();
    let p = Problem {
        at,
        level,
        component: component(target),
        message,
    };
    let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() >= CAPACITY {
        ring.pop_front();
    }
    ring.push_back(p);
}

/// "arr::worker::pass" is the worker; a library's own name is kept as it is.
fn component(target: &str) -> String {
    let t = target.strip_prefix("arr::").unwrap_or(target);
    t.split("::").next().unwrap_or("arr").to_string()
}

/// The stored events, newest first.
pub fn recent() -> Vec<Problem> {
    RING.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .cloned()
        .collect()
}

/// Forgets everything (tests).
pub fn clear() {
    RING.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A layer that keeps WARN and ERROR events. Add it with a `LevelFilter::WARN` filter.
pub fn layer() -> RingLayer {
    RingLayer
}

pub struct RingLayer;

#[derive(Default)]
struct Text {
    message: String,
    fields: String,
}

/// The only structured fields kept: numbers about what happened. Everything else a log line
/// carries (a search query, a title, a provider's name, a path, an address, an error's text)
/// can be private and isn't recognisable by the scrubber once it is unquoted, so it is dropped.
const SAFE_FIELDS: [&str; 7] = [
    "status",
    "code",
    "count",
    "attempt",
    "pages",
    "elapsed_ms",
    "port",
];

impl Text {
    fn field(&mut self, name: &str, value: String) {
        let plain =
            !value.is_empty() && value.len() <= 12 && value.bytes().all(|b| b.is_ascii_digit());
        if SAFE_FIELDS.contains(&name) && plain {
            let _ = write!(self.fields, " {name}={value}");
        }
    }
}

impl Visit for Text {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.field(field.name(), format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        }
    }
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !matches!(*meta.level(), Level::WARN | Level::ERROR) {
            return;
        }
        let mut text = Text::default();
        event.record(&mut text);
        record(
            now(),
            meta.level(),
            meta.target(),
            &format!("{}{}", text.message, text.fields),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    // The ring is process-wide: tests that use it take turns.
    static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn warnings_and_errors_are_kept_scrubbed_and_newest_first() {
        let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let sub = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(sub, || {
            tracing::info!("not kept");
            tracing::warn!(
                target: "arr::worker",
                "couldn't read /home/alice/Movies/Secret Film (2020)/a.mkv from http://192.168.1.20:32400/library?X-Plex-Token=abcdef0123456789"
            );
            tracing::error!(target: "arr::plex", error = "bad", "Plex said no");
        });
        let got = recent();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].level, "error");
        assert_eq!(got[0].component, "plex");
        assert!(got[0].message.contains("Plex said no"));
        let warn = &got[1];
        assert_eq!(warn.level, "warning");
        assert_eq!(warn.component, "worker");
        for leak in [
            "alice",
            "Secret Film",
            "192.168",
            "abcdef0123456789",
            "/home",
        ] {
            assert!(!warn.message.contains(leak), "{leak} in {}", warn.message);
        }
    }

    #[test]
    fn structured_fields_other_than_counts_never_reach_the_list() {
        let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let sub = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(sub, || {
            // The shape of the search warnings in search.rs.
            let (label, query, failure) = ("My Private Indexer", "Secret Film", "timed out");
            tracing::warn!(target: "arr::search", provider = %label, query = %query, failure = %failure, pages = 3u32, "search query partially failed");
            tracing::error!(target: "arr::worker", path = ?std::path::Path::new("/srv/Secret Film"), status = 502u16, "import failed");
        });
        let got = recent();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].message, "search query partially failed pages=3");
        assert_eq!(got[0].message, "import failed status=502");
        for p in &got {
            for leak in [
                "Secret",
                "Private",
                "query=",
                "provider=",
                "timed out",
                "/srv",
            ] {
                assert!(!p.message.contains(leak), "{leak} in {}", p.message);
            }
        }
    }

    #[test]
    fn the_list_is_capped_and_messages_are_short() {
        let _t = TURN.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        for i in 0..(CAPACITY + 10) {
            record(
                i as i64,
                &Level::WARN,
                "arr::x",
                &format!("problem number {i}"),
            );
        }
        let got = recent();
        assert_eq!(got.len(), CAPACITY);
        assert_eq!(got[0].at, (CAPACITY + 9) as i64);
        record(0, &Level::ERROR, "arr::x", &"word ".repeat(500));
        assert!(recent()[0].message.chars().count() <= MAX_MESSAGE);
        record(0, &Level::INFO, "arr::x", "ignored");
        assert_eq!(recent().len(), CAPACITY);
    }
}
