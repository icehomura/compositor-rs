//! The off-screen surface a spatial adjustment renders into (`AdjustmentSurface`).
//!
//! Ported from `Rendering/AdjustmentSurface.swift`: the bitmap `CGContext` is [`Canvas`], the image it
//! produced a [`compositor_rs_core::Rgba8Image`]. `LayerRenderer.deviceScale(of:)` is the canvas's own
//! [`Canvas::device_scale`], and `DocumentLimits.maxSurfaceExtent` is
//! [`compositor_rs_core::limits::MAX_SURFACE_EXTENT`].

use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::image_ops::{FilterJob, FilterKind, FilterSettings};
use compositor_rs_core::layer_adjustment::{AdjustmentKind, LayerAdjustment};
use compositor_rs_core::limits::MAX_SURFACE_EXTENT;
use compositor_rs_core::{CoreError, Result, Rgba8Image};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_rs_pixels::{adjustments, filters};

/// The pixel work of `LayerAdjustment.apply(_:region:scale:)`: the model's record lives in
/// `compositor-rs-core`, which cannot depend on the kernels, so the dispatch lives here where both are
/// visible. `region` is the part of the document `image` covers (the whole image at one unit per pixel
/// when omitted), so Grain's and Add Noise's patterns stay fixed in the document however the canvas
/// splits its drawing.
pub trait AdjustmentApply {
    fn apply(&self, image: &Rgba8Image, region: Option<Rect>, scale: f64) -> Result<Rgba8Image>;
}

impl AdjustmentApply for LayerAdjustment {
    fn apply(&self, image: &Rgba8Image, region: Option<Rect>, scale: f64) -> Result<Rgba8Image> {
        match self.kind {
            AdjustmentKind::Hsv => Ok(adjustments::apply_hue_saturation(
                image,
                &self.resolved_hsv(),
                None,
                AffineTransform::IDENTITY,
            )),
            AdjustmentKind::Levels => Ok(adjustments::apply_levels(
                image,
                &self.levels,
                None,
                AffineTransform::IDENTITY,
            )),
            AdjustmentKind::Curves => {
                adjustments::apply_curves(image, &self.curves).map_err(CoreError::from)
            }
            AdjustmentKind::BlackWhite => {
                adjustments::apply_black_white(image, &self.black_white()).map_err(CoreError::from)
            }
            AdjustmentKind::ColorBalance => adjustments::apply_color_balance(image, &self.color_balance())
                .map_err(CoreError::from),
            AdjustmentKind::Exposure => {
                adjustments::apply_exposure(image, &self.exposure()).map_err(CoreError::from)
            }
            AdjustmentKind::GradientMap => {
                adjustments::apply_gradient_map(image, &self.gradient_map()).map_err(CoreError::from)
            }
            AdjustmentKind::Grain => {
                let region = region.unwrap_or_else(|| image_rect(image));
                adjustments::apply_grain(
                    image,
                    &self.grain(),
                    region.origin,
                    region.width() / image.width().max(1) as f64,
                    None,
                )
                .map_err(CoreError::from)
            }
            AdjustmentKind::Invert => Ok(adjustments::invert(image, None, AffineTransform::IDENTITY)),
            AdjustmentKind::GaussianBlur | AdjustmentKind::MotionBlur | AdjustmentKind::AddNoise => {
                let kind = match self.kind {
                    AdjustmentKind::GaussianBlur => FilterKind::GaussianBlur,
                    AdjustmentKind::MotionBlur => FilterKind::MotionBlur,
                    _ => FilterKind::AddNoise,
                };
                let mut settings = FilterSettings::default();
                settings.radius = self.gaussian_radius();
                settings.angle = self.resolved_motion_angle();
                settings.distance = self.resolved_motion_distance();
                settings.amount = self.resolved_noise_amount();
                settings.gaussian = self.resolved_noise_gaussian();
                settings.monochromatic = self.resolved_noise_monochromatic();
                let mut job = FilterJob::new(
                    kind,
                    image.clone(),
                    settings,
                    scale,
                    None,
                    AffineTransform::IDENTITY,
                );
                job.seed = self.resolved_noise_seed();
                // The region's origin in the image's own pixels.
                job.noise_origin = region
                    .map(|region| {
                        Point::new(
                            region.min_x() * image.width() as f64 / region.width().max(1.0),
                            region.min_y() * image.height() as f64 / region.height().max(1.0),
                        )
                    })
                    .unwrap_or(Point::ZERO);
                filters::PixelFilter::run(&job, None)
            }
        }
    }
}

/// The whole image as a document rect, for the adjustments that need a region.
fn image_rect(image: &Rgba8Image) -> Rect {
    Rect::from_origin_size(
        Point::ZERO,
        Size::new(image.width() as f64, image.height() as f64),
    )
}

/// A one-shot off-screen render around a clipped area.
pub enum AdjustmentSurface {}

impl AdjustmentSurface {
    /// `AdjustmentSurface.draw(in:padding:body:)`.
    ///
    /// Spatial adjustments need pixels outside AppKit's dirty rectangle. Render that halo
    /// offscreen; the destination context still clips the final draw to the requested region.
    pub fn draw(canvas: &mut Canvas, padding: f64, body: impl FnOnce(&mut Canvas)) {
        let output = canvas.clip_bounds().integral();
        let bounds = output.inset_by(-padding, -padding).integral();
        if !(bounds.width() > 0.0 && bounds.height() > 0.0) {
            return;
        }
        // One surface pixel per screen pixel: a surface in points would be half the display's
        // resolution on Retina, stretched back up and soft. Too big for that, it falls back to points.
        let device = canvas.device_scale();
        let area = bounds.width() * bounds.height();
        let scale = if area * device * device <= MAX_SURFACE_EXTENT {
            device
        } else {
            1.0
        };
        if area > MAX_SURFACE_EXTENT {
            return;
        }
        let width = (bounds.width() * scale).round();
        let height = (bounds.height() * scale).round();
        if !(width >= 1.0 && height >= 1.0) {
            return;
        }
        let mut surface = Canvas::new_rgba(width as usize, height as usize);
        surface.scale(scale, scale);
        surface.translate(-bounds.min_x(), -bounds.min_y());
        body(&mut surface);
        let image = surface.snapshot();
        canvas.save();
        canvas.set_interpolation_quality(if scale == device {
            InterpolationQuality::None
        } else {
            InterpolationQuality::High
        });
        canvas.draw_image(&image, bounds);
        canvas.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::limits::MAX_SURFACE_PIXELS;
    use compositor_rs_core::{PaletteColor, Point, Size};

    fn fill_color(canvas: &mut Canvas, rect: Rect, color: PaletteColor) {
        canvas.set_fill_color(color);
        canvas.fill_rect(rect);
    }

    /// A `width × height` image of one premultiplied pixel, as `layer_effects_surface`'s tests make.
    fn base_image(width: usize, height: usize, pixel: [u8; 4]) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, pixel);
            }
        }
        image
    }

    /// The body's drawing lands at the document coordinates it used, even though the surface is
    /// offset and scaled, and nothing outside the destination's clip changes.
    #[test]
    fn the_body_draws_through_the_offset_surface_at_document_coordinates() {
        let mut canvas = Canvas::new_rgba(64, 64);
        fill_color(&mut canvas, Rect::new(0.0, 0.0, 64.0, 64.0), PaletteColor::WHITE);
        canvas.save();
        canvas.clip_rect(Rect::new(16.0, 16.0, 8.0, 8.0));
        AdjustmentSurface::draw(&mut canvas, 0.0, |surface| {
            fill_color(surface, Rect::new(18.0, 18.0, 4.0, 4.0), PaletteColor::BLACK);
        });
        canvas.restore();

        // Drawn where the body put it.
        assert_eq!(canvas.rgba().get(20, 20), [0, 0, 0, 255]);
        // Clipped away outside the destination clip.
        assert_eq!(canvas.rgba().get(12, 12), [255, 255, 255, 255]);
        assert_eq!(canvas.rgba().get(30, 30), [255, 255, 255, 255]);
    }

    /// The padding extends the render past the clip box (the halo `padding` exists for).
    #[test]
    fn padding_lends_the_surface_pixels_beyond_the_clip_box() {
        let mut canvas = Canvas::new_rgba(64, 64);
        fill_color(&mut canvas, Rect::new(0.0, 0.0, 64.0, 64.0), PaletteColor::WHITE);
        canvas.save();
        canvas.clip_rect(Rect::new(32.0, 32.0, 16.0, 16.0));
        AdjustmentSurface::draw(&mut canvas, 8.0, |surface| {
            // A blur-like pass: exactly `padding` outside the clip box, drawn back inside it.
            fill_color(surface, Rect::new(24.0, 24.0, 32.0, 32.0), PaletteColor::BLACK);
        });
        canvas.restore();
        assert_eq!(canvas.rgba().get(34, 34), [0, 0, 0, 255]);
        assert_eq!(canvas.rgba().get(31, 31), [255, 255, 255, 255]);
    }

    /// An empty clip box renders nothing at all.
    #[test]
    fn an_empty_clip_box_draws_nothing() {
        let mut canvas = Canvas::new_rgba(16, 16);
        canvas.clip_to_zero();
        AdjustmentSurface::draw(&mut canvas, 0.0, |surface| {
            fill_color(surface, Rect::new(0.0, 0.0, 16.0, 16.0), PaletteColor::BLACK);
        });
        for pixel in canvas.rgba().pixels() {
            assert_eq!(pixel, [0, 0, 0, 0]);
        }
    }

    /// A surface larger than the pixel ceiling is skipped entirely, exactly like the Swift guard.
    #[test]
    fn an_oversized_surface_is_refused() {
        let mut canvas = Canvas::new_rgba(16, 16);
        canvas.clip_rect(Rect::new(0.0, 0.0, 16.0, 16.0));
        // A padding that would grow the halo past `maxSurfaceExtent` draws nothing at all.
        AdjustmentSurface::draw(&mut canvas, 1_000_000_000.0, |surface| {
            fill_color(surface, Rect::new(0.0, 0.0, 16.0, 16.0), PaletteColor::BLACK);
        });
        for pixel in canvas.rgba().pixels() {
            assert_eq!(pixel, [0, 0, 0, 0]);
        }
    }

    /// `LayerAdjustment.apply` dispatches to the kernels: Invert flips the color and keeps the alpha.
    #[test]
    fn apply_dispatches_the_invert_kind() {
        let mut image = Rgba8Image::new(2, 1);
        image.set(0, 0, [100, 150, 200, 255]);
        image.set(1, 0, [0, 0, 0, 128]);
        let adjustment = LayerAdjustment::new(AdjustmentKind::Invert);
        let result = adjustment.apply(&image, None, 1.0).expect("invert always renders");
        assert_eq!(result.get(0, 0), [155, 105, 55, 255]);
        assert_eq!(result.get(1, 0)[3], 128, "inverting never changes the alpha");
        assert_ne!(result.get(1, 0), image.get(1, 0));
    }

    /// The blur kinds run their filter job and keep the grid they were handed.
    #[test]
    fn apply_dispatches_the_blur_kinds() {
        let image = base_image(8, 8, [200, 100, 50, 255]);
        let adjustment = LayerAdjustment::new(AdjustmentKind::GaussianBlur);
        let result = adjustment.apply(&image, None, 1.0).expect("a blur renders");
        assert_eq!((result.width(), result.height()), (8, 8));
        assert_eq!(result.get(4, 4)[3], 255);
    }
}
