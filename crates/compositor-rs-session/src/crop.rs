//! The Crop tool: the draft rectangle, its ratio choices and drag modes, the snapping an in-progress
//! drag applies, the overlay's view-space geometry, and the commit that resizes the canvas.
//!
//! Ported from `Document/Crop.swift`. The Move-tool snapping helpers Swift keeps in this file
//! (`transformSnapTargets`, `snappedMove`, `snappedResizePoint`, `snappedPoint`,
//! `snappedSelectionOffset`, `cropSnapTargets`) live here too, so `transform.rs` and `selection.rs`
//! can call them instead of duplicating them.

use std::collections::HashSet;

use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::layer_transform::{
    LayerTransform, TransformDrag, TransformDragMode, TransformSnap,
};
use compositor_rs_core::limits::MAX_SIDE_EXTENT;
use compositor_rs_core::viewport::CanvasViewport;
use compositor_rs_core::Id;

use crate::canvas_ops::CanvasResizer;
use crate::session::EditorSession;
use compositor_rs_pixels::warp::DistortWarp;

/// `CropGeometry`: the crop rectangle's arithmetic — snapping to whole pixels, the size limits, and
/// the rectangle a drag draws.
pub struct CropGeometry;

impl CropGeometry {
    /// Rounds a rectangle outwards to whole pixels, never smaller than 1×1.
    pub fn snapped(rect: Rect) -> Rect {
        let rect = rect.standardized();
        let x = rect.min_x().round();
        let y = rect.min_y().round();
        Rect::new(
            x,
            y,
            (rect.max_x().round() - x).max(1.0),
            (rect.max_y().round() - y).max(1.0),
        )
    }

    /// Within the side limit, at least a pixel each way, and not absurdly far off-canvas.
    pub fn valid(rect: Rect) -> bool {
        [rect.min_x(), rect.min_y(), rect.width(), rect.height()]
            .iter()
            .all(|value| value.is_finite())
            && (1.0..=MAX_SIDE_EXTENT).contains(&rect.width())
            && (1.0..=MAX_SIDE_EXTENT).contains(&rect.height())
            && rect.min_x().abs() <= 1_000_000.0
            && rect.min_y().abs() <= 1_000_000.0
    }

    /// A frame dragged from `start` to `end` — or, `symmetric` (Option), grown out from `start` as
    /// its center.
    pub fn create(from: Point, to: Point, ratio: Option<f64>, symmetric: bool) -> Rect {
        let mut dx = to.x - from.x;
        let mut dy = to.y - from.y;
        if let Some(ratio) = ratio {
            if dx.abs() > dy.abs() * ratio {
                dy = if dy < 0.0 { -1.0 } else { 1.0 } * dx.abs() / ratio;
            } else {
                dx = if dx < 0.0 { -1.0 } else { 1.0 } * dy.abs() * ratio;
            }
        }
        if symmetric {
            CropGeometry::snapped(Rect::new(
                from.x - dx.abs(),
                from.y - dy.abs(),
                dx.abs() * 2.0,
                dy.abs() * 2.0,
            ))
        } else {
            CropGeometry::snapped(Rect::new(
                from.x.min(from.x + dx),
                from.y.min(from.y + dy),
                dx.abs(),
                dy.abs(),
            ))
        }
    }
}

/// What a crop drag does (`CropDrag.Mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CropDragMode {
    Create,
    Move,
    Resize(usize),
}

/// A crop drag in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropDrag {
    pub start: Point,
    pub original: Rect,
    pub mode: CropDragMode,
}

impl CropDrag {
    /// `symmetric` (Option held) keeps the frame's center fixed: the opposite edges move with the
    /// dragged ones.
    pub fn updated(&self, to: Point, ratio: Option<f64>, symmetric: bool) -> Rect {
        match self.mode {
            CropDragMode::Create => CropGeometry::create(self.start, to, ratio, symmetric),
            CropDragMode::Move => CropGeometry::snapped(
                self.original
                    .offset_by(to.x - self.start.x, to.y - self.start.y),
            ),
            CropDragMode::Resize(index) => {
                let transform = LayerTransform {
                    origin: self.original.origin,
                    size: self.original.size,
                    ..LayerTransform::default()
                };
                let drag = TransformDrag {
                    original: transform,
                    start: self.start,
                    mode: TransformDragMode::Resize(index),
                    original_corners: None,
                };
                let next = drag.updated(to, ratio.is_some(), false, symmetric);
                CropGeometry::snapped(Rect::from_origin_size(next.origin, next.size))
            }
        }
    }
}

/// Crop edges snap to nearby layer and canvas edges while dragging.
#[derive(Clone, Debug, PartialEq)]
pub struct CropSnap {
    /// Document x and y positions to snap to.
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    /// How close, in document pixels, an edge must come to snap.
    pub tolerance: f64,
}

impl CropSnap {
    pub fn new(xs: Vec<f64>, ys: Vec<f64>, tolerance: f64) -> Self {
        CropSnap { xs, ys, tolerance }
    }

    fn nearest(value: f64, targets: &[f64], tolerance: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        for &target in targets {
            if (target - value).abs() <= tolerance {
                if let Some(current) = best {
                    if (current - value).abs() <= (target - value).abs() {
                        continue;
                    }
                }
                best = Some(target);
            }
        }
        best
    }

    /// Moving the frame snaps its closest edges and keeps its size; creating or resizing snaps only
    /// the edges on the side being dragged — mirrored about the center when `symmetric`. With a
    /// fixed ratio only moves snap, so the ratio stays exact.
    pub fn apply(
        &self,
        rect: Rect,
        drag: &CropDrag,
        point: Point,
        ratio: Option<f64>,
        symmetric: bool,
    ) -> Rect {
        if !(self.tolerance > 0.0) {
            return rect;
        }
        let horizontal: bool;
        let vertical: bool;
        match drag.mode {
            CropDragMode::Move => {
                let shift = |edges: [f64; 2], targets: &[f64]| -> f64 {
                    edges
                        .iter()
                        .filter_map(|&edge| {
                            CropSnap::nearest(edge, targets, self.tolerance)
                                .map(|target| target - edge)
                        })
                        .min_by(|a, b| a.abs().total_cmp(&b.abs()))
                        .unwrap_or(0.0)
                };
                let dx = shift([rect.min_x(), rect.max_x()], &self.xs);
                let dy = shift([rect.min_y(), rect.max_y()], &self.ys);
                return rect.offset_by(dx, dy);
            }
            CropDragMode::Create => {
                if ratio.is_some() {
                    return rect;
                }
                horizontal = true;
                vertical = true;
            }
            CropDragMode::Resize(index) => {
                if ratio.is_some() {
                    return rect;
                }
                let handle = LayerTransform::HANDLES[index];
                horizontal = handle.x != 0.5;
                vertical = handle.y != 0.5;
            }
        }
        let mut result = rect;
        // The dragged edge is the one on the pointer's side.
        if horizontal {
            if (point.x - result.min_x()).abs() <= (point.x - result.max_x()).abs() {
                if let Some(x) = CropSnap::nearest(result.min_x(), &self.xs, self.tolerance) {
                    if x < result.max_x() {
                        result = Rect::new(x, result.min_y(), result.max_x() - x, result.height());
                    }
                }
            } else if let Some(x) = CropSnap::nearest(result.max_x(), &self.xs, self.tolerance) {
                if x > result.min_x() {
                    result.size.width = x - result.min_x();
                }
            }
        }
        if vertical {
            if (point.y - result.min_y()).abs() <= (point.y - result.max_y()).abs() {
                if let Some(y) = CropSnap::nearest(result.min_y(), &self.ys, self.tolerance) {
                    if y < result.max_y() {
                        result = Rect::new(result.min_x(), y, result.width(), result.max_y() - y);
                    }
                }
            } else if let Some(y) = CropSnap::nearest(result.max_y(), &self.ys, self.tolerance) {
                if y > result.min_y() {
                    result.size.height = y - result.min_y();
                }
            }
        }
        if symmetric {
            // The snapped (dragged) edge sets the half size; the opposite edge mirrors it about the
            // center.
            let mut center = Point::new(drag.original.mid_x(), drag.original.mid_y());
            if let CropDragMode::Create = drag.mode {
                center = drag.start;
            }
            if horizontal {
                let half = if point.x >= center.x {
                    result.max_x() - center.x
                } else {
                    center.x - result.min_x()
                };
                if half >= 0.5 {
                    result.origin.x = center.x - half;
                    result.size.width = half * 2.0;
                }
            }
            if vertical {
                let half = if point.y >= center.y {
                    result.max_y() - center.y
                } else {
                    center.y - result.min_y()
                };
                if half >= 0.5 {
                    result.origin.y = center.y - half;
                    result.size.height = half * 2.0;
                }
            }
        }
        result
    }
}

/// The crop frame as the canvas draws it (`TransformOverlay.cropViewRect`, `cropHandles`,
/// `cropResizeRegions`). Document and view geometry only — the overlay itself draws these.
pub struct CropOverlay;

impl CropOverlay {
    /// The resize handles' hit radius, in view points.
    pub const HANDLE_RADIUS: f64 = 10.0;

    /// The crop frame in view points.
    pub fn view_rect(viewport: &CanvasViewport, rect: Rect, document_size: Size) -> Rect {
        let origin = viewport.view_point(rect.origin, document_size);
        let scale = viewport.points_per_pixel();
        Rect::new(
            origin.x,
            origin.y,
            rect.width() * scale,
            rect.height() * scale,
        )
    }

    /// The eight handles' view points, in `LayerTransform.handles` order.
    pub fn handles(view_rect: Rect) -> [Point; 8] {
        LayerTransform::HANDLES.map(|handle| {
            Point::new(
                view_rect.min_x() + handle.x * view_rect.width(),
                view_rect.min_y() + handle.y * view_rect.height(),
            )
        })
    }

    /// The hit regions of the resize handles, the corners first and then the four edges — the order
    /// `cropResizeRegions` lists them in, which decides which wins where they overlap.
    pub fn resize_regions(view_rect: Rect) -> Vec<(usize, Rect)> {
        let handles = CropOverlay::handles(view_rect);
        let radius = CropOverlay::HANDLE_RADIUS;
        let mut regions: Vec<(usize, Rect)> = [0usize, 2, 4, 6]
            .into_iter()
            .map(|index| {
                (
                    index,
                    Rect::new(
                        handles[index].x - radius,
                        handles[index].y - radius,
                        radius * 2.0,
                        radius * 2.0,
                    ),
                )
            })
            .collect();
        // Entire edges are draggable, not just the small midpoint squares.
        for index in [1usize, 5] {
            regions.push((
                index,
                Rect::new(
                    view_rect.min_x() + radius,
                    handles[index].y - radius,
                    (view_rect.width() - radius * 2.0).max(0.0),
                    radius * 2.0,
                ),
            ));
        }
        for index in [3usize, 7] {
            regions.push((
                index,
                Rect::new(
                    handles[index].x - radius,
                    view_rect.min_y() + radius,
                    radius * 2.0,
                    (view_rect.height() - radius * 2.0).max(0.0),
                ),
            ));
        }
        regions
    }
}

impl EditorSession {
    /// What a moving layer snaps to: View > Snap To targets, including the canvas and other layers
    /// by default.
    pub fn transform_snap_targets(&self, excluding: &HashSet<Id>) -> (Vec<f64>, Vec<f64>) {
        self.alignment_snap_targets(excluding, true)
    }

    /// `draft` nudged so the layer it places lines up with a nearby edge or center; `moving` is what
    /// is being dragged, and `tolerance` is in document pixels.
    pub fn snapped_move(
        &mut self,
        draft: LayerTransform,
        moving: &HashSet<Id>,
        tolerance: f64,
    ) -> LayerTransform {
        if !self.snapping_enabled {
            self.snap_guides = (Vec::new(), Vec::new());
            return draft;
        }
        let corners = DistortWarp::corners(&draft);
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for corner in corners {
            min_x = min_x.min(corner.x);
            max_x = max_x.max(corner.x);
            min_y = min_y.min(corner.y);
            max_y = max_y.max(corner.y);
        }
        if !(min_x.is_finite() && max_x.is_finite() && min_y.is_finite() && max_y.is_finite()) {
            return draft;
        }
        let box_ = Rect::new(min_x, min_y, max_x - min_x, max_y - min_y);
        let targets = self.transform_snap_targets(moving);
        let snap = TransformSnap::offset(box_, &targets.0, &targets.1, tolerance);
        self.snap_guides = (
            snap.x.map(|value| vec![value]).unwrap_or_default(),
            snap.y.map(|value| vec![value]).unwrap_or_default(),
        );
        if snap.offset == Size::ZERO {
            return draft;
        }
        let mut snapped = draft;
        snapped.origin.x += snap.offset.width;
        snapped.origin.y += snap.offset.height;
        snapped
    }

    /// A resize handle dragged to `point`: the pointer nudged so the edges the handle moves land on a
    /// nearby target, within `tolerance` document pixels, as a moved layer's do. `update` is the
    /// drag's own result for a pointer. Each edge snaps on its own; kept `proportional`, only the
    /// nearer one does and the other follows the ratio. An upright layer only: a turned one's edges
    /// don't run along the targets.
    pub fn snapped_resize_point(
        &mut self,
        point: Point,
        drag: TransformDrag,
        proportional: bool,
        moving: &HashSet<Id>,
        tolerance: f64,
        update: impl Fn(Point) -> LayerTransform,
    ) -> Point {
        let resize_index = match drag.mode {
            TransformDragMode::Resize(index) => Some(index),
            _ => None,
        };
        let Some(index) =
            resize_index.filter(|_| self.snapping_enabled && drag.original.radians() == 0.0)
        else {
            self.snap_guides = (Vec::new(), Vec::new());
            return point;
        };
        let handle = LayerTransform::HANDLES[index];
        let targets = self.transform_snap_targets(moving);
        let grab = drag.original.point(handle);
        // Where the dragged handle is, to tell its edge from the one across from it.
        let at = Point::new(
            grab.x + point.x - drag.start.x,
            grab.y + point.y - drag.start.y,
        );
        let edge = |transform: &LayerTransform, horizontal: bool| -> f64 {
            let box_ = Rect::from_origin_size(transform.origin, transform.size);
            if horizontal {
                if (box_.min_x() - at.x).abs() <= (box_.max_x() - at.x).abs() {
                    box_.min_x()
                } else {
                    box_.max_x()
                }
            } else if (box_.min_y() - at.y).abs() <= (box_.max_y() - at.y).abs() {
                box_.min_y()
            } else {
                box_.max_y()
            }
        };
        let nearest = |value: f64, lines: &[f64]| -> Option<f64> {
            lines
                .iter()
                .copied()
                .filter(|line| (line - value).abs() <= tolerance)
                .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
        };
        let draft = update(point);
        let mut snaps: Vec<(bool, f64)> = Vec::new();
        if handle.x != 0.5 {
            if let Some(x) = nearest(edge(&draft, true), &targets.0) {
                snaps.push((true, x));
            }
        }
        if handle.y != 0.5 {
            if let Some(y) = nearest(edge(&draft, false), &targets.1) {
                snaps.push((false, y));
            }
        }
        if proportional && snaps.len() == 2 {
            let keep = *snaps
                .iter()
                .min_by(|a, b| {
                    (a.1 - edge(&draft, a.0))
                        .abs()
                        .total_cmp(&(b.1 - edge(&draft, b.0)).abs())
                })
                .expect("two snaps");
            snaps = vec![keep];
        }
        // An edge follows the pointer in a straight line along each axis, so one step measured across
        // a pixel lands it.
        let mut result = point;
        for snap in &snaps {
            let before = edge(&update(result), snap.0);
            let mut nudged = result;
            if snap.0 {
                nudged.x += 1.0;
            } else {
                nudged.y += 1.0;
            }
            let per_pixel = edge(&update(nudged), snap.0) - before;
            if per_pixel.abs() <= 0.01 {
                continue;
            }
            let shift = (snap.1 - before) / per_pixel;
            if snap.0 {
                result.x += shift;
            } else {
                result.y += shift;
            }
        }
        self.snap_guides = (
            snaps
                .iter()
                .filter(|snap| snap.0)
                .map(|snap| snap.1)
                .collect(),
            snaps
                .iter()
                .filter(|snap| !snap.0)
                .map(|snap| snap.1)
                .collect(),
        );
        result
    }

    /// `point` moved onto the nearest crop target within `tolerance` document pixels, each axis on
    /// its own: where a Marquee or a shape starts and where its corner is dragged to.
    pub fn snapped_point(&mut self, point: Point, tolerance: f64) -> Point {
        if !self.snapping_enabled {
            self.snap_guides = (Vec::new(), Vec::new());
            return point;
        }
        let targets = self.crop_snap_targets();
        let nearest = |value: f64, lines: &[f64]| -> Option<f64> {
            lines
                .iter()
                .copied()
                .filter(|line| (line - value).abs() <= tolerance)
                .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
        };
        let x = nearest(point.x, &targets.0);
        let y = nearest(point.y, &targets.1);
        self.snap_guides = (
            x.map(|value| vec![value]).unwrap_or_default(),
            y.map(|value| vec![value]).unwrap_or_default(),
        );
        Point::new(x.unwrap_or(point.x), y.unwrap_or(point.y))
    }

    /// A selection being moved by `offset` from where it started, nudged so its edges or middle meet
    /// a nearby target within `tolerance` document pixels, each axis on its own. An axis Shift has
    /// locked doesn't snap.
    pub fn snapped_selection_offset(
        &mut self,
        offset: Size,
        tolerance: f64,
        horizontal: bool,
        vertical: bool,
    ) -> Size {
        let Some(origin) = self
            .selection_move_origin
            .clone()
            .filter(|_| self.snapping_enabled)
        else {
            self.snap_guides = (Vec::new(), Vec::new());
            return offset;
        };
        let box_ = origin
            .path
            .bounding_box()
            .offset_by(offset.width.round(), offset.height.round());
        let targets = self.crop_snap_targets();
        let xs: &[f64] = if horizontal { &targets.0 } else { &[] };
        let ys: &[f64] = if vertical { &targets.1 } else { &[] };
        let snap = TransformSnap::offset(box_, xs, ys, tolerance);
        self.snap_guides = (
            snap.x.map(|value| vec![value]).unwrap_or_default(),
            snap.y.map(|value| vec![value]).unwrap_or_default(),
        );
        Size::new(
            offset.width.round() + snap.offset.width,
            offset.height.round() + snap.offset.height,
        )
    }

    /// What crop edges snap to: View > Snap To targets, without layer/canvas centers.
    pub fn crop_snap_targets(&self) -> (Vec<f64>, Vec<f64>) {
        self.alignment_snap_targets(&HashSet::new(), false)
    }

    /// Keep the tool frame visible without creating an uncommitted edit.
    pub fn visible_crop_rect(&self) -> Option<Rect> {
        let document = self.document.as_ref()?;
        if self.tool != compositor_rs_core::document::NavigationTool::Crop {
            return None;
        }
        Some(
            self.crop_rect.unwrap_or_else(|| {
                Rect::new(0.0, 0.0, document.size().width, document.size().height)
            }),
        )
    }

    /// The ratio the chosen preset asks for, nil for Free.
    pub fn crop_ratio(&self) -> Option<f64> {
        match self.crop_ratio_choice.as_str() {
            "Original" => self
                .document
                .as_ref()
                .map(|document| document.width as f64 / document.height as f64),
            "1:1" => Some(1.0),
            "4:3" => Some(4.0 / 3.0),
            "3:4" => Some(3.0 / 4.0),
            "16:9" => Some(16.0 / 9.0),
            "9:16" => Some(9.0 / 16.0),
            _ => None,
        }
    }

    pub fn cancel_crop(&mut self) {
        self.crop_rect = None;
    }

    /// The Crop tool's own part of `selectTool` (`value == .crop` branch): a fresh frame over the
    /// canvas — or, with a selection, over its bounds, as Photoshop's does: C, then Return, crops to
    /// it.
    pub fn start_crop_tool(&mut self) {
        if self.crop_rect.is_some() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let canvas = Rect::new(0.0, 0.0, document.size().width, document.size().height);
        let selection_bounds = document
            .selection
            .as_ref()
            .filter(|selection| !selection.is_empty())
            .map(|selection| {
                selection
                    .path
                    .bounding_box()
                    .integral()
                    .intersection(canvas)
            });
        self.crop_ratio_choice = "Free".to_string();
        self.crop_rect = Some(match selection_bounds {
            Some(bounds) if CropGeometry::valid(bounds) => bounds,
            _ => canvas,
        });
    }

    pub fn change_crop_ratio(&mut self) {
        let Some(rect) = self.visible_crop_rect() else {
            return;
        };
        let Some(ratio) = self.crop_ratio() else {
            return;
        };
        let height = rect.width() / ratio;
        let next = CropGeometry::snapped(Rect::new(
            rect.min_x(),
            rect.mid_y() - height / 2.0,
            rect.width(),
            height,
        ));
        if CropGeometry::valid(next) {
            self.crop_rect = Some(next);
        }
    }

    /// Commits the crop.
    ///
    /// Swift: `func commitCrop() async` — `canStartProjectOperation` gate, `isProjectBusy` for the
    /// resizer actor, `CanvasResizer.resize`, then `applyDocumentSize(result, actionName: "Crop")`.
    /// The port runs the resize inline (a caller that wants it off the UI thread can run
    /// [`CanvasResizer::resize`] on a `rayon` job and hand the result to
    /// [`EditorSession::apply_document_size`]) so `is_project_busy` is only ever observed set
    /// within this call; `crop_error` keeps the failure text the sheet shows.
    pub fn commit_crop(&mut self) {
        if !self.can_start_project_operation() {
            return;
        }
        let Some(rect) = self.crop_rect else {
            return;
        };
        if !CropGeometry::valid(rect) {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        self.is_project_busy = true;
        let result = CanvasResizer::resize(&document, &crop_options(rect));
        self.is_project_busy = false;
        match result {
            Ok(result) => {
                self.crop_rect = None;
                self.apply_document_size(result, "Crop");
            }
            Err(error) => self.crop_error = Some(error.to_string()),
        }
    }
}

/// The document a crop commits to, as the resizer's plain-input form: the crop is the one caller
/// that hands [`CanvasSizeOptions`] an explicit content offset.
///
/// [`CanvasSizeOptions`]: compositor_rs_core::canvas_size::CanvasSizeOptions
pub fn crop_options(rect: Rect) -> compositor_rs_core::canvas_size::CanvasSizeOptions {
    let mut options = compositor_rs_core::canvas_size::CanvasSizeOptions::new(
        rect.width() as usize,
        rect.height() as usize,
    );
    options.content_offset = Some(Point::new(-rect.min_x(), -rect.min_y()));
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 0.000001, "{a} != {b}");
    }

    #[test]
    fn snapped_rounds_extremes_out_and_keeps_a_pixel() {
        let rect = CropGeometry::snapped(Rect::new(10.4, 20.6, 5.2, 0.2));
        assert_eq!(rect, Rect::new(10.0, 21.0, 6.0, 1.0));
        let inverted = CropGeometry::snapped(Rect::new(30.2, 40.9, -20.4, -10.8));
        assert_eq!(inverted, Rect::new(10.0, 30.0, 20.0, 11.0));
    }

    #[test]
    fn create_without_a_ratio_draws_the_dragged_box() {
        let rect = CropGeometry::create(Point::new(10.6, 20.2), Point::new(5.1, 30.8), None, false);
        assert_eq!(rect, Rect::new(5.0, 20.0, 6.0, 11.0));
    }

    #[test]
    fn create_with_a_ratio_grows_the_shorter_axis() {
        // 4:3 — a wide drag keeps dy as the y it was, the y axis grows to dx / (4/3).
        let rect = CropGeometry::create(
            Point::new(0.0, 0.0),
            Point::new(80.0, 10.0),
            Some(4.0 / 3.0),
            false,
        );
        close(rect.width(), 80.0);
        close(rect.height(), 60.0);
        // 3:4 — the tall choice mirrors it.
        let rect = CropGeometry::create(
            Point::new(0.0, 0.0),
            Point::new(30.0, 80.0),
            Some(3.0 / 4.0),
            false,
        );
        close(rect.width(), 60.0);
        close(rect.height(), 80.0);
        // 9:16 — portrait.
        let rect = CropGeometry::create(
            Point::new(0.0, 0.0),
            Point::new(90.0, 160.0),
            Some(9.0 / 16.0),
            false,
        );
        close(rect.width(), 90.0);
        close(rect.height(), 160.0);
        // 9:16 from a wide drag grows the height.
        let rect = CropGeometry::create(
            Point::new(0.0, 0.0),
            Point::new(90.0, 10.0),
            Some(9.0 / 16.0),
            false,
        );
        close(rect.width(), 90.0);
        close(rect.height(), 160.0);
        // Dragged up-left, the ratio holds and the box stays where it was dragged.
        let rect = CropGeometry::create(
            Point::new(100.0, 100.0),
            Point::new(40.0, 95.0),
            Some(4.0 / 3.0),
            false,
        );
        close(rect.width(), 60.0);
        close(rect.height(), 45.0);
        assert_eq!(rect.min_x(), 40.0);
        assert_eq!(rect.max_y(), 100.0);
    }

    #[test]
    fn create_symmetric_grows_out_from_the_start_as_its_center() {
        let rect = CropGeometry::create(
            Point::new(100.5, 100.5),
            Point::new(120.2, 130.1),
            None,
            true,
        );
        // Crop.swift:5-9 rounds each edge of the symmetric frame (Crop.swift:22-23): the snapped
        // rect keeps its centre exactly on the start point after rounding.
        assert_eq!(rect, Rect::new(81.0, 71.0, 39.0, 59.0));
        close(rect.mid_x(), 100.5);
        close(rect.mid_y(), 100.5);
    }

    #[test]
    fn valid_holds_the_side_limit_and_finite_numbers() {
        assert!(CropGeometry::valid(Rect::new(0.0, 0.0, 1.0, 1.0)));
        assert!(!CropGeometry::valid(Rect::new(0.0, 0.0, 0.5, 1.0)));
        assert!(!CropGeometry::valid(Rect::new(
            0.0,
            0.0,
            MAX_SIDE_EXTENT + 1.0,
            1.0
        )));
        assert!(!CropGeometry::valid(Rect::new(0.0, 0.0, f64::NAN, 1.0)));
        assert!(!CropGeometry::valid(Rect::new(1_000_001.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn drag_move_keeps_the_size_and_rounds() {
        let drag = CropDrag {
            start: Point::new(10.0, 10.0),
            original: Rect::new(0.0, 0.0, 50.0, 40.0),
            mode: CropDragMode::Move,
        };
        let rect = drag.updated(Point::new(18.4, 5.4), None, false);
        assert_eq!(rect, Rect::new(8.0, -5.0, 50.0, 40.0));
    }

    #[test]
    fn drag_resize_moves_the_dragged_corner_only() {
        let drag = CropDrag {
            start: Point::new(50.0, 40.0),
            original: Rect::new(0.0, 0.0, 50.0, 40.0),
            mode: CropDragMode::Resize(4),
        };
        let rect = drag.updated(Point::new(80.4, 60.6), None, false);
        assert_eq!(rect, Rect::new(0.0, 0.0, 80.0, 61.0));
    }

    fn target_snap() -> CropSnap {
        CropSnap::new(vec![0.0, 100.0], vec![0.0, 200.0], 5.0)
    }

    #[test]
    fn moving_snaps_the_closest_edge_and_keeps_the_size() {
        let drag = CropDrag {
            start: Point::new(0.0, 0.0),
            original: Rect::new(97.0, 50.0, 20.0, 20.0),
            mode: CropDragMode::Move,
        };
        let rect = target_snap().apply(drag.original, &drag, Point::new(-3.0, 0.0), None, false);
        assert_eq!(rect, Rect::new(100.0, 50.0, 20.0, 20.0));
    }

    #[test]
    fn creating_does_not_snap_with_a_fixed_ratio() {
        let rect = Rect::new(96.0, 0.0, 40.0, 40.0);
        let drag = CropDrag {
            start: Point::new(0.0, 0.0),
            original: rect,
            mode: CropDragMode::Create,
        };
        assert_eq!(
            target_snap().apply(rect, &drag, Point::new(136.0, 40.0), Some(1.0), false),
            rect
        );
        // Without a ratio the dragged corner snaps while the anchor stays put
        // (CropTests.swift:89,109-113): right edge 147 -> 150, bottom edge 77 -> 80.
        let snap = CropSnap::new(
            vec![0.0, 200.0, 50.0, 150.0],
            vec![0.0, 100.0, 20.0, 80.0],
            6.0,
        );
        let create = CropDrag {
            start: Point::new(52.0, 18.0),
            original: Rect::new(0.0, 0.0, 0.0, 0.0),
            mode: CropDragMode::Create,
        };
        let dragged = Point::new(147.0, 77.0);
        let snapped = snap.apply(create.updated(dragged, None, false), &create, dragged, None, false);
        assert_eq!(snapped, Rect::new(52.0, 18.0, 98.0, 62.0));
    }

    #[test]
    fn resizing_snaps_only_the_dragged_side() {
        let drag = CropDrag {
            start: Point::new(0.0, 0.0),
            original: Rect::new(0.0, 0.0, 40.0, 40.0),
            mode: CropDragMode::Resize(6),
        };
        // The top-left corner is dragged near x = 0: its x snaps, the right edge does not stay.
        let rect = target_snap().apply(
            Rect::new(3.0, 3.0, 43.0, 43.0),
            &drag,
            Point::new(3.0, 3.0),
            None,
            false,
        );
        assert_eq!(rect, Rect::new(0.0, 0.0, 46.0, 46.0));
        // An edge handle only moves its own axis.
        let edge = CropDrag {
            start: Point::new(0.0, 0.0),
            original: Rect::new(0.0, 0.0, 40.0, 40.0),
            mode: CropDragMode::Resize(1),
        };
        let rect = target_snap().apply(
            Rect::new(0.0, 2.0, 40.0, 42.0),
            &edge,
            Point::new(0.0, 2.0),
            None,
            false,
        );
        // Handle 1 is the top edge (0.5, 0), so only the y axis snaps (Crop.swift:83):
        // minY 2 -> 0 while maxY stays at 44 (Crop.swift:92-94), growing the height 42 -> 44.
        assert_eq!(rect, Rect::new(0.0, 0.0, 40.0, 44.0));
    }

    #[test]
    fn symmetric_snap_mirrors_the_snapped_edge_about_the_center() {
        let drag = CropDrag {
            start: Point::new(50.0, 100.0),
            original: Rect::new(50.0, 100.0, 0.0, 0.0),
            mode: CropDragMode::Create,
        };
        let rect = target_snap().apply(
            Rect::new(50.0, 100.0, 48.0, 8.0),
            &drag,
            Point::new(98.0, 108.0),
            None,
            true,
        );
        // The right edge lands on 100, so the frame spans 0…100 about the 50 start.
        close(rect.min_x(), 0.0);
        close(rect.max_x(), 100.0);
        close(rect.mid_x(), 50.0);
    }

    #[test]
    fn overlay_geometry_maps_the_frame_into_view_points() {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(800.0, 600.0), 1.0, None);
        viewport.set_zoom(2.0, Point::ZERO, Size::new(400.0, 300.0));
        let document_size = Size::new(400.0, 300.0);
        let rect = Rect::new(10.0, 20.0, 100.0, 50.0);
        let view = CropOverlay::view_rect(&viewport, rect, document_size);
        let origin = viewport.view_point(rect.origin, document_size);
        assert_eq!(view.origin, origin);
        close(view.width(), 200.0);
        close(view.height(), 100.0);
        let handles = CropOverlay::handles(view);
        assert_eq!(handles[0], view.origin);
        close(handles[4].x, view.max_x());
        close(handles[4].y, view.max_y());
        let regions = CropOverlay::resize_regions(view);
        assert_eq!(regions.len(), 8);
        assert_eq!(regions[0].0, 0);
        // Corners come first: a corner region wins where it overlaps an edge strip.
        assert_eq!(regions[4].0, 1);
        assert_eq!(regions[6].0, 3);
        // The top edge strip runs between the corner regions.
        close(regions[4].1.min_x(), view.min_x() + 10.0);
        close(regions[4].1.width(), view.width() - 20.0);
    }

    fn session_with_document(width: usize, height: usize) -> EditorSession {
        let mut session = EditorSession::default();
        let mut document = compositor_rs_core::document::CanvasDocument::new(width, height);
        let raster = std::sync::Arc::new(compositor_rs_core::buffer::Rgba8Image::opaque(
            width,
            height,
            [1, 2, 3, 255],
        ));
        let asset = compositor_rs_core::imported_image::ImportedImage::new(
            compositor_rs_core::imported_image::PixelImage::Rgba(raster.clone()),
            compositor_rs_core::imported_image::PixelImage::Rgba(raster),
            "Layer",
        );
        let layer = compositor_rs_core::document::ImageLayer::from_asset(asset, Point::ZERO);
        document.layers = vec![layer];
        session.document = Some(document);
        session
    }

    #[test]
    fn crop_ratios_are_the_picker_choices() {
        let mut session = session_with_document(1600, 900);
        session.crop_ratio_choice = "Free".to_string();
        assert_eq!(session.crop_ratio(), None);
        session.crop_ratio_choice = "Original".to_string();
        close(session.crop_ratio().unwrap(), 1600.0 / 900.0);
        session.crop_ratio_choice = "1:1".to_string();
        assert_eq!(session.crop_ratio(), Some(1.0));
        session.crop_ratio_choice = "4:3".to_string();
        close(session.crop_ratio().unwrap(), 4.0 / 3.0);
        session.crop_ratio_choice = "3:4".to_string();
        close(session.crop_ratio().unwrap(), 3.0 / 4.0);
        session.crop_ratio_choice = "16:9".to_string();
        close(session.crop_ratio().unwrap(), 16.0 / 9.0);
        session.crop_ratio_choice = "9:16".to_string();
        close(session.crop_ratio().unwrap(), 9.0 / 16.0);
    }

    #[test]
    fn changing_the_ratio_keeps_the_width_and_recenters_the_height() {
        let mut session = session_with_document(1600, 900);
        // visibleCropRect is nil until the Crop tool is active (Crop.swift:214-216).
        session.tool = compositor_rs_core::document::NavigationTool::Crop;
        session.crop_rect = Some(Rect::new(100.0, 200.0, 400.0, 300.0));
        session.crop_ratio_choice = "3:4".to_string();
        session.change_crop_ratio();
        let rect = session.crop_rect.expect("changed");
        close(rect.width(), 400.0);
        // changeCropRatio snaps to whole pixels (Crop.swift:233): 533⅓ rounds to 534.
        close(rect.height(), 534.0);
        close(rect.mid_y(), 350.0);
        assert_eq!(rect.min_x(), 100.0);
    }

    #[test]
    fn starting_the_crop_tool_prefers_the_selection_bounds() {
        use compositor_rs_core::path::Path;
        let mut session = session_with_document(100, 80);
        session.tool = compositor_rs_core::document::NavigationTool::Move;
        session.document.as_mut().unwrap().selection =
            Some(compositor_rs_core::selection::DocumentSelection::new(
                Path::rect(Rect::new(10.25, 20.5, 30.0, 40.0)),
            ));
        session.start_crop_tool();
        // The bounds are made whole-pixel, intersected with the canvas.
        assert_eq!(session.crop_rect, Some(Rect::new(10.0, 20.0, 31.0, 41.0)));
        assert_eq!(session.crop_ratio_choice, "Free");
        // Without a selection the whole canvas is the frame.
        session.crop_rect = None;
        session.document.as_mut().unwrap().selection = None;
        session.start_crop_tool();
        assert_eq!(session.crop_rect, Some(Rect::new(0.0, 0.0, 100.0, 80.0)));
    }

    #[test]
    fn committing_the_crop_is_one_undo_step_named_crop() {
        let mut session = session_with_document(100, 80);
        session.crop_rect = Some(Rect::new(10.0, 5.0, 40.0, 30.0));
        session.commit_crop();
        assert!(!session.is_project_busy);
        assert_eq!(session.crop_rect, None);
        assert_eq!(session.crop_error, None);
        let document = session.document.as_ref().expect("document");
        assert_eq!((document.width, document.height), (40, 30));
        // The layer moved by the crop's content offset.
        assert_eq!(document.layers[0].transform.origin, Point::new(-10.0, -5.0));
        assert_eq!(session.history.undo_name(), "Crop");
        assert!(session.history.can_undo());
    }

    #[test]
    fn cancelling_the_crop_keeps_the_document() {
        let mut session = session_with_document(100, 80);
        session.crop_rect = Some(Rect::new(10.0, 5.0, 40.0, 30.0));
        session.cancel_crop();
        assert_eq!(session.crop_rect, None);
        assert_eq!(session.document.as_ref().unwrap().width, 100);
        assert!(!session.history.can_undo());
    }
}
