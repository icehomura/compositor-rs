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

use compositor_core::blend::LayerBlendMode;
use compositor_core::color::to_byte;
use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::path::{FillRule, Path, PathElement, Subpath, flatten};
use compositor_core::{Gray8Image, PaletteColor, Rgba8Image};

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
    /// Device-space coverage at `rect`'s scale, `None` while the clip is the plain rectangle.
    mask: Option<Gray8Image>,
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
        self.state.ctm = AffineTransform::translation(dx, dy).concatenating(self.state.ctm);
    }

    pub fn scale(&mut self, sx: f64, sy: f64) {
        self.state.ctm = AffineTransform::scale(sx, sy).concatenating(self.state.ctm);
    }

    pub fn rotate(&mut self, radians: f64) {
        self.state.ctm = AffineTransform::rotation(radians).concatenating(self.state.ctm);
    }

    pub fn concatenate(&mut self, transform: AffineTransform) {
        self.state.ctm = transform.concatenating(self.state.ctm);
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
        self.intersect_clip(Coverage::empty(self.pixel_size()));
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

    pub fn fill_rect(&mut self, rect: Rect) {
        let coverage = self.rect_coverage(rect);
        self.paint_coverage(&coverage);
    }

    pub fn fill_rects(&mut self, rects: &[Rect]) {
        for rect in rects {
            self.fill_rect(*rect);
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
        let subpath = Subpath { points: corners.to_vec(), closed: true };
        rasterize_polygons(
            &[subpath],
            FillRule::Winding,
            self.state.antialias,
            self.pixel_size(),
        )
    }

    fn image_coverage(&self, image: &Gray8Image, rect: Rect) -> Coverage {
        if rect.is_empty() || image.is_empty() {
            return Coverage::empty(self.pixel_size());
        }
        if image.width() == 1 && image.height() == 1 {
            // A uniform mask keeps the same shape at full coverage.
            return Coverage::uniform(rect, image.get(0, 0) as f64 / 255.0, self.pixel_size());
        }
        let inverse = self.state.ctm.inverted();
        let mut coverage = Coverage::empty(self.pixel_size());
        let bounds = self.device_bounds(rect);
        for y in bounds.0..bounds.1 {
            for x in bounds.2..bounds.3 {
                let device = Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let user = inverse.applying(device);
                let u = (user.x - rect.min_x()) / rect.width();
                let v = (user.y - rect.min_y()) / rect.height();
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                let value = sample_u8(
                    image,
                    u * image.width() as f64 - 0.5,
                    v * image.height() as f64 - 0.5,
                    self.state.interpolation,
                );
                coverage.set(x, y, value as f64 / 255.0);
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
        let min_x = corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min).floor().max(0.0);
        let min_y = corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min).floor().max(0.0);
        let max_x = corners
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(self.width() as f64);
        let max_y = corners
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(self.height() as f64);
        (
            min_y.max(0.0) as usize,
            max_y.max(0.0) as usize,
            min_x.max(0.0) as usize,
            max_x.max(0.0) as usize,
        )
    }

    fn draw_rgba(&mut self, image: &Rgba8Image, rect: Rect, gray_target: bool) {
        debug_assert!(!gray_target);
        if rect.is_empty() || image.is_empty() {
            return;
        }
        let inverse = self.state.ctm.inverted();
        let (min_y, max_y, min_x, max_x) = self.device_bounds(rect);
        let target_size = self.pixel_size();
        let clip = self.state.clip.clone();
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
        let has_mask = current.mask.is_some() || !coverage.is_uniform();
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
            mask: if has_mask { Some(mask) } else { None },
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
                let mut value = coverage.at(x, y);
                if value <= 0.0 {
                    continue;
                }
                value *= clip.coverage_at(x, y) * alpha;
                if value <= 0.0 {
                    continue;
                }
                let pixel = match &paint {
                    Paint::Gray(gray) => {
                        let byte = to_byte(*gray);
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
                };
                self.blend_into_target(x as usize, y as usize, pixel);
            }
        }
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
                // Coverage onto a mask is a plain source-over of the alpha channel.
                let existing = image.get(x, y) as f64 / 255.0;
                let source_alpha = source[3] as f64 / 255.0;
                let value = source_alpha + existing * (1.0 - source_alpha);
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
    fn empty(size: Size) -> Self {
        Coverage {
            rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            values: Vec::new(),
        }
    }

    fn uniform(rect: Rect, value: f64, size: Size) -> Self {
        let clipped = rect.integral().intersection(Rect::from_origin_size(Point::ZERO, size));
        if clipped.is_empty() {
            return Coverage::empty(size);
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
        if x < self.rect.min_x() as i64 || y < self.rect.min_y() as i64 {
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
                mask.get(local_x as usize, local_y as usize) as f64 / 255.0
            }
        }
    }
}

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
            for (dx, wx) in [(0.0, 1.0 - fx), (1.0, fx)] {
                for (dy, wy) in [(0.0, 1.0 - fy), (1.0, fy)] {
                    let sx = x0 + dx;
                    let sy = y0 + dy;
                    if sx < 0.0 || sy < 0.0 || sx as usize >= image.width() || sy as usize >= image.height() {
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
    let sa = source[3] as f64 / 255.0;
    let da = backdrop[3] as f64 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let mut result = [0u8; 4];
    for channel in 0..3 {
        let value = source[channel] as f64 / 255.0 + (backdrop[channel] as f64 / 255.0) * (1.0 - sa);
        result[channel] = to_byte(value);
    }
    result[3] = to_byte(out_a);
    result
}

fn scale_premultiplied(pixel: [u8; 4], coverage: f64) -> [u8; 4] {
    [
        (pixel[0] as f64 * coverage).round().clamp(0.0, 255.0) as u8,
        (pixel[1] as f64 * coverage).round().clamp(0.0, 255.0) as u8,
        (pixel[2] as f64 * coverage).round().clamp(0.0, 255.0) as u8,
        (pixel[3] as f64 * coverage).round().clamp(0.0, 255.0) as u8,
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

/// Scanline rasterization of device-space polygons.
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
        return Coverage::empty(size);
    }
    let top = min_y.floor().max(0.0);
    let bottom = max_y.ceil().min(size.height);
    let left = min_x.floor().max(0.0);
    let right = max_x.ceil().min(size.width);
    if bottom <= top || right <= left {
        return Coverage::empty(size);
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
                add_span(&mut row_coverage, left, start, end);
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

/// Path elements are exported through `compositor_core::path`; this import keeps the type visible for
/// callers that build paths inline.
pub use compositor_core::path::Path as CanvasPath;

// [`PathElement`] is part of the frozen `path` API.
#[allow(unused_imports)]
use PathElement as _PathElement;

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::path::Path;

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
    fn antialiased_path_edge_has_partial_coverage() {
        let mut canvas = Canvas::new_gray(16, 16);
        canvas.set_fill_gray(1.0);
        canvas.fill_path(&Path::rect(Rect::new(2.5, 2.0, 4.0, 4.0)), FillRule::Winding);
        let gray = canvas.snapshot_gray();
        assert!(gray.get(2, 3) > 0 && gray.get(2, 3) < 255, "half-covered column is partial");
        assert_eq!(gray.get(3, 3), 255);
    }
}
