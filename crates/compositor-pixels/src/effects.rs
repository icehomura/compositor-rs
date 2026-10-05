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

use compositor_core::geom::{CGFloat, Point, Rect};
use compositor_core::layer_effects::LayerEffects;
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

/// `ExportError.render`'s message.
fn render_failed() -> CoreError {
    CoreError::Message("The canvas could not be rendered. Try a smaller canvas.".to_string())
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

/// One separable Gaussian pass over the rows, `effects_blur_rows`: weights normalized by their own
/// sum, sampling clamped to the edge.
fn blur_rows(source: &[f32], width: usize, height: usize, sigma: f32, radius: i32) -> Vec<f32> {
    let mut weights = Vec::with_capacity((radius * 2 + 1) as usize);
    let denominator = 2.0 * sigma * sigma;
    for offset in -radius..=radius {
        weights.push((-(offset * offset) as f32 / denominator).exp());
    }
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
    let mut weights = Vec::with_capacity((radius * 2 + 1) as usize);
    let denominator = 2.0 * sigma * sigma;
    for offset in -radius..=radius {
        weights.push((-(offset * offset) as f32 / denominator).exp());
    }
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

// MARK: - The cache

struct CacheEntry {
    image: *const Rgba8Image,
    mask: *const Gray8Image,
    effects: LayerEffects,
    result: EffectsRender,
}

/// SAFETY: the keys are only ever compared as addresses while the owning `Arc`s are alive, exactly
/// as the Swift cache compares `CGImage` identities.
unsafe impl Send for CacheEntry {}

static CACHE: LazyLock<Mutex<Vec<CacheEntry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// The cost of one entry, as the Swift's cache measures it: the result plus what it was made from.
fn entry_cost(entry: &CacheEntry) -> usize {
    entry.result.image.size_bytes() + unsafe { (&*entry.image).size_bytes() }
        + entry
            .mask
            .is_null()
            .then_some(0)
            .unwrap_or_else(|| unsafe { (&*entry.mask).pixel_count() })
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
        let cost = made.image.size_bytes() + image.size_bytes() + mask.map_or(0, |mask| mask.pixel_count());
        if cost <= CACHE_BUDGET_BYTES {
            cache.push(CacheEntry {
                image: key,
                mask: mask_key,
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

/// The render the failures above are reported as: the same message, plus the empty result the
/// caller falls back from.
pub fn render_or_empty(
    image: &Rgba8Image,
    mask: Option<&Gray8Image>,
    effects: &LayerEffects,
) -> std::result::Result<EffectsRender, CoreError> {
    let rendered = LayerEffectsRenderer::render(image, mask, effects)?;
    if rendered.image.is_empty() {
        return Err(render_failed());
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::layer_effects::{
        ColorOverlayEffect, InnerGlowEffect, InnerShadowEffect, OuterGlowEffect, ShadowEffect,
        StrokeEffect,
    };

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
        let grown = LayerEffectsRenderer::placed(&transform, &image, 10.0);
        // 40 * 40 / 20 = 80 wide, 20 * 20 / 0… the heights grow the same way: 20 * 40 / 20 = 40.
        assert_eq!(grown.size.width, 80.0);
        assert_eq!(grown.size.height, 40.0);
        assert_eq!(grown.center().x, transform.center().x);
        assert_eq!(grown.center().y, transform.center().y);
        assert_eq!(grown.origin.x, 120.0 - 40.0);
        assert_eq!(grown.origin.y, 60.0 - 20.0);
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
        let source = vec![0.0, 1.0, 0.0, 0.0];
        assert_eq!(shift(&source, 2, 2, 0.0, 0.0), source);
        let moved = shift(&source, 2, 2, 1.0, 0.0);
        assert_eq!(moved, vec![0.0, 0.0, 0.0, 1.0]);
        // Half a pixel between the two: the sample lies between the two columns.
        let half = shift(&source, 2, 2, 0.5, 0.0);
        assert!((half[1] - 0.5).abs() < 1e-6, "{half:?}");
    }

    #[test]
    fn blur_normalizes_by_its_own_weights() {
        let source = vec![1.0; 16];
        let blurred = blur(&source, 4, 4, 1.0);
        for value in blurred {
            assert!((value - 1.0).abs() < 1e-5, "{value}");
        }
        // A single bright pixel spreads symmetrically.
        let mut dot = vec![0.0f32; 25];
        dot[12] = 1.0;
        let spread = blur(&dot, 5, 5, 1.0);
        assert!((spread[12] - spread[7]).abs() < 1e-6);
        assert!((spread[12] - spread[11]).abs() < 1e-6);
        assert!(spread[12] > spread[0]);
        assert!(spread[0] > 0.0);
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
}
