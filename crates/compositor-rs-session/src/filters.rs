//! The Filter menu and the color editors it shares with the Image menu's adjustments.
//!
//! Ports the `extension EditorSession` of `Document/Filters.swift` (`beginFilter`, `updateFilter`,
//! `renderFilterPreview`, `cancelFilter`, `commitFilter`, `commitBackgroundMask`), the session half of
//! `Document/Levels.swift` and `Document/LevelsAutomatic.swift` (`LevelsEdit` and `beginLevels` /
//! `updateLevels` / `cancelLevels` / `commitLevels`, `autoLevels`, `sampleLevels`),
//! the session half of `Document/HueSaturation.swift` (`HueSaturationEdit` and `beginHueSaturation` /
//! `updateHueSaturation` / `commitHueSaturation` / `cancelHueSaturation`, the eyedroppers and the
//! targeted-adjustment drag), and `Document/AdjustmentEditing.swift`'s editors where they are not in
//! `adjustments.rs` (the `FilterEdit`/`LevelsEdit`/`HueSaturationEdit` types themselves).
//!
//! **Async → sync.** Swift ran every preview in a `Task` and awaited it on commit. The port renders
//! through [`PreviewJobs`] — one worker per editor, the same "the newest request waits for the render in
//! flight" rule as `renderFilterPreview`, results polled by [`EditorSession::poll_filter_previews`] —
//! and a commit that needs the preview (`commitFilter`'s automatic filters) saturates the worker instead
//! of awaiting the task.
//!
//! The pixel work is `compositor_rs_pixels::filters` (`PixelFilter`, `CameraRawScope`, the trim after a
//! blur), `compositor_rs_pixels::adjustments` (`LevelsFilter`, `HueSaturationFilter`) and
//! `compositor_rs_render`'s `MaskClip` for carrying a mask onto a grown layer.

use std::sync::Arc;

use compositor_rs_core::document::ImageLayer;
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::image_ops::{blur_margin, FilterJob, FilterKind, FilterSettings};
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_adjustment::{
    AdjustmentColor, ColorRange, GradientMapSettings, HueSaturationSettings, LevelsSettings, RangeAdjustment,
};
use compositor_rs_core::layer_mask::LayerMask;
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::limits;
use compositor_rs_core::palette::{ColorPickerTarget, PickerHSB};
use compositor_rs_core::selection::SelectionClip;
use compositor_rs_core::{CoreError, Gray8Image, Id, Rgba8Image};
use compositor_rs_pixels::adjustments::{
    blend_gray_through_selection, levels_sampling, AdjustedPixels, HueSaturationFilter, HueSaturationJob,
    LevelsAuto, LevelsFilter, LevelsJob, LevelsSample, PixelAdjust,
};
use compositor_rs_pixels::camera_raw::{CameraRawClipping, CameraRawScope, CameraRawSettings};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_rs_pixels::filters::{CameraRawInputs, PixelFilter};
use compositor_rs_pixels::masks::SubjectRemoval;
use compositor_rs_pixels::raster::{rect_applying, Raster};
use compositor_rs_render::live_mask_renderer::MaskClip;

use crate::camera_raw::{straight_image_pixel, CameraRawPanel};
use crate::previews::{PreviewCancel, PreviewJobs};
use crate::selection::HueSampleMode;
use crate::EditorSession;

/// `FilterEdit.previewLimit`: previews render from a copy no larger than this on its longest side.
pub const FILTER_PREVIEW_LIMIT: f64 = 2048.0;

/// `HueSaturationEdit.previewLimit`: full size for anything ordinary — 8000 pixels on a side — so the
/// canvas shows the real thing rather than a coarse copy stretched to fit.
pub const HUE_SATURATION_PREVIEW_LIMIT: f64 = 8000.0;

/// `ProjectError.invalid`'s message.
pub(crate) fn invalid_project() -> CoreError {
    CoreError::Message("This is not a valid Compositor project, or its metadata is damaged.".to_string())
}

/// `ProjectError.tooLarge`'s message.
pub(crate) fn project_too_large() -> CoreError {
    CoreError::Message(format!(
        "This project exceeds the supported canvas, layer, file-size, or {}-megapixel document limit.",
        limits::document_budget_megapixels()
    ))
}

/// `ExportError.render`'s message.
pub(crate) fn render_failed() -> CoreError {
    CoreError::Message("The canvas could not be rendered. Try a smaller canvas.".to_string())
}

/// Whether two assets show the same pixels (`a === b` on the backing `CGImage`s).
pub(crate) fn same_pixels(lhs: &PixelImage, rhs: &PixelImage) -> bool {
    match (lhs, rhs) {
        (PixelImage::Rgba(a), PixelImage::Rgba(b)) => Arc::ptr_eq(a, b),
        (PixelImage::Gray(a), PixelImage::Gray(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// The coalescing rule of `renderFilterPreview`/`renderLevelsPreview`/`renderPendingPreview`: a render
/// already in flight is never cancelled by a later change — the change waits for it and the *newest*
/// pending job renders when it lands (cancelling starved the preview during a drag, because slider
/// changes arrive faster than a render completes).
pub(crate) fn take_renderable<J>(pending: &mut Option<J>, rendering: bool) -> Option<J> {
    if rendering {
        return None;
    }
    pending.take()
}

/// One finished filter preview (`preparedPreview`, the scope the Camera Raw panel shows and the error a
/// failed render reported).
#[derive(Clone)]
pub struct FilterPreviewResult {
    /// `preparedPreview`: `None` when the render failed.
    pub image: Option<Rgba8Image>,
    pub scope: Option<CameraRawScope>,
    /// Where the preview goes: the grown layer it was made from, or nil for the layer's own place.
    pub placement: Option<LayerTransform>,
    /// `previewSourceVersion` when the render started, so a result made before the layer grew for a
    /// bigger blur does not claim the current settings.
    pub source_version: u64,
    /// The settings the render was made with.
    pub settings: Option<FilterSettings>,
    pub error: Option<String>,
}

/// A queued filter render: the job, the Camera Raw state it needs, and where its result goes.
struct FilterPreviewRequest {
    job: FilterJob,
    camera_raw: CameraRawSettings,
    placement: Option<LayerTransform>,
    source_version: u64,
    settings: FilterSettings,
}

/// `FilterEdit`: a filter's live preview, one undo step on OK.
pub struct FilterEdit {
    pub kind: FilterKind,
    pub layer_id: Id,
    pub original: ImportedImage,
    pub transform: LayerTransform,
    pub selection: Option<SelectionClip>,
    pub mapping: AffineTransform,
    pub preview_source: Rgba8Image,
    pub preview_scale: f64,
    pub preview_mapping: AffineTransform,
    /// A filter reaching past the layer's edge works on the layer's pixels padded out.
    pub grown_image: Option<Rgba8Image>,
    pub grown_transform: Option<LayerTransform>,
    /// How far the padding reaches beyond the layer on every side, in layer pixels.
    pub grown_margin: f64,
    pub settings: FilterSettings,
    /// Camera Raw's grade; `FilterSettings` cannot carry it (`compositor_rs_core::image_ops` documents why).
    pub camera_raw: CameraRawSettings,
    pub preview: bool,
    pub committing: bool,
    pub preview_error: Option<String>,
    pub preparing: bool,
    /// Add Noise's grain, fixed while the panel is open so changing Amount doesn't reshuffle it.
    pub seed: u32,
    /// Camera Raw's panel state: section visibility, the curve/mixer/grading editors and the drags.
    pub panel: CameraRawPanel,
    /// Vignette on an empty layer: the canvas it frames and fills.
    pub canvas: Option<Rect>,
    /// The layer had no pixels yet; the filter started it from clear ones.
    pub started_empty: bool,
    /// Where `prepared_preview` goes; the last preview stays up where it belongs until the next one
    /// replaces it.
    pub prepared_transform: Option<LayerTransform>,
    /// The grown layer the pending render is made from.
    pub pending_transform: Option<LayerTransform>,
    /// Reject a render started before the blur's padded pixel grid changed.
    pub preview_source_version: u64,
    /// The settings `prepared_preview` was made with, for the automatic filters.
    pub prepared_settings: Option<FilterSettings>,
    prepared_preview: Option<Rgba8Image>,
    pending: Option<FilterPreviewRequest>,
    previews: PreviewJobs<Id, FilterPreviewResult>,
}

impl FilterEdit {
    /// `FilterEdit.init(kind:layer:selection:settings:growingTo:)` with `growingTo` nil.
    pub fn new(
        kind: FilterKind,
        layer: ImageLayer,
        selection: Option<SelectionClip>,
        settings: FilterSettings,
    ) -> Result<Self, CoreError> {
        Self::new_growing_to(kind, layer, selection, settings, None)
    }

    /// The same, with the document area the layer's grid should cover (Content-Aware Fill's selection
    /// on the canvas, or the whole canvas for a Vignette on an empty layer).
    pub fn new_growing_to(
        kind: FilterKind,
        layer: ImageLayer,
        selection: Option<SelectionClip>,
        settings: FilterSettings,
        growing_to: Option<Rect>,
    ) -> Result<Self, CoreError> {
        let Some(asset) = layer.asset.clone() else { return Err(invalid_project()) };
        let source = asset.image.as_rgba().ok_or_else(invalid_project)?.clone();
        let mut edit = FilterEdit {
            kind,
            settings: settings.normalized(),
            layer_id: layer.id,
            original: asset,
            transform: layer.transform,
            selection,
            mapping: AffineTransform::IDENTITY,
            preview_source: source.clone(),
            preview_scale: 1.0,
            preview_mapping: AffineTransform::IDENTITY,
            grown_image: None,
            grown_transform: None,
            grown_margin: 0.0,
            camera_raw: CameraRawSettings::default(),
            preview: true,
            committing: false,
            preview_error: None,
            preparing: false,
            seed: random_seed(),
            panel: CameraRawPanel::default(),
            canvas: None,
            started_empty: false,
            prepared_transform: None,
            pending_transform: None,
            preview_source_version: 0,
            prepared_settings: None,
            prepared_preview: None,
            pending: None,
            previews: PreviewJobs::new("compositor-filter-preview"),
        };
        let ready = Self::prepared(kind, &source, &edit.transform)?;
        edit.mapping = ready.0;
        edit.preview_source = ready.1;
        edit.preview_scale = ready.2;
        edit.preview_mapping = ready.3;
        if let Some(area) = growing_to {
            let to_pixels = Raster::pixel_to_document(
                &edit.transform,
                source.width() as f64,
                source.height() as f64,
            )
            .inverted();
            edit.grow(rect_applying(area, to_pixels).integral())?;
        }
        edit.grow_for_blur()?;
        Ok(edit)
    }

    /// The room a blur needs around the layer: about three standard deviations, or half a streak.
    /// `compositor_rs_core::image_ops::blur_margin` is the same rule.
    pub fn blur_margin(kind: FilterKind, settings: &FilterSettings) -> f64 {
        blur_margin(kind, settings)
    }

    /// Pads the layer out so the blur has somewhere to spread; only ever grows, so easing the amount
    /// back off doesn't rebuild anything.
    pub fn grow_for_blur(&mut self) -> Result<(), CoreError> {
        let margin = Self::blur_margin(self.kind, &self.settings);
        if margin <= self.grown_margin {
            return Ok(());
        }
        let bounds = self.original_bounds();
        self.grow(bounds.inset_by(-margin.ceil(), -margin.ceil()))
    }

    fn original_bounds(&self) -> Rect {
        Rect::from_origin_size(
            Point::ZERO,
            Size::new(
                self.original.image.width() as f64,
                self.original.image.height() as f64,
            ),
        )
    }

    /// The layer's pixels drawn into a grid covering `extent` (layer pixels), with the transform that
    /// places it.
    fn grow(&mut self, extent: Rect) -> Result<(), CoreError> {
        let bounds = self.original_bounds();
        let target = bounds.union(extent).integral();
        if target == bounds {
            return Ok(());
        }
        let (width, height) = (target.width() as usize, target.height() as usize);
        if target.width() > limits::MAX_SIDE_EXTENT
            || target.height() > limits::MAX_SIDE_EXTENT
            || target.width() * target.height() > limits::MAX_SURFACE_EXTENT
        {
            return Err(project_too_large());
        }
        let inside = bounds.offset_by(-target.min_x(), -target.min_y());
        let mut pixels = Rgba8Image::new(width, height);
        if let Some(raster) = self.original.raster.as_ref() {
            raster.compose_rgba(&mut pixels, inside);
        } else {
            let mut canvas = Canvas::from_rgba(pixels);
            Raster::draw(&self.original.image, inside, false, &mut canvas);
            pixels = canvas.into_rgba();
        }
        let to_document = Raster::pixel_to_document(
            &self.transform,
            self.original.image.width() as f64,
            self.original.image.height() as f64,
        );
        let mut expanded = self.transform;
        expanded.size = Size::new(
            target.width() * self.transform.size.width / bounds.width(),
            target.height() * self.transform.size.height / bounds.height(),
        );
        let middle = to_document.applying(Point::new(target.mid_x(), target.mid_y()));
        expanded.origin = Point::new(
            middle.x - expanded.size.width / 2.0,
            middle.y - expanded.size.height / 2.0,
        );
        self.grown_image = Some(pixels.clone());
        self.grown_transform = Some(expanded);
        self.grown_margin = (bounds.min_x() - target.min_x())
            .min(bounds.min_y() - target.min_y())
            .min(target.max_x() - bounds.max_x())
            .min(target.max_y() - bounds.max_y());
        self.prepare_from(&pixels, &expanded)
    }

    /// What the filter and its preview read: the full-size grid, and a copy no larger than
    /// [`FILTER_PREVIEW_LIMIT`] for everything but the filters that must be made at full size.
    fn prepared(
        kind: FilterKind,
        source: &Rgba8Image,
        placed: &LayerTransform,
    ) -> Result<(AffineTransform, Rgba8Image, f64, AffineTransform), CoreError> {
        let mapping = Raster::pixel_to_document(placed, source.width() as f64, source.height() as f64);
        // Noise, grain and dither preview at full size: made on a smaller copy they would look coarser
        // once enlarged.
        let full_size = matches!(
            kind,
            FilterKind::AddNoise | FilterKind::Grain | FilterKind::Dither | FilterKind::ContentAwareFill | FilterKind::RemoveBackground
        );
        let factor = if full_size {
            1.0
        } else {
            (FILTER_PREVIEW_LIMIT / source.width().max(source.height()) as f64).min(1.0)
        };
        if factor >= 1.0 {
            return Ok((mapping, source.clone(), 1.0, mapping));
        }
        let width = ((source.width() as f64 * factor) as usize).max(1);
        let height = ((source.height() as f64 * factor) as usize).max(1);
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.draw_image(
            source,
            Rect::from_origin_size(Point::ZERO, Size::new(width as f64, height as f64)),
        );
        let small = canvas.snapshot();
        Ok((
            mapping,
            small,
            width as f64 / source.width() as f64,
            Raster::pixel_to_document(placed, width as f64, height as f64),
        ))
    }

    fn prepare_from(
        &mut self,
        source: &Rgba8Image,
        placed: &LayerTransform,
    ) -> Result<(), CoreError> {
        let ready = Self::prepared(self.kind, source, placed)?;
        self.mapping = ready.0;
        self.preview_source = ready.1;
        self.preview_scale = ready.2;
        self.preview_mapping = ready.3;
        self.preview_source_version += 1;
        Ok(())
    }

    /// `previewImage(for:)`: the prepared preview while it belongs to this layer and previewing is on.
    pub fn preview_image(&self, id: Id) -> Option<Rgba8Image> {
        if self.preview && id == self.layer_id {
            self.prepared_preview.clone()
        } else {
            None
        }
    }

    /// `preparedPreview != nil` — a preview is up for this edit.
    pub fn has_prepared_preview(&self) -> bool {
        self.prepared_preview.is_some()
    }

    /// The preview `prepared_preview` holds.
    pub fn prepared_preview(&self) -> Option<Rgba8Image> {
        self.prepared_preview.clone()
    }

    /// `renderSettings()`: the settings as they will be rendered. Camera Raw's grade is the panel's
    /// eyes applied ([`FilterEdit::rendered_camera_raw`]).
    pub fn render_settings(&self) -> FilterSettings {
        self.settings.clone()
    }

    /// `renderSettings().cameraRaw`: a hidden Camera Raw group contributes nothing.
    pub fn rendered_camera_raw(&self) -> CameraRawSettings {
        self.panel.render_settings(&self.camera_raw)
    }

    /// `previewJob`: the job a preview renders, with the panel's preview-only views folded in.
    fn preview_job(&self) -> FilterJob {
        let mut job = FilterJob::new(
            self.kind,
            self.preview_source.clone(),
            self.rendered_settings(),
            self.preview_scale,
            self.selection.clone(),
            self.preview_mapping,
        );
        job.seed = self.seed;
        job.canvas = self.canvas;
        job.camera_raw_clipping = self.panel.clipping.map(CameraRawClipping::raw_value).unwrap_or(0);
        job.shows_shadow_clipping = self.panel.shows_shadow_clipping;
        job.shows_highlight_clipping = self.panel.shows_highlight_clipping;
        job.visualizes_point_color = self.panel.point_color_visualize_index(&self.camera_raw);
        job.shows_sharpen_mask = self.panel.sharpen_mask;
        job
    }

    /// `renderSettings()` with the Camera Raw panel folded into `FilterSettings`' own groups: the
    /// standalone filters read `settings`, the Camera Raw grade travels separately.
    fn rendered_settings(&self) -> FilterSettings {
        self.render_settings()
    }

    /// Registers the newest settings for a render (`edit.pending = edit.previewJob` +
    /// `edit.pendingTransform = edit.grownTransform`).
    fn queue_preview(&mut self) {
        self.pending = Some(FilterPreviewRequest {
            job: self.preview_job(),
            camera_raw: self.rendered_camera_raw(),
            placement: self.grown_transform,
            source_version: self.preview_source_version,
            settings: self.settings.clone(),
        });
        self.pending_transform = self.grown_transform;
    }

    /// `renderFilterPreview(_:)`: renders the newest settings; changes that arrive mid-render wait for
    /// it rather than cancelling it, so dragging a slider keeps the canvas updating.
    pub fn render_pending(&mut self) {
        let rendering = self.previews.is_rendering(&self.layer_id);
        let Some(request) = take_renderable(&mut self.pending, rendering) else {
            return;
        };
        self.preparing = true;
        self.preview_error = None;
        let key = self.layer_id;
        self.previews.request(key, move |cancel: &PreviewCancel| {
            if cancel.is_cancelled() {
                return None;
            }
            let FilterPreviewRequest { job, camera_raw, placement, source_version, settings } = request;
            let clipping = CameraRawClipping::from_raw_value(job.camera_raw_clipping);
            let rendered = if job.kind == FilterKind::CameraRaw {
                CameraRawScope::preview(&job, &camera_raw, clipping)
                    .map(|(image, scope)| (image, Some(scope)))
            } else {
                PixelFilter::run(&job, Some(CameraRawInputs { settings: &camera_raw, clipping }))
                    .map(|image| (image, None))
            };
            if cancel.is_cancelled() {
                return None;
            }
            Some(match rendered {
                Ok((image, scope)) => FilterPreviewResult {
                    image: Some(image),
                    scope,
                    placement,
                    source_version,
                    settings: Some(settings),
                    error: None,
                },
                Err(error) => FilterPreviewResult {
                    image: None,
                    scope: None,
                    placement,
                    source_version,
                    settings: Some(settings),
                    error: Some(error.to_string()),
                },
            })
        });
    }

    /// The completion half of `renderFilterPreview`: a landed result is shown (until the render from a
    /// grown layer replaces it) and the newest pending render starts. True when the canvas changed.
    pub fn poll(&mut self) -> bool {
        let mut redraw = false;
        if self.previews.take_ready().iter().any(|key| *key == self.layer_id) {
            let result = self.previews.result(&self.layer_id);
            self.preparing = false;
            self.preview_error = result.as_ref().and_then(|result| result.error.clone());
            if let Some(scope) = result.as_ref().and_then(|result| result.scope.clone()) {
                self.panel.scope = Some(scope);
            }
            if self.preview || self.kind.is_automatic() {
                self.prepared_preview = result.as_ref().and_then(|result| result.image.clone());
                self.prepared_transform = result.as_ref().and_then(|result| result.placement);
                self.prepared_settings = match result.as_ref() {
                    Some(result) if result.source_version == self.preview_source_version => result.settings.clone(),
                    _ => None,
                };
                redraw = true;
            }
        }
        self.render_pending();
        redraw
    }

    /// `await edit.previewTask?.value`: no render in flight and the newest settings applied. The port
    /// saturates the worker instead of awaiting the task.
    pub fn finish_previews(&mut self) -> bool {
        let mut redraw = false;
        loop {
            self.previews.saturate();
            redraw |= self.poll();
            if self.pending.is_none() {
                break;
            }
        }
        redraw
    }

    /// The Swift `edit.previewTask?.cancel()`.
    pub fn cancel_previews(&self) {
        self.previews.cancel(&self.layer_id);
    }

    /// True while a preview render is queued or running (`edit.previewTask != nil`).
    pub fn is_previewing(&self) -> bool {
        self.previews.is_rendering(&self.layer_id)
    }

    /// The Swift `edit.pending = nil; edit.preparedPreview = nil` of `updateFilter`'s preview-off path.
    pub fn clear_preview(&mut self) {
        self.pending = None;
        self.prepared_preview = None;
    }
}

/// A targeted-adjustment drag in progress (`HueTargetDrag`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HueTargetDrag {
    pub range: ColorRange,
    pub hue: f64,
    pub saturation: f64,
}

/// `HueSaturationJob`: one Hue/Saturation preview or commit. The port names it after the session field
/// that holds the newest one (`hueSaturationPending`).
pub type HuePreviewJob = HueSaturationJob;

/// `LevelsEdit`: the Levels dialog's preview, its histogram and the eyedroppers' sample state.
pub struct LevelsEdit {
    pub layer_id: Id,
    pub original: ImportedImage,
    pub transform: LayerTransform,
    pub selection: Option<SelectionClip>,
    pub mapping: AffineTransform,
    pub preview_source: Arc<Rgba8Image>,
    pub preview_mapping: AffineTransform,
    pub sample_mode: Option<LevelsSample>,
    pub settings: LevelsSettings,
    pub preview: bool,
    pub committing: bool,
    pub histogram: [Vec<f64>; 4],
    pub histogram_ready: bool,
    prepared_preview: Option<Rgba8Image>,
    pending: Option<LevelsJob>,
    previews: PreviewJobs<Id, Rgba8Image>,
    histogram_jobs: PreviewJobs<Id, [Vec<f64>; 4]>,
}

impl LevelsEdit {
    /// `LevelsEdit.init(layer:selection:)`.
    pub fn new(layer: ImageLayer, selection: Option<SelectionClip>) -> Result<Self, CoreError> {
        let Some(asset) = layer.asset.clone() else { return Err(invalid_project()) };
        let (width, height) = (asset.image.width(), asset.image.height());
        let mapping = Raster::pixel_to_document(&layer.transform, width as f64, height as f64);
        // Full size up to 8000 pixels on a side: a levels preview is a lookup table per pixel, quick
        // enough to run on the whole layer, and a downscaled copy showed the canvas a coarse,
        // pixelated version while dragging.
        let factor = (LEVELS_PREVIEW_LIMIT / width.max(height) as f64).min(1.0);
        let (preview_source, preview_mapping) = if factor < 1.0 {
            let small = ((width as f64 * factor) as usize).max(1);
            let tall = ((height as f64 * factor) as usize).max(1);
            let rect = Rect::from_origin_size(Point::ZERO, Size::new(small as f64, tall as f64));
            let preview = match asset.raster.as_ref() {
                Some(raster) => {
                    let mut pixels = Rgba8Image::new(small, tall);
                    raster.compose_rgba(&mut pixels, rect);
                    pixels
                }
                None => {
                    let mut canvas = Canvas::new_rgba(small, tall);
                    Raster::draw(&asset.image, rect, false, &mut canvas);
                    canvas.into_rgba()
                }
            };
            (Arc::new(preview), Raster::pixel_to_document(&layer.transform, small as f64, tall as f64))
        } else {
            let source = asset.image.as_rgba().ok_or_else(invalid_project)?;
            (Arc::new(source.clone()), mapping)
        };
        Ok(LevelsEdit {
            layer_id: layer.id,
            original: asset,
            transform: layer.transform,
            selection,
            mapping,
            preview_source,
            preview_mapping,
            sample_mode: None,
            settings: LevelsSettings::default(),
            preview: true,
            committing: false,
            histogram: std::array::from_fn(|_| vec![0.0; 256]),
            histogram_ready: false,
            prepared_preview: None,
            pending: None,
            previews: PreviewJobs::new("compositor-levels-preview"),
            histogram_jobs: PreviewJobs::new("compositor-levels-histogram"),
        })
    }

    /// `previewImage(for:)`.
    pub fn preview_image(&self, id: Id) -> Option<Rgba8Image> {
        if self.preview && id == self.layer_id {
            self.prepared_preview.clone()
        } else {
            None
        }
    }

    /// `previewJob`.
    pub fn preview_job(&self) -> LevelsJob {
        LevelsJob {
            image: Arc::clone(&self.preview_source),
            settings: self.settings.clone(),
            selection: self.selection.clone(),
            mapping: self.preview_mapping,
        }
    }

    /// `edit.histogramTask`: the histogram is computed off the UI thread, off the preview source.
    pub fn start_histogram(&mut self) {
        self.histogram_ready = false;
        let job = self.preview_job();
        let key = self.layer_id;
        self.histogram_jobs.request(key, move |cancel: &PreviewCancel| {
            if cancel.is_cancelled() {
                return None;
            }
            LevelsFilter::histogram(&job).ok()
        });
    }

    /// Registers the newest settings as the next render.
    pub fn queue_preview(&mut self) {
        self.pending = Some(self.preview_job());
    }

    /// `renderLevelsPreview(_:)`.
    pub fn render_pending(&mut self) {
        let rendering = self.previews.is_rendering(&self.layer_id);
        let Some(job) = take_renderable(&mut self.pending, rendering) else {
            return;
        };
        let key = self.layer_id;
        self.previews.request(key, move |cancel: &PreviewCancel| {
            if cancel.is_cancelled() {
                return None;
            }
            LevelsFilter::run(&job).ok()
        });
    }

    /// The completion half of `renderLevelsPreview`: applies a landed histogram or preview. True when
    /// the canvas changed.
    pub fn poll(&mut self) -> bool {
        let mut redraw = false;
        if self.histogram_jobs.take_ready().iter().any(|key| *key == self.layer_id) {
            if let Some(bins) = self.histogram_jobs.result(&self.layer_id) {
                self.histogram = bins;
            }
            self.histogram_ready = true;
        }
        if self.previews.take_ready().iter().any(|key| *key == self.layer_id) {
            if self.preview && !self.settings.is_identity() {
                self.prepared_preview = self.previews.result(&self.layer_id);
                redraw = true;
            }
        }
        self.render_pending();
        redraw
    }

    /// `edit.previewTask?.cancel()` and `edit.histogramTask?.cancel()`.
    pub fn cancel_previews(&self) {
        self.previews.cancel(&self.layer_id);
        self.histogram_jobs.cancel(&self.layer_id);
    }

    /// True while a preview render is queued or running (`edit.previewTask != nil`).
    pub fn is_previewing(&self) -> bool {
        self.previews.is_rendering(&self.layer_id)
    }

    /// `edit.pending = nil; edit.preparedPreview = nil`.
    pub fn clear_preview(&mut self) {
        self.pending = None;
        self.prepared_preview = None;
    }
}

/// `LevelsEdit.previewLimit`'s literal 8000.
pub const LEVELS_PREVIEW_LIMIT: f64 = 8000.0;

/// `HueSaturationEdit`: the Hue/Saturation dialog's settings and its live preview from the downscaled
/// original.
pub struct HueSaturationEdit {
    pub layer_id: Id,
    pub original: ImportedImage,
    pub selection: Option<SelectionClip>,
    pub pixel_to_document: AffineTransform,
    /// Downscaled original used for previews, with the mapping for its own pixel grid.
    pub preview_source: Arc<Rgba8Image>,
    pub preview_pixel_to_document: AffineTransform,
    pub settings: HueSaturationSettings,
    pub preview: bool,
    prepared_preview: Option<Rgba8Image>,
    pending: Option<HueSaturationJob>,
    previews: PreviewJobs<Id, Rgba8Image>,
}

impl HueSaturationEdit {
    /// `HueSaturationEdit.init(layerID:original:selection:transform:)`.
    pub fn new(
        layer_id: Id,
        original: ImportedImage,
        selection: Option<SelectionClip>,
        transform: LayerTransform,
    ) -> Result<Self, CoreError> {
        let width = original.image.width();
        let height = original.image.height();
        let pixel_to_document = Raster::pixel_to_document(&transform, width as f64, height as f64);
        let factor = (HUE_SATURATION_PREVIEW_LIMIT / width.max(height) as f64).min(1.0);
        let (preview_source, preview_pixel_to_document) = if factor < 1.0 {
            let small = ((width as f64 * factor) as usize).max(1);
            let tall = ((height as f64 * factor) as usize).max(1);
            let mut canvas = Canvas::new_rgba(small, tall);
            canvas.set_interpolation_quality(InterpolationQuality::Low);
            Raster::draw(
                &original.image,
                Rect::from_origin_size(Point::ZERO, Size::new(small as f64, tall as f64)),
                false,
                &mut canvas,
            );
            (
                Arc::new(canvas.into_rgba()),
                Raster::pixel_to_document(&transform, small as f64, tall as f64),
            )
        } else {
            (
                Arc::new(original.image.as_rgba().ok_or_else(invalid_project)?.clone()),
                pixel_to_document,
            )
        };
        Ok(HueSaturationEdit {
            layer_id,
            original,
            selection,
            pixel_to_document,
            preview_source,
            preview_pixel_to_document,
            settings: HueSaturationSettings::default(),
            preview: true,
            prepared_preview: None,
            pending: None,
            previews: PreviewJobs::new("compositor-hue-saturation-preview"),
        })
    }

    /// `previewImage(for:)`.
    pub fn preview_image(&self, layer_id: Id) -> Option<Rgba8Image> {
        if layer_id == self.layer_id {
            self.prepared_preview.clone()
        } else {
            None
        }
    }

    /// `setPreview(_:)`.
    pub fn set_preview(&mut self, image: Option<Rgba8Image>) {
        self.prepared_preview = image;
    }

    /// Registers the newest settings as the next render (`hueSaturationPending = HueSaturationJob(...)`).
    pub fn submit(&mut self, job: HueSaturationJob) {
        self.pending = Some(job);
        self.render_pending();
    }

    /// `renderPendingPreview(_:)`: requests coalesce rather than cancel — a render already running
    /// finishes and is shown, then the newest request renders.
    pub fn render_pending(&mut self) {
        let rendering = self.previews.is_rendering(&self.layer_id);
        let Some(job) = take_renderable(&mut self.pending, rendering) else {
            return;
        };
        let key = self.layer_id;
        self.previews.request(key, move |cancel: &PreviewCancel| {
            if cancel.is_cancelled() {
                return None;
            }
            HueSaturationFilter::run(&job).ok().map(|adjusted| adjusted.image)
        });
    }

    /// The completion half of `renderPendingPreview`. True when the canvas changed.
    pub fn poll(&mut self) -> bool {
        let mut redraw = false;
        if self.previews.take_ready().iter().any(|key| *key == self.layer_id) {
            if let Some(image) = self.previews.result(&self.layer_id) {
                self.set_preview(Some(image));
                redraw = true;
            }
        }
        self.render_pending();
        redraw
    }

    /// `hueSaturationTask?.cancel()`.
    pub fn cancel_previews(&self) {
        self.previews.cancel(&self.layer_id);
    }

    /// True while a preview render is queued or running (`hueSaturationTask != nil`).
    pub fn is_previewing(&self) -> bool {
        self.previews.is_rendering(&self.layer_id)
    }
}

/// The `extension EditorSession` of `Document/Filters.swift`.
impl EditorSession {
    /// `canContentAwareFill`: a non-empty selection on a layer that can take a color adjustment, with
    /// no other editor open.
    pub fn can_content_aware_fill(&self) -> bool {
        self.can_adjust_colors()
            && !self.is_mask_selected
            && self.selection().is_some_and(|selection| !selection.is_empty())
            && self.filter_edit.is_none()
            && self.hue_saturation.is_none()
    }

    /// `canVignette`: color adjustments need a visible image layer; Vignette also paints an empty
    /// layer, which has no pixels until something is put on it.
    pub fn can_vignette(&self) -> bool {
        if self.can_adjust_colors() {
            return true;
        }
        let Some(layer) = self.active_layer() else { return false };
        if layer.asset.is_some() || layer.adjustment.is_some() || layer.is_group {
            return false;
        }
        self.can_adjust(true)
    }

    /// `canAdjustColors`: a visible image layer, not a mask, with a non-empty selection if there is
    /// one.
    pub fn can_adjust_colors(&self) -> bool {
        self.can_adjust(false)
    }

    /// `canAdjust(allowingEmpty:)`.
    fn can_adjust(&self, allowing_empty: bool) -> bool {
        let _ = self.shows_busy;
        // Text being edited is drawn by its editor, not the layer, so a filter's preview of it would
        // be wrong: commit it first.
        if self.levels.is_some() || self.filter_edit.is_some() || self.text_draft.is_some() {
            return false;
        }
        if self.document.is_none() || self.is_project_busy || self.is_importing {
            return false;
        }
        if self.brush_stroke.is_some() || self.pixel_move.is_some() || self.renaming_layer_id.is_some() {
            return false;
        }
        if self.shows_new_document || self.shows_importer {
            return false;
        }
        if self.selected_layer_ids.len() != 1 || self.is_mask_selected {
            return false;
        }
        let Some(layer) = self.active_layer() else { return false };
        if layer.is_group {
            return false;
        }
        if layer.asset.is_none() && !allowing_empty {
            return false;
        }
        if self
            .document
            .as_ref()
            .map(|document| document.effective_visible_ids().contains(&layer.id))
            != Some(true)
        {
            return false;
        }
        self.selection().map(|selection| selection.is_empty()) != Some(true)
    }

    /// `beginFilter(_:)`: opens a filter's live preview on the active layer.
    pub fn begin_filter(&mut self, kind: FilterKind) {
        if kind == FilterKind::ContentAwareFill && !self.can_content_aware_fill() {
            return;
        }
        let allowed = if kind == FilterKind::Vignette { self.can_vignette() } else { self.can_adjust_colors() };
        if self.filter_edit.is_some() || self.hue_saturation.is_some() || !allowed {
            // `NSSound.beep()`.
            return;
        }
        if self.gradient_edit.is_some() {
            self.resolve_gradient();
            self.begin_filter(kind);
            return;
        }
        self.commit_transform();
        self.cancel_crop();
        self.cancel_lasso();
        let Some(document_size) = self.document.as_ref().map(|document| document.size()) else { return };
        let Some(mut layer) = self.active_layer().cloned() else { return };
        // An empty layer has no pixels until something is put on it; Vignette starts it with clear
        // ones.
        let started_empty = layer.asset.is_none();
        if started_empty {
            let width = (layer.transform.size.width.round() as usize).max(1);
            let height = (layer.transform.size.height.round() as usize).max(1);
            let clear = Rgba8Image::new(width, height);
            let thumbnail = PixelAdjust::thumbnail(&clear);
            layer.asset = Some(ImportedImage::new(
                PixelImage::Rgba(Arc::new(clear)),
                PixelImage::Rgba(Arc::new(thumbnail)),
                layer.name.clone(),
            ));
        }
        let mut settings = self.filter_settings.clone();
        // Gradient Map starts from the foreground and background colors, as in Photoshop.
        if kind == FilterKind::GradientMap {
            settings.gradient_map = GradientMapSettings {
                shadows: AdjustmentColor::from(self.foreground_color()),
                highlights: AdjustmentColor::from(self.background_color),
                ..GradientMapSettings::default()
            };
        }
        // Content-Aware Fill extends the layer over any of the selection on the canvas past its edge;
        // Vignette on an empty layer covers the whole canvas, which it frames and fills.
        let canvas = Rect::from_origin_size(Point::ZERO, document_size);
        let fills_canvas = kind == FilterKind::Vignette && started_empty;
        let area = if kind == FilterKind::ContentAwareFill {
            self.selection()
                .map(|selection| selection.path.bounding_box().intersection(canvas))
                .filter(|rect| !rect.is_null() && !rect.is_empty())
        } else if fills_canvas {
            Some(canvas)
        } else {
            None
        };
        let selection = self.selection().map(|selection| selection.clip(document_size));
        match FilterEdit::new_growing_to(kind, layer, selection, settings, area) {
            Ok(mut edit) => {
                if fills_canvas {
                    edit.canvas = Some(canvas);
                }
                edit.started_empty = started_empty;
                edit.camera_raw = self.camera_raw_settings.normalized();
                let settings = edit.settings.clone();
                self.filter_edit = Some(edit);
                self.update_filter(settings, true);
            }
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// `updateFilter(_:preview:)`: the panel's sliders.
    pub fn update_filter(&mut self, settings: FilterSettings, preview: bool) {
        let normalized = settings.normalized();
        let mut grow_error = None;
        {
            let Some(edit) = self.filter_edit.as_mut() else { return };
            if edit.committing {
                return;
            }
            edit.settings = normalized;
            edit.preview = preview;
            // A bigger blur needs more room around the layer than it was given.
            if FilterEdit::blur_margin(edit.kind, &edit.settings) > edit.grown_margin {
                if let Err(error) = edit.grow_for_blur() {
                    grow_error = Some(error.to_string());
                }
            }
        }
        if let Some(message) = grow_error {
            self.brush_error = Some(message);
        }
        if self.preview_adjustment_editing(preview) {
            return;
        }
        let mut redraw = false;
        let mut render = false;
        {
            let Some(edit) = self.filter_edit.as_mut() else { return };
            if edit.kind.is_automatic()
                && edit.has_prepared_preview()
                && edit.prepared_settings.as_ref() == Some(&edit.settings)
            {
                redraw = true;
            } else if !preview {
                edit.clear_preview();
                redraw = true;
            } else {
                edit.queue_preview();
                render = true;
            }
        }
        if redraw {
            self.brush_revision += 1;
        }
        if render {
            if let Some(edit) = self.filter_edit.as_mut() {
                edit.render_pending();
            }
        }
    }

    /// `cancelFilter()`: the panel's Cancel, and the window's close button.
    pub fn cancel_filter(&mut self) {
        self.close_filter_color_pickers(false);
        if self.finish_adjustment_editing(false) {
            return;
        }
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if edit.committing {
            return;
        }
        if let Some(edit) = self.filter_edit.take() {
            edit.cancel_previews();
        }
        self.brush_revision += 1;
    }

    /// The filter colors still being picked go with the panel (`colorPicker?.target`'s gradient-map,
    /// vignette and dither cases).
    fn close_filter_color_pickers(&mut self, commit: bool) {
        let close = matches!(
            self.color_picker.as_ref().map(|picker| &picker.target),
            Some(
                ColorPickerTarget::GradientMap { .. }
                    | ColorPickerTarget::Vignette
                    | ColorPickerTarget::Dither { .. }
            )
        );
        if close {
            self.close_color_picker(commit);
        }
    }

    /// `commitFilter()`: OK renders the full-size result onto the layer as one undo step.
    pub fn commit_filter(&mut self) {
        self.close_filter_color_pickers(true);
        if self.finish_adjustment_editing(true) {
            return;
        }
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if edit.committing {
            return;
        }
        if edit.kind.is_automatic() {
            // `await edit.previewTask?.value`: Remove Background masks from the full-size image, so a
            // preview made at preview size is fine to discard.
            if let Some(edit) = self.filter_edit.as_mut() {
                edit.finish_previews();
            }
            let ready = self
                .filter_edit
                .as_ref()
                .is_some_and(|edit| edit.has_prepared_preview() && edit.preview_error.is_none());
            if !ready {
                return;
            }
        }
        // A hidden Camera Raw group is absent from the layer. Remember that rendered grade, including
        // when every remaining amount is zero, so the next open does not put the hidden sliders back.
        let rendered_camera_raw = self
            .filter_edit
            .as_ref()
            .map(|edit| edit.rendered_camera_raw())
            .unwrap_or_default();
        if self.filter_edit.as_ref().is_some_and(|edit| edit.kind == FilterKind::CameraRaw) {
            self.camera_raw_settings = rendered_camera_raw.clone();
        }
        // No distortion to remove: close as Cancel does, without an undo step.
        let closes_as_cancel = self.filter_edit.as_ref().is_some_and(|edit| match edit.kind {
            FilterKind::LensCorrection => edit.settings.distortion == 0.0,
            FilterKind::Vignette => edit.settings.vignette_amount == 0.0,
            FilterKind::BloomGlow => edit.settings.bloom_amount == 0.0,
            FilterKind::TonalContrast => {
                edit.settings.tonal_amount == 0.0
                    || (edit.settings.tonal_shadows == 0.0
                        && edit.settings.tonal_midtones == 0.0
                        && edit.settings.tonal_highlights == 0.0)
            }
            FilterKind::Exposure => edit.settings.exposure == Default::default(),
            FilterKind::Grain => edit.settings.grain.amount == 0.0,
            FilterKind::CameraRaw => edit.rendered_camera_raw().is_identity(),
            _ => false,
        });
        if closes_as_cancel {
            self.cancel_filter();
            return;
        }
        let Some(mut edit) = self.filter_edit.take() else { return };
        edit.committing = true;
        edit.cancel_previews();
        if edit.kind != FilterKind::CameraRaw {
            self.filter_settings = edit.settings.clone();
        }
        self.is_project_busy = true;
        // The preview stays up until the result is on the layer, so the canvas never flashes the
        // original: `filterEdit` is cleared only here, at the end.
        if edit.kind == FilterKind::RemoveBackground {
            self.commit_background_mask(&edit);
        } else {
            self.commit_filter_pixels(&edit, &rendered_camera_raw);
        }
        self.filter_edit = None;
        self.is_project_busy = false;
        self.brush_revision += 1;
    }

    /// The layer half of `commitFilter`: renders the full-size job, trims what a blur left empty, and
    /// records one undo step named after the kind.
    fn commit_filter_pixels(&mut self, edit: &FilterEdit, camera_raw: &CameraRawSettings) {
        let Some(original) = edit.original.image.as_rgba().cloned() else {
            self.brush_error = Some(invalid_project().to_string());
            return;
        };
        let mut job = FilterJob::new(
            edit.kind,
            edit.grown_image.clone().unwrap_or(original),
            edit.render_settings(),
            1.0,
            edit.selection.clone(),
            edit.mapping,
        );
        job.seed = edit.seed;
        job.canvas = edit.canvas;
        let cached = if edit.kind.is_automatic() && edit.prepared_settings.as_ref() == Some(&edit.settings) {
            edit.prepared_preview()
        } else {
            None
        };
        let clipping = CameraRawClipping::from_raw_value(job.camera_raw_clipping);
        let made = match cached {
            Some(image) => Ok(image),
            None => PixelFilter::run(&job, Some(CameraRawInputs { settings: camera_raw, clipping })),
        };
        let mut image = match made {
            Ok(image) => image,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        let spreads = matches!(
            edit.kind,
            FilterKind::GaussianBlur | FilterKind::MotionBlur | FilterKind::BloomGlow
        );
        let mut placed = edit.grown_transform;
        if spreads {
            if let Some(grown) = edit.grown_transform {
                match PixelFilter::trimmed(&image, &grown) {
                    Ok((trimmed, transform)) => {
                        image = trimmed;
                        placed = Some(transform);
                    }
                    Err(error) => {
                        self.brush_error = Some(error.to_string());
                        return;
                    }
                }
            }
        }
        let asset = ImportedImage::new(
            PixelImage::Rgba(Arc::new(image.clone())),
            PixelImage::Rgba(Arc::new(PixelAdjust::thumbnail(&image))),
            edit.kind.raw_value(),
        );
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        let Some(current) = self.document.as_ref().and_then(|document| document.layers.get(index)).cloned()
        else {
            return;
        };
        let same_image = current
            .asset
            .as_ref()
            .is_some_and(|asset| same_pixels(&asset.image, &edit.original.image));
        if !(same_image || (edit.started_empty && current.asset.is_none())) || current.transform != edit.transform {
            return;
        }
        // A grown layer's mask (covering the old grid) is carried onto the new one, its edge tone past
        // the old edge.
        let mut mask = current.mask.clone();
        if let Some(grown) = edit.grown_transform {
            if let Some(owned) = current.mask.as_ref() {
                if owned.placement.is_none()
                    && (owned.asset.image.width() > 1 || owned.asset.image.height() > 1)
                {
                    let mut enabled = owned.clone();
                    enabled.is_enabled = true;
                    let carried = enabled.clip_image(
                        Some(&current.transform),
                        &grown,
                        asset.image.width(),
                        asset.image.height(),
                        None,
                    );
                    let Some(carried) = carried else {
                        self.brush_error = Some(render_failed().to_string());
                        return;
                    };
                    match LayerMask::asset(carried) {
                        Ok(carried) => mask = Some(owned.replacing(carried)),
                        Err(error) => {
                            self.brush_error = Some(error.to_string());
                            return;
                        }
                    }
                }
            }
        }
        self.begin_edit(edit.kind.raw_value());
        if let Some(document) = self.document.as_mut() {
            let mut layer = current;
            layer.asset = Some(asset);
            layer.transform = placed.unwrap_or(layer.transform);
            layer.mask = mask;
            layer.adjustment = None;
            document.layers[index] = layer;
        }
        self.end_edit();
    }

    /// `commitBackgroundMask(_:)`: Remove Background as a layer mask — the subject stays white, the
    /// background black. A mask already on the layer (in the layer's own grid) is kept, hiding whatever
    /// either one hides; with a selection, only the selected part of the mask changes.
    fn commit_background_mask(&mut self, edit: &FilterEdit) {
        let Some(source) = edit.original.image.as_rgba().cloned() else {
            self.brush_error = Some(invalid_project().to_string());
            return;
        };
        let current = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == edit.layer_id))
            .cloned();
        let existing = current.as_ref().and_then(|layer| layer.mask.as_ref()).and_then(|owned| {
            let image = &owned.asset.image;
            if owned.placement.is_none()
                && image.width() == source.width()
                && image.height() == source.height()
            {
                image.as_gray().cloned()
            } else {
                None
            }
        });
        let selection = edit.selection.clone();
        let mapping = edit.mapping;
        let settings = edit.settings.normalized();
        let mut mask = match SubjectRemoval::subject_mask(&source, existing.as_ref(), &settings) {
            Ok(mask) => mask,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        if let Some(selection) = selection.as_ref() {
            let background = match existing.as_ref() {
                Some(base) => base.clone(),
                None => Gray8Image::uniform(source.width(), source.height(), 255),
            };
            mask = blend_gray_through_selection(&mask, &background, selection, mapping);
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        let Some(layer) = self.document.as_ref().and_then(|document| document.layers.get(index)).cloned()
        else {
            return;
        };
        let same_image = layer
            .asset
            .as_ref()
            .is_some_and(|asset| same_pixels(&asset.image, &edit.original.image));
        if !same_image || layer.transform != edit.transform {
            return;
        }
        let asset = match LayerMask::asset(PixelImage::Gray(Arc::new(mask))) {
            Ok(asset) => asset,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        self.begin_edit(edit.kind.raw_value());
        if let Some(document) = self.document.as_mut() {
            let mut updated = layer;
            updated.mask = Some(match updated.mask.take() {
                Some(owned) => owned.replacing(asset),
                None => LayerMask::new(asset),
            });
            if let Some(mask) = updated.mask.as_mut() {
                mask.is_enabled = true;
            }
            document.layers[index] = updated;
        }
        self.is_mask_selected = true;
        self.end_edit();
    }
}

/// The `extension EditorSession` of `Document/Levels.swift`, `Document/LevelsAutomatic.swift` and
/// `Document/HueSaturation.swift`.
impl EditorSession {
    /// `beginLevels()`: opens the Levels dialog on the active layer, with its histogram computing off
    /// the UI thread.
    pub fn begin_levels(&mut self) {
        if self.levels.is_some() || self.hue_saturation.is_some() || !self.can_adjust_colors() {
            return;
        }
        if self.gradient_edit.is_some() {
            // `Task { await commitGradient(); beginLevels() }`: the port commits it inline.
            self.commit_gradient();
            self.begin_levels();
            return;
        }
        self.commit_transform();
        self.cancel_crop();
        self.cancel_lasso();
        let Some(document_size) = self.document.as_ref().map(|document| document.size()) else { return };
        let Some(layer) = self.active_layer().cloned() else { return };
        let selection = self.selection().map(|selection| selection.clip(document_size));
        match LevelsEdit::new(layer, selection) {
            Ok(mut edit) => {
                edit.start_histogram();
                self.levels = Some(edit);
            }
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// `updateLevels(_:preview:)`: the panel's sliders, the eyedroppers' result and the automatic
    /// calibrations. An identity or preview-off change drops the preview rather than rendering one.
    pub fn update_levels(&mut self, settings: &LevelsSettings, preview: bool) {
        {
            let Some(edit) = self.levels.as_mut() else { return };
            if edit.committing {
                return;
            }
            edit.settings = settings.clone();
            edit.preview = preview;
        }
        if self.preview_adjustment_editing(preview) {
            return;
        }
        let mut redraw = false;
        {
            let Some(edit) = self.levels.as_mut() else { return };
            if !preview || settings.is_identity() {
                // `edit.previewTask?.cancel(); edit.previewTask = nil; edit.pending = nil;
                // edit.preparedPreview = nil`.
                edit.cancel_previews();
                edit.clear_preview();
                redraw = true;
            } else {
                // `edit.pending = edit.previewJob; renderLevelsPreview(edit)`.
                edit.queue_preview();
                edit.render_pending();
            }
        }
        if redraw {
            self.brush_revision += 1;
        }
    }

    /// `cancelLevels()`: the panel's Cancel, and the window's close button.
    pub fn cancel_levels(&mut self) {
        if self.finish_adjustment_editing(false) {
            return;
        }
        let Some(edit) = self.levels.as_ref() else { return };
        if edit.committing {
            return;
        }
        // `edit.previewTask?.cancel(); edit.histogramTask?.cancel()`.
        if let Some(edit) = self.levels.take() {
            edit.cancel_previews();
        }
        self.brush_revision += 1;
    }

    /// `commitLevels()`: OK renders the full-size result onto the layer as one undo step named
    /// "Levels". Identity settings close as Cancel did, without a step.
    pub fn commit_levels(&mut self) {
        if self.finish_adjustment_editing(true) {
            return;
        }
        let Some(edit) = self.levels.as_ref() else { return };
        if edit.committing {
            return;
        }
        if edit.settings.is_identity() {
            self.cancel_levels();
            return;
        }
        let Some(mut edit) = self.levels.take() else { return };
        edit.committing = true;
        edit.cancel_previews();
        self.is_project_busy = true;
        self.commit_level_pixels(&edit);
        // `defer { levels = nil; isProjectBusy = false; brushRevision += 1 }`.
        self.is_project_busy = false;
        self.brush_revision += 1;
    }

    /// The layer half of `commitLevels`: the full-size render, the same-image guard, and one undo step
    /// named "Levels".
    fn commit_level_pixels(&mut self, edit: &LevelsEdit) {
        let Some(original) = edit.original.image.as_rgba().cloned() else {
            self.brush_error = Some(invalid_project().to_string());
            return;
        };
        let job = LevelsJob {
            image: Arc::new(original),
            settings: edit.settings.clone(),
            selection: edit.selection.clone(),
            mapping: edit.mapping,
        };
        let image = match LevelsFilter::run(&job) {
            Ok(image) => image,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        let Some(current) = self.document.as_ref().and_then(|document| document.layers.get(index)).cloned()
        else {
            return;
        };
        let same_image = current
            .asset
            .as_ref()
            .is_some_and(|asset| same_pixels(&asset.image, &edit.original.image));
        if !same_image || current.transform != edit.transform {
            return;
        }
        let asset = ImportedImage::new(
            PixelImage::Rgba(Arc::new(image.clone())),
            PixelImage::Rgba(Arc::new(PixelAdjust::thumbnail(&image))),
            "Levels",
        );
        self.begin_edit("Levels");
        if let Some(document) = self.document.as_mut() {
            let mut layer = current;
            layer.asset = Some(asset);
            document.layers[index] = layer;
        }
        self.end_edit();
    }

    /// `autoLevels(_:)`: the panel's Auto Levels button, from the histogram the edit has by now.
    pub fn auto_levels(&mut self, mode: LevelsAuto) {
        let (settings, preview) = {
            let Some(edit) = self.levels.as_mut() else { return };
            if !edit.histogram_ready || edit.committing {
                return;
            }
            edit.sample_mode = None;
            (mode.settings(&edit.histogram), edit.preview)
        };
        self.update_levels(&settings, preview);
    }

    /// `sampleLevels(at:)`: the Levels eyedropper. Samples are the original image's straight channels;
    /// a fully transparent pixel and a point outside the canvas are ignored.
    pub fn sample_levels(&mut self, at: Point) {
        let Some(edit) = self.levels.as_ref() else { return };
        let Some(mode) = edit.sample_mode else { return };
        if edit.committing {
            return;
        }
        let Some(document_size) = self.document.as_ref().map(|document| document.size()) else { return };
        if !Rect::from_origin_size(Point::ZERO, document_size).contains(at) {
            return;
        }
        let pixel = edit.mapping.inverted().applying(at);
        let Some((red, green, blue)) = straight_image_pixel(&edit.original.image, pixel) else { return };
        let settings = levels_sampling(&edit.settings, [red, green, blue], mode);
        let preview = edit.preview;
        self.update_levels(&settings, preview);
    }

    /// `beginHueSaturation()`: opens the Hue/Saturation dialog on the active layer.
    pub fn begin_hue_saturation(&mut self) {
        if self.hue_saturation.is_some() || !self.can_adjust_colors() {
            // `NSSound.beep()`.
            return;
        }
        self.commit_transform();
        if self.gradient_edit.is_some() {
            self.resolve_gradient();
        }
        let Some(document_size) = self.document.as_ref().map(|document| document.size()) else { return };
        let Some(layer) = self.active_layer().cloned() else { return };
        let Some(asset) = layer.asset.clone() else { return };
        let selection = self.selection().map(|selection| selection.clip(document_size));
        match HueSaturationEdit::new(layer.id, asset, selection, layer.transform) {
            Ok(edit) => self.hue_saturation = Some(edit),
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// `updateHueSaturation(_:preview:)`: the panel's sliders. Previews render from the downscaled
    /// original and requests coalesce rather than cancel.
    pub fn update_hue_saturation(&mut self, settings: &HueSaturationSettings, preview: bool) {
        {
            let Some(edit) = self.hue_saturation.as_mut() else { return };
            edit.settings = settings.clone();
            edit.preview = preview;
        }
        if self.preview_adjustment_editing(preview) {
            return;
        }
        let mut redraw = false;
        {
            let Some(edit) = self.hue_saturation.as_mut() else { return };
            if !preview || settings.is_identity() {
                // `hueSaturationTask?.cancel(); hueSaturationPending = nil; edit.setPreview(nil)`.
                edit.cancel_previews();
                edit.set_preview(None);
                redraw = true;
            } else {
                // `hueSaturationPending = HueSaturationJob(...); renderPendingPreview(edit)`.
                let job = HueSaturationJob {
                    image: Arc::clone(&edit.preview_source),
                    settings: settings.clone(),
                    selection: edit.selection.clone(),
                    pixel_to_document: edit.preview_pixel_to_document,
                    thumbnail: false,
                };
                edit.submit(job);
            }
        }
        if redraw {
            self.brush_revision += 1;
        }
    }

    /// `commitHueSaturation()`: OK renders at full size and records one "Hue/Saturation" undo step.
    /// Identity settings change nothing at all. The preview stays up until the committed pixels are in
    /// the document.
    pub fn commit_hue_saturation(&mut self) {
        if self.finish_adjustment_editing(true) {
            return;
        }
        let Some(edit) = self.hue_saturation.take() else { return };
        self.hue_sample_mode = None;
        self.hue_targeting = false;
        self.hue_target_drag = None;
        self.hue_saturation_pending = None;
        // `hueSaturationTask?.cancel()`.
        edit.cancel_previews();
        let settings = edit.settings.clone();
        // `defer { hueSaturation = nil; brushRevision += 1 }`.
        if settings.is_identity() {
            self.brush_revision += 1;
            return;
        }
        self.is_project_busy = true;
        self.commit_hue_saturation_pixels(&edit, &settings);
        // `defer { isProjectBusy = false }`.
        self.is_project_busy = false;
        self.brush_revision += 1;
    }

    /// The layer half of `commitHueSaturation`: the full-size render, the same-image guard, and one
    /// undo step named "Hue/Saturation".
    fn commit_hue_saturation_pixels(&mut self, edit: &HueSaturationEdit, settings: &HueSaturationSettings) {
        let Some(original) = edit.original.image.as_rgba().cloned() else {
            self.brush_error = Some(invalid_project().to_string());
            return;
        };
        let job = HueSaturationJob {
            image: Arc::new(original),
            settings: settings.clone(),
            selection: edit.selection.clone(),
            pixel_to_document: edit.pixel_to_document,
            thumbnail: true,
        };
        let adjusted = match HueSaturationFilter::run(&job) {
            Ok(adjusted) => adjusted,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        let Some(current) = self.document.as_ref().and_then(|document| document.layers.get(index)).cloned()
        else {
            return;
        };
        let same_image = current
            .asset
            .as_ref()
            .is_some_and(|asset| same_pixels(&asset.image, &edit.original.image));
        if !same_image {
            return;
        }
        let AdjustedPixels { image, thumbnail } = adjusted;
        // `adjusted.thumbnail ?? adjusted.image`: without one, the image is its own thumbnail.
        let (image, thumbnail) = match thumbnail {
            Some(thumbnail) => (PixelImage::Rgba(Arc::new(image)), PixelImage::Rgba(Arc::new(thumbnail))),
            None => {
                let shared = Arc::new(image);
                (PixelImage::Rgba(Arc::clone(&shared)), PixelImage::Rgba(shared))
            }
        };
        let asset = ImportedImage::new(image, thumbnail, current.name.clone());
        self.begin_edit("Hue/Saturation");
        if let Some(document) = self.document.as_mut() {
            let mut layer = current;
            layer.asset = Some(asset);
            document.layers[index] = layer;
        }
        self.end_edit();
    }

    /// `sampledHue(at:)`: the hue under a document point, from the visible composite. Near-neutral
    /// pixels have no meaningful hue.
    pub fn sampled_hue(&self, at: Point) -> Option<f64> {
        let color = self.sample_composite_color(at)?;
        let hsb = PickerHSB::from_color(color);
        (hsb.saturation > 0.02).then_some(hsb.hue)
    }

    /// `sampleHueRange(at:)`: the eyedroppers — re-center, widen or narrow the selected range's band.
    pub fn sample_hue_range(&mut self, at: Point) {
        let Some(edit) = self.hue_saturation.as_ref() else { return };
        let Some(mode) = self.hue_sample_mode else { return };
        let mut settings = edit.settings.clone();
        if settings.range == ColorRange::Master || settings.colorize {
            // `NSSound.beep()`: master and Colorize have no band to retarget.
            return;
        }
        let Some(hue) = self.sampled_hue(at) else {
            // `NSSound.beep()`: a gray pixel has no hue to sample.
            return;
        };
        let mut band = settings.band();
        match mode {
            HueSampleMode::Replace => band = band.centered(hue),
            HueSampleMode::Add => band.include(hue),
            HueSampleMode::Remove => band.exclude(hue),
        }
        settings.set_band(band);
        let preview = edit.preview;
        self.update_hue_saturation(&settings, preview);
    }

    /// `beginHueTargeting(at:)`: picks the range owning the sampled color and starts dragging its
    /// saturation (or its hue, with Command held). False when nothing can be targeted.
    pub fn begin_hue_targeting(&mut self, at: Point) -> bool {
        let Some(edit) = self.hue_saturation.as_ref() else { return false };
        if !self.hue_targeting || edit.settings.colorize {
            return false;
        }
        let Some(hue) = self.sampled_hue(at) else { return false };
        let mut settings = edit.settings.clone();
        // `ColorRange.colorRanges.max { settings.weight(of: $0, hue:) < settings.weight(of: $1, hue:) }`:
        // a tie keeps the earlier range, as Swift's `max(by:)` did.
        let mut range = ColorRange::Reds;
        for candidate in ColorRange::COLOR_RANGES {
            if settings.weight(candidate, hue) > settings.weight(range, hue) {
                range = candidate;
            }
        }
        settings.range = range;
        let adjustment = settings.adjustments.get(&range).copied().unwrap_or_default();
        let preview = edit.preview;
        self.hue_target_drag = Some(HueTargetDrag {
            range,
            hue: adjustment.hue,
            saturation: adjustment.saturation,
        });
        self.update_hue_saturation(&settings, preview);
        true
    }

    /// `dragHueTargeting(byViewDelta:adjustsHue:)`: dragging right raises the value, left lowers it;
    /// one unit per view point. Each drag is measured from where it started, so it never accumulates.
    pub fn drag_hue_targeting(&mut self, by_view_delta: f64, adjusts_hue: bool) {
        let Some(edit) = self.hue_saturation.as_ref() else { return };
        let Some(drag) = self.hue_target_drag else { return };
        let mut settings = edit.settings.clone();
        let adjustment = settings.adjustments.entry(drag.range).or_insert_with(RangeAdjustment::default);
        if adjusts_hue {
            adjustment.hue = (drag.hue + by_view_delta / 2.0).clamp(-180.0, 180.0);
        } else {
            adjustment.saturation = (drag.saturation + by_view_delta / 2.0).clamp(-100.0, 100.0);
        }
        let preview = edit.preview;
        self.update_hue_saturation(&settings, preview);
    }

    /// `endHueTargeting()`.
    pub fn end_hue_targeting(&mut self) {
        self.hue_target_drag = None;
    }

    /// `cancelHueSaturation()`: the panel's Cancel, and the window's close button.
    pub fn cancel_hue_saturation(&mut self) {
        if self.finish_adjustment_editing(false) {
            return;
        }
        self.hue_sample_mode = None;
        self.hue_targeting = false;
        self.hue_target_drag = None;
        if self.hue_saturation.is_none() {
            return;
        }
        self.hue_saturation_pending = None;
        // `hueSaturationTask?.cancel()`.
        if let Some(edit) = self.hue_saturation.take() {
            edit.cancel_previews();
        }
        self.brush_revision += 1;
    }

    /// Polls every open editor's preview and histogram workers: the completions that bumped the Swift
    /// `brushRevision`. True when the canvas changed.
    pub fn poll_filter_previews(&mut self) -> bool {
        let mut redraw = false;
        if let Some(edit) = self.filter_edit.as_mut() {
            redraw |= edit.poll();
        }
        if let Some(edit) = self.levels.as_mut() {
            redraw |= edit.poll();
        }
        if let Some(edit) = self.hue_saturation.as_mut() {
            redraw |= edit.poll();
        }
        if redraw {
            self.brush_revision += 1;
        }
        redraw
    }
}

/// A fresh seed of its own (`UInt32.random(in: .min ... .max)`).
fn random_seed() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = now
        .wrapping_add(u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed) as u64);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::layer_adjustment::LevelRange;

    /// A canvas exactly as large as `pixels`, holding one layer with them (`EditorSession.insert`).
    fn session_with_image(pixels: Rgba8Image, name: &str) -> (EditorSession, Id) {
        let mut session = EditorSession::default();
        let (width, height) = (pixels.width(), pixels.height());
        let layer = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(pixels)),
                PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
                name,
            ),
            Point::ZERO,
        );
        let id = layer.id;
        let mut document = CanvasDocument::new(width, height);
        document.layers.push(layer);
        session.document = Some(document);
        session.set_active_layer(Some(id));
        (session, id)
    }

    /// `LevelsTests.session()`: a 6×1 canvas holding a gray ramp, its last pixel transparent.
    fn session_with_ramp() -> (EditorSession, Id) {
        let mut pixels = Rgba8Image::new(6, 1);
        for (x, pixel) in [
            [0, 0, 0, 255],
            [64, 64, 64, 255],
            [128, 128, 128, 255],
            [255, 255, 255, 255],
            [64, 32, 0, 128],
            [0, 0, 0, 0],
        ]
        .into_iter()
        .enumerate()
        {
            pixels.set(x, 0, pixel);
        }
        session_with_image(pixels, "Ramp")
    }

    /// `HueSaturationTests.redAndBlue()`: two pure-red pixels, then two pure-blue ones, all opaque.
    fn session_with_red_and_blue() -> (EditorSession, Id) {
        let mut pixels = Rgba8Image::new(4, 1);
        pixels.set(0, 0, [255, 0, 0, 255]);
        pixels.set(1, 0, [255, 0, 0, 255]);
        pixels.set(2, 0, [0, 0, 255, 255]);
        pixels.set(3, 0, [0, 0, 255, 255]);
        session_with_image(pixels, "RedBlue")
    }

    /// The color cube is interpolated, so allow a few levels (`HueSaturationTests.near`).
    fn near(pixel: [u8; 4], target: [u8; 4]) -> bool {
        pixel
            .iter()
            .zip(target)
            .all(|(value, target)| (i32::from(*value) - i32::from(target)).abs() <= 8)
    }

    fn hue_adjustment(session: &EditorSession, range: ColorRange) -> RangeAdjustment {
        session
            .hue_saturation
            .as_ref()
            .expect("a Hue/Saturation dialog is open")
            .settings
            .adjustments
            .get(&range)
            .copied()
            .unwrap_or_default()
    }

    /// `selectionPreviewCancelCommitUndoAndPersistence`'s session half: a preview leaves the document
    /// alone, Cancel costs nothing, and OK is one undo step named "Levels".
    #[test]
    fn levels_preview_cancel_commit_undo() {
        let (mut session, id) = session_with_ramp();
        let before = session.document.clone();
        let count = session.history.undo_count();
        let mut settings = LevelsSettings::default();
        settings.set_current(LevelRange { output_black: 255.0, output_white: 0.0, ..LevelRange::default() });
        session.begin_levels();
        assert!(session.levels.is_some());
        session.update_levels(&settings, true);
        assert!(session.document == before, "the preview never touches the document");
        assert_eq!(session.history.undo_count(), count);
        session.update_levels(&settings, false);
        assert!(
            session.levels.as_ref().expect("open").preview_image(id).is_none(),
            "preview off drops the preview"
        );
        session.cancel_levels();
        assert!(session.document == before && session.history.undo_count() == count);
        session.begin_levels();
        session.update_levels(&settings, false);
        session.commit_levels();
        assert_eq!(session.history.undo_count(), count + 1);
        assert_eq!(session.history.undo_name(), "Levels");
        let image = session
            .active_layer()
            .and_then(|layer| layer.asset.as_ref())
            .and_then(|asset| asset.image.as_rgba())
            .expect("the committed pixels");
        assert_eq!(image.width(), 6);
        assert_eq!(image.get(0, 0), [255, 255, 255, 255], "output black and white are inverted");
        session.undo();
        assert!(session.document == before, "one Undo restores the layer");
    }

    /// `autoAlgorithmsAndEyedropperCalibration`'s session half: Auto Levels needs a ready histogram and
    /// then picks the same endpoints.
    #[test]
    fn auto_levels_picks_the_same_endpoints() {
        let (mut session, _) = session_with_ramp();
        session.begin_levels();
        session.auto_levels(LevelsAuto::Contrast);
        assert!(!session.levels.as_ref().expect("open").histogram_ready);
        assert_eq!(
            session.levels.as_ref().expect("open").settings,
            LevelsSettings::default(),
            "Auto does nothing before the histogram lands"
        );
        {
            let edit = session.levels.as_mut().expect("open");
            let mut bins = std::array::from_fn(|_| vec![0.0; 256]);
            for channel in 1..=3 {
                bins[channel][20 * channel] = 100.0;
                bins[channel][200 + channel * 10] = 100.0;
            }
            edit.histogram = bins;
            edit.histogram_ready = true;
            edit.sample_mode = Some(LevelsSample::White);
        }
        session.auto_levels(LevelsAuto::Contrast);
        let contrast = session.levels.as_ref().expect("open").settings;
        assert_eq!(contrast.ranges[0].black, 20.0);
        assert_eq!(contrast.ranges[0].white, 230.0);
        assert!(
            session.levels.as_ref().expect("open").sample_mode.is_none(),
            "Auto disarms the eyedropper"
        );
        session.auto_levels(LevelsAuto::Color);
        let color = session.levels.as_ref().expect("open").settings;
        assert_eq!(color.ranges[1].black, 20.0);
        assert_eq!(color.ranges[3].black, 60.0);
        assert_eq!(color.ranges[0], LevelRange::default(), "Color leaves the composite alone");
        session.cancel_levels();
    }

    /// `eyedropperSamplesOriginalAndRejectsTransparentPixels`: a sample reads the original's straight
    /// channels; a transparent pixel and a point outside the canvas are ignored.
    #[test]
    fn eyedropper_samples_original_and_rejects_transparent_pixels() {
        let (mut session, _) = session_with_ramp();
        session.begin_levels();
        session.levels.as_mut().expect("open").sample_mode = Some(LevelsSample::Gray);
        session.sample_levels(Point::new(2.5, 0.5));
        let first = session.levels.as_ref().expect("open").settings;
        let expected = (128.0f64 / 255.0).ln() / 0.5f64.ln();
        assert_eq!(first.ranges[1].black, 0.0);
        assert!((first.ranges[1].gamma - expected).abs() < 1e-9, "the mid gray set the gamma");
        session.sample_levels(Point::new(2.5, 0.5));
        assert_eq!(session.levels.as_ref().expect("open").settings, first, "a repeat sample changes nothing");
        session.sample_levels(Point::new(5.5, 0.5));
        assert_eq!(session.levels.as_ref().expect("open").settings, first, "a transparent pixel is ignored");
        session.sample_levels(Point::new(-1.0, 0.5));
        assert_eq!(session.levels.as_ref().expect("open").settings, first, "outside the canvas is ignored");
        session.cancel_levels();
    }

    /// `previewIsLiveDoesNotTouchTheDocumentAndNeverAccumulates`'s session half: the preview is never
    /// written to the document, preview off drops it, Cancel is free, and OK is one undo step named
    /// "Hue/Saturation".
    #[test]
    fn hue_saturation_preview_never_touches_the_document() {
        let (mut session, id) = session_with_red_and_blue();
        let before = session.document.clone();
        let count = session.history.undo_count();
        let settings = HueSaturationSettings::new(120.0, 0.0, 0.0, false, ColorRange::Master);
        session.begin_hue_saturation();
        assert!(session.hue_saturation.is_some());
        session.update_hue_saturation(&settings, true);
        assert!(session.document == before, "the preview never touches the document");
        assert_eq!(session.history.undo_count(), count);
        session.update_hue_saturation(&settings, false);
        assert!(
            session.hue_saturation.as_ref().expect("open").preview_image(id).is_none(),
            "preview off drops back to the layer's own pixels"
        );
        session.cancel_hue_saturation();
        assert!(session.document == before && session.hue_saturation.is_none());
        session.begin_hue_saturation();
        session.update_hue_saturation(&settings, false);
        session.commit_hue_saturation();
        assert_eq!(session.history.undo_count(), count + 1);
        assert_eq!(session.history.undo_name(), "Hue/Saturation");
        let image = session
            .active_layer()
            .and_then(|layer| layer.asset.as_ref())
            .and_then(|asset| asset.image.as_rgba())
            .expect("the committed pixels");
        assert!(near(image.get(0, 0), [0, 255, 0, 255]), "reds rotated to green");
        assert!(near(image.get(2, 0), [255, 0, 0, 255]), "master shifts every hue: blue rotates to red");
        session.undo();
        assert!(session.document == before);
    }

    /// `targetedAdjustmentPicksTheRangeUnderTheCursor`'s drag half: each drag is measured from where it
    /// started, so it never accumulates.
    #[test]
    fn drag_hue_targeting_never_accumulates() {
        let (mut session, _) = session_with_red_and_blue();
        session.begin_hue_saturation();
        session.hue_targeting = true;
        session.hue_target_drag = Some(HueTargetDrag { range: ColorRange::Blues, hue: 0.0, saturation: 0.0 });
        session.drag_hue_targeting(60.0, false);
        assert_eq!(hue_adjustment(&session, ColorRange::Blues).saturation, 30.0);
        session.drag_hue_targeting(20.0, false);
        assert_eq!(hue_adjustment(&session, ColorRange::Blues).saturation, 10.0);
        session.drag_hue_targeting(-40.0, true); // Command holds hue.
        assert_eq!(hue_adjustment(&session, ColorRange::Blues).hue, -20.0);
        session.end_hue_targeting();
        assert!(session.hue_target_drag.is_none());
        session.cancel_hue_saturation();
        assert!(session.hue_saturation.is_none());
    }

    /// `samplingNeedsAColorRangeAndAColorfulPixel`'s first half: Master has no band to retarget, and
    /// Cancel disarms the eyedropper.
    #[test]
    fn sampling_needs_a_color_range() {
        let (mut session, _) = session_with_red_and_blue();
        session.begin_hue_saturation();
        let untouched = session.hue_saturation.as_ref().expect("open").settings.band();
        session.hue_sample_mode = Some(HueSampleMode::Replace);
        session.sample_hue_range(Point::new(0.5, 0.5));
        assert_eq!(session.hue_saturation.as_ref().expect("open").settings.band(), untouched);
        session.cancel_hue_saturation();
        assert!(session.hue_sample_mode.is_none());
    }
}
