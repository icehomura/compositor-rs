//! Path booleans and path stroking — `CGPath.union`/`intersection`/`subtracting`/`symmetricDifference`,
//! `CGPath.normalized(_:using:)` and `CGPath.copy(strokingWithWidth:lineCap:lineJoin:miterLimit:transform:)`
//! — on top of Clipper2 (`clipper2`), a proven planar clipping and offsetting engine.
//!
//! Clipper2 is a fixed-point engine: coordinates are integers. Its scale here is 1024 units per
//! document pixel — a power of two — so every coordinate is rounded to the nearest 1/1024 pixel
//! before an operation and comes back as a multiple of 1/1024 pixel. That snapping is the only
//! difference from Core Graphics' double-precision booleans, and it sits two orders of magnitude
//! below the canvas's 1/16-pixel antialiasing step.
//!
//! Inputs are flattened to polylines with [`flatten`], so results are polygon outlines — subpaths of
//! `move_to` + `add_line` + `close_subpath`, never curves.
//!
//! Where Core Graphics cannot be reproduced exactly: the outline of a round cap or join is an
//! inscribed polygon rather than an arc (Clipper2's documented arc imprecision is 0.2% of the half
//! width), a miter that exceeds the miter limit becomes Clipper2's squared corner instead of Core
//! Graphics' bevelled one, and [`LineJoin::Bevel`] maps to Clipper2's `Square` join rather than its
//! `Bevel` join, which squares the corner off at the offset distance.

use clipper2::{
    inflate, Clipper, ClipperError, EndType, FillRule as ClipperFillRule, JoinType,
    Path as ClipperContour, Paths as ClipperContours, Point as ClipperPoint, PointScaler,
    WithClips,
};

use crate::geom::{AffineTransform, Point};
use crate::path::{flatten, FillRule, Path};

/// Clipper2 units per document pixel. A power of two, so scaling and unscaling are exact and a
/// coordinate only ever loses the bits below 1/1024 pixel.
const UNITS_PER_PIXEL: f64 = 1024.0;

/// Coordinates beyond this many document pixels are refused before they can overflow Clipper2's
/// fixed-point `i64` grid (`MAX_COORDINATE * UNITS_PER_PIXEL` is some nine thousand times smaller
/// than `i64::MAX`). No real document comes near it.
const MAX_COORDINATE: f64 = 1.0e12;

/// The miter limit used when a caller passes a non-finite or non-positive one — Core Graphics'
/// default miter limit.
const DEFAULT_MITER_LIMIT: f64 = 10.0;

/// `CGLineCap`: how the ends of an open subpath are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCap {
    /// `kCGLineCapButt`: the stroke stops at the endpoint.
    Butt,
    /// `kCGLineCapRound`: a half disc of the stroke's half width caps the endpoint.
    Round,
    /// `kCGLineCapSquare`: a half square of the stroke's half width extends past the endpoint.
    Square,
}

/// `CGLineJoin`: how two segments of a subpath meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineJoin {
    /// `kCGLineJoinMiter`: the offset edges meet at their intersection, unless the miter limit cuts
    /// the corner.
    Miter,
    /// `kCGLineJoinRound`: a disc of the stroke's half width fills the corner.
    Round,
    /// `kCGLineJoinBevel`: the offset edges are joined by a straight cut.
    Bevel,
}

/// The Clipper2 coordinate scaler: [`UNITS_PER_PIXEL`] fixed-point units per document pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Units;

impl PointScaler for Units {
    const MULTIPLIER: f64 = UNITS_PER_PIXEL;
}

/// One polygon contour in Clipper2's fixed-point space.
type Contour = ClipperContour<Units>;

/// A set of polygon contours in Clipper2's fixed-point space.
type Contours = ClipperContours<Units>;

/// One fixed-point vertex.
type Vertex = ClipperPoint<Units>;

/// `CGPath.union(_:using:)`: the region filled by either path, with each path's own region taken
/// under `rule`.
pub fn union(a: &Path, b: &Path, rule: FillRule) -> Path {
    boolean(a, b, rule, |clipper, rule| clipper.union(rule))
}

/// `CGPath.intersection(_:using:)`: the region filled by both paths.
pub fn intersection(a: &Path, b: &Path, rule: FillRule) -> Path {
    boolean(a, b, rule, |clipper, rule| clipper.intersect(rule))
}

/// `CGPath.subtracting(_:using:)`: the region filled by `a` but not by `b`.
pub fn subtracting(a: &Path, b: &Path, rule: FillRule) -> Path {
    boolean(a, b, rule, |clipper, rule| clipper.difference(rule))
}

/// `CGPath.symmetricDifference(_:using:)`: the region filled by exactly one of the two paths.
pub fn symmetric_difference(a: &Path, b: &Path, rule: FillRule) -> Path {
    boolean(a, b, rule, |clipper, rule| clipper.xor(rule))
}

/// `CGPath.normalized(_:using:)`: the same coverage as `path` under `rule`, as a path whose
/// subpaths no longer overlap or cross — self-intersections are resolved and intersecting subpaths
/// are merged, with the holes the fill rule implies.
pub fn normalized(path: &Path, rule: FillRule) -> Path {
    boolean(path, &Path::empty(), rule, |clipper, rule| {
        clipper.union(rule)
    })
}

/// `CGPath.copy(strokingWithWidth:lineCap:lineJoin:miterLimit:transform:)`: the outline of the
/// region a pen of `width` covers when dragged along `path`.
///
/// Core Graphics strokes every subpath on its own. An open subpath gets `cap` at both ends; a closed
/// subpath has no ends to cap — its two offset sides meet at the subpath's start vertex as a join.
/// Clipper2's `EndType::Joined` is exactly that (it offsets both sides of a closed polyline and
/// joins them), so closed subpaths stroke as `Joined` and open ones with the cap's end type.
///
/// `transform` is applied to the path before it is stroked, as Core Graphics does, so `width` is in
/// the transform's output space. A non-finite or non-positive width has no outline: the result is
/// empty.
pub fn stroking_with_width(
    path: &Path,
    width: f64,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f64,
    transform: Option<&AffineTransform>,
) -> Path {
    if !width.is_finite() || width <= 0.0 {
        return Path::empty();
    }
    let identity = AffineTransform::IDENTITY;
    let transform = transform.unwrap_or(&identity);
    let join = clipper_join(join);
    let miter_limit = miter_limit_units(miter_limit);
    let delta = width / 2.0;

    let mut stroke = Path::empty();
    let (closed, open) = split_subpaths(path, transform);
    if !closed.is_empty() {
        let band = inflate(closed, delta, join, EndType::Joined, miter_limit);
        stroke.append(&path_from_contours(&band));
    }
    if !open.is_empty() {
        let band = inflate(open, delta, join, clipper_end_type(cap), miter_limit);
        stroke.append(&path_from_contours(&band));
    }
    stroke
}

/// The Clipper2 fill rule that `rule` names: Core Graphics' winding rule is Clipper2's non-zero
/// rule, its even-odd rule is Clipper2's.
fn clipper_fill_rule(rule: FillRule) -> ClipperFillRule {
    match rule {
        FillRule::Winding => ClipperFillRule::NonZero,
        FillRule::EvenOdd => ClipperFillRule::EvenOdd,
    }
}

/// Runs `operation` with `a` as the subject and `b` as the clip, and returns its contours as a
/// polygon path. A failed operation yields an empty path: the callers below fill, clip and stroke
/// with these, so an empty result is the safe answer.
fn boolean(
    a: &Path,
    b: &Path,
    rule: FillRule,
    operation: impl FnOnce(Clipper<WithClips, Units>, ClipperFillRule) -> Result<Contours, ClipperError>,
) -> Path {
    let subject = contours_of(a);
    let clip = contours_of(b);
    if subject.is_empty() && clip.is_empty() {
        return Path::empty();
    }
    let clipper = Clipper::new().add_subject(subject).add_clip(clip);
    match operation(clipper, clipper_fill_rule(rule)) {
        Ok(result) => path_from_contours(&result),
        Err(_) => Path::empty(),
    }
}

/// The path's subpaths as closed Clipper2 contours: [`flatten`] turns each subpath into a polyline,
/// and every subpath is implicitly closed, as filling treats it.
fn contours_of(path: &Path) -> Contours {
    let mut contours = Vec::new();
    for subpath in flatten(path, &AffineTransform::IDENTITY) {
        if let Some(contour) = fixed_contour(&subpath.points, 3) {
            contours.push(contour);
        }
    }
    ClipperContours::new(contours)
}

/// The path's flattened subpaths, split into the closed ones — which the offsetter treats as
/// polygons that meet at their start vertex — and the open ones, which take caps. An open subpath
/// needs one point (a bare move offsets to a dot); a closed one needs three to enclose anything.
fn split_subpaths(path: &Path, transform: &AffineTransform) -> (Contours, Contours) {
    let mut closed = Vec::new();
    let mut open = Vec::new();
    for subpath in flatten(path, transform) {
        let minimum = if subpath.closed { 3 } else { 1 };
        let Some(contour) = fixed_contour(&subpath.points, minimum) else {
            continue;
        };
        if subpath.closed {
            closed.push(contour);
        } else {
            open.push(contour);
        }
    }
    (ClipperContours::new(closed), ClipperContours::new(open))
}

/// A polyline as a Clipper2 contour, or `None` when it has fewer than `minimum` points or holds a
/// coordinate Clipper2's fixed point cannot represent — a non-finite value, or one outside
/// [`MAX_COORDINATE`]. Such a subpath is dropped whole, since one bad vertex makes its geometry
/// meaningless.
fn fixed_contour(points: &[Point], minimum: usize) -> Option<Contour> {
    if points.len() < minimum {
        return None;
    }
    let mut vertices = Vec::with_capacity(points.len());
    for point in points {
        if !point.x.is_finite()
            || !point.y.is_finite()
            || point.x.abs() > MAX_COORDINATE
            || point.y.abs() > MAX_COORDINATE
        {
            return None;
        }
        vertices.push(Vertex::new(point.x, point.y));
    }
    Some(Contour::new(vertices))
}

/// The contours as a polygon path: one closed subpath per contour, in document units. Clipper2
/// reports a contour without repeating its first point at the end, so a move/line/close builds it
/// exactly.
fn path_from_contours(contours: &Contours) -> Path {
    let mut path = Path::empty();
    for contour in contours.iter() {
        if contour.len() < 3 {
            continue;
        }
        let mut vertices = contour.iter();
        let Some(first) = vertices.next() else {
            continue;
        };
        path.move_to(Point::new(first.x(), first.y()));
        for vertex in vertices {
            path.add_line(Point::new(vertex.x(), vertex.y()));
        }
        path.close_subpath();
    }
    path
}

/// The Clipper2 end type that caps an open subpath the way `cap` does.
fn clipper_end_type(cap: LineCap) -> EndType {
    match cap {
        LineCap::Butt => EndType::Butt,
        LineCap::Round => EndType::Round,
        LineCap::Square => EndType::Square,
    }
}

/// The Clipper2 join for `join`.
fn clipper_join(join: LineJoin) -> JoinType {
    match join {
        LineJoin::Miter => JoinType::Miter,
        LineJoin::Round => JoinType::Round,
        LineJoin::Bevel => JoinType::Square,
    }
}

/// The miter limit Clipper2 will actually compare against. Clipper2's offset entry point in the
/// `clipper2` crate scales the limit along with the coordinates before handing it over, but
/// Clipper2 wants a dimensionless ratio — as Core Graphics' `miterLimit` is — so the ratio is
/// divided here to cancel that scaling back out. A non-finite or non-positive limit falls back to
/// [`DEFAULT_MITER_LIMIT`].
fn miter_limit_units(miter_limit: f64) -> f64 {
    let limit = if miter_limit.is_finite() && miter_limit > 0.0 {
        miter_limit
    } else {
        DEFAULT_MITER_LIMIT
    };
    limit / UNITS_PER_PIXEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use crate::path::PathElement;

    /// A rectangle path spanning `[x, x + width] × [y, y + height]`.
    fn rect(x: f64, y: f64, width: f64, height: f64) -> Path {
        Path::rect(Rect::new(x, y, width, height))
    }

    /// A horizontal line from `x0` to `x1` at `y`.
    fn horizontal_line(x0: f64, x1: f64, y: f64) -> Path {
        let mut line = Path::empty();
        line.move_to(Point::new(x0, y));
        line.add_line(Point::new(x1, y));
        line
    }

    fn contains(path: &Path, x: f64, y: f64) -> bool {
        path.contains(Point::new(x, y), FillRule::Winding)
    }

    /// The number of subpaths, one per `MoveTo`.
    fn subpath_count(path: &Path) -> usize {
        path.elements()
            .iter()
            .filter(|element| matches!(element, PathElement::MoveTo(_)))
            .count()
    }

    #[test]
    fn union_of_two_squares_is_one_square() {
        // Two 20×10 squares sharing the edge y = 10.
        let top = rect(0.0, 0.0, 20.0, 10.0);
        let bottom = rect(0.0, 10.0, 20.0, 10.0);
        let joined = union(&top, &bottom, FillRule::Winding);

        assert_eq!(joined.bounding_box(), Rect::new(0.0, 0.0, 20.0, 20.0));
        for (x, y) in [
            (0.5, 0.5),
            (19.5, 0.5),
            (0.5, 19.5),
            (19.5, 19.5),
            (10.0, 13.0),
            (10.0, 7.0),
        ] {
            assert!(contains(&joined, x, y), "({x}, {y}) should be inside");
        }
        for (x, y) in [
            (-1.0, 10.0),
            (10.0, -1.0),
            (21.0, 10.0),
            (10.0, 21.0),
            (20.5, 20.5),
        ] {
            assert!(!contains(&joined, x, y), "({x}, {y}) should be outside");
        }
    }

    #[test]
    fn subtracting_a_centred_square_leaves_a_hole() {
        let big = rect(0.0, 0.0, 20.0, 20.0);
        let centred = rect(5.0, 5.0, 10.0, 10.0);
        let holed = subtracting(&big, &centred, FillRule::Winding);

        assert_eq!(holed.bounding_box(), big.bounding_box());
        assert!(!contains(&holed, 10.0, 10.0), "the hole must not be filled");
        assert!(contains(&holed, 2.0, 2.0));
        assert!(contains(&holed, 17.5, 10.0));
        assert!(contains(&holed, 10.0, 2.5));
    }

    #[test]
    fn intersection_of_disjoint_squares_is_empty() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        let far = rect(100.0, 100.0, 10.0, 10.0);
        assert!(intersection(&a, &far, FillRule::Winding).is_empty());

        let overlapping = rect(5.0, 5.0, 10.0, 10.0);
        let overlap = intersection(&a, &overlapping, FillRule::Winding);
        assert_eq!(overlap.bounding_box(), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert!(contains(&overlap, 7.5, 7.5));
    }

    #[test]
    fn symmetric_difference_excludes_the_overlap() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        let b = rect(5.0, 5.0, 10.0, 10.0);
        let either = symmetric_difference(&a, &b, FillRule::Winding);

        assert!(!contains(&either, 7.5, 7.5), "the overlap is in both");
        assert!(contains(&either, 2.5, 2.5));
        assert!(contains(&either, 12.5, 12.5));
    }

    #[test]
    fn normalized_keeps_the_coverage_of_overlapping_subpaths() {
        // Two same-direction squares that overlap, as two subpaths of one path.
        let mut overlapping = rect(0.0, 0.0, 10.0, 10.0);
        overlapping.append(&rect(5.0, 5.0, 10.0, 10.0));
        let normalized = normalized(&overlapping, FillRule::Winding);

        // The two subpaths merge into the single 15×15 outline they cover.
        assert_eq!(normalized.bounding_box(), Rect::new(0.0, 0.0, 15.0, 15.0));
        assert_eq!(subpath_count(&normalized), 1);

        // Sampling every half pixel off the boundaries, the coverage is unchanged.
        for i in 0..=40 {
            for j in 0..=40 {
                let point = Point::new(0.25 + i as f64 * 0.5, 0.25 + j as f64 * 0.5);
                assert_eq!(
                    normalized.contains(point, FillRule::Winding),
                    overlapping.contains(point, FillRule::Winding),
                    "coverage differs at {point:?}"
                );
            }
        }
    }

    #[test]
    fn the_fill_rule_decides_whether_nested_subpaths_are_a_hole() {
        // Nested squares wound the same way: non-zero winds twice, even-odd crosses twice.
        let mut nested = rect(0.0, 0.0, 10.0, 10.0);
        nested.append(&rect(2.0, 2.0, 6.0, 6.0));

        let nonzero = normalized(&nested, FillRule::Winding);
        assert!(contains(&nonzero, 5.0, 5.0));
        assert_eq!(subpath_count(&nonzero), 1);

        let even_odd = normalized(&nested, FillRule::EvenOdd);
        assert!(!even_odd.contains(Point::new(5.0, 5.0), FillRule::EvenOdd));
        assert!(even_odd.contains(Point::new(1.0, 1.0), FillRule::EvenOdd));
        assert_eq!(subpath_count(&even_odd), 2);
    }

    #[test]
    fn round_caps_cover_the_ends_of_a_stroked_line() {
        // A 10-px horizontal line at y = 50, stroked 4 wide.
        let line = horizontal_line(5.0, 15.0, 50.0);
        let stroke = stroking_with_width(&line, 4.0, LineCap::Round, LineJoin::Round, 10.0, None);

        // The round caps reach half a width past each end -- and no further.
        assert!(contains(&stroke, 4.5, 50.0));
        assert!(contains(&stroke, 15.5, 50.0));
        assert!(!contains(&stroke, 2.5, 50.0));
        assert!(!contains(&stroke, 17.5, 50.0));
        // The exact cap extremes, where an ideal round cap reaches 2 past the line's ends, fall
        // outside the polygonal outline: Clipper2's arcs are inscribed.
        assert!(!contains(&stroke, 3.0, 50.0));
        assert!(!contains(&stroke, 17.0, 50.0));

        // The band is the half width either side, and the line's own ends sit inside the caps.
        assert!(contains(&stroke, 10.0, 48.5));
        assert!(contains(&stroke, 10.0, 51.5));
        assert!(!contains(&stroke, 10.0, 47.0));
        assert!(!contains(&stroke, 10.0, 53.0));
        assert!(contains(&stroke, 5.0, 50.0));
        assert!(contains(&stroke, 15.0, 50.0));
    }

    #[test]
    fn butt_and_square_caps_differ_at_the_ends_of_a_stroked_line() {
        let line = horizontal_line(5.0, 15.0, 50.0);

        let butt = stroking_with_width(&line, 4.0, LineCap::Butt, LineJoin::Miter, 10.0, None);
        assert_eq!(butt.bounding_box(), Rect::new(5.0, 48.0, 10.0, 4.0));
        assert!(!contains(&butt, 4.5, 50.0));
        assert!(contains(&butt, 5.5, 50.0));

        let square = stroking_with_width(&line, 4.0, LineCap::Square, LineJoin::Miter, 10.0, None);
        assert_eq!(square.bounding_box(), Rect::new(3.0, 48.0, 14.0, 4.0));
        assert!(contains(&square, 4.5, 50.0));
        // The corner of a square cap, which a round cap of the same width does not cover.
        assert!(contains(&square, 3.5, 48.25));
        assert!(!contains(
            &stroking_with_width(&line, 4.0, LineCap::Round, LineJoin::Miter, 10.0, None),
            3.5,
            48.25
        ));
    }

    #[test]
    fn a_closed_subpath_strokes_to_a_band_without_caps() {
        // A 10×10 square stroked 2 wide: a band 1 either side of its outline.
        let square = rect(0.0, 0.0, 10.0, 10.0);
        let band = stroking_with_width(&square, 2.0, LineCap::Butt, LineJoin::Miter, 10.0, None);

        assert!(!contains(&band, 5.0, 5.0), "a closed subpath is not filled");
        assert!(contains(&band, 0.5, 5.0));
        assert!(contains(&band, 10.5, 5.0));
        assert!(contains(&band, 5.0, 0.5));
        assert!(contains(&band, -0.5, 5.0));
        assert!(!contains(&band, -1.5, 5.0));
        assert!(!contains(&band, 1.5, 5.0));

        // Mitered corners reach the half width diagonally; the ends meet, so no cap adds anything.
        assert_eq!(band.bounding_box(), Rect::new(-1.0, -1.0, 12.0, 12.0));
        let round = stroking_with_width(&square, 2.0, LineCap::Round, LineJoin::Round, 10.0, None);
        assert_eq!(round.bounding_box(), band.bounding_box());
        assert!(!contains(&round, 5.0, 5.0));
    }

    #[test]
    fn the_miter_limit_is_honoured() {
        // A V whose corner turns 60 degrees: its miter runs 2/sin(60°) ≈ 2.31 half widths out, so
        // a limit below that squares the corner off at the half width instead.
        let v = {
            let mut path = Path::empty();
            path.move_to(Point::new(0.0, -8.660_254_037_844_386));
            path.add_line(Point::new(10.0, 0.0));
            path.add_line(Point::new(0.0, 8.660_254_037_844_386));
            path
        };
        let mitered = stroking_with_width(&v, 4.0, LineCap::Butt, LineJoin::Miter, 10.0, None);
        let squared = stroking_with_width(&v, 4.0, LineCap::Butt, LineJoin::Miter, 1.5, None);

        assert!(contains(&mitered, 12.5, 0.0), "the miter should reach out");
        assert!(!contains(&squared, 12.5, 0.0), "the corner should be cut");
    }

    #[test]
    fn stroking_applies_the_transform_first() {
        let line = horizontal_line(0.0, 10.0, 0.0);
        let moved = stroking_with_width(
            &line,
            2.0,
            LineCap::Butt,
            LineJoin::Miter,
            10.0,
            Some(&AffineTransform::translation(0.0, 100.0)),
        );

        assert_eq!(moved.bounding_box(), Rect::new(0.0, 99.0, 10.0, 2.0));
        assert!(contains(&moved, 5.0, 100.0));
    }

    #[test]
    fn curves_are_flattened_before_the_operation() {
        // An ellipse is a path of four cubics: the boolean sees the flattened polygon.
        let circle = Path::ellipse(Rect::new(0.0, 0.0, 10.0, 10.0));
        let square = rect(5.0, 0.0, 10.0, 10.0);
        let joined = union(&circle, &square, FillRule::Winding);

        assert_eq!(joined.bounding_box(), Rect::new(0.0, 0.0, 15.0, 10.0));
        assert!(contains(&joined, 2.5, 5.0), "inside the circle only");
        assert!(contains(&joined, 12.5, 5.0), "inside the square only");
        assert!(contains(&joined, 5.0, 5.0), "inside both");
        assert!(
            joined.elements().iter().all(|element| !matches!(
                element,
                PathElement::CurveTo { .. } | PathElement::QuadCurveTo { .. }
            )),
            "results are polygons, never curves"
        );
    }

    #[test]
    fn stroking_a_curve_flattens_it_into_a_band() {
        let circle = Path::ellipse(Rect::new(0.0, 0.0, 10.0, 10.0));
        let band = stroking_with_width(&circle, 2.0, LineCap::Round, LineJoin::Round, 10.0, None);

        assert_eq!(band.bounding_box(), Rect::new(-1.0, -1.0, 12.0, 12.0));
        assert!(
            !contains(&band, 5.0, 5.0),
            "a closed curve strokes to a ring"
        );
        assert!(contains(&band, 0.5, 5.0));
        assert!(contains(&band, 5.0, 0.5));
        assert!(!contains(&band, 1.5, 5.0));
    }

    #[test]
    fn expanding_and_contracting_a_selection_uses_the_stroke_band() {
        // `Selection.resizeSelection(by:)`: a band `|delta|` wide on each side of the outline,
        // added or removed.
        let selection = rect(10.0, 10.0, 20.0, 20.0);
        let band = stroking_with_width(
            &selection,
            10.0,
            LineCap::Round,
            LineJoin::Round,
            10.0,
            None,
        );

        let expanded = union(&selection, &band, FillRule::Winding);
        assert_eq!(expanded.bounding_box(), Rect::new(5.0, 5.0, 30.0, 30.0));
        assert!(contains(&expanded, 5.5, 20.0));
        // Round joins round the corners off: the square's own corner would be at (5, 5).
        assert!(!contains(&expanded, 5.5, 5.5));
        assert!(contains(&expanded, 20.0, 20.0));

        let contracted = subtracting(&selection, &band, FillRule::Winding);
        assert_eq!(contracted.bounding_box(), Rect::new(15.0, 15.0, 10.0, 10.0));
        assert!(contains(&contracted, 20.0, 20.0));
        assert!(!contains(&contracted, 12.0, 20.0));
    }

    #[test]
    fn degenerate_inputs_give_empty_paths_rather_than_panicking() {
        let empty = Path::empty();
        let square = rect(0.0, 0.0, 10.0, 10.0);
        assert!(union(&empty, &empty, FillRule::Winding).is_empty());
        assert!(intersection(&empty, &square, FillRule::Winding).is_empty());
        assert!(intersection(&square, &empty, FillRule::Winding).is_empty());
        assert!(subtracting(&empty, &empty, FillRule::Winding).is_empty());
        assert!(symmetric_difference(&empty, &empty, FillRule::Winding).is_empty());
        assert!(normalized(&empty, FillRule::Winding).is_empty());

        let line = horizontal_line(0.0, 10.0, 0.0);
        for width in [0.0, -4.0, f64::NAN, f64::INFINITY] {
            assert!(
                stroking_with_width(&line, width, LineCap::Round, LineJoin::Round, 10.0, None)
                    .is_empty()
            );
        }
        // A nonsense miter limit falls back to the default rather than failing the stroke.
        assert!(
            !stroking_with_width(&line, 4.0, LineCap::Round, LineJoin::Round, f64::NAN, None)
                .is_empty()
        );

        // A vertex the fixed-point grid cannot hold drops its subpath instead of panicking.
        let mut wild = Path::empty();
        wild.move_to(Point::new(f64::INFINITY, 0.0));
        wild.add_line(Point::new(0.0, 5.0));
        assert!(
            stroking_with_width(&wild, 4.0, LineCap::Round, LineJoin::Round, 10.0, None).is_empty()
        );
        assert!(union(&wild, &wild, FillRule::Winding).is_empty());
    }
}
