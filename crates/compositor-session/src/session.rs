//! `EditorSession`: every piece of editor state and every command it runs — tools, drafts, panels,
//! undo transactions, clipboard, crop, gradient, type, brush, warp, project lifecycle.
//!
//! The port of `Document/EditorSession.swift` plus the command bodies that lived inside the
//! SwiftUI/AppKit views. The struct itself lives here; every sibling module (`layers.rs`,
//! `transform.rs`, `brush.rs`, …) contributes an `impl EditorSession` block with its own slice of
//! the commands.
//!
//! ## Async coordination
//!
//! Swift's session coordinated long operations with `Task`s, `await`ed continuations and waiter
//! queues (`projectWaiters`, `fileRequestWaiters`, `conversionContinuation`, `rawContinuation`,
//! `busyIndicatorTask`). The port keeps the same *state* those mechanisms produced — [`is_project_busy`]
//! and the `shows_busy` flag, the import/PSD/RAW answer flags (`conversion_answer_pending`,
//! `import_await`, `confirm_conversions`, …) — but no continuation plumbing: every command runs
//! synchronously, and a command that would have awaited a sheet instead records the pending request
//! and returns, with the answer arriving as a second call (`finish_conversion`,
//! `finish_raw_develop`). The `busyIndicatorTask`'s delayed `showsBusy` becomes the plain
//! `shows_busy` flag the UI sets.
//!
//! [`is_project_busy`]: EditorSession::is_project_busy

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use compositor_core::document::{CanvasDocument, ImageLayer, NavigationTool};
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::guides::{GridAppearance, GuideDrag, LayoutGrid};
use compositor_core::history::{DocumentHistory, Snapshot};
use compositor_core::image_ops::{FilterSettings, GradientSettings, GradientShape};
use compositor_core::layer_adjustment::LayerAdjustment;
use compositor_core::layer_effects::{LayerEffectSelection, LayerEffects};
use compositor_core::layer_mask::MaskDistortPreviewCache;
use compositor_core::layer_shape::{ShapeDraft, ShapeKind};
use compositor_core::layer_text::{LayerTextStyle, TextDraft};
use compositor_core::layer_transform::TransformEdit;
use compositor_core::limits::MAX_SIDE;
use compositor_core::palette::ColorPickerState;
use compositor_core::selection::{DocumentSelection, LassoDraft, LassoKind, SelectionMode, WandMode};
use compositor_core::viewport::CanvasViewport;
use compositor_core::{Id, LayerBlendMode, PaletteColor};
use compositor_pixels::brush::{BrushSettings, BrushStroke, SpotHealingMode};
use compositor_pixels::camera_raw::{CameraRawSettings, RawDevelopSettings};
use compositor_pixels::masks::{ObjectSelectionSettings, WandSettings};
use compositor_pixels::warp::{BlurToolMode, BrushToolMode, WarpStroke};
use compositor_render::effects_preview_cache::EffectsPreviewCache;
use rustc_hash::FxHashMap;

use crate::brush::{LastBrushPoint, ParkedBrushTips, PendingOpacityDigit};
use crate::camera_raw::CameraRawDrag;
use crate::clipboard::{Clipboard, CopiedLayer, PixelClipboard};
use crate::filters::{FilterEdit, HuePreviewJob, HueSaturationEdit, HueTargetDrag, LevelsEdit};
use crate::floating::PixelMove;
use crate::gradient::GradientEdit;
use crate::guides::GuideSettings;
use crate::projects::{
    ImportAwait, ImportCursor, ImportRequest, PSDConversion, PSDConversionRequest, RawDevelop,
};
use crate::selection::{ColorRangeEdit, HueSampleMode, SelectionAmountOperation};
use crate::shape::ShapePreview;
use crate::tools::CloneSettings;
use crate::transform::{DistortEffectsCache, DistortPreviewCache, TransformDuplicate};
use crate::view::PreviewZoomCommand;

/// The whole editor: the open document, the tool in hand, every draft and panel, and the history.
///
/// Every field is `pub` because the sibling `impl EditorSession` blocks and the UI read and write
/// them directly; the Swift `didSet` observers became methods where they carried behavior
/// (`set_active_layer`, `set_mask_paint_white`, `set_gradient_settings`, …).
pub struct EditorSession {
    // MARK: Document & view

    /// Clipboard reads skip their first canvas-size check (`skipsInitialClipboardCanvasSize`).
    pub skips_initial_clipboard_canvas_size: bool,
    pub document: Option<CanvasDocument>,
    /// Bumped to ask the canvas to take the keyboard (`canvasFocusRequest`).
    pub canvas_focus_request: u64,
    /// Draw the eyedropper's sample ring while it samples (`showsSampleRing`).
    pub shows_sample_ring: bool,
    pub viewport: CanvasViewport,
    pub tool: NavigationTool,
    /// Where the open project was last saved or loaded from; `None` for an unsaved canvas.
    pub project_url: Option<PathBuf>,

    // MARK: Busy state

    /// Blocks overlapping edits immediately. Not observed by the UI: controls only dim via
    /// [`shows_busy`](Self::shows_busy), after an operation has run long enough to be worth showing.
    pub is_project_busy: bool,
    /// True once `is_project_busy` has lasted long enough for the interface to say so
    /// (`showsBusy`; Swift raised it from a delayed task, the UI raises it here).
    pub shows_busy: bool,

    // MARK: Layers

    pub active_layer_id: Option<Id>,
    pub selected_layer_ids: HashSet<Id>,
    pub collapsed_group_ids: HashSet<Id>,
    pub renaming_layer_id: Option<Id>,
    /// The layer whose opacity the Move bar is editing (`opacityEditLayerID`).
    pub opacity_edit_layer_id: Option<Id>,
    /// The blend mode a hover preview is showing for a layer (`blendPreview`).
    pub blend_preview: Option<(Id, LayerBlendMode)>,
    pub is_mask_selected: bool,
    /// Option-click on a mask thumbnail: the canvas shows the targeted mask by itself, in grayscale.
    pub views_mask_alone: bool,
    pub history: DocumentHistory,

    // MARK: Transform

    pub transform_edit: Option<TransformEdit>,
    /// The copies an Option-drag made, and what was selected before it, so Escape can take them away.
    pub transform_duplicate: Option<TransformDuplicate>,
    pub distort_preview_cache: FxHashMap<Id, DistortPreviewCache>,
    pub distort_effects_cache: FxHashMap<Id, DistortEffectsCache>,
    /// The last rounded rectangle drawn for a transform in progress, by layer, with the size drawn at.
    pub shape_transform_preview_cache: FxHashMap<Id, ShapePreview>,
    pub locks_transform_ratio: bool,
    /// Off by default: a Move-tool press drags the active layer; hold Cmd (or turn this on) to pick
    /// the layer under the pointer (`transformAutoSelect`).
    pub transform_auto_select: bool,
    /// The Move tool's transform box and handles (⌘H); a pending ⌘T transform still shows its box.
    pub shows_transform_controls: bool,
    /// Document positions a move has just snapped to, drawn as guides while it lasts.
    pub snap_guides: (Vec<f64>, Vec<f64>),

    // MARK: Guides, grid & snapping

    pub snapping_enabled: bool,
    pub shows_guides: bool,
    pub shows_rulers: bool,
    pub shows_grid: bool,
    pub shows_pixel_grid: bool,
    pub snap_enabled: bool,
    pub snap_to_guides: bool,
    pub snap_to_grid: bool,
    pub snap_to_layers: bool,
    pub snap_to_document_bounds: bool,
    pub locks_guides: bool,
    pub layout_grid: LayoutGrid,
    pub grid_appearance: GridAppearance,
    pub guide_drag: Option<GuideDrag>,

    // MARK: Crop

    pub crop_rect: Option<Rect>,
    pub crop_ratio_choice: String,
    pub crop_error: Option<String>,

    // MARK: Brush

    pub brush_settings: BrushSettings,
    pub spot_healing_mode: SpotHealingMode,
    /// The Smear tool's two modes: Liquify pushes pixels, Blur softens them (R cycles them).
    pub blur_mode: BlurToolMode,
    /// The Brush's two modes: Paint lays down the foreground color, Erase clears pixels away.
    pub brush_mode: BrushToolMode,
    /// The brush tips of the families not in use, by family (see [`ParkedBrushTips`]).
    pub parked_brush_tips: ParkedBrushTips,
    /// Where the last brush stroke ended, so a Shift-click paints a straight line on from it.
    pub last_brush_point: Option<LastBrushPoint>,
    /// Where the brush is while Smoothing trails it behind the pointer.
    pub brush_anchor: Option<Point>,
    /// The pointer itself, so a smoothed stroke can catch up to it when the button is released.
    pub brush_pointer: Option<Point>,
    pub pending_opacity_digit: Option<PendingOpacityDigit>,
    pub brush_error: Option<String>,
    /// Bumped whenever painted pixels change; the canvas redraws when it moves.
    pub brush_revision: u64,
    /// A stroke in progress. Not observed by the UI: a stroke keeps the settings it started with.
    pub brush_stroke: Option<BrushStroke>,
    /// A Smudge or Liquify stroke in progress.
    pub warp_stroke: Option<WarpStroke>,
    /// The layer a warp stroke is working on, held while the stroke is down.
    pub warp_layer: Option<ImageLayer>,
    pub mask_distort_preview_cache: Option<MaskDistortPreviewCache>,

    // MARK: Clone Stamp

    /// The source Option-click set (document pixels).
    pub clone_source: Option<Point>,
    pub clone_settings: CloneSettings,
    /// The offset from brush to source that aligned strokes keep.
    pub clone_offset: Option<Size>,

    // MARK: Palette & gradient

    pub mask_paint_white: bool,
    pub background_color: PaletteColor,
    pub gradient_settings: GradientSettings,
    pub gradient_edit: Option<GradientEdit>,
    pub color_picker: Option<ColorPickerState>,
    /// The dialog whose color the picker is open on (`ColorPickerTarget::Dialog`).
    pub dialog_color_change: Option<Box<dyn FnMut(PaletteColor)>>,

    // MARK: Selection

    pub lasso_draft: Option<LassoDraft>,
    pub lasso_kind: LassoKind,
    pub marquee_kind: LassoKind,
    pub selection_mode_choice: SelectionMode,
    /// Mode implied by the Shift/Option keys currently held, nil when neither is.
    pub held_selection_mode: Option<SelectionMode>,
    /// The selection as it was when a drag-move began; the drag is one undo step.
    pub selection_move_origin: Option<DocumentSelection>,
    pub selection_antialiased: bool,
    /// How far Feather softens the selection's edge each time it is applied, in document pixels.
    pub selection_feather_amount: i32,
    /// Pixels the Expand / Contract buttons grow or shrink the selection by.
    pub selection_expand_amount: i32,
    pub selection_contract_amount: i32,
    /// What Select > Modify is asking an amount for (`SelectionAmountOperation`).
    pub selection_amount_operation: Option<SelectionAmountOperation>,
    pub wand_mode: WandMode,
    pub wand_settings: WandSettings,
    pub object_selection_settings: ObjectSelectionSettings,
    /// Select > Color Range's panel is open; the selection shown is its preview until OK.
    pub color_range: Option<ColorRangeEdit>,

    // MARK: Pixels

    pub pixel_move: Option<PixelMove>,
    pub pixel_clipboard: Option<PixelClipboard>,
    pub copied_layer: Option<CopiedLayer>,
    /// The platform clipboard, when one is attached (`NSPasteboard`).
    pub clipboard: Option<Box<dyn Clipboard>>,

    // MARK: Text & shape

    pub text_draft: Option<TextDraft>,
    pub text_defaults: LayerTextStyle,
    /// The text's style before the font menu started previewing faces on it.
    pub font_preview_original: Option<LayerTextStyle>,
    pub shape_kind: ShapeKind,
    /// Corner radius in pixels for rectangles the Shape tool draws; 0 keeps the corners square.
    pub shape_corner_radius: f64,
    /// A Line shape's thickness in document pixels.
    pub shape_line_width: f64,
    /// The shape being dragged out with the Shape tool, before it becomes a layer.
    pub shape_draft: Option<ShapeDraft>,

    // MARK: Adjustments, levels & filters

    pub adjustment_original: Option<LayerAdjustment>,
    pub adjustment_editing_id: Option<Id>,
    pub levels: Option<LevelsEdit>,
    pub hue_saturation: Option<HueSaturationEdit>,
    /// The newest preview request while one is already rendering.
    pub hue_saturation_pending: Option<HuePreviewJob>,
    /// The armed eyedropper while the Color Range (or Hue/Saturation) panel is open.
    pub hue_sample_mode: Option<HueSampleMode>,
    pub hue_targeting: bool,
    pub hue_target_drag: Option<HueTargetDrag>,
    /// The open filter (Filter menu), and the settings the next one starts from.
    pub filter_edit: Option<FilterEdit>,
    pub filter_settings: FilterSettings,
    /// The Camera Raw panel's settings (`filterSettings.cameraRaw`).
    pub camera_raw_settings: CameraRawSettings,
    /// The Camera Raw eyedropper drag in progress.
    pub camera_raw_drag: Option<CameraRawDrag>,

    // MARK: Effects

    /// The layer whose effects panel is open.
    pub effects_editing: Option<LayerEffectSelection>,
    pub effects_editing_original: Option<LayerEffects>,
    pub effect_selection: Option<LayerEffectSelection>,
    pub effects_previews: EffectsPreviewCache,

    // MARK: Dialogs, sheets & imports

    /// A dialog with its own zoomable preview (Export JPEG) is open: the View menu's zoom commands
    /// zoom that instead (`previewZoom`).
    pub preview_zoom: Option<Box<dyn Fn(PreviewZoomCommand)>>,
    pub shows_new_document: bool,
    pub shows_importer: bool,
    pub is_importing: bool,
    pub import_error: Option<String>,
    /// Images waiting for the import drain (`pendingImports`).
    pub pending_imports: Vec<ImportRequest>,
    /// The file within the request at the head of the queue that is being read.
    pub import_cursor: Option<ImportCursor>,
    /// Set while an import waits for a sheet's answer (a RAW develop, a PSD conversion).
    pub import_await: Option<ImportAwait>,
    /// What failed while the current import drain ran; joined into `import_error` at its end.
    pub import_failures: Vec<String>,
    pub shows_conversion_sheet: bool,
    pub conversion_request: Option<PSDConversionRequest>,
    pub(crate) conversion_cancelled: bool,
    /// Set while a PSD conversion sheet is up; the answer arrives through
    /// [`confirm_psd_conversions`](Self::confirm_psd_conversions) /
    /// [`finish_conversion`](Self::finish_conversion).
    pub(crate) conversion_answer_pending: bool,
    /// Tests assign this to answer the PSD conversion sheet without showing it.
    pub(crate) confirm_conversions:
        Option<Arc<dyn Fn(&[PSDConversion]) -> bool + Send + Sync>>,
    /// The RAW file being developed, and the settings the sheet is editing.
    pub raw_develop: Option<RawDevelop>,
    pub shows_raw_develop: bool,
    /// Tests assign this to develop without a sheet.
    pub(crate) confirm_raw_develop: Option<
        Arc<dyn Fn(&Path, RawDevelopSettings) -> Option<RawDevelopSettings> + Send + Sync>,
    >,
}

impl Default for EditorSession {
    fn default() -> Self {
        let guides = GuideSettings::default();
        Self {
            skips_initial_clipboard_canvas_size: false,
            document: None,
            canvas_focus_request: 0,
            shows_sample_ring: true,
            viewport: CanvasViewport::default(),
            tool: NavigationTool::Move,
            project_url: None,

            is_project_busy: false,
            shows_busy: false,

            active_layer_id: None,
            selected_layer_ids: HashSet::new(),
            collapsed_group_ids: HashSet::new(),
            renaming_layer_id: None,
            opacity_edit_layer_id: None,
            blend_preview: None,
            is_mask_selected: false,
            views_mask_alone: false,
            history: DocumentHistory::default(),

            transform_edit: None,
            transform_duplicate: None,
            distort_preview_cache: FxHashMap::default(),
            distort_effects_cache: FxHashMap::default(),
            shape_transform_preview_cache: FxHashMap::default(),
            locks_transform_ratio: true,
            transform_auto_select: false,
            shows_transform_controls: true,
            snap_guides: (Vec::new(), Vec::new()),

            snapping_enabled: true,
            shows_guides: guides.shows_guides,
            shows_rulers: guides.shows_rulers,
            shows_grid: guides.shows_grid,
            shows_pixel_grid: guides.shows_pixel_grid,
            snap_enabled: guides.snap_enabled,
            snap_to_guides: guides.snap_to_guides,
            snap_to_grid: guides.snap_to_grid,
            snap_to_layers: guides.snap_to_layers,
            snap_to_document_bounds: guides.snap_to_document_bounds,
            locks_guides: guides.locks_guides,
            layout_grid: guides.layout_grid,
            grid_appearance: guides.grid_appearance,
            guide_drag: None,

            crop_rect: None,
            crop_ratio_choice: "Free".to_string(),
            crop_error: None,

            brush_settings: BrushSettings::default(),
            spot_healing_mode: SpotHealingMode::default(),
            blur_mode: BlurToolMode::Liquify,
            brush_mode: BrushToolMode::Paint,
            parked_brush_tips: ParkedBrushTips::default(),
            last_brush_point: None,
            brush_anchor: None,
            brush_pointer: None,
            pending_opacity_digit: None,
            brush_error: None,
            brush_revision: 0,
            brush_stroke: None,
            warp_stroke: None,
            warp_layer: None,
            mask_distort_preview_cache: None,

            clone_source: None,
            clone_settings: CloneSettings::default(),
            clone_offset: None,

            mask_paint_white: false,
            background_color: PaletteColor::WHITE,
            gradient_settings: GradientSettings::default(),
            gradient_edit: None,
            color_picker: None,
            dialog_color_change: None,

            lasso_draft: None,
            lasso_kind: LassoKind::Freehand,
            marquee_kind: LassoKind::Rectangle,
            selection_mode_choice: SelectionMode::Replace,
            held_selection_mode: None,
            selection_move_origin: None,
            selection_antialiased: true,
            selection_feather_amount: 2,
            selection_expand_amount: 1,
            selection_contract_amount: 1,
            selection_amount_operation: None,
            wand_mode: WandMode::Wand,
            wand_settings: WandSettings::default(),
            object_selection_settings: ObjectSelectionSettings::default(),
            color_range: None,

            pixel_move: None,
            pixel_clipboard: None,
            copied_layer: None,
            clipboard: None,

            text_draft: None,
            text_defaults: LayerTextStyle::default(),
            font_preview_original: None,
            shape_kind: ShapeKind::Rectangle,
            shape_corner_radius: 0.0,
            shape_line_width: 4.0,
            shape_draft: None,

            adjustment_original: None,
            adjustment_editing_id: None,
            levels: None,
            hue_saturation: None,
            hue_saturation_pending: None,
            hue_sample_mode: None,
            hue_targeting: false,
            hue_target_drag: None,
            filter_edit: None,
            filter_settings: FilterSettings::default(),
            camera_raw_settings: CameraRawSettings::default(),
            camera_raw_drag: None,

            effects_editing: None,
            effects_editing_original: None,
            effect_selection: None,
            effects_previews: EffectsPreviewCache::default(),

            preview_zoom: None,
            shows_new_document: false,
            shows_importer: false,
            is_importing: false,
            import_error: None,
            pending_imports: Vec::new(),
            import_cursor: None,
            import_await: None,
            import_failures: Vec::new(),
            shows_conversion_sheet: false,
            conversion_request: None,
            conversion_cancelled: false,
            conversion_answer_pending: false,
            confirm_conversions: None,
            raw_develop: None,
            shows_raw_develop: false,
            confirm_raw_develop: None,
        }
    }
}

impl EditorSession {
    pub fn new() -> Self {
        Self::default()
    }

    // MARK: - Guards

    /// Whether a layer command may run (`canEditLayers`): no draft, panel or gesture holds the
    /// document, and one is open at all.
    pub fn can_edit_layers(&self) -> bool {
        // The Swift read `showsBusy` here so the UI re-evaluated its observation; the flag is part
        // of the state the caller reads, so the read is kept.
        let _ = self.shows_busy;
        self.selection_amount_operation.is_none()
            && self.color_range.is_none()
            && self.text_draft.is_none()
            && self.document.is_some()
            && self.brush_stroke.is_none()
            && self.warp_stroke.is_none()
            && !self.is_project_busy
            && !self.is_importing
            && !self.shows_new_document
            && !self.shows_importer
            && self.renaming_layer_id.is_none()
            && self.transform_edit.is_none()
            && self.crop_rect.is_none()
            && self.gradient_edit.is_none()
            && self.pixel_move.is_none()
            && self.hue_saturation.is_none()
            && self.levels.is_none()
            && self.filter_edit.is_none()
            && self.adjustment_editing_id.is_none()
    }

    /// Whether Undo and Redo may run (`canUseHistory`).
    pub fn can_use_history(&self) -> bool {
        let _ = self.shows_busy;
        self.selection_amount_operation.is_none()
            && self.color_range.is_none()
            && self.text_draft.is_none()
            && !self.is_project_busy
            && !self.is_importing
            && self.brush_stroke.is_none()
            && self.warp_stroke.is_none()
            && self.levels.is_none()
            && !self.shows_new_document
            && !self.shows_importer
            && self.renaming_layer_id.is_none()
            && self.import_error.is_none()
            && self.transform_edit.is_none()
            && !self.shows_conversion_sheet
    }

    /// Undo is available: the history has a step, or a pending gradient would be discarded first.
    pub fn can_undo(&self) -> bool {
        self.can_use_history() && (self.history.can_undo() || self.gradient_edit.is_some())
    }

    pub fn can_redo(&self) -> bool {
        self.can_use_history() && self.history.can_redo()
    }

    /// Whether a project-level operation (save, open, export, import) may begin
    /// (`canStartProjectOperation`).
    pub fn can_start_project_operation(&self) -> bool {
        let _ = self.shows_busy;
        self.selection_amount_operation.is_none()
            && self.color_range.is_none()
            && self.text_draft.is_none()
            && !self.is_project_busy
            && !self.is_importing
            && self.brush_stroke.is_none()
            && self.warp_stroke.is_none()
            && self.levels.is_none()
            && !self.shows_new_document
            && !self.shows_importer
            && self.renaming_layer_id.is_none()
            && self.import_error.is_none()
            && self.adjustment_editing_id.is_none()
            && !self.shows_conversion_sheet
    }

    /// The active layer, if the document has it.
    pub fn active_layer(&self) -> Option<&ImageLayer> {
        let id = self.active_layer_id?;
        self.document.as_ref()?.layers.iter().find(|layer| layer.id == id)
    }

    /// The current selection: `nil` means nothing is selected (edits run everywhere), an explicit
    /// empty selection means *touch nothing*.
    pub fn selection(&self) -> Option<&DocumentSelection> {
        self.document.as_ref().and_then(|document| document.selection.as_ref())
    }

    /// The layer whose mask the canvas is showing by itself; nil for the ordinary composite.
    pub fn mask_alone_layer(&self) -> Option<&ImageLayer> {
        if !(self.views_mask_alone && self.is_mask_selected) {
            return None;
        }
        let layer = self.active_layer()?;
        layer.mask.as_ref()?;
        Some(layer)
    }

    /// Whether the open project has unsaved changes (`isModified`).
    pub fn is_modified(&self) -> bool {
        self.history.is_modified()
    }

    // MARK: - History

    /// Nestable transaction boundary (`beginEdit(_:)`); a tool groups a complete gesture with it.
    pub fn begin_edit(&mut self, name: &str) {
        self.history.begin(name, self.document.as_ref(), self.active_layer_id);
    }

    pub fn end_edit(&mut self) {
        self.history.end(self.document.as_ref(), self.active_layer_id);
    }

    /// Undo (⌘Z). Like Photoshop, the first Undo discards a pending gradient.
    pub fn undo(&mut self) {
        if self.discard_pending_gradient() {
            return;
        }
        if !self.can_undo() {
            return;
        }
        let Some(snapshot) = self.history.undo() else {
            return;
        };
        self.restore(snapshot);
    }

    pub fn redo(&mut self) {
        if !self.can_redo() {
            return;
        }
        let Some(snapshot) = self.history.redo() else {
            return;
        };
        self.restore(snapshot);
    }

    /// `restore(_:)`: installs a history snapshot, closing drafts that no longer fit and keeping the
    /// mask target when the same layer is still active with a mask.
    fn restore(&mut self, snapshot: Snapshot) {
        self.cancel_crop();
        self.cancel_gradient();
        let changed_canvas = self.document.as_ref().map(|document| document.id) != snapshot.document.as_ref().map(|document| document.id);
        let keep_mask_target = self.is_mask_selected && self.active_layer_id == snapshot.active_layer_id;
        self.document = snapshot.document;
        self.set_active_layer(snapshot.active_layer_id);
        self.is_mask_selected = keep_mask_target && self.active_layer().is_some_and(|layer| layer.mask.is_some());
        if changed_canvas {
            if let Some(size) = self.document.as_ref().map(|document| document.size()) {
                self.viewport.fit(size);
            }
        }
    }

    // MARK: - Layer selection

    /// The `didSet` on `activeLayerID`: changing it clears the mask target, and the selection
    /// becomes just this layer.
    pub fn set_active_layer(&mut self, id: Option<Id>) {
        if self.active_layer_id != id {
            self.is_mask_selected = false;
        }
        self.active_layer_id = id;
        self.selected_layer_ids = id.map(|id| HashSet::from([id])).unwrap_or_default();
    }

    /// `selectLayer(_:)`: closes the effects panel, finishes an open text draft, and moves the
    /// selection to one layer.
    pub fn select_layer(&mut self, id: Option<Id>) {
        self.effect_selection = None;
        if id != self.active_layer_id && !self.finish_text() {
            return;
        }
        if self.brush_stroke.is_some() || self.warp_stroke.is_some() || self.levels.is_some() {
            return;
        }
        if id != self.active_layer_id {
            self.commit_transform();
            self.resolve_gradient();
        }
        self.set_active_layer(id);
    }

    /// `selectLayers(_:primary:)`: the multi-layer selection, filtered to layers the document still
    /// has, with the primary (or the first valid id) becoming the active layer.
    pub fn select_layers(&mut self, ids: impl IntoIterator<Item = Id>, primary: Option<Id>) {
        self.effect_selection = None;
        let ids: HashSet<Id> = ids.into_iter().collect();
        if ids != self.selected_layer_ids && !self.finish_text() {
            return;
        }
        if self.brush_stroke.is_some() {
            return;
        }
        let valid: HashSet<Id> = match self.document.as_ref() {
            Some(document) => {
                let present: HashSet<Id> = document.layers.iter().map(|layer| layer.id).collect();
                ids.intersection(&present).copied().collect()
            }
            None => HashSet::new(),
        };
        if valid != self.selected_layer_ids {
            self.commit_transform();
            self.resolve_gradient();
        }
        let primary = primary
            .filter(|id| valid.contains(id))
            .or_else(|| valid.iter().copied().next());
        self.set_active_layer(primary);
        self.selected_layer_ids = valid;
    }

    // MARK: - Tools

    /// `symbol(for:)`: the tool rail's icon, which follows the mode a tool is in.
    pub fn symbol(&self, tool: NavigationTool) -> String {
        if tool == NavigationTool::Brush && self.brush_mode == BrushToolMode::Erase {
            "eraser".to_string()
        } else {
            tool.symbol().to_string()
        }
    }

    /// `selectTool(_:)`: switches tools, closing the gestures the old one had open, committing a
    /// pending transform and gradient, and handing the brush tip to the new tool's family.
    pub fn select_tool(&mut self, value: NavigationTool) {
        if self.tool != value && !self.finish_text() {
            return;
        }
        if self.is_project_busy || self.brush_stroke.is_some() || self.warp_stroke.is_some() || self.levels.is_some() {
            return;
        }
        if self.tool != value {
            self.commit_transform();
            self.cancel_crop();
            self.resolve_gradient();
            self.cancel_lasso();
            self.cancel_shape();
        }
        self.hand_off_brush_tips(value);
        self.tool = value;
        // Swift warmed `MetalBrushCoverage.shared` here; the port's brush raster allocates on demand.
        if value == NavigationTool::Crop {
            self.start_crop_tool();
        }
    }

    /// Tab steps the current tool through its own modes — the setting at the left of its tool bar.
    /// Tools without modes (Move, Crop, Type, Eyedropper, Hand, Zoom) ignore it (`cycleToolMode`).
    pub fn cycle_tool_mode(&mut self) {
        if self.is_project_busy || self.brush_stroke.is_some() || self.warp_stroke.is_some() {
            return;
        }
        match self.tool {
            NavigationTool::Marquee => self.toggle_marquee_kind(),
            NavigationTool::Wand => self.wand_mode = next_case(&WandMode::ALL, self.wand_mode),
            NavigationTool::Lasso => self.toggle_lasso_kind(),
            NavigationTool::Shape => self.toggle_shape_kind(),
            NavigationTool::Brush => self.brush_mode = next_case(&BrushToolMode::ALL, self.brush_mode),
            NavigationTool::Blur => self.blur_mode = next_case(&BlurToolMode::ALL, self.blur_mode),
            NavigationTool::SpotHealing => {
                self.spot_healing_mode = next_case(&SpotHealingMode::ALL, self.spot_healing_mode);
            }
            NavigationTool::CloneStamp => {
                self.clone_settings.sample_all_layers = !self.clone_settings.sample_all_layers;
            }
            NavigationTool::Gradient => {
                self.gradient_settings.shape = next_case(&GradientShape::ALL, self.gradient_settings.shape);
            }
            _ => {}
        }
    }

    // MARK: - Layers

    /// `nextLayerName()`: the first unused "Layer N".
    pub fn next_layer_name(&self) -> String {
        let names: HashSet<&str> = self
            .document
            .as_ref()
            .map(|document| document.layers.iter().map(|layer| layer.name.as_str()).collect())
            .unwrap_or_default();
        let mut number = 1;
        while names.contains(format!("Layer {number}").as_str()) {
            number += 1;
        }
        format!("Layer {number}")
    }

    /// `addBlankLayer()`: a new empty layer above the active one — inside the selected folder, at
    /// the top of its contents.
    pub fn add_blank_layer(&mut self) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let name = self.next_layer_name();
        let mut layer = ImageLayer::blank(name, document.size());
        let active_is_group = self.active_layer().is_some_and(|layer| layer.is_group);
        layer.parent_id = if active_is_group {
            self.active_layer_id
        } else {
            self.active_layer().and_then(|layer| layer.parent_id)
        };
        if let Some(parent) = layer.parent_id {
            self.collapsed_group_ids.remove(&parent);
        }
        let mut insertion = self
            .active_layer_id
            .and_then(|id| document.layers.iter().position(|layer| layer.id == id))
            .map(|index| index + 1)
            .unwrap_or(document.layers.len());
        // With a folder selected the layer goes to the top of the folder: just above its last
        // (topmost) contents.
        if active_is_group {
            if let Some(folder) = self.active_layer_id {
                let parents: HashMap<Id, Option<Id>> =
                    document.layers.iter().map(|layer| (layer.id, layer.parent_id)).collect();
                let is_inside = |id: Id| -> bool {
                    let mut parent = parents.get(&id).copied().flatten();
                    let mut steps = 0;
                    while let Some(current) = parent {
                        if current == folder {
                            return true;
                        }
                        parent = parents.get(&current).copied().flatten();
                        steps += 1;
                        if steps >= 64 {
                            break;
                        }
                    }
                    false
                };
                if let Some(topmost) = document.layers.iter().rposition(|layer| is_inside(layer.id)) {
                    insertion = insertion.max(topmost + 1);
                }
            }
        }
        self.begin_edit("New Blank Layer");
        if let Some(document) = self.document.as_mut() {
            document.layers.insert(insertion, layer.clone());
        }
        self.set_active_layer(Some(layer.id));
        self.end_edit();
    }

    /// `deleteLayer(_:)`: deletes one layer as one undo step. When it supplies a live mask to a
    /// layer that stays, this returns having done nothing: the UI shows
    /// [`live_mask_delete_alert`](Self::live_mask_delete_alert) and answers it with
    /// [`delete_with_live_mask_choice`](Self::delete_with_live_mask_choice).
    pub fn delete_layer(&mut self, id: Id) {
        if !self.can_edit_layers() {
            return;
        }
        if !self.document.as_ref().is_some_and(|document| document.layers.iter().any(|layer| layer.id == id)) {
            return;
        }
        if !self.live_mask_delete_targets(&[id]).is_empty() {
            return;
        }
        self.finish_deleting_layer(id, &HashMap::new());
    }

    pub fn delete_active_layer(&mut self) {
        if let Some(id) = self.active_layer_id {
            self.delete_layer(id);
        }
    }

    /// Deletes every selected layer as one undo step (a selected folder takes its contents); with one
    /// layer selected, just that one (`deleteSelectedLayers`).
    pub fn delete_selected_layers(&mut self) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        // Captured first: deleting moves the active layer, which resets the selection.
        let ids: Vec<Id> = document
            .layers
            .iter()
            .map(|layer| layer.id)
            .filter(|id| self.selected_layer_ids.contains(id))
            .collect();
        if ids.len() <= 1 {
            self.delete_active_layer();
            return;
        }
        if !self.live_mask_delete_targets(&ids).is_empty() {
            return;
        }
        self.finish_deleting_layers(&ids, &HashMap::new());
    }

    /// `renameLayer(_:to:)`: trims the name and stores it as one undo step.
    pub fn rename_layer(&mut self, id: Id, name: &str) {
        let name = name.trim();
        if self.is_project_busy || self.is_importing || name.is_empty() {
            return;
        }
        let Some(index) = self.document.as_ref().and_then(|document| document.layers.iter().position(|layer| layer.id == id)) else {
            return;
        };
        self.begin_edit("Rename Layer");
        if let Some(document) = self.document.as_mut() {
            document.layers[index].name = name.to_string();
        }
        self.end_edit();
    }

    /// `toggleLayerVisibility(_:)`: the eye clicked, one undo step named for what it did.
    pub fn toggle_layer_visibility(&mut self, id: Id) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(index) = self.document.as_ref().and_then(|document| document.layers.iter().position(|layer| layer.id == id)) else {
            return;
        };
        let name = if self.document.as_ref().is_some_and(|document| document.layers[index].is_visible) {
            "Hide Layer"
        } else {
            "Show Layer"
        };
        self.begin_edit(name);
        if let Some(document) = self.document.as_mut() {
            document.layers[index].is_visible = !document.layers[index].is_visible;
        }
        self.end_edit();
    }

    /// Photoshop's eye swipe: pressing an eye shows or hides that layer, and dragging over other
    /// eyes gives them the same state, all as one undo step (`beginEdit` at the press, `endEdit`
    /// when the button comes up). Returns the state the swipe is giving the layers.
    pub fn begin_visibility_swipe(&mut self, id: Id) -> Option<bool> {
        if !self.can_edit_layers() {
            return None;
        }
        let visible = !self.document.as_ref()?.layers.iter().find(|layer| layer.id == id)?.is_visible;
        self.begin_edit(if visible { "Show Layer" } else { "Hide Layer" });
        self.set_visibility_in_swipe(id, visible);
        Some(visible)
    }

    pub fn set_visibility_in_swipe(&mut self, id: Id, visible: bool) {
        let Some(index) = self.document.as_ref().and_then(|document| document.layers.iter().position(|layer| layer.id == id)) else {
            return;
        };
        if self.document.as_ref().is_some_and(|document| document.layers[index].is_visible == visible) {
            return;
        }
        if let Some(document) = self.document.as_mut() {
            document.layers[index].is_visible = visible;
        }
    }

    pub fn end_visibility_swipe(&mut self) {
        self.end_edit();
    }

    /// `reorderLayers(from:to:)`: `offsets` are indices into the top-to-bottom list the Layers panel
    /// shows; the document stores bottom-to-top, so the list is reversed before and after the move
    /// (see [`crate::layers::reordered_layers`], which pins the conversion).
    pub fn reorder_layers(&mut self, offsets: &[usize], destination: usize) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let Some(next) = crate::layers::reordered_layers(&document.layers, offsets, destination) else {
            return;
        };
        self.begin_edit("Reorder Layers");
        if let Some(document) = self.document.as_mut() {
            document.layers = next;
        }
        self.end_edit();
    }

    /// `canMoveActiveLayer(by:)`: whether the active layer has a sibling `offset` places away.
    pub fn can_move_active_layer(&self, offset: i32) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        let Some(active) = self.active_layer() else {
            return false;
        };
        let siblings: Vec<&ImageLayer> = self
            .document
            .as_ref()
            .map(|document| {
                document
                    .layers
                    .iter()
                    .filter(|layer| layer.parent_id == active.parent_id)
                    .collect()
            })
            .unwrap_or_default();
        let Some(index) = siblings.iter().position(|layer| layer.id == active.id) else {
            return false;
        };
        let target = index as i64 + offset as i64;
        target >= 0 && (target as usize) < siblings.len()
    }

    /// `moveActiveLayer(by:)`: swaps the active layer with the sibling `offset` places away.
    pub fn move_active_layer(&mut self, offset: i32) {
        if !self.can_move_active_layer(offset) {
            return;
        }
        let Some(active) = self.active_layer() else {
            return;
        };
        let (parent_id, active_id) = (active.parent_id, active.id);
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let siblings: Vec<Id> = document
            .layers
            .iter()
            .filter(|layer| layer.parent_id == parent_id)
            .map(|layer| layer.id)
            .collect();
        let Some(index) = siblings.iter().position(|id| *id == active_id) else {
            return;
        };
        let target = (index as i64 + offset as i64) as usize;
        let sibling_id = siblings[target];
        let (Some(a), Some(b)) = (
            document.layers.iter().position(|layer| layer.id == active_id),
            document.layers.iter().position(|layer| layer.id == sibling_id),
        ) else {
            return;
        };
        self.begin_edit("Reorder Layers");
        if let Some(document) = self.document.as_mut() {
            document.layers.swap(a, b);
        }
        self.end_edit();
    }

    /// `descendantIDs(of:)`: everything inside a folder, however deep.
    pub fn descendant_ids(&self, id: Id) -> HashSet<Id> {
        let mut children: HashMap<Option<Id>, Vec<Id>> = HashMap::new();
        if let Some(document) = self.document.as_ref() {
            for layer in &document.layers {
                children.entry(layer.parent_id).or_default().push(layer.id);
            }
        }
        let mut result = HashSet::new();
        let mut pending = vec![id];
        while let Some(parent) = pending.pop() {
            if let Some(kids) = children.get(&Some(parent)) {
                for child in kids {
                    if result.insert(*child) {
                        pending.push(*child);
                    }
                }
            }
        }
        result
    }

    // MARK: - Document

    /// `createDocument(width:height:emptyLayer:)`: File > New, one undo step named "New Canvas".
    /// `empty_layer` starts the canvas with a selected blank "Layer 1", as File > New does.
    pub fn create_document(&mut self, width: usize, height: usize, empty_layer: bool) {
        if self.is_project_busy || self.is_importing {
            return;
        }
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return;
        }
        self.commit_transform();
        self.begin_edit("New Canvas");
        let mut document = CanvasDocument::new(width, height);
        let layer = if empty_layer {
            Some(ImageLayer::blank("Layer 1", document.size()))
        } else {
            None
        };
        if let Some(layer) = layer.as_ref() {
            document.layers = vec![layer.clone()];
        }
        self.document = Some(document);
        self.set_active_layer(layer.map(|layer| layer.id));
        self.renaming_layer_id = None;
        if let Some(size) = self.document.as_ref().map(|document| document.size()) {
            self.viewport.fit(size);
        }
        self.shows_new_document = false;
        self.end_edit();
    }

    // MARK: - Canvas refresh

    /// The port of the `refreshCanvasPreview` closure the canvas view assigned in Swift: bumping the
    /// revision is what makes the canvas redraw, so every caller of the closure calls this instead.
    pub fn refresh_canvas_preview(&mut self) {
        self.brush_revision += 1;
    }
}

/// `CaseIterable`'s `next`: the case after `value`, wrapping at the end.
fn next_case<T: Copy + PartialEq>(all: &[T], value: T) -> T {
    let index = all.iter().position(|case| *case == value).unwrap_or(0);
    all[(index + 1) % all.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_layers() -> EditorSession {
        let mut session = EditorSession::default();
        let mut document = CanvasDocument::new(64, 64);
        // Bottom to top: Bottom, Middle, Top.
        document.layers = vec![
            ImageLayer::blank("Bottom", document.size()),
            ImageLayer::blank("Middle", document.size()),
            ImageLayer::blank("Top", document.size()),
        ];
        let top = document.layers[2].id;
        session.document = Some(document);
        session.set_active_layer(Some(top));
        session
    }

    #[test]
    fn reorder_layers_moves_in_the_top_to_bottom_list_order() {
        let mut session = session_with_layers();
        let bottom = session.document.as_ref().expect("document").layers[0].id;
        // The list shows Top, Middle, Bottom; drag Bottom (list index 2) to the top (destination 0).
        session.reorder_layers(&[2], 0);
        let names: Vec<&str> = session
            .document
            .as_ref()
            .expect("document")
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        // Bottom-to-top storage: the moved layer is now on top.
        assert_eq!(names, ["Middle", "Top", "Bottom"]);
        // And the same offset lands the same way regardless of the list's direction.
        let mut session = session_with_layers();
        // Move Top (list index 0) to the bottom (destination 3).
        session.reorder_layers(&[0], 3);
        let names: Vec<&str> = session
            .document
            .as_ref()
            .expect("document")
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        assert_eq!(names, ["Top", "Bottom", "Middle"]);
    }

    #[test]
    fn reorder_layers_is_one_named_undo_step() {
        let mut session = session_with_layers();
        session.reorder_layers(&[2], 0);
        assert_eq!(session.history.undo_name(), "Reorder Layers");
        session.undo();
        let names: Vec<&str> = session
            .document
            .as_ref()
            .expect("document")
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        assert_eq!(names, ["Bottom", "Middle", "Top"]);
    }

    #[test]
    fn move_active_layer_stays_among_its_own_siblings() {
        let mut session = session_with_layers();
        let middle = session.document.as_ref().expect("document").layers[1].id;
        session.set_active_layer(Some(middle));
        assert!(session.can_move_active_layer(1));
        assert!(session.can_move_active_layer(-1));
        session.move_active_layer(1);
        let names: Vec<&str> = session
            .document
            .as_ref()
            .expect("document")
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        assert_eq!(names, ["Bottom", "Top", "Middle"]);
        // At the end of the siblings there is nowhere to go.
        assert!(!session.can_move_active_layer(1));
        assert!(session.can_move_active_layer(-1));
    }

    #[test]
    fn move_active_layer_ignores_layers_in_other_folders() {
        let mut session = session_with_layers();
        let folder_id = {
            let document = session.document.as_mut().expect("document");
            let mut folder = ImageLayer::blank("Folder", document.size());
            folder.is_group = true;
            let id = folder.id;
            let mut child = ImageLayer::blank("Child", document.size());
            child.parent_id = Some(id);
            document.layers.push(folder);
            document.layers.push(child);
            id
        };
        let child_id = session.document.as_ref().expect("document").layers.last().expect("child").id;
        session.set_active_layer(Some(child_id));
        // The child's only sibling is nothing: the folder has one child.
        assert!(!session.can_move_active_layer(1));
        assert!(!session.can_move_active_layer(-1));
        session.set_active_layer(Some(folder_id));
        // The folder is the topmost root layer: it can move down among its siblings, not up.
        assert!(!session.can_move_active_layer(1));
        assert!(session.can_move_active_layer(-1));
    }

    #[test]
    fn next_layer_name_skips_names_in_use() {
        let mut session = session_with_layers();
        assert_eq!(session.next_layer_name(), "Layer 1");
        let document = session.document.as_mut().expect("document");
        document.layers.push(ImageLayer::blank("Layer 1", document.size()));
        document.layers.push(ImageLayer::blank("Layer 2", document.size()));
        assert_eq!(session.next_layer_name(), "Layer 3");
    }

    #[test]
    fn add_blank_layer_names_and_selects_the_new_layer() {
        let mut session = session_with_layers();
        session.add_blank_layer();
        let document = session.document.as_ref().expect("document");
        assert_eq!(document.layers.len(), 4);
        let added = document.layers.last().expect("new layer");
        assert_eq!(added.name, "Layer 1");
        assert_eq!(session.active_layer_id, Some(added.id));
        assert_eq!(session.history.undo_name(), "New Blank Layer");
    }

    #[test]
    fn rename_and_visibility_are_named_undo_steps() {
        let mut session = session_with_layers();
        let top = session.document.as_ref().expect("document").layers[2].id;
        session.rename_layer(top, "  Sky  ");
        assert_eq!(session.document.as_ref().expect("document").layers[2].name, "Sky");
        assert_eq!(session.history.undo_name(), "Rename Layer");
        session.toggle_layer_visibility(top);
        assert!(!session.document.as_ref().expect("document").layers[2].is_visible);
        assert_eq!(session.history.undo_name(), "Hide Layer");
        let state = session.begin_visibility_swipe(top);
        assert_eq!(state, Some(true));
        assert!(session.document.as_ref().expect("document").layers[2].is_visible);
        session.end_visibility_swipe();
        assert_eq!(session.history.undo_name(), "Show Layer");
    }

    #[test]
    fn history_transactions_carry_the_swift_names() {
        let mut session = session_with_layers();
        session.begin_edit("Move Selection");
        if let Some(document) = session.document.as_mut() {
            document.layers[0].name = "Moved".to_string();
        }
        session.end_edit();
        assert_eq!(session.history.undo_name(), "Move Selection");
        session.undo();
        assert_eq!(session.document.as_ref().expect("document").layers[0].name, "Bottom");
        assert_eq!(session.history.redo_name(), "Move Selection");
    }

    #[test]
    fn descendant_ids_walks_the_whole_folder() {
        let mut session = session_with_layers();
        let (folder, child, grandchild) = {
            let document = session.document.as_mut().expect("document");
            let mut folder = ImageLayer::blank("Folder", document.size());
            folder.is_group = true;
            let folder_id = folder.id;
            let mut child = ImageLayer::blank("Child", document.size());
            child.parent_id = Some(folder_id);
            let child_id = child.id;
            let mut grandchild = ImageLayer::blank("Grandchild", document.size());
            grandchild.parent_id = Some(child_id);
            let grandchild_id = grandchild.id;
            document.layers.push(folder);
            document.layers.push(child);
            document.layers.push(grandchild);
            (folder_id, child_id, grandchild_id)
        };
        let descendants = session.descendant_ids(folder);
        assert_eq!(descendants.len(), 2);
        assert!(descendants.contains(&child));
        assert!(descendants.contains(&grandchild));
        assert!(!descendants.contains(&folder));
    }

    #[test]
    fn delete_layer_is_one_undo_step_and_moves_the_selection() {
        let mut session = session_with_layers();
        let top = session.document.as_ref().expect("document").layers[2].id;
        session.delete_layer(top);
        let names: Vec<&str> = session
            .document
            .as_ref()
            .expect("document")
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        assert_eq!(names, ["Bottom", "Middle"]);
        assert_eq!(session.active_layer_id, Some(session.document.as_ref().expect("document").layers[1].id));
        assert_eq!(session.history.undo_name(), "Delete Layer");
    }

    #[test]
    fn guards_refuse_layer_commands_while_a_draft_is_open() {
        let mut session = session_with_layers();
        assert!(session.can_edit_layers());
        session.crop_rect = Some(Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!(!session.can_edit_layers());
        session.crop_rect = None;
        session.shows_new_document = true;
        assert!(!session.can_edit_layers());
        assert!(!session.can_start_project_operation());
        session.shows_new_document = false;
        assert!(session.can_start_project_operation());
    }

    #[test]
    fn create_document_installs_a_canvas_and_an_optional_layer() {
        let mut session = EditorSession::default();
        session.create_document(128, 64, true);
        let document = session.document.as_ref().expect("document");
        assert_eq!((document.width, document.height), (128, 64));
        assert_eq!(document.layers.len(), 1);
        assert_eq!(document.layers[0].name, "Layer 1");
        assert_eq!(session.active_layer_id, Some(document.layers[0].id));
        assert_eq!(session.history.undo_name(), "New Canvas");
        // Out-of-range sizes are refused.
        let mut session = EditorSession::default();
        session.create_document(0, 64, false);
        assert!(session.document.is_none());
    }

    #[test]
    fn symbol_follows_the_brush_mode() {
        let mut session = EditorSession::default();
        assert_eq!(session.symbol(NavigationTool::Brush), "paintbrush.pointed");
        session.brush_mode = BrushToolMode::Erase;
        assert_eq!(session.symbol(NavigationTool::Brush), "eraser");
        assert_eq!(session.symbol(NavigationTool::Move), "arrow.up.left.and.arrow.down.right");
    }
}
