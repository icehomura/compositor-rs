//! The `CGPath` equivalent: an immutable, value-typed outline of lines and Bézier curves.
//!
//! Paths are built with the `move_to`/`add_*`/`close_subpath` builders (the `CGMutablePath`
//! surface), queried with [`Path::is_empty`], [`Path::bounding_box`] and [`Path::contains`], and
//! consumed either as [`PathElement`]s (the `applyWithBlock` surface) or as the device-space
//! polylines of [`flatten`], which `compositor-pixels::canvas` scanline-fills. A path carries no
//! fill rule, exactly like `CGPath`: the rule is passed to [`Path::contains`] and to
//! `fillPath(using:)`.
//!
//! Coordinate space is the caller's: document pixels in the model, device pixels after `flatten`
//! applies the canvas transform.

use crate::geom::{AffineTransform, CGFloat, Point, Rect};

/// `CGPathFillRule`: how the inside of a filled path is determined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    /// Non-zero winding: a point is inside when the signed crossings of the outline around it do
    /// not cancel. Core Graphics' default for `fillPath()`.
    Winding,
    /// Even-odd: a point is inside when a ray from it crosses the outline an odd number of times.
    EvenOdd,
}

/// One element of a path (`CGPathElement`), in the order it was added.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathElement {
    /// Starts a new subpath at the point.
    MoveTo(Point),
    /// A straight segment from the current point to the point.
    LineTo(Point),
    /// A quadratic segment from the current point, bending toward `control`.
    QuadCurveTo { control: Point, to: Point },
    /// A cubic segment from the current point.
    CurveTo {
        control1: Point,
        control2: Point,
        to: Point,
    },
    /// Ends the current subpath, adding the closing line back to its start point.
    CloseSubpath,
}

/// A flattened subpath in device space, ready for scanline filling ([`flatten`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Subpath {
    /// The subpath's vertices, in order. Filling closes the outline, so a consumer of a filled
    /// path adds the edge from the last vertex back to the first.
    pub points: Vec<Point>,
    /// Whether the subpath ended with an explicit [`PathElement::CloseSubpath`].
    pub closed: bool,
}

/// The flatness tolerance, in device units, that [`flatten`] approximates Bézier curves to: a
/// tenth of a pixel, well under the quarter-pixel steps of the canvas's scanline antialiasing.
pub const FLATTEN_TOLERANCE: CGFloat = 0.1;

/// The fixed recursion cap of [`flatten`]'s subdivision — at most 4096 segments per curve, so
/// pathological geometry cannot blow the point count up.
const MAX_FLATTEN_DEPTH: u32 = 12;

/// The handle length of the cubic that approximates a quarter circle or a quarter ellipse:
/// `4/3 * tan(pi/8)`.
const QUARTER_CIRCLE: CGFloat = 0.552_284_749_830_793_6;

/// A below-this threshold is a degenerate quadratic coefficient in the derivative solvers.
const DEGENERATE: CGFloat = 1e-12;

/// An outline of straight lines and Bézier curves (`CGPath`), stored exactly as Core Graphics
/// stores it: subpaths start with [`PathElement::MoveTo`] and end with
/// [`PathElement::CloseSubpath`], so a path built coordinate-for-coordinate from the Swift code
/// enumerates identically.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    elements: Vec<PathElement>,
}

impl Path {
    /// An empty path — `CGMutablePath()` before anything is added.
    pub fn empty() -> Self {
        Self::default()
    }

    /// `CGPath(rect:transform:nil)`: a rectangle as its own closed subpath.
    pub fn rect(rect: Rect) -> Self {
        let mut path = Self::empty();
        path.add_rect(rect);
        path
    }

    /// `CGPath(ellipseIn:transform:nil)`: an ellipse inscribed in the rectangle, as its own closed
    /// subpath.
    pub fn ellipse(rect: Rect) -> Self {
        let mut path = Self::empty();
        path.add_ellipse(rect);
        path
    }

    /// `CGPath(roundedRect:cornerWidth:cornerHeight:transform:nil)` with equal corner radii: a
    /// rectangle whose corners are quarter ellipses with axes `radius` by `radius`, as its own
    /// closed subpath.
    pub fn rounded_rect(rect: Rect, radius: CGFloat) -> Self {
        let mut path = Self::empty();
        path.add_rounded_rect(rect, radius, radius);
        path
    }

    /// `CGPath.move(to:)`: starts a new subpath at the point.
    pub fn move_to(&mut self, point: Point) {
        self.elements.push(PathElement::MoveTo(point));
    }

    /// `CGPath.addLine(to:)`: a straight segment from the current point.
    pub fn add_line(&mut self, point: Point) {
        self.begin_segment(point);
        self.elements.push(PathElement::LineTo(point));
    }

    /// `CGPath.addQuadCurve(to:control:)`: a quadratic segment from the current point.
    pub fn add_quad_curve(&mut self, control: Point, to: Point) {
        self.begin_segment(to);
        self.elements.push(PathElement::QuadCurveTo { control, to });
    }

    /// `CGPath.addCurve(to:control1:control2:)`: a cubic segment from the current point.
    pub fn add_curve(&mut self, control1: Point, control2: Point, to: Point) {
        self.begin_segment(to);
        self.elements.push(PathElement::CurveTo {
            control1,
            control2,
            to,
        });
    }

    /// `CGPathAddLines` (the SDK header): "Move to the first element of `points' … and append a
    /// line from each point to the next point in `points'." Like that function, this always starts
    /// a new subpath at the first point, even when the path already has a current point — it does
    /// not extend the current subpath (for that, use `add_line`). An empty slice adds nothing; a
    /// single point adds just the move.
    pub fn add_lines(&mut self, points: &[Point]) {
        let Some((first, rest)) = points.split_first() else {
            return;
        };
        self.move_to(*first);
        for point in rest {
            self.elements.push(PathElement::LineTo(*point));
        }
    }

    /// `CGPath.closeSubpath()`: closes the current subpath and ends it. Nothing is added when the
    /// path is empty, when the current subpath has no segments, or when it is already closed —
    /// and after closing, a following segment implicitly starts a new subpath at the same start
    /// point, as Core Graphics does.
    pub fn close_subpath(&mut self) {
        match self.elements.last() {
            None | Some(PathElement::MoveTo(_)) | Some(PathElement::CloseSubpath) => {}
            Some(_) => self.elements.push(PathElement::CloseSubpath),
        }
    }

    /// `CGPath.addRect(_:)`: adds a rectangular subpath, starting at the rectangle's origin and
    /// adding lines counter-clockwise (Core Graphics' y-up reading) before closing it. Negative
    /// sizes are standardized, as Core Graphics' corner construction (its documented
    /// implementation) does. A null rectangle adds nothing — Core Graphics leaves that undefined,
    /// and infinity-cornered elements would poison every consumer of the path.
    pub fn add_rect(&mut self, rect: Rect) {
        if rect.is_null() {
            return;
        }
        let rect = rect.standardized();
        let (min_x, min_y) = (rect.min_x(), rect.min_y());
        let (max_x, max_y) = (rect.max_x(), rect.max_y());
        self.elements.push(PathElement::MoveTo(Point::new(min_x, min_y)));
        self.elements
            .push(PathElement::LineTo(Point::new(max_x, min_y)));
        self.elements
            .push(PathElement::LineTo(Point::new(max_x, max_y)));
        self.elements
            .push(PathElement::LineTo(Point::new(min_x, max_y)));
        self.elements.push(PathElement::CloseSubpath);
    }

    /// `CGPath.addEllipse(in:)`: adds an elliptical subpath inscribed in the rectangle, as a
    /// closed subpath.
    ///
    /// The ellipse is Core Graphics' four-cubic approximation — the standard `4/3 * tan(pi/8)`
    /// handle — starting at the rightmost point and running right → top → left → bottom. Every
    /// control point stays inside the box, so the path's bounding box is exactly the rectangle.
    /// Negative sizes are standardized; a null rectangle adds nothing.
    pub fn add_ellipse(&mut self, rect: Rect) {
        if rect.is_null() {
            return;
        }
        let rect = rect.standardized();
        let (min_x, min_y) = (rect.min_x(), rect.min_y());
        let (max_x, max_y) = (rect.max_x(), rect.max_y());
        let (mid_x, mid_y) = (rect.mid_x(), rect.mid_y());
        let handle_x = rect.width() / 2.0 * QUARTER_CIRCLE;
        let handle_y = rect.height() / 2.0 * QUARTER_CIRCLE;
        self.elements
            .push(PathElement::MoveTo(Point::new(max_x, mid_y)));
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(max_x, mid_y - handle_y),
            control2: Point::new(mid_x + handle_x, min_y),
            to: Point::new(mid_x, min_y),
        });
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(mid_x - handle_x, min_y),
            control2: Point::new(min_x, mid_y - handle_y),
            to: Point::new(min_x, mid_y),
        });
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(min_x, mid_y + handle_y),
            control2: Point::new(mid_x - handle_x, max_y),
            to: Point::new(mid_x, max_y),
        });
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(mid_x + handle_x, max_y),
            control2: Point::new(max_x, mid_y + handle_y),
            to: Point::new(max_x, mid_y),
        });
        self.elements.push(PathElement::CloseSubpath);
    }

    /// `CGPath.addRoundedRect(in:cornerWidth:cornerHeight:)`: adds a rounded-rectangle subpath.
    ///
    /// Each corner is a quarter ellipse with the given axes, approximated by one cubic with the
    /// same handle constant as [`Path::add_ellipse`], so the path's bounding box is exactly the
    /// rectangle. The radii are clamped to half the rectangle's width and height. Negative sizes
    /// are standardized; a null rectangle adds nothing; a zero radius adds a plain rectangle.
    pub fn add_rounded_rect(&mut self, rect: Rect, corner_width: CGFloat, corner_height: CGFloat) {
        if rect.is_null() {
            return;
        }
        let rect = rect.standardized();
        let corner_width = corner_width.abs().min(rect.width() / 2.0);
        let corner_height = corner_height.abs().min(rect.height() / 2.0);
        if corner_width <= 0.0 || corner_height <= 0.0 {
            self.add_rect(rect);
            return;
        }
        let (min_x, min_y) = (rect.min_x(), rect.min_y());
        let (max_x, max_y) = (rect.max_x(), rect.max_y());
        let handle_x = corner_width * QUARTER_CIRCLE;
        let handle_y = corner_height * QUARTER_CIRCLE;
        let (left, top) = (min_x + corner_width, min_y + corner_height);
        let (right, bottom) = (max_x - corner_width, max_y - corner_height);
        self.elements
            .push(PathElement::MoveTo(Point::new(left, min_y)));
        self.elements
            .push(PathElement::LineTo(Point::new(right, min_y)));
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(right + handle_x, min_y),
            control2: Point::new(max_x, top - handle_y),
            to: Point::new(max_x, top),
        });
        self.elements
            .push(PathElement::LineTo(Point::new(max_x, bottom)));
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(max_x, bottom + handle_y),
            control2: Point::new(right + handle_x, max_y),
            to: Point::new(right, max_y),
        });
        self.elements
            .push(PathElement::LineTo(Point::new(left, max_y)));
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(left - handle_x, max_y),
            control2: Point::new(min_x, bottom + handle_y),
            to: Point::new(min_x, bottom),
        });
        self.elements
            .push(PathElement::LineTo(Point::new(min_x, top)));
        self.elements.push(PathElement::CurveTo {
            control1: Point::new(min_x, top - handle_y),
            control2: Point::new(left - handle_x, min_y),
            to: Point::new(left, min_y),
        });
        self.elements.push(PathElement::CloseSubpath);
    }

    /// `CGPath.addPath(_:)`: appends every element of `other` after this path's last element.
    pub fn append(&mut self, other: &Path) {
        self.elements.extend_from_slice(&other.elements);
    }

    /// The path's elements, in order — the `CGPath.applyWithBlock` surface.
    pub fn elements(&self) -> &[PathElement] {
        &self.elements
    }

    /// `CGPath.isEmpty`: the path contains no elements. (A path holding only a move is not empty,
    /// as in Core Graphics.)
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// `CGPath.boundingBoxOfPath`: the tight box around the path's actual geometry.
    ///
    /// Control points that fall outside their curve are *not* part of the box — that is the
    /// deprecated `boundingBox` — so an ellipse reports its box exactly, while a bowing quadratic
    /// reports the curve's own extent. Every curve's extrema are solved exactly, per axis. An
    /// empty path reports [`Rect::NULL`]; a path holding only a move reports a zero-size box at
    /// that point.
    pub fn bounding_box(&self) -> Rect {
        let mut bounds = Bounds::default();
        let mut current: Option<Point> = None;
        for element in &self.elements {
            match *element {
                PathElement::MoveTo(point) => {
                    bounds.add(point);
                    current = Some(point);
                }
                PathElement::LineTo(point) => {
                    bounds.add(point);
                    current = Some(point);
                }
                PathElement::QuadCurveTo { control, to } => {
                    if let Some(start) = current {
                        bounds.add(to);
                        let mut add_extremum = |t: CGFloat| {
                            bounds.add(quadratic_point(start, control, to, t));
                        };
                        for t in roots_in_unit_interval(
                            0.0,
                            start.x - 2.0 * control.x + to.x,
                            control.x - start.x,
                        )
                        .into_iter()
                        .flatten()
                        {
                            add_extremum(t);
                        }
                        for t in roots_in_unit_interval(
                            0.0,
                            start.y - 2.0 * control.y + to.y,
                            control.y - start.y,
                        )
                        .into_iter()
                        .flatten()
                        {
                            add_extremum(t);
                        }
                    } else {
                        bounds.add(to);
                    }
                    current = Some(to);
                }
                PathElement::CurveTo {
                    control1,
                    control2,
                    to,
                } => {
                    if let Some(start) = current {
                        bounds.add(to);
                        let mut add_extremum = |t: CGFloat| {
                            bounds.add(cubic_point(start, control1, control2, to, t));
                        };
                        for t in cubic_extrema(start.x, control1.x, control2.x, to.x)
                            .into_iter()
                            .flatten()
                        {
                            add_extremum(t);
                        }
                        for t in cubic_extrema(start.y, control1.y, control2.y, to.y)
                            .into_iter()
                            .flatten()
                        {
                            add_extremum(t);
                        }
                    } else {
                        bounds.add(to);
                    }
                    current = Some(to);
                }
                PathElement::CloseSubpath => {}
            }
        }
        bounds.rect()
    }

    /// `CGPath.copy(using:)`: the path with `transform` applied. Core Graphics' copy can fail;
    /// this one cannot, so an empty path transforms to an empty path. An affine transform maps
    /// Bézier curves exactly by mapping their control points.
    pub fn transformed(&self, transform: &AffineTransform) -> Path {
        Path {
            elements: self
                .elements
                .iter()
                .map(|element| transformed_element(*element, transform))
                .collect(),
        }
    }

    /// `CGPath.contains(_:using:)`: whether the point is inside the filled path under the given
    /// fill rule. Every subpath is treated as closed, as filling is, whether or not it ends with
    /// [`PathElement::CloseSubpath`]. Curves are flattened (see [`flatten`]), so a point within a
    /// tenth of a pixel of the outline may report either way.
    pub fn contains(&self, point: Point, rule: FillRule) -> bool {
        let mut winding = 0i32;
        let mut crossings = 0usize;
        for subpath in flatten(self, &AffineTransform::IDENTITY) {
            if subpath.points.len() < 2 {
                continue;
            }
            let count = subpath.points.len();
            for index in 0..count {
                let a = subpath.points[index];
                let b = subpath.points[(index + 1) % count];
                // Half-open span test on y, so a vertex row counts once: the ray crosses the edge
                // when it passes from below to at-or-above the row.
                if (a.y <= point.y && b.y > point.y) || (b.y <= point.y && a.y > point.y) {
                    let t = (point.y - a.y) / (b.y - a.y);
                    let x = a.x + t * (b.x - a.x);
                    if x > point.x {
                        crossings += 1;
                        winding += if b.y > a.y { 1 } else { -1 };
                    }
                }
            }
        }
        match rule {
            FillRule::Winding => winding != 0,
            FillRule::EvenOdd => crossings % 2 == 1,
        }
    }

    /// A segment needs a current point. After a close, Core Graphics implicitly starts a new
    /// subpath at the previous subpath's start point; on a completely empty path the segment's end
    /// point starts the subpath, matching `addLines(between:)` with a single point.
    fn begin_segment(&mut self, fallback: Point) {
        match self.elements.last() {
            None => self.elements.push(PathElement::MoveTo(fallback)),
            Some(PathElement::CloseSubpath) => {
                let start = self.elements.iter().rev().find_map(|element| match element {
                    PathElement::MoveTo(point) => Some(*point),
                    _ => None,
                });
                if let Some(start) = start {
                    self.elements.push(PathElement::MoveTo(start));
                }
            }
            _ => {}
        }
    }
}

/// The path's geometry as device-space polylines: `transform` is applied to every point, and each
/// `MoveTo` starts a new [`Subpath`]. Curves are flattened *after* transforming, so the tolerance
/// is in device units.
///
/// Curves use a fixed, deterministic subdivision: each segment is split at `t = 1/2` (de Casteljau)
/// until both control points lie within [`FLATTEN_TOLERANCE`] (0.1 device units) of the chord, or
/// the fixed depth cap of [`MAX_FLATTEN_DEPTH`] (12) is reached. A fixed segment *count* instead
/// would make the approximation coarser with the curve's size, which document-scale outlines would
/// notice; a fixed *tolerance* keeps the same sub-pixel accuracy everywhere.
///
/// `closed` mirrors an explicit `CloseSubpath`: the consumer treats every subpath as implicitly
/// closed when filling, as Core Graphics does.
pub fn flatten(path: &Path, transform: &AffineTransform) -> Vec<Subpath> {
    let mut subpaths: Vec<Subpath> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    let mut closed = false;
    for element in &path.elements {
        match *element {
            PathElement::MoveTo(point) => {
                flush(&mut subpaths, &mut points, &mut closed);
                points.push(transform.applying(point));
            }
            PathElement::LineTo(point) => points.push(transform.applying(point)),
            PathElement::QuadCurveTo { control, to } => {
                let end = transform.applying(to);
                match points.last().copied() {
                    Some(start) => {
                        flatten_quadratic(&mut points, start, transform.applying(control), end);
                    }
                    None => points.push(end),
                }
            }
            PathElement::CurveTo {
                control1,
                control2,
                to,
            } => {
                let end = transform.applying(to);
                match points.last().copied() {
                    Some(start) => flatten_cubic(
                        &mut points,
                        start,
                        transform.applying(control1),
                        transform.applying(control2),
                        end,
                        0,
                    ),
                    None => points.push(end),
                }
            }
            PathElement::CloseSubpath => closed = true,
        }
    }
    flush(&mut subpaths, &mut points, &mut closed);
    subpaths
}

/// Ends the current subpath, when it has any vertices, and starts a fresh one.
fn flush(subpaths: &mut Vec<Subpath>, points: &mut Vec<Point>, closed: &mut bool) {
    if !points.is_empty() {
        subpaths.push(Subpath {
            points: std::mem::take(points),
            closed: *closed,
        });
    }
    *closed = false;
}

/// Flattens a quadratic segment, appending its subdivision to `out` (which already ends with
/// `p0`). The quadratic is elevated to the equivalent cubic and flattened as one.
fn flatten_quadratic(out: &mut Vec<Point>, p0: Point, control: Point, p1: Point) {
    let control1 = Point::new(
        p0.x + (control.x - p0.x) * 2.0 / 3.0,
        p0.y + (control.y - p0.y) * 2.0 / 3.0,
    );
    let control2 = Point::new(
        p1.x + (control.x - p1.x) * 2.0 / 3.0,
        p1.y + (control.y - p1.y) * 2.0 / 3.0,
    );
    flatten_cubic(out, p0, control1, control2, p1, 0);
}

/// Flattens a cubic segment, appending its subdivision to `out` (which already ends with `p0`).
fn flatten_cubic(out: &mut Vec<Point>, p0: Point, p1: Point, p2: Point, p3: Point, depth: u32) {
    if depth >= MAX_FLATTEN_DEPTH || cubic_is_flat(p0, p1, p2, p3) {
        out.push(p3);
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let middle = midpoint(p012, p123);
    flatten_cubic(out, p0, p01, p012, middle, depth + 1);
    flatten_cubic(out, middle, p123, p23, p3, depth + 1);
}

/// Whether a cubic segment is flat enough to be replaced by its chord: both control points lie
/// within [`FLATTEN_TOLERANCE`] of the chord (or of `p0`, when the chord is degenerate).
fn cubic_is_flat(p0: Point, p1: Point, p2: Point, p3: Point) -> bool {
    let dx = p3.x - p0.x;
    let dy = p3.y - p0.y;
    let chord = (dx * dx + dy * dy).sqrt();
    let distance = |point: Point| {
        if chord == 0.0 {
            point.distance(p0)
        } else {
            ((point.x - p0.x) * dy - (point.y - p0.y) * dx).abs() / chord
        }
    };
    distance(p1).max(distance(p2)) <= FLATTEN_TOLERANCE
}

fn midpoint(a: Point, b: Point) -> Point {
    Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

fn quadratic_point(p0: Point, control: Point, p1: Point, t: CGFloat) -> Point {
    let u = 1.0 - t;
    let (uu, uv, vv) = (u * u, u * t, t * t);
    Point::new(
        uu * p0.x + 2.0 * uv * control.x + vv * p1.x,
        uu * p0.y + 2.0 * uv * control.y + vv * p1.y,
    )
}

fn cubic_point(p0: Point, p1: Point, p2: Point, p3: Point, t: CGFloat) -> Point {
    let u = 1.0 - t;
    let (uuu, uuv, uvv, vvv) = (u * u * u, u * u * t, u * t * t, t * t * t);
    Point::new(
        uuu * p0.x + 3.0 * uuv * p1.x + 3.0 * uvv * p2.x + vvv * p3.x,
        uuu * p0.y + 3.0 * uuv * p1.y + 3.0 * uvv * p2.y + vvv * p3.y,
    )
}

/// The parameters strictly inside `(0, 1)` at which a cubic Bézier's coordinate is extremal: the
/// roots of `3 * (A t² + B t + C)` with `A = c1 - p0`, `B = c2 - c1`, `C = p3 - c2`.
fn cubic_extrema(p0: CGFloat, c1: CGFloat, c2: CGFloat, p3: CGFloat) -> [Option<CGFloat>; 2] {
    let a = c1 - p0;
    let b = c2 - c1;
    let c = p3 - c2;
    roots_in_unit_interval(a - 2.0 * b + c, 2.0 * (b - a), a)
}

/// The real roots of `a t² + b t + c` strictly inside `(0, 1)`, padded with `None`; the quadratic
/// coefficient `a` may be zero (a quadratic curve's linear derivative, or a cubic's flat one).
fn roots_in_unit_interval(a: CGFloat, b: CGFloat, c: CGFloat) -> [Option<CGFloat>; 2] {
    let in_unit = |t: CGFloat| (t > 0.0 && t < 1.0).then_some(t);
    if a.abs() <= DEGENERATE {
        return if b.abs() <= DEGENERATE {
            [None, None]
        } else {
            [in_unit(-c / b), None]
        };
    }
    let discriminant = b * b - 4.0 * a * c;
    if discriminant < 0.0 {
        return [None, None];
    }
    let root = discriminant.sqrt();
    [
        in_unit((-b - root) / (2.0 * a)),
        in_unit((-b + root) / (2.0 * a)),
    ]
}

fn transformed_element(element: PathElement, transform: &AffineTransform) -> PathElement {
    match element {
        PathElement::MoveTo(point) => PathElement::MoveTo(transform.applying(point)),
        PathElement::LineTo(point) => PathElement::LineTo(transform.applying(point)),
        PathElement::QuadCurveTo { control, to } => PathElement::QuadCurveTo {
            control: transform.applying(control),
            to: transform.applying(to),
        },
        PathElement::CurveTo {
            control1,
            control2,
            to,
        } => PathElement::CurveTo {
            control1: transform.applying(control1),
            control2: transform.applying(control2),
            to: transform.applying(to),
        },
        PathElement::CloseSubpath => PathElement::CloseSubpath,
    }
}

/// The extreme coordinates of a path's geometry, for [`Path::bounding_box`].
#[derive(Default)]
struct Bounds {
    min_x: CGFloat,
    min_y: CGFloat,
    max_x: CGFloat,
    max_y: CGFloat,
    any: bool,
}

impl Bounds {
    fn add(&mut self, point: Point) {
        if !self.any {
            self.min_x = point.x;
            self.min_y = point.y;
            self.max_x = point.x;
            self.max_y = point.y;
            self.any = true;
            return;
        }
        self.min_x = self.min_x.min(point.x);
        self.min_y = self.min_y.min(point.y);
        self.max_x = self.max_x.max(point.x);
        self.max_y = self.max_y.max(point.y);
    }

    /// The box, or `Rect::NULL` when nothing was added, as `CGPathGetPathBoundingBox` reports.
    fn rect(&self) -> Rect {
        if !self.any {
            return Rect::NULL;
        }
        Rect::new(
            self.min_x,
            self.min_y,
            self.max_x - self.min_x,
            self.max_y - self.min_y,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_near(actual: Rect, expected: Rect, tolerance: CGFloat) {
        assert!(
            (actual.min_x() - expected.min_x()).abs() <= tolerance
                && (actual.min_y() - expected.min_y()).abs() <= tolerance
                && (actual.max_x() - expected.max_x()).abs() <= tolerance
                && (actual.max_y() - expected.max_y()).abs() <= tolerance,
            "{actual:?} is not within {tolerance} of {expected:?}"
        );
    }

    #[test]
    fn an_empty_path_has_null_bounds_and_contains_nothing() {
        let path = Path::empty();
        assert!(path.is_empty());
        assert!(path.elements().is_empty());
        assert_eq!(path.bounding_box(), Rect::NULL);
        assert!(!path.contains(Point::new(0.0, 0.0), FillRule::Winding));
        assert!(!path.contains(Point::new(0.0, 0.0), FillRule::EvenOdd));
    }

    #[test]
    fn a_lone_move_is_not_empty_but_has_a_degenerate_box() {
        let mut path = Path::empty();
        path.move_to(Point::new(7.0, 8.0));
        assert!(!path.is_empty());
        assert_eq!(path.bounding_box(), Rect::new(7.0, 8.0, 0.0, 0.0));
        assert!(!path.contains(Point::new(7.0, 8.0), FillRule::Winding));
    }

    #[test]
    fn rect_bounds_are_exact_and_start_at_the_origin_corner() {
        let rect = Rect::new(20.0, 30.0, 40.0, 41.0);
        let path = Path::rect(rect);
        assert!(!path.is_empty());
        assert_eq!(path.bounding_box(), rect);
        assert_eq!(path.elements().len(), 5);
        assert_eq!(path.elements()[0], PathElement::MoveTo(Point::new(20.0, 30.0)));
        assert_eq!(path.elements()[1], PathElement::LineTo(Point::new(60.0, 30.0)));
        assert_eq!(path.elements()[4], PathElement::CloseSubpath);
        assert!(path.contains(Point::new(40.0, 50.0), FillRule::Winding));
        assert!(path.contains(Point::new(21.0, 31.0), FillRule::EvenOdd));
        assert!(!path.contains(Point::new(19.0, 50.0), FillRule::Winding));
        assert!(!path.contains(Point::new(60.5, 50.0), FillRule::Winding));
        assert!(!path.contains(Point::new(40.0, 71.5), FillRule::EvenOdd));
    }

    #[test]
    fn negative_size_rects_are_standardized() {
        let path = Path::rect(Rect::new(60.0, 71.0, -40.0, -41.0));
        assert_eq!(path.bounding_box(), Rect::new(20.0, 30.0, 40.0, 41.0));
    }

    #[test]
    fn null_rectangles_add_nothing_and_empty_ones_degenerate() {
        assert!(Path::rect(Rect::NULL).is_empty());
        assert!(Path::ellipse(Rect::NULL).is_empty());
        assert!(Path::rounded_rect(Rect::NULL, 4.0).is_empty());
        // A non-null rectangle is always added, even with no area: the subpath degenerates and its
        // box comes out empty, which `DocumentSelection.isEmpty` and the lasso's size guard see.
        let flat = Path::rect(Rect::new(5.0, 5.0, 0.0, 10.0));
        assert!(!flat.is_empty());
        assert!(flat.bounding_box().is_empty());
        let squashed = Path::ellipse(Rect::new(5.0, 5.0, 10.0, 0.0));
        assert!(!squashed.is_empty());
        assert!(squashed.bounding_box().is_empty());
        let point = Path::rounded_rect(Rect::new(0.0, 0.0, 0.0, 0.0), 4.0);
        assert!(!point.is_empty());
        assert!(point.bounding_box().is_empty());
    }

    #[test]
    fn ellipse_bounds_are_its_box_and_its_control_points_stay_inside() {
        let rect = Rect::new(10.0, 20.0, 60.0, 40.0);
        let path = Path::ellipse(rect);
        assert_eq!(path.bounding_box(), rect);
        // Right → top → left → bottom: four cubics and a close, as Core Graphics adds them.
        assert_eq!(path.elements().len(), 6);
        assert_eq!(path.elements()[0], PathElement::MoveTo(Point::new(70.0, 40.0)));
        let cardinal = [
            Point::new(40.0, 20.0),
            Point::new(10.0, 40.0),
            Point::new(40.0, 60.0),
            Point::new(70.0, 40.0),
        ];
        for (element, end) in path.elements()[1..5].iter().zip(cardinal) {
            match *element {
                PathElement::CurveTo { control1, control2, to } => {
                    assert_eq!(to, end);
                    // The handle constant keeps every control point inside the box.
                    for control in [control1, control2] {
                        assert!(
                            control.x >= 10.0 && control.x <= 70.0 && control.y >= 20.0 && control.y <= 60.0,
                            "{control:?} leaves the ellipse's box"
                        );
                    }
                }
                other => panic!("expected a cubic, got {other:?}"),
            }
        }
        assert!(path.contains(Point::new(40.0, 40.0), FillRule::Winding));
        assert!(path.contains(Point::new(11.0, 40.0), FillRule::Winding));
        assert!(!path.contains(Point::new(10.0, 20.0), FillRule::Winding));
        assert!(!path.contains(Point::new(40.0, 19.0), FillRule::Winding));
        assert!(!path.contains(Point::new(40.0, 61.0), FillRule::EvenOdd));
        assert_eq!(
            Path::ellipse(Rect::new(0.0, 0.0, 100.0, 25.0)).bounding_box(),
            Rect::new(0.0, 0.0, 100.0, 25.0)
        );
    }

    #[test]
    fn rounded_rect_bounds_are_its_box_even_when_the_radii_clamp() {
        let rect = Rect::new(4.0, 8.0, 50.0, 20.0);
        assert_eq!(Path::rounded_rect(rect, 6.0).bounding_box(), rect);
        assert_eq!(Path::rounded_rect(rect, 500.0).bounding_box(), rect);
        // A zero radius degenerates to a plain rectangle.
        assert_eq!(
            Path::rounded_rect(rect, 0.0).elements(),
            Path::rect(rect).elements()
        );
        // The corners cut the box: a corner point is outside, the middle of an edge is inside.
        let rounded = Path::rounded_rect(rect, 6.0);
        assert!(!rounded.contains(Point::new(4.5, 8.5), FillRule::Winding));
        assert!(rounded.contains(Point::new(29.0, 9.0), FillRule::Winding));
    }

    #[test]
    fn a_quadratic_is_bounded_by_its_curve_not_its_control_point() {
        let mut path = Path::empty();
        path.move_to(Point::new(0.0, 0.0));
        path.add_quad_curve(Point::new(5.0, 100.0), Point::new(10.0, 0.0));
        // The apex is halfway to the control point; control-point bounds would report 100.
        assert_eq!(path.bounding_box(), Rect::new(0.0, 0.0, 10.0, 50.0));
    }

    #[test]
    fn a_cubic_is_bounded_by_its_curve() {
        let mut path = Path::empty();
        path.move_to(Point::new(0.0, 0.0));
        path.add_curve(
            Point::new(0.0, 10.0),
            Point::new(10.0, 10.0),
            Point::new(10.0, 0.0),
        );
        // The derivative solves to one extremum at t = 1/2, where y is 7.5, not 10.
        assert_eq!(path.bounding_box(), Rect::new(0.0, 0.0, 10.0, 7.5));
    }

    #[test]
    fn transformed_round_trips_through_the_inverse() {
        let mut path = Path::empty();
        path.move_to(Point::new(1.0, 2.0));
        path.add_line(Point::new(5.0, 6.0));
        path.add_quad_curve(Point::new(8.0, 4.0), Point::new(9.0, 1.0));
        path.add_curve(
            Point::new(10.0, 0.0),
            Point::new(12.0, 3.0),
            Point::new(14.0, 2.0),
        );
        path.close_subpath();

        let transform = AffineTransform::translation(10.0, -5.0)
            .concatenating(AffineTransform::scale(2.0, 2.0));
        let moved = path.transformed(&transform);
        assert_eq!(moved.transformed(&transform.inverted()), path);

        let bounds = path.bounding_box();
        assert_eq!(
            moved.bounding_box(),
            Rect::new(
                bounds.min_x() * 2.0 + 10.0,
                bounds.min_y() * 2.0 - 5.0,
                bounds.width() * 2.0,
                bounds.height() * 2.0,
            )
        );

        let turned = Path::ellipse(Rect::new(10.0, 20.0, 30.0, 50.0))
            .transformed(&AffineTransform::rotation(std::f64::consts::FRAC_PI_2));
        // A quarter turn swaps the axes exactly; the tolerance covers the four-cubic ellipse's
        // 0.03% radial error, which moves the box by well under a tenth of a pixel.
        assert_near(
            turned.bounding_box(),
            Rect::new(-70.0, 10.0, 50.0, 30.0),
            0.05,
        );
    }

    #[test]
    fn nested_rectangles_agree_with_the_fill_rules() {
        let mut path = Path::rect(Rect::new(0.0, 0.0, 30.0, 30.0));
        path.add_rect(Rect::new(10.0, 10.0, 10.0, 10.0));
        // Both subpaths wind the same way: the inner rectangle is filled under non-zero winding
        // (winding 2) and punched out under even-odd.
        assert!(path.contains(Point::new(15.0, 15.0), FillRule::Winding));
        assert!(!path.contains(Point::new(15.0, 15.0), FillRule::EvenOdd));
        assert!(path.contains(Point::new(5.0, 5.0), FillRule::Winding));
        assert!(path.contains(Point::new(5.0, 5.0), FillRule::EvenOdd));
        assert!(!path.contains(Point::new(35.0, 15.0), FillRule::EvenOdd));
    }

    #[test]
    fn a_reversed_inner_rectangle_punches_a_hole_under_winding() {
        let mut path = Path::rect(Rect::new(0.0, 0.0, 30.0, 30.0));
        path.move_to(Point::new(10.0, 10.0));
        path.add_line(Point::new(10.0, 20.0));
        path.add_line(Point::new(20.0, 20.0));
        path.add_line(Point::new(20.0, 10.0));
        path.close_subpath();
        assert!(!path.contains(Point::new(15.0, 15.0), FillRule::Winding));
        assert!(!path.contains(Point::new(15.0, 15.0), FillRule::EvenOdd));
        assert!(path.contains(Point::new(5.0, 5.0), FillRule::Winding));
    }

    #[test]
    fn an_open_subpath_is_closed_when_filling() {
        let mut path = Path::empty();
        path.add_lines(&[Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(1.0, 1.0)]);
        assert!(path.contains(Point::new(0.9, 0.1), FillRule::Winding));
        assert!(!path.contains(Point::new(2.0, 2.0), FillRule::Winding));
    }

    #[test]
    fn flattening_a_rectangle_is_a_closed_square() {
        let subpaths = flatten(
            &Path::rect(Rect::new(2.0, 4.0, 6.0, 8.0)),
            &AffineTransform::IDENTITY,
        );
        assert_eq!(subpaths.len(), 1);
        assert!(subpaths[0].closed);
        assert_eq!(
            subpaths[0].points,
            vec![
                Point::new(2.0, 4.0),
                Point::new(8.0, 4.0),
                Point::new(8.0, 12.0),
                Point::new(2.0, 12.0),
            ]
        );
    }

    #[test]
    fn flattening_marks_closed_subpaths_and_applies_the_transform() {
        let mut path = Path::empty();
        path.move_to(Point::new(0.0, 0.0));
        path.add_line(Point::new(4.0, 0.0));
        path.close_subpath();
        path.move_to(Point::new(1.0, 1.0));
        path.add_line(Point::new(2.0, 2.0));
        let subpaths = flatten(&path, &AffineTransform::scale(2.0, 2.0));
        assert_eq!(subpaths.len(), 2);
        assert!(subpaths[0].closed);
        assert!(!subpaths[1].closed);
        assert_eq!(subpaths[0].points, vec![Point::new(0.0, 0.0), Point::new(8.0, 0.0)]);
        assert_eq!(subpaths[1].points, vec![Point::new(2.0, 2.0), Point::new(4.0, 4.0)]);
    }

    #[test]
    fn flattened_ellipse_vertices_lie_on_the_ellipse() {
        let rect = Rect::new(10.0, 20.0, 60.0, 40.0);
        let subpaths = flatten(&Path::ellipse(rect), &AffineTransform::IDENTITY);
        assert_eq!(subpaths.len(), 1);
        assert!(subpaths[0].closed);
        assert!(
            subpaths[0].points.len() > 16,
            "four cubics flatten to more than sixteen vertices"
        );
        // Every vertex is a point of the curve, so the ellipse equation holds on it; the four-cubic
        // approximation is a little tighter than the true ellipse (a 0.03% radial error).
        let (center_x, center_y, radius_x, radius_y) = (40.0, 40.0, 30.0, 20.0);
        for point in &subpaths[0].points {
            let value = ((point.x - center_x) / radius_x).powi(2)
                + ((point.y - center_y) / radius_y).powi(2);
            assert!(
                (value - 1.0).abs() < 0.001,
                "{point:?} is off the ellipse ({value})"
            );
        }
    }

    #[test]
    fn add_lines_moves_to_its_first_point_even_with_a_current_point() {
        // `CGPathAddLines`: "Move to the first element of `points' … and append a line from each
        // point to the next point in `points'." It never extends the current subpath, so a polyline
        // that must continue the current point is written as `move_to` + `add_line` per point —
        // which is also exactly the element stream this produces.
        let mut path = Path::empty();
        path.move_to(Point::new(1.0, 1.0));
        path.add_lines(&[Point::new(4.0, 1.0), Point::new(4.0, 4.0)]);
        assert_eq!(path.elements().len(), 3);
        assert_eq!(path.elements()[0], PathElement::MoveTo(Point::new(1.0, 1.0)));
        assert_eq!(path.elements()[1], PathElement::MoveTo(Point::new(4.0, 1.0)));
        assert_eq!(path.elements()[2], PathElement::LineTo(Point::new(4.0, 4.0)));
        let subpaths = flatten(&path, &AffineTransform::IDENTITY);
        assert_eq!(subpaths.len(), 2);
        assert_eq!(subpaths[0].points, vec![Point::new(1.0, 1.0)]);
        assert_eq!(
            subpaths[1].points,
            vec![Point::new(4.0, 1.0), Point::new(4.0, 4.0)]
        );

        // A polyline that must continue the current point is written as `move_to` + `add_line`:
        // that keeps one subpath, where `add_lines` breaks it at the first point.
        let mut spelled_out = Path::empty();
        spelled_out.move_to(Point::new(1.0, 1.0));
        spelled_out.add_line(Point::new(4.0, 1.0));
        spelled_out.add_line(Point::new(4.0, 4.0));
        assert_eq!(spelled_out.elements().len(), 3);
        assert_eq!(
            spelled_out.elements()[1],
            PathElement::LineTo(Point::new(4.0, 1.0))
        );
        assert_eq!(flatten(&spelled_out, &AffineTransform::IDENTITY).len(), 1);
    }

    #[test]
    fn a_segment_after_closing_starts_a_new_subpath_at_the_start() {
        let mut path = Path::empty();
        path.move_to(Point::new(1.0, 1.0));
        path.add_line(Point::new(4.0, 1.0));
        path.close_subpath();
        path.add_line(Point::new(1.0, 5.0));
        assert_eq!(path.elements().len(), 5);
        assert_eq!(path.elements()[3], PathElement::MoveTo(Point::new(1.0, 1.0)));
        assert_eq!(path.elements()[4], PathElement::LineTo(Point::new(1.0, 5.0)));
        let subpaths = flatten(&path, &AffineTransform::IDENTITY);
        assert_eq!(subpaths.len(), 2);
        assert!(subpaths[0].closed && !subpaths[1].closed);
    }

    #[test]
    fn closing_is_idempotent_and_needs_a_segment() {
        let mut path = Path::empty();
        path.close_subpath();
        path.move_to(Point::new(0.0, 0.0));
        path.close_subpath();
        assert_eq!(path.elements().len(), 1);
        assert_eq!(path.elements()[0], PathElement::MoveTo(Point::new(0.0, 0.0)));
        path.add_line(Point::new(1.0, 0.0));
        path.close_subpath();
        path.close_subpath();
        assert_eq!(path.elements().len(), 3);
    }

    #[test]
    fn appending_joins_the_element_streams() {
        let mut path = Path::rect(Rect::new(0.0, 0.0, 4.0, 4.0));
        path.append(&Path::rect(Rect::new(10.0, 10.0, 4.0, 4.0)));
        assert_eq!(path.elements().len(), 10);
        assert_eq!(path.bounding_box(), Rect::new(0.0, 0.0, 14.0, 14.0));
        assert_eq!(flatten(&path, &AffineTransform::IDENTITY).len(), 2);
        assert!(!path.contains(Point::new(7.0, 7.0), FillRule::Winding));
    }
}
