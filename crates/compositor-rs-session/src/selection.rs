//! The selection tools and commands: marquee, lasso, wand and object creation, the Select menu
//! (All / Deselect / Inverse, Expand / Contract / Feather), the pixel edits confined to a
//! selection (fill, stroke-through-clear, delete, invert, pixel moves), Color Range, and loading a
//! layer's pixels or its mask as a selection.
//!
//! Ports `Document/Selection.swift`, `Document/SelectionEdits.swift`,
//! `Document/ColorRangeSelection.swift`, `Document/MagicWand.swift`,
//! `Document/ObjectSelection.swift`, and the session commands of `Document/MaskTracing.swift` and
//! `Document/SubjectRemoval.swift`.
//!
//! ## Async
//!
//! The Swift ran the wand, object selection, Select Subject, the Color Range match and Invert on
//! detached tasks and `await`ed them. Every command here is synchronous, with `is_project_busy` held
//! across the raster work so `can_edit_layers` / `can_use_history` observe the same refusals while it
//! runs. The one observable difference: a Color Range change can no longer supersede an older one
//! still being computed, because none is ever in flight — see [`EditorSession::update_color_range`].

use std::sync::Arc;

use compositor_rs_core::document::{CanvasDocument, ImageLayer, NavigationTool};
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::image_ops::FilterSettings;
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_transform::{pixel_to_document, LayerSampling, LayerTransform};
use compositor_rs_core::path::{FillRule, Path};
use compositor_rs_core::path_ops::{self, LineCap, LineJoin};
use compositor_rs_core::selection::{
    DocumentSelection, DragBox, LassoDraft, LassoKind, SelectionClip, SelectionMode,
};
use compositor_rs_core::{CoreError, Gray8Image, Id};
use compositor_rs_pixels::brush::{BrushSettings, BrushStroke};
use compositor_rs_pixels::canvas::Canvas;
use compositor_rs_pixels::masks::{
    color_range_mask, MagicWand, MaskTracing, ObjectSelection, SubjectRemoval,
};
use compositor_rs_render::LayerRenderer;

use crate::clipboard::rgba_thumbnail;
use crate::EditorSession;

/// `EditorSession.SelectionAmountOperation`: what Select > Modify is asking an amount for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionAmountOperation {
    Expand,
    Contract,
    Feather,
}

impl SelectionAmountOperation {
    /// `CaseIterable` order, and the raw values the menu keeps.
    pub const ALL: [SelectionAmountOperation; 3] = [
        SelectionAmountOperation::Expand,
        SelectionAmountOperation::Contract,
        SelectionAmountOperation::Feather,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            SelectionAmountOperation::Expand => "Expand",
            SelectionAmountOperation::Contract => "Contract",
            SelectionAmountOperation::Feather => "Feather",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.raw_value() == value)
    }
}

/// Which eyedropper is armed while the Color Range (and Hue/Saturation) panel is open
/// (`HueSampleMode`); the port keeps the one definition here for both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HueSampleMode {
    #[default]
    Replace,
    Add,
    Remove,
}

impl HueSampleMode {
    /// `CaseIterable` order.
    pub const ALL: [HueSampleMode; 3] = [
        HueSampleMode::Replace,
        HueSampleMode::Add,
        HueSampleMode::Remove,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            HueSampleMode::Replace => "Sample",
            HueSampleMode::Add => "Add",
            HueSampleMode::Remove => "Remove",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }

    /// All three are eyedroppers; Add and Remove carry a small badge.
    pub fn symbol(self) -> &'static str {
        "eyedropper"
    }

    pub fn badge(self) -> Option<&'static str> {
        match self {
            HueSampleMode::Replace => None,
            HueSampleMode::Add => Some("plus.circle.fill"),
            HueSampleMode::Remove => Some("minus.circle.fill"),
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            HueSampleMode::Replace => "Click the image to center this range on that color",
            HueSampleMode::Add => "Click the image to widen this range to include that color",
            HueSampleMode::Remove => "Click the image to narrow this range to exclude that color",
        }
    }
}

/// Select > Color Range: every pixel near the colors clicked on the canvas, anywhere in the image.
/// The panel shows the selection live; OK keeps it as one undo step, Cancel puts back the one there was.
///
/// The Swift class was `@Observable`, with an ignored `image` / `original` / `generation`; the port is
/// plain data on the session.
#[derive(Clone, Debug)]
pub struct ColorRangeEdit {
    pub fuzziness: f64,
    pub invert: bool,
    /// What the next click on the canvas does: start over from that color, add it, or take it away.
    pub sample_mode: HueSampleMode,
    /// Shift (add) or Option (take away) held right now, which a click uses over `sample_mode`.
    pub held: Option<HueSampleMode>,
    /// The selection in black and white, small enough for the panel. `None` until a color is picked.
    pub preview: Option<Gray8Image>,
    /// Straight sRGB colors, 3 bytes each.
    pub include: Vec<u8>,
    pub exclude: Vec<u8>,
    pub error: Option<String>,
    /// The image as shown, at document size: what the colors are matched against.
    pub image: compositor_rs_core::Rgba8Image,
    pub original: Option<DocumentSelection>,
    /// Orders the Swift's async previews; kept for parity even though the port's match is synchronous.
    pub generation: i64,
}

impl ColorRangeEdit {
    pub const FUZZINESS_RANGE: std::ops::RangeInclusive<f64> = 0.0..=200.0;
    /// The panel's preview fits in this, in points.
    pub const PREVIEW_SIZE: Size = Size::new(292.0, 200.0);

    /// `ColorRangeEdit.init(image:original:)`.
    pub fn new(image: compositor_rs_core::Rgba8Image, original: Option<DocumentSelection>) -> Self {
        ColorRangeEdit {
            fuzziness: 40.0,
            invert: false,
            sample_mode: HueSampleMode::Replace,
            held: None,
            preview: None,
            include: Vec::new(),
            exclude: Vec::new(),
            error: None,
            image,
            original,
            generation: 0,
        }
    }

    pub fn effective_mode(&self) -> HueSampleMode {
        self.held.unwrap_or(self.sample_mode)
    }

    pub fn has_colors(&self) -> bool {
        !self.include.is_empty()
    }
}

/// The two swatches a fill can take its color from (`FillSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillSource {
    Foreground,
    Background,
}

// MARK: - Small shared helpers

/// `LayerMask.background(of:)` over the pixel type a mask's thumbnail carries.
pub(crate) fn mask_background(thumbnail: &PixelImage) -> f64 {
    let Some(gray) = thumbnail.as_gray() else {
        return 1.0;
    };
    let (width, height) = (gray.width(), gray.height());
    if width == 0 || height == 0 {
        return 1.0;
    }
    let mut total: u64 = 0;
    let mut count: u64 = 0;
    for y in 0..height {
        for x in 0..width {
            if y == 0 || y == height - 1 || x == 0 || x == width - 1 {
                total += gray.get(x, y) as u64;
                count += 1;
            }
        }
    }
    if total * 2 >= count * 255 {
        1.0
    } else {
        0.0
    }
}

/// `CGRect.applying(_:)`: the bounding box of the transformed corners.
pub(crate) fn rect_applying(rect: Rect, transform: &AffineTransform) -> Rect {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for corner in rect.corners() {
        let point = transform.applying(corner);
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// `BrushRaster.draw(_:in:mask:true)` for a gray canvas: `rect` becomes the image scaled into it
/// with no interpolation, everything else in `rect` goes black. The Swift filled black, clipped to
/// the image and filled white; the port writes the sampled byte directly.
pub(crate) fn draw_gray_scaled(canvas: &mut Gray8Image, image: &Gray8Image, rect: Rect) {
    if image.is_empty() || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let start_x = rect.min_x().floor() as i64;
    let start_y = rect.min_y().floor() as i64;
    let end_x = rect.max_x().ceil() as i64;
    let end_y = rect.max_y().ceil() as i64;
    for y in start_y..end_y {
        if y < 0 || y >= canvas.height() as i64 {
            continue;
        }
        let source_y = (((y as f64 - rect.min_y()) * image.height() as f64 / rect.height()).floor())
            .clamp(0.0, image.height() as f64 - 1.0) as usize;
        for x in start_x..end_x {
            if x < 0 || x >= canvas.width() as i64 {
                continue;
            }
            let source_x = (((x as f64 - rect.min_x()) * image.width() as f64 / rect.width())
                .floor())
            .clamp(0.0, image.width() as f64 - 1.0) as usize;
            canvas.set(x as usize, y as usize, image.get(source_x, source_y));
        }
    }
}

/// A gray image resampled to `width`×`height` with bilinear sampling and clamped edges — the port's
/// stand-in for `CGContext`'s `.high` / `.medium` interpolation used for thumbnails and previews.
pub(crate) fn scale_gray(image: &Gray8Image, width: usize, height: usize) -> Gray8Image {
    let mut result = Gray8Image::new(width.max(1), height.max(1));
    if image.is_empty() || width == 0 || height == 0 {
        return result;
    }
    for y in 0..result.height() {
        let source_y = (y as f64 + 0.5) * image.height() as f64 / result.height() as f64 - 0.5;
        for x in 0..result.width() {
            let source_x = (x as f64 + 0.5) * image.width() as f64 / result.width() as f64 - 0.5;
            let value = if image.width() == 1 && image.height() == 1 {
                image.get(0, 0) as f64
            } else {
                bilinear_gray(image, source_x, source_y)
            };
            result.set(x, y, value.round().clamp(0.0, 255.0) as u8);
        }
    }
    result
}

fn bilinear_gray(image: &Gray8Image, x: f64, y: f64) -> f64 {
    let last_x = image.width() as f64 - 1.0;
    let last_y = image.height() as f64 - 1.0;
    let x = x.clamp(0.0, last_x);
    let y = y.clamp(0.0, last_y);
    let x0 = x.floor();
    let y0 = y.floor();
    let x1 = (x0 + 1.0).min(last_x);
    let y1 = (y0 + 1.0).min(last_y);
    let fx = x - x0;
    let fy = y - y0;
    let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
    let top = image.get(x0, y0) as f64 * (1.0 - fx) + image.get(x1, y0) as f64 * fx;
    let bottom = image.get(x0, y1) as f64 * (1.0 - fx) + image.get(x1, y1) as f64 * fx;
    top * (1.0 - fy) + bottom * fy
}

/// `LayerMask.asset(from:)`: the mask as an imported gray image with a thumbnail no larger than
/// 96 pixels on its longest side.
pub(crate) fn mask_imported_image(image: Gray8Image) -> ImportedImage {
    let factor = (96.0 / image.width().max(image.height()).max(1) as f64).min(1.0);
    let width = ((image.width() as f64 * factor) as usize).max(1);
    let height = ((image.height() as f64 * factor) as usize).max(1);
    let thumbnail = scale_gray(&image, width, height);
    ImportedImage::new(
        PixelImage::Gray(Arc::new(image)),
        PixelImage::Gray(Arc::new(thumbnail)),
        "Layer Mask",
    )
}

/// `DocumentSelection.coverage(width:height:)`'s CIGaussianBlur for a feathered edge: a separable
/// Gaussian whose sigma is `feather / 2`, clamped at the edges and truncated at three deviations.
fn gaussian_blur_gray(image: &Gray8Image, sigma: f64) -> Gray8Image {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 || !(sigma > 0.0) || !sigma.is_finite() {
        return image.clone();
    }
    let radius = (sigma * 3.0).ceil() as usize;
    let mut weights = vec![0f32; radius + 1];
    let mut total = 0f32;
    for (offset, weight) in weights.iter_mut().enumerate() {
        let value = (-((offset * offset) as f64) / (2.0 * sigma * sigma)).exp() as f32;
        *weight = value;
        total += if offset == 0 { value } else { value * 2.0 };
    }
    for weight in weights.iter_mut() {
        *weight /= total;
    }
    let mut levels: Vec<f32> = image.data().iter().map(|&byte| byte as f32).collect();
    let mut pass = vec![0f32; levels.len()];
    blur_rows(&levels, &mut pass, width, height, &weights, radius);
    let mut transposed = vec![0f32; levels.len()];
    for y in 0..height {
        for x in 0..width {
            transposed[x * height + y] = pass[y * width + x];
        }
    }
    levels.copy_from_slice(&transposed);
    blur_rows(&levels, &mut pass, height, width, &weights, radius);
    let mut result = Gray8Image::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let value = pass[x * height + y].round().clamp(0.0, 255.0) as u8;
            result.set(x, y, value);
        }
    }
    result
}

/// One clamped-edge Gaussian pass along every row of a `width`×`height` buffer.
fn blur_rows(
    source: &[f32],
    target: &mut [f32],
    width: usize,
    height: usize,
    weights: &[f32],
    radius: usize,
) {
    let last = width as isize - 1;
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            let mut sum = weights[0] * source[row + x];
            for offset in 1..=radius {
                let left = (x as isize - offset as isize).clamp(0, last) as usize;
                let right = (x as isize + offset as isize).clamp(0, last) as usize;
                sum += weights[offset] * (source[row + left] + source[row + right]);
            }
            target[row + x] = sum;
        }
    }
}

/// `layer.asset?.image === other.asset?.image`: the shared rasters compared by identity.
pub(crate) fn same_pixels(lhs: &Option<ImportedImage>, rhs: &Option<ImportedImage>) -> bool {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => match (&lhs.image, &rhs.image) {
            (PixelImage::Rgba(lhs), PixelImage::Rgba(rhs)) => Arc::ptr_eq(lhs, rhs),
            (PixelImage::Gray(lhs), PixelImage::Gray(rhs)) => Arc::ptr_eq(lhs, rhs),
            _ => false,
        },
        (None, None) => true,
        _ => false,
    }
}

/// `ImageLayer.maskTransform`: where the mask sits on the document — its own placement once moved
/// apart, otherwise the layer's transform.
fn mask_transform(layer: &ImageLayer) -> LayerTransform {
    layer
        .mask
        .as_ref()
        .and_then(|mask| mask.placement)
        .unwrap_or(layer.transform)
}

/// `LayerTransform(origin:size:)`: upright, unflipped, with the default sampling.
fn transform_at(origin: Point, size: Size) -> LayerTransform {
    LayerTransform {
        origin,
        size,
        rotation: 0.0,
        flip_x: false,
        flip_y: false,
        sampling: LayerSampling::default(),
    }
}

impl EditorSession {
    // MARK: Reading and combining the selection

    /// `canEditSelection`: selection commands need the same quiet editor as layer edits.
    pub fn can_edit_selection(&self) -> bool {
        self.can_edit_layers()
    }

    /// Shift adds, Option (with or without Shift) subtracts; otherwise the options-bar mode.
    pub fn selection_mode(&self, shift: bool, option: bool) -> SelectionMode {
        if option {
            SelectionMode::Subtract
        } else if shift {
            SelectionMode::Add
        } else {
            self.selection_mode_choice
        }
    }

    /// The mode the cursor advertises: an outline in progress keeps its starting mode,
    /// otherwise the currently held modifiers or the options-bar choice.
    pub fn lasso_cursor_mode(&self, shift: bool, option: bool) -> SelectionMode {
        self.lasso_draft
            .as_ref()
            .map(|draft| draft.mode)
            .unwrap_or_else(|| self.selection_mode(shift, option))
    }

    /// What the options bar highlights: the same rule, using the tracked held keys.
    pub fn displayed_selection_mode(&self) -> SelectionMode {
        self.lasso_draft
            .as_ref()
            .map(|draft| draft.mode)
            .or(self.held_selection_mode)
            .unwrap_or(self.selection_mode_choice)
    }

    pub fn update_held_selection_keys(&mut self, shift: bool, option: bool) {
        let held = if option {
            Some(SelectionMode::Subtract)
        } else if shift {
            Some(SelectionMode::Add)
        } else {
            None
        };
        if self.held_selection_mode != held {
            self.held_selection_mode = held;
        }
    }

    // MARK: Drawing an outline (marquee / lasso)

    pub fn begin_lasso(&mut self, at: Point, mode: SelectionMode) {
        // Click-selection tools never draw a draft outline.
        if !(self.tool.is_selection_tool()
            && self.tool != NavigationTool::Wand
            && self.can_edit_selection()
            && self.selection_move_origin.is_none())
        {
            return;
        }
        if self.tool == NavigationTool::Marquee {
            let anchor = Point::new(at.x.round(), at.y.round());
            self.lasso_draft = Some(LassoDraft {
                points: vec![anchor],
                cursor: None,
                mode,
                kind: self.marquee_kind,
                anchor: Some(anchor),
            });
        } else {
            self.lasso_draft = Some(LassoDraft::new(vec![at], mode, self.lasso_kind));
        }
    }

    /// Marquee drag, snapped to whole pixels. Shift held during the drag makes a square (or
    /// circle); `from_center` grows the box around the anchor. The canvas never asks for that with
    /// Option, which subtracts from the selection instead.
    pub fn drag_marquee(&mut self, to: Point, square: bool, from_center: bool) {
        let Some(draft) = self.lasso_draft.as_mut() else {
            return;
        };
        if !(draft.kind == LassoKind::Rectangle || draft.kind == LassoKind::Ellipse) {
            return;
        }
        let Some(anchor) = draft.anchor else {
            return;
        };
        if !(to.x.is_finite() && to.y.is_finite()) {
            return;
        }
        let rect = DragBox::rect(anchor, to, square, from_center);
        draft.points = vec![
            Point::new(rect.min_x(), rect.min_y()),
            Point::new(rect.max_x(), rect.min_y()),
            Point::new(rect.max_x(), rect.max_y()),
            Point::new(rect.min_x(), rect.max_y()),
        ];
    }

    /// Adds an outline point; points closer than a quarter pixel are skipped.
    pub fn extend_lasso(&mut self, to: Point) {
        let Some(draft) = self.lasso_draft.as_mut() else {
            return;
        };
        if !(to.x.is_finite() && to.y.is_finite()) {
            return;
        }
        if let Some(last) = draft.points.last() {
            if last.distance(to) < 0.25 {
                return;
            }
        }
        draft.points.push(to);
    }

    pub fn move_lasso_cursor(&mut self, to: Option<Point>) {
        if let Some(draft) = self.lasso_draft.as_mut() {
            draft.cursor = to;
        }
    }

    pub fn remove_last_lasso_point(&mut self) {
        let Some(mut draft) = self.lasso_draft.take() else {
            return;
        };
        draft.points.pop();
        self.lasso_draft = if draft.points.is_empty() {
            None
        } else {
            Some(draft)
        };
    }

    pub fn cancel_lasso(&mut self) {
        self.lasso_draft = None;
    }

    /// The M key chooses the Marquee in whichever shape it was last set to (switched only in the tool
    /// bar). The shape stays as last set while this project is open.
    pub fn press_marquee_key(&mut self) {
        self.select_tool(NavigationTool::Marquee);
    }

    pub fn toggle_marquee_kind(&mut self) {
        self.cancel_lasso();
        self.marquee_kind = if self.marquee_kind == LassoKind::Rectangle {
            LassoKind::Ellipse
        } else {
            LassoKind::Rectangle
        };
    }

    /// W picks the Magic tool; Tab switches its Wand and Object modes.
    pub fn press_wand_key(&mut self) {
        self.select_tool(NavigationTool::Wand);
    }

    /// The L key chooses the Lasso in whichever mode it was last set to (switched only in the tool
    /// bar). The mode stays as last set while this project is open.
    pub fn press_lasso_key(&mut self) {
        self.select_tool(NavigationTool::Lasso);
    }

    pub fn toggle_lasso_kind(&mut self) {
        self.cancel_lasso();
        self.lasso_kind = if self.lasso_kind == LassoKind::Freehand {
            LassoKind::Polygonal
        } else {
            LassoKind::Freehand
        };
    }

    /// Closes the outline and combines it with the current selection. A click that
    /// encloses nothing deselects in New mode, as in Photoshop.
    pub fn finish_lasso(&mut self) {
        let Some(draft) = self.lasso_draft.take() else {
            return;
        };
        let mut outline = Path::empty();
        if draft.kind == LassoKind::Ellipse && draft.points.len() == 4 {
            // The drag's box, whole pixels like a rectangle; the oval fills it.
            let min_x = draft
                .points
                .iter()
                .map(|p| p.x)
                .fold(f64::INFINITY, f64::min);
            let max_x = draft
                .points
                .iter()
                .map(|p| p.x)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_y = draft
                .points
                .iter()
                .map(|p| p.y)
                .fold(f64::INFINITY, f64::min);
            let max_y = draft
                .points
                .iter()
                .map(|p| p.y)
                .fold(f64::NEG_INFINITY, f64::max);
            outline.add_ellipse(Rect::new(min_x, min_y, max_x - min_x, max_y - min_y));
        } else if !draft.points.is_empty() {
            // `CGPath.addLines(between:)`: a move to the first point and a line to each of the rest.
            outline.add_lines(&draft.points);
            outline.close_subpath();
        }
        let bounds = outline.bounding_box();
        if !(draft.points.len() >= 3 || draft.kind == LassoKind::Ellipse)
            || bounds.width() <= 0.0
            || bounds.height() <= 0.0
        {
            if draft.mode == SelectionMode::Replace {
                self.deselect();
            }
            return;
        }
        let name = match draft.kind {
            LassoKind::Freehand => "Lasso",
            LassoKind::Polygonal => "Polygonal Lasso",
            LassoKind::Ellipse => "Elliptical Marquee",
            LassoKind::Rectangle => "Rectangular Marquee",
        };
        self.apply_selection(outline, draft.mode, name);
    }

    /// Combines `shape` with the current selection under `mode` and records it as one undo step.
    /// A Subtract with no selection selects nothing new, so nothing changes.
    pub fn apply_selection(&mut self, shape: Path, mode: SelectionMode, name: &str) {
        if self.document.is_none() || !self.can_edit_selection() {
            return;
        }
        let Some(canvas_rect) = self
            .document
            .as_ref()
            .map(|document| Rect::new(0.0, 0.0, document.width as f64, document.height as f64))
        else {
            return;
        };
        let canvas = Path::rect(canvas_rect);
        let clipped = path_ops::intersection(&shape, &canvas, FillRule::Winding);
        let result = match mode {
            SelectionMode::Replace => clipped,
            SelectionMode::Add => match self.selection() {
                Some(current) => path_ops::union(&current.path, &clipped, FillRule::Winding),
                None => clipped,
            },
            SelectionMode::Subtract => {
                // Subtracting from no selection selects nothing new, so nothing changes.
                let Some(current) = self.selection() else {
                    return;
                };
                path_ops::subtracting(&current.path, &clipped, FillRule::Winding)
            }
        };
        let antialiased = self.selection_antialiased;
        self.set_selection(
            Some(DocumentSelection::with_style(result, antialiased, 0.0)),
            name,
        );
    }

    /// Writes a selection and records it, unless it is already exactly this one.
    pub fn set_selection(&mut self, value: Option<DocumentSelection>, name: &str) {
        if self.document.is_none() || !self.can_edit_selection() {
            return;
        }
        if self
            .document
            .as_ref()
            .and_then(|document| document.selection.as_ref())
            == value.as_ref()
        {
            return;
        }
        self.begin_edit(name);
        if let Some(document) = self.document.as_mut() {
            document.selection = value;
        }
        self.end_edit();
    }

    // MARK: Moving the outline

    /// True where dragging in New mode would move the selection outline.
    pub fn can_move_selection(&self, at: Point) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };
        if selection.is_empty() || !self.can_edit_selection() || self.lasso_draft.is_some() {
            return false;
        }
        selection.path.contains(at, FillRule::Winding)
    }

    /// Moves the outline only (never pixels). The whole drag is one undo step.
    pub fn begin_selection_move(&mut self) -> bool {
        if self.selection_move_origin.is_some() {
            return false;
        }
        let Some(selection) = self.selection().cloned() else {
            return false;
        };
        if selection.is_empty() || !self.can_edit_selection() {
            return false;
        }
        self.begin_edit("Move Selection");
        self.selection_move_origin = Some(selection);
        true
    }

    /// Offsets from the drag's start, rounded to whole pixels so edges stay crisp. The
    /// outline is not re-clipped, so it can leave the canvas and come back intact.
    pub fn move_selection(&mut self, offset: Size) {
        let Some(origin) = self.selection_move_origin.as_ref() else {
            return;
        };
        let shift = AffineTransform::translation(offset.width.round(), offset.height.round());
        let path = origin.path.transformed(&shift);
        let moved = DocumentSelection::with_style(path, origin.antialiased, origin.feather);
        if let Some(document) = self.document.as_mut() {
            document.selection = Some(moved);
        }
    }

    pub fn end_selection_move(&mut self) {
        if self.selection_move_origin.is_none() {
            return;
        }
        self.selection_move_origin = None;
        self.end_edit();
    }

    /// Arrow-key nudge: 1 px, or 10 px with Shift. Each press is one undo step.
    pub fn nudge_selection(&mut self, dx: f64, dy: f64) {
        if !self.begin_selection_move() {
            return;
        }
        self.move_selection(Size::new(dx, dy));
        self.end_selection_move();
    }

    // MARK: Expand / Contract / Feather

    /// Expand / Contract need a non-empty selection to work on.
    pub fn can_modify_selection(&self) -> bool {
        self.selection()
            .is_some_and(|selection| !selection.is_empty())
            && self.can_edit_selection()
            && self.lasso_draft.is_none()
    }

    /// Menu commands ask for an amount; the tool header applies its input directly.
    pub fn prompt_selection_amount(&mut self, operation: SelectionAmountOperation) {
        if !self.can_modify_selection() {
            return;
        }
        self.selection_amount_operation = Some(operation);
    }

    pub fn confirm_selection_amount(&mut self, amount: i32) {
        let Some(operation) = self.selection_amount_operation else {
            return;
        };
        let limit = if operation == SelectionAmountOperation::Feather {
            250
        } else {
            500
        };
        if !(1..=limit).contains(&amount) {
            return;
        }
        self.selection_amount_operation = None;
        match operation {
            SelectionAmountOperation::Expand => {
                self.selection_expand_amount = amount;
                self.expand_selection(amount);
            }
            SelectionAmountOperation::Contract => {
                self.selection_contract_amount = amount;
                self.contract_selection(amount);
            }
            SelectionAmountOperation::Feather => {
                self.selection_feather_amount = amount;
                self.feather_selection(amount);
            }
        }
    }

    /// Grows the outline by `amount` pixels with rounded corners (Photoshop's Expand), clipped to the canvas.
    pub fn expand_selection(&mut self, amount: i32) {
        self.resize_selection(amount as f64, "Expand Selection");
    }

    /// Shrinks the outline by `amount` pixels, including away from the canvas edges.
    /// Contracting past the middle leaves an explicit empty selection.
    pub fn contract_selection(&mut self, amount: i32) {
        self.resize_selection(-(amount as f64), "Contract Selection");
    }

    /// Softens the current selection's edge by `amount` pixels, as Select → Modify → Feather does. Applying it
    /// again softens further, the way Expand and Contract stack up.
    pub fn feather_selection(&mut self, amount: i32) {
        if !self.can_modify_selection() {
            return;
        }
        let Some(current) = self.selection().cloned() else {
            return;
        };
        if amount <= 0 {
            return;
        }
        // Two soft edges together spread a little less than their sum, as blurs do.
        let softened =
            (current.feather * current.feather + (amount as f64) * (amount as f64)).sqrt();
        self.set_selection(
            Some(DocumentSelection::with_style(
                current.path.clone(),
                current.antialiased,
                softened.min(250.0),
            )),
            "Feather Selection",
        );
    }

    fn resize_selection(&mut self, delta: f64, name: &str) {
        if self.document.is_none()
            || !self.can_modify_selection()
            || delta == 0.0
            || delta.abs() > 500.0
        {
            return;
        }
        let Some(current) = self.selection().cloned() else {
            return;
        };
        let Some(canvas_rect) = self
            .document
            .as_ref()
            .map(|document| Rect::new(0.0, 0.0, document.width as f64, document.height as f64))
        else {
            return;
        };
        // A band `|delta|` wide on each side of the outline, added or removed.
        let band = path_ops::stroking_with_width(
            &current.path,
            delta.abs() * 2.0,
            LineCap::Round,
            LineJoin::Round,
            10.0,
            None,
        );
        let result = if delta > 0.0 {
            path_ops::intersection(
                &path_ops::union(&current.path, &band, FillRule::Winding),
                &Path::rect(canvas_rect),
                FillRule::Winding,
            )
        } else {
            path_ops::subtracting(&current.path, &band, FillRule::Winding)
        };
        self.set_selection(
            Some(DocumentSelection::with_style(
                result,
                current.antialiased,
                current.feather,
            )),
            name,
        );
    }

    // MARK: Select All / Deselect / Inverse

    pub fn select_all(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let path = Path::rect(Rect::new(
            0.0,
            0.0,
            document.width as f64,
            document.height as f64,
        ));
        self.set_selection(Some(DocumentSelection::new(path)), "Select All");
    }

    pub fn deselect(&mut self) {
        if self.selection().is_none() {
            return;
        }
        self.set_selection(None, "Deselect");
    }

    pub fn invert_selection(&mut self) {
        let Some(canvas_rect) = self
            .document
            .as_ref()
            .map(|document| Rect::new(0.0, 0.0, document.width as f64, document.height as f64))
        else {
            return;
        };
        let Some(current) = self.selection().cloned() else {
            return;
        };
        let canvas = Path::rect(canvas_rect);
        let inverse = DocumentSelection::with_style(
            path_ops::subtracting(&canvas, &current.path, FillRule::Winding),
            current.antialiased,
            current.feather,
        );
        // The inverse of everything is no selection at all, as in Photoshop — not an invisible empty one that
        // quietly stops every brush.
        let value = if inverse.is_empty() {
            None
        } else {
            Some(inverse)
        };
        self.set_selection(value, "Inverse");
    }
}

/// `DocumentSelection.coverage(width:height:)` for a region of the document: the outline filled
/// white on black, antialiased as the selection asks, then feathered when it has a soft edge.
pub fn render_selection_coverage(selection: &DocumentSelection, region: Rect) -> Gray8Image {
    let width = region.width().max(1.0) as usize;
    let height = region.height().max(1.0) as usize;
    let mut canvas = Canvas::new_gray(width, height);
    canvas.set_should_antialias(selection.antialiased || selection.feather > 0.0);
    canvas.set_fill_gray(1.0);
    canvas.fill_path(&selection.outline_for_region(region), FillRule::Winding);
    let image = canvas.into_gray();
    if selection.feather > 0.0 {
        gaussian_blur_gray(&image, selection.feather / 2.0)
    } else {
        image
    }
}

impl EditorSession {
    /// The current selection's clip, or `None` when nothing is selected. An explicit empty
    /// selection gives a clip with no coverage, which clips everything away — "touch nothing".
    pub fn selection_clip(&self) -> Option<SelectionClip> {
        let document = self.document.as_ref()?;
        let selection = document.selection.as_ref()?;
        Some(self.selection_clip_for(selection, document.size()))
    }

    /// `DocumentSelection.clip(canvas:)` with its coverage rasterized: core computes the region,
    /// this fills the pixels (the port's `compositor-rs-render` counterpart for the session).
    pub fn selection_clip_for(&self, selection: &DocumentSelection, canvas: Size) -> SelectionClip {
        let mut clip = selection.clip(canvas);
        if !selection.is_empty()
            && !clip.rect.is_null()
            && clip.rect.width() >= 1.0
            && clip.rect.height() >= 1.0
        {
            clip.coverage = Some(render_selection_coverage(selection, clip.rect));
        }
        clip
    }

    /// `canEditPixels`: whether the active layer (or its mask) can take a fill or clear right now.
    pub fn can_edit_pixels(&self) -> bool {
        self.can_paint()
    }

    /// Fills the selection with the foreground or background color, as one undo step.
    /// With no selection it fills the whole layer; an empty selection fills nothing.
    /// On a mask the palette is black/white, so this reveals or hides.
    ///
    /// The Swift `await`s the commit; the port commits synchronously (the busy flag covered the wait).
    pub fn fill_selection(&mut self, source: FillSource) {
        if !self.can_edit_pixels() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else {
            return;
        };
        let value = self.palette_color(source == FillSource::Background);
        // A text layer that is still text takes the color as its own, rather than being painted over: the letters
        // change color and stay editable.
        if !self.is_mask_selected
            && self.selection().is_none()
            && layer.live_text().is_some()
            && self.recolor_text(layer.id, value)
        {
            return;
        }
        let name = if self.is_mask_selected {
            "Fill Mask"
        } else {
            "Fill"
        };
        self.apply_pixel_edit(&layer, name, |stroke| stroke.fill(value));
    }

    /// Delete with a selection: image pixels become transparent; on a mask the
    /// selection fills with the background color, as in Photoshop.
    pub fn clear_selected_pixels(&mut self) {
        if self.selection().is_none() || !self.can_edit_pixels() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else {
            return;
        };
        if self.is_mask_selected {
            self.fill_selection(FillSource::Background);
            return;
        }
        if layer.asset.is_none() {
            return;
        }
        self.apply_pixel_edit(&layer, "Clear", BrushStroke::clear_pixels);
    }

    /// The Delete key: clears the selection when there is one; otherwise deletes the
    /// targeted mask, or the layer when its pixels are targeted.
    pub fn delete_key_pressed(&mut self) {
        if self.selected_effect().is_some() {
            self.remove_selected_effect();
            return;
        }
        if self.selection().is_some() {
            self.clear_selected_pixels();
        } else {
            self.delete_layer_or_mask();
        }
    }

    /// The trash button and Delete without a selection: with one layer's mask thumbnail targeted
    /// only the mask goes; otherwise every selected layer does, in one undo step.
    pub fn delete_layer_or_mask(&mut self) {
        if self.selected_effect().is_some() {
            self.remove_selected_effect();
            return;
        }
        if self.is_mask_selected
            && self.active_layer().is_some_and(|layer| layer.mask.is_some())
            && self.selected_layer_ids.len() <= 1
        {
            self.delete_active_mask();
        } else {
            self.delete_selected_layers();
        }
    }

    /// `deleteLayerMask` (`Document/LayerMask.swift`): removes the targeted layer's mask as one
    /// undo step. The mask slice (`mask_ops.rs`) has no owner since its worker was aborted, so the
    /// Swift body lives here until that module lands; [`Self::delete_layer_or_mask`] above is the
    /// `SelectionEdits.swift` command and calls this.
    fn delete_active_mask(&mut self) {
        if !self.can_edit_layers() || self.selected_layer_ids.len() != 1 || self.active_layer().is_none() {
            return;
        }
        let Some(index) = self.document.as_ref().and_then(|document| {
            document
                .layers
                .iter()
                .position(|layer| Some(layer.id) == self.active_layer_id)
        }) else {
            return;
        };
        if self.document.as_ref().and_then(|document| document.layers.get(index)).and_then(|layer| layer.mask.as_ref()).is_none() {
            return;
        }
        self.finish_opacity_edit();
        self.begin_edit("Delete Layer Mask");
        if let Some(document) = self.document.as_mut() {
            document.layers[index].mask = None;
        }
        self.is_mask_selected = false;
        self.end_edit();
    }

    /// `applyPixelEdit`: runs `paint` on a fresh raster edit of the layer (or its mask, which
    /// grows past the layer as the brush can) and commits the result as one undo step.
    fn apply_pixel_edit(
        &mut self,
        layer: &ImageLayer,
        name: &str,
        paint: impl FnOnce(&mut BrushStroke) -> Result<(), CoreError>,
    ) {
        self.finish_opacity_edit();
        // On a mask, a fill covers the whole canvas, past the mask's own area, as the brush can.
        let mut edit = match self.make_raster_edit(layer, BrushSettings::default(), true) {
            Ok(edit) => edit,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        if let Err(error) = paint(&mut edit) {
            self.brush_error = Some(error.to_string());
            return;
        }
        if edit.patches().is_empty() {
            return;
        }
        match self.commit_raster_edit(&edit, name, None) {
            Ok(()) => self.brush_revision += 1,
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// Cmd-I is available in every tool: a pending gradient or transform is applied first, and the
    /// Crop tool's rectangle doesn't block it.
    pub fn can_invert(&self) -> bool {
        if self.document.is_none() || self.text_draft.is_some() {
            return false;
        }
        let Some(layer) = self.active_layer() else {
            return false;
        };
        if self.is_project_busy
            || self.is_importing
            || self.brush_stroke.is_some()
            || self.pixel_move.is_some()
            || self.renaming_layer_id.is_some()
            || self.shows_new_document
            || self.shows_importer
        {
            return false;
        }
        if self.selected_layer_ids.len() != 1 {
            return false;
        }
        if layer.is_group && !self.is_mask_selected {
            return false;
        }
        let Some(document) = self.document.as_ref() else {
            return false;
        };
        if !document.effective_visible_ids().contains(&layer.id) {
            return false;
        }
        if self.selection().map(|selection| selection.is_empty()) == Some(true) {
            return false;
        }
        if self.is_mask_selected {
            layer.mask.as_ref().is_some_and(|mask| mask.is_enabled)
        } else {
            layer.asset.is_some()
        }
    }

    /// Cmd-I: inverts the layer's colors (transparency kept) or its mask, inside the
    /// selection or across the whole layer without one; one undo step.
    ///
    /// The Swift ran the inversion on a detached task and awaited it; the port runs it inline with
    /// the busy flag held, and re-checks that the layer has not changed underneath before writing.
    pub fn invert_pixels(&mut self) {
        if !self.can_invert() {
            return;
        }
        self.commit_transform();
        if self.gradient_edit.is_some() {
            self.commit_gradient();
        }
        if !self.can_invert() {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        let Some(layer) = self.active_layer().cloned() else {
            return;
        };
        let Some(index) = document
            .layers
            .iter()
            .position(|candidate| candidate.id == layer.id)
        else {
            return;
        };
        let mask = self.is_mask_selected;
        let source = if mask {
            layer
                .mask
                .as_ref()
                .and_then(|mask| mask.asset.image.as_gray())
                .cloned()
                .map(|image| PixelImage::Gray(Arc::new(image)))
        } else {
            layer.asset.as_ref().map(|asset| asset.image.clone())
        };
        let Some(mut image) = source else {
            return;
        };
        self.finish_opacity_edit();
        self.is_project_busy = true;
        let outcome = (|| -> Result<(), CoreError> {
            let clip = self.selection_clip();
            // A uniform 1×1 mask can't hold a partial selection; give it the layer's pixel grid first.
            if mask && clip.is_some() && image.width() == 1 && image.height() == 1 {
                let width = layer
                    .asset
                    .as_ref()
                    .map(|asset| asset.image.width())
                    .unwrap_or_else(|| layer.size().width.round().max(1.0) as usize);
                let height = layer
                    .asset
                    .as_ref()
                    .map(|asset| asset.image.height())
                    .unwrap_or_else(|| layer.size().height.round().max(1.0) as usize);
                image = expanded_uniform_mask(&image, width, height)?;
            }
            let transform = if mask {
                mask_transform(&layer)
            } else {
                layer.transform
            };
            let mapping = pixel_to_document(&transform, image.width(), image.height());
            let result = invert_image(&image, mask, mapping, clip.as_ref())?;
            let asset = if mask {
                let gray = result.as_gray().cloned().ok_or_else(|| {
                    CoreError::Message("The inverted mask could not be made.".to_string())
                })?;
                mask_imported_image(gray)
            } else {
                let rgba = result.as_rgba().cloned().ok_or_else(|| {
                    CoreError::Message("The inverted image could not be made.".to_string())
                })?;
                let thumbnail = rgba_thumbnail(&rgba);
                ImportedImage::new(
                    PixelImage::Rgba(Arc::new(rgba)),
                    thumbnail,
                    layer.name.clone(),
                )
            };
            // Only write over the layer the invert was computed from.
            let current = self
                .document
                .as_ref()
                .and_then(|document| document.layers.get(index))
                .cloned();
            let Some(current) = current else {
                return Ok(());
            };
            if current.id != layer.id || !same_pixels(&current.asset, &layer.asset) {
                return Ok(());
            }
            let current_mask_image = current.mask.as_ref().map(|mask| mask.asset.image.clone());
            let layer_mask_image = layer.mask.as_ref().map(|mask| mask.asset.image.clone());
            if !same_pixel_images(&current_mask_image, &layer_mask_image) {
                return Ok(());
            }
            self.begin_edit(if mask { "Invert Mask" } else { "Invert" });
            if let Some(document) = self.document.as_mut() {
                let current = document.layers[index].clone();
                if mask {
                    document.layers[index].mask =
                        current.mask.as_ref().map(|mask| mask.replacing(asset));
                } else {
                    document.layers[index] = ImageLayer::with_id(
                        current.id,
                        Some(asset),
                        current.name,
                        current.is_visible,
                        current.transform,
                        current.parent_id,
                        false,
                        current.opacity,
                        current.blend_mode,
                        current.mask,
                        current.mask_source_id,
                        None,
                        None,
                        None,
                        None,
                    );
                }
            }
            self.end_edit();
            self.brush_revision += 1;
            Ok(())
        })();
        self.is_project_busy = false;
        if let Err(error) = outcome {
            self.brush_error = Some(error.to_string());
        }
    }
}

/// Two optional rasters compared by identity.
fn same_pixel_images(lhs: &Option<PixelImage>, rhs: &Option<PixelImage>) -> bool {
    match (lhs, rhs) {
        (Some(PixelImage::Rgba(lhs)), Some(PixelImage::Rgba(rhs))) => Arc::ptr_eq(lhs, rhs),
        (Some(PixelImage::Gray(lhs)), Some(PixelImage::Gray(rhs))) => Arc::ptr_eq(lhs, rhs),
        (None, None) => true,
        _ => false,
    }
}

/// `EditorSession.expandedUniformMask`: a 1×1 mask stretched over the layer's pixel grid so a
/// partial selection can clip the invert.
fn expanded_uniform_mask(
    image: &PixelImage,
    width: usize,
    height: usize,
) -> Result<PixelImage, CoreError> {
    if width == 0 || height == 0 || width * height > compositor_rs_core::limits::MAX_SURFACE_PIXELS {
        return Err(CoreError::TooLarge(
            compositor_rs_core::limits::MAX_SURFACE_PIXELS,
        ));
    }
    match image {
        PixelImage::Gray(gray) if !gray.is_empty() => Ok(PixelImage::Gray(Arc::new(
            Gray8Image::uniform(width, height, gray.get(0, 0)),
        ))),
        _ => Err(CoreError::Message(
            "The mask could not be expanded.".to_string(),
        )),
    }
}

/// `PixelInvert.run(_:)` over the port's canonical rasters: the pixel crate's
/// `PixelInvert::run(&PixelInvertJob { … })` (premultiplied RGBA becomes `alpha − channel`,
/// a mask becomes `255 − value`, then `PixelAdjust.blend` back through the selection).
fn invert_image(
    image: &PixelImage,
    is_mask: bool,
    pixel_to_document: AffineTransform,
    selection: Option<&SelectionClip>,
) -> Result<PixelImage, CoreError> {
    if image.is_mask() != is_mask {
        return Err(CoreError::Message(
            "The invert target does not match its pixels.".to_string(),
        ));
    }
    compositor_rs_pixels::adjustments::PixelInvert::run(
        &compositor_rs_pixels::adjustments::PixelInvertJob {
            image: image.clone(),
            pixel_to_document,
            selection: selection.cloned(),
        },
    )
}

impl EditorSession {
    // MARK: Magic Wand / Object Selection / Select Subject

    /// What selection-from-image tools read, at document size: every visible layer as shown on the canvas,
    /// or just the active layer's own pixels (without its mask, as Cmd-click selection reads them).
    /// A folder or blank layer reads as transparent.
    pub fn selection_sample(
        &self,
        document: &CanvasDocument,
        sample_all_layers: bool,
    ) -> Option<compositor_rs_core::Rgba8Image> {
        let mut canvas = Canvas::new_rgba(document.width, document.height);
        if sample_all_layers {
            self.draw_live_composite(document, &mut canvas, false);
        } else if let Some(layer) = self.active_layer() {
            if !layer.is_group {
                if let Some(asset) = layer.asset.as_ref() {
                    let transform = self.displayed_transform(layer);
                    LayerRenderer::draw(
                        &asset.image,
                        &transform,
                        transform.center(),
                        1.0,
                        1.0,
                        compositor_rs_core::blend::LayerBlendMode::Normal,
                        None,
                        &mut canvas,
                    );
                }
            }
        }
        Some(canvas.into_rgba())
    }

    /// The Magic Wand: selects pixels similar to the one at `point` (document pixels), read from
    /// the active layer or every visible layer, combined with the current selection by `mode`.
    /// Matching and tracing run off the main thread in the Swift; the port runs them inline with the
    /// busy flag held, then re-checks that the document has not been replaced.
    pub fn magic_wand(&mut self, at: Point, mode: SelectionMode) {
        if !self.can_edit_selection()
            || self.is_project_busy
            || self.selection_move_origin.is_some()
        {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        if !(at.x >= 0.0
            && at.y >= 0.0
            && at.x < document.width as f64
            && at.y < document.height as f64)
        {
            return;
        }
        let Some(sample) = self.selection_sample(&document, self.wand_settings.sample_all_layers)
        else {
            return;
        };
        let settings = self.wand_settings;
        self.is_project_busy = true;
        let result = MagicWand::select(&sample, at, &settings);
        self.is_project_busy = false;
        match result {
            Err(error) => self.brush_error = Some(error.to_string()),
            Ok(path) => {
                if self.document.as_ref().map(|current| current.id) != Some(document.id) {
                    return;
                }
                let Some(path) = path else {
                    // Nothing matched: New clears the selection, as a lasso click enclosing nothing does.
                    if mode == SelectionMode::Replace {
                        self.deselect();
                    }
                    return;
                };
                // A traced outline already lies on the canvas, so a new selection skips the clip to
                // the canvas, which is costly for a detailed outline.
                if mode == SelectionMode::Replace {
                    let antialiased = self.selection_antialiased;
                    self.set_selection(
                        Some(DocumentSelection::with_style(path, antialiased, 0.0)),
                        "Magic Wand",
                    );
                } else {
                    self.apply_selection(path, mode, "Magic Wand");
                }
            }
        }
    }

    /// Object Selection: selects the foreground instance under `point`, read from the active layer
    /// or every visible layer, combined with the current selection by `mode`.
    pub fn select_object(&mut self, at: Point, mode: SelectionMode) {
        if !self.can_edit_selection()
            || self.is_project_busy
            || self.selection_move_origin.is_some()
        {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        if !(at.x >= 0.0
            && at.y >= 0.0
            && at.x < document.width as f64
            && at.y < document.height as f64)
        {
            return;
        }
        let Some(sample) =
            self.selection_sample(&document, self.object_selection_settings.sample_all_layers)
        else {
            return;
        };
        let edge_offset = self.object_selection_settings.edge_offset.clamp(-10, 10);
        let smooth_edges = self.selection_antialiased;
        self.is_project_busy = true;
        let result = ObjectSelection::select(&sample, at, edge_offset, smooth_edges);
        self.is_project_busy = false;
        match result {
            Err(error) => self.brush_error = Some(error.to_string()),
            Ok(path) => {
                if self.document.as_ref().map(|current| current.id) != Some(document.id) {
                    return;
                }
                let Some(path) = path else {
                    if mode == SelectionMode::Replace {
                        self.deselect();
                    }
                    return;
                };
                if mode == SelectionMode::Replace {
                    let antialiased = self.selection_antialiased;
                    self.set_selection(
                        Some(DocumentSelection::with_style(path, antialiased, 0.0)),
                        "Object Selection",
                    );
                } else {
                    self.apply_selection(path, mode, "Object Selection");
                }
            }
        }
    }

    /// Select → Subject: the foreground the segmentation finds in the canvas as it is shown,
    /// outlined as a selection. The same shape Remove Background masks out, as a selection instead.
    pub fn can_select_subject(&self) -> bool {
        self.can_edit_selection() && self.document.is_some() && !self.is_project_busy
    }

    pub fn select_subject(&mut self, mode: SelectionMode) {
        if !self.can_select_subject() {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        let mut canvas = Canvas::new_rgba(document.width, document.height);
        self.draw_live_composite(&document, &mut canvas, false);
        let shown = canvas.into_rgba();
        self.is_project_busy = true;
        let found = SubjectRemoval::subject_mask(&shown, None, &FilterSettings::default());
        self.is_project_busy = false;
        if self.document.as_ref().map(|current| current.id) != Some(document.id) {
            return;
        }
        match found {
            Err(error) => self.brush_error = Some(error.to_string()),
            Ok(mask) => {
                // White where the subject is, so its outline is the selection.
                let Some(traced) = MaskTracing::white_pixels(&mask) else {
                    return;
                };
                let to_document = pixel_to_document(
                    &transform_at(Point::ZERO, document.size()),
                    mask.width(),
                    mask.height(),
                );
                let outline = traced.transformed(&to_document);
                self.apply_selection(outline, mode, "Select Subject");
            }
        }
    }

    // MARK: Loading a layer's pixels or mask as a selection

    /// Cmd-click on a mask thumbnail: the mask's black (hidden) areas become the
    /// selection. Shift adds to the current selection; Option subtracts from it.
    pub fn load_mask_selection(&mut self, layer_id: Id, mode: SelectionMode) {
        if !self.can_edit_selection() {
            return;
        }
        let Some((image, to_document)) = self.document.as_ref().and_then(|document| {
            let layer = document.layers.iter().find(|layer| layer.id == layer_id)?;
            let mask = layer.mask.as_ref()?;
            let gray = mask.asset.image.as_gray()?;
            Some((
                gray.clone(),
                pixel_to_document(&mask_transform(layer), gray.width(), gray.height()),
            ))
        }) else {
            return;
        };
        let Some(traced) = MaskTracing::dark_pixels(&image) else {
            return;
        };
        let outline = traced.transformed(&to_document);
        self.apply_selection(outline, mode, "Load Mask Selection");
    }

    /// Cmd-click on a layer thumbnail: the layer's visible (≥ 50% opaque) pixels become
    /// the selection, ignoring its mask, as in Photoshop. Shift adds; Option subtracts.
    pub fn load_layer_selection(&mut self, layer_id: Id, mode: SelectionMode) {
        if !self.can_edit_selection() {
            return;
        }
        let Some((image, to_document)) = self.document.as_ref().and_then(|document| {
            let layer = document.layers.iter().find(|layer| layer.id == layer_id)?;
            if layer.is_group {
                return None;
            }
            let rgba = layer.asset.as_ref()?.image.as_rgba()?;
            Some((
                rgba.clone(),
                pixel_to_document(&layer.transform, rgba.width(), rgba.height()),
            ))
        }) else {
            return;
        };
        let Some(traced) = MaskTracing::opaque_pixels(&image) else {
            return;
        };
        let outline = traced.transformed(&to_document);
        self.apply_selection(outline, mode, "Load Layer Selection");
    }

    // MARK: Select > Color Range

    pub fn can_select_color_range(&self) -> bool {
        self.document.is_some() && self.color_range.is_none() && self.can_edit_selection()
    }

    pub fn begin_color_range(&mut self) {
        if !self.can_select_color_range() {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        let Some(image) = self.selection_sample(&document, true) else {
            return;
        };
        let original = self.selection().cloned();
        self.color_range = Some(ColorRangeEdit::new(image, original));
    }

    /// A click on the canvas while the panel is open. Shift adds the color and Option takes it away, whichever
    /// eyedropper is chosen.
    pub fn sample_color_range(&mut self, at: Point, shift: bool, option: bool) {
        let Some(edit) = self.color_range.as_mut() else {
            return;
        };
        let Some(color) = color_in_image(&edit.image, at) else {
            return;
        };
        let mode = if option {
            HueSampleMode::Remove
        } else if shift {
            HueSampleMode::Add
        } else {
            edit.sample_mode
        };
        match mode {
            HueSampleMode::Replace => {
                edit.include.clear();
                edit.include.extend_from_slice(&color);
                edit.exclude.clear();
            }
            HueSampleMode::Add => edit.include.extend_from_slice(&color),
            HueSampleMode::Remove => edit.exclude.extend_from_slice(&color),
        }
        self.update_color_range();
    }

    /// Matches the image against the picked colors and shows the result as the selection, without
    /// an undo step.
    ///
    /// The Swift ran the match on a detached task and let a newer change supersede one still being
    /// worked out; the port's match is synchronous, so no older one is ever in flight. `generation`
    /// is kept for the panel's state parity.
    pub fn update_color_range(&mut self) {
        let Some(edit) = self.color_range.as_mut() else {
            return;
        };
        if self.document.is_none() {
            return;
        }
        edit.generation += 1;
        if !edit.has_colors() {
            edit.preview = None;
            let original = edit.original.clone();
            if let Some(document) = self.document.as_mut() {
                document.selection = original;
            }
            return;
        }
        let fuzziness = edit.fuzziness.round() as i32;
        let mask = color_range_mask(
            &edit.image,
            &edit.include,
            &edit.exclude,
            fuzziness,
            edit.invert,
        );
        edit.preview = Some(preview_of_mask(
            &mask,
            edit.image.width(),
            edit.image.height(),
        ));
        if mask.iter().all(|&value| value == 0) {
            edit.error = None;
            if let Some(document) = self.document.as_mut() {
                document.selection = None;
            }
            return;
        }
        match MagicWand::outline(&mask, edit.image.width(), edit.image.height()) {
            Ok(path) => {
                edit.error = None;
                let antialiased = self.selection_antialiased;
                let value = path.map(|path| DocumentSelection::with_style(path, antialiased, 0.0));
                if let Some(document) = self.document.as_mut() {
                    document.selection = value;
                }
            }
            Err(error) => {
                edit.preview = None;
                edit.error = Some(error.to_string());
            }
        }
    }

    /// OK in the Color Range panel: the live selection becomes one "Color Range" undo step.
    pub fn commit_color_range(&mut self) {
        let Some(edit) = self.color_range.take() else {
            return;
        };
        let result = self.selection().cloned();
        if let Some(document) = self.document.as_mut() {
            document.selection = edit.original.clone();
        }
        if !edit.has_colors() || edit.error.is_some() {
            return;
        }
        match result {
            Some(selection) => self.set_selection(Some(selection), "Color Range"),
            None => self.deselect(),
        }
    }

    /// Cancel in the Color Range panel: the selection that was there before comes back.
    pub fn cancel_color_range(&mut self) {
        let Some(edit) = self.color_range.take() else {
            return;
        };
        if let Some(document) = self.document.as_mut() {
            document.selection = edit.original;
        }
    }
}

/// The straight color under `point` (document pixels), averaged over the 3 × 3 pixels around it.
/// Pixels outside the image are transparent and contribute nothing; a window that is entirely
/// outside (or fully transparent) reads as no color.
fn color_in_image(image: &compositor_rs_core::Rgba8Image, point: Point) -> Option<[u8; 3]> {
    let x = point.x.floor() as i64;
    let y = point.y.floor() as i64;
    if !point.x.is_finite()
        || !point.y.is_finite()
        || x < 0
        || y < 0
        || x >= image.width() as i64
        || y >= image.height() as i64
    {
        return None;
    }
    let mut sums = [0u64; 4];
    for row in 0..3i64 {
        for column in 0..3i64 {
            let source_x = x + column - 1;
            let source_y = y + row - 1;
            if source_x >= 0
                && source_y >= 0
                && source_x < image.width() as i64
                && source_y < image.height() as i64
            {
                let pixel = image.get(source_x as usize, source_y as usize);
                for channel in 0..4 {
                    sums[channel] += pixel[channel] as u64;
                }
            }
        }
    }
    if sums[3] == 0 {
        return None;
    }
    Some(std::array::from_fn(|channel| {
        ((sums[channel] * 255 + sums[3] / 2) / sums[3]).min(255) as u8
    }))
}

/// The mask shrunk to the panel's preview, white where selected, at twice its size for a sharp picture.
fn preview_of_mask(mask: &[u8], width: usize, height: usize) -> Gray8Image {
    let source = Gray8Image::from_data(width, height, mask.to_vec());
    let scale = (ColorRangeEdit::PREVIEW_SIZE.width / width.max(1) as f64)
        .min(ColorRangeEdit::PREVIEW_SIZE.height / height.max(1) as f64)
        * 2.0;
    let preview_width = ((width as f64 * scale) as usize).max(1);
    let preview_height = ((height as f64 * scale) as usize).max(1);
    scale_gray(&source, preview_width, preview_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::layer_transform::TransformEdit;

    fn session(width: usize, height: usize) -> EditorSession {
        let mut session = EditorSession::new();
        session.document = Some(CanvasDocument::new(width, height));
        session
    }

    fn rect_selection(rect: Rect) -> DocumentSelection {
        DocumentSelection::new(Path::rect(rect))
    }

    fn contains(selection: &DocumentSelection, x: f64, y: f64) -> bool {
        selection.path.contains(Point::new(x, y), FillRule::Winding)
    }

    #[test]
    fn apply_selection_adds_and_subtracts_geometry() {
        let mut session = session(100, 100);
        session.set_selection(
            Some(rect_selection(Rect::new(10.0, 10.0, 40.0, 40.0))),
            "Test",
        );
        session.apply_selection(
            Path::rect(Rect::new(30.0, 30.0, 40.0, 40.0)),
            SelectionMode::Add,
            "Lasso",
        );
        let selection = session.selection().expect("a selection").clone();
        assert!(contains(&selection, 15.0, 15.0), "the first rectangle");
        assert!(contains(&selection, 60.0, 60.0), "the added rectangle");
        assert!(!contains(&selection, 5.0, 5.0), "outside both");
        let bounds = selection.path.bounding_box();
        assert_eq!(bounds.min_x(), 10.0);
        assert_eq!(bounds.min_y(), 10.0);
        assert_eq!(bounds.width(), 60.0);
        assert_eq!(bounds.height(), 60.0);

        session.apply_selection(
            Path::rect(Rect::new(10.0, 10.0, 20.0, 20.0)),
            SelectionMode::Subtract,
            "Lasso",
        );
        let selection = session.selection().expect("a selection").clone();
        assert!(!contains(&selection, 15.0, 15.0), "subtracted away");
        assert!(contains(&selection, 45.0, 45.0), "the rest stays");

        // Subtracting from no selection selects nothing new, so nothing changes.
        session.deselect();
        session.apply_selection(
            Path::rect(Rect::new(0.0, 0.0, 10.0, 10.0)),
            SelectionMode::Subtract,
            "Lasso",
        );
        assert!(session.selection().is_none());
    }

    #[test]
    fn apply_selection_clips_to_the_canvas() {
        let mut session = session(50, 50);
        session.apply_selection(
            Path::rect(Rect::new(-20.0, -20.0, 100.0, 100.0)),
            SelectionMode::Replace,
            "Rectangular Marquee",
        );
        let selection = session.selection().expect("a selection").clone();
        let bounds = selection.path.bounding_box();
        assert_eq!(bounds, Rect::new(0.0, 0.0, 50.0, 50.0));
    }

    #[test]
    fn empty_selection_touches_nothing() {
        let mut session = session(50, 50);
        let mut layer = ImageLayer::blank("Layer 1", Size::new(50.0, 50.0));
        layer.name = "Layer 1".to_string();
        let layer_id = layer.id;
        session
            .document
            .as_mut()
            .expect("a document")
            .layers
            .push(layer);
        session.set_active_layer(Some(layer_id));
        // No selection: edits run everywhere.
        assert!(session.can_paint());
        assert!(session.selection_clip().is_none());

        session.set_selection(Some(DocumentSelection::new(Path::empty())), "Empty");
        let selection = session
            .selection()
            .expect("an explicit empty selection")
            .clone();
        assert!(selection.is_empty());
        // An explicit empty selection clips everything away rather than selecting everything.
        let clip = session.selection_clip().expect("a clip");
        assert!(clip.coverage.is_none());
        // And it refuses the edits a nil selection would allow.
        assert!(!session.can_paint());
        assert!(!session.can_modify_selection());
    }

    #[test]
    fn select_menu_transactions_and_guards() {
        let mut session = session(100, 100);
        session.select_all();
        assert_eq!(session.history.undo_name(), "Select All");
        let selection = session.selection().expect("a selection").clone();
        assert!(contains(&selection, 50.0, 50.0));
        assert!(contains(&selection, 0.5, 99.5));

        session.invert_selection();
        assert_eq!(session.history.undo_name(), "Inverse");
        // The inverse of everything is no selection at all.
        assert!(session.selection().is_none());

        session.select_all();
        session.deselect();
        assert_eq!(session.history.undo_name(), "Deselect");
        assert!(session.selection().is_none());
        // Deselecting again changes nothing.
        session.deselect();
        assert_eq!(session.history.undo_name(), "Deselect");
    }

    #[test]
    fn set_selection_is_idempotent() {
        let mut session = session(10, 10);
        session.set_selection(Some(rect_selection(Rect::new(0.0, 0.0, 5.0, 5.0))), "Test");
        assert_eq!(session.history.undo_count(), 1);
        session.set_selection(Some(rect_selection(Rect::new(0.0, 0.0, 5.0, 5.0))), "Test");
        assert_eq!(session.history.undo_count(), 1);
    }

    #[test]
    fn modify_amounts_record_their_operations() {
        let mut session = session(200, 200);
        session.set_selection(
            Some(rect_selection(Rect::new(50.0, 50.0, 100.0, 100.0))),
            "Test",
        );

        session.prompt_selection_amount(SelectionAmountOperation::Expand);
        assert_eq!(
            session.selection_amount_operation,
            Some(SelectionAmountOperation::Expand)
        );
        session.confirm_selection_amount(0); // out of range: still prompting
        assert_eq!(
            session.selection_amount_operation,
            Some(SelectionAmountOperation::Expand)
        );
        session.confirm_selection_amount(10);
        assert_eq!(session.history.undo_name(), "Expand Selection");
        let expanded = session.selection().expect("a selection").clone();
        assert!(contains(&expanded, 45.0, 100.0), "grown by ten pixels");

        session.prompt_selection_amount(SelectionAmountOperation::Contract);
        session.confirm_selection_amount(10);
        assert_eq!(session.history.undo_name(), "Contract Selection");
        let contracted = session.selection().expect("a selection").clone();
        assert!(!contains(&contracted, 45.0, 100.0), "shrunk again");

        session.prompt_selection_amount(SelectionAmountOperation::Feather);
        session.confirm_selection_amount(30);
        assert_eq!(session.history.undo_name(), "Feather Selection");
        assert_eq!(session.selection().expect("a selection").feather, 30.0);
    }

    #[test]
    fn selection_move_is_one_undo_step() {
        let mut session = session(100, 100);
        session.set_selection(
            Some(rect_selection(Rect::new(10.0, 10.0, 20.0, 20.0))),
            "Test",
        );
        assert!(session.begin_selection_move());
        session.move_selection(Size::new(5.4, -3.6));
        session.end_selection_move();
        assert_eq!(session.history.undo_name(), "Move Selection");
        let selection = session.selection().expect("a selection").clone();
        assert!(contains(&selection, 20.5, 11.5), "rounded to whole pixels");
        assert_eq!(session.selection_move_origin, None);
    }

    #[test]
    fn copy_region_rounds_boolean_noise() {
        let mut session = session(100, 100);
        session.set_selection(
            Some(rect_selection(Rect::new(10.0, 20.0, 39.9999999, 20.0))),
            "Test",
        );
        let region = session.selection_copy_region().expect("a region");
        assert_eq!(region, Rect::new(10.0, 20.0, 40.0, 20.0));
        session.deselect();
        // No selection: the whole canvas.
        assert_eq!(
            session.selection_copy_region().expect("a region"),
            Rect::new(0.0, 0.0, 100.0, 100.0)
        );
    }

    #[test]
    fn floating_transform_merge_moves_the_pixels_and_the_outline() {
        let mut session = session(40, 40);
        let source_image = compositor_rs_core::Rgba8Image::opaque(10, 10, [255, 0, 0, 255]);
        let mut source = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(source_image)),
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::new(1, 1))),
                "Source",
            ),
            Point::new(5.0, 5.0),
        );
        source.name = "Source".to_string();
        let source_id = source.id;
        let floating_image = compositor_rs_core::Rgba8Image::opaque(4, 4, [0, 0, 255, 255]);
        let mut floating = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(floating_image)),
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::new(1, 1))),
                "Floating Selection",
            ),
            Point::new(15.0, 15.0),
        );
        floating.name = "Floating Selection".to_string();
        let floating_id = floating.id;
        let floating_transform = floating.transform;

        {
            let document = session.document.as_mut().expect("a document");
            document.layers = vec![source, floating];
            document.selection = Some(rect_selection(Rect::new(15.0, 15.0, 4.0, 4.0)));
        }
        session.set_active_layer(Some(floating_id));
        let before = session.document.clone().expect("a document");

        session.begin_edit("Transform Selection");
        let mut edit = TransformEdit::new(
            floating_id,
            transform_at(Point::new(20.0, 20.0), Size::new(4.0, 4.0)),
            true,
        );
        edit.floating = Some(compositor_rs_core::layer_transform::FloatingTransform {
            source_id,
            before,
            before_active: Some(floating_id),
            original: floating_transform,
            pixel_size: Size::new(4.0, 4.0),
        });
        let floating = edit.floating.clone().expect("a floating transform");
        session.merge_floating_transform(&edit, &floating);

        let document = session.document.as_ref().expect("a document");
        assert_eq!(document.layers.len(), 1, "the floating layer is gone");
        assert_eq!(document.layers[0].id, source_id);
        assert_eq!(session.active_layer_id, Some(source_id));
        assert_eq!(session.history.undo_name(), "Transform Selection");
        let selection = document.selection.as_ref().expect("the moved selection");
        assert!(
            contains(selection, 20.5, 20.5),
            "the outline moved with the pixels"
        );
        assert!(!contains(selection, 16.0, 16.0));
    }

    #[test]
    fn floating_transform_cancel_restores_the_document() {
        let mut session = session(40, 40);
        let source = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::opaque(
                    10,
                    10,
                    [255, 0, 0, 255],
                ))),
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::new(1, 1))),
                "Source",
            ),
            Point::new(5.0, 5.0),
        );
        let source_id = source.id;
        let floating = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::opaque(
                    4,
                    4,
                    [0, 0, 255, 255],
                ))),
                PixelImage::Rgba(Arc::new(compositor_rs_core::Rgba8Image::new(1, 1))),
                "Floating Selection",
            ),
            Point::new(15.0, 15.0),
        );
        {
            let document = session.document.as_mut().expect("a document");
            document.layers = vec![source.clone(), floating.clone()];
            document.selection = Some(rect_selection(Rect::new(15.0, 15.0, 4.0, 4.0)));
        }
        session.set_active_layer(Some(floating.id));
        let before = session.document.clone().expect("a document");

        session.begin_edit("Transform Selection");
        let floating_transform = compositor_rs_core::layer_transform::FloatingTransform {
            source_id,
            before: before.clone(),
            before_active: Some(floating.id),
            original: floating.transform,
            pixel_size: Size::new(4.0, 4.0),
        };
        session
            .document
            .as_mut()
            .expect("a document")
            .layers
            .remove(0);
        session.cancel_floating_transform(&floating_transform);

        assert_eq!(session.document.as_ref(), Some(&before), "restored exactly");
        assert_eq!(session.active_layer_id, Some(floating.id));
        assert!(
            session.history.undo_count() == 0,
            "nothing to undo after a cancel"
        );
    }

    #[test]
    fn color_in_image_averages_the_neighborhood() {
        let mut image = compositor_rs_core::Rgba8Image::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                image.set(x, y, [100, 150, 200, 255]);
            }
        }
        assert_eq!(
            color_in_image(&image, Point::new(2.0, 2.0)),
            Some([100, 150, 200])
        );
        // A corner window averages only what is inside the image; opaque neighbors keep the color.
        assert_eq!(
            color_in_image(&image, Point::new(0.0, 0.0)),
            Some([100, 150, 200])
        );
        // Outside the image: no color.
        assert_eq!(color_in_image(&image, Point::new(9.0, 0.0)), None);
        assert_eq!(color_in_image(&image, Point::new(-1.0, 0.0)), None);
    }

    #[test]
    fn mask_background_reads_the_thumbnail_border() {
        let white = Gray8Image::uniform(4, 4, 255);
        assert_eq!(mask_background(&PixelImage::Gray(Arc::new(white))), 1.0);
        let black = Gray8Image::uniform(4, 4, 0);
        assert_eq!(mask_background(&PixelImage::Gray(Arc::new(black))), 0.0);
    }

    #[test]
    fn draw_gray_scaled_copies_nearest_into_the_rect() {
        let image = Gray8Image::from_data(2, 2, vec![0, 10, 20, 30]);
        let mut canvas = Gray8Image::uniform(4, 4, 255);
        draw_gray_scaled(&mut canvas, &image, Rect::new(0.0, 0.0, 4.0, 4.0));
        assert_eq!(canvas.get(0, 0), 0);
        assert_eq!(canvas.get(1, 1), 0);
        assert_eq!(canvas.get(3, 3), 30);
        assert_eq!(canvas.get(3, 0), 10);
    }

    #[test]
    fn preview_of_mask_scales_to_the_panel() {
        let mask = vec![255u8; 100 * 100];
        let preview = preview_of_mask(&mask, 100, 100);
        // 2× the 292×200 fit, so the 100×100 mask lands at 400×400.
        assert_eq!((preview.width(), preview.height()), (400, 400));
        assert!(preview.data().iter().all(|&value| value == 255));
    }
}
