//! Projects and files: the session's half of `ProjectController`, the `.comp` package types, the
//! import queue, the PSD conversion sheet, the RAW develop sheet and the export commands.
//!
//! Ports `Document/EditorSession+Projects.swift` (`projectSnapshot`, `installProject`, `reloadProject`,
//! `clearProject`, `createNewProject`), the session-facing half of `IO/ProjectController.swift` and
//! `IO/ProjectController+ExternalChanges.swift` (`begin`, `prepareSave`, `write`, the open path and the
//! export paths), and the import body of `EditorSession.importImages` (`drainImports`, `insert`,
//! `beginPSDReading`/`finishPSDReading`/`endPSDReading`/`confirmPSDConversions`/`finishConversion`,
//! `developRaw`/`finishRawDevelop`, `insertPhotoshop`).
//!
//! # The seam
//! `compositor-session` cannot depend on `compositor-io` (`docs/PORTING.md`'s crate order is
//! `… ← session ← io ← ui ← app`), so the file work the Swift code called directly — `ProjectStore`,
//! `ImageExporter`, `ImageImporter`, `RawImporter`, `PSDReader`, `LiveMaskBaker` — goes through
//! [`SessionHost`], which the io and app crates implement. Every command keeps the Swift guards, the
//! transaction names and the wording; only the calls cross the trait, and a trait method's `Err` string
//! is the message Swift showed (`error.localizedDescription`).
//!
//! # Async → sync
//! * `importImages` suspended on a `CheckedContinuation` until its request drained (waiting first for
//!   [`EditorSession::can_start_project_operation`]). The port queues the request and drains it
//!   synchronously; while a project operation runs the request waits in `pending_imports` and the UI
//!   drains it with [`EditorSession::poll_imports`]. `is_importing`, `import_error` and the blocked
//!   edits read exactly as they did.
//! * `developRaw` and `finishPSDReading` awaited a sheet. The port stops the drain at the sheet
//!   ([`ImportAwait`], with `shows_raw_develop`/`shows_conversion_sheet` up and `is_importing` still
//!   true, as Swift's suspended task kept it) and resumes from
//!   [`EditorSession::finish_raw_develop`]/[`EditorSession::finish_conversion`].
//! * `waitForProjectAccess`/`waitForFileRequest` awaited a busy flag. Nothing blocks: the drain returns
//!   and the UI polls; `ProjectController.drainIncoming`'s waiter half stays with io.
//! * The sandbox's `startAccessingSecurityScopedResource` has no equivalent and is dropped; nothing else
//!   in the queue changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use compositor_core::document::{CanvasDocument, ImageLayer, ProjectLayerRecord, ProjectManifest, ProjectSnapshot};
use compositor_core::geom::{Point, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::LayerMask;
use compositor_core::limits;
use compositor_core::raster::RasterSnapshot;
use compositor_core::{new_id, Id};
use compositor_pixels::camera_raw::RawDevelopSettings;

use crate::EditorSession;

/// The Quick Look preview bytes a project package carries (`QuickLookImages` wraps exactly this one
/// JPEG blob; the seam passes the bytes so the session names no io type).
pub type QuickLookPreview = Vec<u8>;

/// One Photoshop feature that had to be converted (`PSDConversion`, `IO/PSDTypes.swift`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PSDConversion {
    pub id: Id,
    pub layer_name: String,
    pub message: String,
}

impl PSDConversion {
    pub fn new(layer_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self { id: new_id(), layer_name: layer_name.into(), message: message.into() }
    }
}

/// The conversion sheet's state (`PSDConversionRequest`, `UI/PSDConversionSheet.swift`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PSDConversionRequest {
    pub id: Id,
    pub title: String,
    pub confirm_title: String,
    pub conversions: Vec<PSDConversion>,
    /// The file is still being read: the sheet is up so the click feels answered, but it has nothing to
    /// report yet.
    pub is_reading: bool,
}

impl PSDConversionRequest {
    pub fn new(title: impl Into<String>, confirm_title: impl Into<String>, conversions: Vec<PSDConversion>, is_reading: bool) -> Self {
        Self { id: new_id(), title: title.into(), confirm_title: confirm_title.into(), conversions, is_reading }
    }

    /// `init(title:confirmTitle:isReading: true)`.
    pub fn reading(title: impl Into<String>, confirm_title: impl Into<String>) -> Self {
        Self::new(title, confirm_title, Vec::new(), true)
    }
}

/// A Photoshop document that came in whole, with what had to be converted (`PSDImport`,
/// `IO/PSD/PSDDocumentBuilder.swift`). The layers carry their pixels already.
#[derive(Clone, Debug)]
pub struct PhotoshopImport {
    pub layers: Vec<ImageLayer>,
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    pub conversions: Vec<PSDConversion>,
}

/// What reading a Photoshop file produced.
#[derive(Clone, Debug)]
pub enum PhotoshopRead {
    /// Photoshop wrote no layer records — just the merged image — so the file comes in as one layer
    /// (`parsed.layers.isEmpty`).
    BackgroundOnly,
    Imported(Box<PhotoshopImport>),
}

/// The RAW file being developed and the settings the sheet is editing (`rawDevelop`, see `RawImporter`).
#[derive(Clone, Debug)]
pub struct RawDevelop {
    pub url: std::path::PathBuf,
    pub settings: RawDevelopSettings,
}

/// One queued `importImages` call (`ProjectController.Incoming`/`ImportRequest` without the
/// continuation the port does not need).
#[derive(Clone, Debug, Default)]
pub struct ImportRequest {
    pub files: Vec<std::path::PathBuf>,
    pub point: Option<Point>,
}

/// Where the drain stopped because a sheet is up. Swift suspended on a `CheckedContinuation` here; the
/// port records the slot the sheet fills in and resumes when it does.
#[derive(Clone, Debug)]
pub enum ImportAwait {
    /// `developRaw(url)`: `settings` is filled in by `finish_raw_develop`, and the awaited file is
    /// developed with them (or skipped when the sheet was cancelled) when the drain resumes.
    RawDevelop { url: std::path::PathBuf, settings: Option<RawDevelopSettings> },
    /// `finishPSDReading`: `answer` is filled in by `finish_conversion`, and the already-read import is
    /// inserted when it says yes.
    PsdConversions { answer: Option<bool>, imported: Box<PhotoshopImport> },
}

impl ImportAwait {
    /// True while the sheet has not answered yet: the drain waits, and a later poll continues.
    pub fn is_unanswered(&self) -> bool {
        match self {
            Self::RawDevelop { settings, .. } => settings.is_none(),
            Self::PsdConversions { answer, .. } => answer.is_none(),
        }
    }
}

/// How far the drain got inside the front request (`drainImports`'s loop position, kept because the
/// port can stop at a sheet and resume).
#[derive(Clone, Debug, Default)]
pub struct ImportCursor {
    /// The index of the next file of the front request.
    pub file: usize,
}

/// Whether the drain may move on to the next file.
enum ImportStep {
    Next,
    Waiting,
}

/// What the session asks of the host: the `.comp` package, the flattened render, image decoding, RAW
/// develop, Photoshop reading and live-mask baking. The io and app crates implement it; a method that a
/// host does not support returns its default, and the feature it belongs to reports the failure.
///
/// The `Err` string of every method is the message the user is shown (Swift's
/// `error.localizedDescription`).
pub trait SessionHost {
    /// `ProjectStore.shared.load(from:)`.
    fn load_project(&self, url: &Path) -> Result<ProjectSnapshot, String> {
        let _ = url;
        Err("The project store is not available.".to_string())
    }

    /// `ProjectStore.shared.save(_:to:quickLook:)`.
    fn save_project(&self, snapshot: &ProjectSnapshot, to: &Path, quick_look: Option<QuickLookPreview>) -> Result<(), String> {
        let _ = (snapshot, to, quick_look);
        Err("The project store is not available.".to_string())
    }

    /// `ImageExporter.shared.quickLookImages(_:)`: the flattened preview a save stores for Quick Look,
    /// or nil for a canvas too large to flatten.
    fn quick_look_images(&self, snapshot: &ProjectSnapshot) -> Option<QuickLookPreview> {
        let _ = snapshot;
        None
    }

    /// `ImageExporter.shared.render(_:)`: the document flattened, which the adjustment dialog samples
    /// and the JPEG sheet previews.
    fn render(&self, snapshot: &ProjectSnapshot) -> Result<RasterSnapshot, String> {
        let _ = snapshot;
        Err("The renderer is not available.".to_string())
    }

    /// `ImageExporter.shared.exportPNG(_:to:)`.
    fn export_png(&self, snapshot: &ProjectSnapshot, to: &Path) -> Result<(), String> {
        let _ = (snapshot, to);
        Err("The image exporter is not available.".to_string())
    }

    /// `ImageExporter.shared.write(_:to:)`: the already-encoded bytes the JPEG sheet produced.
    fn write_bytes(&self, data: &[u8], to: &Path) -> Result<(), String> {
        let _ = (data, to);
        Err("The image exporter is not available.".to_string())
    }

    /// `FileManager.default.fileExists(atPath:)`, generalized to a URL.
    fn file_exists(&self, url: &Path) -> bool {
        let _ = url;
        false
    }

    /// `ImageImporter.shared.decode(_:remainingPixels:flattenedPhotoshop:)`.
    fn decode_image(&self, url: &Path, remaining_pixels: usize, flattened_photoshop: bool) -> Result<ImportedImage, String> {
        let _ = (url, remaining_pixels, flattened_photoshop);
        Err("The image importer is not available.".to_string())
    }

    /// `ImageImporter.shared.decodeSVG(_:fitting:remainingPixels:)`.
    fn decode_svg(&self, url: &Path, fitting: Option<Size>, remaining_pixels: usize) -> Result<ImportedImage, String> {
        let _ = (url, fitting, remaining_pixels);
        Err("The image importer is not available.".to_string())
    }

    /// `PSDReader.matches(_:)`.
    fn matches_psd(&self, url: &Path) -> bool {
        let _ = url;
        false
    }

    /// `ImageImporter.shared.loadPhotoshop`, `photoshopAssets` and `PSDDocumentBuilder.makeImport`,
    /// which the session called in that order.
    fn read_photoshop(&self, url: &Path, remaining_pixels: usize) -> Result<PhotoshopRead, String> {
        let _ = (url, remaining_pixels);
        Err("The Photoshop reader is not available.".to_string())
    }

    /// `RawImporter.matches(_:)`.
    fn matches_raw(&self, url: &Path) -> bool {
        let _ = url;
        false
    }

    /// `RawImporter.pixelSize(_:)`.
    fn raw_pixel_size(&self, url: &Path) -> Option<Size> {
        let _ = url;
        None
    }

    /// `RawImporter.asShot(_:)`.
    fn raw_as_shot(&self, url: &Path) -> Option<RawDevelopSettings> {
        let _ = url;
        None
    }

    /// `RawImporter.Queue.shared.develop(_:settings:limit:)`, thumbnail included.
    fn develop_raw(&self, url: &Path, settings: &RawDevelopSettings, limit: Option<usize>) -> Result<ImportedImage, String> {
        let _ = (url, settings, limit);
        Err("The RAW developer is not available.".to_string())
    }

    /// `RawImporter.Queue.shared.release()`, which the Swift `finishRawDevelop` called.
    fn release_raw_develop(&self) {}

    /// `LiveMaskBaker.bake(_:target:)`: the target's pixels with its live mask baked in, through the
    /// renderer in `compositor-render`.
    fn bake_live_mask(&self, snapshot: &ProjectSnapshot, target: Id) -> Result<Option<ImportedImage>, String> {
        let _ = (snapshot, target);
        Err("The live-mask baker is not available.".to_string())
    }
}

/// `ProjectSnapshot.mask(for:)` (`Document/LayerMask.swift`): the record's mask when it names one and
/// the package carries it.
pub fn mask_for(snapshot: &ProjectSnapshot, record: &ProjectLayerRecord) -> Option<LayerMask> {
    if record.mask_file.is_none() {
        return None;
    }
    let asset = snapshot.masks.get(&record.id)?;
    Some(LayerMask {
        asset: asset.clone(),
        is_enabled: record.mask_enabled.unwrap_or(true),
        placement: record.mask_placement,
        is_linked: record.mask_linked.unwrap_or(true),
    })
}

impl EditorSession {
    /// `projectSnapshot()`: the document as the package stores it, with every layer's pixels and mask.
    pub fn project_snapshot(&self) -> Option<ProjectSnapshot> {
        let document = self.document.as_ref()?;
        let mut images: HashMap<Id, ImportedImage> = HashMap::new();
        let mut masks: HashMap<Id, ImportedImage> = HashMap::new();
        let layers: Vec<ProjectLayerRecord> = document
            .layers
            .iter()
            .map(|layer| {
                let stem = layer.id.to_string().to_uppercase();
                if let Some(asset) = layer.asset.clone() {
                    images.insert(layer.id, asset);
                }
                if let Some(mask) = layer.mask.as_ref() {
                    masks.insert(layer.id, mask.asset.clone());
                }
                ProjectLayerRecord {
                    id: layer.id,
                    name: layer.name.clone(),
                    is_visible: layer.is_visible,
                    transform: layer.transform,
                    image_file: layer.asset.as_ref().map(|_| format!("{stem}.png")),
                    parent_id: layer.parent_id,
                    is_group: Some(layer.is_group),
                    opacity: Some(layer.opacity),
                    blend_mode: Some(layer.blend_mode),
                    mask_file: layer.mask.as_ref().map(|_| format!("{stem}.mask.png")),
                    mask_enabled: layer.mask.as_ref().map(|mask| mask.is_enabled),
                    mask_source_id: layer.mask_source_id,
                    adjustment: layer.adjustment.clone(),
                    mask_placement: layer.mask.as_ref().and_then(|mask| mask.placement),
                    mask_linked: layer.mask.as_ref().map(|mask| mask.is_linked),
                    shape: layer.shape.as_ref().map(|shape| shape.style.clone()),
                    effects: layer.effects.clone(),
                    text: layer.text.as_ref().map(|text| text.style.clone()),
                }
            })
            .collect();
        let mut manifest = ProjectManifest::new(document.id, document.width, document.height, self.active_layer_id, layers);
        manifest.resolution = Some(document.resolution);
        manifest.guides = if document.guides.is_empty() { None } else { Some(document.guides.clone()) };
        Some(ProjectSnapshot { manifest, images, masks })
    }

    /// `installProject(_:from:)`. Called only after the entire package has successfully validated and
    /// loaded.
    pub fn install_project(&mut self, snapshot: &ProjectSnapshot, from: &Path) {
        self.collapsed_group_ids.clear();
        self.is_mask_selected = false;
        self.cancel_crop();
        self.guide_drag = None;
        let manifest = &snapshot.manifest;
        self.transform_edit = None;
        let layers: Vec<ImageLayer> = manifest
            .layers
            .iter()
            .map(|record| {
                let asset = snapshot.images.get(&record.id).cloned();
                ImageLayer {
                    id: record.id,
                    asset: asset.clone(),
                    name: record.name.clone(),
                    is_visible: record.is_visible,
                    transform: record.transform,
                    parent_id: record.parent_id,
                    is_group: record.is_group == Some(true),
                    opacity: record.opacity.unwrap_or(1.0),
                    blend_mode: record.blend_mode.unwrap_or(compositor_core::blend::LayerBlendMode::Normal),
                    mask: mask_for(snapshot, record),
                    mask_source_id: record.mask_source_id,
                    adjustment: record.adjustment.clone(),
                    shape: compositor_core::layer_shape::LayerShape::loaded(record.shape.clone(), shared_rgba_of(asset.as_ref())),
                    effects: record.effects.clone(),
                    text: compositor_core::layer_text::LayerText::loaded(record.text.clone(), shared_rgba_of(asset.as_ref())),
                }
            })
            .collect();
        let mut document = CanvasDocument::new(manifest.width, manifest.height);
        document.layers = layers;
        document.resolution = manifest.resolution.unwrap_or(72.0);
        document.guides = manifest.guides.clone().unwrap_or_default();
        self.document = Some(document);
        self.set_active_layer(manifest.active_layer_id);
        self.project_url = Some(from.to_path_buf());
        self.renaming_layer_id = None;
        self.history.reset();
        if let Some(document) = self.document.as_ref() {
            let size = document.size();
            self.viewport.fit(size);
        }
    }

    /// `reloadProject(_:)`: replaces the document with what its package holds now, after something else
    /// wrote it. Unlike [`Self::install_project`] it keeps the viewport, the collapsed folders and the
    /// selection where those layers still exist, so the reload is invisible beyond the change itself.
    /// Undo history is session-only and starts over, as after an open.
    pub fn reload_project(&mut self, snapshot: &ProjectSnapshot) {
        let Some(url) = self.project_url.clone() else { return };
        let viewport = self.viewport.clone();
        let collapsed = self.collapsed_group_ids.clone();
        let active = self.active_layer_id;
        let selected = self.selected_layer_ids.clone();
        self.install_project(snapshot, &url);
        self.viewport = viewport;
        let ids: HashSet<Id> = snapshot.manifest.layers.iter().map(|record| record.id).collect();
        self.collapsed_group_ids = collapsed.intersection(&ids).copied().collect();
        if let Some(active) = active {
            if ids.contains(&active) {
                self.set_active_layer(Some(active));
                self.selected_layer_ids = selected.intersection(&ids).copied().collect();
                self.selected_layer_ids.insert(active);
            }
        }
    }

    /// `clearProject()`.
    pub fn clear_project(&mut self) {
        self.collapsed_group_ids.clear();
        self.is_mask_selected = false;
        self.cancel_crop();
        self.transform_edit = None;
        self.guide_drag = None;
        self.document = None;
        self.set_active_layer(None);
        self.renaming_layer_id = None;
        self.project_url = None;
        self.history.reset();
    }

    /// `createNewProject(width:height:)`: File > New.
    pub fn create_new_project(&mut self, width: usize, height: usize) {
        if self.is_project_busy
            || self.is_importing
            || !(1..=limits::MAX_SIDE).contains(&width)
            || !(1..=limits::MAX_SIDE).contains(&height)
        {
            return;
        }
        self.clear_project();
        self.create_document(width, height, true);
    }

    /// `ProjectController.begin()`: refuses when another operation is running, ends the crop and any
    /// transform edit, and takes the busy flag. The caller must pair it with
    /// [`Self::end_project_operation`].
    pub fn begin_project_operation(&mut self) -> bool {
        if !self.can_start_project_operation() {
            return false;
        }
        self.cancel_crop();
        self.commit_transform();
        self.is_project_busy = true;
        true
    }

    /// The `defer { session.isProjectBusy = false }` every controller operation ends with.
    pub fn end_project_operation(&mut self) {
        self.is_project_busy = false;
    }

    /// `prepareSave(asNew:)`: the document as it is now with the revision a finished save counts as
    /// saved. The Save panel and the destination stay with the io controller.
    pub fn prepare_save(&self) -> Option<(ProjectSnapshot, Id)> {
        let revision = self.history.current_revision();
        let snapshot = self.project_snapshot()?;
        Some((snapshot, revision))
    }

    /// The session half of `ProjectController.write(_:to:revision:)`: writes the captured document and,
    /// on success, records where it went and which revision counts as saved. The Quick Look image, the
    /// digest, the watcher and the recent-project note stay with the io controller.
    pub fn save_project(&mut self, snapshot: &ProjectSnapshot, to: &Path, revision: Id, host: &dyn SessionHost) -> Result<(), String> {
        let quick_look = host.quick_look_images(snapshot);
        host.save_project(snapshot, to, quick_look)?;
        self.project_url = Some(to.to_path_buf());
        self.history.mark_saved_at(revision);
        Ok(())
    }

    /// The session half of `ProjectController.open(_:)`: validates the package before it may replace
    /// the live document, then installs it. The Open panel, the unsaved-changes alert, the digest and
    /// the watcher stay with the io controller, which shows the returned message under its
    /// "Couldn’t open the project" title.
    pub fn open_project(&mut self, source: &Path, host: &dyn SessionHost) -> Result<bool, String> {
        if !self.begin_project_operation() {
            return Ok(false);
        }
        let result = if !host.file_exists(source) {
            // A recent project deleted in Finder: name the project, not the manifest inside it.
            Err(format!(
                "The file “{}” couldn’t be opened because there is no such file.",
                source.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| source.display().to_string())
            ))
        } else {
            host.load_project(source).map(|snapshot| {
                self.install_project(&snapshot, source);
                true
            })
        };
        self.end_project_operation();
        result
    }

    /// `ProjectController.reloadFromDisk`'s session half: a package that fails to load, half written or
    /// mid-sync, leaves the open document alone.
    pub fn reload_project_from(&mut self, url: &Path, host: &dyn SessionHost) -> bool {
        let Ok(snapshot) = host.load_project(url) else { return false };
        if self.project_url.as_deref() != Some(url) || self.document.is_none() {
            return false;
        }
        self.reload_project(&snapshot);
        true
    }

    /// The session half of `ProjectController.exportPNG()`.
    pub fn export_png(&mut self, destination: &Path, host: &dyn SessionHost) -> Result<(), String> {
        if self.document.is_none() || !self.begin_project_operation() {
            return Ok(());
        }
        let result = match self.project_snapshot() {
            Some(snapshot) => host.export_png(&snapshot, destination),
            None => Ok(()),
        };
        self.end_project_operation();
        result
    }

    /// `ProjectController.exportJPEG()`'s write half: the JPEG sheet produced the bytes, the destination
    /// came from the Save panel.
    pub fn export_jpeg(&mut self, data: &[u8], destination: &Path, host: &dyn SessionHost) -> Result<(), String> {
        if self.document.is_none() || !self.begin_project_operation() {
            return Ok(());
        }
        let result = host.write_bytes(data, destination);
        self.end_project_operation();
        result
    }

    /// The render the JPEG sheet previews (`ImageExporter.shared.render`), behind the same busy flag.
    pub fn render_export(&mut self, host: &dyn SessionHost) -> Result<Option<RasterSnapshot>, String> {
        if self.document.is_none() || !self.begin_project_operation() {
            return Ok(None);
        }
        let result = match self.project_snapshot() {
            Some(snapshot) => host.render(&snapshot).map(Some),
            None => Ok(None),
        };
        self.end_project_operation();
        result
    }

    /// `importImages(_:at:)`: queues the files and drains the queue.
    ///
    /// Swift awaited the request's completion here; the port drains synchronously when the session is
    /// free and leaves the request queued when a project operation is running, for the UI to drain with
    /// [`Self::poll_imports`].
    pub fn import_images(&mut self, urls: &[std::path::PathBuf], at: Option<Point>, host: &dyn SessionHost) {
        if urls.is_empty() {
            return;
        }
        if self.brush_stroke.is_some() {
            self.finish_brush();
        }
        self.cancel_crop();
        self.commit_transform();
        self.pending_imports.push(ImportRequest { files: urls.to_vec(), point: at });
        if self.is_project_busy {
            // `await waitForProjectAccess()` before the request was enqueued: nothing starts yet.
            return;
        }
        self.poll_imports(host);
    }

    /// Drains the import queue: after `is_project_busy` clears, after a sheet's answer, from
    /// [`Self::import_images`]. Swift's suspended task resumed by itself; the UI polls here.
    pub fn poll_imports(&mut self, host: &dyn SessionHost) {
        if self.pending_imports.is_empty() || self.is_project_busy {
            return;
        }
        self.is_importing = true;
        self.drain_imports(host);
    }

    /// `drainImports()`.
    fn drain_imports(&mut self, host: &dyn SessionHost) {
        while !self.pending_imports.is_empty() {
            if self.import_await.as_ref().is_some_and(ImportAwait::is_unanswered) {
                // A sheet is up: its answer resumes the drain, and `is_importing` stays true.
                return;
            }
            if self.import_cursor.is_none() {
                // No document: the first successful image determines the canvas.
                let psd_only = self.pending_imports[0].files.iter().all(|url| host.matches_psd(url));
                self.begin_edit(if psd_only { "Import Photoshop File" } else { "Import Images" });
                self.import_cursor = Some(ImportCursor::default());
            }
            let file_count = self.pending_imports[0].files.len();
            while self.import_cursor.as_ref().is_some_and(|cursor| cursor.file < file_count) {
                let index = self.import_cursor.as_ref().map(|cursor| cursor.file).unwrap_or(0);
                match self.import_file(index, host) {
                    ImportStep::Next => {
                        if let Some(cursor) = self.import_cursor.as_mut() {
                            cursor.file += 1;
                        }
                    }
                    ImportStep::Waiting => return,
                }
            }
            self.import_cursor = None;
            self.pending_imports.remove(0);
            self.end_edit();
        }
        self.is_importing = false;
        if !self.import_failures.is_empty() {
            self.import_error = Some(self.import_failures.drain(..).collect::<Vec<String>>().join("\n\n"));
        }
    }

    /// One file of the front request. A sheet that stopped the previous pass answers here before the
    /// file is looked at again.
    fn import_file(&mut self, index: usize, host: &dyn SessionHost) -> ImportStep {
        if let Some(awaiting) = self.import_await.take() {
            match awaiting {
                ImportAwait::RawDevelop { url, settings } => {
                    return match settings {
                        // The sheet was cancelled: the file is skipped, as `guard let settings = … else { continue }` did.
                        None => ImportStep::Next,
                        Some(settings) => {
                            self.import_developed_raw(&url, &settings, host);
                            ImportStep::Next
                        }
                    };
                }
                ImportAwait::PsdConversions { answer, imported } => match answer {
                    None => {
                        self.import_await = Some(ImportAwait::PsdConversions { answer: None, imported });
                        return ImportStep::Waiting;
                    }
                    Some(false) => return ImportStep::Next,
                    Some(true) => {
                        let point = self.import_point();
                        let name = self
                            .pending_imports
                            .first()
                            .and_then(|request| request.files.get(index))
                            .map(|url| file_stem(url))
                            .unwrap_or_default();
                        if let Err(message) = self.insert_photoshop(&imported, &name, point) {
                            self.record_import_failure(&name, &message);
                        }
                        return ImportStep::Next;
                    }
                },
            }
        }
        let Some(url) = self.pending_imports.first().and_then(|request| request.files.get(index)).cloned() else {
            return ImportStep::Next;
        };
        let point = self.import_point();
        let file_name = file_name_of(&url);
        // No document: the first successful image determines the canvas, regardless of drop point.
        let used_pixels = self.used_import_pixels();
        let remaining = limits::document_pixel_budget().saturating_sub(used_pixels);
        if host.matches_raw(&url) {
            let Some(size) = host.raw_pixel_size(&url) else {
                self.record_import_failure(&file_name, "The image could not be read. It may be damaged or unavailable.");
                return ImportStep::Next;
            };
            if size.width > limits::MAX_SIDE as f64 || size.height > limits::MAX_SIDE as f64 || (size.width * size.height) as usize > remaining {
                self.record_import_failure(&file_name, &compositor_core::imported_image::ImageImportError::TooLarge.to_string());
                return ImportStep::Next;
            }
            let as_shot = host.raw_as_shot(&url).unwrap_or_default();
            // `confirmRawDevelop`: tests develop without a sheet.
            if let Some(answer) = self.confirm_raw_develop.as_ref().map(|confirm| confirm(&url, as_shot.clone())) {
                if let Some(settings) = answer {
                    self.import_developed_raw(&url, &settings, host);
                }
                return ImportStep::Next;
            }
            // `await developRaw(url)`: the sheet goes up and the drain stops until it answers.
            self.raw_develop = Some(RawDevelop { url: url.clone(), settings: as_shot });
            self.shows_raw_develop = true;
            self.import_await = Some(ImportAwait::RawDevelop { url, settings: None });
            return ImportStep::Waiting;
        }
        if has_extension(&url, "svg") || has_extension(&url, "svgz") {
            match host.decode_svg(&url, self.document.as_ref().map(|document| document.size()), remaining) {
                Ok(asset) => self.insert(asset, point),
                Err(message) => self.record_import_failure(&file_name, &message),
            }
            return ImportStep::Next;
        }
        if host.matches_psd(&url) {
            // The sheet goes up before the file is read, so a big PSD doesn't leave the click unanswered.
            self.begin_psd_reading(&format!("Open “{file_name}”?"), "Import");
            let read = match host.read_photoshop(&url, remaining) {
                Ok(read) => read,
                Err(message) => {
                    self.end_psd_reading();
                    self.record_import_failure(&file_name, &message);
                    return ImportStep::Next;
                }
            };
            return match read {
                PhotoshopRead::BackgroundOnly => {
                    // Only a background: Photoshop wrote no layer records, just the merged image.
                    self.end_psd_reading();
                    match host.decode_image(&url, remaining, true) {
                        Ok(asset) => self.insert(asset, point),
                        Err(message) => self.record_import_failure(&file_name, &message),
                    }
                    ImportStep::Next
                }
                PhotoshopRead::Imported(imported) => match self.finish_psd_reading(imported.conversions.clone()) {
                    Some(true) => {
                        let name = file_stem(&url);
                        if let Err(message) = self.insert_photoshop(&imported, &name, point) {
                            self.record_import_failure(&file_name, &message);
                        }
                        ImportStep::Next
                    }
                    Some(false) => ImportStep::Next,
                    None => {
                        self.import_await = Some(ImportAwait::PsdConversions { answer: None, imported });
                        ImportStep::Waiting
                    }
                },
            };
        }
        match host.decode_image(&url, remaining, false) {
            Ok(asset) => self.insert(asset, point),
            Err(message) => self.record_import_failure(&file_name, &message),
        }
        ImportStep::Next
    }

    /// The develop and insert half of the RAW branch, shared by the sheet's direct answer and the
    /// resumed one.
    fn import_developed_raw(&mut self, url: &Path, settings: &RawDevelopSettings, host: &dyn SessionHost) {
        let point = self.import_point();
        match host.develop_raw(url, settings, None) {
            Ok(asset) => self.insert(asset, point),
            Err(_) => self.record_import_failure(
                &file_name_of(url),
                "The image could not be read. It may be damaged or unavailable.",
            ),
        }
    }

    /// The point the front request drops at: nil while there is no document yet.
    fn import_point(&self) -> Option<Point> {
        if self.document.is_none() {
            return None;
        }
        self.pending_imports.first().and_then(|request| request.point)
    }

    /// The layers' imported pixels (`document?.layers.reduce(0) { … image.width * image.height }`), the
    /// budget the next import is measured against.
    fn used_import_pixels(&self) -> usize {
        self.document
            .as_ref()
            .map(|document| {
                document
                    .layers
                    .iter()
                    .map(|layer| layer.asset.as_ref().map_or(0, |asset| asset.image.width() * asset.image.height()))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// `failures.append("\(url.lastPathComponent): \(error.localizedDescription)")`.
    fn record_import_failure(&mut self, file_name: &str, message: &str) {
        self.import_failures.push(format!("{file_name}: {message}"));
    }

    /// `insert(_:centeredAt:)`: places an imported image, starting the canvas when there is none.
    pub fn insert(&mut self, asset: ImportedImage, centered_at: Option<Point>) {
        self.begin_edit("Import Image");
        if self.document.is_none() {
            let mut document = CanvasDocument::new(asset.image.width(), asset.image.height());
            document.layers.clear();
            self.document = Some(document);
            if let Some(document) = self.document.as_ref() {
                let size = document.size();
                self.viewport.fit(size);
            }
        }
        if let Some(document) = self.document.as_ref() {
            let center = centered_at.unwrap_or_else(|| Point::new(document.width as f64 / 2.0, document.height as f64 / 2.0));
            let origin = Point::new(
                (center.x - asset.image.width() as f64 / 2.0).floor(),
                (center.y - asset.image.height() as f64 / 2.0).floor(),
            );
            let mut layer = ImageLayer::from_asset(asset, origin);
            layer.parent_id = match self.active_layer() {
                Some(active) if active.is_group => self.active_layer_id,
                Some(active) => active.parent_id,
                None => None,
            };
            let layer_id = layer.id;
            let parent = layer.parent_id;
            if let Some(document) = self.document.as_mut() {
                document.layers.push(layer);
            }
            if let Some(parent) = parent {
                self.collapsed_group_ids.remove(&parent);
            }
            self.set_active_layer(Some(layer_id));
        }
        self.end_edit();
    }

    /// `insertPhotoshop(_:named:centeredAt:)`.
    pub fn insert_photoshop(&mut self, imported: &PhotoshopImport, named: &str, centered_at: Option<Point>) -> Result<(), String> {
        self.begin_edit("Import Photoshop File");
        let result = self.insert_photoshop_body(imported, named, centered_at);
        self.end_edit();
        result
    }

    fn insert_photoshop_body(&mut self, imported: &PhotoshopImport, named: &str, centered_at: Option<Point>) -> Result<(), String> {
        let mut incoming = imported.layers.clone();
        let wrapping = self.document.is_some();
        let added = incoming.len() + usize::from(wrapping);
        if self.document.as_ref().map_or(0, |document| document.layers.len()) + added > 10_000 {
            return Err(compositor_core::imported_image::ImageImportError::TooLarge.to_string());
        }
        if self.document.is_none() {
            let mut document = CanvasDocument::new(imported.width, imported.height);
            document.resolution = imported.resolution;
            document.layers.clone_from(&incoming);
            self.document = Some(document);
            if let Some(document) = self.document.as_ref() {
                let size = document.size();
                self.viewport.fit(size);
            }
            self.set_active_layer(
                incoming
                    .iter()
                    .filter(|layer| layer.parent_id.is_none())
                    .next_back()
                    .or_else(|| incoming.last())
                    .map(|layer| layer.id),
            );
            return Ok(());
        }
        let mut group = ImageLayer::blank(named, self.document.as_ref().map(|document| document.size()).unwrap_or(Size::ZERO));
        group.is_group = true;
        group.parent_id = match self.active_layer() {
            Some(active) if active.is_group => self.active_layer_id,
            Some(active) => active.parent_id,
            None => None,
        };
        if let Some(point) = centered_at {
            let mut box_ = compositor_core::geom::Rect::NULL;
            for layer in incoming.iter().filter(|layer| !layer.is_group) {
                box_ = box_.union(compositor_core::geom::Rect::new(
                    layer.transform.origin.x,
                    layer.transform.origin.y,
                    layer.transform.size.width,
                    layer.transform.size.height,
                ));
            }
            if !box_.is_null() && !box_.is_empty()
                && box_.origin.x.is_finite()
                && box_.origin.y.is_finite()
            {
                let dx = point.x - box_.mid_x();
                let dy = point.y - box_.mid_y();
                for layer in incoming.iter_mut() {
                    layer.transform.origin.x += dx;
                    layer.transform.origin.y += dy;
                }
            }
        }
        let group_id = group.id;
        for layer in incoming.iter_mut().filter(|layer| layer.parent_id.is_none()) {
            layer.parent_id = Some(group_id);
        }
        if let Some(document) = self.document.as_mut() {
            document.layers.push(group);
            document.layers.extend(incoming);
        }
        if let Some(parent) = self.document.as_ref().and_then(|document| document.layers.iter().find(|layer| layer.id == group_id)).and_then(|layer| layer.parent_id) {
            self.collapsed_group_ids.remove(&parent);
        }
        self.collapsed_group_ids.remove(&group_id);
        self.set_active_layer(Some(group_id));
        Ok(())
    }

    /// `beginPSDReading(title:confirmTitle:)`: puts the sheet up before the file is read, so a big PSD
    /// doesn't leave the click unanswered.
    pub fn begin_psd_reading(&mut self, title: &str, confirm_title: &str) {
        if self.confirm_conversions.is_some() {
            return;
        }
        self.conversion_cancelled = false;
        self.conversion_request = Some(PSDConversionRequest::reading(title, confirm_title));
        self.shows_conversion_sheet = true;
    }

    /// `finishPSDReading(_:)`: fills the sheet in, or takes it away when there is nothing to report.
    /// `Some(true/false)` is the answer the import can act on at once (nothing to report, the read was
    /// cancelled, or a test's `confirm_conversions` answered); `None` means the sheet is up and
    /// [`Self::finish_conversion`] resumes the import.
    pub fn finish_psd_reading(&mut self, conversions: Vec<PSDConversion>) -> Option<bool> {
        // `confirmConversions`: tests answer without a sheet.
        if let Some(answer) = self
            .confirm_conversions
            .as_ref()
            .map(|confirm| if conversions.is_empty() { true } else { confirm(&conversions) })
        {
            return Some(answer);
        }
        if self.conversion_cancelled {
            self.end_psd_reading();
            return Some(false);
        }
        if conversions.is_empty() {
            self.end_psd_reading();
            return Some(true);
        }
        if let Some(request) = self.conversion_request.as_mut() {
            request.conversions = conversions;
            request.is_reading = false;
        }
        self.conversion_answer_pending = true;
        None
    }

    /// `endPSDReading()`: takes the sheet away without an answer — nothing to report, or the read
    /// failed.
    pub fn end_psd_reading(&mut self) {
        if self.conversion_answer_pending {
            return;
        }
        self.shows_conversion_sheet = false;
        self.conversion_request = None;
    }

    /// `confirmPSDConversions(_:title:confirmTitle:)`: the sheet on its own, for a conversion list that
    /// came from somewhere other than this session's import.
    pub fn confirm_psd_conversions(&mut self, conversions: Vec<PSDConversion>, title: &str, confirm_title: &str) -> Option<bool> {
        if let Some(answer) = self.confirm_conversions.as_ref().map(|confirm| confirm(&conversions)) {
            return Some(answer);
        }
        self.conversion_request = Some(PSDConversionRequest::new(title, confirm_title, conversions, false));
        self.shows_conversion_sheet = true;
        self.conversion_answer_pending = true;
        None
    }

    /// `finishConversion(_:)`: the sheet's button. Cancel while the file is still being read is
    /// remembered, so `finish_psd_reading` answers false instead of showing an empty sheet. The answer
    /// resumes the import, which the UI would otherwise have to poll for.
    pub fn finish_conversion(&mut self, confirmed: bool, host: &dyn SessionHost) {
        if !confirmed && self.conversion_request.as_ref().is_some_and(|request| request.is_reading) {
            self.conversion_cancelled = true;
        }
        self.shows_conversion_sheet = false;
        self.conversion_request = None;
        self.conversion_answer_pending = false;
        if let Some(ImportAwait::PsdConversions { answer, .. }) = self.import_await.as_mut() {
            *answer = Some(confirmed);
        }
        self.poll_imports(host);
    }

    /// `finishRawDevelop(_:)`: the develop sheet's answer. The host releases its develop queue slot, and
    /// the awaiting import resumes.
    pub fn finish_raw_develop(&mut self, settings: Option<RawDevelopSettings>, host: &dyn SessionHost) {
        self.shows_raw_develop = false;
        self.raw_develop = None;
        host.release_raw_develop();
        if let Some(ImportAwait::RawDevelop { settings: slot, .. }) = self.import_await.as_mut() {
            *slot = settings;
        }
        self.poll_imports(host);
    }
}

/// `asset.image` when the asset's pixels are color (a shape or text layer is only ever built from one).
fn shared_rgba_of(asset: Option<&ImportedImage>) -> Option<compositor_core::buffer::SharedImage> {
    match asset.map(|asset| &asset.image) {
        Some(PixelImage::Rgba(image)) => Some(image.clone()),
        _ => None,
    }
}

/// `url.deletingPathExtension().lastPathComponent`.
fn file_stem(url: &Path) -> String {
    url.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `url.lastPathComponent`.
fn file_name_of(url: &Path) -> String {
    url.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| url.display().to_string())
}

/// `url.pathExtension.lowercased() == ext`.
fn has_extension(url: &Path, ext: &str) -> bool {
    url.extension().is_some_and(|value| value.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::buffer::{Gray8Image, Rgba8Image};
    use compositor_core::layer_effects::{LayerEffects, StrokeEffect};
    use std::sync::{Arc, Mutex};

    /// The host the tests use: every file decodes to the same 64×32 image except the ones whose name
    /// says `missing`, and a save is remembered.
    struct TestHost {
        image: ImportedImage,
        saves: Mutex<Vec<(std::path::PathBuf, ProjectManifest)>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                image: ImportedImage::new(
                    PixelImage::Rgba(Arc::new(Rgba8Image::new(64, 32))),
                    PixelImage::Rgba(Arc::new(Rgba8Image::new(16, 8))),
                    "Imported",
                ),
                saves: Mutex::new(Vec::new()),
            }
        }
    }

    impl SessionHost for TestHost {
        fn decode_image(&self, url: &Path, _remaining_pixels: usize, _flattened_photoshop: bool) -> Result<ImportedImage, String> {
            if file_name_of(url).contains("missing") {
                return Err("The image could not be read. It may be damaged or unavailable.".to_string());
            }
            Ok(self.image.clone())
        }

        fn save_project(&self, snapshot: &ProjectSnapshot, to: &Path, _quick_look: Option<QuickLookPreview>) -> Result<(), String> {
            self.saves.lock().expect("the test host is not poisoned").push((to.to_path_buf(), snapshot.manifest.clone()));
            Ok(())
        }

        fn file_exists(&self, _url: &Path) -> bool {
            true
        }
    }

    /// A document with one 64×32 image layer, ready for an edit.
    fn session_with_layer() -> (EditorSession, Id) {
        let mut session = EditorSession::default();
        let asset = ImportedImage::new(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(64, 32))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(16, 8))),
            "Imported",
        );
        let layer = ImageLayer::from_asset(asset, Point::ZERO);
        let id = layer.id;
        let mut document = CanvasDocument::new(128, 128);
        document.layers.push(layer);
        session.document = Some(document);
        session.set_active_layer(Some(id));
        (session, id)
    }

    /// A package with the named image layers, the first one carrying a mask.
    fn snapshot_with(names: &[&str]) -> ProjectSnapshot {
        let mut images: HashMap<Id, ImportedImage> = HashMap::new();
        let mut masks: HashMap<Id, ImportedImage> = HashMap::new();
        let mut layers = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let id = new_id();
            let asset = ImportedImage::new(
                PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 4))),
                PixelImage::Rgba(Arc::new(Rgba8Image::new(4, 2))),
                *name,
            );
            images.insert(id, asset);
            let mask_file = if index == 0 {
                masks.insert(
                    id,
                    ImportedImage::new(
                        PixelImage::Gray(Arc::new(Gray8Image::new(8, 4))),
                        PixelImage::Gray(Arc::new(Gray8Image::new(4, 2))),
                        "Layer Mask",
                    ),
                );
                Some(format!("{id}.mask.png"))
            } else {
                None
            };
            let mut record = ProjectLayerRecord::new(
                id,
                (*name).to_string(),
                true,
                compositor_core::layer_transform::LayerTransform { size: Size::new(8.0, 4.0), ..Default::default() },
                Some(format!("{id}.png")),
            );
            record.mask_file = mask_file;
            layers.push(record);
        }
        let active = layers.last().map(|record| record.id);
        ProjectSnapshot { manifest: ProjectManifest::new(new_id(), 128, 128, active, layers), images, masks }
    }

    #[test]
    fn the_modified_flag_follows_edits_undo_and_redo() {
        let (mut session, id) = session_with_layer();
        let host = TestHost::new();
        let (snapshot, revision) = session.prepare_save().expect("a document is open");
        session
            .save_project(&snapshot, Path::new("P.comp"), revision, &host)
            .expect("the host saves");
        assert_eq!(session.project_url.as_deref(), Some(Path::new("P.comp")));
        assert!(!session.is_modified());

        let mut effects = LayerEffects::default();
        effects.stroke = Some(StrokeEffect { size: 3.0, ..StrokeEffect::default() });
        session.set_effects(effects, Some(id), "Layer Effects");
        assert!(session.is_modified(), "an unsaved edit marks the project modified");

        session.undo();
        assert!(!session.is_modified(), "undoing back to the saved revision is unmodified");
        session.redo();
        assert!(session.is_modified(), "redo is an unsaved change again");
        session.undo();
        assert!(!session.is_modified());

        // A save of a captured revision stays saved while later edits go on. The layer is back at
        // the saved state, so this edit has to actually change the effects (an empty write would be
        // refused as identical, in Swift too).
        let (snapshot, revision) = session.prepare_save().expect("a document is open");
        let mut later = LayerEffects::default();
        later.stroke = Some(StrokeEffect { size: 7.0, ..StrokeEffect::default() });
        session.set_effects(later, Some(id), "Layer Effects");
        session
            .save_project(&snapshot, Path::new("P.comp"), revision, &host)
            .expect("the host saves");
        assert!(session.is_modified(), "the edit made while saving is still unsaved");
    }

    #[test]
    fn the_psd_conversion_sheet_reads_then_reports() {
        let mut session = EditorSession::default();
        let host = TestHost::new();
        session.begin_psd_reading("Open “x.psd”?", "Import");
        let request = session.conversion_request.clone().expect("the sheet is up");
        assert!(session.shows_conversion_sheet);
        assert!(request.is_reading);
        assert!(request.conversions.is_empty());
        assert_eq!(request.title, "Open “x.psd”?");
        assert_eq!(request.confirm_title, "Import");

        // Nothing to report: the sheet goes away and the import goes on.
        assert_eq!(session.finish_psd_reading(Vec::new()), Some(true));
        assert!(!session.shows_conversion_sheet);
        assert!(session.conversion_request.is_none());

        // A conversion keeps the sheet up with the list; the answer comes from finish_conversion.
        session.begin_psd_reading("Open “y.psd”?", "Import");
        let conversion = PSDConversion::new("Text", "The text was converted to pixels.");
        assert_eq!(session.finish_psd_reading(vec![conversion.clone()]), None, "the sheet waits for its answer");
        let request = session.conversion_request.clone().expect("the sheet is still up");
        assert!(!request.is_reading, "the file is read: the sheet reports its conversions");
        assert_eq!(request.conversions, vec![conversion]);
        assert!(session.shows_conversion_sheet);
        session.finish_conversion(true, &host);
        assert!(!session.shows_conversion_sheet);
        assert!(session.conversion_request.is_none());
        assert!(!session.conversion_cancelled);
    }

    #[test]
    fn cancelling_the_reading_sheet_skips_the_file() {
        let mut session = EditorSession::default();
        let host = TestHost::new();
        session.begin_psd_reading("Open “x.psd”?", "Import");
        assert!(session.conversion_request.as_ref().is_some_and(|request| request.is_reading));
        session.finish_conversion(false, &host);
        assert!(session.conversion_cancelled);
        assert!(!session.shows_conversion_sheet);
        // The read finishing afterwards reports the cancellation instead of showing an empty sheet.
        assert_eq!(session.finish_psd_reading(vec![PSDConversion::new("Text", "…")]), Some(false));
        assert!(session.conversion_request.is_none());
    }

    #[test]
    fn a_test_answers_the_sheet_without_showing_it() {
        let mut session = EditorSession::default();
        session.confirm_conversions = Some(Arc::new(|conversions: &[PSDConversion]| conversions.len() == 1));
        session.begin_psd_reading("Open “x.psd”?", "Import");
        assert!(session.conversion_request.is_none(), "the tests' answer replaces the sheet");
        assert!(!session.shows_conversion_sheet);
        assert_eq!(session.finish_psd_reading(Vec::new()), Some(true));
        assert_eq!(session.finish_psd_reading(vec![PSDConversion::new("Text", "…")]), Some(true));
        assert_eq!(session.finish_psd_reading(vec![PSDConversion::new("A", "…"), PSDConversion::new("B", "…")]), Some(false));
    }

    #[test]
    fn an_import_lands_centered_and_reports_its_failures() {
        let mut session = EditorSession::default();
        let host = TestHost::new();
        session.create_document(128, 128, false);
        session.import_images(&["good.png".into(), "missing.png".into()], None, &host);
        assert!(!session.is_importing);
        let document = session.document.as_ref().expect("a document");
        assert_eq!(document.layers.len(), 1, "the unreadable file is skipped");
        assert_eq!(document.layers[0].origin(), Point::new(32.0, 48.0), "the image is centered on the canvas");
        let error = session.import_error.clone().expect("the failure is reported");
        assert_eq!(
            error,
            "missing.png: The image could not be read. It may be damaged or unavailable."
        );
    }

    #[test]
    fn queued_imports_land_one_after_another() {
        let mut session = EditorSession::default();
        let host = TestHost::new();
        session.import_images(&["a.png".into()], None, &host);
        session.import_images(&["b.png".into()], None, &host);
        let document = session.document.as_ref().expect("a document");
        assert_eq!(document.layers.len(), 2);
        assert_eq!(document.layers[0].origin(), Point::ZERO, "the first image sizes the canvas");
        assert_eq!(document.layers[1].origin(), Point::ZERO, "the second is centered on it");
        assert!(!session.is_importing);
        assert!(session.import_error.is_none());
    }

    #[test]
    fn imports_wait_for_a_project_operation_and_the_ui_drains_them() {
        let mut session = EditorSession::default();
        let host = TestHost::new();
        session.create_document(100, 100, false);
        session.is_project_busy = true;
        session.import_images(&["a.png".into()], None, &host);
        assert!(!session.is_importing, "nothing starts while the session is busy");
        assert_eq!(session.document.as_ref().map(|document| document.layers.len()), Some(0));
        session.is_project_busy = false;
        session.poll_imports(&host);
        assert_eq!(session.document.as_ref().map(|document| document.layers.len()), Some(1));
        assert!(!session.is_importing);
    }

    #[test]
    fn installing_a_snapshot_restores_layers_masks_and_forgets_the_history() {
        let snapshot = snapshot_with(&["A", "B"]);
        let first = snapshot.manifest.layers[0].id;
        let mut session = EditorSession::default();
        session.install_project(&snapshot, Path::new("P.comp"));
        let document = session.document.as_ref().expect("installed");
        assert_eq!(document.layers.len(), 2);
        assert_eq!(document.layers[0].name, "A");
        assert_eq!(document.layers[1].name, "B");
        assert!(document.layers[0].mask.is_some(), "the record's mask comes with it");
        assert!(document.layers[1].mask.is_none());
        assert_eq!(session.active_layer_id, snapshot.manifest.active_layer_id);
        assert_eq!(session.project_url.as_deref(), Some(Path::new("P.comp")));
        assert!(!session.is_modified());
        assert!(!session.can_undo());

        // Reloading keeps the collapsed folders and the selection where those layers still exist.
        session.collapsed_group_ids = [first].into_iter().collect();
        let smaller = snapshot_with(&["A"]);
        session.reload_project(&smaller);
        assert_eq!(session.document.as_ref().map(|document| document.layers.len()), Some(1));
        assert_eq!(
            session.active_layer_id,
            smaller.manifest.active_layer_id,
            "the old selection is gone, so the reloaded package's own active layer stays selected"
        );
        assert!(session.collapsed_group_ids.is_empty(), "the collapsed id is not in the reloaded package");
    }

    #[test]
    fn creating_a_new_project_clears_the_document_and_refuses_while_busy() {
        let (mut session, _) = session_with_layer();
        session.project_url = Some("P.comp".into());
        session.is_project_busy = true;
        session.create_new_project(64, 64);
        assert!(session.document.is_some(), "a busy session refuses");
        assert_eq!(session.project_url.as_deref(), Some(Path::new("P.comp")));

        session.is_project_busy = false;
        session.create_new_project(64, 64);
        let document = session.document.as_ref().expect("a fresh document");
        assert_eq!((document.width, document.height), (64, 64));
        assert_eq!(document.layers.len(), 1, "File > New starts with one blank layer");
        assert!(session.project_url.is_none());
        // `clearProject()` resets the history and `createDocument` then records its own "New Canvas"
        // step, so Swift's `isModified` reads true for the fresh document.
        assert!(session.is_modified(), "File > New records the New Canvas step over the reset history");

        session.create_new_project(0, 64);
        assert_eq!(session.document.as_ref().map(|document| document.width), Some(64), "an out-of-range side is refused");
    }

    #[test]
    fn opening_a_missing_project_reports_it_without_replacing_the_document() {
        struct NoFiles(TestHost);
        impl SessionHost for NoFiles {
            fn decode_image(&self, url: &Path, remaining: usize, flattened: bool) -> Result<ImportedImage, String> {
                self.0.decode_image(url, remaining, flattened)
            }
            fn file_exists(&self, _url: &Path) -> bool {
                false
            }
        }
        let (mut session, id) = session_with_layer();
        let before = session.document.clone();
        let host = NoFiles(TestHost::new());
        let error = session
            .open_project(Path::new("Gone.comp"), &host)
            .expect_err("a missing package is an error");
        assert_eq!(error, "The file “Gone.comp” couldn’t be opened because there is no such file.");
        assert_eq!(session.document, before, "the live document survives");
        assert!(session.active_layer_id.is_some());
        assert_eq!(session.active_layer_id, Some(id));
        assert!(!session.is_project_busy, "the busy flag is cleared again");
    }
}
