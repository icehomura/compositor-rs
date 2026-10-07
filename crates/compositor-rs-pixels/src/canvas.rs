//! The `CGContext` replacement: a bitmap drawing target with a transform stack, clipping, fills,
//! gradients and image drawing — used by the brush engine, mask coverage, shape drawing, text, effects
//! and export.
//!
//! Coordinates follow the editor: device space is the target's pixels, top-left origin, y down. The CTM
//! maps user space to device space and is what `translate`/`scale`/`rotate`/`concatenate` build up.
//!
//! Antialiasing is a scanline fill with four vertical subsamples a pixel and exact horizontal span
//! coverage, which reproduces Core Graphics' coverage closely enough for mask and selection edges to be
//! stable; `set_should_antialias(false)` uses one sample at the pixel row's centre and hard coverage.

use compositor_rs_core::blend::LayerBlendMode;
use compositor_rs_core::color::to_byte;
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::path::{FillRule, Path, PathElement, Subpath, flatten};
use compositor_rs_core::path_ops::{LineCap, LineJoin};
use compositor_rs_core::{Gray8Image, PaletteColor, Rgba8Image};
use std::sync::Arc;

/// `CGInterpolationQuality`'s three values, the ones the editor uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InterpolationQuality {
    None,
    Low,
    #[default]
    High,
}

#[derive(Clone, Debug)]
pub enum GradientKind {
    Linear { from: Point, to: Point },
    Radial { center: Point, radius: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientExtend {
    /// Transparent outside the end points (`drawsAfterEndLocation = false`).
    None,
    /// The end colors continue past the ends.
    Pad,
}

/// A gradient in straight (non-premultiplied) sRGB, as `CGGradient` holds it.
#[derive(Clone, Debug)]
pub struct GradientPaint {
    pub kind: GradientKind,
    pub stops: Vec<(f64, [f64; 4])>,
    pub extend: GradientExtend,
}

/// What `fill_path`/`fill_rect` paints with.
#[derive(Clone, Debug)]
enum Paint {
    Gray(f64),
    Color(PaletteColor),
    Gradient(GradientPaint),
}

#[derive(Clone, Debug)]
struct Clip {
    /// The clip's rectangle in device space.
    rect: Rect,
    /// Device-space coverage at `rect`'s scale, `None` while the clip is the plain rectangle. Shared,
    /// not owned: the mask is the size of the whole canvas and every fill clones the state.
    mask: Option<Arc<Gray8Image>>,
}

impl Clip {
    fn covering(size: Size) -> Self {
        Clip {
            rect: Rect::new(0.0, 0.0, size.width, size.height),
            mask: None,
        }
    }
}

#[derive(Clone, Debug)]
struct State {
    ctm: AffineTransform,
    clip: Clip,
    paint: Paint,
    alpha: f64,
    blend_mode: LayerBlendMode,
    antialias: bool,
    interpolation: InterpolationQuality,
}

enum Target {
    Rgba(Rgba8Image),
    Gray(Gray8Image),
}

/// A bitmap drawing surface.
pub struct Canvas {
    target: Target,
    state: State,
    stack: Vec<State>,
}

impl Canvas {
    fn new(target: Target) -> Self {
        let size = match &target {
            Target::Rgba(image) => Size::new(image.width() as f64, image.height() as f64),
            Target::Gray(image) => Size::new(image.width() as f64, image.height() as f64),
        };
        Canvas {
            target,
            state: State {
                ctm: AffineTransform::IDENTITY,
                clip: Clip::covering(size),
                paint: Paint::Gray(0.0),
                alpha: 1.0,
                blend_mode: LayerBlendMode::Normal,
                antialias: true,
                interpolation: InterpolationQuality::High,
            },
            stack: Vec::new(),
        }
    }

    pub fn new_rgba(width: usize, height: usize) -> Self {
        Self::new(Target::Rgba(Rgba8Image::new(width, height)))
    }

    pub fn new_gray(width: usize, height: usize) -> Self {
        Self::new(Target::Gray(Gray8Image::new(width, height)))
    }

    pub fn from_rgba(image: Rgba8Image) -> Self {
        Self::new(Target::Rgba(image))
    }

    pub fn from_gray(image: Gray8Image) -> Self {
        Self::new(Target::Gray(image))
    }

    pub fn is_mask(&self) -> bool {
        matches!(self.target, Target::Gray(_))
    }

    pub fn width(&self) -> usize {
        match &self.target {
            Target::Rgba(image) => image.width(),
            Target::Gray(image) => image.width(),
        }
    }

    pub fn height(&self) -> usize {
        match &self.target {
            Target::Rgba(image) => image.height(),
            Target::Gray(image) => image.height(),
        }
    }

    pub fn pixel_size(&self) -> Size {
        Size::new(self.width() as f64, self.height() as f64)
    }

    /// `CGContext.boundingBoxOfClipPath`: the device-space bounding box of the current clip.
    pub fn clip_bounds(&self) -> Rect {
        self.state.clip.rect
    }

    pub fn save(&mut self) {
        self.stack.push(self.state.clone());
    }

    pub fn restore(&mut self) {
        if let Some(state) = self.stack.pop() {
            self.state = state;
        }
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.state.ctm = self.state.ctm.concatenating(AffineTransform::translation(dx, dy));
    }

    pub fn scale(&mut self, sx: f64, sy: f64) {
        self.state.ctm = self.state.ctm.concatenating(AffineTransform::scale(sx, sy));
    }

    pub fn rotate(&mut self, radians: f64) {
        self.state.ctm = self.state.ctm.concatenating(AffineTransform::rotation(radians));
    }

    pub fn concatenate(&mut self, transform: AffineTransform) {
        self.state.ctm = self.state.ctm.concatenating(transform);
    }

    pub fn ctm(&self) -> AffineTransform {
        self.state.ctm
    }

    /// Device pixels per user-space unit along x.
    pub fn user_space_to_device(&self) -> AffineTransform {
        self.state.ctm
    }

    pub fn device_scale(&self) -> f64 {
        let t = self.state.ctm;
        (t.a * t.a + t.b * t.b).sqrt()
    }

    /// Clips to a path in current user space, intersecting with the existing clip.
    pub fn clip_path(&mut self, path: &Path, rule: FillRule) {
        let coverage = self.path_coverage(path, Some(rule));
        self.intersect_clip(coverage);
    }

    pub fn clip_rect(&mut self, rect: Rect) {
        self.intersect_clip(self.rect_coverage(rect));
    }

    /// `CGContext.clip(to:mask:)`: the image's coverage multiplies the clip.
    pub fn clip_to_image(&mut self, image: &Gray8Image, rect: Rect) {
        let coverage = self.image_coverage(image, rect);
        self.intersect_clip(coverage);
    }

    /// Clips everything away, `CGContext.clip(to: .zero)`.
    pub fn clip_to_zero(&mut self) {
        self.intersect_clip(Coverage::empty());
    }

    pub fn set_fill_color(&mut self, color: PaletteColor) {
        self.state.paint = Paint::Color(color);
    }

    pub fn set_fill_gray(&mut self, value: f64) {
        self.state.paint = Paint::Gray(value);
    }

    pub fn set_fill_gradient(&mut self, gradient: GradientPaint) {
        self.state.paint = Paint::Gradient(gradient);
    }

    pub fn set_alpha(&mut self, alpha: f64) {
        self.state.alpha = alpha;
    }

    pub fn fill_alpha(&self) -> f64 {
        self.state.alpha
    }

    pub fn set_blend_mode(&mut self, mode: LayerBlendMode) {
        self.state.blend_mode = mode;
    }

    pub fn blend_mode(&self) -> LayerBlendMode {
        self.state.blend_mode
    }

    pub fn set_should_antialias(&mut self, value: bool) {
        self.state.antialias = value;
    }

    pub fn set_interpolation_quality(&mut self, quality: InterpolationQuality) {
        self.state.interpolation = quality;
    }

    pub fn fill_path(&mut self, path: &Path, rule: FillRule) {
        let coverage = self.path_coverage(path, Some(rule));
        self.paint_coverage(&coverage);
    }

    /// Strokes `path`: the outline `CGPath.copy(strokingWithWidth:…)` produces, filled.
    pub fn stroke_path(
        &mut self,
        path: &Path,
        rule: FillRule,
        width: f64,
        cap: LineCap,
        join: LineJoin,
        miter_limit: f64,
    ) {
        let outline = compositor_rs_core::path_ops::stroking_with_width(path, width, cap, join, miter_limit, None);
        self.fill_path(&outline, rule);
    }

    pub fn fill_rect(&mut self, rect: Rect) {
        if self.fill_rect_direct(rect) {
            return;
        }
        let coverage = self.rect_coverage(rect);
        self.paint_coverage(&coverage);
    }

    pub fn fill_rects(&mut self, rects: &[Rect]) {
        for rect in rects {
            self.fill_rect(*rect);
        }
    }

    /// Fills an axis-aligned checkerboard directly when the target and clip are simple rectangles.
    /// Canvas backgrounds use opaque colors, so replacing pixels is equivalent to source-over and
    /// avoids constructing a coverage buffer for every tile.
    pub fn fill_checkerboard(
        &mut self,
        rect: Rect,
        origin: Point,
        tile: f64,
        dark: [u8; 4],
        light: [u8; 4],
    ) {
        if tile <= 0.0 || self.state.ctm != AffineTransform::IDENTITY || self.state.clip.mask.is_some() {
            self.set_fill_color(PaletteColor::from_bytes([dark[0], dark[1], dark[2]]));
            self.set_alpha(dark[3] as f64 / 255.0);
            self.fill_rect(rect);
            return;
        }
        let bounds = rect.intersection(self.state.clip.rect).intersection(self.rgba().rect()).integral();
        if bounds.is_empty() {
            return;
        }
        let width = self.width();
        let data = match &mut self.target {
            Target::Rgba(image) => image.data_mut(),
            Target::Gray(_) => {
                self.set_fill_gray(dark[0] as f64 / 255.0);
                self.fill_rect(rect);
                return;
            }
        };
        for y in bounds.min_y() as usize..bounds.max_y() as usize {
            let row = (y as f64 + 0.5 - origin.y) / tile;
            let tile_y = row.floor() as i64;
            for x in bounds.min_x() as usize..bounds.max_x() as usize {
                let column = (x as f64 + 0.5 - origin.x) / tile;
                let color = if (column.floor() as i64 + tile_y).rem_euclid(2) == 0 {
                    dark
                } else {
                    light
                };
                let offset = (y * width + x) * 4;
                data[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }

    /// `CGContext.clear(_:)`: erases `rect` (current user space, current clip applied) back to
    /// transparent — or to black on a mask target. Edges antialias by scaling the existing pixel by
    /// `1 - coverage`, exactly what clearing through a coverage mask does.
    pub fn clear(&mut self, rect: Rect) {
        let coverage = self.rect_coverage(rect);
        self.erase_with_coverage(&coverage);
    }

    /// A `.destinationOut` fill of `rect` with the current paint's coverage: the same erase, kept
    /// separate because callers use it for brush erasing rather than for recompositing.
    pub fn fill_destination_out(&mut self, rect: Rect) {
        let coverage = self.rect_coverage(rect);
        self.erase_with_coverage(&coverage);
    }

    /// A `.destinationOut` fill of a path.
    pub fn fill_path_destination_out(&mut self, path: &Path, rule: FillRule) {
        let coverage = self.path_coverage(path, Some(rule));
        self.erase_with_coverage(&coverage);
    }

    fn erase_with_coverage(&mut self, coverage: &Coverage) {
        let clip = self.state.clip.clone();
        let alpha = self.state.alpha;
        let (min_x, max_x) = (coverage.rect.min_x() as i64, coverage.rect.max_x() as i64);
        let (min_y, max_y) = (coverage.rect.min_y() as i64, coverage.rect.max_y() as i64);
        let width = self.width();
        let height = self.height();
        for y in min_y..max_y {
            if y < 0 || y >= height as i64 {
                continue;
            }
            for x in min_x..max_x {
                if x < 0 || x >= width as i64 {
                    continue;
                }
                let value = coverage.at(x, y) * clip.coverage_at(x, y) * alpha;
                if value <= 0.0 {
                    continue;
                }
                let keep = 1.0 - value;
                let (x, y) = (x as usize, y as usize);
                match &mut self.target {
                    Target::Rgba(image) => {
                        let pixel = image.get(x, y);
                        image.set(
                            x,
                            y,
                            [
                                (pixel[0] as f64 * keep).round() as u8,
                                (pixel[1] as f64 * keep).round() as u8,
                                (pixel[2] as f64 * keep).round() as u8,
                                (pixel[3] as f64 * keep).round() as u8,
                            ],
                        );
                    }
                    Target::Gray(image) => {
                        let existing = image.get(x, y) as f64 * keep;
                        image.set(x, y, existing.round() as u8);
                    }
                }
            }
        }
    }

    /// Fills `rect` (user space) with a gradient.
    pub fn fill_gradient(&mut self, rect: Rect, gradient: &GradientPaint) {
        let saved = self.state.paint.clone();
        self.state.paint = Paint::Gradient(gradient.clone());
        let coverage = self.rect_coverage(rect);
        self.paint_coverage(&coverage);
        self.state.paint = saved;
    }

    pub fn draw_image(&mut self, image: &Rgba8Image, rect: Rect) {
        if matches!(self.target, Target::Gray(_)) {
            // A gray target has no channels for color; the caller draws coverage instead.
            let gray = Gray8Image::from_data(
                image.width().max(1),
                image.height().max(1),
                image.pixels().map(|p| p[3]).collect(),
            );
            self.draw_gray(&gray, rect);
            return;
        }
        self.draw_rgba(image, rect, false);
    }

    pub fn draw_gray(&mut self, image: &Gray8Image, rect: Rect) {
        let coverage = self.image_coverage(image, rect);
        self.paint_coverage(&coverage);
    }

    /// Renders the image's pixels as coverage, bypassing color conversion.
    pub fn draw_coverage(&mut self, image: &Gray8Image, rect: Rect) {
        self.draw_gray(image, rect);
    }

    pub fn into_rgba(self) -> Rgba8Image {
        match self.target {
            Target::Rgba(image) => image,
            Target::Gray(_) => panic!("canvas target is gray"),
        }
    }

    pub fn into_gray(self) -> Gray8Image {
        match self.target {
            Target::Gray(image) => image,
            Target::Rgba(_) => panic!("canvas target is color"),
        }
    }

    pub fn rgba(&self) -> &Rgba8Image {
        match &self.target {
            Target::Rgba(image) => image,
            Target::Gray(_) => panic!("canvas target is gray"),
        }
    }

    pub fn gray(&self) -> &Gray8Image {
        match &self.target {
            Target::Gray(image) => image,
            Target::Rgba(_) => panic!("canvas target is color"),
        }
    }

    /// `CGContext.makeImage()`: a snapshot of the target.
    pub fn snapshot(&self) -> Rgba8Image {
        self.rgba().clone()
    }

    pub fn snapshot_gray(&self) -> Gray8Image {
        self.gray().clone()
    }

    // MARK: - Coverage

    fn path_coverage(&self, path: &Path, rule: Option<FillRule>) -> Coverage {
        let polygons = flatten(path, &self.state.ctm);
        // A `CGPath` carries no fill rule; `fillPath(using:)` supplies it, and Winding is the default.
        let rule = rule.unwrap_or(FillRule::Winding);
        rasterize_polygons(&polygons, rule, self.state.antialias, self.pixel_size())
    }

    fn rect_coverage(&self, rect: Rect) -> Coverage {
        let corners = [
            self.state.ctm.applying(rect.origin),
            self.state.ctm.applying(Point::new(rect.max_x(), rect.min_y())),
            self.state.ctm.applying(Point::new(rect.max_x(), rect.max_y())),
            self.state.ctm.applying(Point::new(rect.min_x(), rect.max_y())),
        ];
        if let Some(coverage) = self.axis_aligned_coverage(&corners) {
            return coverage;
        }
        let subpath = Subpath { points: corners.to_vec(), closed: true };
        rasterize_polygons(
            &[subpath],
            FillRule::Winding,
            self.state.antialias,
            self.pixel_size(),
        )
    }

    /// The scan converter's result for an axis-aligned rectangle, worked out directly.
    ///
    /// Without rotation the four corners share two x values and two y values, so every sample row
    /// crosses the same span and a pixel's coverage is that span added once per sample row that falls
    /// inside, over four — the additions [`rasterize_polygons`] would have made, in the same order.
    /// Clipping to the canvas is what the general routine does with its bounds too. `None` for a
    /// corner that is not finite, or a shape that is not axis aligned, sends the caller to the scan
    /// converter.
    fn axis_aligned_coverage(&self, corners: &[Point; 4]) -> Option<Coverage> {
        if self.state.ctm.b != 0.0 || self.state.ctm.c != 0.0 {
            return None;
        }
        let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
        for corner in corners {
            if !corner.x.is_finite() || !corner.y.is_finite() {
                return None;
            }
            min_x = min_x.min(corner.x);
            max_x = max_x.max(corner.x);
            min_y = min_y.min(corner.y);
            max_y = max_y.max(corner.y);
        }
        let size = self.pixel_size();
        let (left, right) = (min_x.floor().max(0.0), max_x.ceil().min(size.width));
        let (top, bottom) = (min_y.floor().max(0.0), max_y.ceil().min(size.height));
        if bottom <= top || right <= left {
            return Some(Coverage::empty());
        }
        let samples = if self.state.antialias { 4.0 } else { 1.0 };
        let (width, height) = ((right - left) as usize, (bottom - top) as usize);
        // The span each pixel holds, which no sample row changes. This is `add_span`'s overlap.
        let mut spans = vec![0.0f64; width];
        for (column, span) in spans.iter_mut().enumerate() {
            let pixel_start = left + column as f64;
            let overlap = max_x.min(pixel_start + 1.0) - min_x.max(pixel_start);
            if overlap > 0.0 {
                *span = overlap;
            }
        }
        let mut values = vec![0.0f64; width * height];
        for row in 0..height {
            let pixel_y = top + row as f64;
            let mut inside = 0;
            for sample in 0..samples as i32 {
                let sample_y = pixel_y + (sample as f64 + 0.5) / samples;
                if sample_y >= min_y && sample_y < max_y {
                    inside += 1;
                }
            }
            if inside == 0 {
                continue;
            }
            for column in 0..width {
                let mut value = 0.0;
                for _ in 0..inside {
                    value += spans[column];
                }
                values[row * width + column] = (value / samples).clamp(0.0, 1.0);
            }
        }
        Some(Coverage {
            rect: Rect::new(left, top, width as f64, height as f64),
            values,
        })
    }

    fn image_coverage(&self, image: &Gray8Image, rect: Rect) -> Coverage {
        if rect.is_empty() || image.is_empty() {
            return Coverage::empty();
        }
        if image.width() == 1 && image.height() == 1 {
            // A uniform mask keeps the same shape at full coverage. `Coverage` is kept in device
            // space, so the rect is mapped through the CTM first: a context that is scaled or
            // translated (the canvas's viewport) would otherwise clip at the wrong place.
            let (min_y, max_y, min_x, max_x) = self.device_bounds(rect);
            return Coverage::uniform(
                Rect::new(
                    min_x as f64,
                    min_y as f64,
                    (max_x - min_x) as f64,
                    (max_y - min_y) as f64,
                ),
                image.get(0, 0) as f64 / 255.0,
                self.pixel_size(),
            );
        }
        let bounds = self.device_bounds(rect);
        let mut coverage = Coverage::zeroed(Rect::new(
            bounds.2 as f64,
            bounds.0 as f64,
            (bounds.3 - bounds.2) as f64,
            (bounds.1 - bounds.0) as f64,
        ));
        let inverse = self.state.ctm.inverted();
        for y in bounds.0..bounds.1 {
            for x in bounds.2..bounds.3 {
                let device = Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let user = inverse.applying(device);
                let u = (user.x - rect.min_x()) / rect.width();
                let v = (user.y - rect.min_y()) / rect.height();
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                // Core Graphics edge-extends when it resamples a mask: a sample just outside the
                // image keeps the border pixel's value, so a clip along the image's own edge stays
                // fully covered instead of fading toward zero. Only the clip path does this here;
                // image draws keep their own sampling.
                let sx = (u * image.width() as f64 - 0.5).clamp(0.0, image.width() as f64 - 1.0);
                let sy = (v * image.height() as f64 - 0.5).clamp(0.0, image.height() as f64 - 1.0);
                let value = sample_u8(image, sx, sy, self.state.interpolation);
                coverage.set(x - bounds.2, y - bounds.0, value as f64 / 255.0);
            }
        }
        coverage
    }

    fn device_bounds(&self, rect: Rect) -> (usize, usize, usize, usize) {
        let corners = [
            self.state.ctm.applying(rect.origin),
            self.state.ctm.applying(Point::new(rect.max_x(), rect.min_y())),
            self.state.ctm.applying(Point::new(rect.max_x(), rect.max_y())),
            self.state.ctm.applying(Point::new(rect.min_x(), rect.max_y())),
        ];
        // Both edges clamp into the canvas: a rect lying fully outside collapses to an empty range
        // (Swift draws nothing there) instead of leaving min > max, which would underflow below.
        let min_x = corners
            .iter()
            .map(|p| p.x)
            .fold(f64::INFINITY, f64::min)
            .floor()
            .clamp(0.0, self.width() as f64);
        let min_y = corners
            .iter()
            .map(|p| p.y)
            .fold(f64::INFINITY, f64::min)
            .floor()
            .clamp(0.0, self.height() as f64);
        let max_x = corners
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .clamp(0.0, self.width() as f64);
        let max_y = corners
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .clamp(0.0, self.height() as f64);
        (min_y as usize, max_y as usize, min_x as usize, max_x as usize)
    }

    fn draw_rgba(&mut self, image: &Rgba8Image, rect: Rect, gray_target: bool) {
        debug_assert!(!gray_target);
        if rect.is_empty() || image.is_empty() {
            return;
        }
        let ctm = self.state.ctm;
        let inverse = ctm.inverted();
        let (min_y, max_y, min_x, max_x) = self.device_bounds(rect);
        let target_size = self.pixel_size();
        let clip = self.state.clip.clone();
        // A sprite drawn back where it was rasterized — the canvas's shadow — puts one source pixel
        // on each destination pixel with a whole-pixel offset. Bilinear sampling lands exactly on a
        // texel centre there, where the three other taps are multiplied by zero, so it can only ever
        // return that texel: taking it directly is the same bytes without the resampling, and without
        // the per-pixel transform either.
        //
        // `InterpolationQuality::None` rounds the sample, which lands on the same texel, so it agrees.
        let aligned = ctm.b == 0.0
            && ctm.c == 0.0
            && ctm.a == 1.0
            && ctm.d == 1.0
            && ctm.tx.fract() == 0.0
            && ctm.ty.fract() == 0.0
            && rect.min_x().fract() == 0.0
            && rect.min_y().fract() == 0.0
            && rect.width() == image.width() as f64
            && rect.height() == image.height() as f64;
        if aligned {
            let origin_x = rect.min_x() as i64 + ctm.tx as i64;
            let origin_y = rect.min_y() as i64 + ctm.ty as i64;
            for y in min_y..max_y {
                for x in min_x..max_x {
                    let (source_x, source_y) = (x as i64 - origin_x, y as i64 - origin_y);
                    if source_x < 0
                        || source_y < 0
                        || source_x >= image.width() as i64
                        || source_y >= image.height() as i64
                    {
                        continue;
                    }
                    let coverage = clip.coverage_at(x as i64, y as i64) * self.state.alpha;
                    if coverage <= 0.0 {
                        continue;
                    }
                    let pixel = image.get(source_x as usize, source_y as usize);
                    self.blend_into_target(x, y, scale_premultiplied(pixel, coverage));
                }
            }
            return;
        }
        for y in min_y..max_y {
            for x in min_x..max_x {
                let device = Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let user = inverse.applying(device);
                let u = (user.x - rect.min_x()) / rect.width();
                let v = (user.y - rect.min_y()) / rect.height();
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                let coverage = clip.coverage_at(x as i64, y as i64) * self.state.alpha;
                if coverage <= 0.0 {
                    continue;
                }
                let pixel = sample_rgba(
                    image,
                    u * image.width() as f64 - 0.5,
                    v * image.height() as f64 - 0.5,
                    self.state.interpolation,
                );
                let blended = scale_premultiplied(pixel, coverage);
                self.blend_into_target(x, y, blended);
                let _ = target_size;
            }
        }
    }

    fn intersect_clip(&mut self, coverage: Coverage) {
        let current = self.state.clip.clone();
        let rect = current.rect.intersection(coverage.rect);
        if rect.is_empty() {
            self.state.clip = Clip {
                rect: Rect::new(0.0, 0.0, 0.0, 0.0),
                mask: None,
            };
            return;
        }
        let width = rect.width().max(0.0) as usize;
        let height = rect.height().max(0.0) as usize;
        let mut mask = Gray8Image::new(width, height);
        // The clip is kept as a rect plus an optional mask, so a coverage that is flat but not fully
        // opaque — a half-transparent `clip(to:mask:)` — has to stay in the mask; only a product of
        // 1.0 everywhere can be left to the rect alone.
        let uniform_one = coverage.is_uniform() && coverage.values.first() == Some(&1.0);
        let has_mask = current.mask.is_some() || !uniform_one;
        if has_mask {
            for y in 0..height {
                for x in 0..width {
                    let device_x = rect.min_x() as i64 + x as i64;
                    let device_y = rect.min_y() as i64 + y as i64;
                    let value = current.coverage_at(device_x, device_y) * coverage.at(device_x, device_y);
                    mask.set(x, y, to_byte(value));
                }
            }
        }
        self.state.clip = Clip {
            rect,
            mask: if has_mask { Some(Arc::new(mask)) } else { None },
        };
    }

    fn paint_coverage(&mut self, coverage: &Coverage) {
        let rect = coverage.rect;
        if rect.is_empty() {
            return;
        }
        let alpha = self.state.alpha;
        let paint = self.state.paint.clone();
        let clip = self.state.clip.clone();
        let ctm = self.state.ctm;
        let (min_x, max_x) = (rect.min_x() as i64, rect.max_x() as i64);
        let (min_y, max_y) = (rect.min_y() as i64, rect.max_y() as i64);
        let width = self.width();
        let height = self.height();
        let inverse = ctm.inverted();
        for y in min_y..max_y {
            if y < 0 || y >= height as i64 {
                continue;
            }
            for x in min_x..max_x {
                if x < 0 || x >= width as i64 {
                    continue;
                }
                let value = coverage.at(x, y);
                self.emit_pixel(x, y, value, &paint, &clip, alpha, inverse);
            }
        }
    }

    /// Emits the current paint's coverage `value` at one device pixel: the body every fill shares, so
    /// a direct fill and a coverage fill cannot drift apart.
    #[allow(clippy::too_many_arguments)]
    fn emit_pixel(
        &mut self,
        x: i64,
        y: i64,
        value: f64,
        paint: &Paint,
        clip: &Clip,
        alpha: f64,
        inverse: AffineTransform,
    ) {
        if value <= 0.0 {
            return;
        }
        let value = value * (clip.coverage_at(x, y) * alpha);
        if value <= 0.0 {
            return;
        }
        let pixel = Self::paint_pixel(paint, value, inverse, x, y);
        self.blend_into_target(x as usize, y as usize, pixel);
    }

    /// The paint's premultiplied bytes at `value` coverage, before the clip and the blend. Split out
    /// of [`Self::emit_pixel`] so a fill that covers a whole run of pixels with the same value can
    /// work the bytes out once.
    fn paint_pixel(paint: &Paint, value: f64, inverse: AffineTransform, x: i64, y: i64) -> [u8; 4] {
        match paint {
            Paint::Gray(gray) => {
                // Premultiplied like the other paints: a gray fill at half alpha carries half the
                // gray, and the alpha is the coverage (`CGColor(gray:alpha:)`).
                let byte = to_byte(gray * value);
                [byte, byte, byte, to_byte(value)]
            }
            Paint::Color(color) => [
                to_byte(color.red * value),
                to_byte(color.green * value),
                to_byte(color.blue * value),
                to_byte(value),
            ],
            Paint::Gradient(gradient) => {
                let device = Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let user = inverse.applying(device);
                let color = gradient_color(gradient, user);
                [
                    to_byte(color[0] * color[3] * value),
                    to_byte(color[1] * color[3] * value),
                    to_byte(color[2] * color[3] * value),
                    to_byte(color[3] * value),
                ]
            }
        }
    }

    /// Puts an opaque source pixel in place without a blend: `source_over` returns the source
    /// unchanged when its alpha is 255, so the whole conversion is unnecessary.
    fn store_opaque(&mut self, x: i64, y: i64, source: [u8; 4]) {
        if let Target::Rgba(image) = &mut self.target {
            image.set(x as usize, y as usize, source);
        }
    }

    /// The same, through a [`SolidSourceOver`] table, for a source that is not opaque.
    fn blend_table(&mut self, x: i64, y: i64, table: &SolidSourceOver) {
        if let Target::Rgba(image) = &mut self.target {
            let backdrop = image.get(x as usize, y as usize);
            image.set(x as usize, y as usize, table.blend(backdrop));
        }
    }

    /// `fill_rect` without the coverage buffer: when the transform has no rotation or skew and
    /// antialiasing is on, the scan converter's four vertical subsamples a pixel and its exact
    /// horizontal span coverage reduce to the closed form below, which lands the same bytes. Returns
    /// whether it painted; a `false` sends the caller through the general coverage path.
    fn fill_rect_direct(&mut self, rect: Rect) -> bool {
        let ctm = self.state.ctm;
        if !self.state.antialias || ctm.b != 0.0 || ctm.c != 0.0 {
            return false;
        }
        let x0 = ctm.a * rect.min_x() + ctm.tx;
        let x1 = ctm.a * rect.max_x() + ctm.tx;
        let y0 = ctm.d * rect.min_y() + ctm.ty;
        let y1 = ctm.d * rect.max_y() + ctm.ty;
        if !x0.is_finite() || !x1.is_finite() || !y0.is_finite() || !y1.is_finite() {
            return false;
        }
        let (left, right) = (x0.min(x1), x0.max(x1));
        let (top, bottom) = (y0.min(y1), y0.max(y1));
        // The coverage rasterization clips to the target and to whole pixels the same way.
        let width = self.width();
        let height = self.height();
        let first_x = left.floor().max(0.0) as i64;
        let last_x = (right.ceil().min(width as f64) as i64).max(first_x);
        let first_y = top.floor().max(0.0) as i64;
        let last_y = (bottom.ceil().min(height as f64) as i64).max(first_y);
        if last_x == first_x || last_y == first_y {
            return true;
        }
        let alpha = self.state.alpha;
        let paint = self.state.paint.clone();
        let clip = self.state.clip.clone();
        let inverse = ctm.inverted();
        // A solid paint gives every fully covered pixel the same source bytes. When they are opaque,
        // `source_over` would hand the source straight back, so those pixels are simply put there;
        // otherwise the blend is worked out as a table. The backdrop and the checkerboard — most of
        // the frame — are exactly this. A clip that only partly covers a pixel still goes the long
        // way; a mask clip reports its interior as fully covered, which is what `coverage_at` says.
        let solid = match &paint {
            Paint::Gray(_) | Paint::Color(_)
                if self.state.blend_mode == LayerBlendMode::Normal
                    && matches!(self.target, Target::Rgba(_)) =>
            {
                Some(Self::paint_pixel(&paint, alpha, inverse, 0, 0))
            }
            _ => None,
        };
        let opaque = solid.filter(|pixel| pixel[3] == 255);
        let table = solid
            .filter(|pixel| pixel[3] != 255)
            .filter(|_| (last_x - first_x) * (last_y - first_y) >= SOLID_TABLE_PIXELS)
            .map(SolidSourceOver::new);
        for y in first_y..last_y {
            // How many of the four subsamples of this row fall in `[top, bottom)`.
            let pixel_y = y as f64;
            let mut inside = 0;
            for sample in 0..4 {
                let sample_y = pixel_y + (sample as f64 + 0.5) / 4.0;
                if sample_y >= top && sample_y < bottom {
                    inside += 1;
                }
            }
            if inside == 0 {
                continue;
            }
            for x in first_x..last_x {
                let pixel_x = x as f64;
                let span = right.min(pixel_x + 1.0) - left.max(pixel_x);
                if span <= 0.0 {
                    continue;
                }
                if (opaque.is_some() || table.is_some())
                    && span >= 1.0
                    && inside == 4
                    && clip.coverage_at(x, y) == 1.0
                {
                    if let Some(pixel) = opaque {
                        self.store_opaque(x, y, pixel);
                        continue;
                    }
                    if let Some(table) = &table {
                        self.blend_table(x, y, table);
                        continue;
                    }
                }
                // `add_span` adds the span once per subsample; keep the same accumulation.
                let mut value = 0.0;
                for _ in 0..inside {
                    value += span;
                }
                let value = (value / 4.0).clamp(0.0, 1.0);
                self.emit_pixel(x, y, value, &paint, &clip, alpha, inverse);
            }
        }
        true
    }

    fn blend_into_target(&mut self, x: usize, y: usize, source: [u8; 4]) {
        let mode = self.state.blend_mode;
        match &mut self.target {
            Target::Rgba(image) => {
                if x >= image.width() || y >= image.height() {
                    return;
                }
                let backdrop = image.get(x, y);
                let result = if mode == LayerBlendMode::Normal {
                    source_over(backdrop, source)
                } else {
                    crate::blend::blend_pixel(mode, backdrop, source)
                };
                image.set(x, y, result);
            }
            Target::Gray(image) => {
                if x >= image.width() || y >= image.height() {
                    return;
                }
                // A mask target has no channels for color: the paint's own gray is what lands, as
                // `source_over`'s single channel — premultiplied gray over what is there. Using the
                // coverage alone would make every fill white, black ones included.
                let existing = unit(image.get(x, y));
                let source_alpha = unit(source[3]);
                let value = unit(source[0]) + existing * (1.0 - source_alpha);
                image.set(x, y, to_byte(value));
            }
        }
    }
}

/// Device-space coverage for one drawing operation.
struct Coverage {
    rect: Rect,
    values: Vec<f64>,
}

impl Coverage {
    /// No coverage at all: a null rectangle with no buffer. `at` reports zero everywhere and
    /// `is_empty`-style callers treat it as nothing painted.
    fn empty() -> Self {
        Coverage {
            rect: Rect::NULL,
            values: Vec::new(),
        }
    }

    /// A coverage buffer over exactly `rect`, all zero — the allocation the map-a-source-image paths
    /// need, since they only touch the device bounds their source lands in.
    fn zeroed(rect: Rect) -> Self {
        let width = rect.width().max(0.0) as usize;
        let height = rect.height().max(0.0) as usize;
        if width == 0 || height == 0 {
            return Coverage::empty();
        }
        Coverage {
            rect,
            values: vec![0.0; width * height],
        }
    }

    fn uniform(rect: Rect, value: f64, size: Size) -> Self {
        let clipped = rect.integral().intersection(Rect::from_origin_size(Point::ZERO, size));
        if clipped.is_empty() {
            return Coverage::empty();
        }
        let width = clipped.width().max(0.0) as usize;
        let height = clipped.height().max(0.0) as usize;
        Coverage {
            rect: Rect::new(clipped.min_x().floor(), clipped.min_y().floor(), width as f64, height as f64),
            values: vec![value; width * height],
        }
    }

    fn is_uniform(&self) -> bool {
        match self.values.first() {
            None => true,
            Some(first) => self.values.iter().all(|value| value == first),
        }
    }

    fn at(&self, x: i64, y: i64) -> f64 {
        if self.rect.is_null() {
            return 0.0;
        }
        let local_x = x - self.rect.min_x() as i64;
        let local_y = y - self.rect.min_y() as i64;
        if local_x < 0 || local_y < 0 || local_x as usize >= self.rect.width() as usize || local_y as usize >= self.rect.height() as usize {
            return 0.0;
        }
        self.values[local_y as usize * self.rect.width() as usize + local_x as usize]
    }

    fn set(&mut self, x: usize, y: usize, value: f64) {
        let index = y * self.rect.width() as usize + x;
        if index < self.values.len() {
            self.values[index] = value;
        }
    }
}

impl Clip {
    fn coverage_at(&self, x: i64, y: i64) -> f64 {
        if x < self.rect.min_x() as i64
            || y < self.rect.min_y() as i64
            || x >= self.rect.max_x() as i64
            || y >= self.rect.max_y() as i64
        {
            return 0.0;
        }
        match &self.mask {
            None => 1.0,
            Some(mask) => {
                let local_x = x - self.rect.min_x() as i64;
                let local_y = y - self.rect.min_y() as i64;
                if local_x < 0 || local_y < 0 || local_x as usize >= mask.width() || local_y as usize >= mask.height() {
                    return 0.0;
                }
                unit(mask.get(local_x as usize, local_y as usize))
            }
        }
    }
}

/// `source_over` for one constant source pixel: the same arithmetic, worked out once for each of the
/// 256 byte values a backdrop channel can take instead of once per pixel. A fill of a colour that is
/// not opaque — a shadow, a selection, a tint — has one source for the whole of it.
struct SolidSourceOver {
    alpha: [u8; 256],
    channels: [[u8; 256]; 3],
}

impl SolidSourceOver {
    fn new(source: [u8; 4]) -> Self {
        let sa = unit(source[3]);
        let mut table = Self {
            alpha: [0; 256],
            channels: [[0; 256]; 3],
        };
        for byte in 0..256usize {
            let backdrop_alpha = unit(byte as u8);
            for channel in 0..3 {
                table.channels[channel][byte] =
                    to_byte(unit(source[channel]) + backdrop_alpha * (1.0 - sa));
            }
            table.alpha[byte] = to_byte(sa + backdrop_alpha * (1.0 - sa));
        }
        table
    }

    fn blend(&self, backdrop: [u8; 4]) -> [u8; 4] {
        // `source_over` leaves early when the result would be nothing at all, which takes a fully
        // transparent source *and* backdrop — the only way `sa + da·(1 − sa)` reaches zero, since a
        // byte that is not zero carries at least 1/255.
        let alpha = self.alpha[backdrop[3] as usize];
        if alpha == 0 {
            return [0, 0, 0, 0];
        }
        [
            self.channels[0][backdrop[0] as usize],
            self.channels[1][backdrop[1] as usize],
            self.channels[2][backdrop[2] as usize],
            alpha,
        ]
    }
}

/// The unit value of a byte. See [`BYTE_TO_UNIT`].
fn unit(byte: u8) -> f64 {
    BYTE_TO_UNIT[byte as usize]
}

/// A flat fill of at least this many pixels is worth building a [`SolidSourceOver`] table for: the
/// table is four times 256 conversions, so a smaller fill would spend more on it than it saves.
const SOLID_TABLE_PIXELS: i64 = 4_000;

fn sample_rgba(image: &Rgba8Image, x: f64, y: f64, quality: InterpolationQuality) -> [u8; 4] {
    match quality {
        InterpolationQuality::None => {
            let (sx, sy) = (x.round(), y.round());
            if sx < 0.0 || sy < 0.0 || sx as usize >= image.width() || sy as usize >= image.height() {
                return [0, 0, 0, 0];
            }
            image.get(sx as usize, sy as usize)
        }
        _ => {
            let x0 = x.floor();
            let y0 = y.floor();
            let fx = x - x0;
            let fy = y - y0;
            let mut result = [0.0f64; 4];
            // When all four taps are inside the image there is nothing to test, which is every sample
            // a resampled layer takes but the ones along its edge; the arithmetic is untouched.
            let interior = x0 >= 0.0
                && y0 >= 0.0
                && x0 + 1.0 < image.width() as f64
                && y0 + 1.0 < image.height() as f64;
            for (dx, wx) in [(0.0, 1.0 - fx), (1.0, fx)] {
                for (dy, wy) in [(0.0, 1.0 - fy), (1.0, fy)] {
                    let sx = x0 + dx;
                    let sy = y0 + dy;
                    if !interior
                        && (sx < 0.0
                            || sy < 0.0
                            || sx as usize >= image.width()
                            || sy as usize >= image.height())
                    {
                        continue;
                    }
                    let pixel = image.get(sx as usize, sy as usize);
                    for channel in 0..4 {
                        result[channel] += pixel[channel] as f64 * wx * wy;
                    }
                }
            }
            [
                result[0].round().clamp(0.0, 255.0) as u8,
                result[1].round().clamp(0.0, 255.0) as u8,
                result[2].round().clamp(0.0, 255.0) as u8,
                result[3].round().clamp(0.0, 255.0) as u8,
            ]
        }
    }
}

fn sample_u8(image: &Gray8Image, x: f64, y: f64, quality: InterpolationQuality) -> u8 {
    match quality {
        InterpolationQuality::None => {
            let (sx, sy) = (x.round(), y.round());
            if sx < 0.0 || sy < 0.0 || sx as usize >= image.width() || sy as usize >= image.height() {
                return 0;
            }
            image.get(sx as usize, sy as usize)
        }
        _ => {
            let x0 = x.floor();
            let y0 = y.floor();
            let fx = x - x0;
            let fy = y - y0;
            let mut value = 0.0;
            for (dx, wx) in [(0.0, 1.0 - fx), (1.0, fx)] {
                for (dy, wy) in [(0.0, 1.0 - fy), (1.0, fy)] {
                    let sx = x0 + dx;
                    let sy = y0 + dy;
                    if sx < 0.0 || sy < 0.0 || sx as usize >= image.width() || sy as usize >= image.height() {
                        continue;
                    }
                    value += image.get(sx as usize, sy as usize) as f64 * wx * wy;
                }
            }
            value.round().clamp(0.0, 255.0) as u8
        }
    }
}

/// Source-over for premultiplied pixels.
pub fn source_over(backdrop: [u8; 4], source: [u8; 4]) -> [u8; 4] {
    let sa = BYTE_TO_UNIT[source[3] as usize];
    let da = BYTE_TO_UNIT[backdrop[3] as usize];
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let mut result = [0u8; 4];
    for channel in 0..3 {
        let value =
            BYTE_TO_UNIT[source[channel] as usize] + BYTE_TO_UNIT[backdrop[channel] as usize] * (1.0 - sa);
        result[channel] = to_byte(value);
    }
    result[3] = to_byte(out_a);
    result
}

/// `byte as f64 / 255.0`, the division every blend does. Precomputed so the inner loops keep to
/// multiplies and adds; the entries are the same division, so results do not move.
static BYTE_TO_UNIT: [f64; 256] = {
    let mut table = [0.0; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = byte as f64 / 255.0;
        byte += 1;
    }
    table
};

fn scale_premultiplied(pixel: [u8; 4], coverage: f64) -> [u8; 4] {
    // Adding a half and truncating rounds and saturates exactly as `round` then a clamp does, for any
    // value a byte times a coverage can take: see [`to_byte`], which relies on the same identity.
    [
        (pixel[0] as f64 * coverage + 0.5) as u8,
        (pixel[1] as f64 * coverage + 0.5) as u8,
        (pixel[2] as f64 * coverage + 0.5) as u8,
        (pixel[3] as f64 * coverage + 0.5) as u8,
    ]
}

fn gradient_color(gradient: &GradientPaint, point: Point) -> [f64; 4] {
    let t = match gradient.kind {
        GradientKind::Linear { from, to } => {
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let length_squared = dx * dx + dy * dy;
            if length_squared <= 0.0 {
                0.0
            } else {
                ((point.x - from.x) * dx + (point.y - from.y) * dy) / length_squared
            }
        }
        GradientKind::Radial { center, radius } => {
            if radius <= 0.0 {
                0.0
            } else {
                ((point.x - center.x).powi(2) + (point.y - center.y).powi(2)).sqrt() / radius
            }
        }
    };
    let t = match gradient.extend {
        GradientExtend::None => t,
        GradientExtend::Pad => t.clamp(0.0, 1.0),
    };
    if gradient.extend == GradientExtend::None && !(0.0..=1.0).contains(&t) {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let stops = &gradient.stops;
    if stops.is_empty() {
        return [0.0, 0.0, 0.0, 0.0];
    }
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for window in stops.windows(2) {
        let (location0, color0) = window[0];
        let (location1, color1) = window[1];
        if t >= location0 && t <= location1 {
            let span = location1 - location0;
            let f = if span <= 0.0 { 0.0 } else { (t - location0) / span };
            let mut result = [0.0; 4];
            for channel in 0..4 {
                result[channel] = color0[channel] + (color1[channel] - color0[channel]) * f;
            }
            return result;
        }
    }
    stops[stops.len() - 1].1
}

/// Scanline rasterization of device-space polygons. Antialiased fills take four vertical subsamples
/// a pixel and the exact horizontal span coverage; aliased fills take one sample at the pixel row's
/// centre and a hard horizontal rule — a pixel is in or out by its own centre, as Core Graphics'
/// aliased scan conversion has no partial coverage.
fn rasterize_polygons(polygons: &[Subpath], rule: FillRule, antialias: bool, size: Size) -> Coverage {
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    for polygon in polygons {
        for point in &polygon.points {
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
            min_x = min_x.min(point.x);
            max_x = max_x.max(point.x);
        }
    }
    if !min_y.is_finite() || !max_y.is_finite() {
        return Coverage::empty();
    }
    let top = min_y.floor().max(0.0);
    let bottom = max_y.ceil().min(size.height);
    let left = min_x.floor().max(0.0);
    let right = max_x.ceil().min(size.width);
    if bottom <= top || right <= left {
        return Coverage::empty();
    }
    let width = (right - left) as usize;
    let height = (bottom - top) as usize;
    let mut values = vec![0.0f64; width * height];
    let samples = if antialias { 4 } else { 1 };
    for row in 0..height {
        let pixel_y = top + row as f64;
        let mut row_coverage = vec![0.0f64; width];
        for sample in 0..samples {
            let y = pixel_y + (sample as f64 + 0.5) / samples as f64;
            let mut crossings: Vec<(f64, i32)> = Vec::new();
            for polygon in polygons {
                if polygon.points.len() < 2 {
                    continue;
                }
                // Filling implicitly closes an open subpath, as Core Graphics does.
                let count = polygon.points.len();
                for index in 0..count {
                    let a = polygon.points[index];
                    let b = polygon.points[(index + 1) % polygon.points.len()];
                    if (a.y <= y && b.y > y) || (b.y <= y && a.y > y) {
                        let t = (y - a.y) / (b.y - a.y);
                        crossings.push((a.x + t * (b.x - a.x), if b.y > a.y { 1 } else { -1 }));
                    }
                }
            }
            if crossings.is_empty() {
                continue;
            }
            crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut winding = 0;
            let mut even_odd = false;
            for index in 0..crossings.len().saturating_sub(1) {
                winding += crossings[index].1;
                even_odd = !even_odd;
                let inside = match rule {
                    FillRule::Winding => winding != 0,
                    FillRule::EvenOdd => even_odd,
                };
                if !inside {
                    continue;
                }
                let start = crossings[index].0;
                let end = crossings[index + 1].0;
                if end <= start {
                    continue;
                }
                if antialias {
                    add_span(&mut row_coverage, left, start, end);
                } else {
                    add_hard_span(&mut row_coverage, left, start, end);
                }
            }
        }
        let divisor = samples as f64;
        for column in 0..width {
            values[row * width + column] = (row_coverage[column] / divisor).clamp(0.0, 1.0);
        }
    }
    Coverage {
        rect: Rect::new(left, top, width as f64, height as f64),
        values,
    }
}

/// Adds the horizontal coverage of `[start, end)` to a row of pixels.
fn add_span(row: &mut [f64], left: f64, start: f64, end: f64) {
    let start = start.max(left);
    let end = end.min(left + row.len() as f64);
    if end <= start {
        return;
    }
    let first = (start - left).floor() as usize;
    let last = ((end - left).ceil() as usize).min(row.len());
    for index in first..last {
        let pixel_start = left + index as f64;
        let pixel_end = pixel_start + 1.0;
        let overlap = end.min(pixel_end) - start.max(pixel_start);
        if overlap > 0.0 {
            row[index] += overlap;
        }
    }
}

/// Adds `[start, end)` to a row of pixels the aliased way: a pixel is fully covered when its centre
/// — device `x + 0.5`, the same point the row sample tests in y — lies inside the span, and not at
/// all otherwise.
fn add_hard_span(row: &mut [f64], left: f64, start: f64, end: f64) {
    let start = start.max(left);
    let end = end.min(left + row.len() as f64);
    if end <= start {
        return;
    }
    // Index `i` has centre `left + i + 0.5`, so the first covered centre is `ceil(start - left - 0.5)`
    // and the last one is the pixel before `ceil(end - left - 0.5)`.
    let first = ((start - left) - 0.5).ceil().max(0.0) as usize;
    let last = (((end - left) - 0.5).ceil().max(0.0) as usize).min(row.len());
    for index in first..last {
        row[index] = 1.0;
    }
}

/// Path elements are exported through `compositor_rs_core::path`; this import keeps the type visible for
/// callers that build paths inline.
pub use compositor_rs_core::path::Path as CanvasPath;

// [`PathElement`] is part of the frozen `path` API.
#[allow(unused_imports)]
use PathElement as _PathElement;

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::path::Path;

    #[test]
    fn fill_rect_covers_exactly() {
        let mut canvas = Canvas::new_gray(8, 8);
        canvas.set_fill_gray(1.0);
        canvas.fill_rect(Rect::new(2.0, 2.0, 3.0, 3.0));
        let gray = canvas.snapshot_gray();
        assert_eq!(gray.get(2, 2), 255);
        assert_eq!(gray.get(4, 4), 255);
        assert_eq!(gray.get(5, 5), 0);
        assert_eq!(gray.get(1, 1), 0);
    }

    #[test]
    fn clip_intersects() {
        let mut canvas = Canvas::new_gray(8, 8);
        canvas.clip_rect(Rect::new(0.0, 0.0, 4.0, 8.0));
        canvas.clip_rect(Rect::new(2.0, 0.0, 8.0, 8.0));
        canvas.set_fill_gray(1.0);
        canvas.fill_rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        let gray = canvas.snapshot_gray();
        assert_eq!(gray.get(1, 1), 0);
        assert_eq!(gray.get(2, 1), 255);
        assert_eq!(gray.get(3, 1), 255);
        assert_eq!(gray.get(4, 1), 0);
    }

    #[test]
    fn save_restore_restores_the_clip() {
        let mut canvas = Canvas::new_gray(8, 8);
        canvas.save();
        canvas.clip_rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        canvas.restore();
        canvas.set_fill_gray(1.0);
        canvas.fill_rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        assert_eq!(canvas.gray().get(7, 7), 255);
    }

    #[test]
    fn draw_gray_paints_a_multi_pixel_mask() {
        // Regression: the image-coverage path allocated a zero-sized buffer, so every mask larger than
        // 1×1 painted nothing.
        let mut canvas = Canvas::new_rgba(20, 20);
        canvas.set_fill_color(PaletteColor::new(1.0, 0.0, 0.0));
        canvas.draw_gray(&Gray8Image::uniform(4, 2, 255), Rect::new(2.0, 2.0, 4.0, 2.0));
        let image = canvas.snapshot();
        assert_eq!(image.get(3, 3), [255, 0, 0, 255]);
        assert_eq!(image.get(0, 0), [0, 0, 0, 0]);
        assert_eq!(image.get(7, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn draw_gray_partial_values_are_coverage() {
        let mut canvas = Canvas::new_rgba(8, 8);
        canvas.set_fill_color(PaletteColor::new(0.0, 0.0, 1.0));
        canvas.draw_gray(&Gray8Image::uniform(2, 2, 128), Rect::new(0.0, 0.0, 2.0, 2.0));
        let pixel = canvas.snapshot().get(1, 1);
        assert_eq!(pixel[3], 128, "half coverage keeps half the alpha");
        assert_eq!(pixel[2], 128, "premultiplied blue tracks the coverage");
    }

    #[test]
    fn a_mask_clip_at_the_image_border_keeps_full_coverage() {
        // Core Graphics edge-extends a clip mask's samples: magnified 2.25× here, the solid mask
        // stays fully opaque along its own border instead of fading toward zero outside the image.
        let mut canvas = Canvas::new_rgba(16, 16);
        canvas.clip_to_image(&Gray8Image::uniform(4, 4, 255), Rect::new(0.0, 0.0, 9.0, 9.0));
        canvas.set_fill_color(PaletteColor::new(1.0, 1.0, 1.0));
        canvas.fill_rect(Rect::new(0.0, 0.0, 16.0, 16.0));
        let image = canvas.snapshot();
        for y in 0..9 {
            for x in 0..9 {
                assert_eq!(image.get(x, y)[3], 255, "border coverage at ({x}, {y})");
            }
        }
        assert_eq!(image.get(9, 4)[3], 0, "the clip still ends at the mask's rect");
    }

    #[test]
    fn a_mask_fully_outside_the_canvas_draws_nothing() {
        // Regression: a mask placed entirely off-canvas produced device bounds with min > max and
        // panicked with `attempt to subtract with overflow`. Swift clips to nothing there, so both
        // the uniform 1×1 path and the sampled path must collapse to an empty range.
        let mut canvas = Canvas::new_rgba(20, 20);
        canvas.set_fill_color(PaletteColor::new(1.0, 0.0, 0.0));
        canvas.clip_to_image(&Gray8Image::uniform(1, 1, 255), Rect::new(40.0, 40.0, 1.0, 1.0));
        canvas.clip_to_image(&Gray8Image::uniform(4, 4, 255), Rect::new(40.0, -10.0, 4.0, 4.0));
        canvas.fill_rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        let image = canvas.snapshot();
        for y in 0..20 {
            for x in 0..20 {
                assert_eq!(image.get(x, y), [0, 0, 0, 0], "pixel ({x}, {y})");
            }
        }
    }
    #[test]
    fn an_aligned_image_draw_lands_one_source_pixel_per_destination_pixel() {
        // The aligned path replaces resampling with the source texel, so it has to be the source pixel
        // blended over the backdrop — the same thing bilinear sampling with three zero weights gives.
        let source = Rgba8Image::from_data(
            3,
            2,
            vec![
                10, 20, 30, 255, 200, 10, 0, 128, 0, 0, 0, 0, 255, 255, 255, 255, 7, 8, 9, 64, 100,
                100, 100, 200,
            ],
        );
        let mut canvas = Canvas::new_rgba(3, 2);
        canvas.set_fill_gray(0.4);
        canvas.fill_rect(Rect::new(0.0, 0.0, 3.0, 2.0));
        canvas.draw_image(&source, Rect::new(0.0, 0.0, 3.0, 2.0));
        let drawn = canvas.snapshot();
        let mut backdrop = Canvas::new_rgba(3, 2);
        backdrop.set_fill_gray(0.4);
        backdrop.fill_rect(Rect::new(0.0, 0.0, 3.0, 2.0));
        let under = backdrop.snapshot();
        for y in 0..2usize {
            for x in 0..3usize {
                assert_eq!(
                    drawn.get(x, y),
                    source_over(under.get(x, y), source.get(x, y)),
                    "pixel ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_solid_source_over_table_matches_the_blend() {
        // The table is `source_over` with the source held constant, so every entry has to be the byte
        // `source_over` would have produced for the same backdrop and that source.
        for alpha in 0..=255u8 {
            for red in [0u8, 1, 127, 255] {
                for green in [0u8, 255] {
                    for blue in [0u8, 128] {
                        let source = [red, green, blue, alpha];
                        let table = SolidSourceOver::new(source);
                        for byte in 0..=255u8 {
                            for backdrop in [
                                [byte, byte, byte, byte],
                                [byte, 0, 255, alpha],
                                [255, byte, 128, 0],
                                [0, 0, 0, byte],
                            ] {
                                assert_eq!(
                                    table.blend(backdrop),
                                    source_over(backdrop, source),
                                    "backdrop {backdrop:?} over source {source:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_direct_rect_fill_matches_the_coverage_path() {
        // `fill_rect` skips the coverage buffer for a rectangle under a translation-and-scale
        // transform, which is the editor's common case. The two paths must land the same bytes.
        let rects = [
            Rect::new(0.0, 0.0, 8.0, 8.0),
            Rect::new(1.3, 2.7, 5.5, 3.25),
            Rect::new(-2.5, -1.5, 6.0, 6.0),
            Rect::new(2.0, 3.0, 0.25, 9.0),
            Rect::new(-7.5, 4.25, 20.0, 8.5),
            Rect::new(3.5, 3.5, 0.0, 4.0),
            Rect::new(0.25, 0.75, 6.0, 5.0),
        ];
        let transforms = [
            AffineTransform::IDENTITY,
            AffineTransform::IDENTITY.translated_by(0.5, -1.25),
            AffineTransform { a: 1.5, b: 0.0, c: 0.0, d: 1.5, tx: -3.25, ty: 7.5 },
            AffineTransform { a: 2.0, b: 0.0, c: 0.0, d: 0.5, tx: 1.75, ty: -0.75 },
            AffineTransform { a: -1.0, b: 0.0, c: 0.0, d: 1.0, tx: 9.0, ty: 0.0 },
        ];
        // A fractional clip is stored as a mask and an integral one stays a rect test, so the two
        // take different branches: the mask one cannot use the flat fill.
        let clips = [Rect::new(0.5, -0.5, 8.0, 7.0), Rect::new(1.0, 0.0, 6.0, 6.0)];
        for rect in rects {
            for ctm in transforms {
                for clip in clips {
                for (gray, alpha) in [(0.3, 1.0), (0.35, 0.5), (0.105, 1.0)] {
                    let mut direct = Canvas::new_rgba(9, 7);
                    direct.concatenate(ctm);
                    direct.clip_rect(clip);
                    direct.set_fill_gray(gray);
                    direct.set_alpha(alpha);
                    direct.fill_rect(rect);

                    let mut general = Canvas::new_rgba(9, 7);
                    general.concatenate(ctm);
                    general.clip_rect(clip);
                    general.set_fill_gray(gray);
                    general.set_alpha(alpha);
                    // A scratch canvas: asking the direct path whether it applies paints into it.
                    let mut probe = Canvas::new_rgba(9, 7);
                    probe.concatenate(ctm);
                    assert!(
                        probe.fill_rect_direct(rect),
                        "the direct path applies to {rect:?} under {ctm:?}"
                    );
                    let coverage = general.rect_coverage(rect);
                    general.paint_coverage(&coverage);

                    let (a, b) = (direct.snapshot(), general.snapshot());
                    assert_eq!(
                        (a.width(), a.height()),
                        (b.width(), b.height()),
                        "target sizes"
                    );
                    for y in 0..a.height() {
                        for x in 0..a.width() {
                            assert_eq!(
                                a.get(x, y),
                                b.get(x, y),
                                "pixel ({x}, {y}) of {rect:?} under {ctm:?} clip {clip:?} gray {gray} alpha {alpha}"
                            );
                        }
                    }
                }
                }
            }
        }
    }
}

#[cfg(test)]
mod temp_bench_round {
    #[test]
    fn temp_bench_to_byte() {
        use crate::canvas::to_byte;
        let n = 20_000_000u64;
        let t = std::time::Instant::now();
        let mut acc = 0u64;
        for i in 0..n {
            acc += to_byte((i % 1000) as f64 / 1000.0) as u64;
        }
        println!("to_byte  {:?} {acc}", t.elapsed());
        let t = std::time::Instant::now();
        let mut acc2 = 0u64;
        for i in 0..n {
            let v = ((i % 1000) as f64 / 1000.0).clamp(0.0, 1.0) * 255.0;
            acc2 += (v + 0.5) as u8 as u64;
        }
        println!("plus-half {:?} {acc2}", t.elapsed());
    }
}

#[cfg(test)]
mod temp_bench_draw {
    use super::*;

    #[test]
    fn temp_bench_draw_image() {
        // TEMP-PERF: the layer draw at the window's fit zoom.
        let (dw, dh) = (1070usize, 803usize);
        let source = Rgba8Image::from_data(
            800,
            600,
            (0..800 * 600 * 4).map(|i| (i % 251) as u8).collect(),
        );
        let zoom = 1.3375f64;
        let rect = Rect::new(72.0, 86.75, 800.0 * zoom, 600.0 * zoom);
        let mut canvas = Canvas::new_rgba(dw, dh);
        let t = std::time::Instant::now();
        canvas.draw_image(&source, rect);
        println!("TEMP-BENCH draw_image {dw}x{dh} zoom {zoom} -> {:?}", t.elapsed());
        let mut tight = Canvas::new_rgba(dw, dh);
        let t = std::time::Instant::now();
        tight.draw_image(&source, Rect::new(0.0, 0.0, 800.0, 600.0));
        println!("TEMP-BENCH draw_image 1:1 800x600 -> {:?}", t.elapsed());
        let visible = Rect::new(72.0, 86.75, 1070.0, 803.0);
        let mut canvas = Canvas::new_rgba(dw, dh);
        let t = std::time::Instant::now();
        let coverage = canvas.rect_coverage(visible);
        println!("TEMP-BENCH rect_coverage {} -> {:?}", coverage.values.len(), t.elapsed());
        let t = std::time::Instant::now();
        canvas.clip_rect(visible);
        println!("TEMP-BENCH clip_rect -> {:?}", t.elapsed());
    }
}
