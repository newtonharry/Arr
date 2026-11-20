//! How this process is run: where its config is, whether something starts it again after it
//! exits, and a way to ask it to exit so it does (used by the web app's Settings).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// The exit code of a deliberate restart. It is not zero: systemd's `Restart=on-failure` and the
/// Windows task's restart-on-failure only start Arr again after a failure (launchd's KeepAlive
/// restarts after any exit).
pub const RESTART_EXIT: i32 = 75;

static CONFIG: OnceLock<PathBuf> = OnceLock::new();
static SUPERVISED: AtomicBool = AtomicBool::new(false);

/// Remembers the `--config` this process was started with.
pub fn set_config_path(path: &Path) {
    let _ = CONFIG.set(std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
}

/// The config file `arr set` edits for this process: the one it was started with, else
/// `config.toml` in the state folder (where every installer puts it).
pub fn config_path(state_dir: &Path) -> PathBuf {
    CONFIG
        .get()
        .cloned()
        .unwrap_or_else(|| state_dir.join("config.toml"))
}

/// Says that a service manager starts this process again when it exits (the Windows
/// background program, which only a task starts).
pub fn mark_supervised() {
    SUPERVISED.store(true, Ordering::Relaxed);
}

/// Whether a service manager will start Arr again after it exits: systemd sets
/// `INVOCATION_ID` for the services it runs, and launchd names the job in `XPC_SERVICE_NAME`.
/// Arr started by hand from a terminal has neither, and would simply stop.
pub fn supervised() -> bool {
    SUPERVISED.load(Ordering::Relaxed)
        || std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty())
        || std::env::var("XPC_SERVICE_NAME").is_ok_and(|v| v == "io.github.newtonharry.arr")
}

/// The runtime every Arr program runs on: a few workers (handlers are short; database and file
/// work runs on blocking threads) and a bounded blocking pool, so a burst of requests queues
/// instead of starting hundreds of threads.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(32)
        .enable_all()
        .build()
}

/// Caps glibc's malloc at two arenas. By default each thread may get its own arena, and each
/// keeps the memory its thread freed. Call first in `main`, before any thread starts.
pub fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only changes allocator settings; it is called before other threads exist.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

/// Gives memory freed by glibc's malloc back to the system (after the catalog index is
/// replaced, say, whose old copy is large).
pub fn trim_malloc() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes no pointers and is thread-safe; it only releases free memory.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// A request to end the process so the service manager starts it again.
#[derive(Default)]
pub struct Restart {
    requested: AtomicBool,
    notify: Notify,
}

impl Restart {
    pub fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Completes once a restart is requested.
    pub async fn wait(&self) {
        while !self.requested() {
            self.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_wakes_the_waiter_even_if_it_came_first() {
        let r = Restart::default();
        assert!(!r.requested());
        r.request();
        tokio::time::timeout(std::time::Duration::from_secs(1), r.wait())
            .await
            .expect("woken");
        assert!(r.requested());
    }

    #[test]
    fn the_config_defaults_to_the_state_folder() {
        // Other tests never call set_config_path, so the default is what a test server sees.
        if CONFIG.get().is_none() {
            assert_eq!(
                config_path(Path::new("/s")),
                PathBuf::from("/s/config.toml")
            );
        }
    }
}
