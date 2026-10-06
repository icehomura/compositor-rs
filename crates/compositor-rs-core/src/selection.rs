//! Selection geometry: the document-space outline, the region its coverage spans, and the clip
//! edits are masked through.
//!
//! Rasterizing the outline into 8-bit gray coverage is pixel work and lives in
//! `compositor-rs-pixels`/`compositor-rs-render`; core only computes the geometry (see [`SelectionClip`]).

use crate::buffer::Gray8Image;
use crate::geom::{AffineTransform, Point, Rect, Size};
use crate::path::Path;

/// A document-space selection outline, clipped to the canvas. `None` on the document means no
/// selection; a selection whose path is empty is an explicit empty selection, which later edits
/// must treat as "touch nothing", never as "touch everything".
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentSelection {
    pub path: Path,
    pub antialiased: bool,
    /// How far the edge fades, in document pixels. 0 is a hard edge.
    pub feather: f64,
}

impl DocumentSelection {
    /// A selection around `path`, antialiased and hard-edged.
    pub fn new(path: Path) -> Self {
        Self {
            path,
            antialiased: true,
            feather: 0.0,
        }
    }

    /// The Swift memberwise initializer, whose last two arguments default.
    pub fn with_style(path: Path, antialiased: bool, feather: f64) -> Self {
        Self {
            path,
            antialiased,
            feather,
        }
    }

    pub fn is_empty(&self) -> bool {
        if self.path.is_empty() {
            return true;
        }
        let bounds = self.path.bounding_box();
        bounds.is_null() || bounds.is_empty()
    }

    /// Four Gaussian standard deviations retain the visible falloff outside the outline.
    pub fn coverage_bounds(&self) -> Rect {
        let outset = (self.feather * 2.0).ceil();
        self.path.bounding_box().inset_by(-outset, -outset)
    }

    /// Coverage for just the selected region of the canvas, ready to clip edits.
    ///
    /// Core has the geometry but not the pixels, so the returned clip's `coverage` starts `None`.
    /// `compositor-rs-render` fills it by rasterizing [`DocumentSelection::outline_for_region`] for
    /// `clip.rect` at `clip.rect.size` with this selection's `antialiased`/`feather`, and applies
    /// the clip to the drawing target (the Swift `SelectionClip.apply(to:)` is render's, not core's).
    pub fn clip(&self, canvas: Size) -> SelectionClip {
        let region = self
            .coverage_bounds()
            .inset_by(-1.0, -1.0)
            .integral()
            .intersection(Rect::from_origin_size(Point::ZERO, canvas));
        if self.is_empty() || region.is_null() || region.width() < 1.0 || region.height() < 1.0 {
            return SelectionClip::new(Rect::ZERO, None);
        }
        SelectionClip::new(region, None)
    }

    /// The outline translated so the region's top-left corner is its origin — the path whose
    /// rasterized coverage belongs in [`SelectionClip::coverage`].
    pub fn outline_for_region(&self, region: Rect) -> Path {
        self.path
            .transformed(&AffineTransform::translation(-region.min_x(), -region.min_y()))
    }
}

/// Selection coverage for one region of the document. Applied as a clip, soft edges blend
/// partially; with no coverage (an empty selection) it clips everything away.
///
/// Core holds the data only; `compositor-rs-render` rasterizes the coverage (8-bit gray, white =
/// selected, document resolution, top-left origin) and applies the clip to its drawing target.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionClip {
    /// The region of the document the coverage spans.
    pub rect: Rect,
    /// Coverage for `rect`; `None` clips everything away.
    pub coverage: Option<Gray8Image>,
}

impl SelectionClip {
    pub fn new(rect: Rect, coverage: Option<Gray8Image>) -> Self {
        Self { rect, coverage }
    }
}

/// The Magic tool's modes: Wand selects pixels of a similar color, Object traces the outline of
/// whatever the click lands on. Tab switches between them, as with the Brush's Paint and Erase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WandMode {
    Wand,
    Object,
}

impl WandMode {
    /// `CaseIterable` order.
    pub const ALL: [WandMode; 2] = [WandMode::Wand, WandMode::Object];

    pub fn raw_value(self) -> &'static str {
        match self {
            WandMode::Wand => "Wand",
            WandMode::Object => "Object",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LassoKind {
    Freehand,
    Polygonal,
    /// The Marquee's outlines; not offered in the Lasso's Freehand/Polygonal choice.
    Rectangle,
    Ellipse,
}

impl LassoKind {
    /// `CaseIterable` order.
    pub const ALL: [LassoKind; 4] = [
        LassoKind::Freehand,
        LassoKind::Polygonal,
        LassoKind::Rectangle,
        LassoKind::Ellipse,
    ];

    /// The Lasso's Freehand/Polygonal choice.
    pub const LASSO_CHOICES: [LassoKind; 2] = [LassoKind::Freehand, LassoKind::Polygonal];

    /// The Marquee's shape choice.
    pub const MARQUEE_CHOICES: [LassoKind; 2] = [LassoKind::Rectangle, LassoKind::Ellipse];

    pub fn raw_value(self) -> &'static str {
        match self {
            LassoKind::Freehand => "Freehand",
            LassoKind::Polygonal => "Polygonal",
            LassoKind::Rectangle => "Rectangle",
            LassoKind::Ellipse => "Ellipse",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.raw_value() == value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SelectionMode {
    /// The raw value is "New": that is the options bar's label for replacing.
    Replace,
    Add,
    Subtract,
}

impl SelectionMode {
    /// `CaseIterable` order, which is also the options bar's order.
    pub const ALL: [SelectionMode; 3] = [
        SelectionMode::Replace,
        SelectionMode::Add,
        SelectionMode::Subtract,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            SelectionMode::Replace => "New",
            SelectionMode::Add => "Add",
            SelectionMode::Subtract => "Subtract",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// The box a drag from `anchor` to `point` spans, in whole pixels. `square` evens the sides;
/// `from_center` grows the box around the anchor. Shared by the Marquee and the Shape tool.
pub struct DragBox;

impl DragBox {
    pub fn rect(from: Point, to: Point, square: bool, from_center: bool) -> Rect {
        let mut dx = to.x.round() - from.x;
        let mut dy = to.y.round() - from.y;
        if square {
            let side = dx.abs().max(dy.abs());
            dx = if dx < 0.0 { -side } else { side };
            dy = if dy < 0.0 { -side } else { side };
        }
        if from_center {
            Rect::new(
                from.x - dx.abs(),
                from.y - dy.abs(),
                dx.abs() * 2.0,
                dy.abs() * 2.0,
            )
        } else {
            Rect::new(
                from.x.min(from.x + dx),
                from.y.min(from.y + dy),
                dx.abs(),
                dy.abs(),
            )
        }
    }
}

/// A lasso outline being drawn, in document pixels. `cursor` is the polygonal lasso's
/// rubber-band end point.
#[derive(Clone, Debug)]
pub struct LassoDraft {
    pub points: Vec<Point>,
    pub cursor: Option<Point>,
    pub mode: SelectionMode,
    pub kind: LassoKind,
    /// The Rectangular Marquee's starting corner (or center), in whole pixels.
    pub anchor: Option<Point>,
}

impl LassoDraft {
    /// A fresh outline with no rubber-band cursor and no marquee anchor.
    pub fn new(points: Vec<Point>, mode: SelectionMode, kind: LassoKind) -> Self {
        Self {
            points,
            cursor: None,
            mode,
            kind,
            anchor: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_rect(rect: Rect) -> Path {
        Path::rect(rect)
    }

    #[test]
    fn drag_box_spans_whole_pixels_in_any_direction() {
        // Ported from CompositorTests.SelectionTests: the Marquee's whole-pixel rectangle.
        // The session rounds the anchor before it draws, so the anchor is already whole here.
        let rect = DragBox::rect(Point::new(60.0, 71.0), Point::new(20.2, 30.3), false, false);
        assert_eq!(rect, Rect::new(20.0, 30.0, 40.0, 41.0));
        let negative = DragBox::rect(Point::new(100.0, 100.0), Point::new(60.0, 80.0), false, false);
        assert_eq!(negative, Rect::new(60.0, 80.0, 40.0, 20.0));
        // A click: no extent at all.
        assert_eq!(
            DragBox::rect(Point::new(5.0, 5.0), Point::new(5.4, 5.4), false, false),
            Rect::new(5.0, 5.0, 0.0, 0.0)
        );
    }

    #[test]
    fn drag_box_squares_even_the_sides() {
        let rect = DragBox::rect(Point::new(10.0, 10.0), Point::new(40.0, 20.0), true, false);
        assert_eq!(rect, Rect::new(10.0, 10.0, 30.0, 30.0));
        let negative = DragBox::rect(Point::new(100.0, 100.0), Point::new(90.0, 120.0), true, false);
        assert_eq!(negative, Rect::new(80.0, 100.0, 20.0, 20.0));
    }

    #[test]
    fn drag_box_grows_from_the_center() {
        let rect = DragBox::rect(Point::new(50.0, 50.0), Point::new(60.0, 55.0), false, true);
        assert_eq!(rect, Rect::new(40.0, 45.0, 20.0, 10.0));
        let square = DragBox::rect(Point::new(50.0, 50.0), Point::new(45.0, 58.0), true, true);
        assert_eq!(square, Rect::new(42.0, 42.0, 16.0, 16.0));
        let negative = DragBox::rect(Point::new(50.0, 50.0), Point::new(42.0, 46.0), false, true);
        assert_eq!(negative, Rect::new(42.0, 46.0, 16.0, 8.0));
    }

    #[test]
    fn empty_selection_is_distinct_from_a_real_one() {
        let empty = DocumentSelection::new(Path::empty());
        assert!(empty.is_empty());
        assert!(DocumentSelection::new(path_rect(Rect::ZERO)).is_empty());
        let real = DocumentSelection::new(path_rect(Rect::new(10.0, 20.0, 30.0, 40.0)));
        assert!(!real.is_empty());
    }

    #[test]
    fn coverage_bounds_allow_four_gaussian_sigmas() {
        let path = path_rect(Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!(
            DocumentSelection::new(path.clone()).coverage_bounds(),
            Rect::new(10.0, 20.0, 30.0, 40.0)
        );
        assert_eq!(
            DocumentSelection::with_style(path.clone(), true, 3.0).coverage_bounds(),
            Rect::new(4.0, 14.0, 42.0, 52.0)
        );
        // The outset is ceil(feather * 2), so a fractional feather still grows by whole pixels.
        assert_eq!(
            DocumentSelection::with_style(path.clone(), true, 2.5).coverage_bounds(),
            Rect::new(5.0, 15.0, 40.0, 50.0)
        );
        assert_eq!(
            DocumentSelection::with_style(path, true, 0.1).coverage_bounds(),
            Rect::new(9.0, 19.0, 32.0, 42.0)
        );
    }

    #[test]
    fn clip_is_the_coverage_bounds_plus_a_pixel_of_slack_inside_the_canvas() {
        // Ported from CompositorTests.LevelsTests: a 1×1 selection on a 3×1 canvas.
        let selection = DocumentSelection::with_style(path_rect(Rect::new(0.0, 0.0, 1.0, 1.0)), false, 0.0);
        let clip = selection.clip(Size::new(3.0, 1.0));
        assert_eq!(clip.rect, Rect::new(0.0, 0.0, 2.0, 1.0));
        assert!(clip.coverage.is_none());
        assert!(!clip.rect.is_empty());
    }

    #[test]
    fn clip_grows_with_the_feather_and_stays_on_the_canvas() {
        let selection = DocumentSelection::with_style(path_rect(Rect::new(1.0, 1.0, 2.0, 2.0)), true, 2.0);
        let clip = selection.clip(Size::new(10.0, 10.0));
        assert_eq!(clip.rect, Rect::new(0.0, 0.0, 8.0, 8.0));
    }

    #[test]
    fn empty_and_off_canvas_selections_clip_everything_away() {
        let empty = DocumentSelection::new(Path::empty());
        let clip = empty.clip(Size::new(100.0, 100.0));
        assert_eq!(clip.rect, Rect::ZERO);
        assert!(clip.coverage.is_none());
        assert!(clip.rect.is_empty());

        let off_canvas = DocumentSelection::new(path_rect(Rect::new(50.0, 50.0, 10.0, 10.0)));
        let clip = off_canvas.clip(Size::new(3.0, 1.0));
        assert_eq!(clip.rect, Rect::ZERO);
        assert!(clip.coverage.is_none());
    }

    #[test]
    fn outline_for_region_translates_the_path_into_region_coordinates() {
        let selection = DocumentSelection::new(path_rect(Rect::new(10.0, 20.0, 5.0, 5.0)));
        let outline = selection.outline_for_region(Rect::new(12.0, 22.0, 4.0, 4.0));
        assert_eq!(outline.bounding_box(), Rect::new(-2.0, -2.0, 5.0, 5.0));
    }

    #[test]
    fn selection_vocabulary_keeps_its_raw_values() {
        assert_eq!(SelectionMode::Replace.raw_value(), "New");
        assert_eq!(SelectionMode::Add.raw_value(), "Add");
        assert_eq!(SelectionMode::Subtract.raw_value(), "Subtract");
        assert_eq!(SelectionMode::from_raw("New"), Some(SelectionMode::Replace));
        assert_eq!(WandMode::from_raw("Object"), Some(WandMode::Object));
        assert_eq!(LassoKind::from_raw("Rectangle"), Some(LassoKind::Rectangle));
        assert_eq!(LassoKind::LASSO_CHOICES, [LassoKind::Freehand, LassoKind::Polygonal]);
        assert_eq!(LassoKind::MARQUEE_CHOICES, [LassoKind::Rectangle, LassoKind::Ellipse]);
    }
}
