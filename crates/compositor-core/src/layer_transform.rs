//! `LayerTransform` — where a layer sits on the document — together with the value types the Move
//! tool's drag, group and snapping paths work in (`TransformGroup`, `TransformEdit`, `TransformDrag`,
//! `TransformSnap`) and the flip helper from `LayerFlip.swift`.
//!
//! Document coordinates are top-left with **y down**, and `rotation` is clockwise degrees.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::document::CanvasDocument;
use crate::geom::{AffineTransform, Point, Rect, Size};
use crate::Id;

/// `LayerSampling`. The raw strings are the manifest's values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerSampling {
    #[serde(rename = "Nearest")]
    Nearest,
    #[serde(rename = "Smooth")]
    Smooth,
    #[default]
    #[serde(rename = "High quality")]
    High,
}

/// `CGInterpolationQuality`'s three settings, as this app asked Core Graphics for them. The Rust
/// renderers map the same three settings onto their own samplers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LayerInterpolationQuality {
    None,
    Low,
    High,
}

impl LayerSampling {
    /// `CaseIterable` order.
    pub const ALL: [LayerSampling; 3] = [LayerSampling::Nearest, LayerSampling::Smooth, LayerSampling::High];

    pub fn raw_value(self) -> &'static str {
        match self {
            LayerSampling::Nearest => "Nearest",
            LayerSampling::Smooth => "Smooth",
            LayerSampling::High => "High quality",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sampling| sampling.raw_value() == value)
    }

    pub fn quality(self) -> LayerInterpolationQuality {
        match self {
            LayerSampling::Nearest => LayerInterpolationQuality::None,
            LayerSampling::Smooth => LayerInterpolationQuality::Low,
            LayerSampling::High => LayerInterpolationQuality::High,
        }
    }
}

/// Unrotated bounds in document pixels; rotation is clockwise around their center.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerTransform {
    pub origin: Point,
    pub size: Size,
    pub rotation: f64,
    pub flip_x: bool,
    pub flip_y: bool,
    pub sampling: LayerSampling,
}

/// The Swift memberwise initializer's defaults (`rotation`, the flips and `sampling`), with the
/// origin and size left at zero — every caller sets those from the layer it places.
impl Default for LayerTransform {
    fn default() -> Self {
        Self {
            origin: Point::ZERO,
            size: Size::ZERO,
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: LayerSampling::default(),
        }
    }
}

impl LayerTransform {
    pub fn center(&self) -> Point {
        Point::new(
            self.origin.x + self.size.width / 2.0,
            self.origin.y + self.size.height / 2.0,
        )
    }

    pub fn radians(&self) -> f64 {
        (self.rotation % 360.0) * std::f64::consts::PI / 180.0
    }

    pub fn is_valid(&self) -> bool {
        [self.origin.x, self.origin.y, self.size.width, self.size.height, self.rotation]
            .iter()
            .all(|value| value.is_finite())
            && (1.0..=300_000.0).contains(&self.size.width)
            && (1.0..=300_000.0).contains(&self.size.height)
            && self.origin.x.abs() <= 1_000_000.0
            && self.origin.y.abs() <= 1_000_000.0
    }

    /// The unit square (0…1, y down) placed inside this transform.
    pub fn point(&self, unit: Point) -> Point {
        let x = (unit.x - 0.5) * self.size.width;
        let y = (unit.y - 0.5) * self.size.height;
        let (sin, cos) = self.radians().sin_cos();
        let center = self.center();
        Point::new(center.x + x * cos - y * sin, center.y + x * sin + y * cos)
    }

    pub fn contains(&self, point: Point) -> bool {
        let x = point.x - self.center().x;
        let y = point.y - self.center().y;
        let (sin, cos) = self.radians().sin_cos();
        (x * cos + y * sin).abs() <= self.size.width / 2.0
            && (-x * sin + y * cos).abs() <= self.size.height / 2.0
    }

    /// Width as a percentage of the `pixel_size` it places (100% draws them 1:1).
    pub fn scale_percent(&self, pixel_size: Size) -> f64 {
        self.size.width / pixel_size.width.max(1.0) * 100.0
    }

    /// Both sides set to `percent` of `pixel_size`, keeping the center (and rotation and flips).
    pub fn scaled(&self, percent: f64, pixel_size: Size) -> LayerTransform {
        let mut result = *self;
        result.size = Size::new(pixel_size.width * percent / 100.0, pixel_size.height * percent / 100.0);
        let center = self.center();
        result.origin = Point::new(
            center.x - result.size.width / 2.0,
            center.y - result.size.height / 2.0,
        );
        result
    }

    /// Whole pixels and whole degrees: what dragging, scaling and rotating leave behind. Typed values are used
    /// as they are, so a fraction can still be asked for by hand.
    pub fn rounded(&self) -> LayerTransform {
        let mut result = *self;
        result.origin = Point::new(self.origin.x.round(), self.origin.y.round());
        result.size = Size::new(self.size.width.round().max(1.0), self.size.height.round().max(1.0));
        result.rotation = self.rotation.round();
        result
    }

    /// The unit square (0…1, y down) mapped where this transform places a layer on the document.
    pub fn unit_to_document(&self) -> AffineTransform {
        pixel_to_document(self, 1, 1)
    }

    /// A transform placing the unit square as `map` does — a rotated, maybe flipped rectangle (shear, which only
    /// uneven scaling of something rotated adds, is dropped). Keeps this transform's sampling.
    pub fn placing(&self, map: AffineTransform) -> LayerTransform {
        // Kept horizontal flip and the rotation nearest this one's, so the numbers stay familiar.
        let sign: f64 = if self.flip_x { -1.0 } else { 1.0 };
        let angle = (map.b * sign).atan2(map.a * sign);
        let along = -map.c * angle.sin() + map.d * angle.cos();
        let middle = map.applying(Point::new(0.5, 0.5));
        let mut result = *self;
        result.size = Size::new(map.a.hypot(map.b), along.abs());
        let degrees = angle * 180.0 / std::f64::consts::PI;
        result.rotation = degrees + ((self.rotation - degrees) / 360.0).round() * 360.0;
        result.flip_y = along < 0.0;
        result.origin = Point::new(
            middle.x - result.size.width / 2.0,
            middle.y - result.size.height / 2.0,
        );
        result
    }

    /// This placement carried along as a layer moves from `old` to `new`.
    pub fn following(&self, old: LayerTransform, new: LayerTransform) -> LayerTransform {
        if old == new {
            return *self;
        }
        // A plain move carries exactly.
        if old.size == new.size
            && old.rotation == new.rotation
            && old.flip_x == new.flip_x
            && old.flip_y == new.flip_y
        {
            let mut moved = *self;
            moved.origin.x += new.origin.x - old.origin.x;
            moved.origin.y += new.origin.y - old.origin.y;
            return moved;
        }
        self.placing(
            self.unit_to_document()
                .concatenating(old.unit_to_document().inverted())
                .concatenating(new.unit_to_document()),
        )
    }

    /// The same place on the document, whatever the sampling.
    pub fn same_placement(&self, other: LayerTransform) -> bool {
        let mut copy = *self;
        copy.sampling = other.sampling;
        copy == other
    }

    /// This placement mirrored across a vertical line at `axis` (or, not `horizontally`, a horizontal one): the
    /// picture flips, its angle turns the other way, and its middle crosses to the other side of the line.
    pub fn mirrored(&self, horizontally: bool, across: f64) -> LayerTransform {
        let mut result = *self;
        if horizontally {
            result.flip_x = !result.flip_x;
            result.origin.x = 2.0 * across - self.center().x - self.size.width / 2.0;
        } else {
            result.flip_y = !result.flip_y;
            result.origin.y = 2.0 * across - self.center().y - self.size.height / 2.0;
        }
        result.rotation = -self.rotation;
        result
    }

    /// The eight scale handles, in the order hit-testing and the drag math expect.
    pub const HANDLES: [Point; 8] = [
        Point::new(0.0, 0.0),
        Point::new(0.5, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 0.5),
        Point::new(1.0, 1.0),
        Point::new(0.5, 1.0),
        Point::new(0.0, 1.0),
        Point::new(0.0, 0.5),
    ];
}

/// `BrushRaster.pixelToDocument`: the affine map placing a `width`×`height` pixel grid into the
/// document under `transform`. Mirrors `compositor-pixels::raster`'s copy (core cannot depend on the
/// pixel crate).
pub fn pixel_to_document(transform: &LayerTransform, width: usize, height: usize) -> AffineTransform {
    let width = width as f64;
    let height = height as f64;
    AffineTransform::translation(transform.center().x, transform.center().y)
        .rotated_by(transform.radians())
        .scaled_by(
            transform.size.width / width * if transform.flip_x { -1.0 } else { 1.0 },
            transform.size.height / height * if transform.flip_y { -1.0 } else { 1.0 },
        )
        .translated_by(-width / 2.0, -height / 2.0)
}

/// Several layers transformed together: the upright box around them when the edit began (what the draft edits),
/// and each one's transform then.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformGroup {
    pub r#box: LayerTransform,
    pub originals: HashMap<Id, LayerTransform>,
}

/// `FloatingTransform` (`FloatingSelection.swift`): Cmd-T with a selection lifts the pixels onto a
/// temporary layer. The value type is pure data, so it lives here with the rest of the transform math
/// rather than in the session.
#[derive(Clone, Debug)]
pub struct FloatingTransform {
    pub source_id: Id,
    pub before: CanvasDocument,
    pub before_active: Option<Id>,
    pub original: LayerTransform,
    pub pixel_size: Size,
}

#[derive(Clone, Debug)]
pub struct TransformEdit {
    pub layer_id: Id,
    pub draft: LayerTransform,
    pub persistent: bool,
    /// Set when transforming selected pixels (Cmd-T with a selection) rather than a layer.
    pub floating: Option<FloatingTransform>,
    /// Set once a handle is Cmd-dragged: the four corners (document pixels, handle order) move
    /// freely, and Apply resamples the pixels into that shape.
    pub corners: Option<Vec<Point>>,
    /// Set when an unlinked mask is selected: the edit places the mask alone (its `placement`).
    pub mask: bool,
    /// Set when several layers are selected: the draft is the box around them all, and each follows it.
    pub group: Option<TransformGroup>,
    /// Opened by the Move bar's fields, for a value being typed or dragged: applied, one undo step, once they're done
    /// with it — the drag let go, or the field left.
    pub from_fields: bool,
}

impl TransformEdit {
    /// The Swift memberwise initializer: `floating`, `corners`, `mask`, `group` and `fromFields`
    /// default to their empty values.
    pub fn new(layer_id: Id, draft: LayerTransform, persistent: bool) -> Self {
        Self {
            layer_id,
            draft,
            persistent,
            floating: None,
            corners: None,
            mask: false,
            group: None,
            from_fields: false,
        }
    }
}

/// `TransformDrag.Mode`. (Swift's nested `Mode` becomes `TransformDragMode`; Rust types cannot nest.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransformDragMode {
    Move,
    Resize(usize),
    Rotate,
    Distort(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransformDrag {
    pub original: LayerTransform,
    pub start: Point,
    pub mode: TransformDragMode,
    /// The distortion's corners when the drag began; nil for an ordinary transform.
    pub original_corners: Option<Vec<Point>>,
}

impl TransformDrag {
    /// Corners after dragging to `point`: a corner handle moves its corner, an edge handle both of
    /// that edge's corners, and the body the whole shape. Nil when the drag isn't distorting.
    pub fn corners(&self, to: Point, shift: bool) -> Option<Vec<Point>> {
        let mut result = self.original_corners.clone()?;
        let mut dx = to.x - self.start.x;
        let mut dy = to.y - self.start.y;
        // Shift keeps what's being dragged on one axis.
        if shift {
            if dx.abs() >= dy.abs() {
                dy = 0.0;
            } else {
                dx = 0.0;
            }
        }
        let moved: Vec<usize> = match self.mode {
            TransformDragMode::Distort(index) => {
                if index % 2 == 0 {
                    vec![index / 2]
                } else {
                    vec![index / 2, (index / 2 + 1) % 4]
                }
            }
            TransformDragMode::Move => vec![0, 1, 2, 3],
            _ => return None,
        };
        for corner in moved {
            result[corner].x += dx;
            result[corner].y += dy;
        }
        Some(result)
    }

    pub fn updated(&self, to: Point, lock_ratio: bool, shift: bool, option: bool) -> LayerTransform {
        let mut result = self.original;
        match self.mode {
            TransformDragMode::Distort(_) => {}
            TransformDragMode::Move => {
                let mut dx = to.x - self.start.x;
                let mut dy = to.y - self.start.y;
                if shift {
                    if dx.abs() >= dy.abs() {
                        dy = 0.0;
                    } else {
                        dx = 0.0;
                    }
                }
                result.origin.x += dx;
                result.origin.y += dy;
            }
            TransformDragMode::Rotate => {
                let center = self.original.center();
                let delta = (to.y - center.y).atan2(to.x - center.x)
                    - (self.start.y - center.y).atan2(self.start.x - center.x);
                result.rotation += delta * 180.0 / std::f64::consts::PI;
                if shift {
                    result.rotation = (result.rotation / 15.0).round() * 15.0;
                }
            }
            TransformDragMode::Resize(index) => {
                let handle = LayerTransform::HANDLES[index];
                let anchor_unit = if option {
                    Point::new(0.5, 0.5)
                } else {
                    Point::new(1.0 - handle.x, 1.0 - handle.y)
                };
                let anchor = self.original.point(anchor_unit);
                // Use the initial handle plus pointer delta to avoid a jump on grab.
                let initial_handle = self.original.point(handle);
                let dx = initial_handle.x + to.x - self.start.x - anchor.x;
                let dy = initial_handle.y + to.y - self.start.y - anchor.y;
                // Center-to-handle distances cover half the size on each axis.
                let span: f64 = if option { 2.0 } else { 1.0 };
                let (sin, cos) = self.original.radians().sin_cos();
                let local_x = (dx * cos + dy * sin) * span;
                let local_y = (-dx * sin + dy * cos) * span;
                let sx = handle.x * 2.0 - 1.0;
                let sy = handle.y * 2.0 - 1.0;
                // Dragging a handle past the opposite side turns the layer over rather than stopping at nothing:
                // the size stays positive and the layer is flipped on that axis, as a negative scale would.
                let raw_width = if sx == 0.0 { self.original.size.width } else { local_x * sx };
                let raw_height = if sy == 0.0 { self.original.size.height } else { local_y * sy };
                let mirrored_x = raw_width < 0.0;
                let mirrored_y = raw_height < 0.0;
                let mut width = raw_width.abs().max(1.0);
                let mut height = raw_height.abs().max(1.0);
                if lock_ratio != shift {
                    let factor: f64 = if sx == 0.0 {
                        height / self.original.size.height
                    } else if sy == 0.0 {
                        width / self.original.size.width
                    } else {
                        // Project onto the original diagonal for proportional scaling.
                        let projected = (local_x * sx * self.original.size.width
                            + local_y * sy * self.original.size.height)
                            / (self.original.size.width * self.original.size.width
                                + self.original.size.height * self.original.size.height);
                        (1.0 / self.original.size.width.min(self.original.size.height)).max(projected)
                    };
                    width = self.original.size.width * factor;
                    height = self.original.size.height * factor;
                }
                result.size = Size::new(width, height);
                if mirrored_x {
                    result.flip_x = !result.flip_x;
                }
                if mirrored_y {
                    result.flip_y = !result.flip_y;
                }
                // Turned over, the box lies on the other side of the anchor.
                let offset_x = (0.5 - anchor_unit.x) * width * if mirrored_x { -1.0 } else { 1.0 };
                let offset_y = (0.5 - anchor_unit.y) * height * if mirrored_y { -1.0 } else { 1.0 };
                let center = Point::new(
                    anchor.x + offset_x * cos - offset_y * sin,
                    anchor.y + offset_x * sin + offset_y * cos,
                );
                result.origin = Point::new(center.x - width / 2.0, center.y - height / 2.0);
            }
        }
        if result.is_valid() {
            result
        } else {
            self.original
        }
    }
}

/// Moving a layer snaps its edges and center to the canvas and to the other layers. The pull is a fixed distance
/// on screen, so it feels the same at any zoom, and small enough to slide past without a fight.
pub enum TransformSnap {}

/// What [`TransformSnap::offset`] found: the move to apply, and the guide values each axis landed on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapOffset {
    pub offset: Size,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

impl TransformSnap {
    /// How close, in screen points, a guide comes before it snaps.
    pub const DISTANCE: f64 = 10.0;

    /// `box` moved so that whichever of its left, center or right lands nearest an `xs` target does, and the same
    /// vertically — each axis on its own, and only within `tolerance` document pixels. The targets it landed on
    /// come back too, to draw a line along.
    pub fn offset(box_: Rect, xs: &[f64], ys: &[f64], tolerance: f64) -> SnapOffset {
        let horizontal = shift_to(&[box_.min_x(), box_.mid_x(), box_.max_x()], xs, tolerance);
        let vertical = shift_to(&[box_.min_y(), box_.mid_y(), box_.max_y()], ys, tolerance);
        SnapOffset {
            offset: Size::new(horizontal.0, vertical.0),
            x: horizontal.1,
            y: vertical.1,
        }
    }
}

/// The smallest move that puts one of `guides` on one of `targets`, and the target it met.
fn shift_to(guides: &[f64], targets: &[f64], tolerance: f64) -> (f64, Option<f64>) {
    let mut best: Option<(f64, f64)> = None;
    for guide_value in guides {
        for target in targets {
            let movement = target - guide_value;
            if movement.abs() > tolerance {
                continue;
            }
            if let Some(current) = best {
                if current.0.abs() <= movement.abs() {
                    continue;
                }
            }
            best = Some((movement, *target));
        }
    }
    match best {
        Some((movement, target)) => (movement, Some(target)),
        None => (0.0, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn transform() -> LayerTransform {
        LayerTransform {
            origin: Point::new(10.0, 20.0),
            size: Size::new(100.0, 50.0),
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: LayerSampling::High,
        }
    }

    #[test]
    fn sampling_raw_values_round_trip() {
        for sampling in LayerSampling::ALL {
            assert_eq!(LayerSampling::from_raw(sampling.raw_value()), Some(sampling));
        }
        assert_eq!(LayerSampling::High.raw_value(), "High quality");
        assert_eq!(LayerSampling::from_raw("Nope"), None);
        assert_eq!(LayerSampling::default(), LayerSampling::High);
        assert_eq!(LayerSampling::Nearest.quality(), LayerInterpolationQuality::None);
        assert_eq!(LayerSampling::Smooth.quality(), LayerInterpolationQuality::Low);
        assert_eq!(LayerSampling::High.quality(), LayerInterpolationQuality::High);
    }

    #[test]
    fn default_matches_the_swift_memberwise_defaults() {
        let t = LayerTransform::default();
        assert_eq!(t.origin, Point::ZERO);
        assert_eq!(t.size, Size::ZERO);
        assert_eq!(t.rotation, 0.0);
        assert!(!t.flip_x && !t.flip_y);
        assert_eq!(t.sampling, LayerSampling::High);
        let t = LayerTransform { origin: Point::new(0.0, 0.0), size: Size::new(4.0, 4.0), ..Default::default() };
        assert!(t.is_valid());
    }

    #[test]
    fn serde_keys_match_the_manifest() {
        let value = serde_json::to_value(transform()).unwrap();
        assert_eq!(value["origin"], serde_json::json!([10.0, 20.0]));
        assert_eq!(value["size"], serde_json::json!([100.0, 50.0]));
        assert_eq!(value["rotation"], 0.0);
        assert_eq!(value["flipX"], false);
        assert_eq!(value["flipY"], false);
        assert_eq!(value["sampling"], "High quality");
        let back: LayerTransform = serde_json::from_value(value).unwrap();
        assert_eq!(back, transform());
    }

    #[test]
    fn center_radians_and_validity() {
        let mut t = transform();
        assert_eq!(t.center(), Point::new(60.0, 45.0));
        t.rotation = 370.0;
        assert!(near(t.radians(), 10f64.to_radians()));
        t.rotation = -370.0;
        assert!(near(t.radians(), -10f64.to_radians()));

        let mut t = transform();
        t.size = Size::new(0.5, 50.0);
        assert!(!t.is_valid(), "a side under one pixel is not a layer");
        t.size = Size::new(300_000.0, 300_000.0);
        assert!(t.is_valid());
        t.size = Size::new(300_000.5, 50.0);
        assert!(!t.is_valid(), "past the 300,000-pixel side limit");
        t = transform();
        t.origin = Point::new(-1_000_000.0, 1_000_000.0);
        assert!(t.is_valid());
        t.origin = Point::new(1_000_000.5, 0.0);
        assert!(!t.is_valid(), "past the origin limit");
        t = transform();
        t.rotation = f64::NAN;
        assert!(!t.is_valid());
    }

    #[test]
    fn point_and_contains_are_inverse_on_the_unit_square() {
        let straight = transform();
        assert!(straight.contains(straight.point(Point::new(0.5, 0.5))));
        for corner in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let p = straight.point(Point::new(corner.0, corner.1));
            assert!(straight.contains(p), "corner {corner:?} should be on the edge");
        }
        let t = LayerTransform { rotation: 33.0, ..straight };
        let middle = t.point(Point::new(0.5, 0.5));
        assert!(near(middle.x, t.center().x) && near(middle.y, t.center().y));
        assert!(t.contains(middle));
        for inner in [(0.25, 0.25), (0.75, 0.25), (0.75, 0.75), (0.25, 0.75)] {
            assert!(t.contains(t.point(Point::new(inner.0, inner.1))));
        }
        assert!(!t.contains(Point::new(t.center().x + 1_000.0, t.center().y)));
    }

    #[test]
    fn scale_percent_scaled_and_rounded() {
        let t = transform();
        assert!(near(t.scale_percent(Size::new(200.0, 50.0)), 50.0));
        assert!(near(t.scale_percent(Size::new(0.0, 0.0)), 10_000.0), "a zero size clamps to one pixel");
        let scaled = t.scaled(50.0, Size::new(100.0, 50.0));
        assert_eq!(scaled.size, Size::new(50.0, 25.0));
        assert_eq!(scaled.center(), t.center());
        assert_eq!(scaled.rotation, t.rotation);
        assert_eq!(scaled.sampling, t.sampling);

        let mut odd = t;
        odd.origin = Point::new(1.4, -1.5);
        odd.size = Size::new(10.4, 0.2);
        odd.rotation = 44.6;
        let rounded = odd.rounded();
        assert_eq!(rounded.origin, Point::new(1.0, -2.0), "rounded() is away from zero");
        assert_eq!(rounded.size, Size::new(10.0, 1.0));
        assert_eq!(rounded.rotation, 45.0);
    }

    #[test]
    fn unit_to_document_places_the_unit_square() {
        let t = transform();
        let map = t.unit_to_document();
        let middle = map.applying(Point::new(0.5, 0.5));
        assert!(near(middle.x, 60.0) && near(middle.y, 45.0));
        let top_left = map.applying(Point::ZERO);
        assert!(near(top_left.x, 10.0) && near(top_left.y, 20.0));

        let mut flipped = t;
        flipped.flip_x = true;
        let flipped_map = flipped.unit_to_document();
        let top_left = flipped_map.applying(Point::ZERO);
        assert!(near(top_left.x, 110.0) && near(top_left.y, 20.0), "the horizontal flip crosses the square");
    }

    #[test]
    fn placing_recovers_a_rotated_rectangle() {
        let t = LayerTransform { rotation: 30.0, ..transform() };
        let back = t.placing(t.unit_to_document());
        assert!(near(back.size.width, 100.0) && near(back.size.height, 50.0));
        assert!(near(back.origin.x, 10.0) && near(back.origin.y, 20.0));
        assert!(near(back.rotation, 30.0), "rotation comes back within a turn: {}", back.rotation);
        assert!(!back.flip_x && !back.flip_y);
        assert_eq!(back.sampling, t.sampling);
    }

    #[test]
    fn following_carries_a_plain_move_exactly() {
        let old = transform();
        let new = LayerTransform { origin: Point::new(30.0, 60.0), ..old };
        assert_eq!(old.following(old, new).origin, Point::new(30.0, 60.0), "a move carries the placement");
        let other = LayerTransform { origin: Point::new(0.0, 0.0), size: Size::new(10.0, 10.0), ..old };
        assert_eq!(other.following(old, new).origin, Point::new(20.0, 40.0));
        assert_eq!(other.following(old, old), other);
    }

    #[test]
    fn same_placement_ignores_only_sampling() {
        let t = transform();
        let other = LayerTransform { sampling: LayerSampling::Nearest, ..t };
        assert!(t.same_placement(other));
        assert!(!t.same_placement(LayerTransform { rotation: 1.0, ..t }));
    }

    #[test]
    fn mirrored_turns_the_angle_and_crosses_the_axis() {
        let t = LayerTransform { rotation: 30.0, ..transform() };
        let same_line = t.mirrored(true, 60.0);
        assert!(same_line.flip_x && !same_line.flip_y);
        assert!(near(same_line.origin.x, 10.0) && near(same_line.origin.y, 20.0));
        assert_eq!(same_line.rotation, -30.0);
        let across_zero = t.mirrored(true, 0.0);
        assert!(near(across_zero.center().x, -60.0));
        assert!(near(across_zero.origin.x, -110.0));
        let vertical = t.mirrored(false, 45.0);
        assert!(vertical.flip_y && !vertical.flip_x);
        assert!(near(vertical.origin.y, 20.0), "about its own middle it stays put");
        assert_eq!(vertical.rotation, -30.0);
    }

    #[test]
    fn handles_are_the_eight_scale_grips() {
        assert_eq!(LayerTransform::HANDLES.len(), 8);
        assert_eq!(LayerTransform::HANDLES[0], Point::new(0.0, 0.0));
        assert_eq!(LayerTransform::HANDLES[4], Point::new(1.0, 1.0));
        assert_eq!(LayerTransform::HANDLES[7], Point::new(0.0, 0.5));
    }

    #[test]
    fn snap_picks_the_nearest_guide_within_tolerance() {
        let found = TransformSnap::offset(Rect::new(0.0, 0.0, 100.0, 100.0), &[103.0], &[1_000.0], 10.0);
        assert_eq!(found.offset, Size::new(3.0, 0.0));
        assert_eq!(found.x, Some(103.0));
        assert_eq!(found.y, None);

        let nearest = TransformSnap::offset(Rect::new(0.0, 0.0, 10.0, 10.0), &[4.0, 6.0], &[], 10.0);
        assert_eq!(nearest.offset.width, -1.0, "the first guide/target pair at the smallest distance wins");
        assert_eq!(nearest.x, Some(4.0));

        let too_far = TransformSnap::offset(Rect::new(0.0, 0.0, 10.0, 10.0), &[40.0], &[], 10.0);
        assert_eq!(too_far.offset, Size::new(0.0, 0.0));
        assert_eq!(too_far.x, None);
    }

    #[test]
    fn distort_drag_moves_the_right_corners() {
        let corners = vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        let drag = TransformDrag {
            original: transform(),
            start: Point::new(0.0, 0.0),
            mode: TransformDragMode::Distort(2),
            original_corners: Some(corners.clone()),
        };
        let moved = drag.corners(Point::new(5.0, 8.0), false).unwrap();
        assert_eq!(moved[0], corners[0]);
        assert_eq!(moved[1], Point::new(15.0, 8.0), "an even index drags one corner");
        assert_eq!(moved[2], corners[2]);

        let edge = TransformDrag { mode: TransformDragMode::Distort(1), ..drag.clone() };
        let moved = edge.corners(Point::new(5.0, 8.0), false).unwrap();
        assert_eq!(moved[0], Point::new(5.0, 8.0), "an odd index drags that edge's two corners");
        assert_eq!(moved[1], Point::new(15.0, 8.0));
        assert_eq!(moved[2], corners[2]);

        let body = TransformDrag { mode: TransformDragMode::Move, ..drag.clone() };
        let moved = body.corners(Point::new(5.0, 8.0), false).unwrap();
        assert_eq!(moved[0], Point::new(5.0, 8.0));
        assert_eq!(moved[3], Point::new(5.0, 18.0));
    }

    #[test]
    fn shift_keeps_a_distort_on_one_axis_and_nothing_else_distorts() {
        let corners = vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        let drag = TransformDrag {
            original: transform(),
            start: Point::new(0.0, 0.0),
            mode: TransformDragMode::Distort(0),
            original_corners: Some(corners),
        };
        let moved = drag.corners(Point::new(5.0, 8.0), true).unwrap();
        assert_eq!(moved[0], Point::new(0.0, 8.0), "the bigger axis wins and the other is pinned");
        assert!(TransformDrag { mode: TransformDragMode::Rotate, ..drag.clone() }
            .corners(Point::new(1.0, 1.0), false)
            .is_none());
        assert!(TransformDrag { original_corners: None, ..drag }.corners(Point::new(1.0, 1.0), false).is_none());
    }

    #[test]
    fn drag_move_rotate_and_resize() {
        let original = transform();
        let move_drag = TransformDrag {
            original,
            start: Point::new(0.0, 0.0),
            mode: TransformDragMode::Move,
            original_corners: None,
        };
        let moved = move_drag.updated(Point::new(5.0, 3.0), false, false, false);
        assert_eq!(moved.origin, Point::new(15.0, 23.0));
        let shifted = move_drag.updated(Point::new(5.0, 3.0), false, true, false);
        assert_eq!(shifted.origin, Point::new(15.0, 20.0), "shift pins the smaller axis");

        // 20° clockwise, Shift snaps it to the nearest 15°.
        let rotate_drag = TransformDrag {
            original,
            start: original.point(Point::new(1.0, 0.5)),
            mode: TransformDragMode::Rotate,
            original_corners: None,
        };
        let rotated = rotate_drag.updated(Point::new(60.0 + 50.0 * 20f64.to_radians().cos(),
                                                     45.0 + 50.0 * 20f64.to_radians().sin()), false, false, false);
        assert!(near(rotated.rotation, 20.0));
        let snapped = rotate_drag.updated(Point::new(60.0 + 50.0 * 20f64.to_radians().cos(),
                                                     45.0 + 50.0 * 20f64.to_radians().sin()), false, true, false);
        assert!(near(snapped.rotation, 15.0));

        // Dragging the bottom-right handle from (110, 70) to (160, 120) grows to 150×100 keeping the top-left.
        let corner = original.point(LayerTransform::HANDLES[4]);
        assert!(near(corner.x, 110.0) && near(corner.y, 70.0));
        let resize_drag = TransformDrag {
            original,
            start: corner,
            mode: TransformDragMode::Resize(4),
            original_corners: None,
        };
        let resized = resize_drag.updated(Point::new(160.0, 120.0), false, false, false);
        assert!(near(resized.size.width, 150.0) && near(resized.size.height, 100.0));
        assert!(near(resized.origin.x, 10.0) && near(resized.origin.y, 20.0));
        assert!(!resized.flip_x && !resized.flip_y);

        // Past the opposite corner the layer turns over instead of collapsing.
        let mirrored = resize_drag.updated(Point::new(0.0, 0.0), false, false, false);
        assert!(mirrored.flip_x && mirrored.flip_y);
        assert!(near(mirrored.size.width, 10.0) && near(mirrored.size.height, 20.0));
        assert!(mirrored.is_valid());

        // A size past the limits falls back to the original rather than an invalid draft.
        let too_big = resize_drag.updated(Point::new(400_000.0, 400_000.0), false, false, false);
        assert_eq!(too_big, original);
    }

    #[test]
    fn transform_edit_new_leaves_the_optional_parts_empty() {
        let edit = TransformEdit::new(crate::Id::nil(), transform(), true);
        assert!(edit.floating.is_none() && edit.corners.is_none() && edit.group.is_none());
        assert!(!edit.mask && !edit.from_fields);
        assert!(edit.persistent);
    }
}
