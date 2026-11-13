//! How often the indexers are asked about sport. A search for the copies of an event costs up to
//! [`super::find::MAX_QUERIES`] queries to every indexer, so it is not left to the caller:
//!
//! - **One search per event at a time.** Requests for the same event queue on its slot; those
//!   behind the one that searches get its answer.
//! - **A cooldown per event.** Within it, whatever is asked (including `fresh`, and after a
//!   search that failed), the answer is the last one with its age. After a complete answer a
//!   plain request reuses it for longer (the time to live).
//! - **A rate for all events together.** At most `per_minute` searches start in a minute across
//!   the whole server, so a client that walks every event id drives a bounded number of queries:
//!   the rest are answered from what was found or told to come back.
//! - **Bounded memory.** At most [`MAX_SLOTS`] events are remembered; idle ones are dropped.
//!
//! The indexers' own gates (spacing, backoff, query limits, concurrency) still apply to every
//! query that is sent.

use super::find::Searched;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Events remembered at most.
pub const MAX_SLOTS: usize = 256;
/// A search for an event is not repeated within this, whatever is asked.
pub const COOLDOWN: Duration = Duration::from_secs(120);
/// A complete answer is reused for this long by a plain request.
pub const TTL: Duration = Duration::from_secs(15 * 60);
/// Searches that may start in a minute, all events together.
pub const PER_MINUTE: usize = 6;

/// When an event was last searched, and what that brought back (`None` while it runs, or if
/// the request that ran it was dropped before it finished: the start still counts).
struct Entry {
    at: Instant,
    searched: Option<Arc<Searched>>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Entry>>>;

pub struct CopiesGate {
    slots: Mutex<HashMap<String, Slot>>,
    starts: Mutex<VecDeque<Instant>>,
    cooldown_ms: AtomicU64,
    ttl_ms: AtomicU64,
    per_minute: AtomicUsize,
}

/// What a request is answered with.
pub enum Served {
    /// A search ran for this request (or was running, and this request queued behind it).
    Searched(Arc<Searched>),
    /// The last search, `age` old, because it is still fresh enough or the event was searched
    /// too lately or too often. (The search, not a judgement of it: see `find::judge_search`.)
    Cached { found: Arc<Searched>, age: Duration },
    /// Nothing is known yet and the event was searched too lately, or the server is already
    /// searching as much as it allows.
    Busy { retry_after: Duration },
}

impl Default for CopiesGate {
    fn default() -> Self {
        CopiesGate {
            slots: Mutex::new(HashMap::new()),
            starts: Mutex::new(VecDeque::new()),
            cooldown_ms: AtomicU64::new(COOLDOWN.as_millis() as u64),
            ttl_ms: AtomicU64::new(TTL.as_millis() as u64),
            per_minute: AtomicUsize::new(PER_MINUTE),
        }
    }
}

impl CopiesGate {
    /// For tests: other limits, so that a test need not wait minutes.
    #[doc(hidden)]
    pub fn set_limits(&self, cooldown: Duration, ttl: Duration, per_minute: usize) {
        self.cooldown_ms
            .store(cooldown.as_millis() as u64, Ordering::Relaxed);
        self.ttl_ms.store(ttl.as_millis() as u64, Ordering::Relaxed);
        self.per_minute.store(per_minute, Ordering::Relaxed);
    }

    /// How many events are remembered now.
    pub fn remembered(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn slot(&self, id: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = slots.get(id) {
            return s.clone();
        }
        if slots.len() >= MAX_SLOTS {
            // Only a slot nobody holds can go: one with no answer yet, or an old one.
            let ttl = Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed));
            slots.retain(|_, s| {
                if Arc::strong_count(s) > 1 {
                    return true;
                }
                match s.try_lock() {
                    Ok(g) => g.as_ref().is_some_and(|e| e.at.elapsed() < ttl),
                    Err(_) => true,
                }
            });
            // Still full of fresh answers: drop the oldest idle ones.
            while slots.len() >= MAX_SLOTS {
                let oldest = slots
                    .iter()
                    .filter(|(_, s)| Arc::strong_count(s) == 1)
                    .filter_map(|(k, s)| {
                        s.try_lock()
                            .ok()
                            .map(|g| (k.clone(), g.as_ref().map(|e| e.at)))
                    })
                    .min_by_key(|(_, at)| *at)
                    .map(|(k, _)| k);
                match oldest {
                    Some(k) => {
                        slots.remove(&k);
                    }
                    None => break,
                }
            }
        }
        let s: Slot = Arc::new(tokio::sync::Mutex::new(None));
        slots.insert(id.to_string(), s.clone());
        s
    }

    /// Takes a place among the searches of the last minute, or says when to come back.
    fn take_start(&self) -> Result<(), Duration> {
        let cap = self.per_minute.load(Ordering::Relaxed);
        let mut starts = self.starts.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while starts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            starts.pop_front();
        }
        if starts.len() >= cap {
            let wait = starts.front().map_or(Duration::from_secs(60), |t| {
                Duration::from_secs(60).saturating_sub(now.duration_since(*t))
            });
            return Err(wait);
        }
        starts.push_back(now);
        Ok(())
    }

    /// Answers a request for the event `id`: from the last search if it is recent enough,
    /// else by running `search` (once, however many are asking). An error from `search` is
    /// the caller's (a missing backend): it cost no query and is not remembered.
    pub async fn serve<F, Fut, E>(&self, id: &str, fresh: bool, search: F) -> Result<Served, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Searched, E>>,
    {
        let slot = self.slot(id);
        // Everyone asking for this event waits here for the one that is searching.
        let mut guard = slot.lock().await;
        let cooldown = Duration::from_millis(self.cooldown_ms.load(Ordering::Relaxed));
        let ttl = Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed));
        if let Some(e) = guard.as_ref() {
            let age = e.at.elapsed();
            // Fresh, or the last search did not finish: not again within the cooldown.
            let limit = match &e.searched {
                Some(f) if !fresh && f.complete => ttl,
                _ => cooldown,
            };
            if age < limit {
                return Ok(match &e.searched {
                    Some(f) => Served::Cached {
                        found: f.clone(),
                        age,
                    },
                    // A search for this event began (and may have sent its queries) and was
                    // dropped: wait out the cooldown rather than begin another.
                    None => Served::Busy {
                        retry_after: cooldown.saturating_sub(age),
                    },
                });
            }
        }
        if let Err(retry_after) = self.take_start() {
            return Ok(match guard.as_ref().and_then(|e| e.searched.clone()) {
                Some(f) => Served::Cached {
                    age: guard.as_ref().map_or(Duration::ZERO, |e| e.at.elapsed()),
                    found: f,
                },
                None => Served::Busy { retry_after },
            });
        }
        // The start is recorded before the search is awaited, so that a request that is
        // dropped half way (a client that went away) still spends the event's cooldown, and the
        // one queued behind it does not begin another at once.
        let previous = guard.as_ref().and_then(|e| e.searched.clone());
        *guard = Some(Entry {
            at: Instant::now(),
            searched: None,
        });
        let found = match search().await {
            Ok(f) => Arc::new(f),
            Err(e) => {
                // An error is the caller's (a missing backend): no query was sent, so the
                // start is not held against the event.
                *guard = previous.map(|f| Entry {
                    at: Instant::now() - cooldown.max(ttl),
                    searched: Some(f),
                });
                return Err(e);
            }
        };
        *guard = Some(Entry {
            at: Instant::now(),
            searched: Some(found.clone()),
        });
        Ok(Served::Searched(found))
    }
}
