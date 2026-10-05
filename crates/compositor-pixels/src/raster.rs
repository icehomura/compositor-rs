//! The `BrushRaster` helpers `Document/BrushStroke.swift` draws through, on top of
//! [`crate::canvas::Canvas`]: `context`, `copy`, `inBands`, `draw`, `fill`, `falloff` and
//! `pixelToDocument`.
//!
//! This is the crate's `CGContext`-shaped drawing layer. Beyond the canvas primitives it carries the
//! three Core Graphics behaviors a bitmap target has to spell out: a *replacement* draw (`.copy`
//! blend or, for coverage, blacken-then-paint-through-the-mask), a cleared rectangle, and a fill
//! that takes pixels out of the destination's alpha (`.destinationOut`).

use crate::canvas::{Canvas, InterpolationQuality};
use compositor_core::blend::LayerBlendMode;
use compositor_core::geom::{AffineTransform, Point, Rect};
use compositor_core::imported_image::PixelImage;
use compositor_core::layer_transform::LayerTransform;
use compositor_core::{Gray8Image, PaletteColor, Rgba8Image};
use rayon::prelude::*;

/// The raster layouts a paint target can have — `BrushRaster.context(width:height:mask:)`'s choice.
pub struct Raster;

impl Raster {
    /// The flag the drawing calls take: a mask target is a gray canvas, anything else sRGB RGBA.
    pub fn context_is_mask(_width: usize, _height: usize, mask: bool) -> bool {
        mask
    }

    /// `BrushRaster.context(width:height:mask:)`: a blank target. RGBA targets start fully
    /// transparent; gray (mask) targets start black, which the callers overwrite explicitly when
    /// they need white.
    pub fn context(width: usize, height: usize, mask: bool) -> Canvas {
        if mask {
            Canvas::new_gray(width, height)
        } else {
            Canvas::new_rgba(width, height)
        }
    }

    /// `BrushRaster.copy(_:)`: a color context holding `image`'s pixels. An image already in the
    /// canonical layout — every [`Rgba8Image`] is — is copied byte for byte, several times quicker
    /// than drawing it.
    pub fn copy(image: &Rgba8Image) -> Canvas {
        Canvas::from_rgba(image.clone())
    }

    /// `BrushRaster.inBands(count:_:)`: runs `body` over `count` pixels in a few bands at once,
    /// each a `(start, length)` of whole pixels. Fewer than 250,000 pixels run as one band.
    ///
    /// The band arithmetic is the Swift one; the bands run in order here because `body` is `FnMut`.
    /// [`Raster::in_bands_parallel`] is the concurrent form, where `DispatchQueue.concurrentPerform`
    /// was.
    pub fn in_bands<R>(count: usize, mut body: impl FnMut(usize, usize) -> R) {
        for (start, length) in band_ranges(count) {
            body(start, length);
        }
    }

    /// [`Raster::in_bands`] on `rayon`, for the callers whose bodies touch disjoint pixels. Results
    /// come back in band order, so a caller assembling them is independent of the thread count.
    pub fn in_bands_parallel<R: Send>(
        count: usize,
        body: impl Fn(usize, usize) -> R + Send + Sync,
    ) -> Vec<R> {
        band_ranges(count)
            .into_par_iter()
            .map(|(start, length)| body(start, length))
            .collect()
    }

    /// Soft-brush falloff across the region between the hardness radius and the rim: a normalized
    /// Gaussian that fades across the whole radius and reaches zero at the rim.
    pub fn falloff(coverage: f64) -> f64 {
        let k: f64 = 2.5;
        (0.0f64).max(((-k * coverage * coverage).exp() - (-k).exp()) / (1.0 - (-k).exp()))
    }

    /// `BrushRaster.pixelToDocument(_:width:height:)`: the affine map placing a `width`×`height`
    /// image with `transform` into document coordinates.
    pub fn pixel_to_document(transform: &LayerTransform, width: f64, height: f64) -> AffineTransform {
        AffineTransform::translation(transform.center().x, transform.center().y)
            .rotated_by(transform.radians())
            .scaled_by(
                transform.size.width / width * if transform.flip_x { -1.0 } else { 1.0 },
                transform.size.height / height * if transform.flip_y { -1.0 } else { 1.0 },
            )
            .translated_by(-width / 2.0, -height / 2.0)
    }

    /// `BrushRaster.draw(_:in:mask:context:)`: draws `image` into `rect` (document space) of a mask
    /// or RGBA canvas, replacing what is there.
    ///
    /// Into a mask the image is its own coverage — the rect is blackened, clipped through the image
    /// and painted white, so a gray patch lands byte for byte; sampling is nearest. Into a color
    /// target the image is drawn with Core Graphics' `.copy`: the rect is cleared first, then the
    /// pixels are drawn, so transparent source pixels replace too.
    pub fn draw(image: &PixelImage, rect: Rect, mask: bool, canvas: &mut Canvas) {
        if rect.is_null() || rect.is_empty() || image.width() == 0 || image.height() == 0 {
            return;
        }
        canvas.save();
        canvas.set_interpolation_quality(InterpolationQuality::None);
        if mask {
            canvas.set_fill_gray(0.0);
            canvas.fill_rect(rect);
            match image {
                PixelImage::Gray(gray) => canvas.clip_to_image(gray, rect),
                PixelImage::Rgba(rgba) => canvas.clip_to_image(&alpha_coverage(rgba), rect),
            }
            canvas.set_fill_gray(1.0);
            canvas.fill_rect(rect);
        } else {
            canvas.set_blend_mode(LayerBlendMode::Normal);
            canvas.clear_rect(rect);
            match image {
                PixelImage::Rgba(rgba) => canvas.draw_image(rgba, rect),
                PixelImage::Gray(gray) => canvas.draw_image(&gray_as_rgba(gray), rect),
            }
        }
        canvas.restore();
    }

    /// [`Raster::draw`] for a color image, the form the frozen `Raster::draw` names.
    pub fn draw_rgba(image: &Rgba8Image, rect: Rect, mask: bool, canvas: &mut Canvas) {
        Self::draw(&PixelImage::Rgba(std::sync::Arc::new(image.clone())), rect, mask, canvas);
    }

    /// `BrushRaster.fill(_:coverage:in:alpha:context:)`: paints a solid color through grayscale
    /// coverage at a uniform alpha.
    pub fn fill_color(
        color: &PaletteColor,
        coverage: &Gray8Image,
        rect: Rect,
        alpha: f64,
        canvas: &mut Canvas,
    ) {
        canvas.save();
        canvas.set_interpolation_quality(InterpolationQuality::None);
        canvas.clip_to_image(coverage, rect);
        canvas.set_alpha(alpha);
        canvas.set_fill_color(*color);
        canvas.fill_rect(rect);
        canvas.restore();
    }

    /// [`Raster::fill_color`] for a mask's gray value 0…1.
    pub fn fill_gray(gray: f64, coverage: &Gray8Image, rect: Rect, alpha: f64, canvas: &mut Canvas) {
        canvas.save();
        canvas.set_interpolation_quality(InterpolationQuality::None);
        canvas.clip_to_image(coverage, rect);
        canvas.set_alpha(alpha);
        canvas.set_fill_gray(gray);
        canvas.fill_rect(rect);
        canvas.restore();
    }

    /// Fills `rect` with the paint already set, at the canvas's current alpha times
    /// `coverage × alpha` — the scalar form of [`Raster::fill_color`], for callers whose coverage is
    /// uniform.
    pub fn fill(rect: Rect, coverage: f64, alpha: f64, canvas: &mut Canvas) {
        canvas.save();
        canvas.set_alpha((canvas.fill_alpha() * coverage * alpha).clamp(0.0, 1.0));
        canvas.fill_rect(rect);
        canvas.restore();
    }
}

/// The band `(start, length)` pairs `inBands` runs: one band under 250,000 pixels, otherwise twice
/// the processors, `DispatchQueue.concurrentPerform`'s split.
fn band_ranges(count: usize) -> Vec<(usize, usize)> {
    if count == 0 {
        return Vec::new();
    }
    let bands = if count < 250_000 {
        1
    } else {
        rayon::current_num_threads().max(1) * 2
    };
    let size = count.div_ceil(bands);
    (0..bands)
        .filter_map(|band| {
            let start = band * size;
            (start < count).then_some((start, size.min(count - start)))
        })
        .collect()
}

/// `CGRect.applying(_:)`: the bounding box of `rect`'s four mapped corners.
pub fn rect_applying(rect: Rect, transform: AffineTransform) -> Rect {
    if rect.is_null() {
        return rect;
    }
    let corners = rect.corners().map(|corner| transform.applying(corner));
    let min_x = corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min);
    let min_y = corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min);
    let max_x = corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max);
    let max_y = corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// `CGSize.applying(_:)`: sizes take a transform's linear part only, no translation.
pub fn size_applying(size: Size, transform: AffineTransform) -> Size {
    Size::new(
        size.width * transform.a + size.height * transform.c,
        size.width * transform.b + size.height * transform.d,
    )
}

/// A color image's alpha as 8-bit coverage, for `clip(to:mask:)` with a color image.
fn alpha_coverage(image: &Rgba8Image) -> Gray8Image {
    Gray8Image::from_data(
        image.width(),
        image.height(),
        image.pixels().map(|pixel| pixel[3]).collect(),
    )
}

/// A gray image as opaque gray in a color target (`CGContext.draw` of a gray image).
fn gray_as_rgba(image: &Gray8Image) -> Rgba8Image {
    Rgba8Image::from_data(
        image.width(),
        image.height(),
        image
            .data()
            .iter()
            .flat_map(|&value| [value, value, value, 255])
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::geom::Size;

    #[test]
    fn falloff_is_one_at_the_center_and_zero_at_the_rim() {
        assert_eq!(Raster::falloff(0.0), 1.0);
        assert_eq!(Raster::falloff(1.0), 0.0);
        assert_eq!(Raster::falloff(2.0), 0.0, "nothing beyond the rim");
        // About half strength at half radius, the column `BrushTests.softBrushProducesPartialAlpha` reads.
        assert!((Raster::falloff(0.5) - 0.4938).abs() < 2e-4);
    }

    #[test]
    fn falloff_matches_the_normalized_gaussian_exactly() {
        let k = 2.5f64;
        for step in 0..=40 {
            let u = step as f64 / 40.0;
            let expected = (0.0f64).max(((-k * u * u).exp() - (-k).exp()) / (1.0 - (-k).exp()));
            assert!((Raster::falloff(u) - expected).abs() < 1e-12, "u = {u}");
        }
    }

    #[test]
    fn pixel_to_document_places_a_centered_image() {
        let transform = LayerTransform {
            origin: Point::new(10.0, 20.0),
            size: Size::new(200.0, 100.0),
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: Default::default(),
        };
        let map = Raster::pixel_to_document(&transform, 200.0, 100.0);
        assert_eq!(map, AffineTransform::IDENTITY.translated_by(10.0, 20.0));
        // Which is exactly what the unit-square map does.
        assert_eq!(map, Raster::pixel_to_document(&transform, 1.0, 1.0));
        // The image's corners land on the transform's.
        let top_left = map.applying(Point::new(0.0, 0.0));
        assert!((top_left.x - 10.0).abs() < 1e-9 && (top_left.y - 20.0).abs() < 1e-9);
        let bottom_right = map.applying(Point::new(200.0, 100.0));
        assert!((bottom_right.x - 210.0).abs() < 1e-9 && (bottom_right.y - 120.0).abs() < 1e-9);
    }

    #[test]
    fn pixel_to_document_scales_to_a_smaller_grid() {
        let transform = LayerTransform {
            origin: Point::new(0.0, 0.0),
            size: Size::new(80.0, 40.0),
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: Default::default(),
        };
        // A 40×20 image stretched over the 80×40 placement doubles.
        let map = Raster::pixel_to_document(&transform, 40.0, 20.0);
        assert!((map.a - 2.0).abs() < 1e-12 && (map.d - 2.0).abs() < 1e-12);
        let corner = map.applying(Point::new(40.0, 20.0));
        assert!((corner.x - 80.0).abs() < 1e-9 && (corner.y - 40.0).abs() < 1e-9);
    }

    #[test]
    fn draw_gray_replaces_a_mask_rect() {
        let patch = Gray8Image::from_data(2, 2, vec![10, 20, 30, 40]);
        let mut canvas = Canvas::new_gray(6, 6);
        Raster::draw(&PixelImage::Gray(std::sync::Arc::new(patch)), Rect::new(2.0, 2.0, 2.0, 2.0), true, &mut canvas);
        let gray = canvas.snapshot_gray();
        assert_eq!(gray.get(2, 2), 10);
        assert_eq!(gray.get(3, 2), 20);
        assert_eq!(gray.get(2, 3), 30);
        assert_eq!(gray.get(3, 3), 40);
        assert_eq!(gray.get(1, 2), 0);
        assert_eq!(gray.get(4, 4), 0);
    }

    #[test]
    fn draw_rgba_replaces_with_copy_semantics() {
        // The source has a transparent half; copy makes the destination transparent there too.
        let mut pixels = vec![255u8, 0, 0, 255, 0, 0, 0, 0];
        let image = Rgba8Image::from_data(2, 1, std::mem::take(&mut pixels));
        let mut canvas = Canvas::new_rgba(2, 1);
        canvas.set_fill_color(PaletteColor::WHITE);
        canvas.set_alpha(1.0);
        canvas.fill_rect(Rect::new(0.0, 0.0, 2.0, 1.0));
        Raster::draw(&PixelImage::Rgba(std::sync::Arc::new(image)), Rect::new(0.0, 0.0, 2.0, 1.0), false, &mut canvas);
        let rgba = canvas.into_rgba();
        assert_eq!(rgba.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(rgba.get(1, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn fill_color_uses_coverage_and_alpha() {
        let coverage = Gray8Image::uniform(4, 4, 128);
        let mut canvas = Canvas::new_rgba(4, 4);
        Raster::fill_color(&PaletteColor::new(1.0, 0.0, 0.0), &coverage, Rect::new(0.0, 0.0, 4.0, 4.0), 1.0, &mut canvas);
        let rgba = canvas.into_rgba();
        let pixel = rgba.get(1, 1);
        assert_eq!(pixel[3], 128, "coverage is the alpha");
        assert_eq!(pixel[0], 128, "premultiplied red");
    }

    #[test]
    fn in_bands_splits_the_swift_way() {
        let mut seen = Vec::new();
        Raster::in_bands(1000, |start, length| {
            seen.push((start, length));
            length
        });
        assert_eq!(seen, vec![(0, 1000)], "small counts run as one band");
        let ranges = band_ranges(1_000_000);
        assert_eq!(ranges[0].0, 0);
        let covered: usize = ranges.iter().map(|(_, length)| length).sum();
        assert_eq!(covered, 1_000_000, "every pixel is in exactly one band");
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "bands are contiguous");
        }
        let parallel = Raster::in_bands_parallel(1_000_000, |start, length| (start, length));
        assert_eq!(parallel, ranges);
    }

    #[test]
    fn rect_applying_takes_the_bounding_box() {
        let rotated = AffineTransform::rotation(std::f64::consts::FRAC_PI_4);
        let mapped = rect_applying(Rect::new(-1.0, -1.0, 2.0, 2.0), rotated);
        let side = 2.0 * 2.0f64.sqrt();
        assert!((mapped.width() - side).abs() < 1e-9);
        assert!((mapped.height() - side).abs() < 1e-9);
    }
}
