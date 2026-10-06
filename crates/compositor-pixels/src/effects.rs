//! Layer effects rasterization: a layer's stroke, drop shadow, color overlay, inner shadow, outer
//! glow and inner glow drawn around its own pixels.
//!
//! Two Swift files describe the same picture: `Document/LayerEffects.swift`'s
//! `LayerEffectsRenderer` (a `CGContext` fallback) and `Rendering/MetalLayerEffects.swift`'s MSL
//! kernels, which is what actually runs whenever Metal is available — i.e. always, on the machines
//! this app shipped to. This is a 1:1 port of the MSL kernels (`effects_alpha`,
//! `effects_spread_rows`/`columns`, `effects_ring`, `effects_shift`, `effects_blur_rows`/`columns`,
//! `effects_inside`, `effects_compose`) into CPU kernels, so the numbers here are the numbers the
//! GPU produced: `f32` coverage buffers, the same clamps, the same evaluation order, the same
//! composed layers in the same order (shadow, outer glow, outside stroke, the layer's pixels,
//! color overlay, inner glow, inner shadow, inside stroke).
//!
//! `LayerEffectsRenderer::render` is the CGContext half: it takes the layer's pixels and its mask,
//! grows the grid by `margin`, draws the masked pixels into it and hands that to the kernels —
//! exactly the sequence `LayerEffectsRenderer.render` performs before falling back to Core
//! Graphics.

use std::sync::{LazyLock, Mutex};

use compositor_core::geom::{CGFloat, Point, Rect, Size};
use compositor_core::layer_effects::{
    InnerGlowEffect, InnerShadowEffect, LayerEffects, OuterGlowEffect, StrokeEffect,
};
use compositor_core::layer_transform::LayerTransform;
use compositor_core::limits::MAX_SURFACE_PIXELS;
use compositor_core::{CoreError, Gray8Image, Rgba8Image, Result};
use rayon::prelude::*;

/// `MetalLayerEffects.render`'s guard: 80 million pixels.
const MAX_EFFECT_PIXELS: usize = 80_000_000;

/// The surface cache's ceilings, from `LayerEffectsRenderer.Cache`.
const CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const CACHE_MAX_ENTRIES: usize = 8;

/// A layer with its effects: the pixels on a grid grown by `inset` pixels on every side, so the
/// caller places it by growing the layer's transform in the same proportion.
#[derive(Clone, Debug)]
pub struct EffectsRender {
    pub image: Rgba8Image,
    /// How far the grid grew beyond the layer's own pixels on each side.
    pub inset: CGFloat,
}

/// Draws a layer's effects around its pixels. The result is the layer as it should appear —
/// shadow behind, stroke around, pixels on top — on a canvas grown by `inset` pixels on every
/// side, so the caller places it by growing the layer's transform in the same proportion.
pub struct LayerEffectsRenderer;

impl LayerEffectsRenderer {
    /// `image` with `effects` around it, reusing the last result for the same pixels, mask and
    /// settings. `None` when there is nothing to draw or the effects can't be made, so the caller
    /// draws the layer as it is.
    pub fn cached(
        image: &Rgba8Image,
        mask: Option<&Gray8Image>,
        effects: Option<&LayerEffects>,
    ) -> Option<EffectsRender> {
        let visible = effects?.visible();
        if visible.is_empty() || !visible.is_valid() {
            return None;
        }
        cache_result(image, mask, &visible, || Self::render(image, mask, &visible)).ok()
    }

    /// The layer's transform grown by the margin its effects need, so the bigger image lands in
    /// the same place.
    pub fn placed(transform: &LayerTransform, image: &Rgba8Image, inset: CGFloat) -> LayerTransform {
        let mut grown = transform.clone();
        let width = image.width() as f64;
        let height = image.height() as f64;
        if !(width > inset * 2.0 && height > inset * 2.0) {
            return grown;
        }
        grown.size = compositor_core::geom::Size::new(
            transform.size.width * width / (width - inset * 2.0),
            transform.size.height * height / (height - inset * 2.0),
        );
        let center = transform.center();
        grown.origin = Point::new(
            center.x - grown.size.width / 2.0,
            center.y - grown.size.height / 2.0,
        );
        grown
    }

    /// `margin(for:)`: the room the visible effects need around the layer.
    pub fn margin(effects: &LayerEffects) -> CGFloat {
        let effects = effects.visible();
        let mut margin: CGFloat = 0.0;
        if let Some(stroke) = &effects.stroke {
            if !stroke.inside {
                margin = margin.max(stroke.size);
            }
        }
        if let Some(shadow) = &effects.shadow {
            margin = margin.max(shadow.distance + shadow.blur * 3.0);
        }
        if let Some(glow) = &effects.outer_glow {
            margin = margin.max(glow.size * 3.0);
        }
        margin.ceil() + 2.0
    }

    /// `image` with `effects` around it. `mask` (the layer's own mask, in its pixel grid) hides
    /// part of the layer before the effects are made, so they follow the shape that is actually
    /// shown, as in Photoshop.
    pub fn render(
        image: &Rgba8Image,
        mask: Option<&Gray8Image>,
        effects: &LayerEffects,
    ) -> Result<EffectsRender> {
        let effects = effects.visible();
        if !effects.is_valid() {
            return Err(invalid_effects());
        }
        let inset = Self::margin(&effects);
        let width = image.width() + inset as usize * 2;
        let height = image.height() + inset as usize * 2;
        if width == 0 || height == 0 || width * height > MAX_SURFACE_PIXELS {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        // The layer as it is shown: its pixels through its mask.
        let shown = masked(image, mask);
        // The pixels with room around them, then the effects drawn around them.
        let mut padded = Rgba8Image::new(width, height);
        padded.draw_over(&shown, [inset as i32, inset as i32]);
        let built = render_effects(&padded, &effects)?;
        Ok(EffectsRender { image: built, inset })
    }
}

/// `ProjectError.invalid`'s message, the one the panel shows for effects that can't be made.
fn invalid_effects() -> CoreError {
    CoreError::Message(
        "This is not a valid Compositor project, or its metadata is damaged.".to_string(),
    )
}

/// The layer's pixels with its mask applied, or the pixels as they are when it has none.
///
/// Core Graphics' `clip(to:mask:)` multiplies the drawn image by the mask's coverage, so a
/// premultiplied pixel is simply scaled by `value / 255`. A mask of another size than the image is
/// stretched over it, as the clip's rect does.
fn masked(image: &Rgba8Image, mask: Option<&Gray8Image>) -> Rgba8Image {
    let Some(mask) = mask else {
        return image.clone();
    };
    if image.is_empty() {
        return image.clone();
    }
    let mut result = image.clone();
    let (width, height) = (image.width(), image.height());
    let (mask_width, mask_height) = (mask.width().max(1), mask.height().max(1));
    let same_size = mask.width() == width && mask.height() == height;
    let data = result.data_mut();
    let mask_data = mask.data();
    for y in 0..height {
        let mask_y = if same_size {
            y
        } else {
            (y * mask_height / height).min(mask_height - 1)
        };
        for x in 0..width {
            let mask_x = if same_size {
                x
            } else {
                (x * mask_width / width).min(mask_width - 1)
            };
            let coverage = mask_data[mask_y * mask_width + mask_x] as f32 / 255.0;
            if coverage >= 1.0 {
                continue;
            }
            let offset = (y * width + x) * 4;
            for channel in 0..4 {
                data[offset + channel] =
                    ((data[offset + channel] as f32) * coverage + 0.5) as u8;
            }
        }
    }
    result
}

// MARK: - The kernels

/// The alpha of every pixel as a coverage value, `effects_alpha`.
fn alpha_coverage(pixels: &Rgba8Image) -> Vec<f32> {
    let data = pixels.data();
    let mut coverage = vec![0.0f32; pixels.pixel_count()];
    coverage
        .par_chunks_mut(pixels.width().max(1))
        .enumerate()
        .for_each(|(y, row)| {
            let base = y * pixels.stride();
            for (x, value) in row.iter_mut().enumerate() {
                *value = data[base + x * 4 + 3] as f32 / 255.0;
            }
        });
    coverage
}

/// The largest (or smallest) value within `reach` on each side along a row, `effects_spread_rows`.
/// Past the edge there is nothing.
fn spread_rows(source: &[f32], width: usize, height: usize, reach: i32, smallest: bool) -> Vec<f32> {
    let mut result = vec![0.0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let base = y * width;
            for (x, value) in row.iter_mut().enumerate() {
                let mut best = if smallest { 1.0f32 } else { 0.0f32 };
                let mut offset = -reach;
                while offset <= reach {
                    let sample = x as i32 + offset;
                    let sample = if sample < 0 || sample >= width as i32 {
                        0.0
                    } else {
                        source[base + sample as usize]
                    };
                    best = if smallest { best.min(sample) } else { best.max(sample) };
                    offset += 1;
                }
                *value = best;
            }
        });
    result
}

/// The same along a column, `effects_spread_columns`.
fn spread_columns(source: &[f32], width: usize, height: usize, reach: i32, smallest: bool) -> Vec<f32> {
    let mut result = vec![0.0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, value) in row.iter_mut().enumerate() {
                let mut best = if smallest { 1.0f32 } else { 0.0f32 };
                let mut offset = -reach;
                while offset <= reach {
                    let sample = y as i32 + offset;
                    let sample = if sample < 0 || sample >= height as i32 {
                        0.0
                    } else {
                        source[sample as usize * width + x]
                    };
                    best = if smallest { best.min(sample) } else { best.max(sample) };
                    offset += 1;
                }
                *value = best;
            }
        });
    result
}

/// What the stroke covers: the difference between the shape and the reached-out (or pulled-in)
/// shape, `effects_ring`.
fn ring(shape: &[f32], moved: &[f32], smallest: bool) -> Vec<f32> {
    shape
        .par_iter()
        .zip(moved.par_iter())
        .map(|(shape, moved)| {
            let value = if smallest {
                shape - moved
            } else {
                moved - shape
            };
            value.clamp(0.0, 1.0)
        })
        .collect()
}

/// The shape moved by `(dx, dy)`, sampled bilinearly so the shadow moves smoothly rather than in
/// whole steps, `effects_shift`.
fn shift(source: &[f32], width: usize, height: usize, dx: f32, dy: f32) -> Vec<f32> {
    let mut result = vec![0.0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let sy = y as f32 - dy;
            for (x, value) in row.iter_mut().enumerate() {
                let sx = x as f32 - dx;
                if sx >= 0.0 && sy >= 0.0 && sx <= (width - 1) as f32 && sy <= (height - 1) as f32 {
                    let x0 = sx.floor() as usize;
                    let y0 = sy.floor() as usize;
                    let x1 = (x0 + 1).min(width - 1);
                    let y1 = (y0 + 1).min(height - 1);
                    let fx = sx - x0 as f32;
                    let fy = sy - y0 as f32;
                    let top = source[y0 * width + x0]
                        + (source[y0 * width + x1] - source[y0 * width + x0]) * fx;
                    let bottom = source[y1 * width + x0]
                        + (source[y1 * width + x1] - source[y1 * width + x0]) * fx;
                    *value = top + (bottom - top) * fy;
                }
            }
        });
    result
}

/// The Gaussian weights of one pass: `radius` taps either side of the center.
fn blur_weights(sigma: f32, radius: i32) -> Vec<f32> {
    let denominator = 2.0 * sigma * sigma;
    (-radius..=radius)
        .map(|offset| (-(offset * offset) as f32 / denominator).exp())
        .collect()
}

/// One separable Gaussian pass over the rows, `effects_blur_rows`: weights normalized by their own
/// sum, sampling clamped to the edge.
fn blur_rows(source: &[f32], width: usize, height: usize, sigma: f32, radius: i32) -> Vec<f32> {
    let weights = blur_weights(sigma, radius);
    let weight_sum: f32 = weights.iter().sum();
    let mut result = vec![0.0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let base = y * width;
            for (x, value) in row.iter_mut().enumerate() {
                let mut total = 0.0f32;
                for (index, weight) in weights.iter().enumerate() {
                    let sample = (x as i32 + index as i32 - radius).clamp(0, width as i32 - 1);
                    total += weight * source[base + sample as usize];
                }
                *value = total / weight_sum;
            }
        });
    result
}

/// The same over the columns, `effects_blur_columns`.
fn blur_columns(source: &[f32], width: usize, height: usize, sigma: f32, radius: i32) -> Vec<f32> {
    let weights = blur_weights(sigma, radius);
    let weight_sum: f32 = weights.iter().sum();
    let mut result = vec![0.0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, value) in row.iter_mut().enumerate() {
                let mut total = 0.0f32;
                for (index, weight) in weights.iter().enumerate() {
                    let sample = (y as i32 + index as i32 - radius).clamp(0, height as i32 - 1);
                    total += weight * source[sample as usize * width + x];
                }
                *value = total / weight_sum;
            }
        });
    result
}

/// One separable Gaussian blur, row pass then column pass. The Swift only blurs when the standard
/// deviation clears 0.01, with `radius = max(1, round(sigma * 3))`.
fn blur(source: &[f32], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).round().max(1.0) as i32;
    let rows = blur_rows(source, width, height, sigma, radius);
    blur_columns(&rows, width, height, sigma, radius)
}

/// A shadow cast inside the layer's own edges: what is outside the layer, softened, kept to the
/// layer's own shape, `effects_inside`.
fn inside(shape: &[f32], moved: &[f32]) -> Vec<f32> {
    shape
        .par_iter()
        .zip(moved.par_iter())
        .map(|(shape, moved)| (shape * (1.0 - moved)).clamp(0.0, 1.0))
        .collect()
}

/// One color's `(red, green, blue, opacity)`, as the compose kernel's `float4`s.
fn color_vector(color: compositor_core::PaletteColor, opacity: f64) -> [f32; 4] {
    [
        color.red as f32,
        color.green as f32,
        color.blue as f32,
        opacity as f32,
    ]
}

/// The composed layers, `effects_compose`: shadow behind, outer glow over it, outside stroke over
/// that, the layer's pixels over that, then a color overlay, an inner glow, an inner shadow and an
/// inside stroke on top.
#[allow(clippy::too_many_arguments)]
fn compose(
    pixels: &Rgba8Image,
    ring: &[f32],
    shadow: &[f32],
    inner: &[f32],
    shape: &[f32],
    glow: &[f32],
    inner_glow: &[f32],
    stroke_color: [f32; 4],
    shadow_color: [f32; 4],
    overlay_color: [f32; 4],
    inner_color: [f32; 4],
    glow_color: [f32; 4],
    inner_glow_color: [f32; 4],
    has_stroke: bool,
    stroke_inside: bool,
    has_shadow: bool,
    has_inner_shadow: bool,
    has_overlay: bool,
    has_glow: bool,
    has_inner_glow: bool,
) -> Rgba8Image {
    let width = pixels.width();
    let height = pixels.height();
    let mut output = Rgba8Image::new(width, height);
    output
        .data_mut()
        .par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let index = y * width + x;
                let mut color = [0.0f32; 3];
                let mut alpha = 0.0f32;
                if has_shadow {
                    let coverage = (shadow[index] * shadow_color[3]).clamp(0.0, 1.0);
                    color = [
                        shadow_color[0] * coverage,
                        shadow_color[1] * coverage,
                        shadow_color[2] * coverage,
                    ];
                    alpha = coverage;
                }
                if has_glow {
                    let coverage =
                        (glow[index] * (1.0 - shape[index]) * glow_color[3]).clamp(0.0, 1.0);
                    let inverse = 1.0 - coverage;
                    color = [
                        glow_color[0] * coverage + color[0] * inverse,
                        glow_color[1] * coverage + color[1] * inverse,
                        glow_color[2] * coverage + color[2] * inverse,
                    ];
                    alpha = coverage + alpha * inverse;
                }
                let stroke_coverage = if has_stroke {
                    (ring[index] * stroke_color[3]).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if has_stroke && !stroke_inside {
                    let inverse = 1.0 - stroke_coverage;
                    color = [
                        stroke_color[0] * stroke_coverage + color[0] * inverse,
                        stroke_color[1] * stroke_coverage + color[1] * inverse,
                        stroke_color[2] * stroke_coverage + color[2] * inverse,
                    ];
                    alpha = stroke_coverage + alpha * inverse;
                }
                let source_offset = index * 4;
                let source = [
                    pixels.data()[source_offset] as f32 / 255.0,
                    pixels.data()[source_offset + 1] as f32 / 255.0,
                    pixels.data()[source_offset + 2] as f32 / 255.0,
                    pixels.data()[source_offset + 3] as f32 / 255.0,
                ];
                let inverse_source = 1.0 - source[3];
                color = [
                    source[0] + color[0] * inverse_source,
                    source[1] + color[1] * inverse_source,
                    source[2] + color[2] * inverse_source,
                ];
                alpha = source[3] + alpha * inverse_source;
                if has_overlay {
                    let coverage = (shape[index] * overlay_color[3]).clamp(0.0, 1.0);
                    let inverse = 1.0 - coverage;
                    color = [
                        overlay_color[0] * coverage + color[0] * inverse,
                        overlay_color[1] * coverage + color[1] * inverse,
                        overlay_color[2] * coverage + color[2] * inverse,
                    ];
                    alpha = coverage + alpha * inverse;
                }
                if has_inner_glow {
                    let coverage = (inner_glow[index] * inner_glow_color[3]).clamp(0.0, 1.0);
                    let inverse = 1.0 - coverage;
                    color = [
                        inner_glow_color[0] * coverage + color[0] * inverse,
                        inner_glow_color[1] * coverage + color[1] * inverse,
                        inner_glow_color[2] * coverage + color[2] * inverse,
                    ];
                    alpha = coverage + alpha * inverse;
                }
                if has_inner_shadow {
                    let coverage = (inner[index] * inner_color[3]).clamp(0.0, 1.0);
                    let inverse = 1.0 - coverage;
                    color = [
                        inner_color[0] * coverage + color[0] * inverse,
                        inner_color[1] * coverage + color[1] * inverse,
                        inner_color[2] * coverage + color[2] * inverse,
                    ];
                    alpha = coverage + alpha * inverse;
                }
                if has_stroke && stroke_inside {
                    let inverse = 1.0 - stroke_coverage;
                    color = [
                        stroke_color[0] * stroke_coverage + color[0] * inverse,
                        stroke_color[1] * stroke_coverage + color[1] * inverse,
                        stroke_color[2] * stroke_coverage + color[2] * inverse,
                    ];
                    alpha = stroke_coverage + alpha * inverse;
                }
                let to_byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                row[x * 4] = to_byte(color[0]);
                row[x * 4 + 1] = to_byte(color[1]);
                row[x * 4 + 2] = to_byte(color[2]);
                row[x * 4 + 3] = to_byte(alpha);
            }
        });
    output
}

/// The MSL pipeline of `MetalLayerEffects.render`, on the padded pixels: the shape's coverage, the
/// stroke's reach, the shadow's move and blur, the glows, and the compose pass. The result is the
/// same size as `pixels`.
pub fn render_effects(pixels: &Rgba8Image, effects: &LayerEffects) -> Result<Rgba8Image> {
    let width = pixels.width();
    let height = pixels.height();
    let count = width * height;
    if count == 0 || count > MAX_EFFECT_PIXELS {
        return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
    }
    let empty = vec![0.0f32; count];
    // first: the shape's own coverage.
    let shape = alpha_coverage(pixels);

    let stroke = effects
        .stroke
        .clone()
        .filter(|stroke| stroke.is_enabled() && stroke.size > 0.0 && stroke.opacity > 0.0);
    let mut ring_buffer = empty.clone();
    if let Some(stroke) = &stroke {
        // The shape reached out (or pulled in) by the stroke's size; then the ring between them.
        let reach = stroke.size.round().max(1.0) as i32;
        let rows = spread_rows(&shape, width, height, reach, stroke.inside);
        let moved = spread_columns(&rows, width, height, reach, stroke.inside);
        ring_buffer = ring(&shape, &moved, stroke.inside);
    }

    let shadow = effects
        .shadow
        .clone()
        .filter(|shadow| shadow.is_enabled() && shadow.opacity > 0.0);
    let mut shadow_buffer = empty.clone();
    if let Some(shadow) = &shadow {
        // The shape moved and softened.
        let offset = shadow.offset();
        let moved = shift(&shape, width, height, offset.width as f32, offset.height as f32);
        let sigma = (shadow.blur / 2.0) as f32;
        shadow_buffer = if sigma > 0.01 {
            blur(&moved, width, height, sigma)
        } else {
            moved
        };
    }

    let overlay = effects
        .color_overlay
        .clone()
        .filter(|overlay| overlay.is_enabled() && overlay.opacity > 0.0);
    let inner_shadow = effects
        .inner_shadow
        .clone()
        .filter(|inner| inner.is_enabled() && inner.opacity > 0.0);
    let mut inner_buffer = empty.clone();
    if let Some(inner_shadow) = &inner_shadow {
        // What lies outside the layer, moved and softened, kept to the layer's own shape.
        let offset = inner_shadow.offset();
        let moved = shift(&shape, width, height, offset.width as f32, offset.height as f32);
        let sigma = (inner_shadow.blur / 2.0) as f32;
        let softened = if sigma > 0.01 {
            blur(&moved, width, height, sigma)
        } else {
            moved
        };
        inner_buffer = inside(&shape, &softened);
    }

    let glow = effects
        .outer_glow
        .clone()
        .filter(|glow| glow.is_enabled() && glow.size > 0.0 && glow.opacity > 0.0);
    let mut glow_buffer = empty.clone();
    if let Some(glow) = &glow {
        let sigma = (glow.size / 2.0) as f32;
        glow_buffer = if sigma > 0.01 {
            blur(&shape, width, height, sigma)
        } else {
            shift(&shape, width, height, 0.0, 0.0)
        };
    }

    let inner_glow = effects
        .inner_glow
        .clone()
        .filter(|glow| glow.is_enabled() && glow.size > 0.0 && glow.opacity > 0.0);
    let mut inner_glow_buffer = empty.clone();
    if let Some(inner_glow) = &inner_glow {
        let sigma = (inner_glow.size / 2.0) as f32;
        let blurred = if sigma > 0.01 {
            blur(&shape, width, height, sigma)
        } else {
            shift(&shape, width, height, 0.0, 0.0)
        };
        inner_glow_buffer = inside(&shape, &blurred);
    }

    let stroke_color = stroke
        .as_ref()
        .map(|stroke| color_vector(stroke.color(), stroke.opacity))
        .unwrap_or([0.0; 4]);
    let shadow_color = shadow
        .as_ref()
        .map(|shadow| color_vector(shadow.color(), shadow.opacity))
        .unwrap_or([0.0; 4]);
    let overlay_color = overlay
        .as_ref()
        .map(|overlay| color_vector(overlay.color(), overlay.opacity))
        .unwrap_or([0.0; 4]);
    let inner_color = inner_shadow
        .as_ref()
        .map(|inner| color_vector(inner.color(), inner.opacity))
        .unwrap_or([0.0; 4]);
    let glow_color = glow
        .as_ref()
        .map(|glow| color_vector(glow.color(), glow.opacity))
        .unwrap_or([0.0; 4]);
    let inner_glow_color = inner_glow
        .as_ref()
        .map(|glow| color_vector(glow.color(), glow.opacity))
        .unwrap_or([0.0; 4]);

    Ok(compose(
        pixels,
        &ring_buffer,
        &shadow_buffer,
        &inner_buffer,
        &shape,
        &glow_buffer,
        &inner_glow_buffer,
        stroke_color,
        shadow_color,
        overlay_color,
        inner_color,
        glow_color,
        inner_glow_color,
        stroke.is_some(),
        stroke.as_ref().is_some_and(|stroke| stroke.inside),
        shadow.is_some(),
        inner_shadow.is_some(),
        overlay.is_some(),
        glow.is_some(),
        inner_glow.is_some(),
    ))
}


// MARK: - The pieces a surface draws with

/// The per-piece coverage helpers of `LayerEffectsRenderer`: a shape's alpha, a stroke's ring, a
/// shadow's silhouette, a glow's light — each as 8-bit gray coverage (255 is fully covered), the
/// form `Canvas::draw_gray`/`draw_coverage` and `Raster::fill` take. `LayerEffectsSurface` composes
/// a live surface from these; [`LayerEffectsRenderer::render`] uses the kernels instead.
impl LayerEffectsRenderer {
    /// `LayerEffectsRenderer.coverage`: the shape's own alpha, placed in a bigger canvas and
    /// optionally softened (`CIGaussianBlur` at half the given blur, clamped to the canvas).
    pub fn coverage(pixels: &Rgba8Image, placed: Rect, size: Size, blur: CGFloat) -> Result<Gray8Image> {
        let width = size.width as usize;
        let height = size.height as usize;
        if width == 0 || height == 0 {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        let levels = coverage_levels(pixels, placed, width, height);
        let image = image_from_levels(&levels, width, height);
        if blur > 0.0 {
            Ok(crate::filters::gaussian_blur_gray(&image, blur / 2.0, true))
        } else {
            Ok(image)
        }
    }

    /// `LayerEffectsRenderer.masked`: the layer's pixels with its mask applied, or the pixels as they
    /// are when it has none.
    pub fn masked(image: &Rgba8Image, mask: Option<&Gray8Image>) -> Rgba8Image {
        masked(image, mask)
    }

    /// `LayerEffectsRenderer.shadowCoverage`: a shadow's coverage for one piece of a layer — its
    /// shape, moved and softened.
    pub fn shadow_coverage(
        pixels: &Rgba8Image,
        size: Size,
        offset: Size,
        blur: CGFloat,
    ) -> Result<Gray8Image> {
        let placed = Rect::new(0.0, 0.0, pixels.width() as f64, pixels.height() as f64)
            .offset_by(offset.width, offset.height);
        Self::coverage(pixels, placed, size, blur)
    }

    /// `LayerEffectsRenderer.ringCoverage`: a stroke's ring for one piece of a layer.
    pub fn ring_coverage(pixels: &Rgba8Image, size: Size, stroke: &StrokeEffect) -> Result<Gray8Image> {
        Self::stroke_coverage(
            pixels,
            Rect::new(0.0, 0.0, pixels.width() as f64, pixels.height() as f64),
            size,
            stroke,
        )
    }

    /// `LayerEffectsRenderer.strokeCoverage`: where a stroke lands — the shape grown (or shrunk) by
    /// its size, less the shape itself. A square reach, not a round one: a round one eats into the
    /// corners of a rectangle, which reads as a wobbly edge.
    pub fn stroke_coverage(
        pixels: &Rgba8Image,
        placed: Rect,
        size: Size,
        stroke: &StrokeEffect,
    ) -> Result<Gray8Image> {
        let width = size.width as usize;
        let height = size.height as usize;
        if width == 0 || height == 0 {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        let shape = coverage_levels(pixels, placed, width, height);
        let reach = stroke.size.round().max(1.0) as i32;
        let moved = Self::extreme(&shape, width, height, reach, stroke.inside);
        let levels: Vec<f32> = shape
            .iter()
            .zip(moved.iter())
            .map(|(shape, moved)| {
                let value = if stroke.inside {
                    shape - moved
                } else {
                    moved - shape
                };
                value.clamp(0.0, 1.0)
            })
            .collect();
        Ok(image_from_levels(&levels, width, height))
    }

    /// `LayerEffectsRenderer.innerCoverage`: an inner shadow's coverage — what lies outside the
    /// layer, moved and softened, kept to the layer's own shape.
    pub fn inner_coverage(
        pixels: &Rgba8Image,
        placed: Rect,
        size: Size,
        shadow: &InnerShadowEffect,
    ) -> Result<Gray8Image> {
        let width = size.width as usize;
        let height = size.height as usize;
        if width == 0 || height == 0 {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        let shape = coverage_levels(pixels, placed, width, height);
        let offset = shadow.offset();
        let moved = Self::coverage(
            pixels,
            placed.offset_by(offset.width, offset.height),
            size,
            shadow.blur,
        )?;
        let outside: Vec<f32> = moved.data().iter().map(|value| *value as f32 / 255.0).collect();
        let levels = inside_levels(&shape, &outside);
        Ok(image_from_levels(&levels, width, height))
    }

    /// `LayerEffectsRenderer.outerGlowCoverage`: the layer's shape softened omnidirectionally, with
    /// the shape interior excluded.
    pub fn outer_glow_coverage(
        pixels: &Rgba8Image,
        placed: Rect,
        size: Size,
        glow: &OuterGlowEffect,
    ) -> Result<Gray8Image> {
        let width = size.width as usize;
        let height = size.height as usize;
        if width == 0 || height == 0 {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        let shape = coverage_levels(pixels, placed, width, height);
        let soft = Self::coverage(pixels, placed, size, glow.size)?;
        let soft: Vec<f32> = soft.data().iter().map(|value| *value as f32 / 255.0).collect();
        let levels: Vec<f32> = soft
            .iter()
            .zip(shape.iter())
            .map(|(soft, shape)| (soft * (1.0 - shape)).clamp(0.0, 1.0))
            .collect();
        Ok(image_from_levels(&levels, width, height))
    }

    /// `LayerEffectsRenderer.innerGlowCoverage`: the source shape softened inward, kept to the
    /// layer's own shape.
    pub fn inner_glow_coverage(
        pixels: &Rgba8Image,
        placed: Rect,
        size: Size,
        glow: &InnerGlowEffect,
    ) -> Result<Gray8Image> {
        let width = size.width as usize;
        let height = size.height as usize;
        if width == 0 || height == 0 {
            return Err(CoreError::TooLarge(compositor_core::limits::max_surface_megapixels()));
        }
        let shape = coverage_levels(pixels, placed, width, height);
        let blurred = Self::coverage(pixels, placed, size, glow.size)?;
        let outside: Vec<f32> = blurred.data().iter().map(|value| *value as f32 / 255.0).collect();
        let levels = inside_levels(&shape, &outside);
        Ok(image_from_levels(&levels, width, height))
    }

    /// `LayerEffectsRenderer.extreme`: the largest (or smallest) value within `reach` on each side —
    /// two sliding-window passes, so the cost doesn't grow with the reach. Past the edge there is
    /// nothing, so a smallest pass there reads zero.
    pub fn extreme(source: &[f32], width: usize, height: usize, reach: i32, smallest: bool) -> Vec<f32> {
        if width == 0 || height == 0 || source.len() != width * height {
            return Vec::new();
        }
        let rows = spread_rows(source, width, height, reach.max(0), smallest);
        spread_columns(&rows, width, height, reach.max(0), smallest)
    }
}

/// The shape's own alpha, drawn into a `width` × `height` device-space grid at `placed`, nearest
/// neighbor as `BrushRaster.draw` (interpolation quality `.none`) draws it, and nothing outside the
/// rect the image is drawn in.
fn coverage_levels(pixels: &Rgba8Image, placed: Rect, width: usize, height: usize) -> Vec<f32> {
    let mut levels = vec![0.0f32; width * height];
    if pixels.is_empty() || placed.width() <= 0.0 || placed.height() <= 0.0 {
        return levels;
    }
    let (source_width, source_height) = (pixels.width(), pixels.height());
    let data = pixels.data();
    levels
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let point_y = y as f64 + 0.5;
            if point_y < placed.min_y() || point_y >= placed.max_y() {
                return;
            }
            for (x, value) in row.iter_mut().enumerate() {
                let point_x = x as f64 + 0.5;
                if point_x < placed.min_x() || point_x >= placed.max_x() {
                    continue;
                }
                let sx = (((point_x - placed.min_x()) * source_width as f64 / placed.width()).floor()
                    as i64)
                    .clamp(0, source_width as i64 - 1) as usize;
                let sy = (((point_y - placed.min_y()) * source_height as f64 / placed.height()).floor()
                    as i64)
                    .clamp(0, source_height as i64 - 1) as usize;
                *value = data[(sy * source_width + sx) * 4 + 3] as f32 / 255.0;
            }
        });
    levels
}

/// `GuidedMatte.image`: 0–1 levels back to a gray image.
fn image_from_levels(levels: &[f32], width: usize, height: usize) -> Gray8Image {
    let mut image = Gray8Image::new(width, height);
    image
        .data_mut()
        .par_iter_mut()
        .zip(levels.par_iter())
        .for_each(|(byte, level)| {
            *byte = (level * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        });
    image
}

/// `shape × (1 - outside)`, clamped: what the inner shadow and inner glow keep.
fn inside_levels(shape: &[f32], outside: &[f32]) -> Vec<f32> {
    shape
        .par_iter()
        .zip(outside.par_iter())
        .map(|(shape, outside)| (shape * (1.0 - outside)).clamp(0.0, 1.0))
        .collect()
}

// MARK: - The cache

struct CacheEntry {
    /// The address of the image the entry was made from: the same key the Swift's cache uses when
    /// it compares `CGImage` identities. It is never dereferenced — the sizes beside it are what
    /// the cache measures its budget with — so a caller that drops the image can never make this a
    /// dangling read.
    image: *const Rgba8Image,
    mask: *const Gray8Image,
    image_bytes: usize,
    mask_pixels: usize,
    effects: LayerEffects,
    result: EffectsRender,
}

/// SAFETY: the addresses are only ever compared, never read through.
unsafe impl Send for CacheEntry {}

static CACHE: LazyLock<Mutex<Vec<CacheEntry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// The cost of one entry, as the Swift's cache measures it: the result plus what it was made from.
fn entry_cost(entry: &CacheEntry) -> usize {
    entry.result.image.size_bytes() + entry.image_bytes + entry.mask_pixels
}

fn cache_result(
    image: &Rgba8Image,
    mask: Option<&Gray8Image>,
    effects: &LayerEffects,
    make: impl FnOnce() -> Result<EffectsRender>,
) -> Result<EffectsRender> {
    let key = image as *const Rgba8Image;
    let mask_key = mask.map(|mask| mask as *const Gray8Image).unwrap_or(std::ptr::null());
    {
        let cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache
            .iter()
            .find(|entry| entry.image == key && entry.mask == mask_key && entry.effects == *effects)
        {
            return Ok(hit.result.clone());
        }
    }
    let made = make()?;
    {
        let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mask_pixels = mask.map_or(0, |mask| mask.pixel_count());
        let cost = made.image.size_bytes() + image.size_bytes() + mask_pixels;
        if cost <= CACHE_BUDGET_BYTES {
            cache.push(CacheEntry {
                image: key,
                mask: mask_key,
                image_bytes: image.size_bytes(),
                mask_pixels,
                effects: effects.clone(),
                result: made.clone(),
            });
            while cache.len() > CACHE_MAX_ENTRIES
                || cache.iter().map(entry_cost).sum::<usize>() > CACHE_BUDGET_BYTES
            {
                cache.remove(0);
            }
        }
    }
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::layer_effects::{
        ColorOverlayEffect, InnerGlowEffect, InnerShadowEffect, OuterGlowEffect, ShadowEffect,
        StrokeEffect,
    };
    use compositor_core::layer_text::LayerTextStyle;

    fn white(width: usize, height: usize) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        image
    }

    fn alpha(image: &Rgba8Image, x: usize, y: usize) -> u8 {
        image.get(x, y)[3]
    }

    /// A `size` × `size` image with a solid `inner` × `inner` square in its middle, as the Swift's
    /// `createSquareImage`/`solidSquare` draw one.
    fn square(size: usize, inner: usize, color: [u8; 4]) -> Rgba8Image {
        let mut image = Rgba8Image::new(size, size);
        let origin = (size - inner) / 2;
        for y in origin..origin + inner {
            for x in origin..origin + inner {
                image.set(x, y, color);
            }
        }
        image
    }

    /// A pixel's straight (unpremultiplied) color and alpha, as the Swift's `NSBitmapImageRep`
    /// `colorAt` reports them.
    fn straight(image: &Rgba8Image, x: usize, y: usize) -> [f64; 4] {
        let pixel = image.get(x, y);
        let channel = |value: u8| {
            if pixel[3] == 0 {
                0.0
            } else {
                (value as f64 * 255.0 / pixel[3] as f64).min(255.0) / 255.0
            }
        };
        [
            channel(pixel[0]),
            channel(pixel[1]),
            channel(pixel[2]),
            pixel[3] as f64 / 255.0,
        ]
    }

    #[test]
    fn margin_matches_the_visible_effects() {
        let mut effects = LayerEffects::default();
        assert_eq!(LayerEffectsRenderer::margin(&effects), 2.0);
        effects.stroke = Some(StrokeEffect { size: 4.0, ..StrokeEffect::default() });
        assert_eq!(LayerEffectsRenderer::margin(&effects), 6.0);
        // An inside stroke needs no room.
        effects.stroke = Some(StrokeEffect { size: 4.0, inside: true, ..StrokeEffect::default() });
        assert_eq!(LayerEffectsRenderer::margin(&effects), 2.0);
        effects.shadow = Some(ShadowEffect { distance: 20.0, blur: 20.0, ..ShadowEffect::default() });
        assert_eq!(LayerEffectsRenderer::margin(&effects), 82.0); // 20 + 60, then ceil + 2
        effects.outer_glow = Some(OuterGlowEffect { size: 20.0, ..OuterGlowEffect::default() });
        assert_eq!(LayerEffectsRenderer::margin(&effects), 82.0);
        // A disabled effect is not measured.
        effects.shadow = Some(ShadowEffect { distance: 100.0, blur: 100.0, enabled: Some(false), ..ShadowEffect::default() });
        assert_eq!(LayerEffectsRenderer::margin(&effects), 62.0);
    }

    #[test]
    fn placed_grows_the_transform_around_its_center() {
        let mut transform = LayerTransform::default();
        transform.origin = Point::new(100.0, 50.0);
        transform.size = compositor_core::geom::Size::new(40.0, 20.0);
        let image = Rgba8Image::new(40, 20);
        let grown = LayerEffectsRenderer::placed(&transform, &image, 5.0);
        // The grid grew by 10 pixels a side: 40 * 40 / 30 wide, 20 * 20 / 10 tall.
        assert!((grown.size.width - 40.0 * 40.0 / 30.0).abs() < 1e-9);
        assert!((grown.size.height - 40.0).abs() < 1e-9);
        assert_eq!(grown.center().x, transform.center().x);
        assert_eq!(grown.center().y, transform.center().y);
        assert!((grown.origin.x - (transform.center().x - grown.size.width / 2.0)).abs() < 1e-9);
        assert!((grown.origin.y - (transform.center().y - grown.size.height / 2.0)).abs() < 1e-9);
    }

    #[test]
    fn placed_leaves_a_transform_too_small_to_grow() {
        let mut transform = LayerTransform::default();
        transform.size = compositor_core::geom::Size::new(4.0, 4.0);
        let image = Rgba8Image::new(4, 4);
        let grown = LayerEffectsRenderer::placed(&transform, &image, 10.0);
        assert_eq!(grown.size.width, transform.size.width);
    }

    #[test]
    fn spread_rows_reaches_past_the_edge_with_nothing_and_clamps_at_the_shape() {
        // 1 x 5 row: 0 0 1 0 0
        let source = vec![0.0, 0.0, 1.0, 0.0, 0.0];
        let grown = spread_rows(&source, 5, 1, 1, false);
        assert_eq!(grown, vec![0.0, 1.0, 1.0, 1.0, 0.0]);
        let shrunk = spread_rows(&source, 5, 1, 1, true);
        // Past the edge the smallest pass sees 0, so the whole row collapses.
        assert_eq!(shrunk, vec![0.0; 5]);
        let wide = spread_rows(&source, 5, 1, 0, false);
        assert_eq!(wide, source);
    }

    #[test]
    fn spread_columns_is_the_transpose_of_spread_rows() {
        let source = vec![0.0, 0.0, 1.0, 0.0, 0.0];
        let columns = spread_columns(&source, 1, 5, 1, false);
        assert_eq!(columns, vec![0.0, 1.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn ring_keeps_the_part_between_the_two_shapes() {
        let shape = vec![1.0, 1.0, 0.0];
        let moved = spread_rows(&shape, 3, 1, 1, false);
        assert_eq!(ring(&shape, &moved, false), vec![0.0, 0.0, 1.0]);
        let shrunk = spread_rows(&shape, 3, 1, 1, true);
        assert_eq!(ring(&shape, &shrunk, true), vec![1.0, 1.0, 0.0]);
    }

    #[test]
    fn shift_moves_bilinearly_and_zero_offset_copies() {
        // The bright sample sits at (0, 0); the shift samples the source at `x - dx`.
        let source = vec![1.0, 0.0, 0.0, 0.0];
        assert_eq!(shift(&source, 2, 2, 0.0, 0.0), source);
        let moved = shift(&source, 2, 2, 1.0, 0.0);
        assert_eq!(moved, vec![0.0, 1.0, 0.0, 0.0]);
        // Half a pixel between the two columns: the sample lands between them.
        let half = shift(&source, 2, 2, 0.5, 0.0);
        assert!((half[1] - 0.5).abs() < 1e-6, "{half:?}");
        // Past the far edge there is nothing, however far the shift asks.
        let far = shift(&source, 2, 2, 4.0, 0.0);
        assert_eq!(far, vec![0.0; 4]);
    }

    #[test]
    fn blur_normalizes_by_its_own_weights() {
        let source = vec![1.0; 16];
        let blurred = blur(&source, 4, 4, 1.0);
        for value in blurred {
            assert!((value - 1.0).abs() < 1e-5, "{value}");
        }
        // A single bright pixel spreads by the kernel's own weights: the two passes multiply, so the
        // center keeps `(w0 / Σw)²` of the light.
        let mut dot = vec![0.0f32; 21 * 21];
        dot[10 * 21 + 10] = 1.0;
        let spread = blur(&dot, 21, 21, 1.0);
        let weights = blur_weights(1.0, 3);
        let center = weights[(weights.len() - 1) / 2];
        let total: f32 = weights.iter().sum();
        let expected = (center / total) * (center / total);
        assert!(
            (spread[10 * 21 + 10] - expected).abs() < 1e-5,
            "{} vs {expected}",
            spread[10 * 21 + 10]
        );
        // Mirror pairs match, and the light falls off away from the center.
        for (a, b) in [(10 * 21 + 10 - 1, 10 * 21 + 10 + 1), (10 * 21 + 10 - 21, 10 * 21 + 10 + 21)] {
            assert!((spread[a] - spread[b]).abs() < 1e-6);
        }
        assert!(spread[10 * 21 + 10] > spread[10 * 21 + 9]);
        assert!(spread[10 * 21 + 9] > spread[10 * 21 + 8]);
        // Three taps of reach, and nothing beyond it: the kernel's own radius.
        assert!(spread[7 * 21 + 7] > 0.0);
        assert_eq!(spread[6 * 21 + 10], 0.0);
    }

    #[test]
    fn inside_keeps_the_shape_and_removes_what_moved_over_it() {
        let shape = vec![1.0, 1.0, 0.0];
        let moved = vec![1.0, 0.0, 0.0];
        assert_eq!(inside(&shape, &moved), vec![0.0, 1.0, 0.0]);
    }

    /// A drop shadow is drawn behind the pixels; an inside stroke is drawn over them.
    #[test]
    fn compose_draws_the_shadow_behind_and_the_inside_stroke_over_the_pixels() {
        let pixels = white(1, 1);
        let zeros = vec![0.0f32; 1];
        let shadow = vec![1.0f32; 1];
        let ring = vec![1.0f32; 1];
        // Only a shadow: the pixel is opaque white, so nothing of the shadow shows through.
        let built = compose(
            &pixels, &zeros, &shadow, &zeros, &zeros, &zeros, &zeros, [0.0; 4], [1.0, 0.0, 0.0, 1.0],
            [0.0; 4], [0.0; 4], [0.0; 4], [0.0; 4], false, false, true, false, false, false, false,
        );
        assert_eq!(built.get(0, 0), [255, 255, 255, 255]);
        // Only an outside stroke: the ring lands under the pixel too, and the pixel still wins.
        let built = compose(
            &pixels, &ring, &zeros, &zeros, &zeros, &zeros, &zeros, [0.0, 0.0, 1.0, 1.0], [0.0; 4],
            [0.0; 4], [0.0; 4], [0.0; 4], [0.0; 4], true, false, false, false, false, false, false,
        );
        assert_eq!(built.get(0, 0), [255, 255, 255, 255]);
        // An inside stroke is drawn last, so it covers the pixel.
        let built = compose(
            &pixels, &ring, &zeros, &zeros, &zeros, &zeros, &zeros, [0.0, 0.0, 1.0, 1.0], [0.0; 4],
            [0.0; 4], [0.0; 4], [0.0; 4], [0.0; 4], true, true, false, false, false, false, false,
        );
        assert_eq!(built.get(0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn compose_keeps_transparent_pixels_transparent_without_shapes() {
        let pixels = Rgba8Image::new(2, 2);
        let zeros = vec![0.0f32; 4];
        let built = compose(
            &pixels, &zeros, &zeros, &zeros, &zeros, &zeros, &zeros, [0.0; 4], [0.0; 4], [0.0; 4],
            [0.0; 4], [0.0; 4], [0.0; 4], false, false, false, false, false, false, false,
        );
        for pixel in built.pixels() {
            assert_eq!(pixel, [0, 0, 0, 0]);
        }
    }

    #[test]
    fn a_stroke_ring_lands_outside_the_pixels_and_the_pixels_stay_on_top() {
        let mut pixels = Rgba8Image::new(5, 5);
        pixels.set(2, 2, [255, 255, 255, 255]);
        let effects = LayerEffects {
            stroke: Some(StrokeEffect { size: 1.0, red: 1.0, green: 0.0, blue: 0.0, opacity: 1.0, ..StrokeEffect::default() }),
            ..LayerEffects::default()
        };
        let built = render_effects(&pixels, &effects).expect("render");
        assert_eq!(built.get(2, 2), [255, 255, 255, 255]);
        assert_eq!(built.get(1, 2), [255, 0, 0, 255]);
        assert_eq!(built.get(2, 1), [255, 0, 0, 255]);
        assert_eq!(built.get(0, 2)[3], 0);
        // The ring is a square reach: the diagonals are covered too.
        assert_eq!(built.get(1, 1), [255, 0, 0, 255]);
    }

    #[test]
    fn an_inside_stroke_covers_the_pixels_from_the_edge_inward() {
        let mut pixels = Rgba8Image::new(5, 5);
        for y in 0..5 {
            for x in 0..5 {
                pixels.set(x, y, [255, 255, 255, 255]);
            }
        }
        let effects = LayerEffects {
            stroke: Some(StrokeEffect { size: 1.0, red: 0.0, green: 0.0, blue: 0.0, opacity: 1.0, inside: true, ..StrokeEffect::default() }),
            ..LayerEffects::default()
        };
        let built = render_effects(&pixels, &effects).expect("render");
        assert_eq!(built.get(0, 0), [0, 0, 0, 255]);
        assert_eq!(built.get(4, 4), [0, 0, 0, 255]);
        assert_eq!(built.get(2, 2), [255, 255, 255, 255]);
    }

    #[test]
    fn a_glow_spreads_outside_without_touching_the_pixels_beneath_it() {
        let mut pixels = Rgba8Image::new(9, 9);
        pixels.set(4, 4, [255, 255, 255, 255]);
        let effects = LayerEffects {
            outer_glow: Some(OuterGlowEffect { size: 4.0, red: 0.0, green: 0.0, blue: 1.0, opacity: 1.0, ..OuterGlowEffect::default() }),
            ..LayerEffects::default()
        };
        let built = render_effects(&pixels, &effects).expect("render");
        assert_eq!(built.get(4, 4), [255, 255, 255, 255]);
        // The glow is brightest right beside the pixel and fades outwards.
        let near = alpha(&built, 3, 4);
        let far = alpha(&built, 0, 4);
        assert!(near > far, "{near} vs {far}");
        assert!(near > 0);
    }

    #[test]
    fn effects_render_through_the_mask_and_grow_the_grid() {
        let pixels = white(4, 4);
        let mut mask = Gray8Image::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                mask.set(x, y, if x < 2 { 255 } else { 0 });
            }
        }
        let effects = LayerEffects {
            color_overlay: Some(ColorOverlayEffect { red: 1.0, green: 0.0, blue: 0.0, opacity: 1.0, ..ColorOverlayEffect::default() }),
            ..LayerEffects::default()
        };
        let rendered = LayerEffectsRenderer::render(&pixels, Some(&mask), &effects).expect("render");
        assert_eq!(rendered.inset, 2.0);
        assert_eq!(rendered.image.width(), 8);
        assert_eq!(rendered.image.height(), 8);
        // Masked-out pixels carry nothing to overlay.
        assert_eq!(alpha(&rendered.image, 6, 4), 0);
        // Shown pixels are overlaid red.
        assert_eq!(rendered.image.get(3, 4), [255, 0, 0, 255]);
    }

    #[test]
    fn coverage_places_the_shapes_alpha_in_the_grid_it_is_given() {
        let mut pixels = Rgba8Image::new(4, 4);
        for y in 1..3 {
            for x in 1..3 {
                pixels.set(x, y, [255, 255, 255, 128]);
            }
        }
        let image = LayerEffectsRenderer::coverage(
            &pixels,
            Rect::new(2.0, 3.0, 4.0, 4.0),
            Size::new(10.0, 10.0),
            0.0,
        )
        .expect("coverage");
        assert_eq!(image.width(), 10);
        assert_eq!(image.height(), 10);
        // The shape's half alpha lands where the image was placed, and nowhere else.
        assert_eq!(image.get(2, 3), 0);
        assert_eq!(image.get(3, 4), 128);
        assert_eq!(image.get(4, 5), 128);
        assert_eq!(image.get(5, 5), 0);
        assert_eq!(image.get(6, 6), 0);
        assert_eq!(image.get(0, 0), 0);
    }

    #[test]
    fn a_stroke_ring_is_the_shape_grown_less_the_shape() {
        let mut pixels = Rgba8Image::new(5, 5);
        pixels.set(2, 2, [255, 255, 255, 255]);
        let stroke = StrokeEffect { size: 1.0, ..StrokeEffect::default() };
        let ring = LayerEffectsRenderer::ring_coverage(&pixels, Size::new(5.0, 5.0), &stroke)
            .expect("ring");
        assert_eq!(ring.get(1, 2), 255);
        assert_eq!(ring.get(2, 1), 255);
        assert_eq!(ring.get(1, 1), 255);
        assert_eq!(ring.get(2, 2), 0);
        assert_eq!(ring.get(0, 2), 0);
        // An inside stroke turns the ring inward: the ring covers only the shape's own edge.
        let inside = StrokeEffect { size: 1.0, inside: true, ..StrokeEffect::default() };
        let full = Rgba8Image::opaque(5, 5, [255, 255, 255, 255]);
        let ring = LayerEffectsRenderer::ring_coverage(&full, Size::new(5.0, 5.0), &inside)
            .expect("ring");
        assert_eq!(ring.get(0, 0), 255);
        assert_eq!(ring.get(2, 2), 0);
    }

    #[test]
    fn an_outer_glow_is_nothing_inside_the_shape() {
        let mut pixels = Rgba8Image::new(11, 11);
        for y in 4..7 {
            for x in 4..7 {
                pixels.set(x, y, [255, 255, 255, 255]);
            }
        }
        let glow = OuterGlowEffect { size: 4.0, ..OuterGlowEffect::default() };
        let coverage = LayerEffectsRenderer::outer_glow_coverage(
            &pixels,
            Rect::new(0.0, 0.0, 11.0, 11.0),
            Size::new(11.0, 11.0),
            &glow,
        )
        .expect("glow");
        assert_eq!(coverage.get(5, 5), 0);
        assert!(coverage.get(3, 5) > 0);
        assert!(coverage.get(3, 5) > coverage.get(0, 5));
    }

    #[test]
    fn an_inner_shadow_keeps_the_shape_and_removes_what_moved_over_it() {
        let pixels = white(9, 9);
        let shadow = InnerShadowEffect { distance: 2.0, blur: 0.0, angle: 90.0, ..InnerShadowEffect::default() };
        let coverage = LayerEffectsRenderer::inner_coverage(
            &pixels,
            Rect::new(0.0, 0.0, 9.0, 9.0),
            Size::new(9.0, 9.0),
            &shadow,
        )
        .expect("inner shadow");
        // The shadow falls downward: the top edge keeps the most, the bottom the least.
        assert!(coverage.get(4, 0) > coverage.get(4, 8));
        assert!(coverage.get(4, 8) == 0);
    }

    #[test]
    fn extreme_matches_a_brute_force_window() {
        let source: Vec<f32> = (0..35).map(|index| (index % 7) as f32 / 7.0).collect();
        let (width, height, reach) = (7usize, 5usize, 2i32);
        for smallest in [false, true] {
            let fast = LayerEffectsRenderer::extreme(&source, width, height, reach, smallest);
            let mut slow = vec![0.0f32; source.len()];
            for y in 0..height as i32 {
                for x in 0..width as i32 {
                    let mut best = if smallest { 1.0f32 } else { 0.0f32 };
                    for dy in -reach..=reach {
                        for dx in -reach..=reach {
                            let (sx, sy) = (x + dx, y + dy);
                            let value = if sx < 0 || sy < 0 || sx >= width as i32 || sy >= height as i32 {
                                0.0
                            } else {
                                source[sy as usize * width + sx as usize]
                            };
                            best = if smallest { best.min(value) } else { best.max(value) };
                        }
                    }
                    slow[y as usize * width + x as usize] = best;
                }
            }
            assert_eq!(fast, slow, "smallest: {smallest}");
        }
        // A degenerate window is a copy.
        assert_eq!(LayerEffectsRenderer::extreme(&source, width, height, 0, false), source);
        assert!(LayerEffectsRenderer::extreme(&[], 0, 0, 1, false).is_empty());
    }

    #[test]
    fn hidden_effects_render_nothing_at_all() {
        let pixels = white(3, 3);
        let effects = LayerEffects {
            stroke: Some(StrokeEffect { enabled: Some(false), ..StrokeEffect::default() }),
            inner_glow: Some(InnerGlowEffect { enabled: Some(false), ..InnerGlowEffect::default() }),
            inner_shadow: Some(InnerShadowEffect { enabled: Some(false), ..InnerShadowEffect::default() }),
            ..LayerEffects::default()
        };
        assert!(LayerEffectsRenderer::cached(&pixels, None, Some(&effects)).is_none());
    }

    /// An outer glow is cast omnidirectionally: the source keeps its own pixels and the glow shows
    /// on all four sides, symmetric about the shape.
    #[test]
    fn outer_glow_renders_omnidirectionally() {
        // 40 × 40 with a 20 × 20 solid white square in the middle (10, 10 to 30, 30).
        let image = square(40, 20, [255, 255, 255, 255]);
        let effects = LayerEffects {
            outer_glow: Some(OuterGlowEffect {
                size: 10.0,
                red: 0.0,
                green: 1.0,
                blue: 0.0,
                opacity: 1.0,
                ..OuterGlowEffect::default()
            }),
            ..LayerEffects::default()
        };
        let rendered = LayerEffectsRenderer::render(&image, None, &effects).expect("render");
        assert!(rendered.inset > 0.0);
        let inset = rendered.inset as usize;

        // The source's interior stays intact and sharp white.
        let center = straight(&rendered.image, inset + 20, inset + 20);
        assert!(center[3] > 0.95, "{center:?}");
        assert!(center[0] > 0.95 && center[1] > 0.95 && center[2] > 0.95, "{center:?}");

        // Pixels outside the square carry the green glow.
        let left = straight(&rendered.image, inset + 5, inset + 20);
        let right = straight(&rendered.image, inset + 35, inset + 20);
        let top = straight(&rendered.image, inset + 20, inset + 5);
        let bottom = straight(&rendered.image, inset + 20, inset + 35);
        for glow in [left, right, top, bottom] {
            assert!(glow[3] > 0.1, "{glow:?}");
            assert!(glow[1] > 0.8, "{glow:?}");
        }
        // Omnidirectional symmetry: the four points are the same distance out, so their alphas match.
        assert!((left[3] - right[3]).abs() < 0.05, "{left:?} vs {right:?}");
        assert!((top[3] - bottom[3]).abs() < 0.05, "{top:?} vs {bottom:?}");
        assert!((left[3] - top[3]).abs() < 0.05, "{left:?} vs {top:?}");
    }

    /// A bigger glow needs more room and reaches further; a higher opacity is brighter at the same
    /// distance.
    #[test]
    fn outer_glow_size_and_opacity_variations() {
        let image = square(40, 20, [255, 255, 255, 255]);
        let glow = |size: f64, opacity: f64| LayerEffects {
            outer_glow: Some(OuterGlowEffect {
                size,
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                opacity,
                ..OuterGlowEffect::default()
            }),
            ..LayerEffects::default()
        };
        let render =
            |effects: &LayerEffects| LayerEffectsRenderer::render(&image, None, effects).expect("render");

        // A size-20 glow grows the grid further than a size-4 one.
        let small = render(&glow(4.0, 1.0));
        let large = render(&glow(20.0, 1.0));
        assert!(large.inset > small.inset, "{} vs {}", large.inset, small.inset);
        // Sample 8 px outside the square, which starts at inset + 10.
        let small_far = alpha(&small.image, small.inset as usize + 2, small.inset as usize + 20);
        let large_far = alpha(&large.image, large.inset as usize + 2, large.inset as usize + 20);
        assert!(large_far > small_far, "a larger glow reaches further out: {large_far} vs {small_far}");

        // Opacity 1 is brighter than opacity 0.2 at the same point.
        let low = render(&glow(10.0, 0.2));
        let high = render(&glow(10.0, 1.0));
        let low_sample = alpha(&low.image, low.inset as usize + 5, low.inset as usize + 20);
        let high_sample = alpha(&high.image, high.inset as usize + 5, high.inset as usize + 20);
        assert!(high_sample > low_sample, "opacity 1 vs 0.2: {high_sample} vs {low_sample}");
    }

    /// The glow follows a glyph's outline: it surrounds the strokes and reaches into the concave
    /// corners, while the letters keep their own pixels.
    #[test]
    fn outer_glow_renders_around_text_glyphs() {
        // A T-shaped silhouette on a transparent background: the top bar spans x 15…45, y 15…23 and
        // the stem x 26…34, y 23…45, in a 60 × 60 image.
        let mut image = Rgba8Image::new(60, 60);
        for y in 15..23 {
            for x in 15..45 {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        for y in 23..45 {
            for x in 26..34 {
                image.set(x, y, [255, 255, 255, 255]);
            }
        }
        let effects = LayerEffects {
            outer_glow: Some(OuterGlowEffect {
                size: 8.0,
                red: 0.0,
                green: 1.0,
                blue: 1.0,
                opacity: 1.0,
                ..OuterGlowEffect::default()
            }),
            ..LayerEffects::default()
        };
        let rendered = LayerEffectsRenderer::render(&image, None, &effects).expect("render");
        let inset = rendered.inset as usize;

        // A point inside the stem stays white.
        let stem = straight(&rendered.image, inset + 30, inset + 30);
        assert!(stem[3] > 0.9, "{stem:?}");
        assert!(stem[0] > 0.9 && stem[1] > 0.9 && stem[2] > 0.9, "{stem:?}");

        // 3 px above the top bar of the T, outside the silhouette: cyan glow.
        let top = straight(&rendered.image, inset + 30, inset + 12);
        assert!(top[3] > 0.05, "{top:?}");
        assert!(top[1] > 0.5 && top[2] > 0.5, "{top:?}");

        // The concave notch under the left arm, outside the silhouette.
        let notch = straight(&rendered.image, inset + 20, inset + 27);
        assert!(notch[3] > 0.05, "{notch:?}");
        assert!(notch[1] > 0.5 && notch[2] > 0.5, "{notch:?}");
    }

    /// The glow sits between the stroke and the pixels, and the drop shadow falls behind them all.
    #[test]
    fn outer_glow_combined_with_stroke_and_drop_shadow() {
        let image = square(50, 20, [255, 255, 255, 255]);
        let effects = LayerEffects {
            stroke: Some(StrokeEffect {
                size: 3.0,
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                opacity: 1.0,
                inside: false,
                ..StrokeEffect::default()
            }),
            outer_glow: Some(OuterGlowEffect {
                size: 10.0,
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                opacity: 1.0,
                ..OuterGlowEffect::default()
            }),
            // Angle 180 casts the shadow at dx = +25, dy = 0 (a layer's pixels count y downward).
            shadow: Some(ShadowEffect {
                angle: 180.0,
                distance: 25.0,
                blur: 4.0,
                red: 0.0,
                green: 0.0,
                blue: 1.0,
                opacity: 1.0,
                ..ShadowEffect::default()
            }),
            ..LayerEffects::default()
        };
        let rendered = LayerEffectsRenderer::render(&image, None, &effects).expect("render");
        let inset = rendered.inset as usize;
        let center = (inset + 25, inset + 25);

        // The center of the source is still white.
        let middle = straight(&rendered.image, center.0, center.1);
        assert!(middle[0] > 0.9 && middle[1] > 0.9 && middle[2] > 0.9, "{middle:?}");

        // The stroke, 2 px outside the 20 × 20 square's border, is black and opaque.
        let stroke = straight(&rendered.image, inset + 13, center.1);
        assert!(stroke[3] > 0.9, "{stroke:?}");
        assert!(stroke[0] < 0.2 && stroke[1] < 0.2 && stroke[2] < 0.2, "{stroke:?}");

        // The glow, to the left of the square (away from the shadow), is red.
        let glow = straight(&rendered.image, inset + 10, center.1);
        assert!(glow[3] > 0.05, "{glow:?}");
        assert!(glow[0] > 0.6, "{glow:?}");

        // The shadow, 25 px right of the center, is blue.
        let shadow = straight(&rendered.image, center.0 + 25, center.1);
        assert!(shadow[3] > 0.1, "{shadow:?}");
        assert!(shadow[2] > 0.6, "{shadow:?}");
    }

    /// An inner glow stays inside the source: the grid does not grow, the padding stays clear, and
    /// the tint reaches inwards from the edge.
    #[test]
    fn inner_glow_renders_inside_source_without_bounds_expansion() {
        let source = square(40, 40, [0, 0, 0, 255]); // a black square filling the image
        let effects = LayerEffects {
            inner_glow: Some(InnerGlowEffect {
                size: 12.0,
                red: 1.0,
                green: 1.0,
                blue: 0.0, // yellow
                opacity: 1.0,
                ..InnerGlowEffect::default()
            }),
            ..LayerEffects::default()
        };
        // An inner glow needs no room: the margin stays the baseline.
        assert_eq!(LayerEffectsRenderer::margin(&effects), 2.0);

        let rendered = LayerEffectsRenderer::render(&source, None, &effects).expect("render");
        let inset = rendered.inset as usize;

        // The outer padding stays completely transparent.
        assert_eq!(straight(&rendered.image, 0, 0)[3], 0.0);

        // 2 px inside the edge the yellow glow tints the black square.
        let edge = straight(&rendered.image, inset + 2, inset + 20);
        assert!(edge[3] > 0.9, "{edge:?}");
        assert!(edge[0] > 0.3 && edge[1] > 0.3, "{edge:?}");

        // In the deep center the black source dominates.
        let center = straight(&rendered.image, inset + 20, inset + 20);
        assert!(center[0] < 0.2 && center[1] < 0.2, "{center:?}");
    }

    /// The inner glow follows a glyph's outline from the inside, over the letters' own pixels.
    #[test]
    fn inner_glow_renders_around_text_glyphs() {
        if !crate::text::fonts_available() {
            return;
        }
        // "O" at 72 pt in black, rasterized as the editor's text layers are.
        let style = LayerTextStyle {
            content: "O".to_string(),
            ..LayerTextStyle::default()
        };
        let text = crate::text::text_image(&style).expect("a valid style rasterizes");
        let effects = LayerEffects {
            inner_glow: Some(InnerGlowEffect {
                size: 8.0,
                red: 1.0,
                green: 0.0,
                blue: 0.0, // red
                opacity: 0.9,
                ..InnerGlowEffect::default()
            }),
            ..LayerEffects::default()
        };
        let rendered = LayerEffectsRenderer::render(&text, None, &effects).expect("render");
        assert!(rendered.image.width() >= text.width());
        assert!(rendered.image.height() >= text.height());
        let inset = rendered.inset as usize;

        // Outside the glyphs' bounds stays transparent.
        assert_eq!(straight(&rendered.image, 0, 0)[3], 0.0);

        // Somewhere on a letter the glow tints an opaque pixel red.
        let found = (inset..rendered.image.height() - inset).any(|y| {
            (inset..rendered.image.width() - inset).any(|x| {
                let pixel = straight(&rendered.image, x, y);
                pixel[3] > 0.5 && pixel[0] > 0.3
            })
        });
        assert!(found, "an inner glow tints the letters");
    }
}
