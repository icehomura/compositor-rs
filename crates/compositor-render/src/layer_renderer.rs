//! Draws into a top-left coordinate system, shared by the canvas and export.
//!
//! Port of `Rendering/LayerRenderer.swift`. Core Graphics' `CGContext` is
//! [`compositor_pixels::canvas::Canvas`] and `CGImage` is [`PixelImage`] (RGBA for a layer, gray
//! for a mask). The Swift code's Core Graphics rects are mirrored in y (`bounds.maxY - height`)
//! because Core Graphics' native space is y-up; this port's canvas is y-down and top-left
//! anchored, so those expressions become their y-down equivalents.

use compositor_core::blend::LayerBlendMode;
use compositor_core::geom::{AffineTransform, Point, Rect};
use compositor_core::imported_image::PixelImage;
use compositor_core::layer_transform::{LayerInterpolationQuality, LayerSampling, LayerTransform};
use compositor_core::path::{FillRule, Path};
use compositor_core::raster::{BrushPatch, RasterSnapshot};
use compositor_core::{Gray8Image, Rgba8Image};
use compositor_pixels::canvas::{Canvas, InterpolationQuality};

use crate::downsample_cache::DownsampleCache;

/// `LayerRenderer`'s namespace; every member is an associated function, as upstream.
pub enum LayerRenderer {}

/// An image ready to draw a layer `width` context units wide: sharp halvings for large reductions
/// (never for Nearest), and how far past the layer's bounds the copy reaches — halvings round up,
/// so it covers exactly `2^level` source pixels per pixel, a little beyond the right and bottom.
pub struct Reduced {
    pub image: PixelImage,
    pub level: usize,
    pub width_scale: f64,
    pub height_scale: f64,
}

impl LayerRenderer {
    /// Draws `image` in `transform`, centered on `center`, at `opacity` and `blend_mode`, clipped
    /// by `mask` when there is one.
    pub fn draw(
        image: &PixelImage,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        mask: Option<&PixelImage>,
        canvas: &mut Canvas,
    ) {
        let width = transform.size.width * scale;
        let height = transform.size.height * scale;
        // Large reductions draw from sharp halvings; Core Graphics then only does the last 2× or less.
        let device = Self::device_scale(canvas);
        let source = Self::reduced(image, width, device, transform.sampling);
        let clip = mask.map(|mask| Self::reduced(mask, width, device, transform.sampling));
        canvas.save();
        canvas.set_alpha(opacity);
        canvas.set_blend_mode(blend_mode);
        canvas.set_interpolation_quality(Self::interpolation(
            transform.sampling,
            width * device / image.width().max(1) as f64 * (1usize << source.level) as f64,
            transform.radians() == 0.0,
        ));
        canvas.set_should_antialias(transform.sampling != LayerSampling::Nearest);
        // One placement transform rather than three calls: `CGContext` applies each call to the
        // current CTM in turn, and a single transform makes the placement the same whatever order
        // the canvas composes them in.
        canvas.concatenate(placement(transform, center));
        let bounds = Rect::new(-width / 2.0, -height / 2.0, width, height);
        if let Some(clip) = &clip {
            if let Some(gray) = clip.image.as_gray() {
                canvas.clip_to_image(gray, Self::coverage(clip, bounds));
            }
        }
        Self::draw_pixels(&source.image, Self::coverage(&source, bounds), canvas);
        canvas.restore();
    }

    /// Core Graphics's filter for the last resample, `final_factor` device pixels per (reduced)
    /// image pixel. An `upright` layer drawn pixel for pixel copies its pixels straight across.
    ///
    /// Shrinking uses Low: Medium and High prefilter by the image's own size and position, so a
    /// piece of an image would come out different from the whole (painted layers draw in pieces),
    /// and the sharp halvings have already done the heavy reduction. Enlarging keeps the layer's
    /// own setting.
    pub fn interpolation(
        sampling: LayerSampling,
        final_factor: f64,
        upright: bool,
    ) -> InterpolationQuality {
        if sampling == LayerSampling::Nearest || (upright && (final_factor - 1.0).abs() < 0.001) {
            return InterpolationQuality::None;
        }
        if final_factor <= 1.0 {
            InterpolationQuality::Low
        } else {
            canvas_quality(sampling.quality())
        }
    }

    /// `image` reduced for a draw `width` context units wide, and its reduction level.
    pub fn reduced(image: &PixelImage, width: f64, device: f64, sampling: LayerSampling) -> Reduced {
        if sampling == LayerSampling::Nearest {
            return Reduced {
                image: image.clone(),
                level: 0,
                width_scale: 1.0,
                height_scale: 1.0,
            };
        }
        let level = DownsampleCache::level_for(width * device / image.width().max(1) as f64);
        let (result, applied) = DownsampleCache::shared().image_at_level(image, level);
        Reduced {
            width_scale: ((result.width() << applied) as f64) / image.width().max(1) as f64,
            height_scale: ((result.height() << applied) as f64) / image.height().max(1) as f64,
            image: result,
            level: applied,
        }
    }

    /// Where a reduced image goes when its layer fills `bounds`: the reduced copy's top-left corner
    /// sits on the layer's, so what the halvings rounded up reaches beyond the right and bottom.
    pub fn coverage(reduced: &Reduced, bounds: Rect) -> Rect {
        let width = bounds.width() * reduced.width_scale;
        let height = bounds.height() * reduced.height_scale;
        Rect::new(bounds.min_x(), bounds.min_y(), width, height)
    }

    /// Device pixels per unit along the canvas's x axis.
    pub fn device_scale(canvas: &Canvas) -> f64 {
        canvas.device_scale()
    }

    /// Resample coverage as coverage, bypassing grayscale color conversion.
    pub fn draw_coverage(image: &Gray8Image, transform: &LayerTransform, canvas: &mut Canvas) {
        canvas.save();
        canvas.set_interpolation_quality(canvas_quality(transform.sampling.quality()));
        canvas.set_should_antialias(transform.sampling != LayerSampling::Nearest);
        canvas.concatenate(placement(transform, transform.center()));
        let bounds = Rect::new(
            -transform.size.width / 2.0,
            -transform.size.height / 2.0,
            transform.size.width,
            transform.size.height,
        );
        canvas.clip_to_image(image, bounds);
        canvas.set_fill_gray(1.0);
        canvas.fill_rect(bounds);
        canvas.restore();
    }

    /// Draws a `PixelImage` through `draw_image`/`draw_gray`: a gray image drawn into a color target
    /// becomes opaque gray, as `CGContext` draws a DeviceGray image into an sRGB context.
    pub(crate) fn draw_pixels(image: &PixelImage, rect: Rect, canvas: &mut Canvas) {
        match image {
            PixelImage::Rgba(rgba) => {
                if canvas.is_mask() {
                    let coverage = Gray8Image::from_data(
                        rgba.width(),
                        rgba.height(),
                        rgba.pixels().map(|pixel| pixel[3]).collect(),
                    );
                    // The image's own samples land in a mask, so the paint is white.
                    canvas.set_fill_gray(1.0);
                    canvas.draw_coverage(&coverage, rect);
                } else {
                    canvas.draw_image(rgba, rect);
                }
            }
            PixelImage::Gray(gray) => {
                if canvas.is_mask() {
                    canvas.set_fill_gray(1.0);
                    canvas.draw_coverage(gray, rect);
                } else {
                    canvas.draw_image(&opaque_gray(gray), rect);
                }
            }
        }
    }
}

/// `translate(center) · rotate · scale(flip)` as one transform: the layer's local space to the
/// canvas's, exactly as the Swift builds it with three `CGContext` calls.
fn placement(transform: &LayerTransform, center: Point) -> AffineTransform {
    AffineTransform::translation(center.x, center.y)
        .rotated_by(transform.radians())
        .scaled_by(
            if transform.flip_x { -1.0 } else { 1.0 },
            if transform.flip_y { -1.0 } else { 1.0 },
        )
}

/// A `CGInterpolationQuality`-equivalent from the document's sampling setting.
pub(crate) fn canvas_quality(quality: LayerInterpolationQuality) -> InterpolationQuality {
    match quality {
        LayerInterpolationQuality::None => InterpolationQuality::None,
        LayerInterpolationQuality::Low => InterpolationQuality::Low,
        LayerInterpolationQuality::High => InterpolationQuality::High,
    }
}

/// A gray image as the opaque gray pixels `CGContext.draw` makes of a DeviceGray image in a color
/// context: premultiplied `[v, v, v, 255]` (`compositor_core::buffer::opaque_gray`).
pub use compositor_core::buffer::opaque_gray;

impl LayerRenderer {
    /// Preview replacement tiles without building a full-size raster. Disjoint clips ensure opacity
    /// and blend modes are applied exactly once per pixel.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_brush_preview(
        image: Option<&PixelImage>,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        mask: Option<&PixelImage>,
        patches: &[BrushPatch],
        pixel_width: usize,
        pixel_height: usize,
        painting_mask: bool,
        source_rect: Option<Rect>,
        raster: Option<&RasterSnapshot>,
        raster_base: Option<&PixelImage>,
        canvas: &mut Canvas,
    ) {
        let width = transform.size.width * scale;
        let height = transform.size.height * scale;
        let bounds = Rect::new(-width / 2.0, -height / 2.0, width, height);
        let mapped = |source: Rect| {
            Rect::new(
                bounds.min_x() + source.min_x() / pixel_width as f64 * width,
                bounds.min_y() + source.min_y() / pixel_height as f64 * height,
                source.width() / pixel_width as f64 * width,
                source.height() / pixel_height as f64 * height,
            )
        };
        let rect = |patch: &BrushPatch| mapped(patch.rect);
        let original_bounds = source_rect.map(mapped).unwrap_or(bounds);
        let draw_source = |canvas: &mut Canvas| {
            let Some(raster) = raster else {
                if let Some(image) = image {
                    LayerRenderer::draw_pixels(image, original_bounds, canvas);
                }
                return;
            };
            let source_mapped = |r: Rect| {
                Rect::new(
                    original_bounds.min_x() + r.min_x() / raster.width as f64 * original_bounds.width(),
                    original_bounds.min_y() + r.min_y() / raster.height as f64 * original_bounds.height(),
                    r.width() / raster.width as f64 * original_bounds.width(),
                    r.height() / raster.height as f64 * original_bounds.height(),
                )
            };
            if let Some(base) = raster_base.or(raster.base.as_ref()) {
                canvas.save();
                let mut outline = Path::rect(original_bounds);
                for patch in &raster.patches {
                    outline.add_rect(source_mapped(patch.rect));
                }
                canvas.clip_path(&outline, FillRule::EvenOdd);
                LayerRenderer::draw_pixels(base, source_mapped(raster.base_rect), canvas);
                canvas.restore();
            }
            let visible = canvas.clip_bounds();
            for patch in &raster.patches {
                let target = source_mapped(patch.rect);
                if !target.intersects(visible) {
                    continue;
                }
                canvas.save();
                canvas.clip_rect(target);
                LayerRenderer::draw_pixels(&patch.image, target, canvas);
                canvas.restore();
            }
        };
        canvas.save();
        canvas.set_alpha(opacity);
        canvas.set_blend_mode(blend_mode);
        canvas.set_interpolation_quality(canvas_quality(transform.sampling.quality()));
        canvas.concatenate(placement(transform, center));
        // Tile boundaries must not acquire overlapping antialias coverage.
        canvas.set_should_antialias(false);
        if image.is_some() {
            canvas.save();
            let mut outline = Path::rect(bounds);
            for patch in patches {
                outline.add_rect(rect(patch));
            }
            canvas.clip_path(&outline, FillRule::EvenOdd);
            if let Some(mask) = mask.and_then(|mask| mask.as_gray()) {
                canvas.clip_to_image(mask, original_bounds);
            }
            draw_source(canvas);
            canvas.restore();
        }
        for patch in patches {
            let tile_bounds = rect(patch);
            canvas.save();
            canvas.clip_rect(tile_bounds);
            if painting_mask {
                if image.is_some() {
                    if let Some(gray) = patch.image.as_gray() {
                        canvas.clip_to_image(gray, tile_bounds);
                    }
                    draw_source(canvas);
                }
            } else {
                if let Some(mask) = mask.and_then(|mask| mask.as_gray()) {
                    // Paint outside the old raster is revealed; retain the old mask inside it.
                    canvas.save();
                    canvas.clip_to_image(mask, original_bounds);
                    LayerRenderer::draw_pixels(&patch.image, tile_bounds, canvas);
                    canvas.restore();
                    let mut outline = Path::rect(tile_bounds);
                    let overlap = tile_bounds.intersection(original_bounds);
                    if !overlap.is_null() && !overlap.is_empty() {
                        outline.add_rect(overlap);
                    }
                    canvas.clip_path(&outline, FillRule::EvenOdd);
                }
                LayerRenderer::draw_pixels(&patch.image, tile_bounds, canvas);
            }
            canvas.restore();
        }
        canvas.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::geom::Size;
    use compositor_core::layer_transform::LayerSampling;
    use std::sync::Arc;

    fn transform(origin: Point, size: Size, sampling: LayerSampling) -> LayerTransform {
        LayerTransform {
            origin,
            size,
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling,
        }
    }

    fn image(pixels: [[u8; 4]; 4], width: usize, height: usize) -> PixelImage {
        let mut result = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                result.set(x, y, pixels[y * width + x]);
            }
        }
        PixelImage::Rgba(Arc::new(result))
    }

    /// A layer drawn pixel for pixel copies its pixels straight across, so Nearest and an upright
    /// 1:1 draw ask for no interpolation at all.
    #[test]
    fn interpolation_matches_the_swift_rules() {
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::Nearest, 0.25, false),
            InterpolationQuality::None
        );
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::High, 1.0, true),
            InterpolationQuality::None
        );
        // Shrinking uses Low, whatever the layer's own setting: `finalFactor <= 1 ? .low : quality`.
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::High, 1.0, false),
            InterpolationQuality::Low
        );
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::High, 0.5, false),
            InterpolationQuality::Low
        );
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::Smooth, 2.0, false),
            InterpolationQuality::Low
        );
        assert_eq!(
            LayerRenderer::interpolation(LayerSampling::High, 2.0, false),
            InterpolationQuality::High
        );
    }

    /// Halvings round up, so a reduced copy reaches past the layer's right and bottom edges.
    #[test]
    fn coverage_anchors_reduced_images_at_the_top_left() {
        let reduced = Reduced {
            image: image([[0, 0, 0, 0]; 4], 2, 2),
            level: 1,
            width_scale: 1.5,
            height_scale: 1.25,
        };
        let bounds = Rect::new(-5.0, -5.0, 10.0, 10.0);
        assert_eq!(LayerRenderer::coverage(&reduced, bounds), Rect::new(-5.0, -5.0, 15.0, 12.5));
    }

    /// `deviceScale(of:)` reads the context's scale, not its rotation.
    #[test]
    fn device_scale_reads_the_ctm() {
        let mut canvas = Canvas::new_rgba(8, 8);
        assert_eq!(LayerRenderer::device_scale(&canvas), 1.0);
        canvas.scale(2.0, 2.0);
        assert_eq!(LayerRenderer::device_scale(&canvas), 2.0);
    }

    /// A 2 × 2 image at 2 × 2 device pixels, at half opacity: every pixel squared with alpha 128.
    #[test]
    fn draw_applies_opacity_to_every_pixel() {
        let source = image(
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 0, 255],
            ],
            2,
            2,
        );
        let mut canvas = Canvas::new_rgba(4, 4);
        let transform = transform(
            Point::new(1.0, 1.0),
            Size::new(2.0, 2.0),
            LayerSampling::Nearest,
        );
        LayerRenderer::draw(
            &source,
            &transform,
            transform.center(),
            1.0,
            0.5,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        let result = canvas.into_rgba();
        for y in 0..4 {
            for x in 0..4 {
                let pixel = result.get(x, y);
                if (1..3).contains(&x) && (1..3).contains(&y) {
                    assert_eq!(pixel[3], 128, "pixel {x},{y}");
                } else {
                    assert_eq!(pixel, [0, 0, 0, 0], "pixel {x},{y}");
                }
            }
        }
    }

    /// A horizontally flipped layer mirrors its columns.
    #[test]
    fn draw_flips_horizontally() {
        let source = image(
            [
                [255, 0, 0, 255],
                [0, 0, 255, 255],
                [0, 0, 0, 0],
                [0, 0, 0, 0],
            ],
            2,
            1,
        );
        let mut canvas = Canvas::new_rgba(2, 1);
        let mut transform = transform(
            Point::new(0.0, 0.0),
            Size::new(2.0, 1.0),
            LayerSampling::Nearest,
        );
        transform.flip_x = true;
        LayerRenderer::draw(
            &source,
            &transform,
            transform.center(),
            1.0,
            1.0,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [0, 0, 255, 255]);
        assert_eq!(result.get(1, 0), [255, 0, 0, 255]);
    }

    /// A mask clips the layer to its coverage: half the mask hides half the layer.
    #[test]
    fn draw_clips_through_a_mask() {
        let source = image(
            [
                [255, 0, 0, 255],
                [255, 0, 0, 255],
                [0, 0, 0, 0],
                [0, 0, 0, 0],
            ],
            2,
            1,
        );
        let mut mask = Gray8Image::new(2, 1);
        mask.set(0, 0, 255);
        mask.set(1, 0, 0);
        let mut canvas = Canvas::new_rgba(2, 1);
        let transform = transform(
            Point::new(0.0, 0.0),
            Size::new(2.0, 1.0),
            LayerSampling::Nearest,
        );
        LayerRenderer::draw(
            &source,
            &transform,
            transform.center(),
            1.0,
            1.0,
            LayerBlendMode::Normal,
            Some(&PixelImage::Gray(Arc::new(mask))),
            &mut canvas,
        );
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 0, 0]);
    }

    /// `drawCoverage` resamples coverage as coverage: a 45°-rotated mask's coverage stays gray.
    #[test]
    fn draw_coverage_fills_through_the_mask() {
        let mut mask = Gray8Image::new(2, 1);
        mask.set(0, 0, 200);
        mask.set(1, 0, 100);
        let mut canvas = Canvas::new_gray(2, 1);
        let transform = transform(
            Point::new(0.0, 0.0),
            Size::new(2.0, 1.0),
            LayerSampling::Nearest,
        );
        LayerRenderer::draw_coverage(&mask, &transform, &mut canvas);
        let result = canvas.into_gray();
        assert_eq!(result.get(0, 0), 200);
        assert_eq!(result.get(1, 0), 100);
    }

    /// An opaque RGBA image whose pixel colors come from `gray(x, y)`.
    fn gray_image(width: usize, height: usize, gray: impl Fn(usize, usize) -> u8) -> PixelImage {
        let mut result = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let value = gray(x, y);
                result.set(x, y, [value, value, value, 255]);
            }
        }
        PixelImage::Rgba(Arc::new(result))
    }

    /// `image` drawn through the layer renderer, shrunk to `width` × `height`, at High quality.
    fn drawn(image: &PixelImage, width: usize, height: usize) -> Rgba8Image {
        let mut canvas = Canvas::new_rgba(width, height);
        let transform = transform(
            Point::ZERO,
            Size::new(image.width() as f64, image.height() as f64),
            LayerSampling::High,
        );
        let scale = width as f64 / image.width() as f64;
        let center = Point::new(transform.center().x * scale, transform.center().y * scale);
        LayerRenderer::draw(
            image,
            &transform,
            center,
            scale,
            1.0,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        canvas.into_rgba()
    }

    /// Shrinking a hard edge by eight times keeps it sharp: the edge smears across at most three
    /// pixels, and each side of it stays where it belongs.
    #[test]
    fn a_hard_edge_stays_sharp_shrunk_eight_times() {
        let source = gray_image(4096, 64, |x, _| if x < 2048 { 0 } else { 255 });
        let pixels = drawn(&source, 512, 8);
        let row: Vec<i32> = (0..512).map(|x| pixels.get(x, 4)[0] as i32).collect();
        let soft = row.iter().filter(|value| (40..215).contains(*value)).count();
        assert!(soft <= 3, "the edge smears across {soft} pixels");
        assert!(row[250] < 10 && row[262] > 245, "{} and {}", row[250], row[262]);
    }

    /// Fine stripes average to flat gray without shimmering: the halvings average them out rather
    /// than aliasing them.
    #[test]
    fn fine_stripes_average_to_flat_gray_without_shimmer() {
        let source = gray_image(2048, 256, |x, _| if x % 2 == 0 { 0 } else { 255 });
        let pixels = drawn(&source, 256, 32);
        // The outermost pixels fade into the transparent edge; judge the inside.
        let values: Vec<f64> = (4..252).map(|x| pixels.get(x, 16)[0] as f64).collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let spread = (values
            .iter()
            .map(|value| (value - mean) * (value - mean))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt();
        assert!((mean - 127.5).abs() < 8.0, "mean {mean}");
        assert!(spread < 6.0, "stripes shimmer after shrinking: spread {spread}");
    }
}
