//! Tells its owner when a project package changes on disk, whoever changed it: another app, an
//! agent, a sync client, a git checkout. It listens to the kernel's file system events for the
//! package folder, its manifest and its images folder, so there is no polling and no dependency on
//! the writer using file coordination (which a file presenter needs and most other writers skip).
//! Events are coalesced, and the handler runs on the watcher's own worker thread — the app hops to
//! the UI thread from there.
//!
//! A package is replaced atomically by renaming a sibling over it, which retires the handles being
//! watched; every event therefore re-arms the watch by path, so the new package is watched after the
//! swap.

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long to wait after the last event before reporting, so a save that touches several files
/// reports once.
pub const COALESCING: Duration = Duration::from_millis(300);

/// How long between re-arm attempts while the package is mid-replacement.
const REARM_INTERVAL: Duration = Duration::from_millis(100);

/// How many times the watch is re-armed before giving up (20 × 100 ms), as the Swift retried.
const REARM_ATTEMPTS: usize = 20;

/// The longest a worker sleeps without checking whether it has been stopped.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Watches one project package and reports changes to it.
pub struct ProjectWatcher {
    url: PathBuf,
    inner: Option<Arc<Inner>>,
    thread: Option<JoinHandle<()>>,
}

struct Inner {
    watcher: Mutex<RecommendedWatcher>,
    stopped: AtomicBool,
    rearming: AtomicBool,
    handler: Box<dyn Fn() + Send + Sync>,
}

impl ProjectWatcher {
    /// Starts watching `url`. A backend that cannot be started leaves the watcher inert, as the
    /// Swift's `open` failures left it with no dispatch sources.
    pub fn new(url: impl Into<PathBuf>, onChange: impl Fn() + Send + Sync + 'static) -> ProjectWatcher {
        let url = url.into();
        let paths = watched_paths(&url);
        let (events_tx, events_rx) = mpsc::channel::<()>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            if event.is_ok() {
                let _ = events_tx.send(());
            }
        });
        let Ok(mut watcher) = watcher else {
            return ProjectWatcher { url, inner: None, thread: None };
        };
        for path in &paths {
            let _ = watcher.watch(path, RecursiveMode::NonRecursive);
        }
        let inner = Arc::new(Inner {
            watcher: Mutex::new(watcher),
            stopped: AtomicBool::new(false),
            rearming: AtomicBool::new(false),
            handler: Box::new(onChange),
        });
        let thread = {
            let inner = Arc::clone(&inner);
            let paths = paths.clone();
            thread::Builder::new()
                .name("compositor-project-watcher".to_string())
                .spawn(move || run(inner, paths, events_rx))
        };
        ProjectWatcher { url, inner: Some(inner), thread: thread.ok() }
    }

    /// The package being watched.
    pub fn url(&self) -> &Path {
        &self.url
    }

    /// Stops watching and waits for the worker to wind down. Idempotent.
    pub fn stop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.stopped.store(true, Ordering::SeqCst);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The package folder, its manifest and its images folder.
fn watched_paths(url: &Path) -> Vec<PathBuf> {
    vec![url.to_path_buf(), url.join("manifest.json"), url.join("images")]
}

fn run(inner: Arc<Inner>, paths: Vec<PathBuf>, events: Receiver<()>) {
    loop {
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        match events.recv_timeout(STOP_POLL) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Coalesce: report 300 ms after the last event, so a save that touches several files
        // reports once. A later event pushes the deadline out.
        let mut deadline = Instant::now() + COALESCING;
        loop {
            if inner.stopped.load(Ordering::SeqCst) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = (deadline - now).min(STOP_POLL);
            match events.recv_timeout(wait) {
                Ok(()) => deadline = Instant::now() + COALESCING,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        // Re-arm by path once the writer has finished swapping files, retrying briefly while the
        // package is mid-replacement. It runs beside the report — as the Swift's re-arm task did —
        // so the report is not held back by the retries.
        if !inner.rearming.swap(true, Ordering::SeqCst) {
            let rearm_inner = Arc::clone(&inner);
            let rearm_paths = paths.clone();
            let _ = thread::Builder::new()
                .name("compositor-project-watcher-rearm".to_string())
                .spawn(move || {
                    rearm(&rearm_inner, &rearm_paths);
                    rearm_inner.rearming.store(false, Ordering::SeqCst);
                });
        }
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        (inner.handler)();
    }
}

fn rearm(inner: &Inner, paths: &[PathBuf]) {
    for _ in 0..REARM_ATTEMPTS {
        thread::sleep(REARM_INTERVAL);
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut watcher = inner.watcher.lock();
        let mut watched = 0;
        for path in paths {
            let _ = watcher.unwatch(path);
            if watcher.watch(path, RecursiveMode::NonRecursive).is_ok() {
                watched += 1;
            }
        }
        drop(watcher);
        if watched == paths.len() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("compositor-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("images")).unwrap();
        std::fs::write(directory.join("manifest.json"), b"{}").unwrap();
        directory
    }

    #[test]
    fn reports_a_change_and_keeps_watching() {
        let directory = package("change");
        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher = ProjectWatcher::new(directory.clone(), move || {
            let _ = tx.send(());
        });

        std::fs::write(directory.join("manifest.json"), b"{ }").unwrap();
        rx.recv_timeout(Duration::from_secs(10)).expect("the manifest write is reported once");

        // The event re-armed the watch, so the next write is reported too.
        std::fs::write(directory.join("manifest.json"), b"{  }").unwrap();
        rx.recv_timeout(Duration::from_secs(10)).expect("the watcher survives the first report");

        watcher.stop();
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn coalesces_a_burst_into_one_report_after_the_coalescing_window() {
        let directory = package("coalesce");
        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher = ProjectWatcher::new(directory.clone(), move || {
            let _ = tx.send(());
        });

        let start = Instant::now();
        std::fs::write(directory.join("manifest.json"), b"{}").unwrap();
        std::fs::write(directory.join("images").join("a.png"), b"a").unwrap();
        std::fs::write(directory.join("manifest.json"), b"[ ]").unwrap();
        let last_write = Instant::now();
        rx.recv_timeout(Duration::from_secs(10)).expect("the burst is reported once");
        // The report waits the coalescing window after the *last* event, not the first, so a save
        // that touches several files reports once.
        let after_last = last_write.elapsed();
        assert!(after_last >= COALESCING, "reported before the window after the last write: {after_last:?}");
        let total = start.elapsed();
        assert!(total < COALESCING + Duration::from_secs(5), "reported late: {total:?}");

        // The burst re-armed the watch, so the next change is still reported.
        std::fs::write(directory.join("manifest.json"), b"[  ]").unwrap();
        rx.recv_timeout(Duration::from_secs(10)).expect("the next change is reported");

        watcher.stop();
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_stopped_watcher_reports_nothing() {
        let directory = package("stopped");
        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher = ProjectWatcher::new(directory.clone(), move || {
            let _ = tx.send(());
        });
        watcher.stop();
        std::fs::write(directory.join("manifest.json"), b"[ ]").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
