//! The Brush family's session state and commands: the port of `Document/EditorSession+Brush.swift`,
//! the session-facing parts of `Document/BrushStroke.swift` (`parkedBrushTips`, the Shift straight
//! line, the smoothing string, the size/opacity/hardness keys), `Document/BlurTool.swift`,
//! `Document/SmudgeLiquify.swift`'s stroke commands, and `Document/CloneStamp.swift`.
//!
//! **Async → sync (docs/PORTING.md §4).** Swift coordinated these commands with `Task`/`await`:
//! `finishBrush()` awaited `finishBrushImmediately()`, `commitRasterEdit` awaited the `BrushCommit`
//! actor, and `resolveGradient` fired a detached `Task`. Every command here is synchronous: the same
//! work runs inline, with `is_project_busy` held across it so the observable `shows_busy`,
//! `is_project_busy` and `can_undo` states stay exactly as the Swift produced them.
//!
//! The pixel work itself (`BrushStroke`, `BrushRaster`, the commit assembly) lives in
//! `compositor_pixels::brush`; this module owns the session state it reads and the commands that
//! drive it.

use std::sync::Arc;

use compositor_core::blend::LayerBlendMode;
use compositor_core::buffer::SharedImage;
use compositor_core::document::{CanvasDocument, ImageLayer, NavigationTool, ProjectError};
use compositor_core::error::CoreError;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::LayerMask;
use compositor_core::limits;
use compositor_core::raster::{RasterSnapshot, THUMBNAIL_MAX_SIDE};
use compositor_core::Id;
use compositor_pixels::brush::{BrushCommit, BrushSettings, BrushStroke, CloneSample};
use compositor_pixels::canvas::Canvas;
use compositor_pixels::warp::{BlurToolMode, BrushToolMode, WarpStroke};
use compositor_render::LayerRenderer;
use rustc_hash::FxHashMap;

use crate::session::EditorSession;

/// One brush family's parked tip: the size, hardness and opacity it had when the tool left it
/// (`parkedBrushTips`'s value).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrushTip {
    pub diameter: f64,
    pub hardness: f64,
    pub opacity: f64,
}

impl BrushTip {
    pub const fn new(diameter: f64, hardness: f64, opacity: f64) -> Self {
        Self {
            diameter,
            hardness,
            opacity,
        }
    }
}

/// The Brush and Spot Healing Brush share the first family.
pub const BRUSH_FAMILY_SHARED: i32 = 0;
/// Clone Stamp keeps its own tip.
pub const BRUSH_FAMILY_CLONE_STAMP: i32 = 1;
/// Smear (the Blur tool) keeps its own tip.
pub const BRUSH_FAMILY_BLUR: i32 = 2;

/// The tips of the brush families not in use: Clone Stamp and Smear each keep their own size,
/// hardness and opacity (both starting soft); the other brushes share one.
///
/// `parkedBrushTips` was `[Int: (diameter: CGFloat, hardness: CGFloat, opacity: CGFloat)]` with
/// `[1: (40, 0, 1), 2: (40, 0, 1)]`; only the two named families start parked — the shared family is
/// added the first time a tool leaves it.
#[derive(Clone, Debug)]
pub struct ParkedBrushTips(FxHashMap<i32, BrushTip>);

impl Default for ParkedBrushTips {
    fn default() -> Self {
        let mut tips = FxHashMap::default();
        tips.insert(BRUSH_FAMILY_CLONE_STAMP, BrushTip::new(40.0, 0.0, 1.0));
        tips.insert(BRUSH_FAMILY_BLUR, BrushTip::new(40.0, 0.0, 1.0));
        Self(tips)
    }
}

impl ParkedBrushTips {
    /// `tipFamily(_:)`: the two tools with a tip of their own, and the family the rest share.
    pub fn family_of(tool: NavigationTool) -> i32 {
        match tool {
            NavigationTool::CloneStamp => BRUSH_FAMILY_CLONE_STAMP,
            NavigationTool::Blur => BRUSH_FAMILY_BLUR,
            _ => BRUSH_FAMILY_SHARED,
        }
    }

    pub fn tip(&self, family: i32) -> Option<BrushTip> {
        self.0.get(&family).copied()
    }

    pub fn park(&mut self, family: i32, tip: BrushTip) {
        self.0.insert(family, tip);
    }

    /// `selectTool`'s parking step: leaving `from` for `to`, the tip in use is parked under `from`
    /// and the tip parked under `to` is restored. Nothing happens when `to` has never been parked
    /// and no tip was ever left there.
    pub fn hand_off(&mut self, from: i32, to: i32, settings: &mut BrushSettings) {
        if from == to {
            return;
        }
        let Some(parked) = self.0.get(&to).copied() else { return };
        self.park(from, BrushTip::new(settings.diameter, settings.hardness, settings.opacity));
        settings.diameter = parked.diameter;
        settings.hardness = parked.hardness;
        settings.opacity = parked.opacity;
    }

    /// How many families have a parked tip; the two named ones start parked.
    pub fn parked_families(&self) -> usize {
        self.0.len()
    }
}

/// Where the last brush stroke ended, so a Shift-click paints a straight line on from it
/// (`lastBrushPoint`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastBrushPoint {
    pub point: Point,
    pub layer_id: Id,
    pub mask: bool,
}

/// The digit and time of a pending two-digit opacity entry (`pendingOpacityDigit`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PendingOpacityDigit {
    pub digit: i32,
    pub time: f64,
}

/// A Blur stroke's sample and the way to make any part of it: the softened copy of the layer that
/// the tip paints through, in place (`blurSample(for:)`).
pub struct BlurSample {
    /// The image painted through the tip, where it sits, and that it is in the stroke's own grid.
    pub sample: CloneSample,
    /// Makes part of the softened sample, given a rect of its pixels (top-left rows), in place of
    /// cropping it: only what the brush reaches is ever blurred. This is `BrushStroke.cloneRender`.
    pub render: Arc<dyn Fn(Rect) -> Option<PixelImage> + Send + Sync>,
}

/// `ProcessInfo.processInfo.systemUptime`, the monotonic clock the opacity-digit timer reads.
static UPTIME_EPOCH: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);

pub fn system_uptime() -> f64 {
    UPTIME_EPOCH.elapsed().as_secs_f64()
}

/// The session's user-visible failure for a pixel-engine refusal. A size refusal reads exactly like
/// `ProjectError.tooLarge`; anything else keeps the engine's own wording.
pub fn brush_failure(error: &CoreError) -> String {
    match error {
        CoreError::TooLarge(_) => ProjectError::TooLarge.to_string(),
        other => other.to_string(),
    }
}

/// Swift's `lhs.asset?.image === rhs.asset?.image`: the two shared rasters are the same allocation.
fn same_image(lhs: Option<&ImportedImage>, rhs: Option<&ImportedImage>) -> bool {
    match (lhs, rhs) {
        (None, None) => true,
        (Some(lhs), Some(rhs)) => shared_pixels_identical(&lhs.image, &rhs.image),
        _ => false,
    }
}

/// Swift's `CGImage === CGImage` for the two raster kinds a layer or mask carries.
fn shared_pixels_identical(lhs: &PixelImage, rhs: &PixelImage) -> bool {
    match (lhs, rhs) {
        (PixelImage::Rgba(lhs), PixelImage::Rgba(rhs)) => Arc::ptr_eq(lhs, rhs),
        (PixelImage::Gray(lhs), PixelImage::Gray(rhs)) => Arc::ptr_eq(lhs, rhs),
        _ => false,
    }
}

impl EditorSession {
    /// An explicitly empty selection leaves nothing paintable, so painting never starts.
    pub fn can_paint(&self) -> bool {
        // A folder has no pixels of its own, so only its mask can be painted.
        self.can_edit_layers()
            && self.selected_layer_ids.len() == 1
            && (!self.active_layer().map(|layer| layer.is_group).unwrap_or(false) || self.is_mask_selected)
            && !self.selection().map(|selection| selection.is_empty()).unwrap_or(false)
            && self
                .active_layer_id
                .map(|id| {
                    self.document
                        .as_ref()
                        .map(|document| document.effective_visible_ids().contains(&id))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
            && (!self.is_mask_selected
                || self
                    .active_layer()
                    .and_then(|layer| layer.mask.as_ref())
                    .map(|mask| mask.is_enabled)
                    .unwrap_or(false))
            && (self.is_mask_selected || self.active_layer().map(|layer| layer.adjustment.is_none()).unwrap_or(false))
    }

    /// Why a stroke can't start on the target, for the alert, as Photoshop explains a brush it
    /// refuses. Nil when nothing about the target is in the way; while the editor is busy (a
    /// transform, a dialog) a press just waits.
    pub fn paint_refusal(&self) -> Option<String> {
        if !self.can_edit_layers() {
            return None;
        }
        let layer = self.active_layer()?;
        if self.can_paint() {
            return None;
        }
        if self.selected_layer_ids.len() > 1 {
            return Some("Several layers are selected. Select just one to paint on it.".to_string());
        }
        if layer.is_group && !self.is_mask_selected {
            return Some(format!(
                "“{}” is a folder, which has no pixels of its own. Paint on a layer inside it, or on the folder’s mask.",
                layer.name
            ));
        }
        if self.document.as_ref().map(|document| document.effective_visible_ids().contains(&layer.id)) != Some(true) {
            return Some(format!("“{}” is hidden, or inside a hidden folder. Show it to paint on it.", layer.name));
        }
        if self.is_mask_selected && layer.mask.as_ref().map(|mask| mask.is_enabled) != Some(true) {
            return Some(
                "The layer mask is turned off. Shift-click its thumbnail to turn it on, then paint.".to_string(),
            );
        }
        if !self.is_mask_selected && layer.adjustment.is_some() {
            return Some(format!(
                "“{}” is an adjustment layer, with no pixels to paint. Paint on its mask instead.",
                layer.name
            ));
        }
        if self.selection().map(|selection| selection.is_empty()) == Some(true) {
            return Some("Nothing is selected, so there’s nowhere to paint. Choose Select › Deselect (⌘D) to paint anywhere.".to_string());
        }
        None
    }

    /// Tiled raster edit of the active layer's pixels or mask, within the shared pixel budgets.
    pub fn make_raster_edit(
        &self,
        layer: &ImageLayer,
        settings: BrushSettings,
        grows_mask: bool,
    ) -> Result<BrushStroke, CoreError> {
        let Some(document) = self.document.as_ref() else {
            return Err(CoreError::TooLarge(limits::document_budget_megapixels()));
        };
        let mut stroke = BrushStroke::new(layer, self.is_mask_selected, settings, document.size(), grows_mask)?;
        let used: usize = document
            .layers
            .iter()
            .filter(|other| other.id != layer.id)
            .map(|other| {
                let image = if self.is_mask_selected {
                    other.mask.as_ref().map(|mask| &mask.asset.image)
                } else {
                    other.asset.as_ref().map(|asset| &asset.image)
                };
                image.map(|image| image.width() * image.height()).unwrap_or(0)
            })
            .sum();
        stroke.pixel_limit = limits::document_pixel_budget().saturating_sub(used);
        stroke.selection_clip = self.selection().map(|selection| selection.clip(document.size()));
        if !self.is_mask_selected && layer.mask.is_some() {
            let mask_pixels: usize = document
                .layers
                .iter()
                .filter(|other| other.id != layer.id)
                .map(|other| other.mask.as_ref().map(|mask| mask.asset.image.pixel_count()).unwrap_or(0))
                .sum();
            stroke.pixel_limit = stroke.pixel_limit.min(limits::document_pixel_budget().saturating_sub(mask_pixels));
        }
        Ok(stroke)
    }

    pub fn begin_brush(&mut self, point: Point) {
        // Spot Healing and Clone Stamp rework image pixels; they have nothing to do on a mask.
        if self.tool == NavigationTool::Blur && self.blur_mode != BlurToolMode::Blur {
            self.begin_warp(point);
            return;
        }
        if !(self.tool == NavigationTool::Brush
            || self.tool == NavigationTool::Blur
            || (self.tool.is_brush_tool() && !self.is_mask_selected))
        {
            return;
        }
        if !self.can_paint() || self.active_layer().is_none() || self.document.is_none() {
            self.brush_error = self.paint_refusal();
            return;
        }
        let layer = self.active_layer().cloned().expect("checked above");
        let mut source_offset: Option<Size> = None;
        if self.tool == NavigationTool::CloneStamp {
            match self.clone_stroke_offset(point) {
                Some(offset) => source_offset = Some(offset),
                None => {
                    self.brush_error = Some("Option-click where Clone Stamp should copy from first.".to_string());
                    return;
                }
            }
        }
        self.finish_opacity_edit();
        let mut settings = self.brush_settings.clone();
        settings.healing = self.tool == NavigationTool::SpotHealing;
        settings.erasing =
            self.tool == NavigationTool::Brush && self.brush_mode == BrushToolMode::Erase && !self.is_mask_selected;
        settings.healing_mode = self.spot_healing_mode;
        if self.is_mask_selected {
            settings.red = if self.mask_paint_white { 1.0 } else { 0.0 };
            settings.green = settings.red;
            settings.blue = settings.red;
        }
        let grows_mask = self.tool == NavigationTool::Brush;
        let mut stroke = match self.make_raster_edit(&layer, settings, grows_mask) {
            Ok(stroke) => stroke,
            Err(error) => {
                self.cancel_brush();
                self.brush_error = Some(brush_failure(&error));
                return;
            }
        };
        if let Some(offset) = source_offset {
            let sample = self
                .document
                .as_ref()
                .and_then(|document| self.clone_sample(document, &stroke, offset));
            let Some(sample) = sample else { return };
            self.clone_offset = Some(offset);
            stroke.clone = Some(sample);
        }
        // Blur paints a softened copy of the layer, in place, through the brush tip.
        if self.tool == NavigationTool::Blur {
            let Some(blur) = self.blur_sample(&stroke) else { return };
            stroke.clone = Some(blur.sample);
            stroke.clone_render = Some(blur.render);
        }
        stroke.is_blur = self.tool == NavigationTool::Blur;
        if let Err(error) = stroke.append(point) {
            self.cancel_brush();
            self.brush_error = Some(brush_failure(&error));
            return;
        }
        self.brush_stroke = Some(stroke);
        self.brush_anchor = Some(point);
        self.brush_pointer = Some(point);
        self.last_brush_point = Some(LastBrushPoint {
            point,
            layer_id: layer.id,
            mask: self.is_mask_selected,
        });
        self.brush_revision += 1;
    }

    pub fn continue_brush(&mut self, point: Point) {
        if self.warp_stroke.is_some() {
            if let Some(warp) = self.warp_stroke.as_mut() {
                warp.append(point);
            }
            if let Some(last) = self.last_brush_point.as_mut() {
                last.point = point;
            }
            self.brush_revision += 1;
            return;
        }
        if self.brush_stroke.is_none() {
            return;
        }
        self.brush_pointer = Some(point);
        let Some(painted) = self.smoothed(point) else { return };
        let result = self.brush_stroke.as_mut().expect("checked above").append(painted);
        match result {
            Ok(()) => {
                if let Some(last) = self.last_brush_point.as_mut() {
                    last.point = painted;
                }
                self.brush_revision += 1;
            }
            Err(error) => {
                self.cancel_brush();
                self.brush_error = Some(brush_failure(&error));
            }
        }
    }

    /// Where the brush actually is, with Smoothing on: it trails the pointer on a string, and only
    /// moves once the pointer pulls that string taut — the model Photoshop uses. The string's length
    /// is in screen points, so it feels the same however far the canvas is zoomed in. Nil while the
    /// string is still slack, which is the whole point: those jitters never reach the stroke.
    fn smoothed(&mut self, point: Point) -> Option<Point> {
        if !(self.tool == NavigationTool::Brush && self.brush_settings.smoothing > 0.0) {
            return Some(point);
        }
        let Some(anchor) = self.brush_anchor else { return Some(point) };
        let radius = self.brush_settings.smoothing / 0.01f64.max(self.viewport.zoom());
        let delta = Point::new(point.x - anchor.x, point.y - anchor.y);
        let distance = delta.x.hypot(delta.y);
        if distance <= radius {
            return None;
        }
        let step = (distance - radius) / distance;
        let moved = Point::new(anchor.x + delta.x * step, anchor.y + delta.y * step);
        self.brush_anchor = Some(moved);
        Some(moved)
    }

    /// Where a Shift-click paints a line from: the end of the last stroke, while the same layer (or
    /// mask) is the target.
    pub fn shift_line_start(&self) -> Option<Point> {
        let last = self.last_brush_point?;
        if self.active_layer_id != Some(last.layer_id) || last.mask != self.is_mask_selected {
            return None;
        }
        Some(last.point)
    }

    pub fn cancel_brush(&mut self) {
        self.warp_stroke = None;
        self.warp_layer = None;
        self.brush_stroke = None;
        self.brush_anchor = None;
        self.brush_pointer = None;
        self.brush_revision += 1;
    }

    /// Called directly by mouse-up, before the next input event can be handled.
    pub fn finish_brush_immediately(&mut self) -> bool {
        if self.warp_stroke.is_some() {
            if self.is_project_busy {
                return false;
            }
            self.finish_warp();
            return true;
        }
        if self.brush_stroke.is_none() {
            return true;
        }
        if self.is_project_busy {
            return false;
        }
        // Smoothing leaves the brush short of the pointer; the stroke ends where the hand did.
        let pointer = self.brush_pointer;
        let anchor = self.brush_anchor;
        let smoothing = self.brush_settings.smoothing;
        let is_brush = self.tool == NavigationTool::Brush;
        let mut stroke = self.brush_stroke.take().expect("checked above");
        // `defer { cancelBrush() }`: the stroke's own state is cleared whatever happens below.
        self.cancel_brush();
        let outcome = (|| -> Result<(), CoreError> {
            if let (Some(pointer), Some(anchor)) = (pointer, anchor) {
                if pointer != anchor && is_brush && smoothing > 0.0 {
                    stroke.append(pointer)?;
                }
            }
            stroke.flush()?;
            if stroke.settings.healing {
                stroke.heal()?;
            }
            if !stroke.patches().is_empty() {
                self.commit_paint_snapshot(&stroke)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            self.brush_error = Some(brush_failure(&error));
        }
        true
    }

    /// Swift's `async func finishBrush()` awaited nothing but the work below, so it is synchronous
    /// here (docs/PORTING.md §4).
    pub fn finish_brush(&mut self) {
        self.finish_brush_immediately();
    }

    /// Install immutable tiles immediately, including the undo entry. The next stroke and other
    /// tools can start without awaiting full-image assembly.
    pub fn commit_paint_snapshot(&mut self, stroke: &BrushStroke) -> Result<(), CoreError> {
        let result = stroke.paint_snapshot()?;
        if !result.transform.is_valid() {
            return Ok(());
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == stroke.layer.id))
        else {
            return Ok(());
        };
        let current = self.document.as_ref().expect("the index came from it").layers[index].clone();
        if !same_image(current.asset.as_ref(), stroke.layer.asset.as_ref()) || current.transform != stroke.layer.transform {
            return Ok(());
        }
        let mut mask = current.mask.clone();
        if !stroke.is_mask {
            if let Some(original) = mask.as_ref() {
                if original.placement.is_none() && result.bounds != stroke.source_rect {
                    let raster =
                        RasterSnapshot::replacing(Some(&original.asset), stroke.source_rect, &[], result.bounds, true, 1.0);
                    let image = raster.materialize();
                    let thumbnail = raster.thumbnail(THUMBNAIL_MAX_SIDE);
                    let asset = ImportedImage::with_raster(image, thumbnail, original.asset.name.clone(), Some(raster));
                    mask = Some(original.replacing(asset));
                }
            }
        }
        let name = stroke.edit_name.clone().unwrap_or_else(|| {
            if stroke.is_mask {
                "Paint Mask".to_string()
            } else if stroke.settings.erasing {
                "Erase".to_string()
            } else if stroke.is_blur {
                "Blur".to_string()
            } else if stroke.clone.is_some() {
                "Clone Stamp".to_string()
            } else if stroke.settings.healing {
                "Spot Healing".to_string()
            } else {
                "Brush Stroke".to_string()
            }
        });
        self.begin_edit(&name);
        if stroke.is_mask {
            let painted_transform = result.transform;
            let grew = result.bounds != stroke.source_rect;
            let document = self.document.as_mut().expect("checked above");
            document.layers[index].mask = match mask {
                Some(mask) => {
                    let mut painted = mask.replacing(result.asset.clone());
                    // Grown past its layer, or already placed on its own: the mask keeps its place
                    // on the document. A linked one still moves with its layer.
                    if painted.placement.is_some() || grew {
                        painted.placement = Some(painted_transform);
                    }
                    Some(painted)
                }
                None => Some(LayerMask::new(result.asset.clone())),
            };
        } else {
            let document = self.document.as_mut().expect("checked above");
            document.layers[index] = ImageLayer::with_id(
                current.id,
                Some(result.asset.clone()),
                current.name.clone(),
                current.is_visible,
                result.transform,
                current.parent_id,
                false,
                current.opacity,
                current.blend_mode,
                mask,
                current.mask_source_id,
                // Painting makes the layer plain pixels: the Swift memberwise initializer's defaults
                // leave the adjustment, shape and text metadata nil, and so does this.
                None,
                None,
                current.effects.clone(),
                None,
            );
        }
        self.end_edit();
        Ok(())
    }

    /// Assembles a raster edit and replaces the layer's pixels or mask as one undo step. Other layer
    /// properties are read at commit time. `also_apply` runs between the document write and
    /// `endEdit`, so a caller can fold its own state into the same undo entry.
    ///
    /// Swift's version was `async throws` because it awaited the `BrushCommit` actor; the port calls
    /// `BrushCommit::render` inline (docs/PORTING.md §4), holding `is_project_busy` across the work.
    pub fn commit_raster_edit(
        &mut self,
        stroke: &BrushStroke,
        name: &str,
        also_apply: Option<&mut dyn FnMut(&mut EditorSession)>,
    ) -> Result<(), CoreError> {
        // Swift's `defer { isProjectBusy = false }`: the flag is cleared whatever the commit does.
        self.is_project_busy = true;
        let outcome = self.commit_raster_edit_inner(stroke, name, also_apply);
        self.is_project_busy = false;
        outcome
    }

    fn commit_raster_edit_inner(
        &mut self,
        stroke: &BrushStroke,
        name: &str,
        also_apply: Option<&mut dyn FnMut(&mut EditorSession)>,
    ) -> Result<(), CoreError> {
        if !stroke.committed_transform().is_valid() {
            return Err(CoreError::TooLarge(limits::document_budget_megapixels()));
        }
        let input = stroke.commit_input();
        let result = BrushCommit::render(&input)?;
        let committed = stroke.committed_bounds();
        let transform = stroke.transform_for(result.pixel_bounds.offset_by(committed.min_x(), committed.min_y()));
        if !transform.is_valid() {
            return Err(CoreError::TooLarge(limits::document_budget_megapixels()));
        }
        let mut mask = stroke.layer.mask.clone();
        if !stroke.is_mask {
            if let Some(original) = mask.as_ref() {
                if original.placement.is_none() {
                    mask = Some(original.replacing(BrushCommit::expand_mask(&original.asset, &input, result.pixel_bounds)?));
                }
            }
        }
        // The raster was built from this layer's pixels, transform, and mask; never write it over
        // content that changed underneath it.
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == stroke.layer.id))
        else {
            return Ok(());
        };
        let current = self.document.as_ref().expect("the index came from it").layers[index].clone();
        if !same_image(current.asset.as_ref(), stroke.layer.asset.as_ref())
            || current.transform != stroke.layer.transform
            || !same_image(
                current.mask.as_ref().map(|mask| &mask.asset),
                stroke.layer.mask.as_ref().map(|mask| &mask.asset),
            )
        {
            return Ok(());
        }
        self.begin_edit(name);
        if stroke.is_mask {
            let bounds = result.pixel_bounds.offset_by(committed.min_x(), committed.min_y());
            let grew = bounds != stroke.source_rect;
            let document = self.document.as_mut().expect("checked above");
            document.layers[index].mask = match mask {
                Some(mask) => {
                    let mut edited = mask.replacing(result.asset.clone());
                    // Grown past its layer, or already placed on its own: the mask keeps its place on
                    // the document.
                    if edited.placement.is_some() || grew {
                        edited.placement = Some(transform);
                    }
                    Some(edited)
                }
                None => Some(LayerMask::new(result.asset.clone())),
            };
        } else {
            let kept_enabled = current.mask.as_ref().map(|mask| mask.is_enabled);
            let mask = mask.map(|mask| {
                let mut kept = mask;
                kept.is_enabled = kept_enabled.unwrap_or(kept.is_enabled);
                kept
            });
            let document = self.document.as_mut().expect("checked above");
            document.layers[index] = ImageLayer::with_id(
                current.id,
                Some(result.asset.clone()),
                current.name.clone(),
                current.is_visible,
                transform,
                current.parent_id,
                false,
                current.opacity,
                current.blend_mode,
                mask,
                current.mask_source_id,
                None,
                None,
                current.effects.clone(),
                None,
            );
        }
        if let Some(also_apply) = also_apply {
            also_apply(self);
        }
        self.end_edit();
        Ok(())
    }

    /// Tools where number keys set opacity: the brush or gradient opacity, or with Move/Transform
    /// the opacity of the selected layers.
    pub fn uses_opacity_keys(&self) -> bool {
        self.tool.is_brush_tool() || self.tool == NavigationTool::Gradient || self.tool == NavigationTool::Move
    }

    /// Photoshop-style opacity keys: 1 = 10% … 9 = 90%, 0 = 100%.
    /// Two digits typed quickly set an exact value (4 then 5 = 45%, 0 then 5 = 5%).
    pub fn type_opacity_digit_now(&mut self, digit: i32) {
        self.type_opacity_digit(digit, system_uptime());
    }

    pub fn type_opacity_digit(&mut self, digit: i32, at: f64) {
        if !self.uses_opacity_keys() || self.brush_stroke.is_some() || self.is_project_busy || !(0..=9).contains(&digit) {
            return;
        }
        let mut percent = if digit == 0 { 100 } else { digit * 10 };
        match self.pending_opacity_digit {
            Some(pending) if at - pending.time < 0.6 => {
                percent = 1.max(pending.digit * 10 + digit);
                self.pending_opacity_digit = None;
            }
            _ => self.pending_opacity_digit = Some(PendingOpacityDigit { digit, time: at }),
        }
        let value = percent as f64 / 100.0;
        match self.tool {
            NavigationTool::Brush | NavigationTool::SpotHealing | NavigationTool::CloneStamp | NavigationTool::Blur => {
                self.brush_settings.opacity = value;
                self.refresh_gradient();
            }
            NavigationTool::Gradient => {
                self.gradient_settings.opacity = value;
                self.refresh_gradient();
            }
            _ => self.set_selected_layers_opacity(value),
        }
    }

    /// Shift-[ / Shift-]: hardness in Photoshop's 25% steps (0, 25, 50, 75, 100%).
    pub fn change_brush_hardness(&mut self, increase: bool) {
        if self.brush_stroke.is_some() {
            return;
        }
        // Snap to the next step up or down, so 80% goes to 100% or 75%.
        let quarter = self.brush_settings.hardness * 4.0;
        let step = if increase { (quarter + 0.001).floor() + 1.0 } else { (quarter - 0.001).ceil() - 1.0 };
        self.brush_settings.hardness = step.min(4.0).max(0.0) / 4.0;
        self.refresh_gradient();
    }

    /// `[` / `]`: the brush tip's size. A step of a fifth, but always at least one pixel: 2 shrunk by
    /// a fifth would otherwise round back to 2, leaving the smallest brushes out of reach.
    pub fn change_brush_size(&mut self, increase: bool) {
        if self.brush_stroke.is_some() {
            return;
        }
        let current = self.brush_settings.diameter;
        let stepped = if increase {
            (current + 1.0).max((current * 1.2).round())
        } else {
            (current - 1.0).min((current / 1.2).round())
        };
        self.brush_settings.diameter = stepped.min(2000.0).max(1.0);
        self.refresh_gradient();
    }

    /// `selectTool`'s tip hand-off: parks the tip of the family being left and restores the tip of
    /// the family being entered (`parkedBrushTips`).
    pub fn hand_off_brush_tips(&mut self, to: NavigationTool) {
        let from = ParkedBrushTips::family_of(self.tool);
        let to_family = ParkedBrushTips::family_of(to);
        if from == to_family {
            return;
        }
        let mut settings = self.brush_settings.clone();
        self.parked_brush_tips.hand_off(from, to_family, &mut settings);
        if settings != self.brush_settings {
            self.brush_settings = settings;
            self.refresh_gradient();
        }
    }

    /// The `didSet` on `brushSettings`.
    pub fn set_brush_settings(&mut self, settings: BrushSettings) {
        self.brush_settings = settings;
        self.refresh_gradient();
    }

    // MARK: Clone Stamp

    /// Option-click: where Clone Stamp copies from. A new source starts a new alignment.
    pub fn set_clone_source(&mut self, point: Point) {
        if !(point.x.is_finite() && point.y.is_finite()) {
            return;
        }
        self.clone_source = Some(point);
        self.clone_offset = None;
    }

    /// The whole-pixel offset a stroke starting at `point` would copy with: aligned strokes keep the
    /// first stroke's; otherwise it runs from the brush to the source. Nil without a source. Shared
    /// by the stroke and the hover preview, so the preview is exactly what a click stamps.
    pub fn clone_stroke_offset(&self, point: Point) -> Option<Size> {
        let source = self.clone_source?;
        if self.clone_settings.aligned {
            if let Some(offset) = self.clone_offset {
                return Some(offset);
            }
        }
        Some(Size::new((source.x - point.x).round(), (source.y - point.y).round()))
    }

    /// Where the source sits for a brush at `point` (document pixels), for the canvas's crosshair:
    /// the source itself until a stroke fixes the offset.
    pub fn clone_sample_point(&self, point: Point) -> Option<Point> {
        let source = self.clone_source?;
        match self.clone_offset {
            Some(offset) if self.clone_settings.aligned || self.brush_stroke.is_some() => {
                Some(Point::new(point.x + offset.width, point.y + offset.height))
            }
            _ => Some(source),
        }
    }

    /// What `stroke` copies from, taken when it starts, placed `offset` document pixels from where it
    /// paints: the layer's own pixels, at their own resolution; or, sampling all layers, the canvas
    /// as it shows them.
    pub fn clone_sample(&self, document: &CanvasDocument, stroke: &BrushStroke, offset: Size) -> Option<CloneSample> {
        if !self.clone_settings.sample_all_layers {
            let image = stroke.layer.asset.as_ref()?.image.clone();
            return Some(CloneSample {
                image,
                placed: stroke.grid_rect(stroke.source_rect, offset),
                in_grid: true,
            });
        }
        let mut canvas = Canvas::new_rgba(document.width, document.height);
        self.draw_live_composite(document, &mut canvas, false);
        let image = canvas.into_rgba();
        let placed = Rect::new(-offset.width, -offset.height, image.width() as f64, image.height() as f64);
        Some(CloneSample {
            image: PixelImage::Rgba(Arc::new(image)),
            placed,
            in_grid: false,
        })
    }

    // MARK: Blur (Smear's Blur mode)

    /// What a Blur stroke paints: the layer's own pixels (or, painting the mask, its mask), softened
    /// by the Radius set in the options bar, measured on the canvas, at the layer's own resolution.
    /// It is taken when the stroke starts, so going over an area again in a new stroke softens it
    /// further, as in Photoshop. `render` makes any part of the softened sample, so only what the
    /// brush reaches is ever blurred; `image` is the sharp sample, the same size.
    pub fn blur_sample(&self, stroke: &BrushStroke) -> Option<BlurSample> {
        let layer = &stroke.layer;
        let image = if stroke.is_mask {
            layer.mask.as_ref()?.asset.image.clone()
        } else {
            layer.asset.as_ref()?.image.clone()
        };
        // The canvas's softening, carried into the layer's pixels: wider there when the layer is
        // scaled down.
        let map = stroke.pixel_to_document;
        let per_pixel = (map.a * map.d - map.b * map.c).abs().sqrt().max(1e-6);
        let source_rect = stroke.source_rect;
        let side_pixels = source_rect.width().max(source_rect.height());
        let sigma = (self.brush_settings.blur_radius.clamp(0.5, 50.0) / per_pixel).min(side_pixels / 2.0);
        // Room for the blur to spread past the pixels' edges, as it does on the canvas.
        let margin = (3.0 * sigma).ceil();
        let region = source_rect.inset_by(-margin, -margin);
        // At the layer's own resolution up to a budget of several canvases; a huge layer's sample is
        // made coarser instead (the stroke scales it back over the layer), rather than a surface too
        // large to make at every stroke.
        let canvas = self.document.as_ref().map(|document| document.width * document.height).unwrap_or(0);
        let budget = (limits::MAX_SURFACE_PIXELS as f64).min((16_000_000.0f64).max(4.0 * canvas as f64));
        let fit = (budget / (region.width() * region.height())).sqrt().min(1.0);
        let width = 1.max((region.width() * fit).ceil() as usize);
        let height = 1.max((region.height() * fit).ceil() as usize);
        let placed = Rect::new(
            margin * fit,
            margin * fit,
            source_rect.width() * fit,
            source_rect.height() * fit,
        );
        let sharp = if stroke.is_mask {
            let mut target = Canvas::new_gray(width, height);
            if let Some(owned) = layer.mask.as_ref() {
                // Past its pixels a mask keeps its edge tone, so blurring near its edge doesn't pull
                // in the wrong one.
                target.set_fill_gray(owned.asset.thumbnail.as_gray().map_or(1.0, LayerMask::background));
                target.fill_rect(Rect::new(0.0, 0.0, width as f64, height as f64));
            }
            if let PixelImage::Gray(image) = &image {
                target.draw_coverage(image, placed);
            }
            PixelImage::Gray(Arc::new(target.into_gray()))
        } else {
            let mut target = Canvas::new_rgba(width, height);
            if let PixelImage::Rgba(image) = &image {
                target.draw_image(image, placed);
            }
            PixelImage::Rgba(Arc::new(target.into_rgba()))
        };
        let soft = compositor_pixels::filters::gaussian_blur(&sharp, sigma * fit, stroke.is_mask);
        let extent = Rect::new(0.0, 0.0, width as f64, height as f64);
        let render = move |part: Rect| soft.cropped(part.intersection(extent));
        Some(BlurSample {
            sample: CloneSample {
                image: sharp,
                placed: region,
                in_grid: true,
            },
            render: Arc::new(render),
        })
    }

    // MARK: Smudge and Liquify

    /// A Smudge or Liquify stroke works on the active layer as the canvas shows it, at document size,
    /// changing it dab by dab. The working copy is drawn through the renderer — the port's seam for
    /// `WarpStroke`'s initializer, which drew the layer into a document-size context — and handed to
    /// `compositor_pixels::warp`.
    pub fn begin_warp(&mut self, point: Point) {
        let ready = self.can_paint()
            && !self.is_mask_selected
            && self.active_layer().map(|layer| layer.asset.is_some()).unwrap_or(false)
            && self.document.is_some();
        if !ready {
            self.brush_error = if self.is_mask_selected {
                Some("Smudge and Liquify work on a layer's pixels, not its mask.".to_string())
            } else {
                self.paint_refusal()
            };
            return;
        }
        let layer = self.active_layer().cloned().expect("checked above");
        let image = layer.asset.as_ref().expect("checked above").image.clone();
        let document_size = self.document.as_ref().expect("checked above").size();
        self.finish_opacity_edit();
        let transform = self.displayed_transform(&layer);
        let mut canvas = Canvas::new_rgba(document_size.width as usize, document_size.height as usize);
        // The working copy is the layer as the canvas shows it: `LayerRenderer.draw(image, transform:center:in:)`.
        LayerRenderer::draw(
            &image,
            &transform,
            transform.center(),
            1.0,
            1.0,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        let working = canvas.into_rgba();
        let settings = self.brush_settings.clone();
        let mut stroke = WarpStroke::new(
            working,
            self.blur_mode,
            settings.diameter,
            settings.hardness,
            settings.opacity,
        );
        stroke.append(point);
        self.warp_stroke = Some(stroke);
        self.warp_layer = Some(layer.clone());
        self.last_brush_point = Some(LastBrushPoint {
            point,
            layer_id: layer.id,
            mask: false,
        });
        self.brush_revision += 1;
    }

    /// Paints the finished Smudge or Liquify result into the layer's pixels along the stroke, as one
    /// undo step.
    pub fn finish_warp(&mut self) {
        let Some(warp) = self.warp_stroke.take() else { return };
        let started = self.warp_layer.take();
        self.brush_revision += 1;
        if warp.points().is_empty() {
            return;
        }
        let Some(started) = started else { return };
        let Some(current) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == started.id))
            .cloned()
        else {
            return;
        };
        if !same_image(current.asset.as_ref(), started.asset.as_ref()) || current.transform != started.transform {
            return;
        }
        let result: SharedImage = Arc::new(warp.image().clone());
        let mut settings = self.brush_settings.clone();
        // A hard tip a little wider than the brush covers everything the stroke moved.
        settings.diameter = warp.diameter() + 4.0;
        settings.hardness = 1.0;
        settings.opacity = 1.0;
        let mode = warp.mode();
        let diameter = warp.diameter();
        let points = warp.points().to_vec();
        let outcome = (|| -> Result<(), CoreError> {
            let mut stroke = self.make_raster_edit(&current, settings, false)?;
            stroke.clone = Some(CloneSample {
                image: PixelImage::Rgba(result.clone()),
                placed: Rect::new(0.0, 0.0, result.width() as f64, result.height() as f64),
                in_grid: false,
            });
            stroke.replaces_with_clone = true;
            stroke.edit_name = Some(mode.raw_value().to_string());
            // The tip is solid and a little wider than the brush, so a point every twentieth of its
            // width covers what every dab did: a big brush on a big canvas lays thousands of dabs,
            // and replaying each one stalled the release.
            let spacing = 1.0f64.max(diameter * 0.05);
            let mut kept: Option<Point> = None;
            for (index, point) in points.iter().enumerate() {
                if let Some(kept) = kept {
                    if index < points.len() - 1 && point.distance(kept) < spacing {
                        continue;
                    }
                }
                stroke.append(*point)?;
                kept = Some(*point);
            }
            stroke.flush()?;
            if !stroke.patches().is_empty() {
                self.commit_paint_snapshot(&stroke)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            self.brush_error = Some(brush_failure(&error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tip(diameter: f64, hardness: f64, opacity: f64) -> BrushTip {
        BrushTip::new(diameter, hardness, opacity)
    }

    /// The Clone Stamp and Smear families each park their own tip; both start soft and 40 wide.
    #[test]
    fn parked_tip_families_start_as_photoshop_hands_them_over() {
        let tips = ParkedBrushTips::default();
        assert_eq!(tips.parked_families(), 2);
        assert_eq!(tips.tip(BRUSH_FAMILY_CLONE_STAMP), Some(tip(40.0, 0.0, 1.0)));
        assert_eq!(tips.tip(BRUSH_FAMILY_BLUR), Some(tip(40.0, 0.0, 1.0)));
        // The shared family has never been left, so nothing is parked under it.
        assert_eq!(tips.tip(BRUSH_FAMILY_SHARED), None);
    }

    #[test]
    fn tip_family_follows_the_tool() {
        assert_eq!(ParkedBrushTips::family_of(NavigationTool::CloneStamp), BRUSH_FAMILY_CLONE_STAMP);
        assert_eq!(ParkedBrushTips::family_of(NavigationTool::Blur), BRUSH_FAMILY_BLUR);
        for tool in [NavigationTool::Brush, NavigationTool::SpotHealing, NavigationTool::Move, NavigationTool::Gradient] {
            assert_eq!(ParkedBrushTips::family_of(tool), BRUSH_FAMILY_SHARED);
        }
    }

    /// Leaving Brush for Clone Stamp parks the brush's tip and restores Clone Stamp's own; coming
    /// back restores the parked one, with Clone Stamp's put away again.
    #[test]
    fn hand_off_parks_and_restores_each_family() {
        let mut tips = ParkedBrushTips::default();
        let mut settings = BrushSettings { diameter: 120.0, hardness: 0.75, opacity: 0.5, ..BrushSettings::default() };

        tips.hand_off(BRUSH_FAMILY_SHARED, BRUSH_FAMILY_CLONE_STAMP, &mut settings);
        assert_eq!(tips.tip(BRUSH_FAMILY_SHARED), Some(tip(120.0, 0.75, 0.5)));
        assert_eq!((settings.diameter, settings.hardness, settings.opacity), (40.0, 0.0, 1.0));

        tips.hand_off(BRUSH_FAMILY_CLONE_STAMP, BRUSH_FAMILY_SHARED, &mut settings);
        assert_eq!((settings.diameter, settings.hardness, settings.opacity), (120.0, 0.75, 0.5));
        assert_eq!(tips.tip(BRUSH_FAMILY_CLONE_STAMP), Some(tip(40.0, 0.0, 1.0)));
    }

    /// Moving between the two families that both own a tip swaps them; moving within one family
    /// leaves the settings alone.
    #[test]
    fn hand_off_between_owned_families_swaps_them() {
        let mut tips = ParkedBrushTips::default();
        let mut settings = BrushSettings { diameter: 8.0, hardness: 1.0, opacity: 1.0, ..BrushSettings::default() };
        tips.hand_off(BRUSH_FAMILY_CLONE_STAMP, BRUSH_FAMILY_BLUR, &mut settings);
        assert_eq!((settings.diameter, settings.hardness), (40.0, 0.0));
        assert_eq!(tips.tip(BRUSH_FAMILY_CLONE_STAMP), Some(tip(8.0, 1.0, 1.0)));

        let before = settings.clone();
        tips.hand_off(BRUSH_FAMILY_BLUR, BRUSH_FAMILY_BLUR, &mut settings);
        assert_eq!(settings.diameter, before.diameter);
    }

    /// `cloneStrokeOffset`: aligned strokes keep the first stroke's offset; unaligned ones run from
    /// the brush to the source, in whole document pixels.
    #[test]
    fn clone_offsets_follow_alignment() {
        let mut session = EditorSession::default();
        assert_eq!(session.clone_stroke_offset(Point::new(10.0, 10.0)), None);

        session.set_clone_source(Point::new(100.6, 40.4));
        assert_eq!(session.clone_stroke_offset(Point::new(10.2, 10.2)), Some(Size::new(90.0, 30.0)));
        assert_eq!(session.clone_sample_point(Point::new(10.0, 10.0)), Some(Point::new(100.6, 40.4)));

        session.clone_offset = Some(Size::new(5.0, 5.0));
        assert_eq!(session.clone_stroke_offset(Point::new(10.0, 10.0)), Some(Size::new(5.0, 5.0)));
        assert_eq!(session.clone_sample_point(Point::new(10.0, 10.0)), Some(Point::new(15.0, 15.0)));

        session.clone_settings.aligned = false;
        assert_eq!(session.clone_stroke_offset(Point::new(20.0, 20.0)), Some(Size::new(81.0, 20.0)));

        // A new source starts a new alignment.
        session.set_clone_source(Point::new(1.0, 2.0));
        assert_eq!(session.clone_offset, None);
    }

    /// `[`/`]` step by a fifth but never stall on the smallest brush, and stop at 1…2000.
    #[test]
    fn brush_size_keys_step_and_clamp() {
        let mut session = EditorSession::default();
        session.brush_settings.diameter = 2.0;
        session.change_brush_size(false);
        assert_eq!(session.brush_settings.diameter, 1.0);
        session.brush_settings.diameter = 1.0;
        session.change_brush_size(false);
        assert_eq!(session.brush_settings.diameter, 1.0);
        session.brush_settings.diameter = 2000.0;
        session.change_brush_size(true);
        assert_eq!(session.brush_settings.diameter, 2000.0);
        session.brush_settings.diameter = 100.0;
        session.change_brush_size(true);
        assert_eq!(session.brush_settings.diameter, 120.0);
        session.change_brush_size(false);
        assert_eq!(session.brush_settings.diameter, 100.0);
    }

    /// Shift-[ / Shift-] snap to the 25% steps.
    #[test]
    fn brush_hardness_keys_snap_to_quarters() {
        let mut session = EditorSession::default();
        session.brush_settings.hardness = 0.8;
        session.change_brush_hardness(true);
        assert_eq!(session.brush_settings.hardness, 1.0);
        session.brush_settings.hardness = 0.8;
        session.change_brush_hardness(false);
        assert_eq!(session.brush_settings.hardness, 0.75);
        session.brush_settings.hardness = 0.0;
        session.change_brush_hardness(false);
        assert_eq!(session.brush_settings.hardness, 0.0);
    }

    /// Two digits typed inside 0.6 s set an exact value; the second alone is a percentage.
    #[test]
    fn opacity_digits_pair_up_within_the_window() {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Brush;
        session.type_opacity_digit(4, 100.0);
        assert_eq!(session.brush_settings.opacity, 0.4);
        session.type_opacity_digit(5, 100.5);
        assert_eq!(session.brush_settings.opacity, 0.45);
        assert_eq!(session.pending_opacity_digit, None);

        session.type_opacity_digit(0, 200.0);
        assert_eq!(session.brush_settings.opacity, 1.0);
        session.type_opacity_digit(5, 200.7);
        assert_eq!(session.brush_settings.opacity, 0.05);

        // Only the brush families, Gradient and Move read the number keys.
        session.tool = NavigationTool::Type;
        session.type_opacity_digit(7, 300.0);
        assert_eq!(session.brush_settings.opacity, 0.05);
    }
}
