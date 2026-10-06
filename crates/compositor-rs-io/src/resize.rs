//! Image Size, Canvas Size and Crop. The sheets ask for a new canvas, and these resizers carry the
//! document across it: Image Size resamples every layer (and mask) through its own transform, Canvas
//! Size and Crop keep the pixels and move the placements, guides and masks by the same offset.
//!
//! The resampling goes through `compositor_rs_pixels::canvas::Canvas`, the `CGContext` replacement, which
//! samples with the layer's own `LayerSampling` quality, exactly as `LayerRenderer.draw` asked Core
//! Graphics to.

use compositor_rs_core::buffer::{Gray8Image, Rgba8Image};
use compositor_rs_core::canvas_size::{CanvasExtensionColor, CanvasSizeOptions};
use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::document::{ProjectError, ProjectLayerRecord};
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_mask::LayerMask;
use compositor_rs_core::layer_transform::{
    LayerInterpolationQuality, LayerSampling, LayerTransform, pixel_to_document,
};
use compositor_rs_core::limits::{MAX_SIDE, MAX_SURFACE_PIXELS, document_pixel_budget};
use compositor_rs_core::raster::THUMBNAIL_MAX_SIDE;
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::project_manifest::{ProjectManifest, image_filename};
use crate::project_store::ProjectSnapshot;

/// What the Image Size sheet asks the resizer for.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageSizeOptions {
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    pub sampling: LayerSampling,
}

impl ImageSizeOptions {
    /// The sheet's options; the sampling defaults to High quality, as the sheet's picker does.
    pub fn new(width: usize, height: usize, resolution: f64) -> Self {
        Self { width, height, resolution, sampling: LayerSampling::High }
    }
}

pub struct ImageResizer;

impl ImageResizer {
    /// Resamples every layer's pixels and bounds to a new document size.
    pub fn resize(snapshot: &ProjectSnapshot, options: &ImageSizeOptions) -> Result<ProjectSnapshot, ProjectError> {
        if !(1..=MAX_SIDE).contains(&options.width)
            || !(1..=MAX_SIDE).contains(&options.height)
            || !options.resolution.is_finite()
            || !(1.0..=9600.0).contains(&options.resolution)
        {
            return Err(ProjectError::TooLarge);
        }
        let old = &snapshot.manifest;
        let mut manifest = ProjectManifest {
            resolution: Some(options.resolution),
            guides: old.guides.clone(),
            ..ProjectManifest::new(old.document_id, options.width, options.height, old.active_layer_id, Vec::new())
        };
        if old.width == options.width && old.height == options.height {
            manifest.layers = old.layers.clone();
            return Ok(ProjectSnapshot { manifest, images: snapshot.images.clone(), masks: snapshot.masks.clone() });
        }
        if options.width * options.height > MAX_SURFACE_PIXELS {
            return Err(ProjectError::TooLarge);
        }
        let sx = options.width as f64 / old.width as f64;
        let sy = options.height as f64 / old.height as f64;
        manifest.guides = old.guides.as_ref().map(|guides| guides.iter().map(|guide| guide.scaled(sx, sy)).collect());
        let mut images = HashMap::new();
        let mut masks = HashMap::new();
        let mut used_pixels = 0usize;
        let mut used_mask_pixels = 0usize;
        for layer in &old.layers {
            // Rasterize each transformed layer independently. Nonuniform scaling of a
            // rotated rectangle can introduce shear, which width/height/angle cannot represent.
            let corners = [
                Point::new(0.0, 0.0),
                Point::new(1.0, 0.0),
                Point::new(1.0, 1.0),
                Point::new(0.0, 1.0),
            ]
            .map(|unit| layer.transform.point(unit))
            .map(|point| Point::new(point.x * sx, point.y * sy));
            let left = corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min).floor();
            let top = corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min).floor();
            let width = (corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max).ceil() - left) as i64;
            let height = (corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max).ceil() - top) as i64;
            let transform = LayerTransform {
                origin: Point::new(left, top),
                size: Size::new(width as f64, height as f64),
                rotation: 0.0,
                flip_x: false,
                flip_y: false,
                sampling: options.sampling,
            };
            if !transform.is_valid() {
                return Err(ProjectError::TooLarge);
            }
            let width = width as usize;
            let height = height as usize;
            if layer.image_file.is_some() {
                if !(1..=MAX_SIDE).contains(&width)
                    || !(1..=MAX_SIDE).contains(&height)
                    || width * height > document_pixel_budget().saturating_sub(used_pixels)
                {
                    return Err(ProjectError::TooLarge);
                }
                used_pixels += width * height;
                let source = snapshot.images.get(&layer.id).ok_or(ProjectError::MissingImage)?;
                let pixels = resample_rgba(source, &layer.transform, options.sampling, left, top, sx, sy, width, height);
                let thumbnail = rgba_thumbnail(&pixels);
                images.insert(
                    layer.id,
                    ImportedImage::new(
                        PixelImage::Rgba(Arc::new(pixels)),
                        PixelImage::Rgba(Arc::new(thumbnail)),
                        source.name.clone(),
                    ),
                );
            }
            if layer.mask_file.is_some() {
                let source = snapshot.masks.get(&layer.id).ok_or(ProjectError::MissingImage)?;
                // Uniform masks are resolution independent; avoid allocating a full canvas for reveal/hide-all.
                // A mask on its own placement keeps its pixels; the placement scales with the canvas.
                if (source.image.width() == 1 && source.image.height() == 1) || layer.mask_placement.is_some() {
                    masks.insert(layer.id, source.clone());
                } else {
                    if !(1..=MAX_SIDE).contains(&width)
                        || !(1..=MAX_SIDE).contains(&height)
                        || width * height > document_pixel_budget().saturating_sub(used_mask_pixels)
                    {
                        return Err(ProjectError::TooLarge);
                    }
                    used_mask_pixels += width * height;
                    let gray = resample_gray(source, &layer.transform, options.sampling, left, top, sx, sy, width, height);
                    let asset = LayerMask::asset(PixelImage::Gray(Arc::new(gray))).map_err(|_| ProjectError::Invalid)?;
                    masks.insert(layer.id, asset);
                }
            }
            let mut record = layer.clone();
            record.transform = transform;
            record.mask_placement = layer.mask_placement.map(|placement| {
                placement.placing(
                    placement
                        .unit_to_document()
                        .then(AffineTransform::scale(sx, sy)),
                )
            });
            manifest.layers.push(record);
        }
        Ok(ProjectSnapshot { manifest, images, masks })
    }
}

/// What the Canvas Size sheet (or Crop) asks the resizer for: the core's `CanvasSizeOptions`.
pub struct CanvasResizer;

impl CanvasResizer {
    /// Moves the document to a new canvas, keeping every pixel where the offset puts it.
    pub fn resize(snapshot: &ProjectSnapshot, options: &CanvasSizeOptions) -> Result<ProjectSnapshot, ProjectError> {
        if !(1..=MAX_SIDE).contains(&options.width) || !(1..=MAX_SIDE).contains(&options.height) || options.anchor > 8 {
            return Err(ProjectError::TooLarge);
        }
        let old = &snapshot.manifest;
        let offset = options.offset(old.width, old.height);
        if !offset.x.is_finite() || !offset.y.is_finite() || offset.x.abs() > 1_000_000.0 || offset.y.abs() > 1_000_000.0 {
            return Err(ProjectError::Invalid);
        }
        if options.width == old.width && options.height == old.height && offset == Point::ZERO {
            return Ok(snapshot.clone());
        }
        let mut manifest = ProjectManifest {
            resolution: old.resolution,
            guides: old
                .guides
                .as_ref()
                .map(|guides| guides.iter().map(|guide| guide.offset(offset.x, offset.y)).collect()),
            ..ProjectManifest::new(old.document_id, options.width, options.height, old.active_layer_id, Vec::new())
        };
        for layer in &old.layers {
            let mut record = layer.clone();
            record.transform.origin.x += offset.x;
            record.transform.origin.y += offset.y;
            if !record.transform.is_valid() {
                return Err(ProjectError::TooLarge);
            }
            if let Some(placement) = &mut record.mask_placement {
                placement.origin.x += offset.x;
                placement.origin.y += offset.y;
            }
            manifest.layers.push(record);
        }
        let mut images = snapshot.images.clone();
        // A colored extension is separate bottom-layer content. The old canvas
        // intersection remains transparent, including holes in the existing artwork.
        if let Some(color) = options.fill {
            if options.width > old.width || options.height > old.height {
                let used: usize = images.values().map(|asset| asset.image.width() * asset.image.height()).sum();
                if options.width * options.height > document_pixel_budget().saturating_sub(used)
                    || manifest.layers.len() >= 10_000
                {
                    return Err(ProjectError::TooLarge);
                }
                if [color.red, color.green, color.blue].iter().any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)) {
                    return Err(ProjectError::Invalid);
                }
                let id = Uuid::new_v4();
                let pixels = extension_pixels(options.width, options.height, color, offset, old.width, old.height);
                let thumbnail = rgba_thumbnail(&pixels);
                images.insert(
                    id,
                    ImportedImage::new(
                        PixelImage::Rgba(Arc::new(pixels)),
                        PixelImage::Rgba(Arc::new(thumbnail)),
                        "Canvas Extension",
                    ),
                );
                manifest.layers.insert(
                    0,
                    ProjectLayerRecord::new(
                        id,
                        "Canvas Extension".to_string(),
                        true,
                        LayerTransform {
                            origin: Point::ZERO,
                            size: Size::new(options.width as f64, options.height as f64),
                            rotation: 0.0,
                            flip_x: false,
                            flip_y: false,
                            sampling: LayerSampling::High,
                        },
                        Some(image_filename(id)),
                    ),
                );
            }
        }
        Ok(ProjectSnapshot { manifest, images, masks: snapshot.masks.clone() })
    }
}

/// The canvas extension asset: `color` everywhere outside the old canvas, transparent inside it.
fn extension_pixels(
    width: usize,
    height: usize,
    color: CanvasExtensionColor,
    offset: Point,
    old_width: usize,
    old_height: usize,
) -> Rgba8Image {
    let mut canvas = Canvas::new_rgba(width, height);
    canvas.set_fill_color(PaletteColor { red: color.red, green: color.green, blue: color.blue });
    canvas.fill_rect(Rect::new(0.0, 0.0, width as f64, height as f64));
    let mut pixels = canvas.into_rgba();
    // `context.clear(rect)`: the pixels the old canvas covered are made transparent again.
    clear_rect(&mut pixels, Rect::new(offset.x, offset.y, old_width as f64, old_height as f64));
    pixels
}

/// Clears `rect` out of `image`, with the analytic coverage `CGContext.clear(_:)` gives a fractional
/// rectangle.
fn clear_rect(image: &mut Rgba8Image, rect: Rect) {
    let min_x = rect.min_x().floor().max(0.0) as i64;
    let min_y = rect.min_y().floor().max(0.0) as i64;
    let max_x = rect.max_x().ceil().min(image.width() as f64) as i64;
    let max_y = rect.max_y().ceil().min(image.height() as f64) as i64;
    for y in min_y..max_y {
        let vertical = overlap(y as f64, y as f64 + 1.0, rect.min_y(), rect.max_y());
        if vertical <= 0.0 {
            continue;
        }
        for x in min_x..max_x {
            let horizontal = overlap(x as f64, x as f64 + 1.0, rect.min_x(), rect.max_x());
            let clear = horizontal * vertical;
            if clear <= 0.0 {
                continue;
            }
            let keep = 1.0 - clear;
            let pixel = image.get(x as usize, y as usize);
            image.set(
                x as usize,
                y as usize,
                [
                    (pixel[0] as f64 * keep).round() as u8,
                    (pixel[1] as f64 * keep).round() as u8,
                    (pixel[2] as f64 * keep).round() as u8,
                    (pixel[3] as f64 * keep).round() as u8,
                ],
            );
        }
    }
}

/// The part of `start..end` that lies inside `low..high`.
fn overlap(start: f64, end: f64, low: f64, high: f64) -> f64 {
    (end.min(high) - start.max(low)).max(0.0)
}

/// Draws a layer's pixels into a canvas of `width`×`height` at `left`,`top` after scaling by `sx`,`sy`.
#[allow(clippy::too_many_arguments)]
fn resample_rgba(
    source: &ImportedImage,
    transform: &LayerTransform,
    sampling: LayerSampling,
    left: f64,
    top: f64,
    sx: f64,
    sy: f64,
    width: usize,
    height: usize,
) -> Rgba8Image {
    let mut canvas = Canvas::new_rgba(width, height);
    let map = canvas_map(left, top, sx, sy);
    draw_pixels(&mut canvas, &source.image, transform, sampling, map);
    canvas.into_rgba()
}

#[allow(clippy::too_many_arguments)]
fn resample_gray(
    source: &ImportedImage,
    transform: &LayerTransform,
    sampling: LayerSampling,
    left: f64,
    top: f64,
    sx: f64,
    sy: f64,
    width: usize,
    height: usize,
) -> Gray8Image {
    let mut canvas = Canvas::new_gray(width, height);
    let map = canvas_map(left, top, sx, sy);
    draw_pixels(&mut canvas, &source.image, transform, sampling, map);
    canvas.into_gray()
}

/// The map from old document pixels to a new canvas of `width`×`height` placed at `left`,`top`.
fn canvas_map(left: f64, top: f64, sx: f64, sy: f64) -> AffineTransform {
    AffineTransform::new(sx, 0.0, 0.0, sy, -left, -top)
}

/// Draws `image` where `transform` places it, then through `canvas_map` into the target.
fn draw_pixels(
    canvas: &mut Canvas,
    image: &PixelImage,
    transform: &LayerTransform,
    sampling: LayerSampling,
    canvas_map: AffineTransform,
) {
    let width = image.width();
    let height = image.height();
    let rect = Rect::new(0.0, 0.0, width as f64, height as f64);
    let map = pixel_to_document(transform, width, height).then(canvas_map);
    canvas.save();
    canvas.set_interpolation_quality(interpolation(sampling));
    canvas.concatenate(map);
    match image {
        PixelImage::Rgba(rgba) => {
            if canvas.is_mask() {
                // Into a mask, an RGBA image's alpha is its coverage and the samples land, so the
                // paint is white (`CGContext.draw` of an image with alpha).
                let coverage = Gray8Image::from_data(
                    rgba.width(),
                    rgba.height(),
                    rgba.pixels().map(|pixel| pixel[3]).collect(),
                );
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
                canvas.draw_image(&compositor_rs_core::buffer::opaque_gray(gray), rect);
            }
        }
    }
    canvas.restore();
}

fn interpolation(sampling: LayerSampling) -> InterpolationQuality {
    match sampling.quality() {
        LayerInterpolationQuality::None => InterpolationQuality::None,
        LayerInterpolationQuality::Low => InterpolationQuality::Low,
        LayerInterpolationQuality::High => InterpolationQuality::High,
    }
}

/// The thumbnail size Core Graphics was asked for: no larger than 96 pixels on its longest side.
fn thumbnail_size(width: usize, height: usize) -> (usize, usize) {
    let factor = 1.0f64.min(THUMBNAIL_MAX_SIDE / width.max(height).max(1) as f64);
    (
        ((width as f64 * factor) as usize).max(1),
        ((height as f64 * factor) as usize).max(1),
    )
}

fn rgba_thumbnail(image: &Rgba8Image) -> Rgba8Image {
    let (width, height) = thumbnail_size(image.width(), image.height());
    if (width, height) == (image.width(), image.height()) {
        return image.clone();
    }
    let mut canvas = Canvas::new_rgba(width, height);
    canvas.set_interpolation_quality(InterpolationQuality::High);
    canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
    canvas.into_rgba()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(origin: Point, size: Size) -> LayerTransform {
        LayerTransform { origin, size, rotation: 0.0, flip_x: false, flip_y: false, sampling: LayerSampling::Nearest }
    }

    fn record(id: Uuid, transform: LayerTransform) -> ProjectLayerRecord {
        ProjectLayerRecord::new(id, "Layer".to_string(), true, transform, Some(image_filename(id)))
    }

    fn document(width: usize, height: usize, record: ProjectLayerRecord, asset: Rgba8Image) -> ProjectSnapshot {
        let mut manifest = ProjectManifest::new(Uuid::new_v4(), width, height, Some(record.id), vec![record.clone()]);
        manifest.resolution = Some(72.0);
        let mut images = HashMap::new();
        images.insert(
            record.id,
            ImportedImage::new(PixelImage::Rgba(Arc::new(asset)), PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))), "Layer"),
        );
        ProjectSnapshot { manifest, images, masks: HashMap::new() }
    }

    fn red(width: usize, height: usize) -> Rgba8Image {
        Rgba8Image::opaque(width, height, [255, 0, 0, 255])
    }

    fn rgba_arc(asset: &ImportedImage) -> Arc<Rgba8Image> {
        match &asset.image {
            PixelImage::Rgba(shared) => Arc::clone(shared),
            PixelImage::Gray(_) => panic!("the test's layer asset is RGBA"),
        }
    }

    /// A document whose single layer has no pixels, like a freshly created one.
    fn blank_document(width: usize, height: usize) -> ProjectSnapshot {
        let id = Uuid::new_v4();
        let record = ProjectLayerRecord::new(id, "Layer".to_string(), true, transform(Point::ZERO, Size::new(width as f64, height as f64)), None);
        let mut manifest = ProjectManifest::new(Uuid::new_v4(), width, height, Some(id), vec![record]);
        manifest.resolution = Some(72.0);
        ProjectSnapshot { manifest, images: HashMap::new(), masks: HashMap::new() }
    }

    /// Ported from CompositorTests.CanvasSizeTests.everyAnchorPreservesSourceAndTransformForExpansionAndShrink.
    #[test]
    fn every_anchor_preserves_source_and_transform_for_expansion_and_shrink() {
        let id = Uuid::new_v4();
        let base = LayerTransform {
            origin: Point::new(-16.0, 4.0),
            size: Size::new(64.0, 32.0),
            rotation: 37.0,
            flip_x: true,
            flip_y: false,
            sampling: LayerSampling::Nearest,
        };
        let snapshot = document(64, 32, record(id, base), red(64, 32));
        for delta in [5i64, -5] {
            for anchor in 0..9usize {
                let options = CanvasSizeOptions {
                    anchor,
                    ..CanvasSizeOptions::new((64 + delta) as usize, (32 + delta) as usize)
                };
                let result = CanvasResizer::resize(&snapshot, &options).unwrap();
                let output = &result.manifest.layers[0];
                let expected = [0.0, if delta == 5 { 2.0 } else { -3.0 }, delta as f64];
                assert_eq!(output.transform.origin.x, base.origin.x + expected[anchor % 3], "anchor {anchor}");
                assert_eq!(output.transform.origin.y, base.origin.y + expected[anchor / 3], "anchor {anchor}");
                assert_eq!(output.transform.size, base.size);
                assert!(output.transform.rotation == 37.0 && output.transform.flip_x);
                assert_eq!(output.id, id);
                assert!(Arc::ptr_eq(&rgba_arc(&result.images[&id]), &rgba_arc(&snapshot.images[&id])));
            }
        }
    }

    /// Ported from CompositorTests.CanvasSizeTests.coloredExtensionPreservesOldTransparencyAndRoundTripsWithUndo.
    #[test]
    fn colored_extension_preserves_old_transparency() {
        let id = Uuid::new_v4();
        let snapshot = document(4, 4, record(id, transform(Point::ZERO, Size::new(4.0, 4.0))), red(4, 4));
        let red_extension = CanvasExtensionColor::new(1.0, 0.0, 0.0);
        let mut options = CanvasSizeOptions::new(8, 2);
        options.fill = Some(red_extension);
        let output = CanvasResizer::resize(&snapshot, &options).unwrap();
        assert_eq!(output.manifest.layers.len(), 2);
        assert_eq!(output.manifest.active_layer_id, snapshot.manifest.active_layer_id);
        let extension = output.manifest.layers[0].id;
        let pixels = output.images[&extension].image.as_rgba().unwrap();
        // The left band and the right band are the extension; the old canvas' intersection is clear.
        assert_eq!(pixels.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(pixels.get(3, 0)[3], 0);
        assert_eq!(pixels.get(7, 1), [255, 0, 0, 255]);
        assert_eq!(output.manifest.layers[1].transform.origin, Point::new(2.0, -1.0));
    }

    /// Ported from CompositorTests.CanvasSizeTests.transparentResizeIsAllocationFreeAndShrinkDoesNotAddFill.
    #[test]
    fn transparent_resize_is_allocation_free_and_shrink_does_not_add_fill() {
        let snapshot = blank_document(4, 4);
        let large = CanvasResizer::resize(&snapshot, &CanvasSizeOptions::new(30_000, 30_000)).unwrap();
        assert!(large.images.is_empty());

        let white = CanvasExtensionColor::new(1.0, 1.0, 1.0);
        let mut shrink = CanvasSizeOptions::new(2, 2);
        shrink.fill = Some(white);
        let small = CanvasResizer::resize(&snapshot, &shrink).unwrap();
        assert!(small.images.is_empty() && small.manifest.layers.len() == 1);

        let mut huge = CanvasSizeOptions::new(30_000, 30_000);
        huge.fill = Some(white);
        assert!(CanvasResizer::resize(&snapshot, &huge).is_err());
    }

    /// Ported from CompositorTests.ImageSizeTests.rotatedHiddenLayerScalesInDocumentAxesAndInvalidSizeIsRejected.
    #[test]
    fn rotated_hidden_layer_scales_in_document_axes_and_invalid_size_is_rejected() {
        let id = Uuid::new_v4();
        let mut record = record(id, transform(Point::new(-16.0, 4.0), Size::new(64.0, 32.0)));
        record.transform.rotation = 90.0;
        record.is_visible = false;
        let snapshot = document(64, 32, record, red(64, 32));

        let options = ImageSizeOptions {
            width: 128,
            height: 96,
            resolution: 72.0,
            sampling: LayerSampling::Nearest,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        let layer = &output.manifest.layers[0];
        assert!(!layer.is_visible);
        assert_eq!(layer.transform.rotation, 0.0);
        // A 90-degree 64×32 layer becomes 32×64, then scales 2× horizontally and 3× vertically.
        assert!((layer.transform.size.width - 64.0).abs() <= 1.0);
        assert!((layer.transform.size.height - 192.0).abs() <= 1.0);
        assert!(layer.transform.origin.y < 0.0);

        let invalid = ImageSizeOptions {
            width: 30_000,
            height: 30_000,
            resolution: 72.0,
            sampling: LayerSampling::High,
        };
        assert!(ImageResizer::resize(&snapshot, &invalid).is_err());
    }

    /// Ported from CompositorTests.ImageSizeTests.resolutionOnlyRetainsPixelsAndSurvivesSaveAndExport.
    #[test]
    fn resolution_only_retains_pixels() {
        let id = Uuid::new_v4();
        let snapshot = document(32, 16, record(id, transform(Point::ZERO, Size::new(32.0, 16.0))), red(32, 16));
        let options = ImageSizeOptions {
            width: 32,
            height: 16,
            resolution: 300.0,
            sampling: LayerSampling::High,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        assert_eq!(output.manifest.resolution, Some(300.0));
        assert_eq!(output.manifest.layers[0].transform, snapshot.manifest.layers[0].transform);
        assert!(Arc::ptr_eq(&rgba_arc(&output.images[&id]), &rgba_arc(&snapshot.images[&id])));
    }

    #[test]
    fn image_size_resamples_the_layer_into_its_scaled_bounds() {
        let id = Uuid::new_v4();
        let snapshot = document(2, 2, record(id, transform(Point::ZERO, Size::new(2.0, 2.0))), red(2, 2));
        let options = ImageSizeOptions {
            width: 4,
            height: 4,
            resolution: 72.0,
            sampling: LayerSampling::Nearest,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        let layer = &output.manifest.layers[0];
        assert_eq!(layer.transform.origin, Point::ZERO);
        assert_eq!(layer.transform.size, Size::new(4.0, 4.0));
        let pixels = output.images[&id].image.as_rgba().unwrap();
        assert_eq!((pixels.width(), pixels.height()), (4, 4));
        assert_eq!(pixels.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(pixels.get(3, 3), [255, 0, 0, 255]);
    }

    #[test]
    fn image_size_preserves_masks_effects_group_and_text_metadata() {
        use compositor_rs_core::blend::LayerBlendMode;
        use compositor_rs_core::layer_effects::LayerEffects;
        use compositor_rs_core::layer_text::LayerTextStyle;

        let id = Uuid::new_v4();
        let group = Uuid::new_v4();
        let mut snapshot = document(2, 2, record(id, transform(Point::ZERO, Size::new(2.0, 2.0))), red(2, 2));
        let layer = &mut snapshot.manifest.layers[0];
        layer.text = Some(LayerTextStyle::default());
        layer.effects = Some(LayerEffects::default());
        layer.parent_id = Some(group);
        layer.is_group = Some(false);
        layer.opacity = Some(0.5);
        layer.blend_mode = Some(LayerBlendMode::Multiply);
        layer.mask_file = Some(crate::project_manifest::mask_filename(id));
        snapshot.masks.insert(
            id,
            ImportedImage::new(
                PixelImage::Gray(Arc::new(Gray8Image::uniform(2, 2, 255))),
                PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, 255))),
                "Layer Mask",
            ),
        );

        let options = ImageSizeOptions {
            width: 4,
            height: 4,
            resolution: 72.0,
            sampling: LayerSampling::Nearest,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        let resized = &output.manifest.layers[0];
        assert_eq!(resized.text, snapshot.manifest.layers[0].text);
        assert_eq!(resized.effects, snapshot.manifest.layers[0].effects);
        assert_eq!(resized.parent_id, Some(group));
        assert_eq!(resized.is_group, Some(false));
        assert_eq!(resized.opacity, Some(0.5));
        assert_eq!(resized.blend_mode, Some(LayerBlendMode::Multiply));
        assert_eq!(resized.mask_file, snapshot.manifest.layers[0].mask_file);
        assert!(output.masks.contains_key(&id));
    }

    #[test]
    fn image_size_scales_guides() {
        use compositor_rs_core::guides::{CanvasGuide, CanvasGuideAxis};

        let id = Uuid::new_v4();
        let mut snapshot = document(2, 2, record(id, transform(Point::ZERO, Size::new(2.0, 2.0))), red(2, 2));
        snapshot.manifest.guides = Some(vec![
            CanvasGuide::new(Uuid::new_v4(), CanvasGuideAxis::Horizontal, 1.0),
            CanvasGuide::new(Uuid::new_v4(), CanvasGuideAxis::Vertical, 1.0),
        ]);
        let options = ImageSizeOptions {
            width: 4,
            height: 8,
            resolution: 72.0,
            sampling: LayerSampling::High,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        let guides = output.manifest.guides.as_ref().unwrap();
        // A horizontal guide sits at a Y, a vertical one at an X.
        assert_eq!(guides[0].position, 4.0);
        assert_eq!(guides[1].position, 2.0);
    }

    #[test]
    fn image_size_resamples_a_layer_mask() {
        let id = Uuid::new_v4();
        let mut snapshot = document(2, 2, record(id, transform(Point::ZERO, Size::new(2.0, 2.0))), red(2, 2));
        snapshot.manifest.layers[0].mask_file = Some(crate::project_manifest::mask_filename(id));
        let mut mask = Gray8Image::new(2, 2);
        mask.set(0, 0, 255);
        snapshot.masks.insert(
            id,
            ImportedImage::new(
                PixelImage::Gray(Arc::new(mask)),
                PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, 255))),
                "Layer Mask",
            ),
        );
        let options = ImageSizeOptions {
            width: 4,
            height: 4,
            resolution: 72.0,
            sampling: LayerSampling::Nearest,
        };
        let output = ImageResizer::resize(&snapshot, &options).unwrap();
        let resampled = output.masks[&id].image.as_gray().unwrap();
        assert_eq!((resampled.width(), resampled.height()), (4, 4));
        assert_eq!(resampled.get(0, 0), 255);
        assert_eq!(resampled.get(3, 3), 0);
        assert_eq!(output.masks[&id].name, "Layer Mask");
    }

    #[test]
    fn canvas_size_offsets_guides_and_mask_placement() {
        use compositor_rs_core::guides::{CanvasGuide, CanvasGuideAxis};

        let id = Uuid::new_v4();
        let mut record = record(id, transform(Point::new(1.0, 1.0), Size::new(2.0, 2.0)));
        record.mask_file = Some(crate::project_manifest::mask_filename(id));
        record.mask_placement = Some(transform(Point::new(1.0, 1.0), Size::new(2.0, 2.0)));
        let mut manifest = ProjectManifest::new(Uuid::new_v4(), 4, 4, Some(id), vec![record]);
        manifest.resolution = Some(72.0);
        manifest.guides = Some(vec![CanvasGuide::new(Uuid::new_v4(), CanvasGuideAxis::Horizontal, 3.0)]);
        let mut images = HashMap::new();
        images.insert(id, ImportedImage::new(PixelImage::Rgba(Arc::new(red(2, 2))), PixelImage::Rgba(Arc::new(red(1, 1))), "Layer"));
        let mut masks = HashMap::new();
        masks.insert(id, ImportedImage::new(PixelImage::Gray(Arc::new(Gray8Image::uniform(2, 2, 255))), PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, 255))), "Layer Mask"));
        let snapshot = ProjectSnapshot { manifest, images, masks };

        let mut options = CanvasSizeOptions::new(6, 6);
        options.anchor = 0;
        let output = CanvasResizer::resize(&snapshot, &options).unwrap();
        assert_eq!(output.manifest.layers[0].transform.origin, Point::new(1.0, 1.0));
        assert_eq!(output.manifest.layers[0].mask_placement.unwrap().origin, Point::new(1.0, 1.0));
        assert_eq!(output.manifest.guides.as_ref().unwrap()[0].position, 3.0);

        options.anchor = 8;
        let output = CanvasResizer::resize(&snapshot, &options).unwrap();
        assert_eq!(output.manifest.layers[0].transform.origin, Point::new(3.0, 3.0));
        assert_eq!(output.manifest.layers[0].mask_placement.unwrap().origin, Point::new(3.0, 3.0));
        assert_eq!(output.manifest.guides.as_ref().unwrap()[0].position, 5.0);
    }

    #[test]
    fn crop_content_offset_overrides_the_anchor() {
        let id = Uuid::new_v4();
        let snapshot = document(4, 4, record(id, transform(Point::ZERO, Size::new(4.0, 4.0))), red(4, 4));
        let mut options = CanvasSizeOptions::new(2, 2);
        options.content_offset = Some(Point::new(-1.0, -2.0));
        let output = CanvasResizer::resize(&snapshot, &options).unwrap();
        assert_eq!(output.manifest.layers[0].transform.origin, Point::new(-1.0, -2.0));
        // Crop keeps every pixel; only the placement moves.
        assert!(Arc::ptr_eq(&rgba_arc(&output.images[&id]), &rgba_arc(&snapshot.images[&id])));
    }
}
