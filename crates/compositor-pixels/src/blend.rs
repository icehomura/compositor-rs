//! Layer blending for the canonical premultiplied sRGB RGBA8 buffers: the modes `SeparableBlend` sent
//! through Core Image, and the ones Core Graphics drew, computed here for every [`LayerBlendMode`].
//!
//! The original split the modes between the two frameworks. Core Graphics got Color Burn and Color Dodge
//! wrong — its versions ignore how transparent the source is, so a soft brush came out with a hard edge —
//! and it had no equivalent at all for Linear Burn, Linear Dodge, Vivid Light, Linear Light, Pin Light,
//! Hard Mix, Subtract or Divide. Those modes were drawn into a copy of the canvas, blended there through a
//! Core Image filter and the result put back ([`LayerBlendMode::needs_surface`]). Soft Light went the same
//! way: Core Graphics's own formula came out up to 25 levels lighter than Photoshop's with a light blend
//! color. Photoshop's Darker Color and Lighter Color are left out: they compare a pixel's whole brightness
//! rather than working a channel at a time, and neither framework implements them.
//!
//! Every mode here is computed in the **sRGB** the canvas holds, never in linear light. Core Image works in
//! a linear space unless told otherwise, and Color Burn, Color Dodge and Soft Light are not separable from
//! the gamma they are computed in: over 40% gray, an 80% gray layer dodges to 62% instead of Photoshop's
//! 100%, and burns to 0% instead of 25% ([`LayerBlendMode::is_gamma_sensitive`]). The blend has to happen
//! in the same sRGB the canvas is in.
//!
//! # Arithmetic
//!
//! The buffers are premultiplied; the blend functions themselves are Adobe's (the ones Photoshop, Core
//! Graphics and Core Image all implement — the PDF blend functions) and work on **straight**
//! (unpremultiplied) components in 0…1. For straight colors `Cs`/`Cb`, alphas `αs`/`αb` and blend function
//! `B`, a composited pixel is
//!
//! ```text
//! αo = αs + αb·(1 − αs)
//! Co = αs·(1 − αb)·Cs + αb·(1 − αs)·Cb + αs·αb·B(Cb, Cs)
//! ```
//!
//! which is source-over with the blend applied where the two pixels overlap: the source where the backdrop
//! is transparent, the backdrop where the source does not reach, and the blended color where both are
//! opaque. A layer's opacity scales its alpha — `αs` is the pixel's alpha times the opacity, exactly as
//! `CGContext.setAlpha` did before a draw — and a source pixel with no alpha leaves the backdrop pixel
//! untouched. A fully transparent backdrop is replaced by the source, whatever the mode.
//!
//! Results are written back the way the `effects_compose` Metal kernel wrote them:
//! `clamp(component, 0, 1) * 255 + 0.5` truncated, i.e. rounded to the nearest byte
//! ([`compositor_core::buffer::pack`]). The arithmetic is `f64` throughout — the precision the rest of the
//! port uses for pixel color, where Core Image's own kernels used `f32` and could differ in the last place
//! before that rounding — and the pixel is rounded once, at the end.

use compositor_core::buffer::pack;
use compositor_core::{LayerBlendMode, Rgba8Image, RGBA_PIXEL};
use rayon::prelude::*;

/// The luminance weights Photoshop's non-separable modes measure hue, saturation and luminosity with.
const LUMINANCE: [f64; 3] = [0.3, 0.59, 0.11];

/// One premultiplied sRGB pixel of `source` blended over `backdrop` at full opacity: the mode's blend
/// function on straight components, composited source-over.
///
/// The per-pixel core of [`blend_over`], exposed for callers that blend a handful of pixels of their own
/// (and for tests). Neither the pixel's alpha nor its opacity is quantized: the result is rounded once,
/// when it is packed back into bytes.
pub fn blend_pixel(mode: LayerBlendMode, backdrop: [u8; 4], source: [u8; 4]) -> [u8; 4] {
    blend_pixel_with_opacity(mode, backdrop, source, 1.0)
}

/// `source` blended over what `backdrop` holds, written back into `backdrop`, at the layer's `opacity` —
/// the modes Core Graphics couldn't draw and the ones it drew, all through the same arithmetic.
///
/// Both images stay in the canonical premultiplied sRGB RGBA8 layout. The overlap is composited pixel by
/// pixel: a source that reaches past the backdrop is clipped and one that is smaller leaves the rest of
/// the backdrop alone. `opacity` (0…1, values outside are clamped, as `setAlpha` did) scales the source's
/// alpha before the blend. Rows are independent, so the work is spread over `rayon`; the output is
/// bit-identical to a single-threaded run.
pub fn blend_over(mode: LayerBlendMode, backdrop: &mut Rgba8Image, source: &Rgba8Image, opacity: f64) {
    let opacity = if opacity.is_nan() { 0.0 } else { opacity.clamp(0.0, 1.0) };
    if opacity <= 0.0 {
        return;
    }
    let width = backdrop.width().min(source.width());
    let height = backdrop.height().min(source.height());
    if width == 0 || height == 0 {
        return;
    }
    let backdrop_stride = backdrop.stride();
    let source_stride = source.stride();
    backdrop
        .data_mut()
        .par_chunks_exact_mut(backdrop_stride)
        .zip(source.data().par_chunks_exact(source_stride))
        .take(height)
        .for_each(|(backdrop_row, source_row)| blend_row(mode, backdrop_row, source_row, width, opacity));
}

/// Blends one row of a pair of images; see [`blend_over`].
fn blend_row(mode: LayerBlendMode, backdrop: &mut [u8], source: &[u8], width: usize, opacity: f64) {
    for (backdrop_pixel, source_pixel) in backdrop
        .chunks_exact_mut(RGBA_PIXEL)
        .zip(source.chunks_exact(RGBA_PIXEL))
        .take(width)
    {
        let blended = blend_pixel_with_opacity(
            mode,
            [backdrop_pixel[0], backdrop_pixel[1], backdrop_pixel[2], backdrop_pixel[3]],
            [source_pixel[0], source_pixel[1], source_pixel[2], source_pixel[3]],
            opacity,
        );
        backdrop_pixel.copy_from_slice(&blended);
    }
}

/// [`blend_pixel`] with the layer's opacity folding into the source's alpha.
fn blend_pixel_with_opacity(
    mode: LayerBlendMode,
    backdrop: [u8; 4],
    source: [u8; 4],
    opacity: f64,
) -> [u8; 4] {
    let opacity = if opacity.is_nan() { 0.0 } else { opacity.clamp(0.0, 1.0) };
    let source_alpha = source[3] as f64 / 255.0;
    let alpha = source_alpha * opacity;
    // Nothing to draw: the overlay is fully transparent, and the backdrop pixel stays exactly as it was.
    if alpha <= 0.0 {
        return backdrop;
    }
    let (backdrop_color, backdrop_alpha) = straight(backdrop);
    let (source_color, _) = straight(source);
    let blended = blend_straight(mode, backdrop_color, source_color);
    let keep = 1.0 - alpha;
    let mut components = [0.0; 4];
    for i in 0..3 {
        components[i] = alpha * (1.0 - backdrop_alpha) * source_color[i]
            + backdrop_alpha * keep * backdrop_color[i]
            + alpha * backdrop_alpha * blended[i];
    }
    components[3] = alpha + backdrop_alpha * keep;
    pack(components)
}

/// A premultiplied pixel as its straight color and its alpha, both 0…1.
///
/// A pixel with no alpha has no color to recover, so it reads as black; the colors are clamped to 0…1,
/// which is all the blend functions are defined on (valid premultiplied data is already inside it).
fn straight(pixel: [u8; 4]) -> ([f64; 3], f64) {
    let alpha = pixel[3] as f64 / 255.0;
    if alpha <= 0.0 {
        return ([0.0; 3], 0.0);
    }
    let color = [
        (pixel[0] as f64 / 255.0 / alpha).clamp(0.0, 1.0),
        (pixel[1] as f64 / 255.0 / alpha).clamp(0.0, 1.0),
        (pixel[2] as f64 / 255.0 / alpha).clamp(0.0, 1.0),
    ];
    (color, alpha)
}

/// The blend function `B(Cb, Cs)` for a whole straight color, clamped to the 0…1 it is defined on.
fn blend_straight(mode: LayerBlendMode, backdrop: [f64; 3], source: [f64; 3]) -> [f64; 3] {
    let blended = match mode {
        LayerBlendMode::Normal => source,
        LayerBlendMode::Darken => per_channel(backdrop, source, |b, s| b.min(s)),
        LayerBlendMode::Multiply => per_channel(backdrop, source, |b, s| b * s),
        LayerBlendMode::ColorBurn => per_channel(backdrop, source, color_burn),
        LayerBlendMode::LinearBurn => per_channel(backdrop, source, |b, s| (b + s - 1.0).max(0.0)),
        LayerBlendMode::Lighten => per_channel(backdrop, source, |b, s| b.max(s)),
        LayerBlendMode::Screen => per_channel(backdrop, source, |b, s| b + s - b * s),
        LayerBlendMode::ColorDodge => per_channel(backdrop, source, color_dodge),
        LayerBlendMode::LinearDodge => per_channel(backdrop, source, |b, s| (b + s).min(1.0)),
        // Multiply where the backdrop is dark, screen where it is light: Hard Light with the two colors
        // swapped, so a 50% gray backdrop leaves the source alone.
        LayerBlendMode::Overlay => per_channel(backdrop, source, |b, s| {
            if b <= 0.5 {
                2.0 * b * s
            } else {
                1.0 - 2.0 * (1.0 - b) * (1.0 - s)
            }
        }),
        LayerBlendMode::SoftLight => per_channel(backdrop, source, soft_light),
        // Multiply where the source is dark, screen where it is light.
        LayerBlendMode::HardLight => per_channel(backdrop, source, |b, s| {
            if s <= 0.5 {
                2.0 * b * s
            } else {
                1.0 - 2.0 * (1.0 - b) * (1.0 - s)
            }
        }),
        LayerBlendMode::VividLight => per_channel(backdrop, source, |b, s| {
            if s <= 0.5 {
                color_burn(b, 2.0 * s)
            } else {
                color_dodge(b, 2.0 * s - 1.0)
            }
        }),
        // Linear Burn on a doubled source below ½, Linear Dodge above it: both are `b + 2·s − 1`.
        LayerBlendMode::LinearLight => per_channel(backdrop, source, |b, s| (b + 2.0 * s - 1.0).clamp(0.0, 1.0)),
        LayerBlendMode::PinLight => per_channel(backdrop, source, |b, s| {
            if s <= 0.5 {
                b.min(2.0 * s)
            } else {
                b.max(2.0 * s - 1.0)
            }
        }),
        // 0 or 1 per channel: white once Vivid Light reaches its threshold — the sum of Color Burn and
        // Color Dodge reaching 1, which is `b + s >= 1` — and black below it.
        LayerBlendMode::HardMix => per_channel(backdrop, source, |b, s| if b + s < 1.0 { 0.0 } else { 1.0 }),
        LayerBlendMode::Difference => per_channel(backdrop, source, |b, s| (b - s).abs()),
        LayerBlendMode::Exclusion => per_channel(backdrop, source, |b, s| b + s - 2.0 * b * s),
        LayerBlendMode::Subtract => per_channel(backdrop, source, |b, s| (b - s).max(0.0)),
        LayerBlendMode::Divide => per_channel(backdrop, source, divide),
        // The source's hue with the backdrop's saturation and luminosity.
        LayerBlendMode::Hue => set_lum(set_sat(source, saturation(backdrop)), luminance(backdrop)),
        // The source's saturation with the backdrop's hue and luminosity.
        LayerBlendMode::Saturation => set_lum(set_sat(backdrop, saturation(source)), luminance(backdrop)),
        // The source's hue and saturation with the backdrop's luminosity.
        LayerBlendMode::Color => set_lum(source, luminance(backdrop)),
        // The source's luminosity with the backdrop's hue and saturation.
        LayerBlendMode::Luminosity => set_lum(backdrop, luminance(source)),
    };
    [
        blended[0].clamp(0.0, 1.0),
        blended[1].clamp(0.0, 1.0),
        blended[2].clamp(0.0, 1.0),
    ]
}

/// Applies a separable blend function to the three components of a pair of colors.
fn per_channel(backdrop: [f64; 3], source: [f64; 3], f: impl Fn(f64, f64) -> f64) -> [f64; 3] {
    [
        f(backdrop[0], source[0]),
        f(backdrop[1], source[1]),
        f(backdrop[2], source[2]),
    ]
}

/// `Color Burn`: `1 − min(1, (1 − Cb) / Cs)`, and 0 where the source channel is black. Adobe's formula,
/// the one Photoshop computes, and it has to run on sRGB components: in linear light an 80% gray layer
/// over 40% gray burns to 0% instead of Photoshop's 25%.
fn color_burn(backdrop: f64, source: f64) -> f64 {
    if source <= 0.0 {
        return 0.0;
    }
    (1.0 - (1.0 - backdrop) / source).max(0.0)
}

/// `Color Dodge`: `min(1, Cb / (1 − Cs))`, and 1 where the source channel is white. sRGB only, for the
/// same reason as Color Burn: in linear light an 80% gray layer over 40% gray dodges to 62% instead of
/// Photoshop's 100%.
fn color_dodge(backdrop: f64, source: f64) -> f64 {
    if source >= 1.0 {
        return 1.0;
    }
    (backdrop / (1.0 - source)).min(1.0)
}

/// `Soft Light`: `Cb − (1 − 2·Cs)·Cb·(1 − Cb)` for a dark source, and `Cb + (2·Cs − 1)·(D(Cb) − Cb)` for a
/// light one — Adobe's curve, the one Photoshop draws. Core Graphics's own formula came out up to 25
/// levels lighter with a light blend color; this one puts 50% gray under 90% gray at 170 of 255.
fn soft_light(backdrop: f64, source: f64) -> f64 {
    if source <= 0.5 {
        backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
    } else {
        backdrop + (2.0 * source - 1.0) * (soft_light_curve(backdrop) - backdrop)
    }
}

/// `D(Cb)`: `((16·Cb − 12)·Cb + 4)·Cb` over the bottom quarter, `sqrt(Cb)` above it — the curve Adobe's
/// Soft Light lightens toward, continuous at ¼ where both give ½.
fn soft_light_curve(backdrop: f64) -> f64 {
    if backdrop <= 0.25 {
        ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
    } else {
        backdrop.sqrt()
    }
}

/// `Divide`: `Cb / Cs`, saturating at 1. A black source channel divides to white — any nonzero backdrop
/// over 0 is infinite — while black over black stays black, as it does for the other saturating modes.
fn divide(backdrop: f64, source: f64) -> f64 {
    if source <= 0.0 {
        return if backdrop <= 0.0 { 0.0 } else { 1.0 };
    }
    (backdrop / source).min(1.0)
}

/// Photoshop's luminance: `0.3·r + 0.59·g + 0.11·b`.
fn luminance(color: [f64; 3]) -> f64 {
    LUMINANCE[0] * color[0] + LUMINANCE[1] * color[1] + LUMINANCE[2] * color[2]
}

/// A color's saturation, the spread between its largest and smallest components.
fn saturation(color: [f64; 3]) -> f64 {
    color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2])
}

/// `SetSat(C, s)`: the color with its saturation replaced by `s` — the components keep their order, the
/// smallest becoming 0. A neutral color has no spread to scale, so it stays black.
fn set_sat(color: [f64; 3], saturation: f64) -> [f64; 3] {
    let mut lowest = 0;
    let mut highest = 0;
    for i in 1..3 {
        if color[i] < color[lowest] {
            lowest = i;
        }
        if color[i] > color[highest] {
            highest = i;
        }
    }
    if color[highest] <= color[lowest] {
        return [0.0; 3];
    }
    let middle = 3 - lowest - highest;
    let mut result = [0.0; 3];
    result[lowest] = 0.0;
    result[middle] = (color[middle] - color[lowest]) * saturation / (color[highest] - color[lowest]);
    result[highest] = saturation;
    result
}

/// `SetLum(C, l)`: the color shifted to the luminance `l`, then clipped back into gamut (see
/// [`clip_color`]).
fn set_lum(color: [f64; 3], luminosity: f64) -> [f64; 3] {
    let delta = luminosity - luminance(color);
    clip_color([color[0] + delta, color[1] + delta, color[2] + delta])
}

/// `ClipColor(C)`: scales a color that left 0…1 around its own luminance so it comes back into gamut,
/// keeping the luminance it was given. Both fixes are checked against the color as it arrived, and a
/// color that is already in gamut is returned untouched.
fn clip_color(color: [f64; 3]) -> [f64; 3] {
    let luminosity = luminance(color);
    let lowest = color[0].min(color[1]).min(color[2]);
    let highest = color[0].max(color[1]).max(color[2]);
    let mut result = color;
    if lowest < 0.0 && luminosity != lowest {
        for component in result.iter_mut() {
            *component = luminosity + (*component - luminosity) * luminosity / (luminosity - lowest);
        }
    }
    if highest > 1.0 && luminosity != highest {
        for component in result.iter_mut() {
            *component = luminosity + (*component - luminosity) * (1.0 - luminosity) / (highest - luminosity);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(value: u8) -> [u8; 4] {
        [value, value, value, 255]
    }

    fn image(width: usize, height: usize, pixels: &[[u8; 4]]) -> Rgba8Image {
        let mut data = Vec::new();
        for pixel in pixels {
            data.extend_from_slice(pixel);
        }
        assert_eq!(pixels.len(), width * height);
        Rgba8Image::from_data(width, height, data)
    }

    #[test]
    fn multiply_takes_white_and_black_to_themselves() {
        assert_eq!(blend_pixel(LayerBlendMode::Multiply, gray(255), gray(128)), gray(128));
        assert_eq!(blend_pixel(LayerBlendMode::Multiply, gray(128), gray(255)), gray(128));
        assert_eq!(blend_pixel(LayerBlendMode::Multiply, [200, 150, 100, 255], [0, 0, 0, 255]), [0, 0, 0, 255]);
        // The Swift suite's known pixels: 0.4 under 0.8, 0.32 of 255.
        assert_eq!(blend_pixel(LayerBlendMode::Multiply, gray(102), gray(204)), gray(82));
    }

    #[test]
    fn screen_takes_black_and_white_to_themselves() {
        assert_eq!(blend_pixel(LayerBlendMode::Screen, gray(0), gray(128)), gray(128));
        assert_eq!(blend_pixel(LayerBlendMode::Screen, gray(255), gray(128)), gray(255));
        // 0.4 over 0.8: 1 − 0.6·0.2 = 0.88, the Swift suite's known pixel.
        assert_eq!(blend_pixel(LayerBlendMode::Screen, gray(102), gray(204)), gray(224));
    }

    #[test]
    fn color_burn_and_color_dodge_match_the_documented_srgb_examples() {
        // 40% gray under 80% gray, the pair the Swift note is written about: Photoshop dodges to 100%
        // (the linear value would be 62%) and burns to 25% (linear would be 0%).
        assert_eq!(blend_pixel(LayerBlendMode::ColorDodge, gray(102), gray(204)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::ColorBurn, gray(102), gray(204)), gray(64));
        // Black burns to black, white dodges to white.
        assert_eq!(blend_pixel(LayerBlendMode::ColorBurn, gray(200), gray(0)), gray(0));
        assert_eq!(blend_pixel(LayerBlendMode::ColorDodge, gray(200), gray(255)), gray(255));
    }

    #[test]
    fn overlay_is_multiply_below_the_midpoint_and_screen_above_it() {
        // A black or white backdrop takes the whole result with it.
        assert_eq!(blend_pixel(LayerBlendMode::Overlay, gray(0), gray(128)), gray(0));
        assert_eq!(blend_pixel(LayerBlendMode::Overlay, gray(255), gray(128)), gray(255));
        // A 50% source doubles the backdrop back to itself on either side of the branch.
        assert_eq!(blend_pixel(LayerBlendMode::Overlay, gray(102), gray(128)), gray(102));
        assert_eq!(blend_pixel(LayerBlendMode::Overlay, gray(153), gray(128)), gray(153));
        // 0.4 over 0.8: 2·0.4·0.8 = 0.64, the Swift suite's known pixel.
        assert_eq!(blend_pixel(LayerBlendMode::Overlay, gray(102), gray(204)), gray(163));
    }

    #[test]
    fn soft_light_matches_photoshop() {
        // Photoshop: 50% gray under 90% gray is 170 of 255 — Core Graphics's formula came out 25 levels
        // lighter, Core Image's within 5.
        assert_eq!(blend_pixel(LayerBlendMode::SoftLight, gray(128), gray(230)), gray(170));
        // A 50% source is a no-op, and black or white takes the extremes.
        assert_eq!(blend_pixel(LayerBlendMode::SoftLight, gray(200), gray(128)), gray(200));
        assert_eq!(blend_pixel(LayerBlendMode::SoftLight, gray(255), gray(0)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::SoftLight, gray(0), gray(255)), gray(0));
    }

    #[test]
    fn hard_mix_is_a_threshold_of_the_sum() {
        assert_eq!(blend_pixel(LayerBlendMode::HardMix, gray(51), gray(51)), gray(0));
        assert_eq!(blend_pixel(LayerBlendMode::HardMix, gray(0), gray(0)), gray(0));
        assert_eq!(blend_pixel(LayerBlendMode::HardMix, gray(100), gray(200)), gray(255));
        // 0.2 + 0.8 is exactly 1, so the sum reaches the threshold.
        assert_eq!(blend_pixel(LayerBlendMode::HardMix, gray(51), gray(204)), gray(255));
    }

    #[test]
    fn linear_and_pin_and_vivid_light() {
        // Linear Dodge of 0.4 and 0.3 is 0.7, the Swift stacking test's drawn value.
        assert_eq!(blend_pixel(LayerBlendMode::LinearDodge, gray(102), gray(77)), gray(179));
        assert_eq!(blend_pixel(LayerBlendMode::LinearDodge, gray(200), gray(200)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::LinearBurn, gray(102), gray(204)), gray(51));
        assert_eq!(blend_pixel(LayerBlendMode::LinearBurn, gray(102), gray(51)), gray(0));
        // 0.4 + 2·0.8 − 1 = 1, and 0.4 + 2·0.2 − 1 = −0.2 clamps to black.
        assert_eq!(blend_pixel(LayerBlendMode::LinearLight, gray(102), gray(204)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::LinearLight, gray(102), gray(51)), gray(0));
        // A light source pins to Lighten, a dark one to Darken.
        assert_eq!(blend_pixel(LayerBlendMode::PinLight, gray(102), gray(204)), gray(153));
        assert_eq!(blend_pixel(LayerBlendMode::PinLight, gray(102), gray(51)), gray(102));
        // Vivid Light is a dodge above ½ and a burn below it.
        assert_eq!(blend_pixel(LayerBlendMode::VividLight, gray(102), gray(204)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::VividLight, gray(102), gray(51)), gray(0));
    }

    #[test]
    fn difference_exclusion_subtract_and_divide() {
        for value in [0u8, 128, 255] {
            assert_eq!(blend_pixel(LayerBlendMode::Difference, gray(value), gray(value)), gray(0));
        }
        assert_eq!(blend_pixel(LayerBlendMode::Difference, gray(204), gray(102)), gray(102));
        // 0.4 + 0.8 − 2·0.32 = 0.56.
        assert_eq!(blend_pixel(LayerBlendMode::Exclusion, gray(102), gray(204)), gray(143));
        assert_eq!(blend_pixel(LayerBlendMode::Subtract, gray(204), gray(102)), gray(102));
        assert_eq!(blend_pixel(LayerBlendMode::Subtract, gray(102), gray(204)), gray(0));
        assert_eq!(blend_pixel(LayerBlendMode::Divide, gray(102), gray(204)), gray(128));
        assert_eq!(blend_pixel(LayerBlendMode::Divide, gray(102), gray(0)), gray(255));
        assert_eq!(blend_pixel(LayerBlendMode::Divide, gray(0), gray(0)), gray(0));
    }

    #[test]
    fn non_separable_modes_keep_the_backdrop_when_the_colors_are_equal() {
        for mode in [
            LayerBlendMode::Hue,
            LayerBlendMode::Saturation,
            LayerBlendMode::Color,
            LayerBlendMode::Luminosity,
        ] {
            assert_eq!(blend_pixel(mode, [200, 100, 50, 255], [200, 100, 50, 255]), [200, 100, 50, 255], "{mode:?}");
        }
    }

    #[test]
    fn hue_and_saturation_of_a_neutral_color_cannot_show() {
        // A gray backdrop has no hue or saturation to replace, so the result stays that gray.
        assert_eq!(blend_pixel(LayerBlendMode::Hue, gray(128), [255, 0, 0, 255]), gray(128));
        assert_eq!(blend_pixel(LayerBlendMode::Saturation, gray(128), [255, 0, 0, 255]), gray(128));
    }

    #[test]
    fn color_takes_the_source_hue_and_saturation() {
        // Red's hue and saturation at the backdrop's 50% gray luminosity, clipped back into gamut:
        // 0.3·1 + 0.59·0.28852 + 0.11·0.28852 = 0.5, the gray it came from.
        assert_eq!(blend_pixel(LayerBlendMode::Color, gray(128), [255, 0, 0, 255]), [255, 74, 74, 255]);
    }

    #[test]
    fn luminosity_takes_the_backdrop_hue_and_saturation() {
        // The red backdrop keeps its hue and saturation and takes the gray source's luminosity.
        assert_eq!(blend_pixel(LayerBlendMode::Luminosity, [255, 0, 0, 255], gray(128)), [255, 74, 74, 255]);
    }

    #[test]
    fn a_transparent_source_leaves_the_backdrop_untouched() {
        for mode in LayerBlendMode::ALL {
            assert_eq!(blend_pixel(mode, [200, 100, 50, 128], [10, 20, 30, 0]), [200, 100, 50, 128], "{mode:?}");
            // Even a backdrop that is itself transparent, and holds color bytes of its own.
            assert_eq!(blend_pixel(mode, [17, 250, 3, 0], [10, 20, 30, 0]), [17, 250, 3, 0], "{mode:?}");
        }
    }

    #[test]
    fn a_transparent_backdrop_is_replaced_by_the_source() {
        for mode in LayerBlendMode::ALL {
            assert_eq!(blend_pixel(mode, [0, 0, 0, 0], [200, 100, 50, 255]), [200, 100, 50, 255], "{mode:?}");
            // A half-transparent source contributes its color premultiplied, so its bytes come back
            // unchanged: 128/255 · (100/128, 50/128, 25/128) = (100, 50, 25)/255.
            assert_eq!(blend_pixel(mode, [0, 0, 0, 0], [100, 50, 25, 128]), [100, 50, 25, 128], "{mode:?}");
        }
    }

    #[test]
    fn opacity_scales_the_source_alpha() {
        // The Swift suite's known pixels: a 0.8 layer at half opacity over 0.4 is 0.6 of 255, and at no
        // opacity the backdrop stays as it was.
        assert_eq!(blend_pixel_with_opacity(LayerBlendMode::Normal, gray(102), gray(204), 0.5), gray(153));
        assert_eq!(blend_pixel_with_opacity(LayerBlendMode::Normal, gray(102), gray(204), 0.0), gray(102));
        // 0.5·0.4 + 0.5·(0.4·0.8) = 0.36.
        assert_eq!(blend_pixel_with_opacity(LayerBlendMode::Multiply, gray(102), gray(204), 0.5), gray(92));
        // A partly transparent backdrop keeps its own color where the source does not reach — the
        // composite is 0.5·0.49804·0.8 + 0.50196·0.5·0.796875 + 0.5·0.50196·0.8 = 0.6 — and the alphas
        // add to 0.5 + 0.50196·0.5 = 0.75098, 192 of 255.
        assert_eq!(
            blend_pixel_with_opacity(LayerBlendMode::Normal, [102, 102, 102, 128], gray(204), 0.5),
            [153, 153, 153, 192]
        );
        assert_eq!(blend_pixel_with_opacity(LayerBlendMode::Multiply, gray(102), gray(204), f64::NAN), gray(102));
    }

    #[test]
    fn separable_modes_stay_gray_on_gray() {
        for mode in LayerBlendMode::ALL {
            if matches!(
                mode,
                LayerBlendMode::Hue | LayerBlendMode::Saturation | LayerBlendMode::Color | LayerBlendMode::Luminosity
            ) {
                continue;
            }
            let blended = blend_pixel(mode, gray(90), gray(200));
            assert!(blended[0] == blended[1] && blended[1] == blended[2], "{mode:?}: {blended:?}");
        }
    }

    #[test]
    fn blend_over_is_the_row_wise_application_of_blend_pixel() {
        let backdrop_pixels: Vec<[u8; 4]> = (0..15u8)
            .map(|i| [i * 17, 255 - i * 11, i * 7 + 3, 255 - i * 5])
            .collect();
        let source_pixels: Vec<[u8; 4]> = (0..15u8).map(|i| [200 - i * 9, i * 13, 255 - i * 3, 255]).collect();
        for mode in LayerBlendMode::ALL {
            let mut backdrop = image(5, 3, &backdrop_pixels);
            let source = image(5, 3, &source_pixels);
            blend_over(mode, &mut backdrop, &source, 1.0);
            for index in 0..15 {
                let expected = blend_pixel(mode, backdrop_pixels[index], source_pixels[index]);
                let (x, y) = (index % 5, index / 5);
                assert_eq!(backdrop.get(x, y), expected, "{mode:?} at {x},{y}");
            }
        }
    }

    #[test]
    fn blend_over_is_deterministic_and_skips_transparent_source_pixels() {
        let backdrop_pixels: Vec<[u8; 4]> = (0..24u8).map(|i| [i * 9, 255 - i * 7, i * 5, 255]).collect();
        let mut source_pixels: Vec<[u8; 4]> = (0..24u8).map(|i| [255 - i * 3, i * 11, 200 - i * 5, 255]).collect();
        source_pixels[7] = [12, 34, 56, 0];
        for mode in LayerBlendMode::ALL {
            let mut first = image(6, 4, &backdrop_pixels);
            let mut second = image(6, 4, &backdrop_pixels);
            let source = image(6, 4, &source_pixels);
            blend_over(mode, &mut first, &source, 1.0);
            blend_over(mode, &mut second, &source, 1.0);
            assert_eq!(first.data(), second.data(), "{mode:?}");
            assert_eq!(first.get(1, 1), backdrop_pixels[7], "{mode:?}");
        }
    }

    #[test]
    fn blend_over_clips_to_the_smaller_image() {
        // A source that reaches past the backdrop is clipped, and one that is smaller leaves the rest of
        // the backdrop alone.
        let mut backdrop = image(4, 4, &[gray(10); 16]);
        let source = image(2, 2, &[gray(200); 4]);
        blend_over(LayerBlendMode::Normal, &mut backdrop, &source, 1.0);
        for y in 0..4 {
            for x in 0..4 {
                let expected = if x < 2 && y < 2 { gray(200) } else { gray(10) };
                assert_eq!(backdrop.get(x, y), expected, "at {x},{y}");
            }
        }
        // A wider source is read row by row, not as one long run.
        let mut backdrop = image(2, 2, &[gray(10), gray(10), gray(10), gray(10)]);
        let source = image(4, 1, &[gray(200), gray(201), gray(202), gray(203)]);
        blend_over(LayerBlendMode::Normal, &mut backdrop, &source, 1.0);
        assert_eq!(backdrop.get(0, 0), gray(200));
        assert_eq!(backdrop.get(1, 0), gray(201));
        assert_eq!(backdrop.get(0, 1), gray(10));
        assert_eq!(backdrop.get(1, 1), gray(10));
    }

    #[test]
    fn blend_over_with_no_opacity_or_no_pixels_does_nothing() {
        let mut backdrop = image(2, 2, &[gray(10), gray(20), gray(30), gray(40)]);
        let before = backdrop.data().to_vec();
        let source = image(2, 2, &[gray(200); 4]);
        blend_over(LayerBlendMode::Multiply, &mut backdrop, &source, 0.0);
        blend_over(LayerBlendMode::Multiply, &mut backdrop, &source, -1.0);
        assert_eq!(backdrop.data(), before);
        let empty = Rgba8Image::new(0, 0);
        blend_over(LayerBlendMode::Multiply, &mut backdrop, &empty, 1.0);
        assert_eq!(backdrop.data(), before);
    }
}
