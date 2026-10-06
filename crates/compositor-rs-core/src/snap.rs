//! Snapping: the alignment targets a move, resize, crop or selection snaps to, the tie-breaking
//! rule, the alignment lines to draw, and a slider's value snapping.
//!
//! Ported from `TransformSnap` (already in [`crate::layer_transform`], re-exported here), the
//! snapping half of `Document/Guides.swift`, `Document/Crop.swift`'s `CropSnap` and the
//! `extension EditorSession` that drives the move/crop snaps, and `UI/SliderSnap.swift`.
//!
//! Everything here is pure: the session hands in the candidate list it built from the document and
//! gets an offset and the lines to draw back.

use crate::geom::{Point, Rect, Size};
use crate::guides::{CanvasGuide, CanvasGuideAxis, LayoutGrid};
use crate::layer_transform::{LayerTransform, TransformDrag, TransformDragMode};

pub use crate::layer_transform::{SnapOffset, TransformSnap};

/// How close, in screen points, a guide comes before it snaps (`TransformSnap.distance`).
pub const SNAP_DISTANCE: f64 = TransformSnap::DISTANCE;

/// How close, in screen points, a pointer must come to a guide to hit it
/// (`EditorSession.guideHitDistance`).
pub const GUIDE_HIT_DISTANCE: f64 = 5.0;

/// The document-pixel tolerance a snap uses: a fixed screen-point distance converted at the
/// viewport's zoom. A zoom of zero is floored at `0.0001` so the tolerance stays finite, as the
/// Swift does.
pub fn document_tolerance(points_per_pixel: f64) -> f64 {
    SNAP_DISTANCE / points_per_pixel.max(0.0001)
}

/// The document X and Y positions a snap may land on. The order is the priority order: the first
/// target at the smallest distance wins a tie, so the targets are built in the same order the Swift
/// `alignmentSnapTargets` appends them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapTargets {
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
}

impl SnapTargets {
    pub fn new() -> Self {
        SnapTargets::default()
    }

    pub fn is_empty(&self) -> bool {
        self.xs.is_empty() && self.ys.is_empty()
    }

    /// The canvas' edges, and optionally its centers (`snapToDocumentBounds`).
    pub fn add_document_bounds(&mut self, document_size: Size, include_centers: bool) {
        self.xs.extend([0.0, document_size.width]);
        self.ys.extend([0.0, document_size.height]);
        if include_centers {
            self.xs.push(document_size.width / 2.0);
            self.ys.push(document_size.height / 2.0);
        }
    }

    /// One other layer's bounding box: its edges, and optionally its center (`snapToLayers`).
    /// Corners are the four distorted corners in handle order; the box is rounded to whole pixels.
    pub fn add_layer_box(&mut self, corners: &[Point; 4], include_centers: bool) {
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
        // A non-finite corner has no usable box to snap to, so the layer is skipped.
        if !(min_x.is_finite() && max_x.is_finite() && min_y.is_finite() && max_y.is_finite()) {
            return;
        }
        if include_centers {
            self.xs
                .extend([min_x.round(), ((min_x + max_x) / 2.0).round(), max_x.round()]);
            self.ys
                .extend([min_y.round(), ((min_y + max_y) / 2.0).round(), max_y.round()]);
        } else {
            self.xs.extend([min_x.round(), max_x.round()]);
            self.ys.extend([min_y.round(), max_y.round()]);
        }
    }

    /// Every grid line along both document edges (`snapToGrid` and the grid shown).
    pub fn add_grid(&mut self, grid: &LayoutGrid, document_size: Size) {
        self.xs.extend(grid.lines(document_size.width));
        self.ys.extend(grid.lines(document_size.height));
    }

    /// The user's vertical guides onto `xs` and horizontal ones onto `ys` (`snapToGuides` and the
    /// guides shown).
    pub fn add_guides(&mut self, guides: &[CanvasGuide]) {
        for guide in guides {
            if guide.axis == CanvasGuideAxis::Vertical {
                self.xs.push(guide.position);
            } else {
                self.ys.push(guide.position);
            }
        }
    }

    /// The nearest target to `value` within `tolerance`, per axis, and the line to draw.
    pub fn nearest_per_axis(&self, value: Point, tolerance: f64) -> (Point, SnapIndicators) {
        let x = nearest_target(value.x, &self.xs, tolerance);
        let y = nearest_target(value.y, &self.ys, tolerance);
        (
            Point::new(x.unwrap_or(value.x), y.unwrap_or(value.y)),
            SnapIndicators {
                xs: x.into_iter().collect(),
                ys: y.into_iter().collect(),
            },
        )
    }
}

/// The inputs `alignmentSnapTargets` reads off the session, handed in so building the targets stays
/// pure. `layer_corners` is already in render order and already excludes the layers being moved.
pub struct AlignmentSnapInput<'a> {
    pub snap_enabled: bool,
    pub snap_to_document_bounds: bool,
    pub snap_to_layers: bool,
    pub snap_to_grid: bool,
    pub snap_to_guides: bool,
    pub shows_grid: bool,
    pub shows_guides: bool,
    pub document_size: Size,
    /// The four (possibly distorted) corners of each layer that may be snapped to.
    pub layer_corners: &'a [[Point; 4]],
    pub grid: &'a LayoutGrid,
    pub guides: &'a [CanvasGuide],
}

/// Alignment lines a move or crop may snap to, according to View > Snap and Snap To. Document
/// bounds come first, then the other layers in render order, then the grid, then the guides —
/// which is also the order a tie between equally distant targets is broken in.
pub fn alignment_targets(input: &AlignmentSnapInput, include_centers: bool) -> SnapTargets {
    let mut targets = SnapTargets::new();
    if !input.snap_enabled {
        return targets;
    }
    if input.snap_to_document_bounds {
        targets.add_document_bounds(input.document_size, include_centers);
    }
    if input.snap_to_layers {
        for corners in input.layer_corners {
            targets.add_layer_box(corners, include_centers);
        }
    }
    // Hidden extras do not snap, matching Photoshop.
    if input.snap_to_grid && input.shows_grid {
        targets.add_grid(input.grid, input.document_size);
    }
    if input.snap_to_guides && input.shows_guides {
        targets.add_guides(input.guides);
    }
    targets
}

/// What a snap found: the move to apply and the lines to draw (`EditorSession.snapGuides`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapIndicators {
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
}

impl SnapIndicators {
    pub fn is_empty(&self) -> bool {
        self.xs.is_empty() && self.ys.is_empty()
    }

    /// The lines for a `TransformSnap.offset` result: each axis draws one line when it landed.
    pub fn from_snap(snap: &SnapOffset) -> Self {
        SnapIndicators {
            xs: snap.x.into_iter().collect(),
            ys: snap.y.into_iter().collect(),
        }
    }
}

/// The target nearest `value` within `tolerance`. An earlier target wins a tie, as every one of the
/// Swift `nearest`/`min` helpers does (they keep the current best unless the next is strictly
/// closer).
pub fn nearest_target(value: f64, targets: &[f64], tolerance: f64) -> Option<f64> {
    let mut best: Option<f64> = None;
    for &target in targets {
        // `guard abs(move) <= tolerance`: written positive, so a NaN target counts as out of reach.
        if !((target - value).abs() <= tolerance) {
            continue;
        }
        if let Some(current) = best {
            if (current - value).abs() <= (target - value).abs() {
                continue;
            }
        }
        best = Some(target);
    }
    best
}

/// `box` moved so that whichever of its left, center or right lands nearest an `xs` target does, and
/// the same vertically — each axis on its own, and only within `tolerance` document pixels. The
/// targets it landed on come back too, to draw a line along (`TransformSnap.offset` plus the
/// session's `snapGuides`).
pub fn snap_box(box_: Rect, targets: &SnapTargets, tolerance: f64) -> (Size, SnapIndicators) {
    let snap = TransformSnap::offset(box_, &targets.xs, &targets.ys, tolerance);
    (snap.offset, SnapIndicators::from_snap(&snap))
}

/// `point` moved onto the nearest crop target within `tolerance` document pixels, each axis on its
/// own (`snappedPoint`): where a Marquee or a shape starts and where its corner is dragged to.
pub fn snap_point(point: Point, targets: &SnapTargets, tolerance: f64) -> (Point, SnapIndicators) {
    targets.nearest_per_axis(point, tolerance)
}

/// A selection being moved by `offset` from where it started, nudged so its edges or middle meet a
/// nearby target within `tolerance` document pixels, each axis on its own. `bounding_box` is the
/// selection's box where it started; an axis Shift has locked is passed as false and doesn't snap
/// (`snappedSelectionOffset`).
pub fn snap_selection_offset(
    offset: Size,
    bounding_box: Rect,
    targets: &SnapTargets,
    tolerance: f64,
    horizontal: bool,
    vertical: bool,
) -> (Size, SnapIndicators) {
    let rounded = Size::new(offset.width.round(), offset.height.round());
    let box_ = bounding_box.offset_by(rounded.width, rounded.height);
    let snap = TransformSnap::offset(
        box_,
        if horizontal { &targets.xs } else { &[] },
        if vertical { &targets.ys } else { &[] },
        tolerance,
    );
    (
        Size::new(rounded.width + snap.offset.width, rounded.height + snap.offset.height),
        SnapIndicators::from_snap(&snap),
    )
}

/// A guide position pulled onto the nearest nearby target within `tolerance` document pixels
/// (`snappedGuidePosition`). Guide snapping builds its targets in its own order — grid, other
/// guides on the same axis, the document's ends and center, then the layers — so a tie breaks
/// differently from [`alignment_targets`].
pub fn snapped_guide_position(
    position: f64,
    axis: CanvasGuideAxis,
    targets: &SnapTargets,
    tolerance: f64,
) -> f64 {
    let lines = if axis == CanvasGuideAxis::Vertical {
        &targets.xs
    } else {
        &targets.ys
    };
    nearest_target(position, lines, tolerance).unwrap_or(position)
}

/// The transform's corners in handle order: top-left, top-right, bottom-right, bottom-left
/// (`DistortWarp.corners(of:)`).
pub fn transform_corners(transform: &LayerTransform) -> [Point; 4] {
    [
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
        Point::new(0.0, 1.0),
    ]
    .map(|unit| transform.point(unit))
}

/// `draft` nudged so the layer it places lines up with a nearby target (`snappedMove`). The box is
/// the drawn shape's bounding box (a rotated layer's box, not its upright origin/size), and the
/// move is applied to the draft's origin.
pub fn snapped_move(
    draft: LayerTransform,
    targets: &SnapTargets,
    tolerance: f64,
) -> (LayerTransform, SnapIndicators) {
    let corners = transform_corners(&draft);
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
        return (draft, SnapIndicators::default());
    }
    let box_ = Rect::new(min_x, min_y, max_x - min_x, max_y - min_y);
    let snap = TransformSnap::offset(box_, &targets.xs, &targets.ys, tolerance);
    let mut snapped = draft;
    snapped.origin.x += snap.offset.width;
    snapped.origin.y += snap.offset.height;
    (snapped, SnapIndicators::from_snap(&snap))
}

/// A resize handle dragged to `point`: the pointer nudged so the edges the handle moves land on a
/// nearby target, within `tolerance` document pixels, as a moved layer's do (`snappedResizePoint`).
/// `update` is the drag's own result for a pointer. Each edge snaps on its own; kept `proportional`,
/// only the nearer one does and the other follows the ratio. An upright layer only: a turned one's
/// edges don't run along the targets, so it returns `point` untouched.
pub fn snapped_resize_point(
    point: Point,
    drag: &TransformDrag,
    proportional: bool,
    targets: &SnapTargets,
    tolerance: f64,
    update: impl Fn(Point) -> LayerTransform,
) -> (Point, SnapIndicators) {
    let TransformDragMode::Resize(index) = drag.mode else {
        return (point, SnapIndicators::default());
    };
    if drag.original.radians() != 0.0 {
        return (point, SnapIndicators::default());
    }
    let handle = LayerTransform::HANDLES[index];
    let grab = drag.original.point(handle);
    // Where the dragged handle is, to tell its edge from the one across from it.
    let at = Point::new(
        grab.x + point.x - drag.start.x,
        grab.y + point.y - drag.start.y,
    );
    let edge = |transform: LayerTransform, horizontal: bool| -> f64 {
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
    let draft = update(point);
    let mut snaps: Vec<(bool, f64)> = Vec::new();
    if handle.x != 0.5 {
        if let Some(x) = nearest_target(edge(draft, true), &targets.xs, tolerance) {
            snaps.push((true, x));
        }
    }
    if handle.y != 0.5 {
        if let Some(y) = nearest_target(edge(draft, false), &targets.ys, tolerance) {
            snaps.push((false, y));
        }
    }
    if proportional && snaps.len() == 2 {
        // Keep the edge whose target is strictly closer; the first wins a tie.
        let mut best = snaps[0];
        for &candidate in &snaps[1..] {
            let best_distance = (best.1 - edge(draft, best.0)).abs();
            let candidate_distance = (candidate.1 - edge(draft, candidate.0)).abs();
            if candidate_distance < best_distance {
                best = candidate;
            }
        }
        snaps = vec![best];
    }
    // An edge follows the pointer in a straight line along each axis, so one step measured across a
    // pixel lands it.
    let mut result = point;
    for &(horizontal, target) in &snaps {
        let before = edge(update(result), horizontal);
        let mut nudged = result;
        if horizontal {
            nudged.x += 1.0;
        } else {
            nudged.y += 1.0;
        }
        let per_pixel = edge(update(nudged), horizontal) - before;
        // `guard abs(perPixel) > 0.01 else { continue }`: a NaN step takes the guard's exit as well.
        if !(per_pixel.abs() > 0.01) {
            continue;
        }
        let shift = (target - before) / per_pixel;
        if horizontal {
            result.x += shift;
        } else {
            result.y += shift;
        }
    }
    let indicators = SnapIndicators {
        xs: snaps.iter().filter(|(h, _)| *h).map(|(_, t)| *t).collect(),
        ys: snaps.iter().filter(|(h, _)| !*h).map(|(_, t)| *t).collect(),
    };
    (result, indicators)
}

/// A linear slider's track and knob and the mapping of a press on the track to a value — the
/// arithmetic `NSSliderCell.snapValue` ran before native tracking began (port of `UI/SliderSnap.swift`).
///
/// `track` is the cell's `trackRect` (the caller already substituted the view's bounds when the
/// track was empty) and `knob` is `knobRect(flipped:)`; only the knob's width and whether the press
/// falls inside it matter here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderSnap {
    pub track: Rect,
    pub knob: Rect,
    pub min_value: f64,
    pub max_value: f64,
    pub right_to_left: bool,
    /// `allowsTickMarkValuesOnly` with a positive `numberOfTickMarks`: the value rounds to the
    /// nearest evenly spaced mark.
    pub tick_marks: Option<usize>,
}

impl SliderSnap {
    pub fn new(track: Rect, knob: Rect, min_value: f64, max_value: f64, right_to_left: bool) -> Self {
        SliderSnap {
            track,
            knob,
            min_value,
            max_value,
            right_to_left,
            tick_marks: None,
        }
    }

    /// Makes the slider take only tick-mark values, like `allowsTickMarkValuesOnly` with
    /// `number_of_tick_marks` marks spread evenly from one end of the range to the other.
    pub fn tick_marks(mut self, number_of_tick_marks: usize) -> Self {
        self.tick_marks = (number_of_tick_marks > 0).then_some(number_of_tick_marks);
        self
    }

    /// Where a press sits along the knob's travel, `0` at the left end and `1` at the right one:
    /// `(point.x - track.minX - knob.width / 2) / travel`, clamped. `None` when the knob has no room
    /// to travel, which the Swift hook treated as "leave the value alone".
    pub fn fraction(&self, press: Point) -> Option<f64> {
        let travel = self.track.width() - self.knob.width();
        if !(travel > 0.0) {
            return None;
        }
        // `min(1, max(0, …))` as the Swift wrote it: no panic on a NaN press.
        let raw = (press.x - self.track.min_x() - self.knob.width() / 2.0) / travel;
        let fraction = if raw < 0.0 {
            0.0
        } else if raw > 1.0 {
            1.0
        } else {
            raw
        };
        Some(if self.right_to_left { 1.0 - fraction } else { fraction })
    }

    /// The value whose knob is centered on a press, or `None` when the press should leave the value
    /// alone: the range is empty, the knob has no room to travel, or the press lands on the knob
    /// (which drags it from where it is instead).
    pub fn value(&self, press: Point) -> Option<f64> {
        if !(self.max_value > self.min_value) || self.knob.contains(press) {
            return None;
        }
        let fraction = self.fraction(press)?;
        let value = self.min_value + fraction * (self.max_value - self.min_value);
        Some(match self.tick_marks {
            Some(count) => {
                closest_tick_mark_value(value, self.min_value, self.max_value, count)
            }
            None => value,
        })
    }
}

/// `NSSliderCell.closestTickMarkValue(toValue:)`: the evenly spaced tick mark nearest `value`. The
/// range's ends are marks, so `count` marks sit `(max - min) / (count - 1)` apart.
pub fn closest_tick_mark_value(value: f64, min: f64, max: f64, count: usize) -> f64 {
    if count < 2 || max <= min {
        return clamped(value, min, max);
    }
    let spacing = (max - min) / (count - 1) as f64;
    let last = (count - 1) as f64;
    let raw = ((value - min) / spacing).round();
    let index = if raw < 0.0 {
        0.0
    } else if raw > last {
        last
    } else {
        raw
    };
    min + index * spacing
}

/// `(value / step).rounded() * step`: the grid a dragged numeric value snaps to, whole numbers for
/// a step of 1 (`NumericScrub`: typing can still give any value).
pub fn snapped_to_step(value: f64, step: f64) -> f64 {
    (value / step).round() * step
}

/// `min(upper, max(lower, value))`: the clamp the numeric fields and scrubs apply to a dragged
/// value. Written as comparisons, so it never panics the way `f64::clamp` does on a NaN or a
/// reversed range.
pub fn clamped(value: f64, lower: f64, upper: f64) -> f64 {
    if value < lower {
        lower
    } else if value > upper {
        upper
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(spacing: usize, subdivisions: usize) -> LayoutGrid {
        LayoutGrid::new(spacing, subdivisions)
    }

    fn transform(origin: Point, size: Size) -> LayerTransform {
        LayerTransform {
            origin,
            size,
            ..LayerTransform::default()
        }
    }

    #[test]
    fn the_tolerance_is_a_fixed_screen_distance_at_the_zoom() {
        assert_eq!(document_tolerance(1.0), 10.0);
        assert_eq!(document_tolerance(2.0), 5.0);
        assert_eq!(document_tolerance(0.0), 100_000.0, "a zero zoom is floored");
        assert_eq!(SNAP_DISTANCE, 10.0);
    }

    #[test]
    fn alignment_targets_keep_the_swift_order() {
        let guides = [
            CanvasGuide::at(CanvasGuideAxis::Vertical, 7.0),
            CanvasGuide::at(CanvasGuideAxis::Horizontal, 8.0),
        ];
        let corners = [Point::new(10.0, 20.0), Point::new(30.0, 20.0), Point::new(30.0, 40.0), Point::new(10.0, 40.0)];
        let input = AlignmentSnapInput {
            snap_enabled: true,
            snap_to_document_bounds: true,
            snap_to_layers: true,
            snap_to_grid: true,
            snap_to_guides: true,
            shows_grid: true,
            shows_guides: true,
            document_size: Size::new(100.0, 50.0),
            layer_corners: &[corners],
            grid: &grid(20, 1),
            guides: &guides,
        };
        let targets = alignment_targets(&input, true);
        assert_eq!(
            targets.xs,
            vec![0.0, 100.0, 50.0, 10.0, 20.0, 30.0, 0.0, 20.0, 40.0, 60.0, 80.0, 100.0, 7.0],
            "bounds, then layers, then the grid, then guides"
        );
        assert_eq!(
            targets.ys,
            vec![0.0, 50.0, 25.0, 20.0, 30.0, 40.0, 0.0, 20.0, 40.0, 8.0]
        );
        // Without centers a layer contributes only its outer edges.
        let edges = alignment_targets(&input, false);
        assert_eq!(edges.xs, vec![0.0, 100.0, 10.0, 30.0, 0.0, 20.0, 40.0, 60.0, 80.0, 100.0, 7.0]);
        assert_eq!(edges.ys, vec![0.0, 50.0, 20.0, 40.0, 0.0, 20.0, 40.0, 8.0]);
    }

    #[test]
    fn hidden_or_disabled_targets_are_left_out() {
        let guides = [CanvasGuide::at(CanvasGuideAxis::Vertical, 7.0)];
        let input = AlignmentSnapInput {
            snap_enabled: false,
            snap_to_document_bounds: true,
            snap_to_layers: true,
            snap_to_grid: true,
            snap_to_guides: true,
            shows_grid: true,
            shows_guides: true,
            document_size: Size::new(100.0, 50.0),
            layer_corners: &[],
            grid: &grid(20, 1),
            guides: &guides,
        };
        assert!(alignment_targets(&input, true).is_empty(), "snapping off means no targets");
        let input = AlignmentSnapInput { snap_enabled: true, ..input };
        let shown = alignment_targets(&input, true);
        assert_eq!(shown.xs, vec![0.0, 100.0, 50.0, 0.0, 20.0, 40.0, 60.0, 80.0, 100.0, 7.0]);
        let input = AlignmentSnapInput { snap_to_grid: false, snap_to_guides: false, ..input };
        let bounds_only = alignment_targets(&input, true);
        assert_eq!(bounds_only.xs, vec![0.0, 100.0, 50.0]);
        assert_eq!(bounds_only.ys, vec![0.0, 50.0, 25.0]);
    }

    #[test]
    fn a_tie_goes_to_the_earlier_guide_and_target() {
        assert_eq!(nearest_target(5.0, &[4.0, 6.0], 10.0), Some(4.0), "the first of an equal pair wins");
        assert_eq!(nearest_target(5.0, &[9.0, 4.0], 2.0), Some(4.0), "9 is out of reach; 4 is one away");
        assert_eq!(nearest_target(5.0, &[39.0, 4.0, 5.0], 2.0), Some(5.0));

        // box 0…10, targets 4 and 6: the middle lands on 4 (an equal move, taken first).
        let targets = SnapTargets {
            xs: vec![4.0, 6.0],
            ys: vec![],
        };
        let (offset, indicators) = snap_box(Rect::new(0.0, 0.0, 10.0, 10.0), &targets, 10.0);
        assert_eq!(offset, Size::new(-1.0, 0.0));
        assert_eq!(indicators.xs, vec![4.0]);
        assert!(indicators.ys.is_empty());
    }

    #[test]
    fn a_box_snaps_only_within_tolerance() {
        let targets = SnapTargets {
            xs: vec![40.0],
            ys: vec![40.0],
        };
        let (offset, indicators) = snap_box(Rect::new(0.0, 0.0, 10.0, 10.0), &targets, 10.0);
        assert_eq!(offset, Size::ZERO);
        assert!(indicators.is_empty());
        let (offset, indicators) = snap_box(Rect::new(0.0, 0.0, 10.0, 10.0), &targets, 30.0);
        assert_eq!(offset, Size::new(30.0, 30.0), "an edge exactly at the tolerance is still in reach");
        assert_eq!(indicators.xs, vec![40.0]);
        assert_eq!(indicators.ys, vec![40.0]);
    }

    #[test]
    fn a_point_snaps_each_axis_on_its_own() {
        let targets = SnapTargets {
            xs: vec![0.0, 100.0],
            ys: vec![0.0, 50.0],
        };
        let (point, indicators) = snap_point(Point::new(3.0, 47.0), &targets, 5.0);
        assert_eq!(point, Point::new(0.0, 50.0));
        assert_eq!(indicators.xs, vec![0.0]);
        assert_eq!(indicators.ys, vec![50.0]);
    }

    #[test]
    fn a_guide_position_uses_only_its_own_axis() {
        let targets = SnapTargets {
            xs: vec![0.0, 50.0, 100.0],
            ys: vec![12.0],
        };
        assert_eq!(snapped_guide_position(46.0, CanvasGuideAxis::Vertical, &targets, 10.0), 50.0);
        assert_eq!(snapped_guide_position(46.0, CanvasGuideAxis::Vertical, &targets, 2.0), 46.0);
        assert_eq!(snapped_guide_position(14.0, CanvasGuideAxis::Horizontal, &targets, 10.0), 12.0);
    }

    #[test]
    fn a_selection_offset_rounds_first_then_snaps() {
        let targets = SnapTargets {
            xs: vec![11.0],
            ys: vec![-3.0],
        };
        let (offset, indicators) = snap_selection_offset(
            Size::new(2.4, -1.6),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            &targets,
            5.0,
            true,
            true,
        );
        // Rounded to (2, -2), the box is 2…12 / -2…8: the right edge meets 11 and the top edge -3.
        assert_eq!(offset, Size::new(1.0, -3.0));
        assert_eq!(indicators.xs, vec![11.0]);
        assert_eq!(indicators.ys, vec![-3.0]);
    }

    #[test]
    fn a_locked_axis_does_not_snap() {
        let targets = SnapTargets {
            xs: vec![11.0],
            ys: vec![-3.0],
        };
        let (offset, indicators) = snap_selection_offset(
            Size::new(0.0, 0.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            &targets,
            5.0,
            false,
            true,
        );
        assert_eq!(offset, Size::new(0.0, -3.0));
        assert!(indicators.xs.is_empty());
        assert_eq!(indicators.ys, vec![-3.0]);
    }

    #[test]
    fn moving_a_layer_nudges_its_origin() {
        let targets = SnapTargets {
            xs: vec![12.0],
            ys: vec![],
        };
        let draft = transform(Point::new(0.0, 0.0), Size::new(10.0, 10.0));
        let (snapped, indicators) = snapped_move(draft, &targets, 5.0);
        assert_eq!(snapped.origin, Point::new(2.0, 0.0));
        assert_eq!(snapped.size, Size::new(10.0, 10.0));
        assert_eq!(indicators.xs, vec![12.0]);
        assert!(indicators.ys.is_empty());
    }

    #[test]
    fn corners_come_back_in_handle_order() {
        let draft = transform(Point::new(0.0, 0.0), Size::new(10.0, 20.0));
        assert_eq!(
            transform_corners(&draft),
            [
                Point::new(0.0, 0.0),
                Point::new(10.0, 0.0),
                Point::new(10.0, 20.0),
                Point::new(0.0, 20.0),
            ]
        );
    }

    #[test]
    fn a_resize_handle_lands_its_edge_on_the_target() {
        // Dragging the bottom-right handle sets the size to the pointer: origin top-left.
        let drag = TransformDrag {
            original: transform(Point::new(0.0, 0.0), Size::new(10.0, 10.0)),
            start: Point::new(10.0, 10.0),
            mode: TransformDragMode::Resize(4),
            original_corners: None,
        };
        let update = |point: Point| transform(Point::new(0.0, 0.0), Size::new(point.x, point.y));
        let targets = SnapTargets {
            xs: vec![14.0],
            ys: vec![10.0],
        };
        let (point, indicators) = snapped_resize_point(Point::new(13.0, 10.0), &drag, false, &targets, 5.0, update);
        assert_eq!(point, Point::new(14.0, 10.0));
        assert_eq!(indicators.xs, vec![14.0]);
        assert_eq!(indicators.ys, vec![10.0]);

        // A rotated layer's edges don't run along the targets, so nothing snaps.
        let mut rotated = drag;
        rotated.original.rotation = 30.0;
        let (point, indicators) = snapped_resize_point(Point::new(13.0, 10.0), &rotated, false, &targets, 5.0, update);
        assert_eq!(point, Point::new(13.0, 10.0));
        assert!(indicators.is_empty());
    }

    #[test]
    fn a_slider_press_on_the_track_snaps_under_the_click() {
        // A 220-wide slider with a 20-point knob: the press at 0.9 of the width is 0.94 of the way.
        let snap = SliderSnap::new(
            Rect::new(0.0, 0.0, 220.0, 30.0),
            Rect::new(90.0, 0.0, 20.0, 30.0),
            0.0,
            1.0,
            false,
        );
        assert_eq!(snap.value(Point::new(198.0, 15.0)), Some(0.94));
        assert_eq!(snap.fraction(Point::new(198.0, 15.0)), Some(0.94));
        // The ends clamp to the knob's travel.
        assert_eq!(snap.value(Point::new(0.0, 15.0)), Some(0.0));
        assert_eq!(snap.value(Point::new(219.0, 15.0)), Some(1.0));
    }

    #[test]
    fn a_press_on_the_knob_or_a_dead_travel_leaves_the_value_alone() {
        let snap = SliderSnap::new(
            Rect::new(0.0, 0.0, 220.0, 30.0),
            Rect::new(90.0, 0.0, 20.0, 30.0),
            0.0,
            1.0,
            false,
        );
        assert_eq!(snap.value(Point::new(100.0, 15.0)), None, "the knob drags from where it is");
        let dead = SliderSnap::new(
            Rect::new(0.0, 0.0, 20.0, 30.0),
            Rect::new(0.0, 0.0, 20.0, 30.0),
            0.0,
            1.0,
            false,
        );
        assert_eq!(dead.value(Point::new(25.0, 15.0)), None, "no room to travel");
        let empty = SliderSnap::new(
            Rect::new(0.0, 0.0, 200.0, 30.0),
            Rect::new(0.0, 0.0, 20.0, 30.0),
            5.0,
            5.0,
            false,
        );
        assert_eq!(empty.value(Point::new(50.0, 15.0)), None, "an empty range");
    }

    #[test]
    fn a_right_to_left_slider_mirrors_and_tick_marks_round() {
        // The knob must sit clear of both presses: a press inside the knob leaves the value alone
        // (`snapValue`), and only the knob's width and whether the press falls inside it matter.
        let mirrored = SliderSnap::new(
            Rect::new(0.0, 0.0, 200.0, 30.0),
            Rect::new(90.0, 0.0, 20.0, 30.0),
            0.0,
            100.0,
            true,
        );
        assert_eq!(mirrored.value(Point::new(0.0, 15.0)), Some(100.0));
        assert_eq!(mirrored.value(Point::new(200.0, 15.0)), Some(0.0));

        let ticked = SliderSnap::new(
            Rect::new(0.0, 0.0, 200.0, 30.0),
            Rect::new(0.0, 0.0, 20.0, 30.0),
            0.0,
            100.0,
            false,
        )
        .tick_marks(11);
        // The press lands at 0.472 of the travel — 47.2 — and the eleven marks sit every 10.
        assert_eq!(ticked.value(Point::new(10.0 + 0.472 * 180.0, 15.0)), Some(50.0));
        let fraction = ticked
            .fraction(Point::new(10.0 + 0.472 * 180.0, 15.0))
            .expect("the press maps");
        assert!((fraction - 0.472).abs() < 1e-12);
        assert_eq!(closest_tick_mark_value(47.2, 0.0, 100.0, 11), 50.0);
        assert_eq!(closest_tick_mark_value(-5.0, 0.0, 100.0, 11), 0.0);
        assert_eq!(closest_tick_mark_value(105.0, 0.0, 100.0, 11), 100.0);
    }

    #[test]
    fn the_numeric_field_helpers_snap_to_a_grid_and_clamp() {
        assert_eq!(snapped_to_step(12.3, 1.0), 12.0);
        assert_eq!(snapped_to_step(12.5, 1.0), 13.0, "f64::round is half away from zero");
        assert_eq!(snapped_to_step(7.4, 2.0), 8.0);
        assert_eq!(clamped(150.0, 0.0, 100.0), 100.0);
        assert_eq!(clamped(-4.0, 0.0, 100.0), 0.0);
        assert_eq!(clamped(40.0, 0.0, 100.0), 40.0);
    }
}
