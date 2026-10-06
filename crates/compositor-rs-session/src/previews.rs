//! The shared preview-job infrastructure: request, coalesce, complete.
//!
//! The Swift session drove every expensive preview from a `Task { … }` block and kept the result in an
//! `@ObservationIgnored` cache — `EffectsPreviewCache` and its single `DispatchQueue` worker, the
//! distort and mask-distort caches, the levels histogram task, the filter/levels/hue-saturation
//! preview tasks. The port keeps that model exactly: one worker per family of previews, the request for
//! a key superseding the one before it, the last finished result shown while the next one renders, and
//! a small per-key history so undo/redo does not blink the preview off.
//!
//! What changes is only how the result gets back to the UI: Swift hopped to the main actor and called a
//! completion closure, the port records the key in a results queue the UI drains with
//! [`PreviewJobs::take_ready`]. The worker is a plain `std::thread`, as the Swift cache used a single
//! `DispatchQueue`; the job body itself is free to fan out over `rayon` inside `compositor-rs-pixels`.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

/// How many finished results of a key are remembered, newest last (`EffectsPreviewCache.recentPerLayer`).
pub const RECENT_PER_KEY: usize = 3;

/// A key identifying a preview. Two requests with the same key are the same preview, so the later one
/// supersedes the earlier one; a key is also what the UI asks for a result by.
pub trait PreviewKey: Clone + Eq + Hash + Send + 'static {}
impl<T: Clone + Eq + Hash + Send + 'static> PreviewKey for T {}

/// Cancellation for one job, handed to the job body (`EffectsPreviewCache.Request.isCancelled`).
///
/// A running job cannot be interrupted, only told to stop; the worker checks it before starting and
/// again before publishing, exactly as the Swift worker checks `request.isCancelled`.
#[derive(Clone, Default)]
pub struct PreviewCancel {
    flag: Arc<AtomicBool>,
}

impl PreviewCancel {
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

type Work<V> = Box<dyn FnOnce(&PreviewCancel) -> Option<V> + Send + 'static>;

struct Task<K, V> {
    key: K,
    generation: u64,
    cancel: PreviewCancel,
    work: Work<V>,
}

struct Entry<V> {
    /// The request currently rendering; a result that lands under an older generation is dropped.
    generation: u64,
    cancel: PreviewCancel,
    /// The result on show for this key, kept while the next request renders.
    result: Option<V>,
}

struct State<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// Keys whose result changed since the UI last drained them, in completion order.
    ready: VecDeque<K>,
    /// A few recent finished results per key, newest last (undo/redo puts earlier pixels back).
    recent: HashMap<K, VecDeque<V>>,
    /// Results handed in from elsewhere (`seed`), shown until a fresh preview is ready.
    seeds: HashMap<K, V>,
    generation: u64,
    queued: usize,
    rendering: bool,
}

struct Shared<K, V> {
    state: Mutex<State<K, V>>,
    /// Signalled when a job finishes and when the queue empties (`saturate`).
    idle: Condvar,
}

/// A family of previews rendered off the UI thread and polled by it.
///
/// `V` must be cheap to clone — every use holds an image or another `Arc`-backed raster.
pub struct PreviewJobs<K, V> {
    shared: Arc<Shared<K, V>>,
    /// Owned here, not by the worker: dropping the jobs ends the worker's loop (`Drop`).
    sender: Mutex<Option<Sender<Task<K, V>>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl<K: PreviewKey, V: Clone + Send + 'static> Default for PreviewJobs<K, V> {
    fn default() -> Self {
        Self::new("compositor-preview")
    }
}

impl<K: PreviewKey, V: Clone + Send + 'static> PreviewJobs<K, V> {
    /// Starts the worker thread for this family of previews (`EffectsPreviewCache.worker`).
    pub fn new(name: &str) -> Self {
        let (sender, receiver) = mpsc::channel::<Task<K, V>>();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                entries: HashMap::new(),
                ready: VecDeque::new(),
                recent: HashMap::new(),
                seeds: HashMap::new(),
                generation: 0,
                queued: 0,
                rendering: false,
            }),
            idle: Condvar::new(),
        });
        let worker = std::thread::Builder::new()
            .name(name.to_string())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run_worker(shared, receiver)
            })
            .ok();
        Self { shared, sender: Mutex::new(Some(sender)), worker: Mutex::new(worker) }
    }

    /// Asks for `key` to be rendered, cancelling the request before it for the same key
    /// (`EffectsPreviewCache.preview(for:…)`). The last finished result stays on show until the new one
    /// lands; a job that supersedes a *running* one cannot stop it, only take its place when it finishes.
    ///
    /// The job runs on this family's worker; `work` returns `None` when the render failed, which clears
    /// the key's result exactly as the Swift cache cleared it.
    pub fn request(&self, key: K, work: impl FnOnce(&PreviewCancel) -> Option<V> + Send + 'static) -> u64 {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        // The result of the request being replaced is kept: effects stay visible during a slider drag.
        let previous = state.entries.get(&key).and_then(|entry| entry.result.clone());
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.cancel.cancel();
        }
        state.generation += 1;
        let generation = state.generation;
        let cancel = PreviewCancel::default();
        state.entries.insert(key.clone(), Entry { generation, cancel: cancel.clone(), result: previous });
        state.queued += 1;
        let task = Task { key, generation, cancel, work: Box::new(work) };
        let sender = self.sender.lock().unwrap_or_else(|err| err.into_inner());
        let sent = sender.as_ref().is_some_and(|sender| sender.send(task).is_ok());
        if !sent {
            // The worker thread is gone (dropped, or the process is shutting down): nothing will render.
            state.queued -= 1;
        }
        generation
    }

    /// Shows `value` for `key` until a fresh preview is ready (`EffectsPreviewCache.seed(_:image:placement:)`).
    /// Whatever was rendering is for the pixels this replaces, so it is cancelled.
    pub fn seed(&self, key: K, value: V) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.cancel.cancel();
            entry.result = Some(value.clone());
        }
        state.seeds.insert(key, value);
    }

    /// What is on show for `key`: the finished preview, or the seed standing in for it while the worker
    /// renders. `None` when nothing is rendered and nothing is seeded.
    pub fn result(&self, key: &K) -> Option<V> {
        let state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.entries.get(key).and_then(|entry| entry.result.clone()).or_else(|| state.seeds.get(key).cloned())
    }

    /// True while a request for `key` is queued or rendering (`EffectsPreviewCache` entries without a
    /// result yet).
    pub fn is_rendering(&self, key: &K) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.entries.get(key).is_some_and(|entry| entry.result.is_none() || state.queued > 0)
    }

    /// The last remembered result for `key` the predicate accepts, newest first — undo and redo put a
    /// layer's earlier pixels back, and the effects for them are taken from here instead of blinking
    /// off while they are rendered again (`EffectsPreviewCache.recent`).
    pub fn remembered(&self, key: &K, mut matches: impl FnMut(&V) -> bool) -> Option<V> {
        let state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.recent.get(key)?.iter().rev().find(|value| matches(value)).cloned()
    }

    /// Takes the keys whose result changed since the last drain, for the UI to mark dirty
    /// (`EffectsPreviewCache`'s completion closure, without hopping actors).
    pub fn take_ready(&self) -> Vec<K> {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.ready.drain(..).collect()
    }

    /// Stops the request for `key`; its last result stays on show.
    pub fn cancel(&self, key: &K) {
        let state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(entry) = state.entries.get(key) {
            entry.cancel.cancel();
        }
    }

    /// Stops the request for `key` and forgets its result and seed.
    pub fn remove(&self, key: &K) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(entry) = state.entries.remove(key) {
            entry.cancel.cancel();
        }
        state.seeds.remove(key);
        state.ready.retain(|ready| ready != key);
    }

    /// Keeps only the keys the predicate accepts, cancelling and dropping the rest
    /// (`EffectsPreviewCache.prepare(layers:)`).
    pub fn retain(&self, keep: impl Fn(&K) -> bool) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        let mut dropped = Vec::new();
        state.entries.retain(|key, entry| {
            if keep(key) {
                true
            } else {
                entry.cancel.cancel();
                dropped.push(key.clone());
                false
            }
        });
        state.seeds.retain(|key, _| keep(key));
        state.ready.retain(|key| keep(key));
        for key in dropped {
            let keep_recent = state.entries.contains_key(&key) || state.seeds.contains_key(&key) || keep(&key);
            if !keep_recent {
                state.recent.remove(&key);
            }
        }
    }

    /// Forgets every key: results, seeds, remembered previews and queued work.
    pub fn clear(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        for entry in state.entries.values() {
            entry.cancel.cancel();
        }
        state.entries.clear();
        state.seeds.clear();
        state.recent.clear();
        state.ready.clear();
    }

    /// Waits until every queued job has run and no job is rendering. Swift had no equivalent — the UI
    /// polled — but the port's tests and a window close need a definite point where work has stopped.
    pub fn saturate(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|err| err.into_inner());
        while state.queued > 0 || state.rendering {
            state = self.shared.idle.wait(state).unwrap_or_else(|err| err.into_inner());
        }
    }
}

impl<K, V> Drop for PreviewJobs<K, V> {
    fn drop(&mut self) {
        // Dropping the sender ends the worker loop; joining here keeps the thread from outliving the
        // session (the Swift cache's worker outlived its owner, which the port does not need).
        self.sender.lock().unwrap_or_else(|err| err.into_inner()).take();
        let worker = self.worker.lock().unwrap_or_else(|err| err.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

fn run_worker<K: PreviewKey, V: Clone + Send + 'static>(shared: Arc<Shared<K, V>>, receiver: Receiver<Task<K, V>>) {
    while let Ok(task) = receiver.recv() {
        {
            let mut state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
            state.queued -= 1;
            state.rendering = true;
        }
        let Task { key, generation, cancel, work } = task;
        if !cancel.is_cancelled() {
            let value = work(&cancel);
            if !cancel.is_cancelled() {
                let mut state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
                // The entry is still the same request: a newer one has its own generation.
                if state.entries.get(&key).is_some_and(|entry| entry.generation == generation) {
                    state.entries.insert(key.clone(), Entry { generation, cancel: cancel.clone(), result: value.clone() });
                    if let Some(value) = value {
                        state.seeds.remove(&key);
                        let recent = state.recent.entry(key.clone()).or_default();
                        recent.push_back(value);
                        while recent.len() > RECENT_PER_KEY {
                            recent.pop_front();
                        }
                    }
                    state.ready.push_back(key);
                }
            }
        }
        let mut state = shared.state.lock().unwrap_or_else(|err| err.into_inner());
        state.rendering = false;
        if state.queued == 0 {
            shared.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A job that hands back the value it was built with, so a test can tell generations apart.
    fn value_job(value: usize, beacon: &Arc<AtomicUsize>) -> impl FnOnce(&PreviewCancel) -> Option<usize> + Send + 'static {
        let beacon = Arc::clone(beacon);
        move |cancel: &PreviewCancel| {
            beacon.fetch_add(1, Ordering::SeqCst);
            if cancel.is_cancelled() {
                return None;
            }
            Some(value)
        }
    }

    #[test]
    fn a_finished_result_is_published_and_drained_once() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        let beacon = Arc::new(AtomicUsize::new(0));
        jobs.request(1, value_job(7, &beacon));
        jobs.saturate();
        assert_eq!(jobs.result(&1), Some(7));
        assert_eq!(jobs.take_ready(), vec![1]);
        assert!(jobs.take_ready().is_empty());
    }

    #[test]
    fn a_superseded_request_does_not_clobber_the_newer_result() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        let beacon = Arc::new(AtomicUsize::new(0));
        jobs.request(4, value_job(1, &beacon));
        // The second request replaces the first before the worker ever looks at it.
        let (release, held) = mpsc::channel::<()>();
        jobs.request(4, move |_| {
            held.recv().ok();
            Some(2)
        });
        release.send(()).expect("the worker waits on the second request");
        jobs.saturate();
        assert_eq!(jobs.result(&4), Some(2));
        assert_eq!(beacon.load(Ordering::SeqCst), 0, "the superseded request never ran");
    }

    #[test]
    fn a_seed_stands_in_until_the_preview_lands() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        let (release, held) = mpsc::channel::<()>();
        jobs.request(9, move |_| {
            held.recv().ok();
            Some(11)
        });
        // Seeding cancels whatever was rendering for the pixels it replaces, and stands in until the
        // next request lands (the canvas asks again on its next redraw).
        jobs.seed(9, 3);
        assert_eq!(jobs.result(&9), Some(3));
        release.send(()).expect("the worker holds the request");
        jobs.saturate();
        assert_eq!(jobs.result(&9), Some(3), "the cancelled render does not land over the seed");

        jobs.request(9, |_| Some(12));
        jobs.saturate();
        assert_eq!(jobs.result(&9), Some(12));
        assert_eq!(jobs.remembered(&9, |_| true), Some(12));
    }

    #[test]
    fn a_failed_job_clears_the_result_and_still_reports_completion() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        jobs.request(2, |_| Some(5));
        jobs.saturate();
        assert_eq!(jobs.take_ready(), vec![2]);
        jobs.request(2, |_| None);
        jobs.saturate();
        assert_eq!(jobs.result(&2), None, "a failed render clears the key's result");
        assert_eq!(jobs.take_ready(), vec![2], "the UI still hears about the attempt");

        // A seed outlives a failed render, exactly as `rendered(_:)` fell back to it in Swift.
        jobs.seed(2, 7);
        jobs.request(2, |_| None);
        jobs.saturate();
        assert_eq!(jobs.result(&2), Some(7));
    }

    #[test]
    fn retain_cancels_and_drops_every_other_key() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        let beacon = Arc::new(AtomicUsize::new(0));
        jobs.request(1, value_job(1, &beacon));
        jobs.request(2, value_job(2, &beacon));
        jobs.saturate();
        assert_eq!(jobs.take_ready().len(), 2);
        jobs.retain(|key| *key == 1);
        assert_eq!(jobs.result(&1), Some(1));
        assert_eq!(jobs.result(&2), None);
        assert!(jobs.remembered(&2, |_| true).is_none(), "the dropped key's history goes with it");
    }

    #[test]
    fn a_cancelled_request_leaves_its_previous_result_alone() {
        let jobs: PreviewJobs<u64, usize> = PreviewJobs::new("test-preview");
        jobs.request(6, |_| Some(8));
        jobs.saturate();
        assert_eq!(jobs.take_ready(), vec![6]);
        let (release, held) = mpsc::channel::<()>();
        jobs.request(6, move |_| {
            held.recv().ok();
            Some(9)
        });
        jobs.cancel(&6);
        release.send(()).expect("the worker holds the request");
        jobs.saturate();
        assert_eq!(jobs.result(&6), Some(8), "the last good preview stays on show");
        assert!(jobs.take_ready().is_empty(), "a cancelled result is not published");
    }
}
