//! The Filter menu's pixel work: every `PixelFilter` applier of `Document/Filters.swift`, plus the
//! `DitherSettings.apply` pipeline of `Document/Dither.swift` that Filter › Dither drives.
//!
//! The Swift versions of these ran on Core Image (`CIGaussianBlur`, `CIMotionBlur`, `CIBloom`) and
//! Core Graphics; here each is a CPU kernel over the canonical buffers. Where Core Image's kernel
//! isn't public, the substitute keeps documented behavior rather than inventing a look: Gaussian
//! blurs are separable with `radius = round(sigma * 3)` and normalize by their own weights (so flat
//! areas are untouched exactly); a blur that isn't clamped samples outside the image as nothing,
//! which is how a filter's blur softens the layer's own edges; motion blur tapers along the streak
//! with a Gaussian whose spread is the Core Image radius; bloom adds the blurred image to itself at
//! Core Image's intensity. The C kernels (`noise_add_at`, `adjust_colored_vignette`,
//! `adjust_tonal_contrast`, `lens_distort`, `dither_apply`, …) are called exactly as the Swift did.
//!
//! Blurs spread past a layer's edges: the caller grows the layer's grid first
//! (`compositor_core::image_ops::blur_margin`) and trims what stays empty afterwards
//! ([`PixelFilter::trimmed`]), which is what makes a blurred layer's border soft instead of smeared
//! against the edge.

use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::image_ops::{
    DitherColors, DitherPixelShape, DitherSettings, DitherStyle, FilterJob, FilterKind,
};
use compositor_core::imported_image::PixelImage;
use compositor_core::layer_transform::{pixel_to_document, LayerTransform};
use compositor_core::{CoreError, Gray8Image, Rgba8Image, Result};
use rayon::prelude::*;

use crate::adjustments;
use crate::brush_pixels::brush_alpha_bounds;
use crate::camera_raw::{CameraRawClipping, CameraRawSettings};
use crate::dither_pixels::{dither_apply, dither_dots, dither_glow, DitherParams};
use crate::lens_pixels::lens_distort;
use crate::masks::{ContentFill, SubjectRemoval};
use crate::noise_pixels::noise_add_at;

/// `CIMotionBlur`'s radius per pixel of streak length. Photoshop smears evenly along the whole
/// distance; Core Image tapers like a Gaussian whose spread is about its radius (measured on a
/// single dot). An even streak of length d spreads d / √12, so this radius matches its spread.
pub const MOTION_RADIUS_PER_PIXEL: f64 = 1.0 / 3.464_101_615_137_754_6; // 1 / sqrt(12)

/// Remove Distortion at ±100 moves the image's corners by this share of their distance from the
/// center.
pub const LENS_STRENGTH: f64 = 0.35;

/// `ProjectError.invalid`'s message.
pub(crate) fn invalid_settings() -> CoreError {
    CoreError::Message(
        "This is not a valid Compositor project, or its metadata is damaged.".to_string(),
    )
}

/// `ExportError.render`'s message.
pub(crate) fn render_failed() -> CoreError {
    CoreError::Message("The canvas could not be rendered. Try a smaller canvas.".to_string())
}

/// `CGRect.applying(_:)`: the smallest rectangle containing the transformed rect.
fn rect_applying(rect: Rect, transform: AffineTransform) -> Rect {
    let corners = rect.corners().map(|corner| transform.applying(corner));
    let (mut min_x, mut min_y) = (corners[0].x, corners[0].y);
    let (mut max_x, mut max_y) = (corners[0].x, corners[0].y);
    for corner in corners.iter().skip(1) {
        min_x = min_x.min(corner.x);
        min_y = min_y.min(corner.y);
        max_x = max_x.max(corner.x);
        max_y = max_y.max(corner.y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// Camera Raw's settings, which live in `compositor_pixels::camera_raw` — core cannot name them, so
/// they travel beside the job rather than inside it.
pub struct CameraRawInputs<'a> {
    pub settings: &'a CameraRawSettings,
    pub clipping: Option<CameraRawClipping>,
}

pub struct PixelFilter;

impl PixelFilter {
    /// `image` cropped to the pixels that are actually there, with the transform that keeps them
    /// in place: a blur is given generous room to spread, and whatever it leaves empty is cut away
    /// again.
    pub fn trimmed(image: &Rgba8Image, placed: &LayerTransform) -> Result<(Rgba8Image, LayerTransform)> {
        let width = image.width();
        let height = image.height();
        let full = Rect::new(0.0, 0.0, width as f64, height as f64);
        let edges = brush_alpha_bounds(image.data(), width, height, image.stride());
        let crop = Rect::new(
            edges[0] as f64,
            edges[1] as f64,
            (edges[2] - edges[0]) as f64,
            (edges[3] - edges[1]) as f64,
        );
        if !(crop.width() >= 1.0 && crop.height() >= 1.0) || crop == full {
            return Ok((image.clone(), placed.clone()));
        }
        let Some(cropped) = image.cropped(crop) else {
            return Ok((image.clone(), placed.clone()));
        };
        let mut result = placed.clone();
        result.size = Size::new(
            crop.width() * placed.size.width / full.width(),
            crop.height() * placed.size.height / full.height(),
        );
        let to_document = pixel_to_document(placed, width, height);
        let middle = to_document.applying(Point::new(crop.mid_x(), crop.mid_y()));
        result.origin = Point::new(
            middle.x - result.size.width / 2.0,
            middle.y - result.size.height / 2.0,
        );
        Ok((cropped, result))
    }

    /// The filter on one job, the same sequence `PixelFilter.run` performs.
    ///
    /// `camera_raw` is only read for [`FilterKind::CameraRaw`]: its settings type lives in
    /// `compositor_pixels::camera_raw`, which `compositor_core::image_ops::FilterJob` cannot name.
    pub fn run(job: &FilterJob, camera_raw: Option<CameraRawInputs<'_>>) -> Result<Rgba8Image> {
        let settings = job.settings.normalized();
        let image = &job.image;
        let width = image.width();
        let height = image.height();
        let extent = Rect::new(0.0, 0.0, width as f64, height as f64);
        let made = match job.kind {
            FilterKind::Curves => adjustments::apply_curves(image, &settings.curves)?,
            FilterKind::Exposure => adjustments::apply_exposure(image, &settings.exposure)?,
            FilterKind::GradientMap => {
                adjustments::apply_gradient_map(image, &settings.gradient_map)?
            }
            FilterKind::BlackWhite => adjustments::apply_black_white(image, &settings.black_white)?,
            FilterKind::ColorBalance => {
                adjustments::apply_color_balance(image, &settings.color_balance)?
            }
            FilterKind::CameraRaw => {
                let inputs = camera_raw.ok_or_else(invalid_settings)?;
                inputs.settings.apply(
                    image,
                    inputs.clipping,
                    job.scale,
                    job.seed,
                    job.visualizes_point_color,
                    job.shows_sharpen_mask,
                )?
            }
            // Grain sits in layer pixels; the job's seed gives each application its own pattern.
            FilterKind::Grain => adjustments::apply_grain(
                image,
                &settings.grain,
                Point::ZERO,
                1.0 / job.scale,
                Some(job.seed),
            )?,
            FilterKind::Dither => apply_dither(image, &settings.dither)?,
            FilterKind::RemoveBackground => {
                SubjectRemoval::run(image, &settings).map_err(|error| CoreError::Message(error.to_string()))?
            }
            FilterKind::ContentAwareFill => {
                ContentFill::run(job).map_err(|error| CoreError::Message(error.to_string()))?
            }
            FilterKind::GaussianBlur => {
                gaussian_blur_rgba(image, settings.radius * job.scale, false)
            }
            FilterKind::MotionBlur => motion_blur_rgba(
                image,
                settings.distance * job.scale * MOTION_RADIUS_PER_PIXEL,
                settings.angle * std::f64::consts::PI / 180.0,
            ),
            FilterKind::AddNoise => {
                // C, not Core Image: its random generator is uniform only, and Gaussian noise is
                // needed too.
                let mut result = image.clone();
                let stride = result.stride();
                noise_add_at(
                    result.data_mut(),
                    width,
                    height,
                    stride,
                    settings.amount as f32,
                    if settings.gaussian { 1 } else { 0 },
                    if settings.monochromatic { 1 } else { 0 },
                    job.seed,
                    job.noise_origin.x.floor() as i64,
                    job.noise_origin.y.floor() as i64,
                );
                result
            }
            FilterKind::Vignette => {
                let mut result = image.clone();
                // The canvas in this image's pixels, rows top-down as every buffer here is.
                let frame = match job.canvas {
                    Some(canvas) => rect_applying(canvas, job.mapping.inverted()),
                    None => extent,
                };
                let stride = result.stride();
                crate::adjust_pixels::adjust_colored_vignette(
                    result.data_mut(),
                    width,
                    height,
                    stride,
                    frame.min_x(),
                    frame.min_y(),
                    frame.width(),
                    frame.height(),
                    if job.canvas.is_none() { 0 } else { 1 },
                    settings.vignette_amount,
                    settings.vignette_midpoint,
                    settings.vignette_roundness,
                    settings.vignette_feather,
                    settings.vignette_highlights,
                    settings.vignette_color.red,
                    settings.vignette_color.green,
                    settings.vignette_color.blue,
                );
                result
            }
            FilterKind::BloomGlow => {
                bloom_rgba(image, settings.bloom_radius * job.scale, settings.bloom_amount / 50.0)
            }
            FilterKind::TonalContrast => {
                let base = gaussian_blur_rgba(image, settings.tonal_radius * job.scale, false);
                let mut result = image.clone();
                let (stride, base_stride) = (result.stride(), base.stride());
                crate::adjust_pixels::adjust_tonal_contrast(
                    result.data_mut(),
                    base.data(),
                    width,
                    height,
                    stride,
                    base_stride,
                    settings.tonal_amount,
                    settings.tonal_shadows,
                    settings.tonal_midtones,
                    settings.tonal_highlights,
                );
                result
            }
            FilterKind::LensCorrection => {
                // The warp is relative to the image's own size, so a downscaled preview bends the
                // same way.
                let mut result = Rgba8Image::new(width, height);
                lens_distort(
                    image.data(),
                    result.data_mut(),
                    width,
                    height,
                    image.stride(),
                    settings.distortion / 100.0 * LENS_STRENGTH,
                );
                result
            }
        };
        let Some(selection) = &job.selection else {
            return Ok(made);
        };
        Ok(adjustments::blend_through_selection(
            &made,
            &job.image,
            selection,
            job.mapping,
        ))
    }
}

// MARK: - Gaussian blurs

/// A separable Gaussian blur of premultiplied RGBA8. `clamped` extends the edge pixels outward
/// (`clampedToExtent()`, as the Swift's mask and glow blurs do); without it, samples outside the
/// image are nothing, so a layer's own edge softens rather than smearing outwards.
pub fn gaussian_blur_rgba(image: &Rgba8Image, sigma: f64, clamped: bool) -> Rgba8Image {
    if !(sigma > 0.01) || image.is_empty() {
        return image.clone();
    }
    let weights = gaussian_weights(sigma);
    let radius = (weights.len() / 2) as i64;
    let (width, height) = (image.width(), image.height());
    let mut rows = image.clone();
    rows.data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let source = image.data();
            let base = y * width * 4;
            for x in 0..width {
                let mut out = [0.0f32; 4];
                for (index, weight) in weights.iter().enumerate() {
                    let sample = x as i64 + index as i64 - radius;
                    if !clamped && (sample < 0 || sample >= width as i64) {
                        continue;
                    }
                    let sample = sample.clamp(0, width as i64 - 1) as usize;
                    for channel in 0..4 {
                        out[channel] += *weight as f32 * source[base + sample * 4 + channel] as f32;
                    }
                }
                let normalizer = if clamped {
                    weights.iter().sum::<f64>() as f32
                } else {
                    window_sum(&weights, x as i64, width as i64) as f32
                };
                for channel in 0..4 {
                    row[x * 4 + channel] = ((out[channel] / normalizer) + 0.5) as u8;
                }
            }
        });
    let blurred = rows;
    let mut result = blurred.clone();
    result
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let source = blurred.data();
            for x in 0..width {
                let mut out = [0.0f32; 4];
                for (index, weight) in weights.iter().enumerate() {
                    let sample = y as i64 + index as i64 - radius;
                    if !clamped && (sample < 0 || sample >= height as i64) {
                        continue;
                    }
                    let sample = sample.clamp(0, height as i64 - 1) as usize;
                    let base = sample * width * 4 + x * 4;
                    for channel in 0..4 {
                        out[channel] += *weight as f32 * source[base + channel] as f32;
                    }
                }
                let normalizer = if clamped {
                    weights.iter().sum::<f64>() as f32
                } else {
                    window_sum(&weights, y as i64, height as i64) as f32
                };
                for channel in 0..4 {
                    row[x * 4 + channel] = ((out[channel] / normalizer) + 0.5) as u8;
                }
            }
        });
    result
}

/// The same blur on an 8-bit gray raster: masks, and the Blur tool's coverage.
pub fn gaussian_blur_gray(image: &Gray8Image, sigma: f64, clamped: bool) -> Gray8Image {
    if !(sigma > 0.01) || image.is_empty() {
        return image.clone();
    }
    let weights = gaussian_weights(sigma);
    let radius = (weights.len() / 2) as i64;
    let (width, height) = (image.width(), image.height());
    let mut rows = image.clone();
    rows.data_mut()
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let source = image.data();
            let base = y * width;
            for x in 0..width {
                let mut out = 0.0f32;
                for (index, weight) in weights.iter().enumerate() {
                    let sample = x as i64 + index as i64 - radius;
                    if !clamped && (sample < 0 || sample >= width as i64) {
                        continue;
                    }
                    let sample = sample.clamp(0, width as i64 - 1) as usize;
                    out += *weight as f32 * source[base + sample] as f32;
                }
                let normalizer = if clamped {
                    weights.iter().sum::<f64>() as f32
                } else {
                    window_sum(&weights, x as i64, width as i64) as f32
                };
                row[x] = ((out / normalizer) + 0.5) as u8;
            }
        });
    let blurred = rows;
    let mut result = blurred.clone();
    result
        .data_mut()
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let source = blurred.data();
            for x in 0..width {
                let mut out = 0.0f32;
                for (index, weight) in weights.iter().enumerate() {
                    let sample = y as i64 + index as i64 - radius;
                    if !clamped && (sample < 0 || sample >= height as i64) {
                        continue;
                    }
                    let sample = sample.clamp(0, height as i64 - 1) as usize;
                    out += *weight as f32 * source[sample * width + x] as f32;
                }
                let normalizer = if clamped {
                    weights.iter().sum::<f64>() as f32
                } else {
                    window_sum(&weights, y as i64, height as i64) as f32
                };
                row[x] = ((out / normalizer) + 0.5) as u8;
            }
        });
    result
}

/// The blur of whichever raster is handed over, keeping its kind — the Blur tool's entry point.
pub fn gaussian_blur(image: &PixelImage, sigma: f64, clamped: bool) -> PixelImage {
    match image {
        PixelImage::Rgba(shared) => {
            PixelImage::Rgba(std::sync::Arc::new(gaussian_blur_rgba(shared, sigma, clamped)))
        }
        PixelImage::Gray(shared) => {
            PixelImage::Gray(std::sync::Arc::new(gaussian_blur_gray(shared, sigma, clamped)))
        }
    }
}

/// The Gaussian weights of a kernel `round(sigma * 3)` pixels either side of the center.
fn gaussian_weights(sigma: f64) -> Vec<f64> {
    let radius = (sigma * 3.0).round().max(1.0) as i64;
    let denominator = 2.0 * sigma * sigma;
    (-radius..=radius)
        .map(|offset| (-(offset * offset) as f64 / denominator).exp())
        .collect()
}

/// The weights that land inside the image for a center at `center`, so a pixel near the edge keeps
/// its brightness instead of fading.
fn window_sum(weights: &[f64], center: i64, size: i64) -> f64 {
    let radius = (weights.len() / 2) as i64;
    let mut sum = 0.0;
    for (index, weight) in weights.iter().enumerate() {
        let sample = center + index as i64 - radius;
        if sample >= 0 && sample < size {
            sum += weight;
        }
    }
    sum
}

// MARK: - Motion blur

/// `CIMotionBlur`: the image smeared along `angle` (counterclockwise from horizontal, Core Image's
/// convention) over a Gaussian whose spread is `radius`. Alpha is smeared with the color, as Core
/// Image blurs premultiplied pixels.
pub fn motion_blur_rgba(image: &Rgba8Image, radius: f64, angle: f64) -> Rgba8Image {
    if !(radius > 0.0) || image.is_empty() {
        return image.clone();
    }
    let sigma = radius;
    let radius = (sigma * 3.0).round().max(1.0) as i64;
    let denominator = 2.0 * sigma * sigma;
    let weights: Vec<f64> = (-radius..=radius)
        .map(|offset| (-(offset * offset) as f64 / denominator).exp())
        .collect();
    let total: f64 = weights.iter().sum();
    // Core Image's y axis points up, so its counterclockwise angle matches Photoshop's; the
    // document's y grows downward.
    let (dx, dy) = (angle.cos(), -angle.sin());
    let (width, height) = (image.width(), image.height());
    let source = image.data();
    let mut result = image.clone();
    result
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let mut out = [0.0f32; 4];
                for (index, weight) in weights.iter().enumerate() {
                    let offset = index as i64 - radius;
                    let sx = x as f64 - offset as f64 * dx;
                    let sy = y as f64 - offset as f64 * dy;
                    let sample = sample_bilinear(source, width, height, sx, sy);
                    for channel in 0..4 {
                        out[channel] += *weight as f32 * sample[channel];
                    }
                }
                for channel in 0..4 {
                    row[x * 4 + channel] = ((out[channel] / total as f32) + 0.5) as u8;
                }
            }
        });
    result
}

/// One premultiplied pixel of `source` at a fractional position, nothing outside the image.
fn sample_bilinear(source: &[u8], width: usize, height: usize, x: f64, y: f64) -> [f32; 4] {
    if x < -1.0 || y < -1.0 || x > width as f64 || y > height as f64 {
        return [0.0; 4];
    }
    let x0 = x.floor();
    let y0 = y.floor();
    let fx = (x - x0) as f32;
    let fy = (y - y0) as f32;
    let at = |x: i64, y: i64, channel: usize| -> f32 {
        if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
            0.0
        } else {
            source[(y as usize * width + x as usize) * 4 + channel] as f32
        }
    };
    let (x0i, y0i) = (x0 as i64, y0 as i64);
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        let top = at(x0i, y0i, channel) * (1.0 - fx) + at(x0i + 1, y0i, channel) * fx;
        let bottom = at(x0i, y0i + 1, channel) * (1.0 - fx) + at(x0i + 1, y0i + 1, channel) * fx;
        out[channel] = top * (1.0 - fy) + bottom * fy;
    }
    out
}

// MARK: - Bloom / Glow

/// `CIBloom`: the image plus its own blurred light, `intensity` times over (Core Image's
/// `inputIntensity`, which the Swift feeds `bloomAmount / 50`). Premultiplied, so the glow is
/// visible past a transparent layer's bright pixels.
pub fn bloom_rgba(image: &Rgba8Image, radius: f64, intensity: f64) -> Rgba8Image {
    if !(radius > 0.0) || intensity == 0.0 || image.is_empty() {
        return image.clone();
    }
    let blurred = gaussian_blur_rgba(image, radius, false);
    let mut result = image.clone();
    result
        .data_mut()
        .par_iter_mut()
        .zip(blurred.data().par_iter())
        .with_min_len(4)
        .for_each(|(target, light)| {
            let value = *target as f64 + *light as f64 * intensity;
            *target = value.clamp(0.0, 255.0).round() as u8;
        });
    result
}

// MARK: - Dither

/// `DitherSettings.apply`: chunky pixels, the dither kernel, the CRT's glow and the dot gaps.
pub fn apply_dither(image: &Rgba8Image, settings: &DitherSettings) -> Result<Rgba8Image> {
    if image.is_empty() {
        return Ok(image.clone());
    }
    let settings = settings.normalized();
    // ASCII and Scanlines draw at full resolution: shrinking the image first would blur and break
    // them up.
    let block = if settings.style.uses_pixel_size() {
        settings.pixel_size.max(1.0) as usize
    } else {
        1
    };
    // Chunky pixels: dither a copy averaged down by the pixel size, then blow it back up without
    // smoothing.
    let working = if block > 1 {
        let width = image.width().div_ceil(block);
        let height = image.height().div_ceil(block);
        average_down_rgba(image, width.max(1), height.max(1))
    } else {
        image.clone()
    };
    let dithered = dither_settings(&working, &settings)?;
    let dithered = if settings.style == DitherStyle::Scanlines && settings.glow > 0.0 {
        glowing(&dithered, &settings)?
    } else {
        dithered
    };
    if block == 1 {
        return Ok(dithered);
    }
    let mut full = upscale_rgba_nearest(&dithered, dithered.width() * block, dithered.height() * block);
    if settings.pixel_shape == DitherPixelShape::Dot {
        // The gaps are the dark color: black, or the one picked.
        let gap: [u8; 3] = if settings.colors == DitherColors::TwoColors {
            dark_bytes(&settings.dark)
        } else {
            [0, 0, 0]
        };
        let (width, height, stride) = (full.width(), full.height(), full.stride());
        dither_dots(full.data_mut(), width, height, stride, block as i32, &gap);
    }
    Ok(full)
}

/// The three bytes of an adjustment color, as the dither kernel takes them.
fn dark_bytes(color: &compositor_core::layer_adjustment::AdjustmentColor) -> [u8; 3] {
    [
        (color.red * 255.0).round() as u8,
        (color.green * 255.0).round() as u8,
        (color.blue * 255.0).round() as u8,
    ]
}

/// The lines' light, blurred across a few line spacings and added back over them, as a CRT's
/// phosphors bloom.
fn glowing(image: &Rgba8Image, settings: &DitherSettings) -> Result<Rgba8Image> {
    // The glow is wide and soft, so it's blurred at a fraction of the size and scaled back up: the
    // same light for a small part of the work.
    let sigma = settings.line_spacing * 3.0 + 3.0;
    let shrink = (sigma / 4.0).floor().max(1.0);
    let small_width = ((image.width() as f64 / shrink).round() as usize).max(1);
    let small_height = ((image.height() as f64 / shrink).round() as usize).max(1);
    let small = average_down_rgba(image, small_width, small_height);
    let blurred = gaussian_blur_rgba(&small, sigma / shrink, true);
    let bloom = upscale_rgba_bilinear(&blurred, image.width(), image.height());
    let mut result = image.clone();
    let (width, height, stride) = (image.width(), image.height(), result.stride());
    dither_glow(
        result.data_mut(),
        bloom.data(),
        width,
        height,
        stride,
        (settings.glow / 100.0 * 2.5) as f32,
    );
    Ok(result)
}

/// One run of the dither kernel, with the ASCII glyphs it needs.
fn dither_settings(image: &Rgba8Image, settings: &DitherSettings) -> Result<Rgba8Image> {
    let cell = if settings.style == DitherStyle::Scanlines {
        settings.line_spacing
    } else {
        settings.cell_size
    };
    let glyphs = if settings.style == DitherStyle::Ascii {
        ascii_glyphs(
            if settings.characters.is_empty() {
                DitherSettings::DEFAULT_CHARACTERS
            } else {
                settings.characters.as_str()
            },
            settings.text_size as usize,
        )
    } else {
        Glyphs { maps: Vec::new(), coverage: Vec::new(), width: 1, height: 1 }
    };
    let (dark_color, light_color) = if settings.colors == DitherColors::TwoColors {
        (dark_bytes(&settings.dark), dark_bytes(&settings.light))
    } else {
        ([0, 0, 0], [255, 255, 255])
    };
    let mut result = image.clone();
    let params = DitherParams {
        style: settings.style.code(),
        levels: settings.levels as i32,
        diffusion: (settings.diffusion / 100.0) as f32,
        density: (settings.density / 100.0) as f32,
        contrast: (settings.contrast / 100.0) as f32,
        cell: cell as i32,
        angle: (settings.angle * std::f64::consts::PI / 180.0) as f32,
        light_on_dark: if settings.light_on_dark { 1 } else { 0 },
        original_colors: if settings.colors == DitherColors::Original { 1 } else { 0 },
        dark: dark_color,
        light: light_color,
        glyph_width: glyphs.width as i32,
        glyph_height: glyphs.height as i32,
        glyphs: &glyphs.maps,
        glyph_coverage: &glyphs.coverage,
        glyph_count: glyphs.coverage.len() as i32,
        dots: (settings.dots / 100.0) as f32,
        wobble: settings.wobble as f32,
    };
    let (width, height, stride) = (image.width(), image.height(), result.stride());
    let ok = dither_apply(result.data_mut(), width, height, stride, &params);
    if ok == 0 {
        return Err(render_failed());
    }
    Ok(result)
}

/// The ASCII character cells: each distinct character drawn in a monospaced font, sorted from least
/// ink to most.
struct Glyphs {
    maps: Vec<u8>,
    coverage: Vec<f32>,
    width: usize,
    height: usize,
}

/// Each distinct character drawn into a cell of monospaced text, `line_height` tall and one
/// character wide, on a shared baseline as a terminal lays them out, sorted from least ink to most.
///
/// The Swift drew these with Core Text (`NSFont.monospacedSystemFont`) and centered each cell on
/// its advance width; here the font comes from the system's font database and the glyph from
/// `fontdue`, keeping the same cell size, baseline and centering.
fn ascii_glyphs(characters: &str, line_height: usize) -> Glyphs {
    let height = line_height.max(1);
    let size = height as f32 / 1.2;
    let Some(font) = monospaced_font() else {
        return Glyphs { maps: Vec::new(), coverage: Vec::new(), width: 1, height: 1 };
    };
    let metrics = font.horizontal_line_metrics(size);
    let (ascender, descender) = match metrics {
        Some(metrics) => (metrics.ascent, metrics.descent),
        None => (size * 0.8, -size * 0.2),
    };
    let width = font
        .metrics('M', size)
        .advance_width
        .round()
        .max(1.0) as usize;
    let baseline = ((height as f32 - (ascender - descender)) / 2.0 - descender).round();
    let mut drawn: Vec<(Vec<u8>, f32)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for character in characters.chars() {
        if !seen.insert(character) {
            continue;
        }
        let mut map = vec![0u8; width * height];
        let (metrics, bitmap) = font.rasterize(character, size);
        let pen_x = ((width as f32 - metrics.advance_width) / 2.0).round();
        let origin_x = pen_x as i64 + metrics.xmin as i64;
        let origin_y = baseline as i64 - (metrics.ymin as i64 + metrics.height as i64);
        for row in 0..metrics.height {
            let y = origin_y + row as i64;
            if y < 0 || y >= height as i64 {
                continue;
            }
            for column in 0..metrics.width {
                let x = origin_x + column as i64;
                if x < 0 || x >= width as i64 {
                    continue;
                }
                let value = bitmap[row * metrics.width + column];
                let target = y as usize * width + x as usize;
                if value > map[target] {
                    map[target] = value;
                }
            }
        }
        let coverage = map.iter().map(|value| *value as f32).sum::<f32>() / (255.0 * (width * height) as f32);
        drawn.push((map, coverage));
    }
    drawn.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let coverage = drawn.iter().map(|(_, coverage)| *coverage).collect();
    let maps = drawn.into_iter().flat_map(|(map, _)| map).collect();
    Glyphs { maps, coverage, width, height }
}

/// A bold monospaced face from the system's fonts, the stand-in for
/// `NSFont.monospacedSystemFont(ofSize:weight: .bold)`.
fn monospaced_font() -> Option<fontdue::Font> {
    use fontdb::{Family, Query, Style, Weight};
    static FONT: std::sync::LazyLock<Option<fontdue::Font>> = std::sync::LazyLock::new(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        let query = Query {
            families: &[Family::Monospace],
            weight: Weight::BOLD,
            stretch: fontdb::Stretch::Normal,
            style: Style::Normal,
        };
        let id = database.query(&query)?;
        let (data, index) = database.with_face_data(id, |data, index| (data.to_vec(), index))?;
        let settings = fontdue::FontSettings {
            collection_index: index as u32,
            ..fontdue::FontSettings::default()
        };
        fontdue::Font::from_bytes(data, settings).ok()
    });
    FONT.clone()
}

// MARK: - Resampling (the Core Graphics `draw` calls Dither makes)

/// A box average of the image down to `width` × `height`: the Swift's `.high` interpolation when
/// the dither panel shrinks an image to its chunky pixel grid. Averaging every source pixel that
/// lands in a destination pixel is what that draw is for.
fn average_down_rgba(image: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut result = Rgba8Image::new(width.max(1), height.max(1));
    if image.is_empty() || width == 0 || height == 0 {
        return result;
    }
    let source = image.data();
    let (source_width, source_height) = (image.width(), image.height());
    result
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let y0 = y * source_height / height;
            let y1 = (((y + 1) * source_height).div_ceil(height)).max(y0 + 1).min(source_height);
            for x in 0..width {
                let x0 = x * source_width / width;
                let x1 = (((x + 1) * source_width).div_ceil(width)).max(x0 + 1).min(source_width);
                let mut sums = [0.0f32; 4];
                let mut count = 0.0f32;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let offset = (sy * source_width + sx) * 4;
                        for channel in 0..4 {
                            sums[channel] += source[offset + channel] as f32;
                        }
                        count += 1.0;
                    }
                }
                for channel in 0..4 {
                    row[x * 4 + channel] = (sums[channel] / count + 0.5) as u8;
                }
            }
        });
    result
}

/// Nearest-neighbor, the `.none` interpolation the Swift draws chunky dither pixels back up with.
fn upscale_rgba_nearest(image: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut result = Rgba8Image::new(width.max(1), height.max(1));
    if image.is_empty() || width == 0 || height == 0 {
        return result;
    }
    let source = image.data();
    let (source_width, source_height) = (image.width(), image.height());
    result
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let sy = (y * source_height / height).min(source_height - 1);
            for x in 0..width {
                let sx = (x * source_width / width).min(source_width - 1);
                let offset = (sy * source_width + sx) * 4;
                row[x * 4..x * 4 + 4].copy_from_slice(&source[offset..offset + 4]);
            }
        });
    result
}

/// The same as [`sample_bilinear`], with the edge pixels extended outward: the sampling a Core Image
/// image that was `clampedToExtent()` gives a transform, which is how the dither glow is scaled
/// back up.
fn sample_bilinear_clamped(source: &[u8], width: usize, height: usize, x: f64, y: f64) -> [f32; 4] {
    let x0 = (x.floor() as i64).clamp(0, width as i64 - 1);
    let y0 = (y.floor() as i64).clamp(0, height as i64 - 1);
    let x1 = (x0 + 1).min(width as i64 - 1);
    let y1 = (y0 + 1).min(height as i64 - 1);
    let fx = (x - x0 as f64).clamp(0.0, 1.0) as f32;
    let fy = (y - y0 as f64).clamp(0.0, 1.0) as f32;
    let at = |x: i64, y: i64, channel: usize| -> f32 {
        source[(y as usize * width + x as usize) * 4 + channel] as f32
    };
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        let top = at(x0, y0, channel) * (1.0 - fx) + at(x1, y0, channel) * fx;
        let bottom = at(x0, y1, channel) * (1.0 - fx) + at(x1, y1, channel) * fx;
        out[channel] = top * (1.0 - fy) + bottom * fy;
    }
    out
}

/// Bilinear, the sampling a Core Image transform uses when the dither glow is scaled back up.
fn upscale_rgba_bilinear(image: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut result = Rgba8Image::new(width.max(1), height.max(1));
    if image.is_empty() || width == 0 || height == 0 {
        return result;
    }
    let source = image.data();
    let (source_width, source_height) = (image.width(), image.height());
    result
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let sy = (y as f64 + 0.5) * source_height as f64 / height as f64 - 0.5;
            for x in 0..width {
                let sx = (x as f64 + 0.5) * source_width as f64 / width as f64 - 0.5;
                let sample = sample_bilinear_clamped(source, source_width, source_height, sx, sy);
                for channel in 0..4 {
                    row[x * 4 + channel] = (sample[channel] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::image_ops::DitherPixelShape;

    fn opaque(width: usize, height: usize, color: [u8; 4]) -> Rgba8Image {
        Rgba8Image::opaque(width, height, color)
    }

    #[test]
    fn blur_spreads_past_the_layer_edge_without_clamping() {
        // A 3-pixel block in the middle of a transparent 11 x 1 row.
        let mut image = Rgba8Image::new(11, 1);
        for x in 4..7 {
            image.set(x, 0, [255, 255, 255, 255]);
        }
        let blurred = gaussian_blur_rgba(&image, 1.0, false);
        let alpha = |x: usize| blurred.get(x, 0)[3];
        assert!(alpha(3) > 0 && alpha(7) > 0, "the blur reaches past the block");
        assert!(alpha(3) < alpha(5), "and fades outward");
        // Nothing at all outside the kernel's reach.
        assert_eq!(alpha(0), 0);
        // The same signal on the edge of the image loses its outside half instead of sliding in.
        let mut edge = Rgba8Image::new(11, 1);
        for x in 0..3 {
            edge.set(x, 0, [255, 255, 255, 255]);
        }
        let blurred = gaussian_blur_rgba(&edge, 1.0, false);
        let clamped = gaussian_blur_rgba(&edge, 1.0, true);
        assert!(blurred.get(0, 0)[3] < clamped.get(0, 0)[3]);
        assert!(blurred.get(0, 0)[3] > 0);
    }

    #[test]
    fn blur_leaves_a_flat_field_exactly_alone() {
        let image = opaque(16, 16, [120, 200, 40, 255]);
        let blurred = gaussian_blur_rgba(&image, 2.0, true);
        for pixel in blurred.pixels() {
            assert_eq!(pixel, [120, 200, 40, 255]);
        }
        let blurred = gaussian_blur_rgba(&image, 2.0, false);
        // The interior is still exact; only the border dims.
        assert_eq!(blurred.get(8, 8), [120, 200, 40, 255]);
    }

    /// Ported from `FilterTests.gaussianBlurSoftensAHardEdgeAndSpreadsPastTheLayerEdgeAsOneUndoStep`.
    /// The session half (the filter edit, `canEditLayers`, the undo count, the committed settings)
    /// belongs to `compositor-session`; this is the kernel half: a hard half-image edge blurred
    /// without clamping, with the room the blur is given and the trim that follows it.
    #[test]
    fn gaussian_blur_softens_a_hard_edge_and_spreads_past_the_layer_edge_as_one_undo_step() {
        use compositor_core::image_ops::{blur_margin, FilterSettings};

        let settings = FilterSettings {
            radius: 3.0,
            ..FilterSettings::default()
        };
        // Left half opaque white, right half transparent.
        let mut layer = Rgba8Image::new(40, 20);
        for y in 0..20 {
            for x in 0..20 {
                layer.set(x, y, [255, 255, 255, 255]);
            }
        }
        // The caller grows the layer's grid first (`compositor_core::image_ops::blur_margin`) so the
        // blur has somewhere to spread into.
        let margin = blur_margin(FilterKind::GaussianBlur, &settings) as i32;
        let mut placed = LayerTransform::default();
        placed.origin = Point::new(-(margin as f64), -(margin as f64));
        placed.size = Size::new(
            (40 + 2 * margin) as f64,
            (20 + 2 * margin) as f64,
        );
        let grown = layer.padded(
            [margin, margin],
            Rect::new(0.0, 0.0, placed.size.width, placed.size.height),
        );
        let blurred = gaussian_blur_rgba(&grown, settings.radius, false);
        // ...and whatever stays empty is cut away again, the pixels kept in place.
        let (result, transform) = PixelFilter::trimmed(&blurred, &placed).expect("trim");
        assert!(
            transform.origin.x < 0.0 && transform.origin.y < 0.0,
            "the layer grew on every side: origin {:?}",
            transform.origin
        );
        // The strongest evidence that the blur left the layer: it was 20 tall and opaque top to
        // bottom, so it could not have grown vertically unless the blur went past the edge and the
        // layer was given room.
        assert!(
            result.height() > 20,
            "the blur spread past the layer's edge: height {}",
            result.height()
        );
        assert!(
            result.width() < 40,
            "and the half that stayed empty was trimmed away: width {}",
            result.width()
        );
        let middle: Vec<u8> = (0..result.width())
            .map(|x| result.get(x, result.height() / 2)[3])
            .collect();
        // 250 rather than 255: the block's centre is 10 px from its edges, which at this radius
        // leaves it a fraction of a level below full opacity. What would break here is the inside
        // fading, not rounding.
        assert!(
            middle.iter().copied().max().unwrap_or(0) >= 250,
            "the block's inside is untouched: {:?}",
            middle.iter().max()
        );
        assert!(
            middle.iter().any(|alpha| *alpha > 20 && *alpha < 235),
            "the hard edge is now soft: {middle:?}"
        );
        assert!(*middle.last().unwrap() < 20, "and it fades out on the far side");
    }

    #[test]
    fn blur_below_the_threshold_returns_the_same_pixels() {
        let image = opaque(4, 4, [1, 2, 3, 255]);
        assert_eq!(gaussian_blur_rgba(&image, 0.005, false), image);
        let gray = Gray8Image::uniform(4, 4, 77);
        assert_eq!(gaussian_blur_gray(&gray, 0.0, true), gray);
    }

    #[test]
    fn gray_blur_normalizes_flat_fields_too() {
        let image = Gray8Image::uniform(8, 8, 200);
        let blurred = gaussian_blur_gray(&image, 1.5, true);
        assert!(blurred.data().iter().all(|value| *value == 200));
    }

    #[test]
    fn motion_blur_streaks_along_its_angle_counterclockwise_from_horizontal() {
        let mut image = Rgba8Image::new(41, 41);
        image.set(20, 20, [255, 255, 255, 255]);
        let alpha = |image: &Rgba8Image, x: usize, y: usize| image.get(x, y)[3];
        // Horizontally: light either side, nothing above or below.
        let horizontal = motion_blur_rgba(&image, 16.0 * MOTION_RADIUS_PER_PIXEL, 0.0);
        assert!(alpha(&horizontal, 24, 20) > 0 && alpha(&horizontal, 16, 20) > 0);
        assert_eq!(alpha(&horizontal, 20, 24), 0);
        // Vertically.
        let vertical = motion_blur_rgba(&image, 16.0 * MOTION_RADIUS_PER_PIXEL, std::f64::consts::FRAC_PI_2);
        assert!(alpha(&vertical, 20, 24) > 0 && alpha(&vertical, 20, 16) > 0);
        assert_eq!(alpha(&vertical, 24, 20), 0);
        // 45° runs up-right and down-left on screen, never up-left.
        let diagonal = motion_blur_rgba(&image, 16.0 * MOTION_RADIUS_PER_PIXEL, std::f64::consts::FRAC_PI_4);
        assert!(alpha(&diagonal, 23, 17) > 0 && alpha(&diagonal, 17, 23) > 0);
        assert_eq!(alpha(&diagonal, 17, 17), 0);
    }

    #[test]
    fn bloom_adds_its_own_light_and_keeps_alpha_where_pixels_are() {
        let mut image = Rgba8Image::new(65, 65);
        for y in 30..35 {
            for x in 30..35 {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        let bloomed = bloom_rgba(&image, 12.0, 2.0);
        assert!(bloomed.get(40, 32)[0] > 0, "the light spreads");
        assert!(bloomed.get(2, 2)[0] < bloomed.get(40, 32)[0]);
        assert_eq!(bloomed.get(32, 32)[3], 255);
        // An intensity of zero is the identity.
        assert_eq!(bloom_rgba(&image, 12.0, 0.0), image);
    }

    #[test]
    fn motion_blur_leaves_a_flat_field_untouched_where_its_kernel_fits() {
        // Well inside the image the kernel's weights sum to 1, so nothing changes; near an edge the
        // streak pulls in the nothing outside, as Core Image does.
        let image = opaque(41, 41, [200, 100, 50, 255]);
        let streaked = motion_blur_rgba(&image, 4.0, 0.7);
        assert_eq!(streaked.get(20, 20), [200, 100, 50, 255]);
        assert_ne!(streaked.get(0, 0), [200, 100, 50, 255]);
    }

    #[test]
    fn dither_dots_round_the_dark_gaps_between_lit_pixels() {
        // A flat white layer dithered to white, drawn in 4 x 4 dots on the dark color.
        let image = Rgba8Image::opaque(8, 8, [255, 255, 255, 255]);
        let settings = DitherSettings {
            style: DitherStyle::Atkinson,
            pixel_size: 4.0,
            pixel_shape: DitherPixelShape::Dot,
            colors: DitherColors::TwoColors,
            ..DitherSettings::default()
        };
        let dithered = apply_dither(&image, &settings).expect("dither");
        assert_eq!(dithered.width(), 8);
        assert_eq!(dithered.height(), 8);
        // The middle of a cell keeps its light, the corners darken toward the gap.
        assert_eq!(dithered.get(2, 2), [255, 255, 255, 255]);
        assert!(dithered.get(0, 0)[0] < 60, "{:?}", dithered.get(0, 0));
        assert!(dithered.get(4, 4)[0] < 60);
        // Alpha is kept.
        assert!(dithered.pixels().all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn dither_scanlines_draw_lines_of_light_spaced_by_the_line_spacing() {
        let image = Rgba8Image::opaque(16, 32, [255, 255, 255, 255]);
        let settings = DitherSettings {
            style: DitherStyle::Scanlines,
            line_spacing: 8.0,
            glow: 0.0,
            ..DitherSettings::default()
        };
        let dithered = apply_dither(&image, &settings).expect("dither");
        let column: Vec<u8> = (0..32).map(|y| dithered.get(5, y)[0]).collect();
        assert!(column.iter().any(|value| *value > 200), "a lit line: {column:?}");
        assert!(column.iter().any(|value| *value < 40), "dark screen between lines: {column:?}");
    }

    #[test]
    fn glow_lights_the_screen_between_the_lines() {
        let image = Rgba8Image::opaque(16, 32, [255, 255, 255, 255]);
        let base = DitherSettings {
            style: DitherStyle::Scanlines,
            line_spacing: 8.0,
            glow: 0.0,
            ..DitherSettings::default()
        };
        let plain = apply_dither(&image, &base).expect("dither");
        let glowing = apply_dither(&image, &DitherSettings { glow: 100.0, ..base }).expect("dither");
        let dark = |image: &Rgba8Image| (0..32).map(|y| image.get(5, y)[0]).min().unwrap_or(0);
        assert!(
            dark(&glowing) > dark(&plain),
            "the glow lights the dark screen: {} vs {}",
            dark(&glowing),
            dark(&plain)
        );
    }

    #[test]
    fn ascii_cells_are_sorted_by_ink_and_drawn_from_the_system_font() {
        let glyphs = ascii_glyphs(" .:-=+*#%@", 14);
        assert_eq!(glyphs.height, 14);
        assert!(glyphs.width >= 1);
        assert_eq!(glyphs.coverage.len(), 10);
        assert_eq!(glyphs.maps.len(), 10 * glyphs.width * glyphs.height);
        // Least ink first, most last.
        for pair in glyphs.coverage.windows(2) {
            assert!(pair[0] <= pair[1], "{:?}", glyphs.coverage);
        }
        // The space draws nothing; the densest character draws something.
        assert_eq!(glyphs.coverage[0], 0.0);
        if monospaced_font().is_some() {
            assert!(glyphs.coverage[9] > 0.0, "{:?}", glyphs.coverage);
        }
        // Repeated characters are drawn once.
        let repeated = ascii_glyphs("aaa", 14);
        assert_eq!(repeated.coverage.len(), 1);
    }

    #[test]
    fn dither_is_deterministic_and_keeps_alpha() {
        let image = opaque(8, 8, [128, 128, 128, 255]);
        let settings = DitherSettings::default();
        let once = apply_dither(&image, &settings).expect("dither");
        let twice = apply_dither(&image, &settings).expect("dither");
        assert_eq!(once, twice);
        assert!(once.pixels().all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn trimmed_cuts_the_empty_room_away_and_keeps_the_pixels_in_place() {
        let mut image = Rgba8Image::new(40, 20);
        for y in 0..20 {
            for x in 0..10 {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        let mut placed = LayerTransform::default();
        placed.origin = Point::new(-8.0, -8.0);
        placed.size = Size::new(40.0, 20.0);
        let (cropped, transform) = PixelFilter::trimmed(&image, &placed).expect("trim");
        assert_eq!(cropped.width(), 10);
        assert_eq!(cropped.height(), 20);
        // The crop is the layer's left half: its own grid is unchanged, so its size halves in x.
        assert_eq!(transform.origin.x, -8.0);
        assert_eq!(transform.origin.y, -8.0);
        assert_eq!(transform.size.width, 10.0);
        assert_eq!(transform.size.height, 20.0);
    }

    #[test]
    fn trimmed_leaves_a_fully_opaque_image_alone() {
        let image = opaque(6, 4, [255, 255, 255, 255]);
        let mut placed = LayerTransform::default();
        placed.size = Size::new(6.0, 4.0);
        let (cropped, transform) = PixelFilter::trimmed(&image, &placed).expect("trim");
        assert_eq!(cropped, image);
        assert_eq!(transform.size.width, 6.0);
    }

    #[test]
    fn trimmed_scales_a_placed_sub_image_back_into_place() {
        // A 20 x 10 layer placed at (100, 50) 40 x 20 document units wide, whose left half is empty.
        let mut image = Rgba8Image::new(20, 10);
        for y in 0..10 {
            for x in 10..20 {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        let mut placed = LayerTransform::default();
        placed.origin = Point::new(100.0, 50.0);
        placed.size = Size::new(40.0, 20.0);
        let (cropped, transform) = PixelFilter::trimmed(&image, &placed).expect("trim");
        assert_eq!(cropped.width(), 10);
        assert_eq!(transform.size.width, 20.0);
        // The kept pixels were the document's right half: origin moves to the layer's middle.
        assert_eq!(transform.origin.x, 120.0);
        assert_eq!(transform.origin.y, 50.0);
    }

    #[test]
    fn motion_blur_radius_per_pixel_is_one_over_root_twelve() {
        assert!((MOTION_RADIUS_PER_PIXEL - 1.0 / 12.0f64.sqrt()).abs() < 1e-15);
        assert_eq!(LENS_STRENGTH, 0.35);
    }

    #[test]
    fn resampling_helpers_keep_the_corners_and_the_kind() {
        let image = Rgba8Image::opaque(4, 4, [10, 20, 30, 255]);
        let down = average_down_rgba(&image, 2, 2);
        assert_eq!(down.width(), 2);
        assert_eq!(down.get(0, 0), [10, 20, 30, 255]);
        let up = upscale_rgba_nearest(&down, 8, 8);
        assert_eq!(up.get(7, 7), [10, 20, 30, 255]);
        // Bilinear extends the edges, so the corner is exactly the corner pixel.
        let bilinear = upscale_rgba_bilinear(&down, 8, 8);
        assert_eq!(bilinear.get(0, 0), [10, 20, 30, 255]);
        assert_eq!(bilinear.get(7, 7), [10, 20, 30, 255]);
        let clamped = sample_bilinear_clamped(&[0, 0, 0, 255, 10, 20, 30, 255], 2, 1, -0.5, 0.0);
        assert_eq!(clamped, [0.0, 0.0, 0.0, 255.0]);
    }
}
