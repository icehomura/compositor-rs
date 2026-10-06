//! Smudge and Liquify strokes and free distortion.
//!
//! Two Swift files describe a Smudge or Liquify stroke: `Document/SmudgeLiquify.swift`'s
//! `WarpStroke`, which also carries a CPU dab path, and `Rendering/MetalWarp.swift`'s MSL kernels,
//! which is where the dabs actually ran whenever Metal was available — i.e. on every machine this
//! app shipped to. This module is a 1:1 port of the MSL kernels (`warp_pick_up`, `warp_smudge`,
//! `warp_copy`, `warp_clear`, `warp_push`) into CPU kernels, so the numbers here are the numbers the
//! GPU produced: `f32` carried colors and offsets, the same clamps, the same evaluation order, the
//! same unorm8 conversions. The two paths differ only in `push`: the GPU moves a *source offset* a
//! pixel and draws each dab afresh from the untouched layer, where the Swift's CPU dabs moved the
//! pixels themselves and resampled them again at every dab, softening them a little each time.
//!
//! The working copy is the layer as the canvas showed it when the stroke began, at document size.
//! The GPU kept it in a texture, drew the canvas from it and read it back once, at the end; here it
//! is the `Rgba8Image` itself, so `image` hands it out directly and no commit or read-back exists.
//!
//! `DistortWarp` is `Document/Distort.swift`: the perspective mapping of the unit square onto four
//! corners, `CIPerspectiveTransform`'s resampling and `PixelAdjust.render` done here on the CPU, the
//! two triangles a folded shape is drawn as, and the trims and mask backgrounds around them.

use std::sync::Arc;

use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::limits::{max_surface_megapixels, MAX_SIDE_EXTENT, MAX_SURFACE_EXTENT};
use compositor_rs_core::path::{FillRule, Path, PathElement};
use compositor_rs_core::{CoreError, Gray8Image, Rgba8Image};

use crate::canvas::Canvas;
use crate::raster::Raster;

/// The Brush tool's modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrushToolMode {
    Paint,
    Erase,
}

impl BrushToolMode {
    pub const ALL: [BrushToolMode; 2] = [BrushToolMode::Paint, BrushToolMode::Erase];

    pub fn raw_value(self) -> &'static str {
        match self {
            BrushToolMode::Paint => "Paint",
            BrushToolMode::Erase => "Erase",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// The Blur tool's modes. Smudge and Liquify push the active layer's pixels around under the brush.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlurToolMode {
    Liquify,
    Blur,
    Smudge,
}

impl BlurToolMode {
    pub const ALL: [BlurToolMode; 3] = [
        BlurToolMode::Liquify,
        BlurToolMode::Blur,
        BlurToolMode::Smudge,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            BlurToolMode::Liquify => "Liquify",
            BlurToolMode::Blur => "Blur",
            BlurToolMode::Smudge => "Smudge",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// How much a dab moves pixels at a distance `u` (0 center, 1 rim) from its center.
fn weight(u: f32, hardness: f32) -> f32 {
    if u >= 1.0 {
        return 0.0;
    }
    if u <= hardness {
        return 1.0;
    }
    let t = (1.0 - u) / (1.0 - hardness);
    t * t * (3.0 - 2.0 * t)
}

/// `mix(a, b, t)`, as MSL's.
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// `clamp(round(value), 0, 255)`: the MSL kernels' unorm8 write.
fn byte(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// A Smudge or Liquify stroke in progress. It works on the active layer as the canvas shows it, at
/// document size, changing it dab by dab; the canvas shows that working copy in place of the layer.
/// When the stroke ends, the result is painted into the layer's own pixels along the stroke's path
/// (see `EditorSession.finish_warp`).
pub struct WarpStroke {
    mode: BlurToolMode,
    diameter: f64,
    hardness: f64,
    strength: f64,
    width: usize,
    height: usize,
    /// The working copy the dabs change: the layer as it was shown when the stroke began.
    pixels: Rgba8Image,
    /// Every dab's center, for painting the result into the layer.
    points: Vec<Point>,
    last: Option<Point>,
    /// Smudge: the color the brush carries, a (2r+1)² RGBA square, in 0…255.
    carried: Vec<f32>,
    /// Liquify: the layer as the stroke found it, and how far each pixel has moved from it — a source
    /// offset a pixel, in pixels. Each dab moves the offsets, never the pixels, and a pixel is drawn
    /// afresh from the untouched ones through its offset; resampled at every dab instead, as the
    /// pixels themselves were, they softened a little each time, where Photoshop's Liquify keeps them
    /// sharp.
    original: Option<Rgba8Image>,
    offsets: Vec<[f32; 2]>,
    /// Liquify: the offsets in the dab's area as they were before the dab, which the dab reads.
    scratch: Vec<[f32; 2]>,
}

impl WarpStroke {
    /// `WarpStroke.init`: the working copy — the layer drawn through its transform into a
    /// document-size context — with the brush's diameter, hardness and strength.
    ///
    /// The Swift built that context here (and handed it to Metal); the port's caller draws it with
    /// the renderer and passes it in.
    pub fn new(pixels: Rgba8Image, mode: BlurToolMode, diameter: f64, hardness: f64, opacity: f64) -> Self {
        let diameter = diameter.max(2.0);
        let hardness = hardness.max(0.0).min(0.98);
        let strength = opacity.max(0.01).min(1.0);
        let (width, height) = (pixels.width(), pixels.height());
        Self {
            mode,
            diameter,
            hardness,
            strength,
            width,
            height,
            pixels,
            points: Vec::new(),
            last: None,
            carried: Vec::new(),
            original: None,
            offsets: Vec::new(),
            scratch: Vec::new(),
        }
    }

    pub fn mode(&self) -> BlurToolMode {
        self.mode
    }

    /// The diameter the stroke runs at, `max(2, settings.diameter)`.
    pub fn diameter(&self) -> f64 {
        self.diameter
    }

    /// Every dab's center, for painting the result into the layer.
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// The working copy as an image: where the dabs sent so far have taken it.
    pub fn image(&self) -> &Rgba8Image {
        &self.pixels
    }

    fn radius(&self) -> i64 {
        (self.diameter / 2.0).ceil() as i64
    }

    /// How much a dab moves pixels at a distance `u` (0 center, 1 rim) from its center.
    fn dab_weight(&self, u: f32) -> f32 {
        weight(u, self.hardness as f32)
    }

    /// Continues the stroke to `point`, dabbing along the way.
    pub fn append(&mut self, point: Point) {
        let Some(from) = self.last else {
            self.last = Some(point);
            if self.mode == BlurToolMode::Smudge {
                self.pick_up(point);
            }
            return;
        };
        let distance = point.distance(from);
        // Smudge drags the pixels one dab's spacing at a time and mixes them with what's there:
        // spaced widely, each step left a faint copy of what it dragged, echoes along the stroke. A
        // pixel apart (a little more for a huge brush) the steps run together into one smear, as
        // Photoshop's does.
        let spacing = 1.0f64.max(self.diameter * if self.mode == BlurToolMode::Smudge { 0.005 } else { 0.025 });
        if distance < spacing {
            return;
        }
        let steps = ((distance / spacing).ceil() as i64).max(1);
        let mut previous = from;
        for step in 1..=steps {
            let t = step as f64 / steps as f64;
            let next = Point::new(
                from.x + (point.x - from.x) * t,
                from.y + (point.y - from.y) * t,
            );
            if self.mode == BlurToolMode::Smudge {
                self.smudge(next);
            } else {
                self.push(previous, next);
            }
            self.points.push(next);
            previous = next;
        }
        self.last = Some(point);
    }

    /// `warp_pick_up`: the pixels under the brush, into the square it carries. Outside the canvas is
    /// transparent.
    fn pick_up(&mut self, center: Point) {
        let radius = self.radius();
        let side = (2 * radius + 1) as usize;
        self.carried = vec![0.0; side * side * 4];
        let cx = center.x.round() as i64;
        let cy = center.y.round() as i64;
        for grid_y in 0..side {
            let y = cy + grid_y as i64 - radius;
            if y < 0 || y >= self.height as i64 {
                continue;
            }
            for grid_x in 0..side {
                let x = cx + grid_x as i64 - radius;
                if x < 0 || x >= self.width as i64 {
                    continue;
                }
                let source = self.pixels.get(x as usize, y as usize);
                let carried = (grid_y * side + grid_x) * 4;
                for channel in 0..4 {
                    self.carried[carried + channel] = source[channel] as f32;
                }
            }
        }
    }

    /// `warp_smudge`: what was under the brush at the last dab, laid down here at the smudge's
    /// strength; the brush then carries what it just left, and nothing older.
    fn smudge(&mut self, center: Point) {
        let radius = self.radius();
        let side = (2 * radius + 1) as usize;
        if self.carried.len() < side * side * 4 {
            return;
        }
        let cx = center.x.round() as i64;
        let cy = center.y.round() as i64;
        let keep = self.strength as f32;
        let inverse_radius = 1.0f32 / (self.diameter / 2.0) as f32;
        for grid_y in 0..side {
            let y = cy + grid_y as i64 - radius;
            if y < 0 || y >= self.height as i64 {
                continue;
            }
            for grid_x in 0..side {
                let x = cx + grid_x as i64 - radius;
                if x < 0 || x >= self.width as i64 {
                    continue;
                }
                let dx = grid_x as i64 - radius;
                let dy = grid_y as i64 - radius;
                let w = self.dab_weight(((dx * dx + dy * dy) as f32).sqrt() * inverse_radius);
                if w <= 0.0 {
                    continue;
                }
                let carried = (grid_y * side + grid_x) * 4;
                let mut pixel = self.pixels.get(x as usize, y as usize);
                for channel in 0..4 {
                    let under = pixel[channel] as f32;
                    // What was under the brush at the last dab, laid down here at the smudge's
                    // strength, as Photoshop does: all of it drags the pixels along; less mixes them
                    // with what's here, softening the trail.
                    let painted = under + (self.carried[carried + channel] - under) * w * keep;
                    pixel[channel] = byte(painted);
                    // The brush then carries what it just left — the unrounded value, as the write to
                    // the carried texture was — and nothing older: holding on to what it picked up at
                    // the start stamped it again at every dab, a trail of ghost copies.
                    self.carried[carried + channel] = painted;
                }
                self.pixels.set(x as usize, y as usize, pixel);
            }
        }
    }

    /// Forward warp, as `warp_push`: what's under the brush moves with it, most at its center, fading
    /// to none at its rim — worked on the offsets, with the pixels under the dab drawn again from the
    /// untouched ones. The offset a pixel takes is the one found behind the brush's travel, less the
    /// travel; its color is the untouched layer's, there, sampled once.
    fn push(&mut self, from: Point, to: Point) {
        // `warp_copy` of the whole canvas into `original`, and `warp_clear` of the offsets: the first
        // push keeps the layer as it is, and starts every offset at nothing.
        if self.original.is_none() {
            self.original = Some(self.pixels.clone());
            self.offsets = vec![[0.0, 0.0]; self.width * self.height];
        }
        if self.width < 2 || self.height < 2 {
            return;
        }
        let radius = self.radius();
        let move_x = (to.x - from.x) as f32 * self.strength as f32;
        let move_y = (to.y - from.y) as f32 * self.strength as f32;
        let margin = move_x.abs().max(move_y.abs()).ceil() as i64 + 2;
        let cx = to.x.round() as i64;
        let cy = to.y.round() as i64;
        let x0 = (cx - radius - margin).max(0);
        let x1 = (cx + radius + margin).min(self.width as i64 - 1);
        let y0 = (cy - radius - margin).max(0);
        let y1 = (cy + radius + margin).min(self.height as i64 - 1);
        if x0 > x1 || y0 > y1 {
            return;
        }
        let cw = (x1 - x0 + 1) as usize;
        let ch = (y1 - y0 + 1) as usize;
        let width = self.width;
        // `warp_copy`: the offsets in the dab's area as they were before the dab, which the dab reads.
        self.scratch = vec![[0.0, 0.0]; cw * ch];
        for y in 0..ch {
            for x in 0..cw {
                let source = (y0 + y as i64) as usize * width + (x0 + x as i64) as usize;
                self.scratch[y * cw + x] = self.offsets[source];
            }
        }
        let side = (2 * radius + 1) as usize;
        let inverse_radius = 1.0f32 / (self.diameter / 2.0) as f32;
        let height = self.height;
        let hardness = self.hardness as f32;
        let Self {
            pixels,
            offsets,
            scratch,
            original,
            ..
        } = self;
        let Some(original) = original.as_ref() else {
            return;
        };
        let last_x = x0 + cw as i64 - 1;
        let last_y = y0 + ch as i64 - 1;
        for grid_y in 0..side {
            let y = cy + grid_y as i64 - radius;
            if y < y0 || y > last_y {
                continue;
            }
            for grid_x in 0..side {
                let x = cx + grid_x as i64 - radius;
                if x < x0 || x > last_x {
                    continue;
                }
                let dx = grid_x as i64 - radius;
                let dy = grid_y as i64 - radius;
                let w = weight(((dx * dx + dy * dy) as f32).sqrt() * inverse_radius, hardness);
                if w <= 0.0 {
                    continue;
                }
                // Bilinear sample of the offsets as they were, from behind the brush's travel.
                let sx = ((x - x0) as f32 - move_x * w).clamp(0.0, (cw - 1) as f32);
                let sy = ((y - y0) as f32 - move_y * w).clamp(0.0, (ch - 1) as f32);
                let ix = (sx as i64).min(cw as i64 - 2);
                let iy = (sy as i64).min(ch as i64 - 2);
                if ix < 0 || iy < 0 {
                    continue;
                }
                let fx = sx - ix as f32;
                let fy = sy - iy as f32;
                let o00 = scratch[iy as usize * cw + ix as usize];
                let o10 = scratch[iy as usize * cw + ix as usize + 1];
                let o01 = scratch[(iy as usize + 1) * cw + ix as usize];
                let o11 = scratch[(iy as usize + 1) * cw + ix as usize + 1];
                let moved = [
                    mix(mix(o00[0], o10[0], fx), mix(o01[0], o11[0], fx), fy) - move_x * w,
                    mix(mix(o00[1], o10[1], fx), mix(o01[1], o11[1], fx), fy) - move_y * w,
                ];
                offsets[y as usize * width + x as usize] = moved;
                // The untouched layer where that offset points, held to its edges.
                let source_x = (x as f32 + moved[0]).clamp(0.0, width as f32 - 1.0);
                let source_y = (y as f32 + moved[1]).clamp(0.0, height as f32 - 1.0);
                let i = (
                    (source_x as i64).min(width as i64 - 2),
                    (source_y as i64).min(height as i64 - 2),
                );
                let f = (source_x - i.0 as f32, source_y - i.1 as f32);
                let c00 = texel(original, i.0 as usize, i.1 as usize);
                let c10 = texel(original, i.0 as usize + 1, i.1 as usize);
                let c01 = texel(original, i.0 as usize, i.1 as usize + 1);
                let c11 = texel(original, i.0 as usize + 1, i.1 as usize + 1);
                let mut pixel = [0u8; 4];
                for channel in 0..4 {
                    let top = mix(c00[channel], c10[channel], f.0);
                    let bottom = mix(c01[channel], c11[channel], f.0);
                    pixel[channel] = byte(mix(top, bottom, f.1));
                }
                pixels.set(x as usize, y as usize, pixel);
            }
        }
    }
}

/// One pixel of an RGBA plane as `f32` 0…255, the way the MSL kernels read a texture.
fn texel(image: &Rgba8Image, x: usize, y: usize) -> [f32; 4] {
    image.get(x, y).map(|channel| channel as f32)
}

/// `DistortWarp`: free distortion (Cmd-drag a transform handle): the layer's four corners move
/// independently. Layer transforms are affine, so a distortion is previewed live and, on Apply, the
/// pixels (and mask) are resampled into the new shape — as Photoshop does for pixel layers — leaving
/// an ordinary axis-aligned layer over the shape's bounds.
pub enum DistortWarp {}

impl DistortWarp {
    /// The transform's corners in handle order: top-left, top-right, bottom-right, bottom-left.
    pub fn corners(transform: &LayerTransform) -> [Point; 4] {
        [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ]
        .map(|unit| transform.point(unit))
    }

    /// Four finite corners with some area to them. A convex shape is warped in perspective; anything
    /// else — a corner pulled past its neighbours, which folds the shape over — is warped as two
    /// triangles instead (see `warp`).
    pub fn is_usable(corners: &[Point]) -> bool {
        if corners.len() != 4 {
            return false;
        }
        let all_finite = corners
            .iter()
            .all(|corner| corner.x.is_finite() && corner.y.is_finite() && corner.x.abs() <= 1_000_000.0 && corner.y.abs() <= 1_000_000.0);
        if !all_finite {
            return false;
        }
        // Both halves need area, or one of them has nothing to draw.
        area(corners[0], corners[1], corners[2]).abs() > 0.01
            && area(corners[0], corners[2], corners[3]).abs() > 0.01
    }

    /// A shape a perspective warp can take: convex, wound consistently either way (so a mirrored one
    /// counts).
    pub fn is_convex(corners: &[Point]) -> bool {
        if !Self::is_usable(corners) {
            return false;
        }
        let mut sign: f64 = 0.0;
        for index in 0..4 {
            let a = corners[index];
            let b = corners[(index + 1) % 4];
            let c = corners[(index + 2) % 4];
            let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
            if cross.abs() <= 0.01 {
                return false;
            }
            if sign == 0.0 {
                sign = if cross < 0.0 { -1.0 } else { 1.0 };
            } else if (cross < 0.0) != (sign < 0.0) {
                return false;
            }
        }
        true
    }

    /// The affine map taking three source points to three destination points.
    fn affine(from: (Point, Point, Point), to: (Point, Point, Point)) -> Option<AffineTransform> {
        let u = Point::new(from.1.x - from.0.x, from.1.y - from.0.y);
        let v = Point::new(from.2.x - from.0.x, from.2.y - from.0.y);
        let uu = Point::new(to.1.x - to.0.x, to.1.y - to.0.y);
        let vv = Point::new(to.2.x - to.0.x, to.2.y - to.0.y);
        let det = u.x * v.y - v.x * u.y;
        if det.abs() <= 1e-9 {
            return None;
        }
        let a = (uu.x * v.y - vv.x * u.y) / det;
        let c = (vv.x * u.x - uu.x * v.x) / det;
        let b = (uu.y * v.y - vv.y * u.y) / det;
        let d = (vv.y * u.x - uu.y * v.x) / det;
        Some(AffineTransform {
            a,
            b,
            c,
            d,
            tx: to.0.x - (a * from.0.x + c * from.0.y),
            ty: to.0.y - (b * from.0.x + d * from.0.y),
        })
    }

    /// The perspective mapping of the unit square (corners in `corners(of:)` order) onto `c`.
    ///
    /// The Swift returns a closure; the mapping is kept as a matrix here so `warp` can run it
    /// backwards, sampling the source for each output pixel.
    pub fn homography(corners: &[Point]) -> impl Fn(Point) -> Point + '_ {
        let perspective = Perspective::new(corners);
        move |point| match perspective {
            Some(perspective) => perspective.map(point),
            // A collapsed quad has no mapping to run: the Swift's closure would be built from a
            // degenerate solve (its `den` guard leaves `g` and `h` at zero), so the point is left
            // where it is rather than moved through nonsense.
            None => point,
        }
    }

    /// Where each corner of the image's own pixels lands: a flipped layer shows its pixels mirrored,
    /// so they go to the opposite corners of the shape.
    fn image_corners(
        corners: &[Point],
        flip_x: bool,
        flip_y: bool,
    ) -> (Point, Point, Point, Point) {
        fn corner(corners: &[Point], flip_x: bool, flip_y: bool, x: usize, y: usize) -> Point {
            let u = if flip_x { 1 - x } else { x };
            let v = if flip_y { 1 - y } else { y };
            corners[[0, 1, 3, 2][v * 2 + u]]
        }
        (
            corner(corners, flip_x, flip_y, 0, 0),
            corner(corners, flip_x, flip_y, 1, 0),
            corner(corners, flip_x, flip_y, 1, 1),
            corner(corners, flip_x, flip_y, 0, 1),
        )
    }

    /// `image`, shown through `transform`, resampled so its corners land on `corners`. Returns the
    /// warped pixels over the shape's whole-pixel bounds and the axis-aligned transform for them.
    /// `limit` caps the longest side for previews.
    pub fn warp(
        image: &PixelImage,
        transform: &LayerTransform,
        corners: &[Point],
        is_mask: bool,
        limit: Option<f64>,
    ) -> Result<(PixelImage, LayerTransform), CoreError> {
        if !Self::is_usable(corners) {
            return Err(invalid_shape());
        }
        let xs: Vec<f64> = corners.iter().map(|corner| corner.x).collect();
        let ys: Vec<f64> = corners.iter().map(|corner| corner.y).collect();
        let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min).floor();
        let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min).floor();
        let bounds = Rect::new(
            min_x,
            min_y,
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max).ceil() - min_x,
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max).ceil() - min_y,
        );
        if !(bounds.width() >= 1.0
            && bounds.height() >= 1.0
            && bounds.width() <= MAX_SIDE_EXTENT
            && bounds.height() <= MAX_SIDE_EXTENT
            && bounds.width() * bounds.height() <= MAX_SURFACE_EXTENT)
        {
            return Err(too_large());
        }
        let placed = LayerTransform {
            origin: bounds.origin,
            size: bounds.size,
            sampling: transform.sampling,
            ..LayerTransform::default()
        };
        // A uniform 1 × 1 mask already covers any shape.
        if is_mask && image.width() == 1 && image.height() == 1 {
            return Ok((image.clone(), placed));
        }
        let factor = limit
            .map(|limit| 1.0f64.min(limit / bounds.width().max(bounds.height())))
            .unwrap_or(1.0);
        let width = 1usize.max((bounds.width() * factor).ceil() as usize);
        let height = 1usize.max((bounds.height() * factor).ceil() as usize);
        let target = Self::image_corners(corners, transform.flip_x, transform.flip_y);
        // A folded shape (a corner dragged past its neighbours) has no perspective that takes the
        // image to it, so each half is taken there on its own, as two triangles meeting along the
        // shape's diagonal.
        if !Self::is_convex(corners) {
            return Ok((
                Self::warp_folded(image, &target, bounds, factor, width, height, is_mask)?,
                placed,
            ));
        }
        let target_corners = [target.0, target.1, target.2, target.3];
        let Some(perspective) = Perspective::new(&target_corners) else {
            return Err(invalid_shape());
        };
        Ok((
            perspective_warp(image, &perspective, bounds, factor, width, height, is_mask),
            placed,
        ))
    }

    /// The image drawn into a shape as two triangles: the halves either side of the diagonal, each
    /// taken there by its own affine map. Handles folded and dented shapes, which a perspective warp
    /// cannot.
    fn warp_folded(
        image: &PixelImage,
        target: &(Point, Point, Point, Point),
        bounds: Rect,
        factor: f64,
        width: usize,
        height: usize,
        is_mask: bool,
    ) -> Result<PixelImage, CoreError> {
        let mut canvas = if is_mask {
            Canvas::new_gray(width, height)
        } else {
            Canvas::new_rgba(width, height)
        };
        let source = Rect::new(0.0, 0.0, image.width() as f64, image.height() as f64);
        let corners = (
            Point::new(source.min_x(), source.min_y()),
            Point::new(source.max_x(), source.min_y()),
            Point::new(source.max_x(), source.max_y()),
            Point::new(source.min_x(), source.max_y()),
        );
        let placed = |point: Point| {
            Point::new(
                (point.x - bounds.min_x()) * factor,
                (point.y - bounds.min_y()) * factor,
            )
        };
        let halves = [
            (
                (corners.0, corners.1, corners.2),
                (target.0, target.1, target.2),
            ),
            (
                (corners.0, corners.2, corners.3),
                (target.0, target.2, target.3),
            ),
        ];
        for (from, to) in halves {
            let destination = (placed(to.0), placed(to.1), placed(to.2));
            let Some(map) = Self::affine(from, destination) else {
                continue;
            };
            canvas.save();
            // Hard edges along the shared diagonal, so the two halves meet exactly instead of
            // blending twice.
            canvas.set_should_antialias(false);
            let mut triangle = Path::empty();
            triangle.add_lines(&[destination.0, destination.1, destination.2]);
            triangle.close_subpath();
            canvas.clip_path(&triangle, FillRule::Winding);
            canvas.concatenate(map);
            canvas.set_should_antialias(true);
            Raster::draw(image, source, is_mask, &mut canvas);
            canvas.restore();
        }
        Ok(if is_mask {
            PixelImage::Gray(Arc::new(canvas.into_gray()))
        } else {
            PixelImage::Rgba(Arc::new(canvas.into_rgba()))
        })
    }

    /// A full-resolution warp cropped to its visible pixels. A distorted shape rarely fills its
    /// bounding box — and a brush stroke never does — so the layer (and its transform handles)
    /// should hug what is actually there. The crop is in the warp's pixels, for cropping a mask to
    /// match.
    ///
    /// Returns the pixels, the transform that places them, and the crop rectangle.
    pub fn warp_trimmed(
        image: &PixelImage,
        transform: &LayerTransform,
        corners: &[Point],
    ) -> Result<(PixelImage, LayerTransform, Rect), CoreError> {
        let (warped, placed) = Self::warp(image, transform, corners, false, None)?;
        let full = Rect::new(0.0, 0.0, warped.width() as f64, warped.height() as f64);
        // `brush_alpha_bounds` of the warped pixels: the half-open bounds of nonzero alpha.
        let crop = warped.as_rgba().and_then(|rgba| rgba.alpha_bounds());
        // Nothing visible, or nothing to trim: keep the warp as it is.
        let Some(crop) = crop else {
            return Ok((warped, placed, full));
        };
        if crop.width() < 1.0 || crop.height() < 1.0 || crop == full {
            return Ok((warped, placed, full));
        }
        let Some(cropped) = warped.cropped(crop) else {
            return Ok((warped, placed, full));
        };
        let mut moved = placed;
        moved.origin = Point::new(
            placed.origin.x + crop.min_x(),
            placed.origin.y + crop.min_y(),
        );
        moved.size = crop.size;
        Ok((cropped, moved, crop))
    }

    /// Where `placement`'s corners (handle order) land when the perspective taking `transform`'s
    /// corners to `corners` is applied around it too — how a linked mask placed apart from its layer
    /// distorts with the layer.
    pub fn carried(
        placement: &LayerTransform,
        transform: &LayerTransform,
        corners: &[Point],
    ) -> [Point; 4] {
        // The Swift builds this inverse by hand rather than from `unitToDocument`, because a flipped
        // layer's placement must not fold the flips into the perspective.
        // Swift `A.concatenating(B)` runs the receiver first, which is `then` here.
        let to_unit = AffineTransform::translation(-0.5, -0.5)
            .then(AffineTransform::scale(
                transform.size.width,
                transform.size.height,
            ))
            .then(AffineTransform::rotation(transform.radians()))
            .then(AffineTransform::translation(
                transform.center().x,
                transform.center().y,
            ))
            .inverted();
        let map = Self::homography(corners);
        Self::corners(placement).map(|corner| map(to_unit.applying(corner)))
    }

    /// A mask warped like `warp`, but `background` (its tone past its pixels) outside the shape
    /// instead of black — for masks placed apart from their layers, which show beyond their own
    /// bounds.
    pub fn warp_mask(
        image: &PixelImage,
        transform: &LayerTransform,
        corners: &[Point],
        background: f64,
        limit: Option<f64>,
    ) -> Result<(PixelImage, LayerTransform), CoreError> {
        let (warped, placed) = Self::warp(image, transform, corners, true, limit)?;
        if background <= 0.0 || same_pixels(&warped, image) {
            return Ok((warped, placed));
        }
        let width = warped.width();
        let height = warped.height();
        let full = Rect::new(0.0, 0.0, width as f64, height as f64);
        let mut canvas = Canvas::new_gray(width, height);
        canvas.set_fill_gray(background);
        canvas.fill_rect(full);
        let sx = width as f64 / placed.size.width;
        let sy = height as f64 / placed.size.height;
        let mut shape = Path::empty();
        shape.add_lines(
            &corners
                .iter()
                .map(|corner| {
                    Point::new(
                        (corner.x - placed.origin.x) * sx,
                        (corner.y - placed.origin.y) * sy,
                    )
                })
                .collect::<Vec<_>>(),
        );
        shape.close_subpath();
        canvas.clip_path(&shape, FillRule::Winding);
        Raster::draw(&warped, full, true, &mut canvas);
        Ok((PixelImage::Gray(Arc::new(canvas.into_gray())), placed))
    }

    /// Carries an outline drawn over the original pixels (placed by `pixel_to_document`) into the
    /// distorted shape, so a transformed selection keeps matching its pixels.
    pub fn map_path(
        path: &Path,
        pixel_to_document: AffineTransform,
        pixel_size: Size,
        transform: &LayerTransform,
        corners: &[Point],
    ) -> Option<Path> {
        if !Self::is_convex(corners) || !(pixel_size.width > 0.0 && pixel_size.height > 0.0) {
            return None;
        }
        let to_pixels = pixel_to_document.inverted();
        let perspective = Perspective::new(corners)?;
        let carry = |point: Point| {
            let pixel = to_pixels.applying(point);
            let mut u = pixel.x / pixel_size.width;
            let mut v = pixel.y / pixel_size.height;
            if transform.flip_x {
                u = 1.0 - u;
            }
            if transform.flip_y {
                v = 1.0 - v;
            }
            perspective.map(Point::new(u, v))
        };
        let mut result = Path::empty();
        for element in path.elements().iter().copied() {
            match element {
                PathElement::MoveTo(point) => result.move_to(carry(point)),
                PathElement::LineTo(point) => result.add_line(carry(point)),
                PathElement::QuadCurveTo { control, to } => {
                    result.add_quad_curve(carry(control), carry(to))
                }
                PathElement::CurveTo {
                    control1,
                    control2,
                    to,
                } => result.add_curve(carry(control1), carry(control2), carry(to)),
                PathElement::CloseSubpath => result.close_subpath(),
            }
        }
        Some(result)
    }
}

/// `(b - a) × (c - a)`: twice the signed area of the triangle.
fn area(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Whether two raster kinds are the same allocation (`CGImage === CGImage`).
fn same_pixels(left: &PixelImage, right: &PixelImage) -> bool {
    match (left, right) {
        (PixelImage::Rgba(left), PixelImage::Rgba(right)) => Arc::ptr_eq(left, right),
        (PixelImage::Gray(left), PixelImage::Gray(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

/// `ProjectError.invalid`'s message.
fn invalid_shape() -> CoreError {
    CoreError::Message(
        "This is not a valid Compositor project, or its metadata is damaged.".to_string(),
    )
}

/// `ProjectError.tooLarge`'s message.
fn too_large() -> CoreError {
    CoreError::TooLarge(max_surface_megapixels())
}

/// A homography: the mapping of the unit square onto four corners, and its inverse, which `warp`
/// samples the source through.
#[derive(Clone, Copy, Debug)]
struct Perspective {
    /// `x' = (a·u + b·v + x0) / w`, `y' = (d·u + e·v + y0) / w`, `w = g·u + h·v + 1`, the Swift's
    /// coefficients.
    forward: [[f64; 3]; 3],
    inverse: [[f64; 3]; 3],
}

impl Perspective {
    /// The perspective mapping of the unit square onto `corners`, or `None` for a shape with no
    /// invertible mapping. A shape that is a parallelogram has `g = h = 0`: the mapping is affine and
    /// the solve is skipped, exactly as the Swift does.
    fn new(corners: &[Point]) -> Option<Self> {
        if corners.len() < 4 {
            return None;
        }
        let (c0, c1, c2, c3) = (corners[0], corners[1], corners[2], corners[3]);
        let sx = c0.x - c1.x + c2.x - c3.x;
        let sy = c0.y - c1.y + c2.y - c3.y;
        let (mut g, mut h) = (0.0f64, 0.0f64);
        if sx.abs() > 1e-9 || sy.abs() > 1e-9 {
            let dx1 = c1.x - c2.x;
            let dx2 = c3.x - c2.x;
            let dy1 = c1.y - c2.y;
            let dy2 = c3.y - c2.y;
            let den = dx1 * dy2 - dx2 * dy1;
            if den.abs() > 1e-12 {
                g = (sx * dy2 - dx2 * sy) / den;
                h = (dx1 * sy - sx * dy1) / den;
            }
        }
        let a = c1.x - c0.x + g * c1.x;
        let b = c3.x - c0.x + h * c3.x;
        let x0 = c0.x;
        let d = c1.y - c0.y + g * c1.y;
        let e = c3.y - c0.y + h * c3.y;
        let y0 = c0.y;
        let forward = [[a, b, x0], [d, e, y0], [g, h, 1.0]];
        let inverse = invert3(forward)?;
        Some(Self { forward, inverse })
    }

    /// Where a unit-square point lands on the document.
    fn map(&self, point: Point) -> Point {
        apply(&self.forward, point)
    }

    /// Where a document point came from in the unit square.
    fn unmap(&self, point: Point) -> Point {
        apply(&self.inverse, point)
    }
}

/// A projective map of `point`, divided through by its third coordinate.
fn apply(matrix: &[[f64; 3]; 3], point: Point) -> Point {
    let w = matrix[2][0] * point.x + matrix[2][1] * point.y + matrix[2][2];
    Point::new(
        (matrix[0][0] * point.x + matrix[0][1] * point.y + matrix[0][2]) / w,
        (matrix[1][0] * point.x + matrix[1][1] * point.y + matrix[1][2]) / w,
    )
}

/// The inverse of a projective map's matrix, `None` when it is degenerate.
fn invert3(matrix: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let m = matrix;
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) / det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) / det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) / det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) / det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) / det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) / det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) / det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) / det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) / det,
        ],
    ])
}

/// `CIPerspectiveTransform` and `PixelAdjust.render` on the CPU: `image` resampled over
/// `width`×`height` so the four corners of its own pixels land where `target` puts them.
///
/// The output pixel (i, j) covers the document point `(bounds.minX + (i + 0.5) / factor,
/// bounds.minY + (j + 0.5) / factor)` — the y flip Core Image measures from the bottom cancels out
/// of `vector(_:)`. That point is mapped back through the perspective to the source's unit square
/// and read bilinearly, with the sample held to the image's edges; outside the unit square the pixel
/// keeps its blank value, as Core Image leaves what is outside the input's extent.
fn perspective_warp(
    image: &PixelImage,
    perspective: &Perspective,
    bounds: Rect,
    factor: f64,
    width: usize,
    height: usize,
    is_mask: bool,
) -> PixelImage {
    let sample = |x: usize, y: usize| -> Option<[f32; 4]> {
        let document = Point::new(
            bounds.min_x() + (x as f64 + 0.5) / factor,
            bounds.min_y() + (y as f64 + 0.5) / factor,
        );
        let unit = perspective.unmap(document);
        if !(unit.x >= 0.0 && unit.x <= 1.0 && unit.y >= 0.0 && unit.y <= 1.0) {
            return None;
        }
        Some(bilinear(
            image,
            unit.x * image.width() as f64,
            unit.y * image.height() as f64,
            is_mask,
        ))
    };
    if is_mask {
        let mut result = Gray8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                if let Some(pixel) = sample(x, y) {
                    result.set(x, y, byte(pixel[0]));
                }
            }
        }
        PixelImage::Gray(Arc::new(result))
    } else {
        let mut result = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                if let Some(pixel) = sample(x, y) {
                    result.set(
                        x,
                        y,
                        [byte(pixel[0]), byte(pixel[1]), byte(pixel[2]), byte(pixel[3])],
                    );
                }
            }
        }
        PixelImage::Rgba(Arc::new(result))
    }
}

/// A bilinear read of `source` at `(x, y)`, in Core Image's pixel space — the image spans
/// `0…width` by `0…height` and pixel `i`'s center sits at `i + 0.5` — held to its edges: the four
/// pixels around the sample mixed by their fractional weights. A mask target reads the gray value
/// (an RGBA plane reduced to its luminance, the L8 render Core Image is asked for); a color target
/// reads the color (a gray plane replicated, fully opaque).
fn bilinear(source: &PixelImage, x: f64, y: f64, is_mask: bool) -> [f32; 4] {
    let width = source.width();
    let height = source.height();
    let x = x.clamp(0.5, width as f64 - 0.5);
    let y = y.clamp(0.5, height as f64 - 0.5);
    let left = (x - 0.5).floor();
    let top = (y - 0.5).floor();
    let fx = (x - 0.5 - left) as f32;
    let fy = (y - 0.5 - top) as f32;
    let x0 = left as usize;
    let y0 = top as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let c00 = source_texel(source, x0, y0, is_mask);
    let c10 = source_texel(source, x1, y0, is_mask);
    let c01 = source_texel(source, x0, y1, is_mask);
    let c11 = source_texel(source, x1, y1, is_mask);
    let mut result = [0.0f32; 4];
    for channel in 0..4 {
        let first = mix(c00[channel], c10[channel], fx);
        let second = mix(c01[channel], c11[channel], fx);
        result[channel] = mix(first, second, fy);
    }
    result
}

/// One source pixel as the target's kind reads it.
fn source_texel(source: &PixelImage, x: usize, y: usize, is_mask: bool) -> [f32; 4] {
    match (source, is_mask) {
        (PixelImage::Gray(image), true) => {
            let value = image.get(x, y) as f32;
            [value, value, value, 255.0]
        }
        (PixelImage::Rgba(image), false) => image.get(x, y).map(|channel| channel as f32),
        // A mask's pixels are gray, so these two conversions only arise if a caller asks for the
        // other kind; Core Image would render the plane into the bitmap's format the same way.
        (PixelImage::Rgba(image), true) => {
            let pixel = image.get(x, y);
            let value = 0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32;
            [value, value, value, 255.0]
        }
        (PixelImage::Gray(image), false) => {
            let value = image.get(x, y) as f32;
            [value, value, value, 255.0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_(x: f64, y: f64, width: f64, height: f64) -> LayerTransform {
        LayerTransform {
            origin: Point::new(x, y),
            size: Size::new(width, height),
            ..LayerTransform::default()
        }
    }

    /// A color image whose pixels all differ, at partial alpha.
    fn gradient(width: usize, height: usize) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(
                    x,
                    y,
                    [
                        (x * 7 % 256) as u8,
                        (y * 13 % 256) as u8,
                        ((x + y) * 5 % 256) as u8,
                        200,
                    ],
                );
            }
        }
        image
    }

    fn rgba(image: &PixelImage) -> &Rgba8Image {
        image.as_rgba().expect("color pixels")
    }

    #[test]
    fn the_tool_modes_round_trip_their_raw_values() {
        for mode in BrushToolMode::ALL {
            assert_eq!(BrushToolMode::from_raw(mode.raw_value()), Some(mode));
        }
        for mode in BlurToolMode::ALL {
            assert_eq!(BlurToolMode::from_raw(mode.raw_value()), Some(mode));
        }
        assert_eq!(BrushToolMode::from_raw("Paint"), Some(BrushToolMode::Paint));
        assert_eq!(BlurToolMode::from_raw("Liquify"), Some(BlurToolMode::Liquify));
        assert_eq!(BlurToolMode::from_raw("Nope"), None);
    }

    #[test]
    fn a_homography_lands_on_the_rectangle_it_was_given() {
        let corners = DistortWarp::corners(&box_(10.0, 20.0, 100.0, 50.0));
        assert_eq!(corners[0], Point::new(10.0, 20.0));
        assert_eq!(corners[1], Point::new(110.0, 20.0));
        assert_eq!(corners[2], Point::new(110.0, 70.0));
        assert_eq!(corners[3], Point::new(10.0, 70.0));
        let map = DistortWarp::homography(&corners);
        assert_eq!(map(Point::new(0.0, 0.0)), corners[0]);
        assert_eq!(map(Point::new(1.0, 0.0)), corners[1]);
        assert_eq!(map(Point::new(1.0, 1.0)), corners[2]);
        assert_eq!(map(Point::new(0.0, 1.0)), corners[3]);
        assert_eq!(map(Point::new(0.5, 0.5)), Point::new(60.0, 45.0));
    }

    #[test]
    fn a_placement_carried_by_its_unchanged_corners_stays_put() {
        // `carried` builds Swift's receiver-first chain (translate, scale, rotate, centre), then
        // inverts it: a placement carried by the perspective that leaves its own corners alone must
        // come back on those corners. The reversed chain maps the corners far outside the box.
        let transform = box_(10.0, 20.0, 100.0, 50.0);
        let corners = DistortWarp::corners(&transform);
        let carried = DistortWarp::carried(&transform, &transform, &corners);
        for (moved, original) in carried.iter().zip(corners.iter()) {
            assert!((moved.x - original.x).abs() < 1e-6, "x {} vs {}", moved.x, original.x);
            assert!((moved.y - original.y).abs() < 1e-6, "y {} vs {}", moved.y, original.y);
        }
    }

    #[test]
    fn a_perspective_maps_a_trapezoid_and_unmaps_it_again() {
        let corners = [
            Point::new(10.0, 5.0),
            Point::new(90.0, 5.0),
            Point::new(80.0, 60.0),
            Point::new(20.0, 55.0),
        ];
        assert!(DistortWarp::is_usable(&corners));
        assert!(DistortWarp::is_convex(&corners));
        let perspective = Perspective::new(&corners).expect("a solvable quad");
        for unit in [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
            Point::new(0.25, 0.75),
        ] {
            let mapped = perspective.map(unit);
            assert!(
                (mapped.x - DistortWarp::homography(&corners)(unit).x).abs() < 1e-9
                    && (mapped.y - DistortWarp::homography(&corners)(unit).y).abs() < 1e-9
            );
            let back = perspective.unmap(mapped);
            assert!((back.x - unit.x).abs() < 1e-9, "u of {unit:?}");
            assert!((back.y - unit.y).abs() < 1e-9, "v of {unit:?}");
        }
    }

    #[test]
    fn a_collapsed_or_folded_shape_is_rejected() {
        let rectangle = DistortWarp::corners(&box_(0.0, 0.0, 10.0, 10.0));
        assert!(DistortWarp::is_usable(&rectangle));
        assert!(DistortWarp::is_convex(&rectangle));
        // Three corners, and a corner sitting on its neighbour: nothing a warp could take.
        assert!(!DistortWarp::is_usable(&rectangle[..3]));
        assert!(!DistortWarp::is_usable(&[
            rectangle[0],
            rectangle[0],
            rectangle[2],
            rectangle[3],
        ]));
        assert!(!DistortWarp::is_usable(&[
            Point::new(f64::NAN, 0.0),
            rectangle[1],
            rectangle[2],
            rectangle[3],
        ]));
        // A corner pulled past its neighbours folds the shape over: usable, but not convex.
        let folded = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(-5.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        assert!(DistortWarp::is_usable(&folded));
        assert!(!DistortWarp::is_convex(&folded));
    }

    #[test]
    fn an_identity_quad_warps_the_source_unchanged() {
        let source = gradient(12, 8);
        let image = PixelImage::Rgba(Arc::new(source.clone()));
        let transform = box_(0.0, 0.0, 12.0, 8.0);
        let corners = DistortWarp::corners(&transform);
        let (warped, placed) =
            DistortWarp::warp(&image, &transform, &corners, false, None).expect("a warp");
        let warped = rgba(&warped);
        assert_eq!((warped.width(), warped.height()), (12, 8));
        assert_eq!(placed.origin, Point::new(0.0, 0.0));
        assert_eq!(placed.size, Size::new(12.0, 8.0));
        for y in 0..8 {
            for x in 0..12 {
                assert_eq!(warped.get(x, y), source.get(x, y), "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn a_folded_shape_is_warped_as_two_triangles() {
        let image = PixelImage::Rgba(Arc::new(gradient(10, 10)));
        let transform = box_(0.0, 0.0, 10.0, 10.0);
        let corners = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(-5.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        let (warped, placed) =
            DistortWarp::warp(&image, &transform, &corners, false, None).expect("a warp");
        let warped = rgba(&warped);
        // The shape's bounds, and only the source's own pixels inside them: the empty corners stay
        // transparent.
        assert_eq!((placed.origin.x, placed.origin.y), (-5.0, 0.0));
        assert_eq!((warped.width(), warped.height()), (15, 10));
        assert!(warped.pixels().all(|pixel| pixel[3] == 200 || pixel[3] == 0));
        assert!(warped.pixels().any(|pixel| pixel[3] == 200));
        assert!(warped.pixels().any(|pixel| pixel[3] == 0));
    }

    #[test]
    fn a_uniform_mask_passes_a_warp_through_untouched() {
        let mask = PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, 255)));
        let transform = box_(0.0, 0.0, 20.0, 10.0);
        let corners = DistortWarp::corners(&transform);
        let (warped, placed) =
            DistortWarp::warp(&mask, &transform, &corners, true, None).expect("a warp");
        assert!(same_pixels(&warped, &mask));
        assert_eq!(placed.size, Size::new(20.0, 10.0));
    }

    #[test]
    fn a_warped_mask_shows_its_background_outside_the_shape() {
        let mask = PixelImage::Gray(Arc::new(Gray8Image::uniform(8, 8, 255)));
        let transform = box_(0.0, 0.0, 8.0, 8.0);
        // A diamond: its bounding box is mostly outside the shape.
        let corners = [
            Point::new(10.0, 0.0),
            Point::new(20.0, 10.0),
            Point::new(10.0, 20.0),
            Point::new(0.0, 10.0),
        ];
        assert!(DistortWarp::is_convex(&corners));
        // A mid-gray background — `background` is a 0–1 gray, as `setFillColor(gray:alpha:)`'s is.
        let (warped, placed) =
            DistortWarp::warp_mask(&mask, &transform, &corners, 128.0 / 255.0, None).expect("a warp");
        let warped = warped.as_gray().expect("mask pixels");
        assert_eq!((warped.width(), warped.height()), (20, 20));
        assert_eq!(placed.size, Size::new(20.0, 20.0));
        assert_eq!(warped.get(10, 10), 255);
        assert_eq!(warped.get(0, 0), 128);
        assert_eq!(warped.get(19, 19), 128);
    }

    #[test]
    fn a_warp_is_trimmed_to_its_visible_pixels() {
        let mut source = Rgba8Image::new(10, 10);
        for y in 3..7 {
            for x in 3..7 {
                source.set(x, y, [200, 100, 50, 255]);
            }
        }
        let image = PixelImage::Rgba(Arc::new(source));
        let transform = box_(0.0, 0.0, 10.0, 10.0);
        let corners = DistortWarp::corners(&transform);
        let (trimmed, placed, crop) =
            DistortWarp::warp_trimmed(&image, &transform, &corners).expect("a warp");
        assert_eq!(crop, Rect::new(3.0, 3.0, 4.0, 4.0));
        assert_eq!((trimmed.width(), trimmed.height()), (4, 4));
        assert_eq!(placed.origin, Point::new(3.0, 3.0));
        assert_eq!(placed.size, Size::new(4.0, 4.0));
        assert_eq!(rgba(&trimmed).get(0, 0), [200, 100, 50, 255]);
        // Nothing visible at all: the warp is kept as it is.
        let empty = PixelImage::Rgba(Arc::new(Rgba8Image::new(10, 10)));
        let (kept, _, crop) = DistortWarp::warp_trimmed(&empty, &transform, &corners).expect("a warp");
        assert_eq!((kept.width(), kept.height()), (10, 10));
        assert_eq!(crop, Rect::new(0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn a_selection_path_that_is_already_in_place_comes_back_unchanged() {
        let transform = box_(0.0, 0.0, 10.0, 10.0);
        let corners = DistortWarp::corners(&transform);
        let path = Path::rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        let placement = compositor_rs_core::layer_transform::pixel_to_document(&transform, 10, 10);
        let mapped = DistortWarp::map_path(&path, placement, Size::new(10.0, 10.0), &transform, &corners)
            .expect("a convex shape");
        assert_eq!(mapped.bounding_box(), path.bounding_box());
        // A folded shape has no perspective to carry a path into.
        let folded = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(-5.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        assert!(DistortWarp::map_path(&path, placement, Size::new(10.0, 10.0), &transform, &folded).is_none());
    }

    #[test]
    fn a_smudge_stroke_drags_color_and_is_deterministic() {
        let mut source = Rgba8Image::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                source.set(
                    x,
                    y,
                    if x < 16 { [255, 0, 0, 255] } else { [0, 0, 255, 255] },
                );
            }
        }
        let stroke = |source: &Rgba8Image| {
            let mut stroke = WarpStroke::new(source.clone(), BlurToolMode::Smudge, 9.0, 0.5, 1.0);
            stroke.append(Point::new(10.0, 16.0));
            stroke.append(Point::new(22.0, 16.0));
            stroke
        };
        let first = stroke(&source);
        let second = stroke(&source);
        assert_eq!(first.image(), second.image());
        assert_ne!(first.image(), &source);
        // The brush dragged red across the boundary: pixels that were blue now hold some of it.
        assert!((16..22).any(|x| first.image().get(x, 16)[0] > 0));
        // Every dab is on the path, and the stroke ends where it was told to.
        assert_eq!(first.points().last(), Some(&Point::new(22.0, 16.0)));
        assert!(first.points().iter().all(|point| point.y == 16.0));
    }

    #[test]
    fn a_liquify_dab_redraws_from_the_untouched_layer() {
        // A whole-pixel move with a hard tip puts every sample on a pixel center, so the dab comes
        // out as an exact shift of the layer the stroke found — the offsets model (see `offsets`),
        // which resamples the untouched pixels once instead of softening them dab by dab.
        let mut source = Rgba8Image::new(40, 12);
        for y in 0..12 {
            for x in 0..40 {
                source.set(x, y, [x as u8 * 6, y as u8 * 20, 30, 255]);
            }
        }
        let mut stroke = WarpStroke::new(source.clone(), BlurToolMode::Liquify, 200.0, 1.0, 1.0);
        stroke.append(Point::new(30.0, 6.0));
        stroke.append(Point::new(25.0, 6.0));
        assert_eq!(stroke.points().len(), 1);
        assert_eq!(stroke.points()[0], Point::new(25.0, 6.0));
        let pushed = stroke.image();
        assert_ne!(pushed, &source);
        for y in 0..12 {
            for x in 0..40 {
                assert_eq!(pushed.get(x, y), source.get((x + 5).min(39), y), "pixel {x},{y}");
            }
        }
    }

    /// Ported from `MetalWarpTests.smudgeLeavesOneFadingTrail`, CPU half (`gpu: false`): the GPU
    /// half compared Metal's smudge against this CPU one, and Metal is gone in this port.
    #[test]
    fn smudge_leaves_one_fading_trail() {
        // A dark field with a white dot under where the brush starts.
        let mut source = Rgba8Image::opaque(300, 100, [26, 26, 26, 255]);
        for y in 42..58 {
            for x in 52..68 {
                let (dx, dy) = (x as f64 + 0.5 - 60.0, y as f64 + 0.5 - 50.0);
                if dx * dx + dy * dy <= 8.0 * 8.0 {
                    source.set(x, y, [255, 255, 255, 255]);
                }
            }
        }
        // A size-40 hardness-0.5 opacity-0.6 smudge from x = 60 to 240 along y = 50.
        let mut stroke = WarpStroke::new(source, BlurToolMode::Smudge, 40.0, 0.5, 0.6);
        let mut x = 60.0;
        while x <= 240.0 {
            stroke.append(Point::new(x, 50.0));
            x += 3.0;
        }
        let result = stroke.image();
        // Brightness along the stroke's line, past the dot.
        let row: Vec<i32> = (70..240).map(|x| result.get(x, 50)[0] as i32).collect();
        // A ghost is a bump: brighter than a little way either side of it, however faint.
        let peaks = (2..row.len() - 2)
            .filter(|i| {
                let i = *i;
                row[i] > row[i - 2] + 2 && row[i] > row[i + 2] + 2
            })
            .count();
        assert_eq!(
            peaks, 0,
            "the trail fades without repeating: {peaks} ghost peaks along {row:?}"
        );
        assert!(
            row.first().unwrap() > &(row.last().unwrap() + 20),
            "and there is a trail: {} to {}",
            row.first().unwrap(),
            row.last().unwrap()
        );
    }
}
