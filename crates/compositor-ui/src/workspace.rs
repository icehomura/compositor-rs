//! The project workspace: one tab per open project, the tab strip above the editor, and the view the
//! window shows.
//!
//! Ported from `Document/ProjectWorkspace.swift`, `UI/ProjectTabs.swift` and `UI/ProjectTabLayout.swift`.
//! Each tab owns its session, its view and its own `ProjectController` — the file layer's state machine
//! every open, save, close and quit goes through — and the workspace drives those controllers, asking
//! the app's prompter for the panels and the unsaved-changes questions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use compositor_core::geom::Point;
use compositor_core::Id;
use compositor_io::project_controller::{is_project_package, ProjectController, ProjectPrompter};
use compositor_session::clipboard::Clipboard as _;
use compositor_session::projects::SessionHost;
use compositor_session::EditorSession;

use crate::content_view::ContentView;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Horizontal gap between adjacent tab pills, and between the overflow pill and the first tab after it
/// (`projectTabSpacing`).
pub const PROJECT_TAB_SPACING: f64 = 6.0;

/// A tab pill's height (`frame(height: 28)`), inside the strip's 34.
pub const TAB_PILL_HEIGHT: f32 = 28.0;
/// The strip's height (`frame(height: 34)`).
pub const TAB_STRIP_HEIGHT: f32 = 34.0;
/// A pill's leading and trailing padding, the close button and the gap after it (`projectTabPillWidth`).
pub const TAB_PILL_CHROME: f64 = 40.0;
/// A label's bounds (`projectTabLabelWidth`).
pub const TAB_LABEL_MAX: f64 = 155.0;
pub const TAB_LABEL_MIN: f64 = 35.0;
/// The width of the modified dot and the gap after it.
pub const TAB_DOT_WIDTH: f64 = 10.0;
/// The overflow pill's chrome: leading, the gap before the chevron, the chevron and trailing.
pub const OVERFLOW_PILL_CHROME: f64 = 36.0;

/// A pill's slot in the strip: `id` identifies the tab it belongs to (or is absent for the overflow pill).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectTabSlot {
    pub id: Id,
    pub x: f64,
    pub width: f64,
}

/// The overflow pill's own slot — no tab id, since it isn't one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectTabPillSlot {
    pub x: f64,
    pub width: f64,
}

/// What the strip should draw right now: the tabs that fit, the ones that don't (for the overflow
/// menu, in their real order), and the overflow pill's own slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectTabOverflow {
    pub visible: Vec<ProjectTabSlot>,
    pub hidden_ids: Vec<Id>,
    pub pill: Option<ProjectTabPillSlot>,
}

impl ProjectTabOverflow {
    pub fn content_width(&self) -> f64 {
        self.visible
            .last()
            .map(|slot| slot.x + slot.width)
            .or_else(|| self.pill.map(|pill| pill.x + pill.width))
            .unwrap_or(0.0)
    }
}

/// "N more tabs", singular for one (`projectTabOverflowLabel`).
pub fn project_tab_overflow_label(hidden_count: usize) -> String {
    if hidden_count == 1 {
        "1 more tab".to_string()
    } else {
        format!("{hidden_count} more tabs")
    }
}

/// Lays out the strip left to right: everything shows when it all fits. Otherwise tabs are dropped from
/// the front of `order` — oldest overflow first — until the rest fit alongside an overflow pill at
/// x = 0. The selected tab is never dropped: if it would be, it takes the first visible slot (right
/// after the pill) and whichever tab was there instead is hidden in its place, so the visible count
/// never changes.
pub fn project_tab_overflow(
    order: &[Id],
    widths: &dyn Fn(Id) -> f64,
    selected_id: Id,
    available_width: f64,
    pill_width: &dyn Fn(usize) -> f64,
) -> ProjectTabOverflow {
    if order.is_empty() {
        return ProjectTabOverflow::default();
    }
    let span = |ids: &[Id]| -> f64 {
        if ids.is_empty() {
            return 0.0;
        }
        ids.iter().map(|id| widths(*id)).sum::<f64>() + PROJECT_TAB_SPACING * (ids.len() - 1) as f64
    };
    let place = |ids: &[Id], start_x: f64| -> Vec<ProjectTabSlot> {
        let mut x = start_x;
        ids.iter()
            .map(|id| {
                let width = widths(*id);
                let slot = ProjectTabSlot {
                    id: *id,
                    x,
                    width,
                };
                x += width + PROJECT_TAB_SPACING;
                slot
            })
            .collect()
    };
    // Not yet measured, or everything already fits: no pill needed.
    if available_width <= 0.0 || span(order) <= available_width {
        return ProjectTabOverflow {
            visible: place(order, 0.0),
            hidden_ids: Vec::new(),
            pill: None,
        };
    }
    let mut shown = order.len();
    while shown > 1 {
        let hidden = order.len() - shown;
        let width = pill_width(hidden) + PROJECT_TAB_SPACING + span(&order[order.len() - shown..]);
        if width <= available_width {
            break;
        }
        shown -= 1;
    }
    let mut visible_ids: Vec<Id> = order[order.len() - shown..].to_vec();
    let mut hidden_ids: Vec<Id> = order[..order.len() - shown].to_vec();
    if let Some(bumped) = visible_ids.first().copied() {
        if hidden_ids.contains(&selected_id) {
            hidden_ids.retain(|id| *id != selected_id);
            hidden_ids.push(bumped);
            visible_ids[0] = selected_id;
            // The selected tab can be wider than the one it replaced (a longer title, set in semibold):
            // hide the tabs after it until the row fits again.
            while visible_ids.len() > 1
                && pill_width(hidden_ids.len()) + PROJECT_TAB_SPACING + span(&visible_ids) > available_width
            {
                let removed = visible_ids.remove(1);
                hidden_ids.push(removed);
            }
        }
    }
    let pill_w = pill_width(hidden_ids.len());
    ProjectTabOverflow {
        visible: place(&visible_ids, pill_w + PROJECT_TAB_SPACING),
        hidden_ids,
        pill: Some(ProjectTabPillSlot {
            x: 0.0,
            width: pill_w,
        }),
    }
}

/// The label's width (`projectTabLabelWidth`): the title measured at the tab's weight plus the
/// modified dot. GPUI shapes text at paint time, so the port measures with the 12-point system font's
/// average advance instead of a font metrics query.
pub fn project_tab_label_width(title: &str, modified: bool, active: bool) -> f64 {
    let advance = if active { 6.9 } else { 6.6 };
    let title_width = title.chars().count() as f64 * advance;
    let dot = if modified { TAB_DOT_WIDTH } else { 0.0 };
    (title_width + dot).clamp(TAB_LABEL_MIN, TAB_LABEL_MAX)
}

/// A pill's width (`projectTabPillWidth`).
pub fn project_tab_pill_width(title: &str, modified: bool, active: bool) -> f64 {
    project_tab_label_width(title, modified, active) + TAB_PILL_CHROME
}

/// The overflow pill's width (`projectTabOverflowPillWidth`).
pub fn project_tab_overflow_pill_width(hidden_count: usize) -> f64 {
    let text = project_tab_overflow_label(hidden_count);
    text.chars().count() as f64 * 6.6 + OVERFLOW_PILL_CHROME
}

/// One open project (`ProjectTab`).
pub struct ProjectTab {
    pub id: Id,
    pub default_name: String,
    pub session: Entity<EditorSession>,
    pub controller: ProjectController,
    pub view: Entity<ContentView>,
}

impl ProjectTab {
    /// The tab's title: the project's file name, or the name it was given (`title`).
    pub fn title(&self, cx: &App) -> String {
        self.session
            .read(cx)
            .project_url
            .as_ref()
            .and_then(|url| url.file_stem())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.default_name.clone())
    }
}

/// `ProjectTab.init(name:)`: a fresh session, its controller and the view that draws it.
fn new_tab(name: String, host: Arc<dyn SessionHost>, cx: &mut Context<ProjectWorkspace>) -> ProjectTab {
    let session = cx.new(|_| EditorSession::new());
    let view = cx.new(|cx| ContentView::new(session.clone(), host, cx));
    ProjectTab {
        id: compositor_core::new_id(),
        default_name: name,
        session,
        controller: ProjectController::new(),
        view,
    }
}

/// The tabs, and which one is in front (`ProjectWorkspace`).
pub struct ProjectWorkspace {
    tabs: Vec<ProjectTab>,
    selected_id: Id,
    next_number: i32,
    /// `isManaging`: an operation (open, close, quit, a copy between projects) is up, so no tab may
    /// start another.
    is_managing: bool,
    /// A tab whose close button was pressed, waiting for the pump to ask the unsaved-changes
    /// question (`close(_:)`). The strip has no prompter of its own, so it leaves the request here
    /// and `poll_tabs` runs the flow the app answers, exactly as Swift's `Task { await close(id) }` did.
    pending_close: Option<Id>,
    /// The platform services every tab's session needs.
    host: Arc<dyn SessionHost>,
}

impl ProjectWorkspace {
    /// The first tab, `Untitled`, with the clipboard's first canvas-size check skipped.
    pub fn new(host: Arc<dyn SessionHost>, cx: &mut Context<Self>) -> Self {
        let tab = new_tab("Untitled".to_string(), host.clone(), cx);
        let id = tab.id;
        tab.session
            .update(cx, |session, _| session.skips_initial_clipboard_canvas_size = true);
        let mut workspace = Self {
            tabs: vec![tab],
            selected_id: id,
            next_number: 2,
            is_managing: false,
            pending_close: None,
            host,
        };
        workspace.sync_controllers();
        workspace
    }

    /// Swift's controller read the workspace back through a weak reference: `canStart` refused while
    /// `workspace.isManaging` was up, and `isFrontmost` compared `workspace.current.controller === self`.
    /// The port has no back-reference, so both are mirrored onto every tab's controller here.
    fn sync_controllers(&mut self) {
        let selected = self.selected_id;
        let managing = self.is_managing;
        for tab in &mut self.tabs {
            tab.controller.managing = managing;
            tab.controller.frontmost = tab.id == selected;
        }
    }

    pub fn tabs(&self) -> &[ProjectTab] {
        &self.tabs
    }

    pub fn selected_id(&self) -> Id {
        self.selected_id
    }

    /// The tab in front (`current`).
    pub fn current(&self) -> &ProjectTab {
        self.tabs
            .iter()
            .find(|tab| tab.id == self.selected_id)
            .unwrap_or(&self.tabs[0])
    }

    pub fn tab(&self, id: Id) -> Option<&ProjectTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn host(&self) -> &Arc<dyn SessionHost> {
        &self.host
    }

    /// Whether a tab may be switched away from (`canSwitch`).
    pub fn can_switch(&self, cx: &App) -> bool {
        if self.is_managing {
            return false;
        }
        let session = self.current().session.read(cx);
        session.can_start_project_operation()
            && session.hue_saturation.is_none()
            && session.filter_edit.is_none()
            && session.gradient_edit.is_none()
            && session.pixel_move.is_none()
            && session.color_picker.is_none()
    }

    /// `addTab(reuseEmpty:)`: the empty first tab is reused unless a new one is asked for.
    pub fn add_tab(&mut self, reuse_empty: bool, cx: &mut Context<Self>) -> Id {
        if reuse_empty && self.tabs.len() == 1 && self.current().session.read(cx).document.is_none() {
            return self.selected_id;
        }
        let tab = new_tab(format!("Untitled {}", self.next_number), self.host.clone(), cx);
        self.next_number += 1;
        let id = tab.id;
        self.tabs.push(tab);
        self.selected_id = id;
        self.sync_controllers();
        id
    }

    /// `moveTab(_:to:)`: reorders the strip. Chrome, not a document edit, so it never touches undo.
    pub fn move_tab(&mut self, id: Id, to: usize) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let target = to.min(self.tabs.len().saturating_sub(1));
        if target == from {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(target, tab);
    }

    /// `select(_:)`: commits the outgoing tab's transform and brings the chosen one forward.
    pub fn select(&mut self, id: Id, cx: &mut Context<Self>) {
        if id == self.selected_id || !self.can_switch(cx) || !self.tabs.iter().any(|tab| tab.id == id) {
            return;
        }
        let previous = self.current().session.clone();
        previous.update(cx, |session, _| session.commit_transform());
        self.selected_id = id;
        self.sync_controllers();
    }

    /// `newCanvas()`: a fresh tab beside the current one.
    pub fn new_canvas(&mut self, cx: &mut Context<Self>) {
        if !self.can_switch(cx) {
            return;
        }
        let session = self.current().session.clone();
        session.update(cx, |session, _| session.commit_transform());
        self.add_tab(false, cx);
    }

    /// `removeTab(_:)`: the strip always keeps at least one tab.
    pub fn remove_tab(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.add_tab(false, cx);
        } else if self.selected_id == id {
            self.selected_id = self.tabs[index.min(self.tabs.len() - 1)].id;
        }
        self.sync_controllers();
    }

    /// `quitOrder`: the tab on screen first, then the rest left to right.
    pub fn quit_order(&self) -> Vec<Id> {
        let mut order = vec![self.selected_id];
        order.extend(self.tabs.iter().map(|tab| tab.id).filter(|id| *id != self.selected_id));
        order
    }

    /// `open(_:)`: one project per tab, from the files the app supplied or the Open panel. `canSwitch`
    /// refuses while another operation is up.
    pub fn open(&mut self, supplied: Option<&Path>, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) -> bool {
        if !self.can_switch(cx) {
            return false;
        }
        self.is_managing = true;
        self.sync_controllers();
        let mut urls: Vec<PathBuf> = supplied.map(|url| vec![url.to_path_buf()]).unwrap_or_default();
        if urls.is_empty() {
            // The Open panel allows several projects; its Cancel supplies none.
            if let Some(url) = prompter.choose_project_to_open() {
                urls.push(url);
            }
        }
        let mut opened = false;
        for url in &urls {
            opened = self.load_project(url, prompter, cx) || opened;
        }
        self.is_managing = false;
        self.sync_controllers();
        opened
    }

    /// `loadProject(_:)`: opens a project, or brings its tab forward when it is already open. An
    /// already-open project compares by path, as `resolvingSymlinksInPath` did.
    pub fn load_project(&mut self, url: &Path, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) -> bool {
        let resolved = resolve_symlinks(url);
        if let Some(existing) = self.tabs.iter().position(|tab| {
            tab.session
                .read(cx)
                .project_url
                .as_ref()
                .is_some_and(|open| resolve_symlinks(open) == resolved)
        }) {
            self.selected_id = self.tabs[existing].id;
            self.sync_controllers();
            return true;
        }
        // Load into an unattached tab, so a failed open never leaves a broken tab.
        let name = url
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let mut tab = new_tab(name, self.host.clone(), cx);
        let id = tab.id;
        let host = self.host.clone();
        let session = tab.session.clone();
        let controller = &mut tab.controller;
        let opened = session.update(cx, |session, _| controller.open(session, host.as_ref(), prompter, Some(url)));
        if !opened {
            return false;
        }
        if self.tabs.len() == 1 && self.current().session.read(cx).document.is_none() {
            self.tabs.clear();
        }
        self.tabs.push(tab);
        self.selected_id = id;
        self.sync_controllers();
        true
    }

    /// A tab's close button was pressed: remember it so [`Self::poll_tabs`] can ask the
    /// unsaved-changes question with the app's prompter, as Swift's `Task { await close(id) }` did.
    pub fn request_close(&mut self, id: Id) {
        self.pending_close = Some(id);
    }

    /// `close(_:)`: the unsaved-changes question first, then the tab goes.
    pub fn close(&mut self, id: Id, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) {
        if !self.can_switch(cx) {
            return;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.is_managing = true;
        self.sync_controllers();
        let host = self.host.clone();
        let session = self.tabs[index].session.clone();
        let controller = &mut self.tabs[index].controller;
        let confirmed = session.update(cx, |session, _| controller.confirm_quit(session, host.as_ref(), prompter));
        if confirmed {
            self.remove_tab(id, cx);
        }
        self.is_managing = false;
        self.sync_controllers();
    }

    /// `receive(_:into:at:)`: a `.comp` opens as a project; anything else imports into `destination`
    /// (or a new tab). The drop's security scope (`startAccessingSecurityScopedResource`) has no port,
    /// and a session that cannot start an operation yet queues the images it is handed.
    pub fn receive_paths(
        &mut self,
        urls: &[PathBuf],
        destination: Option<Id>,
        at: Option<Point>,
        prompter: &mut dyn ProjectPrompter,
        cx: &mut Context<Self>,
    ) {
        self.is_managing = true;
        self.sync_controllers();
        let host = self.host.clone();
        for url in urls {
            if is_project_package(url) {
                self.load_project(url, prompter, cx);
                continue;
            }
            let tab_id = match destination {
                Some(id) => match self.tab(id).map(|tab| tab.id) {
                    Some(id) => id,
                    None => continue,
                },
                None => self.add_tab(true, cx),
            };
            self.selected_id = tab_id;
            self.sync_controllers();
            if let Some(tab) = self.tab(tab_id) {
                let session = tab.session.clone();
                let single = [url.clone()];
                session.update(cx, |session, _| {
                    session.import_images(&single, at, host.as_ref());
                });
            }
        }
        self.is_managing = false;
        self.sync_controllers();
    }

    /// The `layerType` half of `receiveProviders(_:into:at:)`: a layer row dropped on another tab (or
    /// on the canvas) carries the row's id as a plain string. The `NSItemProvider` list itself has no
    /// port — a gpui drop carries one typed payload — and the file half is [`Self::receive_paths`].
    pub fn receive_layer_provider(&mut self, value: &str, destination: Option<Id>, at: Option<Point>, cx: &mut Context<Self>) {
        if let Ok(id) = Id::parse_str(value.trim()) {
            self.copy_layer(id, destination, at, cx);
        }
    }

    /// Cmd-V with a whole layer copied: pastes it complete — a copy above it in its own project, or
    /// brought over as dragging it onto this tab does. False when there is none, and Paste goes on
    /// with pixels.
    pub fn paste_copied_layer(&mut self, cx: &mut Context<Self>) -> bool {
        // `NSPasteboard.general.changeCount`: every session holds the app's pasteboard handle, so the
        // front project's reads the same count, and anything copied since replaces the layer copy.
        let count = self
            .current()
            .session
            .read(cx)
            .clipboard
            .as_ref()
            .map(|clipboard| clipboard.change_count())
            .unwrap_or(0);
        let Some(source) = self.tabs.iter().position(|tab| {
            tab.session
                .read(cx)
                .copied_layer
                .as_ref()
                .is_some_and(|copied| copied.change_count == count)
        }) else {
            return false;
        };
        let source_id = self.tabs[source].id;
        let ids: Vec<Id> = {
            let session = self.tabs[source].session.read(cx);
            let (Some(copied), Some(document)) = (session.copied_layer.as_ref(), session.document.as_ref()) else {
                return false;
            };
            copied
                .ids
                .iter()
                .copied()
                .filter(|id| document.layers.iter().any(|layer| layer.id == *id))
                .collect()
        };
        if ids.is_empty() {
            return false;
        }
        if source_id == self.selected_id {
            // Copied in this project: a plain Duplicate above the original.
            let session = self.tabs[source].session.clone();
            if !session.read(cx).can_edit_layers() {
                return false;
            }
            session.update(cx, |session, _| session.duplicate_layers(&ids, "Paste"));
            return true;
        }
        let destination = self.selected_id;
        self.copy_layers(&ids, Some(destination), None, cx);
        true
    }

    /// `copyLayer(_:into:at:)`: one layer, the way a drag onto a tab brings it over.
    pub fn copy_layer(&mut self, id: Id, destination: Option<Id>, at: Option<Point>, cx: &mut Context<Self>) {
        self.copy_layers(&[id], destination, at, cx);
    }

    /// Copies layers (folders with all they hold) into another project, or a new one, as one undo
    /// step there. Several keep where they sit relative to each other, centered on `at` or the canvas
    /// as a whole.
    pub fn copy_layers(&mut self, ids: &[Id], destination: Option<Id>, at: Option<Point>, cx: &mut Context<Self>) {
        let Some(first) = ids.first().copied() else {
            return;
        };
        if !self.can_switch(cx) {
            return;
        }
        let Some(source) = self.tabs.iter().position(|tab| {
            tab.session
                .read(cx)
                .document
                .as_ref()
                .is_some_and(|document| document.layers.iter().any(|layer| layer.id == first))
        }) else {
            return;
        };
        let source_id = self.tabs[source].id;
        let source_size = {
            let session = self.tabs[source].session.read(cx);
            if !session.can_edit_layers() {
                return;
            }
            let Some(document) = session.document.as_ref() else {
                return;
            };
            document.size()
        };
        if destination == Some(source_id) {
            return;
        }
        let target = match destination {
            Some(id) => {
                let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                    return;
                };
                if !self.tabs[index].session.read(cx).can_start_project_operation() {
                    return;
                }
                index
            }
            None => {
                let id = self.add_tab(false, cx);
                self.tabs
                    .iter()
                    .position(|tab| tab.id == id)
                    .expect("the tab just added")
            }
        };
        {
            let session = self.tabs[target].session.read(cx);
            if session.document.is_some() && !session.can_edit_layers() {
                return;
            }
        }
        self.is_managing = true;
        self.sync_controllers();
        let host = self.host.clone();
        let source_session = self.tabs[source].session.clone();
        let target_session = self.tabs[target].session.clone();
        // `target.session.isProjectBusy`: up while the masks bake and dropped before the edit, since
        // `createDocument` and `canEditLayers` refuse while a session is busy. The source's own flag is
        // taken by `layersForCopy`.
        target_session.update(cx, |session, _| session.is_project_busy = true);
        let copied = source_session.update(cx, |session, _| session.layers_for_copy(ids, host.as_ref()));
        target_session.update(cx, |session, _| session.is_project_busy = false);
        let Some(copied) = copied else {
            // A bake failure leaves its message in the source session's `brush_error`, where
            // `layersForCopy` puts it (`copyLayers`' `catch` set the target's `importError`).
            self.is_managing = false;
            self.sync_controllers();
            return;
        };
        if target_session.update(cx, |session, _| session.copy_layers_into(&copied, ids, source_size, at)) {
            self.selected_id = self.tabs[target].id;
        }
        self.is_managing = false;
        self.sync_controllers();
    }

    /// `settlePendingEdits()`: canvas edits are applied, open dialogs are cancelled.
    pub fn settle_pending_edits(&mut self, cx: &mut Context<Self>) {
        let session = self.current().session.clone();
        session.update(cx, |session, _| {
            if session.gradient_edit.is_some() {
                session.commit_gradient();
            }
            if session.pixel_move.is_some() {
                session.finish_pixel_move();
            }
            session.cancel_filter();
            session.cancel_hue_saturation();
            session.cancel_levels();
            session.finish_adjustment_editing(false);
            session.cancel_color_range();
            session.selection_amount_operation = None;
            if session.color_picker.is_some() {
                session.close_color_picker(false);
            }
        });
    }

    /// `finishTextEditing()`: every tab's text draft is finished, the tab on screen first.
    pub fn finish_text_editing(&mut self, cx: &mut Context<Self>) -> bool {
        for id in self.quit_order() {
            if let Some(tab) = self.tab(id) {
                let session = tab.session.clone();
                let finished = session.update(cx, |session, _| {
                    if session.text_draft.is_some() {
                        session.finish_text()
                    } else {
                        true
                    }
                });
                if !finished {
                    return false;
                }
            }
        }
        true
    }

    /// `confirmQuit()`: the last chance to save, for every open project in turn, the tab on screen
    /// first. A project that still has something to finish refuses (`NSSound.beep()` has no sound in
    /// the port).
    pub fn confirm_quit(&mut self, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) -> bool {
        if self.is_managing || !self.finish_text_editing(cx) {
            return false;
        }
        self.settle_pending_edits(cx);
        if !self.can_switch(cx) {
            return false;
        }
        self.is_managing = true;
        self.sync_controllers();
        let mut quit = true;
        for id in self.quit_order() {
            self.selected_id = id;
            self.sync_controllers();
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                continue;
            };
            let host = self.host.clone();
            let session = self.tabs[index].session.clone();
            let controller = &mut self.tabs[index].controller;
            if !session.update(cx, |session, _| controller.confirm_quit(session, host.as_ref(), prompter)) {
                quit = false;
                break;
            }
        }
        self.is_managing = false;
        self.sync_controllers();
        quit
    }

    /// `closeWindow(_:)`: asks about every unsaved project, then drops every tab and starts a fresh
    /// one. True when the projects were all confirmed, so the app may close the window; false when the
    /// user cancelled and the window stays.
    pub fn close_all(&mut self, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) -> bool {
        if !self.confirm_quit(prompter, cx) {
            return false;
        }
        self.tabs.clear();
        self.add_tab(false, cx);
        true
    }

    /// `isManaging`: the app's Quit asks before it starts anything (`applicationShouldTerminate`).
    pub fn is_managing(&self) -> bool {
        self.is_managing
    }

    /// The tab on screen with its controller and its session (`applicationDelegate.projects`): how the
    /// app's menu commands — Save, Save As, the size and trim sheets — reach the file layer. The
    /// workspace keeps the controller's `managing` and `frontmost` in step, so callers leave those be.
    pub fn current_controller(&mut self) -> (&mut ProjectController, Entity<EditorSession>) {
        let index = self.tabs.iter().position(|tab| tab.id == self.selected_id).unwrap_or(0);
        let tab = &mut self.tabs[index];
        (&mut tab.controller, tab.session.clone())
    }

    /// The port's UI pump, every tab in `quitOrder`: the imports each session is still holding
    /// (`pollImports`), and the file watch's checks and queued drops (`pollRecheck`,
    /// `resumeExternalChangeCheck`, `drainIncoming`). Swift's sessions and controllers resumed their
    /// own tasks; the port's wait for the UI to call again, so the app runs this on its timer with
    /// the prompter its alerts go through.
    pub fn poll_tabs(&mut self, prompter: &mut dyn ProjectPrompter, cx: &mut Context<Self>) {
        // The strip's close button: ask about unsaved work, then drop the tab (`close(_:)`).
        if let Some(id) = self.pending_close.take() {
            self.close(id, prompter, cx);
        }
        let host = self.host.clone();
        for id in self.quit_order() {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                continue;
            };
            let session = self.tabs[index].session.clone();
            let controller = &mut self.tabs[index].controller;
            session.update(cx, |session, _| {
                session.poll_imports(host.as_ref());
                controller.poll_recheck(session, host.as_ref(), prompter);
                controller.resume_external_change_check(session, host.as_ref(), prompter);
                controller.drain_incoming(session, host.as_ref(), prompter);
            });
        }
    }
}

/// `URL.resolvingSymlinksInPath()`: the path two project URLs compare by. A path that cannot be
/// resolved — a recent project already deleted — stands for itself.
fn resolve_symlinks(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl EventEmitter<()> for ProjectWorkspace {}

/// The tab strip above the editor: the pills that fit, the overflow pill, the "New" drop slot and the
/// drag that reorders them (`ProjectTabStrip`).
pub struct ProjectTabStrip {
    workspace: Entity<ProjectWorkspace>,
    /// The width the strip is laid out at, mirrored for the gesture handlers.
    slot_width: f64,
    /// A tab being reordered, if any: its id and how far the pointer has moved.
    reorder: Option<(Id, f64)>,
}

impl ProjectTabStrip {
    pub fn new(workspace: Entity<ProjectWorkspace>, cx: &mut Context<Self>) -> Self {
        cx.observe(&workspace, |_, _, cx| cx.notify()).detach();
        Self {
            workspace,
            slot_width: 0.0,
            reorder: None,
        }
    }

    /// The widths of every pill at this moment.
    fn widths(&self, cx: &App) -> Vec<(Id, f64)> {
        let workspace = self.workspace.read(cx);
        workspace
            .tabs()
            .iter()
            .map(|tab| {
                let active = tab.id == workspace.selected_id();
                (
                    tab.id,
                    project_tab_pill_width(&tab.title(cx), tab.session.read(cx).is_modified(), active),
                )
            })
            .collect()
    }

    fn overflow(&self, cx: &App, available_width: f64) -> ProjectTabOverflow {
        let workspace = self.workspace.read(cx);
        let widths = self.widths(cx);
        let lookup = move |id: Id| {
            widths
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(_, width)| *width)
                .unwrap_or(0.0)
        };
        let order: Vec<Id> = workspace.tabs().iter().map(|tab| tab.id).collect();
        project_tab_overflow(
            &order,
            &lookup,
            workspace.selected_id(),
            available_width,
            &project_tab_overflow_pill_width,
        )
    }

    /// `commitReorder(_:)`: applies the drag's drop target to the real order.
    fn commit_reorder(&mut self, cx: &mut Context<Self>) {
        let Some((id, translation)) = self.reorder.take() else {
            return;
        };
        let workspace = self.workspace.read(cx);
        let order: Vec<Id> = workspace.tabs().iter().map(|tab| tab.id).collect();
        let Some(from) = order.iter().position(|candidate| *candidate == id) else {
            return;
        };
        let widths = self.widths(cx);
        let mut x = 0.0;
        let mut target = order.len();
        for (index, candidate) in order.iter().enumerate() {
            let width = widths
                .iter()
                .find(|(other, _)| other == candidate)
                .map(|(_, width)| *width)
                .unwrap_or(0.0);
            if index != from {
                let center = x + width / 2.0;
                if translation < center {
                    target = index;
                    break;
                }
                x += width + PROJECT_TAB_SPACING;
            }
        }
        let mut target = target;
        if from < target {
            target -= 1;
        }
        self.workspace.update(cx, |workspace, cx| {
            workspace.move_tab(id, target);
            cx.notify();
        });
    }
}

impl Render for ProjectTabStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The strip spans the window (`w_full`), so its width is the viewport's: this is the port's
        // substitute for reading the strip's own laid-out bounds (`onGeometryChange`).
        let viewport_width = f64::from(f32::from(window.viewport_size().width));
        if (viewport_width - self.slot_width).abs() > 0.5 {
            self.slot_width = viewport_width;
        }
        let layout = self.overflow(cx, self.slot_width);
        let workspace = self.workspace.read(cx);
        let selected = workspace.selected_id();
        let can_switch = workspace.can_switch(cx);
        let tabs: Vec<AnyElement> = layout
            .visible
            .iter()
            .filter_map(|slot| {
                let tab = workspace.tab(slot.id)?;
                let active = slot.id == selected;
                let modified = tab.session.read(cx).is_modified();
                let title = tab.title(cx);
                let label = if modified { format!("• {title}") } else { title };
                let id = slot.id;
                let workspace = self.workspace.clone();
                let close = cx.listener(move |this, _, _, cx| {
                    this.workspace.update(cx, |workspace, cx| {
                        workspace.request_close(id);
                        cx.notify();
                    });
                });
                Some(
                    div()
                        .absolute()
                        .left(px(slot.x as f32))
                        .top(px(3.0))
                        .w(px(slot.width as f32))
                        .h(px(TAB_PILL_HEIGHT))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .pl(px(11.0))
                        .pr(px(8.0))
                        .rounded_full()
                        .bg(if active {
                            hsla(0.0, 0.0, 1.0, 0.12)
                        } else {
                            hsla(0.0, 0.0, 1.0, 0.04)
                        })
                        .when(active, |this| this.text_color(hsla(0.0, 0.0, 1.0, 0.95)))
                        .when(!active, |this| this.text_color(hsla(0.0, 0.0, 1.0, 0.72)))
                        .when(!can_switch, |this| this.opacity(0.5))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .text_size(px(12.0))
                                .when(active, |this| this.font_weight(FontWeight::SEMIBOLD))
                                .child(label),
                        )
                        .child(
                            Button::new(format!("tab-close-{}", slot.id))
                                .icon(Icon::new(IconName::X))
                                .tooltip("Close Project")
                                .w(px(16.0))
                                .h(px(16.0))
                                .on_click(close),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                if this.workspace.read(cx).can_switch(cx) {
                                    this.workspace.update(cx, |workspace, cx| {
                                        workspace.select(id, cx);
                                        cx.notify();
                                    });
                                    this.reorder = Some((id, event.position.x.as_f32() as f64));
                                }
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect();

        let overflow_pill: Option<AnyElement> = layout.pill.map(|pill| {
            let hidden = layout.hidden_ids.clone();
            let label = project_tab_overflow_label(hidden.len());
            div()
                .absolute()
                .left(px(pill.x as f32))
                .top(px(3.0))
                .w(px(pill.width as f32))
                .h(px(TAB_PILL_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(11.0))
                .rounded_full()
                .bg(hsla(0.0, 0.0, 1.0, 0.035))
                .text_size(px(12.0))
                .text_color(hsla(0.0, 0.0, 1.0, 0.72))
                .when(!can_switch, |this| this.opacity(0.5))
                .child(div().child(label.clone()))
                .child(Icon::new(IconName::ChevronDown).size(px(9.0)))
                .id("project-tab-overflow")
                .tooltip({
                    let label = label.clone();
                    move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                })
                .into_any_element()
        });

        let content_width = layout.content_width();
        let workspace = self.workspace.clone();
        div()
            .id("project-tab-strip")
            .relative()
            .h(px(TAB_STRIP_HEIGHT))
            .w_full()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some((_, start)) = this.reorder else {
                    return;
                };
                let x = event.position.x.as_f32() as f64;
                this.reorder = Some((this.reorder.map(|(id, _)| id).unwrap_or_default(), x - start));
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.commit_reorder(cx)),
            )
            .child(
                div()
                    .relative()
                    .w(px(content_width as f32))
                    .h(px(TAB_STRIP_HEIGHT))
                    .children(tabs)
                    .children(overflow_pill),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .on_mouse_down(MouseButton::Left, {
                        let workspace = workspace.clone();
                        move |_, window, cx| {
                            let _ = &workspace;
                            // The gap beside the tabs drags the window, as the title bar does.
                            window.start_window_move();
                            cx.stop_propagation();
                        }
                    }),
            )
    }
}

/// The window's editor content: the current tab's `ContentView`, and the tab strip above it.
pub struct ProjectWorkspaceView {
    workspace: Entity<ProjectWorkspace>,
    strip: Entity<ProjectTabStrip>,
}

impl ProjectWorkspaceView {
    pub fn new(workspace: Entity<ProjectWorkspace>, cx: &mut Context<Self>) -> Self {
        let strip = cx.new(|cx| ProjectTabStrip::new(workspace.clone(), cx));
        cx.observe(&workspace, |_, _, cx| cx.notify()).detach();
        Self { workspace, strip }
    }
}

impl Render for ProjectWorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.read(cx);
        let current = workspace.current();
        let session = current.session.clone();
        let content = current.view.clone();
        let can_switch = workspace.can_switch(cx);
        let _ = session;
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(hsla(0.0, 0.0, 0.14, 1.0))
            .child(self.strip.clone())
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .when(!can_switch, |this| this.opacity(0.6))
                    .child(content),
            )
    }
}

/// A fresh `Id` for a tab, exposed so the app can build one before the workspace exists.
pub fn new_tab_id() -> Id {
    compositor_core::new_id()
}

#[cfg(test)]
mod tests {
    // `use super::*` would also bring in the toolkit's `test` attribute macro (gpui-kit's test-support
    // exports one), which shadows the built-in `#[test]`; these tests are the built-in's.
    use ::core::prelude::v1::test;
    use super::*;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    #[test]
    fn overflow_label_is_singular_for_one() {
        assert_eq!(project_tab_overflow_label(1), "1 more tab");
        assert_eq!(project_tab_overflow_label(3), "3 more tabs");
    }

    #[test]
    fn everything_fits_without_a_pill() {
        let order = [id(1), id(2)];
        let widths = |_: Id| 100.0;
        let layout = project_tab_overflow(&order, &widths, id(1), 500.0, &project_tab_overflow_pill_width);
        assert_eq!(layout.hidden_ids.len(), 0);
        assert!(layout.pill.is_none());
        assert_eq!(layout.visible.len(), 2);
        assert_eq!(layout.content_width(), 206.0);
    }

    #[test]
    fn the_selected_tab_is_never_hidden() {
        let order = [id(1), id(2), id(3), id(4)];
        let widths = |_: Id| 100.0;
        let layout = project_tab_overflow(&order, &widths, id(4), 320.0, &project_tab_overflow_pill_width);
        assert!(layout.visible.iter().any(|slot| slot.id == id(4)));
        assert!(!layout.hidden_ids.contains(&id(4)));
        assert!(layout.pill.is_some());
    }

    #[test]
    fn an_unmeasured_strip_lays_everything_out() {
        let order = [id(1), id(2)];
        let widths = |_: Id| 100.0;
        let layout = project_tab_overflow(&order, &widths, id(1), 0.0, &project_tab_overflow_pill_width);
        assert_eq!(layout.visible.len(), 2);
        assert!(layout.pill.is_none());
    }
}
