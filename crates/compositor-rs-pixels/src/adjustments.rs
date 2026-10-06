//! The whole-image adjustments: the pixel appliers behind the Filter menu's color adjustments,
//! the adjustment layers and Invert.
//!
//! Ports `Document/ImageAdjustments.swift` (`ImageAdjustmentPixels`, Exposure, Gradient Map,
//! Black & White, Color Balance, Grain), `Document/Curves.swift`, `Document/Levels.swift`,
//! `Document/LevelsAutomatic.swift`, `Document/HueSaturation.swift`, `Document/PixelAdjust.swift`
//! and `Document/PixelInvert.swift`.
//!
//! Every applier takes the canonical `compositor_rs_core` buffers and the settings value type from
//! `compositor_rs_core::layer_adjustment`, and returns the adjusted buffer. The C kernels of
//! [`crate::adjust_pixels`] and [`crate::levels_pixels`] do the per-pixel work exactly where the
//! Swift called them; the `CGImage` plumbing around them (drawing into an RGBA context, running
//! the kernel, `makeImage`) is [`ImageAdjustmentPixels::run`].
//!
//! `Document/Dither.swift`'s appliers are not here: they are [`crate::filters::apply_dither`],
//! which needs the Filter menu's whole job rather than one adjustment's settings.

use std::sync::{Arc, LazyLock, Mutex};

use compositor_rs_core::geom::{AffineTransform, Point, Rect};
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_adjustment::{
    AdjustmentColor, BlackWhiteSettings, ColorBalanceSettings, ColorRange, CurvesSettings,
    ExposureSettings, GradientMapSettings, GrainSettings, HueBand, HueSaturationSettings,
    LevelRange, LevelsSettings, RangeAdjustment,
};
use compositor_rs_core::selection::SelectionClip;
use compositor_rs_core::{Gray8Image, Rgba8Image, Result, SharedImage};
use rayon::prelude::*;

use crate::canvas::{Canvas, InterpolationQuality};
use crate::filters::invalid_settings;
use crate::gradient::gradient_map_table;
use crate::levels_pixels::{cube_apply, levels_apply, levels_histogram};
use crate::raster::Raster;

/// The longest side of a Layers-panel thumbnail (`PixelAdjust.thumbnail(of:)`).
const THUMBNAIL_SIDE: f64 = 96.0;

/// `PixelAdjust`: the shared plumbing for whole-image adjustments — rendering a float image into
/// an 8-bit buffer, selection coverage on an image's own pixel grid, and blending a result back
/// through a selection.
pub struct PixelAdjust;

impl PixelAdjust {
    /// `PixelAdjust.render(_:width:height:isMask:)`: a float image rendered into an 8-bit buffer.
    ///
    /// **Substitution.** The Swift handed a `CIImage` to a `CIContext` built with no working and no
    /// output color space, so the values passed through unchanged, after the caller had applied
    /// `.cropped(to: extent)`. Here the caller passes the already-cropped pixels: `width * height`
    /// RGBA floats, four per pixel. Each is clamped to 0…1, scaled and rounded to the nearest byte,
    /// which is what Core Image's `.RGBA8` conversion does. A mask takes each pixel's red channel:
    /// every mask the Swift rendered came from a neutral image, where `.L8` and the red channel are
    /// the same number.
    pub fn render(floats: &[f32], width: usize, height: usize, is_mask: bool) -> PixelImage {
        if is_mask {
            let mut gray = Gray8Image::new(width, height);
            for index in 0..width * height {
                gray.set(index % width, index / width, byte(floats[index * 4]));
            }
            PixelImage::Gray(Arc::new(gray))
        } else {
            let mut rgba = Rgba8Image::new(width, height);
            for index in 0..width * height {
                let pixel = &floats[index * 4..index * 4 + 4];
                rgba.set(
                    index % width,
                    index / width,
                    [byte(pixel[0]), byte(pixel[1]), byte(pixel[2]), byte(pixel[3])],
                );
            }
            PixelImage::Rgba(Arc::new(rgba))
        }
    }

    /// `PixelAdjust.thumbnail(of:)`: a small preview for the Layers panel, matching imported
    /// thumbnails — at most 96 px on the long side, drawn with the nearest source pixel (`Int`
    /// truncates the scaled size, `max(1, …)` keeps it at least one pixel).
    pub fn thumbnail(image: &Rgba8Image) -> Rgba8Image {
        let factor = (THUMBNAIL_SIDE / image.width().max(image.height()) as f64).min(1.0);
        let width = ((image.width() as f64 * factor) as usize).max(1);
        let height = ((image.height() as f64 * factor) as usize).max(1);
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::None);
        canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
        canvas.into_rgba()
    }
}

/// One float channel to a byte: clamped to 0…1, scaled, rounded to the nearest.
fn byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// `PixelAdjust.thumbnail(of:)` for an asset's shared pixels.
pub fn thumbnail(image: &SharedImage) -> Rgba8Image {
    PixelAdjust::thumbnail(image)
}

/// `PixelAdjust.coverage(_:width:height:pixelToDocument:)`: selection coverage rasterized on the
/// image's pixel grid (a fill, which is exact).
pub fn coverage(
    selection: &SelectionClip,
    width: usize,
    height: usize,
    pixel_to_document: AffineTransform,
) -> Gray8Image {
    let mut canvas = Raster::context(width, height, true);
    canvas.set_fill_gray(0.0);
    canvas.fill_rect(Rect::new(0.0, 0.0, width as f64, height as f64));
    canvas.concatenate(pixel_to_document.inverted());
    apply_selection_clip(&mut canvas, selection);
    canvas.set_fill_gray(1.0);
    canvas.fill_rect(selection.rect);
    canvas.into_gray()
}

/// `SelectionClip.apply(to:)`: clips to the coverage's region, or clips everything away when the
/// selection is empty.
fn apply_selection_clip(canvas: &mut Canvas, clip: &SelectionClip) {
    match &clip.coverage {
        Some(coverage) if !clip.rect.is_empty() => canvas.clip_to_image(coverage, clip.rect),
        _ => canvas.clip_to_zero(),
    }
}

/// `PixelAdjust.blend(_:over:through:pixelToDocument:isMask:)` for color images:
/// `coverage × adjusted + (1 − coverage) × original`, in float without color conversion, so fully
/// selected pixels stay exact and soft edges blend. What Core Image's `CIBlendWithMask` did, on
/// the premultiplied values the buffers hold.
pub fn blend_through_selection(
    adjusted: &Rgba8Image,
    original: &Rgba8Image,
    selection: &SelectionClip,
    pixel_to_document: AffineTransform,
) -> Rgba8Image {
    let (width, height) = (adjusted.width(), adjusted.height());
    let mask = coverage(selection, width, height, pixel_to_document);
    let source = original;
    let mut result = adjusted.clone();
    for (index, pixel) in result.data_mut().chunks_exact_mut(4).enumerate() {
        let coverage = mask.data()[index];
        if coverage == 255 {
            continue;
        }
        let (x, y) = (index % width, index / width);
        if coverage == 0 {
            if source.contains(x as i64, y as i64) {
                pixel.copy_from_slice(&source.get(x, y));
            }
            continue;
        }
        let background = if source.contains(x as i64, y as i64) {
            source.get(x, y)
        } else {
            [0, 0, 0, 0]
        };
        let coverage = coverage as f64 / 255.0;
        for channel in 0..4 {
            let value =
                coverage * pixel[channel] as f64 + (1.0 - coverage) * background[channel] as f64;
            pixel[channel] = value.round().clamp(0.0, 255.0) as u8;
        }
    }
    result
}

/// [`blend_through_selection`] for a mask: the same blend on gray bytes, which is what the mask
/// filters' `isMask: true` renders were.
pub fn blend_gray_through_selection(
    adjusted: &Gray8Image,
    original: &Gray8Image,
    selection: &SelectionClip,
    pixel_to_document: AffineTransform,
) -> Gray8Image {
    let (width, height) = (adjusted.width(), adjusted.height());
    let mask = coverage(selection, width, height, pixel_to_document);
    let mut result = adjusted.clone();
    for (index, value) in result.data_mut().iter_mut().enumerate() {
        let coverage = mask.data()[index];
        if coverage == 255 {
            continue;
        }
        let background = original.data().get(index).copied().unwrap_or(0);
        if coverage == 0 {
            *value = background;
            continue;
        }
        let coverage = coverage as f64 / 255.0;
        let blended = coverage * *value as f64 + (1.0 - coverage) * background as f64;
        *value = blended.round().clamp(0.0, 255.0) as u8;
    }
    result
}

/// Runs `body` over the pixel bands [`Raster::in_bands`] cuts — equal bands of `ceil(count /
/// bands)`, handed out as whole-pixel slices so the bands can run at once. The Swift ran
/// `levels_apply`/`cube_apply` per band on the dispatch pool; the bands touch disjoint pixels, so
/// which one runs where cannot change a byte.
fn for_each_pixel_band<F>(pixels: &mut [u8], count: usize, body: F)
where
    F: Fn(&mut [u8]) + Send + Sync,
{
    if count == 0 {
        return;
    }
    let bands = if count < 250_000 {
        1
    } else {
        rayon::current_num_threads().max(1) * 2
    };
    let size = count.div_ceil(bands);
    pixels.par_chunks_mut(size * 4).for_each(|band| body(band));
}

/// A band's pixel count (`Raster::in_bands` hands out `(start, length)`, the kernels take a count).
fn band_len(band: &[u8]) -> usize {
    band.len() / 4
}

/// `ImageAdjustmentPixels`: draws an image into an RGBA buffer (premultiplied, alpha last), lets a
/// C kernel change it in place, and returns the result.
pub struct ImageAdjustmentPixels;

impl ImageAdjustmentPixels {
    /// `ImageAdjustmentPixels.run(_:_:)`: an RGBA copy of `image` — the Swift's
    /// `BrushRaster.draw` with `.copy`, which replaces what was there and samples the nearest
    /// pixel — the kernel `body` changing it in place, and the result.
    pub fn run(image: &Rgba8Image, body: impl FnOnce(&mut Rgba8Image)) -> Rgba8Image {
        let (width, height) = (image.width(), image.height());
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::None);
        canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
        let mut result = canvas.into_rgba();
        body(&mut result);
        result
    }

    /// `ImageAdjustmentPixels.clamp(_:_:_:)`: `value` inside `range`, or `fallback` when it is not
    /// a number.
    pub fn clamp(value: f64, range: (f64, f64), fallback: f64) -> f64 {
        if value.is_finite() {
            value.min(range.1).max(range.0)
        } else {
            fallback
        }
    }

    /// Exposure's ranges (`ExposureSettings.exposureRange` and friends).
    pub const EXPOSURE_RANGE: (f64, f64) = (-20.0, 20.0);
    pub const OFFSET_RANGE: (f64, f64) = (-0.5, 0.5);
    pub const GAMMA_RANGE: (f64, f64) = (0.01, 9.99);

    /// `ExposureSettings.isValid`.
    pub fn exposure_is_valid(settings: &ExposureSettings) -> bool {
        in_range(settings.exposure, Self::EXPOSURE_RANGE)
            && in_range(settings.offset, Self::OFFSET_RANGE)
            && in_range(settings.gamma, Self::GAMMA_RANGE)
    }

    /// `ExposureSettings.table`: each channel's output (0–1) for each input byte, decoded to
    /// linear light and encoded back.
    pub fn exposure_table(settings: &ExposureSettings) -> Vec<f32> {
        let scale = 2.0f64.powf(settings.exposure);
        (0..256)
            .map(|index| {
                let encoded = index as f64 / 255.0;
                let mut linear = if encoded <= 0.04045 {
                    encoded / 12.92
                } else {
                    ((encoded + 0.055) / 1.055).powf(2.4)
                };
                linear = (linear * scale + settings.offset).max(0.0).powf(1.0 / settings.gamma);
                let output = if linear <= 0.0031308 {
                    linear * 12.92
                } else {
                    1.055 * linear.powf(1.0 / 2.4) - 0.055
                };
                output.min(1.0).max(0.0) as f32
            })
            .collect()
    }

    /// `ExposureSettings.apply(_:)`: `exposure` stops of light and `offset` on the linear values,
    /// then gamma correction, the same curve on every channel; alpha is kept.
    pub fn exposure(image: &Rgba8Image, settings: &ExposureSettings) -> Result<Rgba8Image> {
        if !Self::exposure_is_valid(settings) {
            return Err(invalid_settings());
        }
        let table = Self::exposure_table(settings);
        let mut tables = Vec::with_capacity(256 * 3);
        for _ in 0..3 {
            tables.extend_from_slice(&table);
        }
        Ok(Self::run(image, |pixels| {
            let count = pixels.pixel_count();
            levels_apply(pixels.data_mut(), count, &tables);
        }))
    }

    /// `GradientMapSettings.isValid`.
    pub fn gradient_map_is_valid(settings: &GradientMapSettings) -> bool {
        color_is_valid(&settings.shadows) && color_is_valid(&settings.highlights)
    }

    /// `GradientMapSettings.apply(_:)`: each pixel's brightness picks a color between `shadows`
    /// and `highlights` (the other way round when reversed); alpha is kept.
    pub fn gradient_map(image: &Rgba8Image, settings: &GradientMapSettings) -> Result<Rgba8Image> {
        if !Self::gradient_map_is_valid(settings) {
            return Err(invalid_settings());
        }
        let table = gradient_map_table(settings);
        Ok(Self::run(image, |pixels| {
            let (width, height, stride) = (pixels.width(), pixels.height(), pixels.stride());
            crate::adjust_pixels::adjust_gradient_map(
                pixels.data_mut(),
                width,
                height,
                stride,
                &table,
            );
        }))
    }

    /// `BlackWhiteSettings.range`.
    pub const BLACK_WHITE_RANGE: (f64, f64) = (-200.0, 300.0);

    /// `BlackWhiteSettings.isValid`.
    pub fn black_white_is_valid(settings: &BlackWhiteSettings) -> bool {
        [
            settings.reds,
            settings.yellows,
            settings.greens,
            settings.cyans,
            settings.blues,
            settings.magentas,
        ]
        .iter()
        .all(|weight| in_range(*weight, Self::BLACK_WHITE_RANGE))
            && in_range(settings.tint_hue, (0.0, 360.0))
            && in_range(settings.tint_saturation, (0.0, 100.0))
    }

    /// `BlackWhiteSettings.apply(_:)`: how bright each family of colors becomes in gray, with an
    /// optional tint that keeps the result's tone.
    pub fn black_white(image: &Rgba8Image, settings: &BlackWhiteSettings) -> Result<Rgba8Image> {
        if !Self::black_white_is_valid(settings) {
            return Err(invalid_settings());
        }
        // The C routine's order: red, yellow, green, cyan, blue, magenta.
        let weights = [
            settings.reds,
            settings.yellows,
            settings.greens,
            settings.cyans,
            settings.blues,
            settings.magentas,
        ]
        .map(|weight| (weight / 100.0) as f32);
        Ok(Self::run(image, |pixels| {
            let (width, height, stride) = (pixels.width(), pixels.height(), pixels.stride());
            crate::adjust_pixels::adjust_black_white(
                pixels.data_mut(),
                width,
                height,
                stride,
                &weights,
                if settings.tint { 1 } else { 0 },
                settings.tint_hue,
                settings.tint_saturation / 100.0,
            );
        }))
    }

    /// `ColorBalanceSettings.range`.
    pub const COLOR_BALANCE_RANGE: (f64, f64) = (-100.0, 100.0);

    /// `ColorBalanceSettings.isValid`.
    pub fn color_balance_is_valid(settings: &ColorBalanceSettings) -> bool {
        color_balance_amounts(settings)
            .iter()
            .all(|amount| in_range(*amount, Self::COLOR_BALANCE_RANGE))
    }

    /// `ColorBalanceSettings.isIdentity`.
    pub fn color_balance_is_identity(settings: &ColorBalanceSettings) -> bool {
        color_balance_amounts(settings).iter().all(|amount| *amount == 0.0)
    }

    /// `ColorBalanceSettings.apply(_:)`: each pixel shifts towards one end of each opposing pair
    /// by however much it belongs to the shadows, midtones and highlights; Preserve Luminosity
    /// puts its brightness back afterwards.
    pub fn color_balance(
        image: &Rgba8Image,
        settings: &ColorBalanceSettings,
    ) -> Result<Rgba8Image> {
        if !Self::color_balance_is_valid(settings) {
            return Err(invalid_settings());
        }
        if Self::color_balance_is_identity(settings) {
            return Ok(image.clone());
        }
        let shadows = [
            settings.shadow_cyan_red,
            settings.shadow_magenta_green,
            settings.shadow_yellow_blue,
        ]
        .map(|amount| (amount / 100.0) as f32);
        let midtones = [
            settings.mid_cyan_red,
            settings.mid_magenta_green,
            settings.mid_yellow_blue,
        ]
        .map(|amount| (amount / 100.0) as f32);
        let highlights = [
            settings.highlight_cyan_red,
            settings.highlight_magenta_green,
            settings.highlight_yellow_blue,
        ]
        .map(|amount| (amount / 100.0) as f32);
        Ok(Self::run(image, |pixels| {
            let (width, height, stride) = (pixels.width(), pixels.height(), pixels.stride());
            crate::adjust_pixels::adjust_color_balance(
                pixels.data_mut(),
                width,
                height,
                stride,
                &shadows,
                &midtones,
                &highlights,
                if settings.preserve_luminosity { 1 } else { 0 },
            );
        }))
    }

    /// Grain's ranges (`GrainSettings.amountRange` and friends).
    pub const GRAIN_AMOUNT_RANGE: (f64, f64) = (0.0, 100.0);
    pub const GRAIN_SIZE_RANGE: (f64, f64) = (0.5, 20.0);
    pub const GRAIN_ROUGHNESS_RANGE: (f64, f64) = (0.0, 100.0);

    /// `GrainSettings.isValid`.
    pub fn grain_is_valid(settings: &GrainSettings) -> bool {
        in_range(settings.amount, Self::GRAIN_AMOUNT_RANGE)
            && in_range(settings.size, Self::GRAIN_SIZE_RANGE)
            && in_range(settings.roughness, Self::GRAIN_ROUGHNESS_RANGE)
    }

    /// `GrainSettings.apply(_:origin:unitsPerPixel:seed:)`: brightness noise, strongest in the
    /// midtones, fixed in document space by its seed. `origin` and `unitsPerPixel` place the
    /// image's pixels in document space; `seed` replaces the stored pattern when given.
    pub fn grain(
        image: &Rgba8Image,
        settings: &GrainSettings,
        origin: Point,
        units_per_pixel: f64,
        seed: Option<u32>,
    ) -> Result<Rgba8Image> {
        if !Self::grain_is_valid(settings) || !units_per_pixel.is_finite() || !(units_per_pixel > 0.0)
        {
            return Err(invalid_settings());
        }
        if !(settings.amount > 0.0) {
            return Ok(image.clone());
        }
        let pattern = seed.unwrap_or(settings.seed);
        Ok(Self::run(image, |pixels| {
            let (width, height, stride) = (pixels.width(), pixels.height(), pixels.stride());
            crate::adjust_pixels::adjust_grain(
                pixels.data_mut(),
                width,
                height,
                stride,
                settings.amount,
                settings.size,
                settings.roughness,
                pattern,
                origin.x,
                origin.y,
                units_per_pixel,
            );
        }))
    }
}

/// A settings value inside its slider's range, a number (`ClosedRange.contains` on a NaN is false
/// in Swift too).
fn in_range(value: f64, range: (f64, f64)) -> bool {
    value.is_finite() && value >= range.0 && value <= range.1
}

/// `AdjustmentColor.isValid`: a number in 0…1 on every channel.
fn color_is_valid(color: &AdjustmentColor) -> bool {
    [color.red, color.green, color.blue]
        .iter()
        .all(|channel| in_range(*channel, (0.0, 1.0)))
}

/// `ColorBalanceSettings.all`: the nine amounts, in the order the settings declare them.
fn color_balance_amounts(settings: &ColorBalanceSettings) -> [f64; 9] {
    [
        settings.shadow_cyan_red,
        settings.shadow_magenta_green,
        settings.shadow_yellow_blue,
        settings.mid_cyan_red,
        settings.mid_magenta_green,
        settings.mid_yellow_blue,
        settings.highlight_cyan_red,
        settings.highlight_magenta_green,
        settings.highlight_yellow_blue,
    ]
}

/// `ExposureSettings.apply(_:)` — the Filter menu's Exposure.
pub fn apply_exposure(image: &Rgba8Image, settings: &ExposureSettings) -> Result<Rgba8Image> {
    ImageAdjustmentPixels::exposure(image, settings)
}

/// `GradientMapSettings.apply(_:)` — the Filter menu's Gradient Map.
pub fn apply_gradient_map(image: &Rgba8Image, settings: &GradientMapSettings) -> Result<Rgba8Image> {
    ImageAdjustmentPixels::gradient_map(image, settings)
}

/// `BlackWhiteSettings.apply(_:)` — the Filter menu's Black & White.
pub fn apply_black_white(image: &Rgba8Image, settings: &BlackWhiteSettings) -> Result<Rgba8Image> {
    ImageAdjustmentPixels::black_white(image, settings)
}

/// `ColorBalanceSettings.apply(_:)` — the Filter menu's Color Balance.
pub fn apply_color_balance(
    image: &Rgba8Image,
    settings: &ColorBalanceSettings,
) -> Result<Rgba8Image> {
    ImageAdjustmentPixels::color_balance(image, settings)
}

/// `GrainSettings.apply(_:origin:unitsPerPixel:seed:)` — the Filter menu's Grain.
pub fn apply_grain(
    image: &Rgba8Image,
    settings: &GrainSettings,
    origin: Point,
    units_per_pixel: f64,
    seed: Option<u32>,
) -> Result<Rgba8Image> {
    ImageAdjustmentPixels::grain(image, settings, origin, units_per_pixel, seed)
}

/// `CurvesSettings.value(_:channel:)`: shape-preserving cubic Hermite interpolation avoids
/// overshoot between handles.
fn curve_value(settings: &CurvesSettings, x: f64, channel: usize) -> f64 {
    let points = &settings.channels[channel];
    let deltas: Vec<f64> = points
        .windows(2)
        .map(|pair| (pair[1].y - pair[0].y) / (pair[1].x - pair[0].x))
        .collect();

    /// The tangent at a handle: the neighbours' secant at the ends, the harmonic mean of the two
    /// when the curve keeps going the same way, and flat at a turning point.
    fn slope(deltas: &[f64], count: usize, index: usize) -> f64 {
        if index == 0 {
            return deltas[0];
        }
        if index == count - 1 {
            return deltas[deltas.len() - 1];
        }
        if deltas[index - 1] * deltas[index] <= 0.0 {
            return 0.0;
        }
        2.0 / (1.0 / deltas[index - 1] + 1.0 / deltas[index])
    }

    let last = points.iter().rposition(|point| point.x <= x).unwrap_or(0);
    let i = (points.len() - 2).min(last);
    let h = points[i + 1].x - points[i].x;
    let t = ((x - points[i].x) / h).min(1.0).max(0.0);
    let y = (2.0 * t * t * t - 3.0 * t * t + 1.0) * points[i].y
        + (t * t * t - 2.0 * t * t + t) * h * slope(&deltas, points.len(), i)
        + (-2.0 * t * t * t + 3.0 * t * t) * points[i + 1].y
        + (t * t * t - t * t) * h * slope(&deltas, points.len(), i + 1);
    y.min(255.0).max(0.0)
}

/// `CurvesSettings.apply(_:)`'s lookup: the red, green and blue tables, each the channel's own
/// curve followed by the composite RGB one, normalized for `levels_apply`.
fn curves_tables(settings: &CurvesSettings) -> Vec<f32> {
    (1..4)
        .flat_map(|channel| {
            (0..256).map(move |index| {
                (curve_value(settings, curve_value(settings, index as f64, channel), 0) / 255.0)
                    as f32
            })
        })
        .collect()
}

/// `CurvesSettings.apply(_:)` — the Filter menu's Curves.
pub fn apply_curves(image: &Rgba8Image, settings: &CurvesSettings) -> Result<Rgba8Image> {
    if !settings.is_valid() {
        return Err(invalid_settings());
    }
    let tables = curves_tables(settings);
    Ok(ImageAdjustmentPixels::run(image, |pixels| {
        let count = pixels.pixel_count();
        levels_apply(pixels.data_mut(), count, &tables);
    }))
}

/// `LevelRange.apply(_:)`: the value's normalized position between the black and white points,
/// bent by the gamma, placed between the output points.
fn level_range_apply(range: &LevelRange, value: f64) -> f64 {
    let range = range.normalized();
    let input = ((value * 255.0 - range.black) / (range.white - range.black))
        .min(1.0)
        .max(0.0);
    (range.output_black + input.powf(1.0 / range.gamma) * (range.output_white - range.output_black))
        / 255.0
}

/// `LevelsSettings.apply(_:channel:)`: the channel's own range, then the composite RGB one.
/// `channel` is `LevelsChannel`'s index (red 1, green 2, blue 3, composite 0).
fn levels_value(settings: &LevelsSettings, value: f64, channel: usize) -> f64 {
    level_range_apply(
        &settings.ranges[0],
        level_range_apply(&settings.ranges[channel], value),
    )
}

/// `LevelsSettings.isIdentity`: every range is its own default.
fn levels_is_identity(settings: &LevelsSettings) -> bool {
    settings.ranges.iter().all(|range| {
        let range = range.normalized();
        range.black == 0.0
            && range.gamma == 1.0
            && range.white == 255.0
            && range.output_black == 0.0
            && range.output_white == 255.0
    })
}

/// `LevelsFilter.run`'s lookup: the red, green and blue tables, 256 entries each, as
/// `levels_apply` reads them.
fn levels_tables(settings: &LevelsSettings) -> Vec<f32> {
    (1..4)
        .flat_map(|channel| {
            (0..256).map(move |index| levels_value(settings, index as f64 / 255.0, channel) as f32)
        })
        .collect()
}

/// `LevelsSettings.apply` on a whole image: the three channel tables, and through the selection
/// when there is one. Identity settings return the image untouched.
///
/// The tables are for colors, not colors already multiplied by their alpha, and `levels_apply`
/// already divides each channel by its alpha before the lookup and multiplies it back after.
/// Doing it here as well ran a soft edge through the conversion twice: 50% gray at half alpha came
/// out at a quarter. A Levels layer runs on the whole canvas view every frame, so the pixels are
/// split across the cores.
pub fn apply_levels(
    image: &Rgba8Image,
    settings: &LevelsSettings,
    selection: Option<&SelectionClip>,
    mapping: AffineTransform,
) -> Rgba8Image {
    if levels_is_identity(settings) {
        return image.clone();
    }
    let tables = levels_tables(settings);
    let mut result = image.clone();
    let count = result.pixel_count();
    for_each_pixel_band(result.data_mut(), count, |band| {
        levels_apply(band, band_len(band), &tables);
    });
    match selection {
        Some(selection) => blend_through_selection(&result, image, selection, mapping),
        None => result,
    }
}

/// `LevelsJob`: the image, the settings, the selection they are confined to and the transform
/// that puts the image's pixels in the document.
pub struct LevelsJob {
    pub image: SharedImage,
    pub settings: LevelsSettings,
    pub selection: Option<SelectionClip>,
    pub mapping: AffineTransform,
}

/// `LevelsFilter`.
pub struct LevelsFilter;

impl LevelsFilter {
    /// `LevelsFilter.run(_:)`: the image through [`apply_levels`], which is the same work the
    /// identity check short-circuits.
    pub fn run(job: &LevelsJob) -> Result<Rgba8Image> {
        Ok(apply_levels(
            &job.image,
            &job.settings,
            job.selection.as_ref(),
            job.mapping,
        ))
    }

    /// `LevelsFilter.histogram(_:)`: 1024 bins, the composite RGB first. RGB is the mean of the
    /// three channel histograms, not a luminance histogram.
    pub fn histogram(job: &LevelsJob) -> Result<[Vec<f64>; 4]> {
        let width = job.image.width();
        let height = job.image.height();
        let mask = job
            .selection
            .as_ref()
            .map(|selection| coverage(selection, width, height, job.mapping));
        let mut bins = [0.0f64; 1024];
        levels_histogram(
            job.image.data(),
            mask.as_ref().map(|mask| mask.data()),
            width * height,
            &mut bins,
        );
        Ok(std::array::from_fn(|index| {
            bins[index * 256..(index + 1) * 256].to_vec()
        }))
    }
}

/// `LevelsHistogramDisplay`: display-only vertical scaling. Keeps linear bin ratios, but caps
/// isolated spikes so large solid backgrounds cannot flatten the useful tonal distribution.
pub struct LevelsHistogramDisplay;

impl LevelsHistogramDisplay {
    /// `LevelsHistogramDisplay.scale(for:)`.
    pub fn scale(bins: &[f64]) -> f64 {
        let peak = bins
            .iter()
            .filter(|bin| bin.is_finite() && **bin > 0.0)
            .fold(0.0f64, |peak, bin| peak.max(*bin));
        if !(peak > 0.0) {
            return 0.0;
        }
        let interior = if bins.len() >= 2 {
            &bins[1..bins.len() - 1]
        } else {
            &[][..]
        };
        let mut interior: Vec<f64> = interior
            .iter()
            .copied()
            .filter(|bin| bin.is_finite() && *bin > 0.0)
            .collect();
        if interior.is_empty() {
            return peak;
        }
        interior.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let typical_peak = interior[((interior.len() - 1) as f64 * 0.95) as usize];
        peak.min(typical_peak * 4.0)
    }
}

/// `LevelsAuto`: the panel's automatic calibrations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LevelsAuto {
    /// A shared interval preserves channel relationships.
    Contrast,
    /// Each channel's own endpoints.
    Color,
    /// Each channel's own endpoints, with the midtones neutralized.
    Neutral,
}

impl LevelsAuto {
    /// `LevelsAuto.allCases`.
    pub const ALL: [LevelsAuto; 3] = [Self::Contrast, Self::Color, Self::Neutral];

    /// `LevelsAuto.rawValue`: the panel's button label.
    pub fn raw_value(self) -> &'static str {
        match self {
            Self::Contrast => "Contrast",
            Self::Color => "Color",
            Self::Neutral => "Color + neutral midtones",
        }
    }

    /// `LevelsAuto.settings(histogram:)`: the histogram's first 0.1% at each end sets the black and
    /// white points.
    pub fn settings(self, histogram: &[Vec<f64>; 4]) -> LevelsSettings {
        /// The black and white points of one channel: the first and last bins whose cumulative
        /// share passes 0.1%, or nothing when the channel is empty or flat.
        fn endpoints(bins: &[f64]) -> Option<(f64, f64)> {
            let total: f64 = bins.iter().sum();
            if !(total > 0.0) {
                return None;
            }
            let mut sum = 0.0;
            let mut low = 0usize;
            let mut high = 255usize;
            for i in 0..256 {
                sum += bins[i];
                if sum > total * 0.001 {
                    low = i;
                    break;
                }
            }
            sum = 0.0;
            for i in (0..256).rev() {
                sum += bins[i];
                if sum > total * 0.001 {
                    high = i;
                    break;
                }
            }
            if low < high {
                Some((low as f64, high as f64))
            } else {
                None
            }
        }

        let mut result = LevelsSettings::default();
        if self == LevelsAuto::Contrast {
            let limits: Vec<(f64, f64)> = histogram[1..]
                .iter()
                .filter_map(|bins| endpoints(bins))
                .collect();
            if !limits.is_empty() {
                let low = limits.iter().map(|limit| limit.0).fold(f64::INFINITY, f64::min);
                let high = limits
                    .iter()
                    .map(|limit| limit.1)
                    .fold(f64::NEG_INFINITY, f64::max);
                if low < high {
                    result.ranges[0] = LevelRange {
                        black: low,
                        gamma: 1.0,
                        white: high,
                        output_black: 0.0,
                        output_white: 255.0,
                    };
                }
            }
        } else {
            for channel in 1..=3 {
                let Some((low, high)) = endpoints(&histogram[channel]) else {
                    continue;
                };
                let mut range = LevelRange {
                    black: low,
                    gamma: 1.0,
                    white: high,
                    output_black: 0.0,
                    output_white: 255.0,
                };
                if self == LevelsAuto::Neutral {
                    let total: f64 = histogram[channel].iter().sum();
                    let mut mean = 0.0;
                    for (index, weight) in histogram[channel].iter().enumerate() {
                        mean += level_range_apply(&range, index as f64 / 255.0) * weight;
                    }
                    mean /= total;
                    if mean > 0.0 && mean < 1.0 {
                        range.gamma = (mean.ln() / 0.5f64.ln()).min(9.99).max(0.1);
                    }
                }
                result.ranges[channel] = range;
            }
        }
        result
    }
}

/// `LevelsSample`: which of the three points an eyedropper sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LevelsSample {
    Black,
    Gray,
    White,
}

impl LevelsSample {
    /// `LevelsSample.allCases`.
    pub const ALL: [LevelsSample; 3] = [Self::Black, Self::Gray, Self::White];

    /// `LevelsSample.rawValue`: the eyedropper's label.
    pub fn raw_value(self) -> &'static str {
        match self {
            Self::Black => "Black",
            Self::Gray => "Gray",
            Self::White => "White",
        }
    }
}

/// `LevelsSettings.sampling(_:mode:)`: samples are unpremultiplied original RGB, 0–1 per channel.
/// All three channels are calibrated together, and the composite range is cleared. A free function
/// because the settings type belongs to `compositor_rs_core`.
pub fn levels_sampling(settings: &LevelsSettings, rgb: [f64; 3], mode: LevelsSample) -> LevelsSettings {
    let mut result = settings.clone();
    result.ranges[0] = LevelRange {
        black: 0.0,
        gamma: 1.0,
        white: 255.0,
        output_black: 0.0,
        output_white: 255.0,
    };
    for channel in 1..=3 {
        let mut range = result.ranges[channel].clone();
        let value = rgb[channel - 1] * 255.0;
        match mode {
            LevelsSample::Black => range.black = (range.white - 1.0).min(value.max(0.0)),
            LevelsSample::White => range.white = (range.black + 1.0).max(value.min(255.0)),
            LevelsSample::Gray => {
                let fraction = (value - range.black) / (range.white - range.black);
                // A sample already at either end has no midpoint to set.
                if !(fraction > 0.0 && fraction < 1.0) {
                    continue;
                }
                range.gamma = fraction.ln() / 0.5f64.ln();
            }
        }
        range.output_black = 0.0;
        range.output_white = 255.0;
        result.ranges[channel] = range.normalized();
    }
    result
}

/// `ColorRange`'s ranges in the order the panels and Photoshop list them: Master, then the six
/// color ranges. The Swift iterated a `Dictionary` when it summed a hue's response; walking a fixed
/// order makes that sum the same on every run.
const HUE_RANGES: [ColorRange; 7] = [
    ColorRange::Master,
    ColorRange::Reds,
    ColorRange::Yellows,
    ColorRange::Greens,
    ColorRange::Cyans,
    ColorRange::Blues,
    ColorRange::Magentas,
];

/// `ColorRange.defaultBand`: Photoshop's starting hue band — falloff start, range start, range end,
/// falloff end. The settings type belongs to `compositor_rs_core`, so the table lives here with the
/// filter that reads it.
pub fn default_hue_band(range: &ColorRange) -> HueBand {
    let band = |falloff_start, range_start, range_end, falloff_end| HueBand {
        falloff_start,
        range_start,
        range_end,
        falloff_end,
    };
    match range {
        ColorRange::Master => band(0.0, 0.0, 360.0, 360.0),
        ColorRange::Reds => band(315.0, 345.0, 15.0, 45.0),
        ColorRange::Yellows => band(15.0, 45.0, 75.0, 105.0),
        ColorRange::Greens => band(75.0, 105.0, 135.0, 165.0),
        ColorRange::Cyans => band(135.0, 165.0, 195.0, 225.0),
        ColorRange::Blues => band(195.0, 225.0, 255.0, 285.0),
        ColorRange::Magentas => band(255.0, 285.0, 315.0, 345.0),
    }
}

/// `HueBand.forward(_:_:)`: degrees from `from` forward to `to`, always 0…360.
fn degrees_forward(from: f64, to: f64) -> f64 {
    let delta = (to - from) % 360.0;
    if delta < 0.0 {
        delta + 360.0
    } else {
        delta
    }
}

/// `HueBand.weight(of:)`: how strongly the band claims a hue — 1 inside the range, ramping
/// linearly through each falloff shoulder, 0 outside. Wraparound is handled by measuring forward.
pub fn hue_band_weight(band: &HueBand, hue: f64) -> f64 {
    let span = degrees_forward(band.falloff_start, band.falloff_end);
    if !(span > 0.0) {
        // Master covers everything.
        return 1.0;
    }
    let position = degrees_forward(band.falloff_start, hue);
    if position > span {
        return 0.0;
    }
    let ramp_in = degrees_forward(band.falloff_start, band.range_start);
    let plateau_end = degrees_forward(band.falloff_start, band.range_end);
    if position < ramp_in {
        return if ramp_in > 0.0 { position / ramp_in } else { 1.0 };
    }
    if position <= plateau_end {
        return 1.0;
    }
    let ramp_out = span - plateau_end;
    if ramp_out > 0.0 {
        (span - position) / ramp_out
    } else {
        1.0
    }
}

/// `HueSaturationSettings.weight(of:hue:)`: Master everywhere, the other ranges through their band,
/// flipped when the selected range's band is inverted.
fn hue_range_weight(settings: &HueSaturationSettings, range: &ColorRange, hue: f64) -> f64 {
    if *range == ColorRange::Master {
        return 1.0;
    }
    let band = settings
        .bands
        .get(range)
        .cloned()
        .unwrap_or_else(|| default_hue_band(range));
    let weight = hue_band_weight(&band, hue);
    if settings.invert_range && *range == settings.range {
        1.0 - weight
    } else {
        weight
    }
}

/// `RangeAdjustment()`: a range that shifts nothing.
fn no_range_adjustment() -> RangeAdjustment {
    RangeAdjustment {
        hue: 0.0,
        saturation: 0.0,
        lightness: 0.0,
    }
}

/// `HueSaturationSettings.hue` and friends: the sliders read and write the selected range's
/// adjustment, which is a zero adjustment until one is set.
fn selected_adjustment(settings: &HueSaturationSettings) -> RangeAdjustment {
    settings
        .adjustments
        .get(&settings.range)
        .cloned()
        .unwrap_or_else(no_range_adjustment)
}

/// `HueSaturationFilter.HueResponse`: how much every range shifts a given hue.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HueResponse {
    pub shift: f64,
    pub saturation: f64,
    pub lightness: f64,
}

/// `HueSaturationFilter.hueResponse(_:)`: how much every range shifts a given hue, sampled once per
/// degree. Building this once per settings keeps the cube cheap: without it each of ~36k cube
/// entries would re-evaluate all seven ranges.
pub fn hue_response(settings: &HueSaturationSettings) -> Vec<HueResponse> {
    (0..=360)
        .map(|degree| {
            let mut response = HueResponse::default();
            for range in HUE_RANGES.iter() {
                let Some(adjustment) = settings.adjustments.get(range) else {
                    continue;
                };
                if *adjustment == no_range_adjustment() {
                    continue;
                }
                let weight = hue_range_weight(settings, range, degree as f64);
                if !(weight > 0.0) {
                    continue;
                }
                response.shift += adjustment.hue * weight;
                response.saturation += adjustment.saturation * weight;
                response.lightness += adjustment.lightness * weight;
            }
            response
        })
        .collect()
}

/// `HueSaturationFilter`'s cube applied to a whole image: the lookup for every pixel, and through
/// the selection when there is one.
///
/// On the CPU across the cores rather than Core Image: a Hue/Saturation layer runs on the whole
/// canvas view every frame, and the trip to the GPU and back cost more than the lookup. The lookup
/// unpremultiplies around itself.
pub fn apply_hue_saturation(
    image: &Rgba8Image,
    settings: &HueSaturationSettings,
    selection: Option<&SelectionClip>,
    mapping: AffineTransform,
) -> Rgba8Image {
    let cube = HueSaturationFilter::cube(settings);
    let mut result = image.clone();
    let count = result.pixel_count();
    for_each_pixel_band(result.data_mut(), count, |band| {
        cube_apply(
            band,
            band_len(band),
            &cube,
            HueSaturationFilter::DIMENSION as i32,
        );
    });
    match selection {
        Some(selection) => blend_through_selection(&result, image, selection, mapping),
        None => result,
    }
}

/// `HueSaturationJob`: the image, the settings, the selection they are confined to and the
/// transform that puts the image's pixels in the document.
pub struct HueSaturationJob {
    pub image: SharedImage,
    pub settings: HueSaturationSettings,
    pub selection: Option<SelectionClip>,
    pub pixel_to_document: AffineTransform,
    /// Previews skip the layer-panel thumbnail.
    pub thumbnail: bool,
}

/// `AdjustedPixels`: the adjusted image and, unless the job skipped it, its thumbnail.
pub struct AdjustedPixels {
    pub image: Rgba8Image,
    pub thumbnail: Option<Rgba8Image>,
}

/// `HueSaturationFilter`: a color cube built from the settings, applied in one lookup. Working
/// through a cube keeps slider dragging fast on large images; identity settings never reach here.
pub struct HueSaturationFilter;

/// The last few cubes built: a Hue/Saturation layer redraws with the same settings on every canvas
/// frame, and building one takes longer than applying it.
static CUBES: LazyLock<Mutex<Vec<(HueSaturationSettings, Arc<Vec<f32>>)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

impl HueSaturationFilter {
    /// 33 points per axis, the usual size for this kind of lookup: fast to build, smooth enough.
    pub const DIMENSION: usize = 33;

    /// `HueSaturationFilter.run(_:)`: the cube over the image's pixels, the selection blended back
    /// afterwards, and the layer-panel thumbnail unless the job skipped it.
    pub fn run(job: &HueSaturationJob) -> Result<AdjustedPixels> {
        let image = apply_hue_saturation(
            &job.image,
            &job.settings,
            job.selection.as_ref(),
            job.pixel_to_document,
        );
        Ok(AdjustedPixels {
            thumbnail: job.thumbnail.then(|| PixelAdjust::thumbnail(&image)),
            image,
        })
    }

    /// `HueSaturationFilter.cube(_:)`: the cached lookup table, or a fresh one.
    pub fn cube(settings: &HueSaturationSettings) -> Arc<Vec<f32>> {
        {
            let cubes = CUBES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(data) = cubes
                .iter()
                .find(|(cached, _)| cached == settings)
                .map(|(_, data)| Arc::clone(data))
            {
                return data;
            }
        }
        let data = Arc::new(Self::build_cube(settings));
        let mut cubes = CUBES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cubes.retain(|(cached, _)| cached != settings);
        cubes.insert(0, (settings.clone(), Arc::clone(&data)));
        if cubes.len() > 8 {
            cubes.pop();
        }
        data
    }

    /// `HueSaturationFilter.buildCube(_:)`: every cube corner converted to HSL, adjusted, and back.
    /// Red varies fastest, the order `cube_apply` reads.
    fn build_cube(settings: &HueSaturationSettings) -> Vec<f32> {
        let response = hue_response(settings);
        let step = (Self::DIMENSION - 1) as f64;
        let mut values = vec![0.0f32; Self::DIMENSION * Self::DIMENSION * Self::DIMENSION * 4];
        let mut index = 0;
        for blue in 0..Self::DIMENSION {
            for green in 0..Self::DIMENSION {
                for red in 0..Self::DIMENSION {
                    let color = Self::adjust(
                        red as f64 / step,
                        green as f64 / step,
                        blue as f64 / step,
                        settings,
                        &response,
                    );
                    values[index] = color.0 as f32;
                    values[index + 1] = color.1 as f32;
                    values[index + 2] = color.2 as f32;
                    values[index + 3] = 1.0;
                    index += 4;
                }
            }
        }
        values
    }

    /// `HueSaturationFilter.adjust(red:green:blue:settings:response:)`: one color through the
    /// ranges, or through Colorize's single hue.
    pub fn adjust(
        red: f64,
        green: f64,
        blue: f64,
        settings: &HueSaturationSettings,
        response: &[HueResponse],
    ) -> (f64, f64, f64) {
        let (mut hue, mut saturation, mut lightness) = to_hsl(red, green, blue);
        let lightness_amount;
        if settings.colorize {
            hue = selected_adjustment(settings).hue % 360.0;
            saturation = (selected_adjustment(settings).saturation / 100.0).min(1.0).max(0.0);
            lightness_amount = selected_adjustment(settings).lightness / 100.0;
        } else {
            // Every range contributes, weighted by how strongly it claims the original hue.
            let sampled = response[(response.len() - 1).min(hue.round().max(0.0) as usize)];
            lightness_amount = sampled.lightness / 100.0;
            hue = (hue + sampled.shift) % 360.0;
            if hue < 0.0 {
                hue += 360.0;
            }
            saturation = Self::adjusted_saturation(saturation, sampled.saturation);
        }
        // Lightness pulls toward white above 0 and toward black below, reaching either at ±100.
        let amount = lightness_amount.min(1.0).max(-1.0);
        lightness = if amount >= 0.0 {
            lightness + (1.0 - lightness) * amount
        } else {
            lightness * (1.0 + amount)
        };
        to_rgb(hue, saturation, lightness.min(1.0).max(0.0))
    }

    /// `HueSaturationFilter.adjustedSaturation(_:by:)`: Photoshop's Saturation — below 0 it scales
    /// toward gray (−100 is gray); above 0 it divides by what's left, so +50 doubles it and +100
    /// takes any color all the way. Multiplicative both ways, so neutral grays stay neutral.
    pub fn adjusted_saturation(saturation: f64, amount: f64) -> f64 {
        let amount = (amount / 100.0).min(1.0).max(-1.0);
        if !(amount > 0.0) {
            return (saturation * (1.0 + amount)).max(0.0);
        }
        if amount >= 1.0 {
            return if saturation > 0.0 { 1.0 } else { 0.0 };
        }
        (saturation / (1.0 - amount)).min(1.0)
    }

    /// `HueSaturationFilter.shiftedHue(_:settings:)`: the hue a spectrum swatch becomes, for the
    /// "after" bar.
    pub fn shifted_hue(hue: f64, settings: &HueSaturationSettings) -> f64 {
        let mut shift = 0.0;
        for range in HUE_RANGES.iter() {
            let Some(adjustment) = settings.adjustments.get(range) else {
                continue;
            };
            if adjustment.hue == 0.0 {
                continue;
            }
            shift += adjustment.hue * hue_range_weight(settings, range, hue);
        }
        let shifted = (hue + shift) % 360.0;
        if shifted < 0.0 {
            shifted + 360.0
        } else {
            shifted
        }
    }
}

/// `HueSaturationFilter.toHSL(red:green:blue:)`.
fn to_hsl(red: f64, green: f64, blue: f64) -> (f64, f64, f64) {
    let high = red.max(green).max(blue);
    let low = red.min(green).min(blue);
    let lightness = (high + low) / 2.0;
    let delta = high - low;
    if !(delta > 0.0) {
        return (0.0, 0.0, lightness);
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let mut hue = if high == red {
        (green - blue) / delta
    } else if high == green {
        (blue - red) / delta + 2.0
    } else {
        (red - green) / delta + 4.0
    };
    hue *= 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    (hue, saturation.min(1.0), lightness)
}

/// `HueSaturationFilter.toRGB(hue:saturation:lightness:)`.
fn to_rgb(hue: f64, saturation: f64, lightness: f64) -> (f64, f64, f64) {
    if !(saturation > 0.0) {
        return (lightness, lightness, lightness);
    }
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let base = lightness - chroma / 2.0;
    let (red, green, blue) = match sector as i32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    (
        (red + base).min(1.0).max(0.0),
        (green + base).min(1.0).max(0.0),
        (blue + base).min(1.0).max(0.0),
    )
}

/// `PixelInvert.run(_:)` for a color image: whole-image invert in one pass, optionally limited to
/// a selection. Inverting never changes a layer's size, so no tiles, bounds scans or re-cropping.
///
/// **Substitution.** The Swift used vImage's fixed-point 4×4 matrix (each color becomes alpha −
/// color, so transparency is kept, alpha itself is unchanged), which is exact byte arithmetic; this
/// is that arithmetic written out. `clamp` is vImage's own clamp to the byte range.
pub fn invert(
    image: &Rgba8Image,
    selection: Option<&SelectionClip>,
    mapping: AffineTransform,
) -> Rgba8Image {
    let mut result = image.clone();
    for pixel in result.data_mut().chunks_exact_mut(4) {
        let alpha = pixel[3] as i32;
        for channel in 0..3 {
            pixel[channel] = (alpha - pixel[channel] as i32).clamp(0, 255) as u8;
        }
    }
    match selection {
        Some(selection) => blend_through_selection(&result, image, selection, mapping),
        None => result,
    }
}

/// `PixelInvert`: whole-image invert over RGBA and gray, optionally limited to a selection.
pub struct PixelInvert;

/// `PixelInvert.Job`: the image, whether it is a mask, and the selection. The Swift's `isMask` flag
/// is the image's own kind here ([`PixelImage`]).
pub struct PixelInvertJob {
    pub image: PixelImage,
    pub pixel_to_document: AffineTransform,
    pub selection: Option<SelectionClip>,
}

impl PixelInvert {
    /// `PixelInvert.run(_:)`.
    ///
    /// **Substitution.** A mask's `vImageTableLookUp_Planar8` with the table `255 − i` is the same
    /// arithmetic written out; the selection is blended back with the same coverage × inverted +
    /// (1 − coverage) × original the color path uses.
    pub fn run(job: &PixelInvertJob) -> Result<PixelImage> {
        match &job.image {
            PixelImage::Rgba(image) => Ok(PixelImage::Rgba(Arc::new(invert(
                image,
                job.selection.as_ref(),
                job.pixel_to_document,
            )))),
            PixelImage::Gray(mask) => {
                let mut result = mask.as_ref().clone();
                for value in result.data_mut() {
                    *value = 255 - *value;
                }
                match &job.selection {
                    Some(selection) => Ok(PixelImage::Gray(Arc::new(blend_gray_through_selection(
                        &result,
                        mask,
                        selection,
                        job.pixel_to_document,
                    )))),
                    None => Ok(PixelImage::Gray(Arc::new(result))),
                }
            }
        }
    }

    /// Kept for existing call sites; thumbnails live in [`PixelAdjust`].
    pub fn thumbnail(image: &Rgba8Image) -> Rgba8Image {
        PixelAdjust::thumbnail(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::layer_adjustment::CurvePoint;

    /// A `width × height` image from raw premultiplied bytes.
    fn rgba(width: usize, height: usize, data: &[u8]) -> Rgba8Image {
        Rgba8Image::from_data(width, height, data.to_vec())
    }

    /// A pixel's straight (unpremultiplied) color and alpha, 0–255, as the Swift's `pixels(_:)`
    /// reads them.
    fn straight_pixel(image: &Rgba8Image, x: usize, y: usize) -> [f64; 4] {
        let pixel = image.get(x, y);
        let channel = |value: u8| {
            if pixel[3] == 0 {
                0.0
            } else {
                (value as f64 * 255.0 / pixel[3] as f64).min(255.0)
            }
        };
        [
            channel(pixel[0]),
            channel(pixel[1]),
            channel(pixel[2]),
            pixel[3] as f64,
        ]
    }

    /// The gray ramp the exposure and curves no-op checks run over: every byte, opaque.
    fn gray_ramp() -> Rgba8Image {
        let data: Vec<u8> = (0..256u32)
            .flat_map(|value| [value as u8, value as u8, value as u8, 255])
            .collect();
        Rgba8Image::from_data(256, 1, data)
    }

    /// `LevelsTests.histogramDisplayKeepsDistributionVisibleBesideClippingSpikes`.
    #[test]
    fn levels_histogram_display_keeps_distribution_visible_beside_clipping_spikes() {
        let mut bins = vec![100.0f64; 256];
        bins[255] = 100_000.0;
        assert_eq!(LevelsHistogramDisplay::scale(&bins), 400.0);
        bins[0] = 200_000.0;
        assert_eq!(LevelsHistogramDisplay::scale(&bins), 400.0);
        // An isolated spike away from the endpoints should not flatten the graph either.
        bins[128] = 500_000.0;
        assert_eq!(LevelsHistogramDisplay::scale(&bins), 400.0);
        assert_eq!(bins[128], 500_000.0); // Counts are never modified.
        assert_eq!(LevelsHistogramDisplay::scale(&vec![100.0; 256]), 100.0);

        let mut sparse = vec![0.0f64; 256];
        assert_eq!(LevelsHistogramDisplay::scale(&sparse), 0.0);
        sparse[255] = 50.0;
        assert_eq!(LevelsHistogramDisplay::scale(&sparse), 50.0);
        sparse[0] = 100.0;
        assert_eq!(LevelsHistogramDisplay::scale(&sparse), 100.0);
        sparse[128] = 200.0;
        assert_eq!(LevelsHistogramDisplay::scale(&sparse), 200.0);
    }

    /// `LevelsTests.autoAlgorithmsAndEyedropperCalibration`.
    #[test]
    fn levels_auto_algorithms_and_eyedropper_calibration() {
        let mut bins: [Vec<f64>; 4] = std::array::from_fn(|_| vec![0.0; 256]);
        for channel in 1..4 {
            bins[channel][20 * channel] = 100.0;
            bins[channel][200 + channel * 10] = 100.0;
        }
        let linked = LevelsAuto::Contrast.settings(&bins);
        assert_eq!(linked.ranges[0].black, 20.0);
        assert_eq!(linked.ranges[0].white, 230.0);

        let color = LevelsAuto::Color.settings(&bins);
        assert_eq!(color.ranges[1].black, 20.0);
        assert_eq!(color.ranges[3].black, 60.0);
        // The composite range is left at its default.
        assert_eq!(color.ranges[0].black, 0.0);
        assert_eq!(color.ranges[0].white, 255.0);
        assert_eq!(color.ranges[0].gamma, 1.0);
        // The two ends balance out, so the midtones need no gamma.
        assert_eq!(LevelsAuto::Neutral.settings(&bins).ranges[1].gamma, 1.0);

        let empty: [Vec<f64>; 4] = std::array::from_fn(|_| vec![0.0; 256]);
        for mode in LevelsAuto::ALL {
            assert!(levels_is_identity(&mode.settings(&empty)));
        }

        // Each eyedropper takes the sample to its own end of every channel, together.
        let rgb = [0.25, 0.4, 0.6];
        for mode in LevelsSample::ALL {
            let settings = levels_sampling(&LevelsSettings::default(), rgb, mode);
            let target = match mode {
                LevelsSample::Black => 0.0,
                LevelsSample::White => 1.0,
                LevelsSample::Gray => 0.5,
            };
            for (index, channel) in [1usize, 2, 3].iter().enumerate() {
                let applied = levels_value(&settings, rgb[index], *channel);
                assert!(
                    (applied - target).abs() < 0.0001,
                    "{mode:?} channel {channel}: {applied} instead of {target}"
                );
            }
        }
    }

    /// `LevelsTests.channelsCoexistAndUseDocumentedOrder`.
    #[test]
    fn levels_channels_coexist_and_use_documented_order() {
        let mut settings = LevelsSettings::default();
        settings.ranges[1] = LevelRange {
            black: 0.0,
            gamma: 2.0,
            white: 255.0,
            output_black: 0.0,
            output_white: 255.0,
        };
        settings.ranges[0] = LevelRange {
            black: 40.0,
            gamma: 1.0,
            white: 210.0,
            output_black: 0.0,
            output_white: 255.0,
        };
        let expected = level_range_apply(
            &settings.ranges[0],
            level_range_apply(&settings.ranges[1], 64.0 / 255.0),
        );

        let image = rgba(1, 1, &[64, 64, 64, 255]);
        let result = apply_levels(&image, &settings, None, AffineTransform::IDENTITY);
        let pixel = result.get(0, 0);
        assert!((pixel[0] as f64 - expected * 255.0).abs() <= 1.0, "{pixel:?}");
        assert_eq!(pixel[1], pixel[2]);
        assert!(pixel[0] > pixel[1]);

        // Invalid ranges normalize: black below white, gamma and the outputs back in range.
        let invalid = LevelRange {
            black: 300.0,
            gamma: f64::NAN,
            white: -1.0,
            output_black: -100.0,
            output_white: 400.0,
        }
        .normalized();
        assert!(invalid.black < invalid.white);
        assert_eq!(invalid.gamma, 1.0);
        assert_eq!(invalid.output_black, 0.0);
        assert_eq!(invalid.output_white, 255.0);
    }

    /// `LevelsTests.inputClippingGammaOutputInversionAndAlpha`.
    #[test]
    fn levels_input_clipping_gamma_output_inversion_and_alpha() {
        let source = rgba(
            6,
            1,
            &[
                0, 0, 0, 255, 64, 64, 64, 255, 128, 128, 128, 255, 255, 255, 255, 255, 64, 32, 0,
                128, 0, 0, 0, 0,
            ],
        );
        let range = |black, gamma, white, output_black, output_white| LevelRange {
            black,
            gamma,
            white,
            output_black,
            output_white,
        };

        let mut settings = LevelsSettings::default();
        settings.ranges[0] = range(64.0, 1.0, 128.0, 0.0, 255.0);
        let clipped = apply_levels(&source, &settings, None, AffineTransform::IDENTITY);
        assert_eq!(
            &clipped.data()[0..12],
            &[0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255]
        );

        settings.ranges[0] = range(0.0, 2.0, 255.0, 0.0, 255.0);
        let brightened = apply_levels(&source, &settings, None, AffineTransform::IDENTITY);
        assert!((brightened.data()[4] as i32 - 128).abs() <= 1);
        assert!((brightened.data()[8] as i32 - 181).abs() <= 1);
        // Half alpha: the table is for colors, so it is looked up after unpremultiplying.
        assert!(brightened.data()[16] <= 128 && brightened.data()[17] <= 128);
        assert_eq!(brightened.data()[19], 128);
        // A fully transparent pixel is left alone.
        assert_eq!(brightened.data()[23], 0);

        settings.ranges[0] = range(0.0, 1.0, 255.0, 255.0, 0.0);
        let inverted = apply_levels(&source, &settings, None, AffineTransform::IDENTITY);
        assert_eq!(inverted.data()[0], 255);
        assert_eq!(inverted.data()[12], 0);
        assert_eq!(
            &inverted.data()[16..20],
            &[64, 96, 128, 128],
            "premultiplied [64,32,0,128] inverted"
        );

        // Identity settings are an exact no-op, byte for byte.
        let untouched = apply_levels(&source, &LevelsSettings::default(), None, AffineTransform::IDENTITY);
        assert_eq!(untouched.data(), source.data());
    }

    /// `LevelsTests.histogramExcludesTransparencyAndWeightsSelection`.
    #[test]
    fn levels_histogram_excludes_transparency_and_weights_the_selection() {
        let source = rgba(3, 1, &[255, 0, 0, 255, 0, 128, 0, 128, 0, 0, 0, 0]);
        let job = LevelsJob {
            image: Arc::new(source.clone()),
            settings: LevelsSettings::default(),
            selection: None,
            mapping: AffineTransform::IDENTITY,
        };
        let bins = LevelsFilter::histogram(&job).expect("a histogram");
        assert_eq!(bins[1][255], 1.0);
        assert!((bins[2][255] - 128.0 / 255.0).abs() < 0.00001);
        let total: f64 = bins[0].iter().sum();
        assert!((total - (1.0 + 128.0 / 255.0)).abs() < 0.00001);

        let clipped = LevelsJob {
            image: Arc::new(source),
            settings: LevelsSettings::default(),
            selection: Some(SelectionClip::new(
                Rect::new(0.0, 0.0, 1.0, 1.0),
                Some(Gray8Image::from_data(1, 1, vec![255])),
            )),
            mapping: AffineTransform::IDENTITY,
        };
        let bins = LevelsFilter::histogram(&clipped).expect("a histogram");
        assert_eq!(bins[1][255], 1.0);
        assert_eq!(bins[2][255], 0.0);

        let empty = LevelsJob {
            image: Arc::new(rgba(3, 1, &[255, 0, 0, 255, 0, 128, 0, 128, 0, 0, 0, 0])),
            settings: LevelsSettings::default(),
            selection: Some(SelectionClip::new(Rect::ZERO, None)),
            mapping: AffineTransform::IDENTITY,
        };
        let bins = LevelsFilter::histogram(&empty).expect("a histogram");
        assert!(bins.iter().flatten().all(|bin| *bin == 0.0));
    }

    /// `HueSaturationTests.bandWeightsRampThroughFalloffAndWrapAround`.
    #[test]
    fn hue_saturation_band_weights_ramp_through_falloff_and_wrap_around() {
        let reds = default_hue_band(&ColorRange::Reds); // 315 / 345 / 15 / 45, wrapping past 0.
        assert_eq!(hue_band_weight(&reds, 0.0), 1.0);
        assert_eq!(hue_band_weight(&reds, 345.0), 1.0);
        assert_eq!(hue_band_weight(&reds, 15.0), 1.0);
        assert!((hue_band_weight(&reds, 330.0) - 0.5).abs() < 0.001); // Up the shoulder.
        assert!((hue_band_weight(&reds, 30.0) - 0.5).abs() < 0.001); // Down the far one.
        assert_eq!(hue_band_weight(&reds, 315.0), 0.0);
        assert_eq!(hue_band_weight(&reds, 45.0), 0.0);
        assert_eq!(hue_band_weight(&reds, 180.0), 0.0);
        assert_eq!(
            hue_band_weight(&default_hue_band(&ColorRange::Master), 123.0),
            1.0
        );

        let greens = default_hue_band(&ColorRange::Greens);
        assert_eq!(greens.falloff_start, 75.0);
        assert_eq!(greens.range_start, 105.0);
        assert_eq!(greens.range_end, 135.0);
        assert_eq!(greens.falloff_end, 165.0);
    }

    /// `HueSaturationTests.positiveSaturationMatchesPhotoshop`.
    #[test]
    fn hue_saturation_positive_saturation_matches_photoshop() {
        assert!((HueSaturationFilter::adjusted_saturation(0.2, 50.0) - 0.4).abs() < 1e-9);
        assert!((HueSaturationFilter::adjusted_saturation(0.3, 62.0) - 0.3 / 0.38).abs() < 1e-9);
        assert_eq!(HueSaturationFilter::adjusted_saturation(0.1, 100.0), 1.0);
        assert_eq!(HueSaturationFilter::adjusted_saturation(0.8, 50.0), 1.0);
        assert_eq!(HueSaturationFilter::adjusted_saturation(0.0, 100.0), 0.0);
        assert!((HueSaturationFilter::adjusted_saturation(0.6, -50.0) - 0.3).abs() < 1e-9);
    }

    /// The Master range of the settings, the way the sliders fill it in.
    fn master_adjustment(hue: f64, saturation: f64, lightness: f64, colorize: bool) -> HueSaturationSettings {
        let mut settings = HueSaturationSettings::default();
        settings.adjustments.insert(
            ColorRange::Master,
            RangeAdjustment {
                hue,
                saturation,
                lightness,
            },
        );
        settings.colorize = colorize;
        settings
    }

    /// `HueSaturationTests.hueRotatesSaturationAndLightnessFollowPhotoshopRanges`, on the maths
    /// rather than through a document.
    #[test]
    fn hue_saturation_rotates_saturation_and_lightness_follow_the_photoshop_ranges() {
        let rotated = master_adjustment(120.0, 0.0, 0.0, false);
        let response = hue_response(&rotated);
        let red = HueSaturationFilter::adjust(1.0, 0.0, 0.0, &rotated, &response);
        assert!((red.0).abs() < 1e-9 && (red.1 - 1.0).abs() < 1e-9 && (red.2).abs() < 1e-9);
        // Gray has no hue to rotate.
        let gray = HueSaturationFilter::adjust(0.5, 0.5, 0.5, &rotated, &response);
        assert_eq!(gray, (0.5, 0.5, 0.5));

        let desaturated = master_adjustment(0.0, -100.0, 0.0, false);
        let response = hue_response(&desaturated);
        let gray = HueSaturationFilter::adjust(1.0, 0.0, 0.0, &desaturated, &response);
        assert!((gray.0 - gray.1).abs() < 1e-9 && (gray.1 - gray.2).abs() < 1e-9);

        let lightened = master_adjustment(0.0, 0.0, 100.0, false);
        let response = hue_response(&lightened);
        assert_eq!(
            HueSaturationFilter::adjust(1.0, 0.0, 0.0, &lightened, &response),
            (1.0, 1.0, 1.0)
        );
        let darkened = master_adjustment(0.0, 0.0, -100.0, false);
        let response = hue_response(&darkened);
        assert_eq!(
            HueSaturationFilter::adjust(1.0, 0.0, 0.0, &darkened, &response),
            (0.0, 0.0, 0.0)
        );
    }

    /// The default settings change nothing, on any color.
    #[test]
    fn hue_saturation_defaults_are_an_identity() {
        let settings = HueSaturationSettings::default();
        let response = hue_response(&settings);
        for color in [(1.0, 0.0, 0.0), (0.2, 0.5, 0.8), (0.0, 0.0, 0.0), (1.0, 1.0, 1.0)] {
            let adjusted = HueSaturationFilter::adjust(color.0, color.1, color.2, &settings, &response);
            assert!((adjusted.0 - color.0).abs() < 1e-9);
            assert!((adjusted.1 - color.1).abs() < 1e-9);
            assert!((adjusted.2 - color.2).abs() < 1e-9);
        }
    }

    /// `HueSaturationTests.slidersEditTheSelectedRangeAndTheAfterBarFollowsHueShifts`, and that a
    /// range leaves hues outside its band alone.
    #[test]
    fn hue_saturation_shifted_hue_follows_the_band() {
        let mut greens = HueSaturationSettings::default();
        greens.adjustments.insert(
            ColorRange::Greens,
            RangeAdjustment {
                hue: 60.0,
                saturation: 0.0,
                lightness: 0.0,
            },
        );
        greens.adjustments.insert(ColorRange::Master, no_range_adjustment());
        assert!((HueSaturationFilter::shifted_hue(120.0, &greens) - 180.0).abs() < 0.001);
        assert!((HueSaturationFilter::shifted_hue(0.0, &greens) - 0.0).abs() < 0.001);

        // The response samples each range by how much it claims the hue: reds cover 0, not 180.
        let mut reds = HueSaturationSettings::default();
        reds.adjustments.insert(
            ColorRange::Reds,
            RangeAdjustment {
                hue: 60.0,
                saturation: 0.0,
                lightness: 0.0,
            },
        );
        let response = hue_response(&reds);
        assert!((response[0].shift - 60.0).abs() < 1e-9);
        assert_eq!(response[180].shift, 0.0);
    }

    /// Invert is alpha − color on premultiplied values, so transparency is kept.
    #[test]
    fn pixel_invert_turns_each_channel_into_alpha_minus_color() {
        let image = rgba(2, 1, &[64, 32, 0, 128, 0, 0, 0, 0]);
        let inverted = invert(&image, None, AffineTransform::IDENTITY);
        assert_eq!(inverted.get(0, 0), [64, 96, 128, 128]);
        assert_eq!(inverted.get(1, 0), [0, 0, 0, 0]);

        let job = PixelInvertJob {
            image: PixelImage::Gray(Arc::new(Gray8Image::from_data(2, 1, vec![10, 250]))),
            pixel_to_document: AffineTransform::IDENTITY,
            selection: None,
        };
        let PixelImage::Gray(mask) = PixelInvert::run(&job).expect("inverts") else {
            panic!("a gray mask stays gray");
        };
        assert_eq!(mask.data(), &[245, 5]);

        // Through a selection only the covered pixels move.
        let selection = SelectionClip::new(
            Rect::new(0.0, 0.0, 1.0, 1.0),
            Some(Gray8Image::from_data(1, 1, vec![255])),
        );
        let image = rgba(2, 1, &[0, 0, 0, 255, 0, 0, 0, 255]);
        let inverted = invert(&image, Some(&selection), AffineTransform::IDENTITY);
        assert_eq!(inverted.get(0, 0), [255, 255, 255, 255]);
        assert_eq!(inverted.get(1, 0), [0, 0, 0, 255]);
    }

    /// `PixelAdjust.blend`: fully selected pixels stay exact, soft edges blend, the rest is the
    /// original.
    #[test]
    fn blend_through_selection_keeps_selected_pixels_exact() {
        let adjusted = Rgba8Image::opaque(4, 1, [255, 255, 255, 255]);
        let original = Rgba8Image::opaque(4, 1, [0, 0, 0, 255]);
        let selection = SelectionClip::new(
            Rect::new(0.0, 0.0, 4.0, 1.0),
            Some(Gray8Image::from_data(4, 1, vec![255, 128, 0, 255])),
        );
        let result = blend_through_selection(&adjusted, &original, &selection, AffineTransform::IDENTITY);
        assert_eq!(result.get(0, 0), [255, 255, 255, 255]);
        assert_eq!(result.get(1, 0)[0], 128);
        assert_eq!(result.get(2, 0), [0, 0, 0, 255]);
        assert_eq!(result.get(3, 0), [255, 255, 255, 255]);
    }

    /// `PixelAdjust.thumbnail(of:)`: 96 px on the long side, `Int` truncating and `max(1, …)`.
    #[test]
    fn thumbnails_are_no_larger_than_96_on_the_long_side() {
        let wide = Rgba8Image::opaque(192, 96, [10, 20, 30, 255]);
        let small = thumbnail(&Arc::new(wide));
        assert_eq!((small.width(), small.height()), (96, 48));
        assert_eq!(small.get(0, 0), [10, 20, 30, 255]);

        let square = Rgba8Image::opaque(96, 96, [1, 2, 3, 255]);
        let same = PixelAdjust::thumbnail(&square);
        assert_eq!((same.width(), same.height()), (96, 96));

        let single = Rgba8Image::opaque(1, 1, [4, 5, 6, 255]);
        let tiny = PixelAdjust::thumbnail(&single);
        assert_eq!((tiny.width(), tiny.height()), (1, 1));
        assert_eq!(tiny.get(0, 0), [4, 5, 6, 255]);
    }

    /// Exposure's table decodes to linear light and back, so a stop of light doubles it and the
    /// default settings leave an image alone.
    #[test]
    fn exposure_decodes_to_linear_light_and_back() {
        let flat = ExposureSettings::default();
        let table = ImageAdjustmentPixels::exposure_table(&flat);
        assert_eq!(table[0], 0.0);
        assert_eq!(table[255], 1.0);
        let brighter = ExposureSettings {
            exposure: 1.0,
            ..ExposureSettings::default()
        };
        let brighter = ImageAdjustmentPixels::exposure_table(&brighter);
        assert_eq!(brighter[255], 1.0);
        assert!(brighter[128] > table[128]);
        let lifted = ExposureSettings {
            gamma: 2.0,
            ..ExposureSettings::default()
        };
        assert!(ImageAdjustmentPixels::exposure_table(&lifted)[128] > table[128]);

        let ramp = gray_ramp();
        let result = apply_exposure(&ramp, &flat).expect("valid settings");
        for index in 0..result.data().len() {
            if index % 4 == 3 {
                assert_eq!(result.data()[index], 255);
                continue;
            }
            let expected = ramp.data()[index] as i32;
            assert!((result.data()[index] as i32 - expected).abs() <= 1, "byte {index}");
        }
    }

    /// Curve, Gradient Map and Color Balance at their defaults.
    #[test]
    fn default_color_adjustments_leave_a_gray_ramp_alone() {
        let ramp = gray_ramp();

        let curves = apply_curves(&ramp, &CurvesSettings::default()).expect("valid settings");
        for index in 0..curves.data().len() {
            if index % 4 == 3 {
                assert_eq!(curves.data()[index], 255);
                continue;
            }
            let expected = ramp.data()[index] as i32;
            assert!((curves.data()[index] as i32 - expected).abs() <= 1, "byte {index}");
        }

        // The default gradient map runs black to white, so a gray pixel keeps its tone.
        let gradient = apply_gradient_map(&ramp, &GradientMapSettings::default()).expect("valid settings");
        for index in 0..gradient.data().len() {
            let expected = ramp.data()[index] as i32;
            assert!((gradient.data()[index] as i32 - expected).abs() <= 1, "byte {index}");
        }

        let balance = apply_color_balance(&ramp, &ColorBalanceSettings::default()).expect("valid settings");
        assert_eq!(balance.data(), ramp.data());
    }

    /// A Gradient Map reversed paints the ends the other way round.
    #[test]
    fn gradient_map_reversed_swaps_the_ends() {
        let settings = GradientMapSettings {
            reversed: true,
            ..GradientMapSettings::default()
        };
        let image = rgba(2, 1, &[0, 0, 0, 255, 255, 255, 255, 255]);
        let result = apply_gradient_map(&image, &settings).expect("valid settings");
        assert_eq!(result.get(0, 0), [255, 255, 255, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 0, 255]);
    }

    /// Black & White at its defaults: pure red comes out 40% gray, as Photoshop's does.
    #[test]
    fn black_white_defaults_put_pure_red_at_forty_percent_gray() {
        let settings = BlackWhiteSettings::default();
        let image = rgba(3, 1, &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]);
        let result = apply_black_white(&image, &settings).expect("valid settings");
        let red = result.get(0, 0);
        assert_eq!(red[0], red[1]);
        assert_eq!(red[1], red[2]);
        assert!((red[0] as i32 - 102).abs() <= 2, "{red:?}");
        assert_eq!(red[3], 255);
        let green = result.get(1, 0);
        assert!((green[1] as i32 - 102).abs() <= 2, "{green:?}");
        // Blues at 20% are the darkest of the three.
        let blue = result.get(2, 0);
        assert!((blue[2] as i32 - 51).abs() <= 2, "{blue:?}");
        assert!(blue[2] < red[0]);
    }

    /// Grain is a no-op at zero amount, and its pattern is fixed by its seed.
    #[test]
    fn grain_is_fixed_by_its_seed_and_silent_at_zero_amount() {
        let image = Rgba8Image::opaque(64, 64, [128, 128, 128, 255]);
        let quiet = GrainSettings {
            amount: 0.0,
            ..GrainSettings::default()
        };
        let result = apply_grain(&image, &quiet, Point::ZERO, 1.0, None).expect("valid settings");
        assert_eq!(result.data(), image.data());

        let settings = GrainSettings {
            amount: 50.0,
            size: 2.0,
            roughness: 30.0,
            seed: 7,
        };
        let first = apply_grain(&image, &settings, Point::ZERO, 1.0, None).expect("valid settings");
        let again = apply_grain(&image, &settings, Point::ZERO, 1.0, None).expect("valid settings");
        assert_eq!(first.data(), again.data());
        assert_ne!(first.data(), image.data());
        // A different pattern, but the same amount of noise.
        let other = apply_grain(&image, &settings, Point::ZERO, 1.0, Some(9)).expect("valid settings");
        assert_ne!(first.data(), other.data());
    }

    /// Exposure works in linear light: `+1` stop doubles it, gamma 2 takes the square root, and
    /// offset adds light; the defaults change nothing and alpha is kept.
    #[test]
    fn exposure_works_in_linear_light_with_offset_and_gamma() {
        let input = rgba(1, 1, &[128, 128, 128, 255]);
        let defaults = apply_exposure(&input, &ExposureSettings::default()).expect("valid settings");
        assert_eq!(defaults.data(), input.data(), "defaults change nothing");
        let brighter = apply_exposure(
            &input,
            &ExposureSettings { exposure: 1.0, ..ExposureSettings::default() },
        )
        .expect("valid settings")
        .get(0, 0);
        assert!((brighter[0] as i32 - 176).abs() <= 2, "+1 stop doubles linear light: {brighter:?}");
        assert_eq!(brighter[0], brighter[2]);
        let lifted = apply_exposure(
            &input,
            &ExposureSettings { gamma: 2.0, ..ExposureSettings::default() },
        )
        .expect("valid settings")
        .get(0, 0);
        assert!((lifted[0] as i32 - 181).abs() <= 2, "gamma 2 takes the square root of linear light: {lifted:?}");
        let black = rgba(1, 1, &[0, 0, 0, 255]);
        let offset = apply_exposure(
            &black,
            &ExposureSettings { offset: 0.1, ..ExposureSettings::default() },
        )
        .expect("valid settings")
        .get(0, 0);
        assert!((offset[0] as i32 - 89).abs() <= 2, "offset adds linear light: {offset:?}");
        let translucent = rgba(1, 1, &[64, 64, 64, 128]);
        let kept = apply_exposure(
            &translucent,
            &ExposureSettings { exposure: 1.0, ..ExposureSettings::default() },
        )
        .expect("valid settings");
        assert_eq!(kept.get(0, 0)[3], translucent.get(0, 0)[3], "alpha kept");
    }

    /// Gradient Map colors by brightness: black takes the shadows color, white the highlights,
    /// mid gray sits halfway between, and reversed swaps the ends.
    #[test]
    fn gradient_map_colors_by_brightness_and_reverses() {
        let mut settings = GradientMapSettings {
            shadows: AdjustmentColor::new(1.0, 0.0, 0.0),
            highlights: AdjustmentColor::new(0.0, 0.0, 1.0),
            ..GradientMapSettings::default()
        };
        assert_eq!(
            apply_gradient_map(&rgba(1, 1, &[0, 0, 0, 255]), &settings)
                .expect("valid settings")
                .get(0, 0),
            [255, 0, 0, 255]
        );
        assert_eq!(
            apply_gradient_map(&rgba(1, 1, &[255, 255, 255, 255]), &settings)
                .expect("valid settings")
                .get(0, 0),
            [0, 0, 255, 255]
        );
        let middle = apply_gradient_map(&rgba(1, 1, &[128, 128, 128, 255]), &settings)
            .expect("valid settings")
            .get(0, 0);
        assert!((middle[0] as i32 - 127).abs() <= 2, "{middle:?}");
        assert!((middle[2] as i32 - 128).abs() <= 2, "{middle:?}");
        assert_eq!(middle[1], 0, "{middle:?}");

        // A translucent white keeps its color and its alpha.
        let translucent = apply_gradient_map(&rgba(1, 1, &[128, 128, 128, 128]), &settings)
            .expect("valid settings");
        let cleared = straight_pixel(&translucent, 0, 0);
        assert!(cleared[2] >= 250.0, "{cleared:?}");
        assert!(cleared[0] <= 5.0, "{cleared:?}");
        assert!((cleared[3] - 128.0).abs() <= 1.0, "alpha kept: {cleared:?}");

        settings.reversed = true;
        assert_eq!(
            apply_gradient_map(&rgba(1, 1, &[0, 0, 0, 255]), &settings)
                .expect("valid settings")
                .get(0, 0),
            [0, 0, 255, 255]
        );
    }

    /// Grain is fixed in document space: a piece of the canvas drawn at its own place gets that part
    /// of the whole pattern, and transparency is left alone.
    #[test]
    fn grain_is_fixed_in_document_space_and_leaves_transparency_alone() {
        let settings = GrainSettings { amount: 60.0, size: 2.0, roughness: 40.0, seed: 7 };
        let flat = |width: usize, height: usize| Rgba8Image::opaque(width, height, [128, 128, 128, 255]);
        let whole = apply_grain(&flat(40, 40), &settings, Point::ZERO, 1.0, None)
            .expect("valid settings");
        let values: std::collections::HashSet<u8> = whole.pixels().map(|pixel| pixel[0]).collect();
        assert!(values.len() > 5, "grain varies the brightness");
        assert!(
            whole.pixels().all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2]),
            "the same change on every channel"
        );
        // A 20 × 20 piece drawn at its place in the document gets the same grain as that part of the whole.
        let part = apply_grain(&flat(20, 20), &settings, Point::new(10.0, 10.0), 1.0, None)
            .expect("valid settings");
        for y in 0..20 {
            for x in 0..20 {
                assert_eq!(
                    part.get(x, y),
                    whole.get(x + 10, y + 10),
                    "grain must not shift when only part of the canvas redraws"
                );
            }
        }
        let reseeded = GrainSettings { seed: 8, ..settings };
        assert_ne!(
            apply_grain(&flat(40, 40), &reseeded, Point::ZERO, 1.0, None)
                .expect("valid settings")
                .data(),
            whole.data(),
            "another seed, another pattern"
        );
        let quiet = GrainSettings { amount: 0.0, ..settings };
        let untouched = flat(4, 4);
        assert_eq!(
            apply_grain(&untouched, &quiet, Point::ZERO, 1.0, None)
                .expect("valid settings")
                .data(),
            untouched.data(),
            "no amount, no change"
        );
        let cleared = apply_grain(&Rgba8Image::new(4, 4), &settings, Point::ZERO, 1.0, None)
            .expect("valid settings");
        assert!(cleared.pixels().all(|pixel| pixel[3] == 0), "clear pixels stay clear");
    }

    /// Grain's Size controls the particle scale even with Roughness up: larger particles change less
    /// from pixel to pixel.
    #[test]
    fn grain_size_controls_particle_scale_even_with_roughness() {
        let source = Rgba8Image::opaque(64, 64, [128, 128, 128, 255]);
        let small = apply_grain(
            &source,
            &GrainSettings { amount: 70.0, size: 1.0, roughness: 70.0, seed: 17 },
            Point::ZERO,
            1.0,
            None,
        )
        .expect("valid settings");
        let large = apply_grain(
            &source,
            &GrainSettings { amount: 70.0, size: 12.0, roughness: 70.0, seed: 17 },
            Point::ZERO,
            1.0,
            None,
        )
        .expect("valid settings");
        let neighboring_difference = |image: &Rgba8Image| -> f64 {
            let mut total = 0.0f64;
            let mut count = 0.0f64;
            for y in 0..64 {
                for x in 1..64 {
                    total += (image.get(x, y)[0] as f64 - image.get(x - 1, y)[0] as f64).abs();
                    count += 1.0;
                }
            }
            total / count
        };
        assert!(
            neighboring_difference(&large) < neighboring_difference(&small) * 0.7,
            "larger grain should form visibly larger, more coherent particles: {} vs {}",
            neighboring_difference(&large),
            neighboring_difference(&small)
        );
    }

    /// Curves bend through their handles and are clamped outside them.
    #[test]
    fn curves_bend_through_their_handles() {
        let mut settings = CurvesSettings::default();
        // The red channel through a midpoint pulling it up.
        settings.channels[1] = vec![
            CurvePoint { x: 0.0, y: 0.0 },
            CurvePoint { x: 128.0, y: 200.0 },
            CurvePoint { x: 255.0, y: 255.0 },
        ];
        assert!(settings.is_valid());
        assert_eq!(curve_value(&settings, 0.0, 1), 0.0);
        assert!((curve_value(&settings, 128.0, 1) - 200.0).abs() < 1e-9);
        assert!((curve_value(&settings, 255.0, 1) - 255.0).abs() < 1e-6);
        assert_eq!(curve_value(&settings, 300.0, 1), 255.0);
        // The composite curve is still the identity, so the tables follow the channel's own curve.
        assert!((curve_value(&settings, 100.0, 0) - 100.0).abs() < 1e-6);
        let tables = curves_tables(&settings);
        assert!((tables[128] as f64 * 255.0 - 200.0).abs() < 1e-3);
        // The green channel's table, 128 entries further in, is still the identity.
        assert!((tables[256 + 128] as f64 * 255.0 - 128.0).abs() < 1.0);
    }

    /// A settings value outside its range is rejected, as the Swift threw `ProjectError.invalid`.
    #[test]
    fn invalid_settings_are_rejected() {
        let image = Rgba8Image::opaque(1, 1, [0, 0, 0, 255]);
        let invalid = ExposureSettings {
            exposure: 21.0,
            ..ExposureSettings::default()
        };
        assert!(apply_exposure(&image, &invalid).is_err());
        let nan = ExposureSettings {
            gamma: f64::NAN,
            ..ExposureSettings::default()
        };
        assert!(apply_exposure(&image, &nan).is_err());
        let grain = GrainSettings {
            size: 0.1,
            ..GrainSettings::default()
        };
        assert!(apply_grain(&image, &grain, Point::ZERO, 1.0, None).is_err());
        assert!(apply_grain(&image, &GrainSettings::default(), Point::ZERO, 0.0, None).is_err());
        let balance = ColorBalanceSettings {
            mid_cyan_red: 101.0,
            ..ColorBalanceSettings::default()
        };
        assert!(apply_color_balance(&image, &balance).is_err());
        let white = BlackWhiteSettings {
            tint_saturation: 101.0,
            ..BlackWhiteSettings::default()
        };
        assert!(apply_black_white(&image, &white).is_err());
    }
}
