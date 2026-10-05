//! Core geometry: `CGFloat`-equivalent scalars, points, sizes, rectangles and affine transforms.
//!
//! These mirror the CoreGraphics value types the Swift editor works in, including their edge cases
//! (`CGRectNull`, empty rectangles, standardized negative sizes). Document coordinates are top-left
//! origin with **y increasing downward**.

use serde::{Deserialize, Serialize};

/// `CGFloat` on a 64-bit platform.
pub type CGFloat = f64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: CGFloat,
    pub y: CGFloat,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: CGFloat, y: CGFloat) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Point) -> CGFloat {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl std::ops::Add for Point {
    type Output = Point;
    fn add(self, rhs: Point) -> Point {
        Point::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, rhs: Point) -> Point {
        Point::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Mul<CGFloat> for Point {
    type Output = Point;
    fn mul(self, rhs: CGFloat) -> Point {
        Point::new(self.x * rhs, self.y * rhs)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: CGFloat,
    pub height: CGFloat,
}

impl Size {
    pub const ZERO: Size = Size { width: 0.0, height: 0.0 };

    pub const fn new(width: CGFloat, height: CGFloat) -> Self {
        Self { width, height }
    }

    pub fn square(side: CGFloat) -> Self {
        Self::new(side, side)
    }

    /// The area as a number of pixels, for the document budget.
    pub fn pixel_count(self) -> f64 {
        (self.width * self.height).max(0.0)
    }
}

/// `CGRect` equivalent. A *null* rectangle is represented the way CoreGraphics does, with an infinite
/// origin, so `is_null`, `intersection` and `union` behave identically.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Default for Rect {
    fn default() -> Self {
        Rect::NULL
    }
}

impl Rect {
    /// `CGRectNull`.
    pub const NULL: Rect = Rect {
        origin: Point {
            x: CGFloat::INFINITY,
            y: CGFloat::INFINITY,
        },
        size: Size::ZERO,
    };

    pub const ZERO: Rect = Rect {
        origin: Point::ZERO,
        size: Size::ZERO,
    };

    pub const fn new(x: CGFloat, y: CGFloat, width: CGFloat, height: CGFloat) -> Self {
        Self {
            origin: Point::new(x, y),
            size: Size::new(width, height),
        }
    }

    pub const fn from_origin_size(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    pub const fn from_origin(origin: Point, width: CGFloat, height: CGFloat) -> Self {
        Self {
            origin,
            size: Size::new(width, height),
        }
    }

    pub fn min_x(self) -> CGFloat {
        self.origin.x
    }

    pub fn min_y(self) -> CGFloat {
        self.origin.y
    }

    pub fn max_x(self) -> CGFloat {
        if self.is_null() {
            return self.origin.x;
        }
        self.origin.x + self.size.width
    }

    pub fn max_y(self) -> CGFloat {
        if self.is_null() {
            return self.origin.y;
        }
        self.origin.y + self.size.height
    }

    pub fn mid_x(self) -> CGFloat {
        if self.is_null() {
            return self.origin.x;
        }
        if self.size.width == 0.0 {
            return self.origin.x;
        }
        self.origin.x + self.size.width / 2.0
    }

    pub fn mid_y(self) -> CGFloat {
        if self.is_null() {
            return self.origin.y;
        }
        if self.size.height == 0.0 {
            return self.origin.y;
        }
        self.origin.y + self.size.height / 2.0
    }

    pub fn width(self) -> CGFloat {
        self.size.width
    }

    pub fn height(self) -> CGFloat {
        self.size.height
    }

    /// `CGRectIsNull`.
    pub fn is_null(self) -> bool {
        self.origin.x.is_infinite() || self.origin.y.is_infinite()
    }

    /// `CGRectIsEmpty`: null, or with a non-positive side.
    pub fn is_empty(self) -> bool {
        if self.is_null() {
            return true;
        }
        !(self.size.width > 0.0 && self.size.height > 0.0)
    }

    pub fn is_infinite(self) -> bool {
        self.size.width.is_infinite() || self.size.height.is_infinite()
    }

    /// `CGRectIsInfinite`.
    pub fn standardized(self) -> Rect {
        if self.is_null() {
            return self;
        }
        Rect::new(
            if self.size.width < 0.0 {
                self.origin.x + self.size.width
            } else {
                self.origin.x
            },
            if self.size.height < 0.0 {
                self.origin.y + self.size.height
            } else {
                self.origin.y
            },
            self.size.width.abs(),
            self.size.height.abs(),
        )
    }

    /// The smallest integer rectangle containing this one (`CGRectIntegral`).
    pub fn integral(self) -> Rect {
        if self.is_null() {
            return self;
        }
        let standardized = self.standardized();
        let x = standardized.origin.x.floor();
        let y = standardized.origin.y.floor();
        let max_x = standardized.max_x().ceil();
        let max_y = standardized.max_y().ceil();
        Rect::new(x, y, max_x - x, max_y - y)
    }

    pub fn inset_by(self, dx: CGFloat, dy: CGFloat) -> Rect {
        if self.is_null() {
            return self;
        }
        Rect::new(
            self.origin.x + dx,
            self.origin.y + dy,
            (self.size.width - 2.0 * dx).max(0.0),
            (self.size.height - 2.0 * dy).max(0.0),
        )
    }

    pub fn offset_by(self, dx: CGFloat, dy: CGFloat) -> Rect {
        if self.is_null() {
            return self;
        }
        Rect::new(self.origin.x + dx, self.origin.y + dy, self.size.width, self.size.height)
    }

    pub fn intersection(self, other: Rect) -> Rect {
        if self.is_null() || other.is_null() {
            return Rect::NULL;
        }
        let x = self.origin.x.max(other.origin.x);
        let y = self.origin.y.max(other.origin.y);
        let max_x = self.max_x().min(other.max_x());
        let max_y = self.max_y().min(other.max_y());
        if max_x < x || max_y < y {
            return Rect::NULL;
        }
        Rect::new(x, y, max_x - x, max_y - y)
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_null() {
            return other;
        }
        if other.is_null() {
            return self;
        }
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.origin.x.min(other.origin.x);
        let y = self.origin.y.min(other.origin.y);
        let max_x = self.max_x().max(other.max_x());
        let max_y = self.max_y().max(other.max_y());
        Rect::new(x, y, max_x - x, max_y - y)
    }

    pub fn contains(self, point: Point) -> bool {
        !self.is_null()
            && !self.is_empty()
            && point.x >= self.min_x()
            && point.x < self.max_x()
            && point.y >= self.min_y()
            && point.y < self.max_y()
    }

    pub fn contains_rect(self, other: Rect) -> bool {
        !self.is_null()
            && !other.is_null()
            && other.min_x() >= self.min_x()
            && other.max_x() <= self.max_x()
            && other.min_y() >= self.min_y()
            && other.max_y() <= self.max_y()
    }

    pub fn intersects(self, other: Rect) -> bool {
        !self.intersection(other).is_empty()
    }

    /// `CGRectDivide` with `CGRectMinXEdge`/`CGRectMaxXEdge`/`CGRectMinYEdge`/`CGRectMaxYEdge`.
    pub fn divided_at_x(self, distance: CGFloat, from_max_edge: bool) -> (Rect, Rect) {
        if from_max_edge {
            let slice = Rect::new(self.max_x() - distance, self.origin.y, distance, self.size.height);
            let remainder = Rect::new(self.origin.x, self.origin.y, self.size.width - distance, self.size.height);
            (slice, remainder)
        } else {
            let slice = Rect::new(self.origin.x, self.origin.y, distance, self.size.height);
            let remainder = Rect::new(self.origin.x + distance, self.origin.y, self.size.width - distance, self.size.height);
            (slice, remainder)
        }
    }

    pub fn divided_at_y(self, distance: CGFloat, from_max_edge: bool) -> (Rect, Rect) {
        if from_max_edge {
            let slice = Rect::new(self.origin.x, self.max_y() - distance, self.size.width, distance);
            let remainder = Rect::new(self.origin.x, self.origin.y, self.size.width, self.size.height - distance);
            (slice, remainder)
        } else {
            let slice = Rect::new(self.origin.x, self.origin.y, self.size.width, distance);
            let remainder = Rect::new(self.origin.x, self.origin.y + distance, self.size.width, self.size.height - distance);
            (slice, remainder)
        }
    }

    /// The four corners, clockwise from the top-left.
    pub fn corners(self) -> [Point; 4] {
        [
            self.origin,
            Point::new(self.max_x(), self.min_y()),
            Point::new(self.max_x(), self.max_y()),
            Point::new(self.min_x(), self.max_y()),
        ]
    }
}

/// `CGAffineTransform`: the row-vector convention CoreGraphics uses, so `a`, `b`, `c`, `d`, `tx`, `ty`
/// mean exactly what they mean there and `concatenating` composes left-to-right.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AffineTransform {
    pub a: CGFloat,
    pub b: CGFloat,
    pub c: CGFloat,
    pub d: CGFloat,
    pub tx: CGFloat,
    pub ty: CGFloat,
}

impl Default for AffineTransform {
    fn default() -> Self {
        AffineTransform::IDENTITY
    }
}

impl AffineTransform {
    pub const IDENTITY: AffineTransform = AffineTransform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub const fn new(a: CGFloat, b: CGFloat, c: CGFloat, d: CGFloat, tx: CGFloat, ty: CGFloat) -> Self {
        Self { a, b, c, d, tx, ty }
    }

    pub fn translation(tx: CGFloat, ty: CGFloat) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    pub fn scale(sx: CGFloat, sy: CGFloat) -> Self {
        Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// Counter-clockwise for y-up spaces; in this top-left/y-down document space it turns the same way
    /// clockwise, which is what the Swift code relies on.
    pub fn rotation(radians: CGFloat) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self::new(cos, sin, -sin, cos, 0.0, 0.0)
    }

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    pub fn applying(self, point: Point) -> Point {
        Point::new(
            self.a * point.x + self.c * point.y + self.tx,
            self.b * point.x + self.d * point.y + self.ty,
        )
    }

    /// `t.concatenating(self)`: `t` applied first, then `self` (CoreGraphics argument order).
    pub fn concatenating(self, t: AffineTransform) -> AffineTransform {
        AffineTransform {
            a: t.a * self.a + t.b * self.c,
            b: t.a * self.b + t.b * self.d,
            c: t.c * self.a + t.d * self.c,
            d: t.c * self.b + t.d * self.d,
            tx: t.tx * self.a + t.ty * self.c + self.tx,
            ty: t.tx * self.b + t.ty * self.d + self.ty,
        }
    }

    /// `self` then `t` — the order Swift's `A.concatenating(B)` reads.
    pub fn then(self, t: AffineTransform) -> AffineTransform {
        t.concatenating(self)
    }

    pub fn inverted(self) -> AffineTransform {
        let determinant = self.a * self.d - self.b * self.c;
        if determinant == 0.0 {
            return AffineTransform::IDENTITY;
        }
        let inverse = 1.0 / determinant;
        return AffineTransform {
            a: self.d * inverse,
            b: -self.b * inverse,
            c: -self.c * inverse,
            d: self.a * inverse,
            tx: (self.c * self.ty - self.d * self.tx) * inverse,
            ty: (self.b * self.tx - self.a * self.ty) * inverse,
        };
    }

    pub fn scaled_by(self, sx: CGFloat, sy: CGFloat) -> AffineTransform {
        self.concatenating(AffineTransform::scale(sx, sy))
    }

    pub fn translated_by(self, tx: CGFloat, ty: CGFloat) -> AffineTransform {
        self.concatenating(AffineTransform::translation(tx, ty))
    }

    pub fn rotated_by(self, radians: CGFloat) -> AffineTransform {
        self.concatenating(AffineTransform::rotation(radians))
    }
}

/// `SIMD2<Int>` cell keys, used by the raster snapshot's spatial index.
pub type Cell = [i32; 2];

pub fn cell_of(x: CGFloat, y: CGFloat, tile: CGFloat) -> Cell {
    [(x / tile).floor() as i32, (y / tile).floor() as i32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_empty_match_coregraphics() {
        assert!(Rect::NULL.is_null());
        assert!(Rect::NULL.is_empty());
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).is_empty());
        assert!(!Rect::new(0.0, 0.0, 1.0, 1.0).is_empty());
        assert!(Rect::NULL.intersection(Rect::new(0.0, 0.0, 1.0, 1.0)).is_null());
    }

    #[test]
    fn magic_trick_and_the_transform_that_undoes_it() {
        // The Swift `placing(_:)` path relies on `unitToDocument.concatenating(old.inverted())`.
        let unit = AffineTransform::new(20.0, 0.0, 0.0, 10.0, 5.0, 7.0);
        let inverse = unit.inverted();
        let roundtrip = unit.concatenating(inverse);
        assert!(roundtrip.is_identity() || (roundtrip.a - 1.0).abs() < 1e-12);
        let p = unit.applying(Point::new(0.5, 0.5));
        assert!((p.x - 15.0).abs() < 1e-9 && (p.y - 12.0).abs() < 1e-9);
    }

    #[test]
    fn integral_grows_to_whole_pixels() {
        let r = Rect::new(1.2, -0.5, 3.0, 2.0).integral();
        assert_eq!(r, Rect::new(1.0, -1.0, 4.0, 4.0));
    }
}
