//! `ProjectController`: the document lifecycle at the file layer — open, save, save-as, close, the
//! unsaved-changes question, the recent-project note and the external-change watch.
//!
//! The session half of every operation lives in `compositor_rs_session` (`EditorSession::prepare_save`,
//! `save_project`, `open_project`, `reload_project_from`, `begin_project_operation`,
//! `end_project_operation`, `project_snapshot`); this module drives it and owns what Swift's
//! controller owned besides: where a save goes, the question asked before unsaved work is replaced,
//! the digests, the watcher and the incoming-file queue. The file work itself goes through
//! [`SessionHost`], which the app crate implements over this crate's store and exporters.
//! The controller owns no session: every operation takes the live [`EditorSession`] the UI's entity
//! holds, as Swift's controller held a reference to its class.
//!
//! # Async → sync
//! * `save`/`open`/`close`/`newCanvas` awaited an `NSAlert` or `NSSavePanel`. The port asks
//!   [`ProjectPrompter`] and acts on the answer in the same order; the session's busy flag is held
//!   across the part Swift held it for.
//! * `write` ran on a background `Task` so editing went on while the package was written, and
//!   `finishWriting` awaited it. `ProjectStore` writes synchronously in the port, so the write blocks
//!   the caller and `finish_writing` has nothing to wait for; `save_in_progress` reports the same
//!   state while [`Self::write`] runs.
//! * `checkExternalChange`'s `Task.detached` digest read runs inline, and `scheduleRecheck`'s sleeping
//!   task becomes [`Self::poll_recheck`], which the UI calls on its timer; `checking`, `pending` and
//!   `recheck_attempt` read exactly as they did.
//! * The watcher's handler ran on the main actor. In the port it runs on the watcher's thread and only
//!   raises [`ExternalChangeState::pending`], which the UI observes, so no session state is touched off
//!   the calling thread.
//! * `saveGeneration` and the re-load it guarded are gone: `EditorSession::open_project` loads the
//!   package after the unsaved-changes question, so a save made while the question was up is what gets
//!   opened, without the port comparing URLs.
//! * `startAccessingSecurityScopedResource` has no equivalent and is dropped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use compositor_rs_core::Id;
use compositor_rs_core::geom::Point;
use compositor_rs_session::EditorSession;
use compositor_rs_session::projects::SessionHost;

use crate::project_digest::ProjectDigest;
use crate::project_store::{ProjectError, ProjectSnapshot};
use crate::project_watcher::ProjectWatcher;
use crate::recent_projects::RecentProjects;

/// The unsaved-changes alert's answer (`confirmReplacement`'s three buttons).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// `alertFirstButtonReturn`: "Save".
    Save,
    /// `alertSecondButtonReturn`: "Cancel".
    Cancel,
    /// `alertThirdButtonReturn`: "Don’t Save".
    Discard,
}

/// What the controller asks of the UI. Swift awaited an `NSAlert` or an `NSSavePanel`; the port asks
/// the caller and acts on the answer, so the questions are asked in the same order with the same
/// wording (see [`save_changes_alert`] and [`external_change_alert`]).
pub trait ProjectPrompter {
    /// `NSSavePanel` for Save/Save As, with the file name the panel showed. `None` cancels.
    fn choose_project_destination(&mut self, suggested_name: &str, title: &str) -> Option<PathBuf>;
    /// `NSOpenPanel` for Open with no file supplied. `None` cancels.
    fn choose_project_to_open(&mut self) -> Option<PathBuf>;
    /// The unsaved-changes alert (`confirmReplacement`), whose wording is [`save_changes_alert`].
    fn confirm_replacement(&mut self, project_name: &str) -> Replacement;
    /// `askToRevert(in:)`: true for "Revert", false for "Keep Mine".
    fn ask_to_revert(&mut self, project_name: &str) -> bool;
    /// `showError(_:error:)`: the title, and the error's message.
    fn show_error(&mut self, title: &str, message: &str);
}

/// The unsaved-changes alert's title and message.
pub fn save_changes_alert(project_name: &str) -> (String, String) {
    (
        format!("Save changes to {project_name}?"),
        "Your changes will be lost if you don’t save them.".to_string(),
    )
}

/// The external-change alert's title and message.
pub fn external_change_alert(project_name: &str) -> (String, String) {
    (
        format!("“{project_name}” was changed on disk."),
        "Another app changed this project. You can revert to the version on disk, losing your unsaved changes, or keep what you have.".to_string(),
    )
}

/// The controller's bookkeeping for the watch: what the package looked like when last read or written,
/// and whether a check is running, waiting, or deferred because a save of our own is in flight.
pub struct ExternalChangeState {
    pub watcher: Option<ProjectWatcher>,
    pub known_digest: Option<ProjectDigest>,
    pub checking: bool,
    /// Raised by the watcher's thread and by [`ProjectController::schedule_recheck`]; the setter is
    /// shared because the watcher cannot borrow the controller.
    pub pending: Arc<AtomicBool>,
    pub saving: bool,
    /// The `Task.sleep` `scheduleRecheck` awaited. The port records when the retry is due and
    /// [`ProjectController::poll_recheck`] runs it.
    pub recheck_at: Option<Instant>,
    pub recheck_attempt: usize,
    /// Reloads performed because the package changed on disk. Read by tests.
    pub reload_count: usize,
}

impl Default for ExternalChangeState {
    fn default() -> Self {
        Self {
            watcher: None,
            known_digest: None,
            checking: false,
            pending: Arc::new(AtomicBool::new(false)),
            saving: false,
            recheck_at: None,
            recheck_attempt: 0,
            reload_count: 0,
        }
    }
}

/// One queued `receive` (`Incoming`). Swift's continuation is not needed: the drain is synchronous.
struct Incoming {
    files: Vec<PathBuf>,
    point: Option<Point>,
}

/// Swift's controller: the file-layer state it owned. The session it drives is not owned here — every
/// operation takes the live one, as Swift's controller held a reference to its class.
pub struct ProjectController {
    pub external_changes: ExternalChangeState,
    /// `workspace.isManaging`: another tab has an operation up, so this one waits. The app sets it.
    pub managing: bool,
    /// `workspace.current.controller === self`: the tab is in front. The app sets it.
    pub frontmost: bool,
    /// True only while [`Self::write`] runs; saves are synchronous in the port.
    save_in_progress: bool,
    incoming: Vec<Incoming>,
    processing: bool,
}

impl ProjectController {
    pub fn new() -> Self {
        Self {
            external_changes: ExternalChangeState::default(),
            managing: false,
            frontmost: true,
            save_in_progress: false,
            incoming: Vec::new(),
            processing: false,
        }
    }

    /// `canStart`: the session can begin, and no other tab is managing the project.
    pub fn can_start(&self, session: &EditorSession) -> bool {
        session.can_start_project_operation() && !self.managing
    }

    /// `begin()`: refuses when another operation is running, ends the crop and any transform edit, and
    /// takes the busy flag.
    fn begin(&mut self, session: &mut EditorSession) -> bool {
        session.begin_project_operation()
    }

    /// The project's name for the alerts (`session.projectURL?.lastPathComponent ?? "Untitled"`).
    pub fn project_name(&self, session: &EditorSession) -> String {
        session
            .project_url
            .as_ref()
            .and_then(|url| url.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string())
    }

    /// The save still writing, if any. Close, quit and replacing the document wait for it.
    pub fn finish_writing(&self) {
        // Swift awaited the background task here. The port's writes are synchronous, so by the time the
        // caller gets control there is never one in flight.
    }

    /// `writing != nil`: whether a write is running. True only while [`Self::write`] is on the stack.
    pub fn save_in_progress(&self) -> bool {
        self.save_in_progress
    }

    /// File > Save / Save As. A `Save As` always asks for a destination; a `Save` asks only when the
    /// project has never been saved.
    pub fn save(
        &mut self,
        session: &mut EditorSession,
        host: &dyn SessionHost,
        prompter: &mut dyn ProjectPrompter,
        as_new: bool,
    ) -> bool {
        if session.document.is_none() {
            return false;
        }
        // Another save still writing finishes first; then this one saves whatever has changed since.
        self.finish_writing();
        if !self.begin(session) {
            return false;
        }
        let prepared = self.prepare_save(session, prompter, as_new);
        // Only the snapshot (and the Save panel) holds the tools. The document is captured, so editing
        // can go on while the package is written, as in Photoshop.
        session.end_project_operation();
        let Some((snapshot, destination, revision)) = prepared else { return false };
        self.write(session, host, prompter, &snapshot, &destination, revision)
    }

    /// The document as it is now, and where it goes: asks with the Save panel when it has no file yet
    /// (or Save As), and captures the revision a finished save counts as saved.
    fn prepare_save(
        &mut self,
        session: &mut EditorSession,
        prompter: &mut dyn ProjectPrompter,
        as_new: bool,
    ) -> Option<(ProjectSnapshot, PathBuf, Id)> {
        let (snapshot, revision) = session.prepare_save()?;
        let mut destination = if as_new { None } else { session.project_url.clone() };
        if destination.is_none() {
            let suggested = session
                .project_url
                .as_ref()
                .and_then(|url| url.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled.comp".to_string());
            let title = if as_new { "Save Project As" } else { "Save Project" };
            destination = prompter.choose_project_destination(&suggested, title);
        }
        let destination = destination?;
        Some((snapshot, destination, revision))
    }

    /// Writes a captured document. Only that captured version counts as saved.
    fn write(
        &mut self,
        session: &mut EditorSession,
        host: &dyn SessionHost,
        prompter: &mut dyn ProjectPrompter,
        snapshot: &ProjectSnapshot,
        to: &Path,
        revision: Id,
    ) -> bool {
        self.save_in_progress = true;
        // Our own save changes the package too; the watch ignores events until the saved bytes are remembered.
        self.external_changes.saving = true;
        let result = session.save_project(snapshot, to, revision, host);
        self.external_changes.saving = false;
        self.save_in_progress = false;
        match result {
            Ok(()) => {
                RecentProjects::shared().lock().note(to.to_path_buf());
                self.remember_project_digest(to);
                self.watch_project(to);
                true
            }
            Err(message) => {
                prompter.show_error("Couldn’t save the project", &message);
                false
            }
        }
    }

    /// `saveCurrent`: Save with no changes to the destination choice.
    fn save_current(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) -> bool {
        if session.document.is_none() {
            return true;
        }
        let Some((snapshot, destination, revision)) = self.prepare_save(session, prompter, false) else { return false };
        self.write(session, host, prompter, &snapshot, &destination, revision)
    }

    /// File > Open. With no file supplied the Open panel asks for one.
    ///
    /// The session's [`EditorSession::open_project`] takes the busy flag for the part that validates and
    /// installs the package; the answer to the unsaved-changes question and the Open panel itself ask
    /// without holding it, which is the one place this port's busy window is narrower than the Swift's.
    pub fn open(
        &mut self,
        session: &mut EditorSession,
        host: &dyn SessionHost,
        prompter: &mut dyn ProjectPrompter,
        supplied_url: Option<&Path>,
    ) -> bool {
        let mut source = supplied_url.map(Path::to_path_buf);
        if source.is_none() {
            source = prompter.choose_project_to_open();
        }
        let Some(source) = source else { return false };
        // A recent project deleted in Finder: name the project, not the manifest inside it the load would miss.
        if !host.file_exists(&source) {
            RecentProjects::shared().lock().refresh();
        }
        if !self.confirm_replacement(session, host, prompter) {
            return false;
        }
        match session.open_project(&source, host) {
            Ok(true) => {
                RecentProjects::shared().lock().note(source.clone());
                self.remember_project_digest(&source);
                self.watch_project(&source);
                true
            }
            Ok(false) => false,
            Err(message) => {
                prompter.show_error("Couldn’t open the project", &message);
                false
            }
        }
    }

    /// File > New. Swift handed a workspace its tab new-canvas command; the port's caller does that and
    /// calls this only for the plain, window-local case.
    pub fn new_canvas(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) -> bool {
        if !self.begin(session) {
            return false;
        }
        let proceed = self.confirm_replacement(session, host, prompter);
        session.end_project_operation();
        if proceed {
            session.clear_project();
            self.stop_watching_project();
        }
        proceed
    }

    /// The window's close button. Returns whether the project was closed; the caller closes its window.
    pub fn close(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) -> bool {
        if !self.begin(session) {
            return false;
        }
        let proceed = self.confirm_replacement(session, host, prompter);
        session.end_project_operation();
        if proceed {
            session.clear_project();
            self.stop_watching_project();
        }
        proceed
    }

    /// Quit: the last chance to save, for every open project in turn.
    pub fn confirm_quit(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) -> bool {
        if !self.begin(session) {
            return false;
        }
        let proceed = self.confirm_replacement(session, host, prompter);
        session.end_project_operation();
        proceed
    }

    /// The unsaved-changes question asked before anything replaces the live document.
    fn confirm_replacement(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) -> bool {
        // A save still writing finishes before the project can be closed or replaced, so its file is never cut short.
        self.finish_writing();
        if !session.is_modified() || session.document.is_none() {
            return true;
        }
        match prompter.confirm_replacement(&self.project_name(session)) {
            Replacement::Save => self.save_current(session, host, prompter),
            Replacement::Cancel => false,
            Replacement::Discard => true,
        }
    }

    // MARK: - External changes

    /// Starts (or restarts) watching the project the session has open. Called after a successful open
    /// and after every save, so the digest of the package on disk is always the one we last read or wrote.
    pub fn watch_project(&mut self, url: &Path) {
        if self.external_changes.watcher.as_ref().map(|watcher| watcher.url()) == Some(url) {
            return;
        }
        let pending = Arc::clone(&self.external_changes.pending);
        self.external_changes.watcher = Some(ProjectWatcher::new(url.to_path_buf(), move || {
            pending.store(true, Ordering::SeqCst);
        }));
    }

    pub fn stop_watching_project(&mut self) {
        self.external_changes.watcher = None;
        self.external_changes.known_digest = None;
        self.external_changes.recheck_at = None;
        self.external_changes.pending.store(false, Ordering::SeqCst);
    }

    /// Remembers the package as it is now, so the next event compares against it.
    pub fn remember_project_digest(&mut self, url: &Path) {
        self.external_changes.known_digest = ProjectDigest::compute(url).ok();
    }

    /// The tab came to the front: a change that arrived while it had unsaved work and was hidden can be
    /// asked about now.
    pub fn resume_external_change_check(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) {
        if self.external_changes.pending.load(Ordering::SeqCst) {
            self.note_external_change(session, host, prompter);
        }
    }

    /// `noteExternalChange()`: records that the package may have changed and checks it, unless a check is
    /// already running (which re-reads the flag itself).
    pub fn note_external_change(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) {
        self.external_changes.pending.store(true, Ordering::SeqCst);
        if self.external_changes.checking {
            return;
        }
        self.check_external_change(session, host, prompter);
    }

    /// Compares the package on disk against the known digest and, for a real change, adopts it or asks.
    pub fn check_external_change(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) {
        self.external_changes.checking = true;
        while self.external_changes.pending.swap(false, Ordering::SeqCst) {
            if session.project_url.is_none() || session.document.is_none() || self.external_changes.saving {
                self.external_changes.checking = false;
                return;
            }
            let url = session.project_url.clone().expect("guarded above");
            // Compare content, not modification dates: sync clients touch metadata without changing anything.
            let Some(digest) = ProjectDigest::compute(&url).ok() else { continue };
            if Some(&digest) == self.external_changes.known_digest.as_ref() {
                continue;
            }
            // Wait for an edit in progress to finish rather than pulling the document out from under it.
            if !self.can_start(session) || session.transform_edit.is_some() {
                self.schedule_recheck();
                self.external_changes.checking = false;
                return;
            }
            if session.is_modified() {
                if !self.frontmost {
                    self.external_changes.pending.store(true, Ordering::SeqCst);
                    self.external_changes.checking = false;
                    return;
                }
                if !prompter.ask_to_revert(&self.project_name(session)) {
                    self.external_changes.known_digest = Some(digest);
                    continue;
                }
            }
            self.reload_from_disk(session, host, &url);
        }
        self.external_changes.checking = false;
    }

    /// Retries while the session is busy, backing off so a long operation is not polled. The retry runs
    /// from [`Self::poll_recheck`] once its delay is up.
    pub fn schedule_recheck(&mut self) {
        self.external_changes.pending.store(true, Ordering::SeqCst);
        let attempt = self.external_changes.recheck_attempt;
        self.external_changes.recheck_attempt = (attempt + 1).min(7);
        self.external_changes.recheck_at = Some(Instant::now() + recheck_delay(attempt));
    }

    /// `scheduleRecheck`'s awaited `Task.sleep`: does nothing until the recorded delay is up, then asks
    /// for the check the sleeping task would have started.
    pub fn poll_recheck(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) {
        let Some(at) = self.external_changes.recheck_at else { return };
        if Instant::now() < at {
            return;
        }
        self.external_changes.recheck_at = None;
        self.note_external_change(session, host, prompter);
    }

    fn reload_from_disk(&mut self, session: &mut EditorSession, host: &dyn SessionHost, url: &Path) {
        self.external_changes.recheck_attempt = 0;
        session.is_project_busy = true;
        let loaded = session.reload_project_from(url, host);
        session.is_project_busy = false;
        if !loaded {
            return;
        }
        // Remember the package as loaded, not as first seen: it may have changed again while a sheet was up.
        self.remember_project_digest(url);
        self.external_changes.reload_count += 1;
    }

    // MARK: - Incoming files

    /// The files dropped on the window or opened by Finder, in order. A queued request waits for the
    /// session to be free; the UI drains it with [`Self::drain_incoming`], as it drains imports.
    ///
    /// Swift routed a tabbed window's drops to its workspace; the port's caller does that and calls this
    /// for the window-local case.
    pub fn receive(
        &mut self,
        session: &mut EditorSession,
        host: &dyn SessionHost,
        prompter: &mut dyn ProjectPrompter,
        urls: Vec<PathBuf>,
        at: Option<Point>,
    ) {
        if urls.is_empty() {
            return;
        }
        self.incoming.push(Incoming { files: urls, point: at });
        if !self.processing {
            self.processing = true;
            self.drain_incoming(session, host, prompter);
        }
    }

    /// Drains the queue. Stops (and leaves `processing` set) while a project operation is running, so
    /// the UI calls again once the session frees; Swift's request task simply resumed.
    pub fn drain_incoming(&mut self, session: &mut EditorSession, host: &dyn SessionHost, prompter: &mut dyn ProjectPrompter) {
        while !self.incoming.is_empty() {
            if !session.can_start_project_operation() {
                return;
            }
            let request = self.incoming.remove(0);
            let projects: Vec<PathBuf> =
                request.files.iter().filter(|url| is_project_package(url)).cloned().collect();
            if projects.len() > 1 {
                prompter.show_error("Open one project at a time", &ProjectError::Invalid.to_string());
            } else {
                let mut proceed = true;
                if let Some(project) = projects.first() {
                    proceed = self.open(session, host, prompter, Some(project.as_path()));
                }
                if proceed {
                    let images: Vec<PathBuf> =
                        request.files.iter().filter(|url| !is_project_package(url)).cloned().collect();
                    session.import_images(&images, if projects.is_empty() { request.point } else { None }, host);
                }
            }
        }
        self.processing = false;
    }
}

/// `url.pathExtension.lowercased() == "comp"`: a `.comp` package opens as a project, anything else as
/// an image.
pub fn is_project_package(url: &Path) -> bool {
    url.extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase() == "comp").unwrap_or(false)
}

/// `Task.sleep(for: .milliseconds(250 * (1 << attempt)))`, capped at seven doublings.
fn recheck_delay(attempt: usize) -> Duration {
    Duration::from_millis(250 * (1 << attempt.min(7)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alerts_keep_the_swift_wording() {
        assert_eq!(
            save_changes_alert("Poster.comp"),
            ("Save changes to Poster.comp?".to_string(), "Your changes will be lost if you don’t save them.".to_string())
        );
        assert_eq!(
            external_change_alert("Poster.comp"),
            (
                "“Poster.comp” was changed on disk.".to_string(),
                "Another app changed this project. You can revert to the version on disk, losing your unsaved changes, or keep what you have.".to_string()
            )
        );
    }

    #[test]
    fn only_a_comp_extension_opens_as_a_project() {
        assert!(is_project_package(Path::new("Poster.comp")));
        assert!(is_project_package(Path::new("Poster.COMP")));
        assert!(!is_project_package(Path::new("Poster.comp.bak")));
        assert!(!is_project_package(Path::new("photo.png")));
        assert!(!is_project_package(Path::new("Poster")));
    }

    #[test]
    fn the_recheck_backoff_doubles_and_stops_at_seven_attempts() {
        let delays: Vec<u64> = (0..10).map(|attempt| recheck_delay(attempt).as_millis() as u64).collect();
        assert_eq!(delays, [250, 500, 1000, 2000, 4000, 8000, 16000, 32000, 32000, 32000]);
    }
}
