//! Port of `references/Compositor/Compositor/Rendering/AdjustPixels.{h,c}`: adjustment kernels that work
//! directly on premultiplied RGBA8 buffers — gradient map, film grain, black & white, color balance, and
//! the Camera Raw stages (Light and Color, clip overlays, curve/color mixer/color grading, effects,
//! tonal contrast, detail, sharpen mask overlay, optics and calibration).
//!
//! The C signatures are kept (4 bytes per pixel, `stride` bytes per row) and every step is evaluated in
//! the same precision and order as the original, so results can be diffed against the C line for line.

use crate::lens_pixels::lens_distort;

/// Gradient Map on premultiplied RGBA pixels (4 bytes per pixel, `stride` bytes per row): each pixel's
/// luminance picks a color from `table` (256 × 3 straight sRGB bytes, darkest first). Alpha is kept and
/// fully transparent pixels are left alone.
pub fn adjust_gradient_map(rgba: &mut [u8], width: usize, height: usize, stride: usize, table: &[u8]) {
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = p[3] as u32;
            if a == 0 {
                continue;
            }
            let mut r = p[0] as u32;
            let mut g = p[1] as u32;
            let mut b = p[2] as u32;
            if a < 255 {
                r = (r * 255 + a / 2) / a;
                g = (g * 255 + a / 2) / a;
                b = (b * 255 + a / 2) / a;
                if r > 255 {
                    r = 255;
                }
                if g > 255 {
                    g = 255;
                }
                if b > 255 {
                    b = 255;
                }
            }
            let level = (2126 * r + 7152 * g + 722 * b + 5000) / 10000;
            let level = if level > 255 { 255 } else { level } as usize;
            let color = &table[level * 3..level * 3 + 3];
            p[0] = ((color[0] as u32 * a + 127) / 255) as u8;
            p[1] = ((color[1] as u32 * a + 127) / 255) as u8;
            p[2] = ((color[2] as u32 * a + 127) / 255) as u8;
        }
    }
}

fn mix32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    x
}

/// A value in −1…1 for an integer lattice point, fixed by the point and the seed. Two uniform halves
/// summed give a triangular spread, closer to film grain than flat noise.
fn lattice(ix: i64, iy: i64, seed: u32) -> f32 {
    let h = mix32((ix as u32).wrapping_mul(0x9E3779B1) ^ mix32((iy as u32).wrapping_mul(0x85EBCA77) ^ seed));
    (h & 0xFFFF) as f32 / 65535.0f32 + (h >> 16) as f32 / 65535.0f32 - 1.0f32
}

/// Smooth seeded noise whose features follow `scale` document pixels. Keeping both the broad and
/// detailed patterns relative to the requested grain size makes Size remain visible at any Roughness.
fn grain_field(u: f64, v: f64, scale: f64, seed: u32) -> f32 {
    let cell_x = (u / scale).floor();
    let cell_y = (v / scale).floor();
    let mut tx = (u / scale - cell_x) as f32;
    let mut ty = (v / scale - cell_y) as f32;
    tx = tx * tx * (3.0f32 - 2.0f32 * tx);
    ty = ty * ty * (3.0f32 - 2.0f32 * ty);
    let ix = cell_x as i64;
    let iy = cell_y as i64;
    let n00 = lattice(ix, iy, seed);
    let n10 = lattice(ix + 1, iy, seed);
    let n01 = lattice(ix, iy + 1, seed);
    let n11 = lattice(ix + 1, iy + 1, seed);
    let top = n00 + (n10 - n00) * tx;
    let bottom = n01 + (n11 - n01) * tx;
    // Blending neighboring lattice values narrows the spread; restore approximately its original range.
    (top + (bottom - top) * ty) * 1.6f32
}

fn clamp255(value: f32) -> f32 {
    if value < 0.0 {
        0.0
    } else if value > 255.0 {
        255.0
    } else {
        value
    }
}

/// Film grain on premultiplied RGBA pixels: the same brightness change on all three channels, strongest
/// in the midtones. `amount` is 0–100, `size` the grain's scale in document units, and `roughness`
/// (0–100) adds smaller irregular particles whose scale remains relative to `size`. Pixel (x, y) sits at
/// (originX + (x + 0.5) × unitsPerPixel, originY + (y + 0.5) × unitsPerPixel), and its grain depends
/// only on that position and `seed`, so a piece of an image gets the same grain as that part of the
/// whole.
pub fn adjust_grain(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    amount: f64,
    size: f64,
    roughness: f64,
    seed: u32,
    origin_x: f64,
    origin_y: f64,
    units_per_pixel: f64,
) {
    if !(amount > 0.0) || !(units_per_pixel > 0.0) {
        return;
    }
    let size = if size > 0.0 { size } else { 1.0 };
    let strength = (if amount > 100.0 { 1.0 } else { amount / 100.0 }) as f32 * 0.35f32 * 255.0f32;
    let rough = (if roughness < 0.0 {
        0.0
    } else if roughness > 100.0 {
        1.0
    } else {
        roughness / 100.0
    }) as f32;
    let fine_seed = mix32(seed ^ 0xA511E9B3);
    // Roughness adds smaller, less regular particles, as in Photoshop, but their size remains
    // proportional to the Size control instead of collapsing to fixed one-pixel noise.
    let detail_size = 0.5f64.max(size * 0.35);
    for y in 0..height {
        let v = origin_y + (y as f64 + 0.5) * units_per_pixel;
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = p[3] as u32;
            if a == 0 {
                continue;
            }
            let u = origin_x + (x as f64 + 0.5) * units_per_pixel;
            let smooth = grain_field(u, v, size, seed);
            let fine = grain_field(u, v, detail_size, fine_seed);
            let noise = smooth + (fine - smooth) * rough;
            let unpremultiply = if a == 255 { 1.0f32 } else { 255.0f32 / a as f32 };
            let r = p[0] as f32 * unpremultiply;
            let g = p[1] as f32 * unpremultiply;
            let b = p[2] as f32 * unpremultiply;
            let mut level = (0.2126f32 * r + 0.7152f32 * g + 0.0722f32 * b) / 255.0f32;
            if level > 1.0 {
                level = 1.0;
            }
            // Film grain shows most in the midtones.
            let delta = noise * strength * (0.4f32 + 2.4f32 * level * (1.0f32 - level));
            let coverage = a as f32 / 255.0f32;
            p[0] = (clamp255(r + delta) * coverage + 0.5f32) as u8;
            p[1] = (clamp255(g + delta) * coverage + 0.5f32) as u8;
            p[2] = (clamp255(b + delta) * coverage + 0.5f32) as u8;
        }
    }
}

/// Black & White on premultiplied RGBA pixels, the way Photoshop's is: a color is split into the gray
/// it contains, the secondary (cyan/magenta/yellow) between its two brightest channels, and the primary
/// (red/green/blue) of its brightest, and each of those six ranges has its own weight. `weights` is six
/// floats in the order red, yellow, green, cyan, blue, magenta, as fractions (Photoshop's 40% is 0.4).
/// Pure red at the default 40% comes out 40% gray, as it does there. With `tint`, the result is colored
/// at `tintHue` degrees and `tintSaturation` (0–1) while keeping that gray as its lightness.
///
/// Which of the six ranges a color's primary and secondary fall in, and how much of each it holds.
/// A color is min(r,g,b) of gray, plus (mid-min) of the secondary between its two brightest channels,
/// plus (max-mid) of the primary of its brightest — so the weights below are exactly Photoshop's.
pub fn adjust_black_white(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    weights: &[f32],
    tint: i32,
    tint_hue: f64,
    tint_saturation: f64,
) {
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f32;
            if alpha == 0.0 {
                continue;
            }
            let mut r = p[0] as f32 * 255.0f32 / alpha;
            let mut g = p[1] as f32 * 255.0f32 / alpha;
            let mut b = p[2] as f32 * 255.0f32 / alpha;
            r = 255.0f32.min(r) / 255.0f32;
            g = 255.0f32.min(g) / 255.0f32;
            b = 255.0f32.min(b) / 255.0f32;
            let mx = r.max(g.max(b));
            let mn = r.min(g.min(b));
            let md = r + g + b - mx - mn;
            // weights: 0 red, 1 yellow, 2 green, 3 cyan, 4 blue, 5 magenta
            let (primary, secondary) = if mx == r {
                (0usize, if g >= b { 1usize } else { 5 })
            } else if mx == g {
                (2, if r >= b { 1 } else { 3 })
            } else {
                (4, if g >= r { 3 } else { 5 })
            };
            let mut gray = mn + (md - mn) * weights[secondary] + (mx - md) * weights[primary];
            gray = 1.0f32.min(0.0f32.max(gray));
            let mut out_r = gray;
            let mut out_g = gray;
            let mut out_b = gray;
            if tint != 0 && tint_saturation > 0.0 {
                // The gray becomes the lightness of a color at the chosen hue.
                let c = (1.0 - (2.0 * gray as f64 - 1.0).abs()) * tint_saturation;
                let hp = (tint_hue % 360.0) / 60.0;
                let xx = c * (1.0 - ((hp % 2.0) - 1.0).abs());
                let (mut r1, mut g1, mut b1) = (0.0f64, 0.0f64, 0.0f64);
                if hp < 1.0 {
                    r1 = c;
                    g1 = xx;
                } else if hp < 2.0 {
                    r1 = xx;
                    g1 = c;
                } else if hp < 3.0 {
                    g1 = c;
                    b1 = xx;
                } else if hp < 4.0 {
                    g1 = xx;
                    b1 = c;
                } else if hp < 5.0 {
                    r1 = xx;
                    b1 = c;
                } else {
                    r1 = c;
                    b1 = xx;
                }
                let m = gray as f64 - c / 2.0;
                out_r = 1.0f64.min(0.0f64.max(r1 + m)) as f32;
                out_g = 1.0f64.min(0.0f64.max(g1 + m)) as f32;
                out_b = 1.0f64.min(0.0f64.max(b1 + m)) as f32;
            }
            p[0] = alpha.min(0.0f32.max((out_r * alpha).round())) as u8;
            p[1] = alpha.min(0.0f32.max((out_g * alpha).round())) as u8;
            p[2] = alpha.min(0.0f32.max((out_b * alpha).round())) as u8;
        }
    }
}

/// How much a tone belongs to the shadows, midtones and highlights: three overlapping curves that sum
/// to about one across the range, so a shift fades in and out rather than banding at a threshold.
fn tonal_weights(v: f32) -> (f32, f32, f32) {
    const A: f32 = 0.25;
    const B: f32 = 0.333;
    const SCALE: f32 = 0.7;
    let mut s = (v - B) / -A + 0.5f32;
    let mut h = (v + B - 1.0f32) / A + 0.5f32;
    s = 1.0f32.min(0.0f32.max(s));
    h = 1.0f32.min(0.0f32.max(h));
    let m1 = 1.0f32.min(0.0f32.max((v - B) / A + 0.5f32));
    let m2 = 1.0f32.min(0.0f32.max((v + B - 1.0f32) / -A + 0.5f32));
    (s * SCALE, m1 * m2 * SCALE, h * SCALE)
}

/// Color Balance on premultiplied RGBA pixels. `shadows`, `midtones` and `highlights` are each three
/// floats — cyan/red, magenta/green, yellow/blue — from -1 to 1 (Photoshop's -100 to 100). Each pixel is
/// shifted by however much it belongs to each tonal range, and with `preserveLuminosity` its original
/// brightness is put back afterwards, so only the color moves.
pub fn adjust_color_balance(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    shadows: &[f32],
    midtones: &[f32],
    highlights: &[f32],
    preserve_luminosity: i32,
) {
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f32;
            if alpha == 0.0 {
                continue;
            }
            let mut c = [0.0f32; 3];
            for i in 0..3 {
                c[i] = 255.0f32.min(p[i] as f32 * 255.0f32 / alpha) / 255.0f32;
            }
            let before = 0.299f32 * c[0] + 0.587f32 * c[1] + 0.114f32 * c[2];
            for i in 0..3 {
                let (s, m, h) = tonal_weights(c[i]);
                c[i] += shadows[i] * s + midtones[i] * m + highlights[i] * h;
                c[i] = 1.0f32.min(0.0f32.max(c[i]));
            }
            if preserve_luminosity != 0 {
                let after = 0.299f32 * c[0] + 0.587f32 * c[1] + 0.114f32 * c[2];
                if after > 0.0001f32 {
                    let ratio = before / after;
                    for i in 0..3 {
                        c[i] = 1.0f32.min(0.0f32.max(c[i] * ratio));
                    }
                }
            }
            for i in 0..3 {
                p[i] = alpha.min(0.0f32.max((c[i] * alpha).round())) as u8;
            }
        }
    }
}

fn camera_clamp(value: f64) -> f64 {
    if value < 0.0 {
        return 0.0;
    }
    if value > 1.0 {
        return 1.0;
    }
    value
}

fn srgb_to_linear(encoded: f64) -> f64 {
    if encoded <= 0.04045 {
        return encoded / 12.92;
    }
    ((encoded + 0.055) / 1.055).powf(2.4)
}

fn linear_to_srgb(linear: f64) -> f64 {
    if linear <= 0.0 {
        return 0.0;
    }
    if linear >= 1.0 {
        return 1.0;
    }
    if linear <= 0.0031308 {
        return linear * 12.92;
    }
    1.055 * linear.powf(1.0 / 2.4) - 0.055
}

fn rec709(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Moves `r`, `g`, `b` so their Rec. 709 luminance becomes `target`, keeping the hue. Pure black cannot
/// be scaled, so a lift paints neutral light of that luminance.
fn scale_luminance(r: &mut f64, g: &mut f64, b: &mut f64, target: f64) {
    let target = camera_clamp(target);
    let y = rec709(*r, *g, *b);
    if (target - y).abs() < 1e-8 {
        return;
    }
    if y < 1e-8 {
        if target > y {
            *r = target;
            *g = target;
            *b = target;
        }
        return;
    }
    let scale = target / y;
    *r = camera_clamp(*r * scale);
    *g = camera_clamp(*g * scale);
    *b = camera_clamp(*b * scale);
}

fn tone_highlights(y: f64, amount: f64) -> f64 {
    let t = camera_clamp((y - 0.5) / 0.5);
    let weight = t * t;
    if amount >= 0.0 {
        return camera_clamp(y + amount * weight * (1.0 - y));
    }
    camera_clamp(y + amount * weight * (y - 0.5))
}

fn tone_shadows(y: f64, amount: f64) -> f64 {
    let t = camera_clamp((0.5 - y) / 0.5);
    let weight = t * t;
    if amount >= 0.0 {
        return camera_clamp(y + amount * weight * (0.5 - y));
    }
    camera_clamp(y + amount * weight * y)
}

/// The top quarter is the white point: +1 maps 0.875 to 1, −1 pulls everything above 0.75 down to 0.75.
fn tone_whites(y: f64, amount: f64) -> f64 {
    if y <= 0.75 {
        return y;
    }
    camera_clamp(0.75 + (y - 0.75) * (1.0 + amount))
}

/// The bottom quarter is the black point. Negative amounts crush toward 0; positive ones lift toward 0.25.
fn tone_blacks(y: f64, amount: f64) -> f64 {
    if y >= 0.25 {
        return y;
    }
    camera_clamp(0.25 + (y - 0.25) * (1.0 - amount))
}

fn vibrance_and_saturation(r: &mut f64, g: &mut f64, b: &mut f64, vibrance: f64, saturation: f64) {
    let lum = rec709(*r, *g, *b);
    let maxc = (*r).max((*g).max(*b));
    let minc = (*r).min((*g).min(*b));
    let chroma = maxc - minc;
    let sat = if maxc <= 1e-8 { 0.0 } else { chroma / maxc };
    let mut hue = 0.0;
    if chroma > 1e-8 {
        if *r >= *g && *r >= *b {
            hue = 60.0 * ((*g - *b) / chroma % 6.0);
        } else if *g >= *r && *g >= *b {
            hue = 60.0 * ((*b - *r) / chroma + 2.0);
        } else {
            hue = 60.0 * ((*r - *g) / chroma + 4.0);
        }
        if hue < 0.0 {
            hue += 360.0;
        }
    }
    let mut skin = 0.0;
    if hue >= 10.0 && hue <= 50.0 {
        skin = if hue <= 30.0 { (hue - 10.0) / 20.0 } else { (50.0 - hue) / 20.0 };
        skin *= camera_clamp((sat - 0.15) / 0.35);
    }
    let mut amount = vibrance * (1.0 - sat);
    if vibrance > 0.0 {
        amount *= 1.0 - 0.7 * skin;
    }
    let mut factor = 1.0 + amount;
    *r = camera_clamp(lum + (*r - lum) * factor);
    *g = camera_clamp(lum + (*g - lum) * factor);
    *b = camera_clamp(lum + (*b - lum) * factor);
    let lum = rec709(*r, *g, *b);
    factor = 1.0 + saturation;
    *r = camera_clamp(lum + (*r - lum) * factor);
    *g = camera_clamp(lum + (*g - lum) * factor);
    *b = camera_clamp(lum + (*b - lum) * factor);
}

/// Writes one straight color into premultiplied storage, each channel scaled by `alpha` and clamped to
/// it.
fn write_premultiplied(p: &mut [u8], r: f64, g: f64, b: f64, alpha: f64) {
    p[0] = alpha.min(0.0f64.max((r * alpha).round())) as u8;
    p[1] = alpha.min(0.0f64.max((g * alpha).round())) as u8;
    p[2] = alpha.min(0.0f64.max((b * alpha).round())) as u8;
}

/// Camera Raw's Light and Color groups on premultiplied RGBA pixels, in this order: white balance
/// (the three channel gains), exposure in stops of linear light, contrast about mid gray, highlights,
/// shadows, whites, blacks, vibrance, then saturation. Temperature and tint are relative, so the gains
/// are computed by the caller. Amounts are Camera Raw's own ranges (exposure −5…5, the rest −100…100).
/// `clipping` 0 renders the grade; 1 replaces it with a highlight-clip view (clipped channels lit on
/// black); 2 replaces it with a shadow-clip view (clipped channels dark on white). Alpha is kept.
pub fn adjust_camera_raw(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    red_gain: f64,
    green_gain: f64,
    blue_gain: f64,
    exposure: f64,
    contrast: f64,
    highlights: f64,
    shadows: f64,
    whites: f64,
    blacks: f64,
    vibrance: f64,
    saturation: f64,
    clipping: i32,
) {
    let light = exposure.exp2();
    let contrast_scale = 1.0 + contrast / 100.0;
    let highlight_amount = highlights / 100.0;
    let shadow_amount = shadows / 100.0;
    let white_amount = whites / 100.0;
    let black_amount = blacks / 100.0;
    let vibrance_amount = vibrance / 100.0;
    let saturation_amount = saturation / 100.0;
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let mut r = 255.0f64.min(p[0] as f64 * 255.0 / alpha) / 255.0;
            let mut g = 255.0f64.min(p[1] as f64 * 255.0 / alpha) / 255.0;
            let mut b = 255.0f64.min(p[2] as f64 * 255.0 / alpha) / 255.0;
            r = camera_clamp(srgb_to_linear(r) * red_gain * light);
            g = camera_clamp(srgb_to_linear(g) * green_gain * light);
            b = camera_clamp(srgb_to_linear(b) * blue_gain * light);
            r = camera_clamp(0.5 + (linear_to_srgb(r) - 0.5) * contrast_scale);
            g = camera_clamp(0.5 + (linear_to_srgb(g) - 0.5) * contrast_scale);
            b = camera_clamp(0.5 + (linear_to_srgb(b) - 0.5) * contrast_scale);
            let target = tone_highlights(rec709(r, g, b), highlight_amount);
            scale_luminance(&mut r, &mut g, &mut b, target);
            let target = tone_shadows(rec709(r, g, b), shadow_amount);
            scale_luminance(&mut r, &mut g, &mut b, target);
            let target = tone_whites(rec709(r, g, b), white_amount);
            scale_luminance(&mut r, &mut g, &mut b, target);
            let target = tone_blacks(rec709(r, g, b), black_amount);
            scale_luminance(&mut r, &mut g, &mut b, target);
            vibrance_and_saturation(&mut r, &mut g, &mut b, vibrance_amount, saturation_amount);
            if clipping == 1 {
                let rc = r >= 254.5 / 255.0;
                let gc = g >= 254.5 / 255.0;
                let bc = b >= 254.5 / 255.0;
                r = if rc { 1.0 } else { 0.0 };
                g = if gc { 1.0 } else { 0.0 };
                b = if bc { 1.0 } else { 0.0 };
            } else if clipping == 2 {
                let rc = r <= 0.5 / 255.0;
                let gc = g <= 0.5 / 255.0;
                let bc = b <= 0.5 / 255.0;
                if rc || gc || bc {
                    r = if rc { 0.0 } else { 1.0 };
                    g = if gc { 0.0 } else { 1.0 };
                    b = if bc { 0.0 } else { 1.0 };
                } else {
                    r = 1.0;
                    g = 1.0;
                    b = 1.0;
                }
            }
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

fn clamped_index(index: i32, limit: usize) -> usize {
    if index < 0 {
        return 0;
    }
    if index as usize >= limit {
        return limit - 1;
    }
    index as usize
}

/// Edge-clamped box blur. `dst` may not alias `src`.
fn box_blur_plane(src: &[f32], dst: &mut [f32], width: usize, height: usize, radius: i32) {
    if radius < 1 {
        dst[..width * height].copy_from_slice(&src[..width * height]);
        return;
    }
    let mut temp = vec![0.0f32; width * height];
    let window = radius * 2 + 1;
    for y in 0..height {
        let mut sum = 0.0f64;
        for k in -radius..=radius {
            sum += src[y * width + clamped_index(k, width)] as f64;
        }
        for x in 0..width {
            temp[y * width + x] = (sum / window as f64) as f32;
            sum += src[y * width + clamped_index(x as i32 + radius + 1, width)] as f64;
            sum -= src[y * width + clamped_index(x as i32 - radius, width)] as f64;
        }
    }
    for x in 0..width {
        let mut sum = 0.0f64;
        for k in -radius..=radius {
            sum += temp[clamped_index(k, height) * width + x] as f64;
        }
        for y in 0..height {
            dst[y * width + x] = (sum / window as f64) as f32;
            sum += temp[clamped_index(y as i32 + radius + 1, height) * width + x] as f64;
            sum -= temp[clamped_index(y as i32 - radius, height) * width + x] as f64;
        }
    }
}

fn effects_radius(base: f64, scale: f64) -> i32 {
    let mut radius = base * (if scale > 0.0 { scale } else { 1.0 });
    if radius < 1.0 {
        radius = 1.0;
    }
    if radius > 64.0 {
        radius = 64.0;
    }
    radius.round() as i32
}

fn effects_dehaze(r: &mut f64, g: &mut f64, b: &mut f64, amount: f64) {
    let d = amount / 100.0;
    let y = rec709(*r, *g, *b);
    let contrast = 1.0 + 0.8 * d;
    let pivot = 0.45 - 0.1 * (if d > 0.0 { d } else { 0.0 });
    let mut y2 = camera_clamp(pivot + (y - 0.45) * contrast);
    if d < 0.0 {
        y2 = camera_clamp(y2 + (-d) * (1.0 - y2) * 0.45);
    } else {
        y2 = camera_clamp(y2 - d * 0.0f64.max(0.4 - y2));
    }
    scale_luminance(r, g, b, y2);
    let y2 = rec709(*r, *g, *b);
    let sat = 1.0 + 0.7 * d;
    *r = camera_clamp(y2 + (*r - y2) * sat);
    *g = camera_clamp(y2 + (*g - y2) * sat);
    *b = camera_clamp(y2 + (*b - y2) * sat);
}

/// The vignette's strength at a point `px`, `py` of a `width` × `height` frame (0 at its middle, 1 past its edges).
fn vignette_mask_at(
    px: f64,
    py: f64,
    width: f64,
    height: f64,
    midpoint: f64,
    roundness: f64,
    feather: f64,
) -> f64 {
    let nx = px / width * 2.0 - 1.0;
    let ny = py / height * 2.0 - 1.0;
    let square = nx.abs().max(ny.abs());
    let circle = nx.hypot(ny) / 2.0f64.sqrt();
    let shape = (1.0 - roundness / 100.0) * 0.5;
    let dist = circle + (square - circle) * shape;
    let start = (midpoint / 100.0) * 0.85;
    let mut soft = feather / 100.0;
    if soft < 0.05 {
        soft = 0.05;
    }
    let t = camera_clamp((dist - start) / soft);
    t * t * (3.0 - 2.0 * t)
}

fn vignette_mask(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    midpoint: f64,
    roundness: f64,
    feather: f64,
) -> f64 {
    vignette_mask_at(
        x as f64 + 0.5,
        y as f64 + 0.5,
        width as f64,
        height as f64,
        midpoint,
        roundness,
        feather,
    )
}

fn effects_vignette(
    r: &mut f64,
    g: &mut f64,
    b: &mut f64,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    amount: f64,
    midpoint: f64,
    roundness: f64,
    feather: f64,
    highlights: f64,
    style: i32,
) {
    if amount == 0.0 || width == 0 || height == 0 {
        return;
    }
    let mask = vignette_mask(x, y, width, height, midpoint, roundness, feather);
    let mut effect = (amount / 100.0) * mask;
    // Highlight Priority eases a darkening vignette off bright pixels. The other styles do not.
    if effect < 0.0 && style == 0 {
        let bright = camera_clamp((rec709(*r, *g, *b) - 0.45) / 0.55);
        effect *= 1.0 - (highlights / 100.0) * bright;
    }
    if effect < 0.0 {
        let factor = 1.0 + effect;
        *r *= factor;
        *g *= factor;
        *b *= factor;
    } else if effect > 0.0 {
        *r = *r + (1.0 - *r) * effect;
        *g = *g + (1.0 - *g) * effect;
        *b = *b + (1.0 - *b) * effect;
    }
    if style == 1 && mask > 0.0 {
        let lum = rec709(*r, *g, *b);
        let sat = 1.0 - 0.75 * mask * (amount / 100.0).abs();
        *r = camera_clamp(lum + (*r - lum) * sat);
        *g = camera_clamp(lum + (*g - lum) * sat);
        *b = camera_clamp(lum + (*b - lum) * sat);
    }
}

/// Standalone Vignette: blends straight sRGB toward the selected edge color using Camera Raw's
/// falloff shape and Highlight Priority. Preserves the source alpha and premultiplied storage.
/// The vignette is shaped to the frame (in the image's pixels). With fillsClear it paints transparent
/// pixels too; without, it recolors only the pixels that are there.
pub fn adjust_colored_vignette(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    frame_x: f64,
    frame_y: f64,
    frame_width: f64,
    frame_height: f64,
    fills_clear: i32,
    amount: f64,
    midpoint: f64,
    roundness: f64,
    feather: f64,
    highlights: f64,
    red: f64,
    green: f64,
    blue: f64,
) {
    if amount <= 0.0 || width == 0 || height == 0 || frame_width <= 0.0 || frame_height <= 0.0 {
        return;
    }
    let strength = camera_clamp(amount / 100.0);
    let red = camera_clamp(red);
    let green = camera_clamp(green);
    let blue = camera_clamp(blue);
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            if p[3] == 0 && fills_clear == 0 {
                continue;
            }
            let mask = vignette_mask_at(
                x as f64 + 0.5 - frame_x,
                y as f64 + 0.5 - frame_y,
                frame_width,
                frame_height,
                midpoint,
                roundness,
                feather,
            );
            if mask <= 0.0 {
                continue;
            }
            let alpha = p[3] as f64 / 255.0;
            let mut r = 0.0;
            let mut g = 0.0;
            let mut b = 0.0;
            let mut bright = 0.0;
            if p[3] != 0 {
                r = 1.0f64.min(p[0] as f64 / p[3] as f64);
                g = 1.0f64.min(p[1] as f64 / p[3] as f64);
                b = 1.0f64.min(p[2] as f64 / p[3] as f64);
                bright = camera_clamp((rec709(r, g, b) - 0.45) / 0.55);
            }
            let effect = strength * mask * (1.0 - (highlights / 100.0) * bright);
            if fills_clear == 0 {
                // Only the pixels that are there change color; their coverage stays as it was.
                write_premultiplied(
                    p,
                    r + (red - r) * effect,
                    g + (green - g) * effect,
                    b + (blue - b) * effect,
                    p[3] as f64,
                );
                continue;
            }
            // The color painted over the pixel at `effect`: an opaque pixel moves toward it, a clear one takes it on.
            let out = alpha + effect * (1.0 - alpha);
            if out <= 0.0 {
                continue;
            }
            r = (red * effect + r * alpha * (1.0 - effect)) / out;
            g = (green * effect + g * alpha * (1.0 - effect)) / out;
            b = (blue * effect + b * alpha * (1.0 - effect)) / out;
            p[3] = 255.0f64.min((out * 255.0).round()) as u8;
            write_premultiplied(p, r, g, b, p[3] as f64);
        }
    }
}

/// Blue over clipped shadows and red over clipped highlights, on top of the grade. Preview only.
pub fn adjust_camera_raw_clip_overlay(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    shadows: i32,
    highlights: i32,
) {
    if shadows == 0 && highlights == 0 {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let mut r = 1.0f64.min(p[0] as f64 / alpha);
            let mut g = 1.0f64.min(p[1] as f64 / alpha);
            let mut b = 1.0f64.min(p[2] as f64 / alpha);
            if shadows != 0 && (r <= 0.5 / 255.0 || g <= 0.5 / 255.0 || b <= 0.5 / 255.0) {
                r *= 0.35;
                g *= 0.35;
                b = b * 0.35 + 0.65;
            }
            if highlights != 0 && (r >= 254.5 / 255.0 || g >= 254.5 / 255.0 || b >= 254.5 / 255.0) {
                r = r * 0.35 + 0.65;
                g *= 0.35;
                b *= 0.35;
            }
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

/// Camera Raw Effects after Light and Color. Texture is a fine local contrast, Clarity a broader one.
/// Dehaze raises contrast and saturation when positive and lifts the shadows when negative. Glow, its
/// range, spread and warmth do nothing until `glow` is above zero: styles are 0 diffusion, 1 bloom,
/// 2 halation. Vignette styles are 0 highlight priority, 1 color priority, 2 paint overlay; Highlights
/// protects bright pixels only while the amount darkens. `scale` is preview pixels per layer pixel, so
/// the radii match a full-size render. Grain is applied separately. Alpha is kept.
pub fn adjust_camera_raw_effects(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    texture: f64,
    clarity: f64,
    dehaze: f64,
    glow: f64,
    glow_style: i32,
    glow_range: f64,
    glow_spread: f64,
    glow_warmth: f64,
    vignette_amount: f64,
    vignette_midpoint: f64,
    vignette_roundness: f64,
    vignette_feather: f64,
    vignette_highlights: f64,
    vignette_style: i32,
    scale: f64,
) {
    if width == 0 || height == 0 {
        return;
    }
    if texture == 0.0 && clarity == 0.0 && dehaze == 0.0 && !(glow > 0.0) && vignette_amount == 0.0 {
        return;
    }
    let count = width * height;
    let mut fine: Option<Vec<f32>> = None;
    let mut coarse: Option<Vec<f32>> = None;
    let mut glow_plane: Option<Vec<f32>> = None;
    if texture != 0.0 || clarity != 0.0 || glow > 0.0 {
        let mut luma = vec![0.0f32; count];
        for y in 0..height {
            for x in 0..width {
                let p = &rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    luma[y * width + x] = 0.0;
                    continue;
                }
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                luma[y * width + x] = rec709(r, g, b) as f32;
            }
        }
        if texture != 0.0 {
            let mut dst = vec![0.0f32; count];
            box_blur_plane(&luma, &mut dst, width, height, effects_radius(1.0, scale));
            fine = Some(dst);
        }
        if clarity != 0.0 {
            let mut dst = vec![0.0f32; count];
            box_blur_plane(&luma, &mut dst, width, height, effects_radius(4.0, scale));
            coarse = Some(dst);
        }
        if glow > 0.0 {
            let spread = glow_spread / 100.0;
            let base = if glow_style == 1 { 2.0 } else { 5.0 };
            let mut widened = base * (1.0 + spread);
            if widened < 1.0 {
                widened = 1.0;
            }
            let glow_radius = effects_radius(widened, scale);
            let threshold = (0.55 + 0.4 * (glow_range / 100.0)) as f32;
            let mut source = vec![0.0f32; count];
            let mut denom = 1.0f32 - threshold;
            if denom < 0.05f32 {
                denom = 0.05f32;
            }
            for i in 0..count {
                let mut t = (luma[i] - threshold) / denom;
                if t < 0.0 {
                    t = 0.0;
                }
                if t > 1.0 {
                    t = 1.0;
                }
                source[i] = t;
            }
            let mut dst = vec![0.0f32; count];
            box_blur_plane(&source, &mut dst, width, height, glow_radius);
            glow_plane = Some(dst);
        }
    }
    let warmth = glow_warmth / 100.0;
    let (glow_red, glow_green, glow_blue, glow_gain);
    if glow_style == 2 {
        // Halation's fringe is red. Warmth pushes it further that way, rather than toward yellow or blue.
        glow_red = 1.0;
        glow_green = 0.35 - 0.3 * warmth;
        glow_blue = 0.2 - 0.2 * warmth;
        glow_gain = 1.0;
    } else {
        glow_red = 0.75 + 0.25 * warmth;
        glow_green = 0.6 + 0.2 * warmth;
        glow_blue = 0.75 - 0.6 * warmth;
        glow_gain = if glow_style == 1 { 1.4 } else { 1.0 };
    }
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let index = y * width + x;
            let mut r = 1.0f64.min(p[0] as f64 / alpha);
            let mut g = 1.0f64.min(p[1] as f64 / alpha);
            let mut b = 1.0f64.min(p[2] as f64 / alpha);
            if fine.is_some() || coarse.is_some() {
                let tone = rec709(r, g, b);
                let mut detail = 0.0;
                if let Some(fine) = &fine {
                    detail += (texture / 100.0) * (tone - fine[index] as f64);
                }
                if let Some(coarse) = &coarse {
                    detail += (clarity / 100.0) * (tone - coarse[index] as f64);
                }
                if detail != 0.0 {
                    scale_luminance(&mut r, &mut g, &mut b, camera_clamp(tone + detail));
                }
            }
            if dehaze != 0.0 {
                effects_dehaze(&mut r, &mut g, &mut b, dehaze);
            }
            if let Some(glow_plane) = &glow_plane {
                if glow > 0.0 {
                    let add = glow_plane[index] as f64 * (glow / 100.0) * glow_gain;
                    r = camera_clamp(r + add * glow_red);
                    g = camera_clamp(g + add * glow_green);
                    b = camera_clamp(b + add * glow_blue);
                }
            }
            effects_vignette(
                &mut r,
                &mut g,
                &mut b,
                x,
                y,
                width,
                height,
                vignette_amount,
                vignette_midpoint,
                vignette_roundness,
                vignette_feather,
                vignette_highlights,
                vignette_style,
            );
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

fn detail_radius(slider: f64, scale: f64) -> f64 {
    let base = 0.5 + (slider / 100.0) * 2.5;
    let mut radius = base * (if scale > 0.0 { scale } else { 1.0 });
    if radius < 0.5 {
        radius = 0.5;
    }
    if radius > 64.0 {
        radius = 64.0;
    }
    radius
}

fn pixel_hue_deg(r: f64, g: f64, b: f64) -> f64 {
    let maxc = r.max(g.max(b));
    let minc = r.min(g.min(b));
    let chroma = maxc - minc;
    if chroma < 1e-6 {
        return 0.0;
    }
    let mut hue;
    if maxc == r {
        hue = (g - b) / chroma % 6.0;
    } else if maxc == g {
        hue = (b - r) / chroma + 2.0;
    } else {
        hue = (r - g) / chroma + 4.0;
    }
    hue = hue * 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    hue
}

fn hue_in_range(hue: f64, low: f64, high: f64) -> bool {
    if low <= high {
        return hue >= low && hue <= high;
    }
    hue >= low || hue <= high
}

fn sharpen_edge_at(luma: &[f32], width: usize, height: usize, x: usize, y: usize, radius: i32) -> f32 {
    let radius = if radius < 1 { 1 } else { radius };
    let center = luma[y * width + x];
    let mut sum = 0.0f32;
    let mut count = 0i32;
    let mut dy = -radius;
    while dy <= radius {
        let mut dx = -radius;
        while dx <= radius {
            if !(dx == 0 && dy == 0) {
                let sx = x as i64 + dx as i64;
                let sy = y as i64 + dy as i64;
                if sx >= 0 && sy >= 0 && sx < width as i64 && sy < height as i64 {
                    sum += (luma[sy as usize * width + sx as usize] - center).abs();
                    count += 1;
                }
            }
            dx += radius;
        }
        dy += radius;
    }
    if count != 0 {
        sum / count as f32
    } else {
        0.0
    }
}

/// Preview only: white where sharpening would land, black where masking protects. Uses the current
/// sharpen sliders.
pub fn adjust_camera_raw_sharpen_mask_overlay(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    sharpen_radius: f64,
    sharpen_detail: f64,
    sharpen_masking: f64,
    scale: f64,
) {
    if width == 0 || height == 0 {
        return;
    }
    let count = width * height;
    let mut luma = vec![0.0f32; count];
    for y in 0..height {
        for x in 0..width {
            let p = &rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                luma[y * width + x] = 0.0;
                continue;
            }
            let r = 1.0f64.min(p[0] as f64 / alpha);
            let g = 1.0f64.min(p[1] as f64 / alpha);
            let b = 1.0f64.min(p[2] as f64 / alpha);
            luma[y * width + x] = rec709(r, g, b) as f32;
        }
    }
    let radius = effects_radius(detail_radius(sharpen_radius, scale), 1.0);
    let threshold = (sharpen_masking / 100.0) * 0.35;
    let detail_boost = 0.5 + sharpen_detail / 100.0;
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let edge = sharpen_edge_at(&luma, width, height, x, y, radius);
            let mask = camera_clamp(
                (edge as f64 * detail_boost - threshold) / 0.04f64.max(0.35 - threshold * 0.5),
            );
            let gray = (mask * alpha).round() as u8;
            p[0] = gray;
            p[1] = gray;
            p[2] = gray;
        }
    }
}

/// Manual noise reduction, then sharpening. `scale` maps radius to preview pixels. Applied after the
/// creative grade.
pub fn adjust_camera_raw_detail(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    sharpen_amount: f64,
    sharpen_radius: f64,
    sharpen_detail: f64,
    sharpen_masking: f64,
    noise_luminance: f64,
    noise_luminance_detail: f64,
    noise_luminance_contrast: f64,
    noise_color: f64,
    noise_color_detail: f64,
    noise_color_smoothness: f64,
    scale: f64,
) {
    if width == 0 || height == 0 {
        return;
    }
    if sharpen_amount == 0.0 && noise_luminance == 0.0 && noise_color == 0.0 {
        return;
    }
    let count = width * height;
    let mut luma = vec![0.0f32; count];
    let mut work = vec![0.0f32; count];
    for y in 0..height {
        for x in 0..width {
            let p = &rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                luma[y * width + x] = 0.0;
                continue;
            }
            let r = 1.0f64.min(p[0] as f64 / alpha);
            let g = 1.0f64.min(p[1] as f64 / alpha);
            let b = 1.0f64.min(p[2] as f64 / alpha);
            luma[y * width + x] = rec709(r, g, b) as f32;
        }
    }
    if noise_luminance > 0.0 {
        let radius = effects_radius(1.0 + noise_luminance / 50.0, scale);
        box_blur_plane(&luma, &mut work, width, height, radius);
        let strength = noise_luminance / 100.0;
        let preserve = noise_luminance_detail / 100.0;
        let contrast = noise_luminance_contrast / 100.0;
        for y in 0..height {
            for x in 0..width {
                let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let index = y * width + x;
                let edge = sharpen_edge_at(&luma, width, height, x, y, 1);
                let local = strength * (1.0 - preserve * 1.0f64.min(edge as f64 * 6.0));
                let blurred = work[index];
                let mut target =
                    (luma[index] as f64 * (1.0 - local) + blurred as f64 * local) as f32;
                if contrast != 0.0 {
                    target =
                        (target as f64 + contrast * 0.25 * ((luma[index] - blurred) as f64)) as f32;
                }
                luma[index] = target;
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                let (mut r, mut g, mut b) = (r, g, b);
                scale_luminance(&mut r, &mut g, &mut b, target as f64);
                write_premultiplied(p, r, g, b, alpha);
            }
        }
    }
    if noise_color > 0.0 {
        let radius = effects_radius(1.0 + noise_color_smoothness / 40.0, scale);
        // The C left fully transparent pixels' chroma uninitialized; the port starts them at zero.
        let mut chroma = vec![0.0f32; count];
        let mut chroma_blur = vec![0.0f32; count];
        for y in 0..height {
            for x in 0..width {
                let p = &rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                let (_, s, _) = rgb_to_hsl(r, g, b);
                chroma[y * width + x] = s as f32;
            }
        }
        box_blur_plane(&chroma, &mut chroma_blur, width, height, radius);
        let strength = noise_color / 100.0;
        let preserve = noise_color_detail / 100.0;
        for y in 0..height {
            for x in 0..width {
                let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let index = y * width + x;
                let edge = (chroma[index] - chroma_blur[index]).abs();
                let local = strength * (1.0 - preserve * 1.0f64.min(edge as f64 * 4.0));
                let sat = chroma[index] * (1.0 - local) as f32 + chroma_blur[index] * local as f32;
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                let (h, _, l) = rgb_to_hsl(r, g, b);
                let (r, g, b) = hsl_to_rgb(h, sat as f64, l);
                write_premultiplied(p, r, g, b, alpha);
            }
        }
    }
    if sharpen_amount > 0.0 {
        for y in 0..height {
            for x in 0..width {
                let p = &rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                luma[y * width + x] = rec709(r, g, b) as f32;
            }
        }
        let radius = effects_radius(detail_radius(sharpen_radius, scale), 1.0);
        box_blur_plane(&luma, &mut work, width, height, radius);
        let amount = sharpen_amount / 100.0;
        let detail_mix = sharpen_detail / 100.0;
        let threshold = (sharpen_masking / 100.0) * 0.35;
        for y in 0..height {
            for x in 0..width {
                let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
                let alpha = p[3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let index = y * width + x;
                let edge = sharpen_edge_at(&luma, width, height, x, y, radius);
                let mask = camera_clamp(
                    (edge as f64 * (0.5 + detail_mix) - threshold)
                        / 0.04f64.max(0.35 - threshold * 0.5),
                );
                let high = (luma[index] - work[index]) as f64;
                let sharpened = camera_clamp(
                    luma[index] as f64 + high * amount * mask * (0.5 + detail_mix),
                );
                let r = 1.0f64.min(p[0] as f64 / alpha);
                let g = 1.0f64.min(p[1] as f64 / alpha);
                let b = 1.0f64.min(p[2] as f64 / alpha);
                let (mut r, mut g, mut b) = (r, g, b);
                scale_luminance(&mut r, &mut g, &mut b, sharpened);
                write_premultiplied(p, r, g, b, alpha);
            }
        }
    }
}

fn optics_defringe(
    r: &mut f64,
    g: &mut f64,
    b: &mut f64,
    purple_amount: f64,
    purple_low: f64,
    purple_high: f64,
    green_amount: f64,
    green_low: f64,
    green_high: f64,
) {
    let hue = pixel_hue_deg(*r, *g, *b);
    let maxc = (*r).max((*g).max(*b));
    let minc = (*r).min((*g).min(*b));
    let chroma = maxc - minc;
    if chroma < 1e-6 {
        return;
    }
    let sat = chroma / maxc;
    let mut reduce = 0.0f64;
    if purple_amount > 0.0 && hue_in_range(hue, purple_low, purple_high) {
        reduce = reduce.max(purple_amount / 100.0);
    }
    if green_amount > 0.0 && hue_in_range(hue, green_low, green_high) {
        reduce = reduce.max(green_amount / 100.0);
    }
    if reduce <= 0.0 {
        return;
    }
    let lum = rec709(*r, *g, *b);
    let factor = 1.0 - reduce * sat;
    *r = camera_clamp(lum + (*r - lum) * factor);
    *g = camera_clamp(lum + (*g - lum) * factor);
    *b = camera_clamp(lum + (*b - lum) * factor);
}

fn optics_chromatic(rgba: &mut [u8], width: usize, height: usize, stride: usize, strength: f64) {
    if strength <= 0.0 {
        return;
    }
    let mut copy = vec![0u8; height * stride];
    for y in 0..height {
        copy[y * stride..y * stride + width * 4]
            .copy_from_slice(&rgba[y * stride..y * stride + width * 4]);
    }
    let cx = width as f64 * 0.5;
    let cy = height as f64 * 0.5;
    let max_r = cx.hypot(cy);
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let dx = x as f64 + 0.5 - cx;
            let dy = y as f64 + 0.5 - cy;
            let radial = dx.hypot(dy) / max_r;
            let shift = strength * radial * radial * 2.5;
            let rx = (x as f64 - shift).round() as i32;
            let bx = (x as f64 + shift).round() as i32;
            let pr = &copy[y * stride + clamped_index(rx, width) * 4..][..4];
            let pb = &copy[y * stride + clamped_index(bx, width) * 4..][..4];
            let g = 1.0f64.min(copy[y * stride + x * 4 + 1] as f64 / alpha);
            let r = 1.0f64.min(pr[0] as f64 / 1.0f64.max(pr[3] as f64));
            let b = 1.0f64.min(pb[2] as f64 / 1.0f64.max(pb[3] as f64));
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

fn optics_vignette_correct(
    r: &mut f64,
    g: &mut f64,
    b: &mut f64,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    amount: f64,
    midpoint: f64,
) {
    if amount == 0.0 || width == 0 || height == 0 {
        return;
    }
    let nx = (x as f64 + 0.5) / width as f64 * 2.0 - 1.0;
    let ny = (y as f64 + 0.5) / height as f64 * 2.0 - 1.0;
    let dist = nx.hypot(ny) / 2.0f64.sqrt();
    let start = (midpoint / 100.0) * 0.85;
    let t = camera_clamp((dist - start) / 0.35);
    let mask = t * t * (3.0 - 2.0 * t);
    let lift = (amount / 100.0) * mask;
    if lift > 0.0 {
        *r = camera_clamp(*r + (1.0 - *r) * lift);
        *g = camera_clamp(*g + (1.0 - *g) * lift);
        *b = camera_clamp(*b + (1.0 - *b) * lift);
    } else {
        let factor = 1.0 + lift;
        *r *= factor;
        *g *= factor;
        *b *= factor;
    }
}

/// Chromatic aberration, lens distortion, defringe, and lens-vignetting correction. `distortionK`
/// matches `lens_distort`. `profile_distortion` and `scale` are part of the C signature but the kernel
/// does not read them; they are accepted here only for call-site parity.
#[allow(unused_variables)]
pub fn adjust_camera_raw_optics(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    remove_chromatic: i32,
    lens_profile: i32,
    profile_distortion: f64,
    profile_vignetting: f64,
    distortion_k: f64,
    purple_amount: f64,
    purple_hue_low: f64,
    purple_hue_high: f64,
    green_amount: f64,
    green_hue_low: f64,
    green_hue_high: f64,
    vignette_amount: f64,
    vignette_midpoint: f64,
    scale: f64,
) {
    if width == 0 || height == 0 {
        return;
    }
    let profile_vignette = if lens_profile != 0 { profile_vignetting / 100.0 } else { 0.0 };
    let vignette = vignette_amount + profile_vignette * 35.0;
    if distortion_k != 0.0 {
        let bytes = height * stride;
        let copy = rgba[..bytes].to_vec();
        lens_distort(&copy, rgba, width, height, stride, distortion_k);
    }
    if remove_chromatic != 0 {
        optics_chromatic(rgba, width, height, stride, 0.45);
    }
    if purple_amount == 0.0 && green_amount == 0.0 && vignette == 0.0 {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let mut r = 1.0f64.min(p[0] as f64 / alpha);
            let mut g = 1.0f64.min(p[1] as f64 / alpha);
            let mut b = 1.0f64.min(p[2] as f64 / alpha);
            optics_defringe(
                &mut r,
                &mut g,
                &mut b,
                purple_amount,
                purple_hue_low,
                purple_hue_high,
                green_amount,
                green_hue_low,
                green_hue_high,
            );
            optics_vignette_correct(
                &mut r,
                &mut g,
                &mut b,
                x,
                y,
                width,
                height,
                vignette,
                vignette_midpoint,
            );
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

/// Camera calibration before the main grade. Primary hue and saturation shifts are −100…100; shadow tint
/// is green/magenta.
pub fn adjust_camera_raw_calibration(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    shadow_tint: f64,
    red_hue: f64,
    red_saturation: f64,
    green_hue: f64,
    green_saturation: f64,
    blue_hue: f64,
    blue_saturation: f64,
    process_version: i32,
) {
    if width == 0 || height == 0 {
        return;
    }
    let version_scale = if process_version <= 1 {
        0.55
    } else if process_version == 2 {
        0.65
    } else if process_version == 3 {
        0.75
    } else if process_version == 4 {
        0.85
    } else if process_version == 5 {
        0.92
    } else {
        1.0
    };
    let tint = shadow_tint / 100.0 * version_scale;
    let rh = red_hue / 100.0 * (15.0 / 360.0) * version_scale;
    let rs = red_saturation / 100.0 * 0.45 * version_scale;
    let gh = green_hue / 100.0 * (15.0 / 360.0) * version_scale;
    let gs = green_saturation / 100.0 * 0.45 * version_scale;
    let bh = blue_hue / 100.0 * (15.0 / 360.0) * version_scale;
    let bs = blue_saturation / 100.0 * 0.45 * version_scale;
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let r = 1.0f64.min(p[0] as f64 / alpha);
            let g = 1.0f64.min(p[1] as f64 / alpha);
            let b = 1.0f64.min(p[2] as f64 / alpha);
            let (mut h, mut s, l) = rgb_to_hsl(r, g, b);
            if l < 0.35 && tint != 0.0 {
                h += tint * 0.06;
                if h < 0.0 {
                    h += 1.0;
                }
                if h >= 1.0 {
                    h -= 1.0;
                }
            }
            let maxc = r.max(g.max(b));
            let minc = r.min(g.min(b));
            if maxc - minc > 1e-5 {
                if r >= g && r >= b {
                    h += rh;
                    s = camera_clamp(s * (1.0 + rs));
                } else if g >= r && g >= b {
                    h += gh;
                    s = camera_clamp(s * (1.0 + gs));
                } else {
                    h += bh;
                    s = camera_clamp(s * (1.0 + bs));
                }
                if h < 0.0 {
                    h += 1.0;
                }
                if h >= 1.0 {
                    h -= 1.0;
                }
            }
            let (r, g, b) = hsl_to_rgb(h, s, l);
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}
fn tonal_smooth(low: f64, high: f64, value: f64) -> f64 {
    let t = camera_clamp((value - low) / (high - low));
    t * t * (3.0 - 2.0 * t)
}

/// Local luminance contrast with independent shadow, midtone, and highlight gains.
/// `blurred` is the same premultiplied RGBA image blurred at the chosen detail radius.
pub fn adjust_tonal_contrast(
    rgba: &mut [u8],
    blurred: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    blurred_stride: usize,
    amount: f64,
    shadows: f64,
    midtones: f64,
    highlights: f64,
) {
    if amount <= 0.0 || (shadows == 0.0 && midtones == 0.0 && highlights == 0.0) {
        return;
    }
    let strength = amount / 50.0;
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let base = &blurred[y * blurred_stride + x * 4..y * blurred_stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 || base[3] == 0 {
                continue;
            }
            let r = 1.0f64.min(p[0] as f64 / alpha);
            let g = 1.0f64.min(p[1] as f64 / alpha);
            let b = 1.0f64.min(p[2] as f64 / alpha);
            let lum = rec709(r, g, b);
            let base_lum = rec709(
                1.0f64.min(base[0] as f64 / base[3] as f64),
                1.0f64.min(base[1] as f64 / base[3] as f64),
                1.0f64.min(base[2] as f64 / base[3] as f64),
            );
            let shadow_weight = 1.0 - tonal_smooth(0.15, 0.5, base_lum);
            let highlight_weight = tonal_smooth(0.5, 0.85, base_lum);
            let midtone_weight = 1.0 - shadow_weight - highlight_weight;
            let weight = (shadows * shadow_weight
                + midtones * midtone_weight
                + highlights * highlight_weight)
                / 100.0;
            let detail = lum - base_lum;
            let delta = 0.18 * (detail * 6.0).tanh() * weight * strength * (4.0 * lum * (1.0 - lum));
            write_premultiplied(
                p,
                camera_clamp(r + delta),
                camera_clamp(g + delta),
                camera_clamp(b + delta),
                alpha,
            );
        }
    }
}

fn lut_at(lut: &[f32], value: f64) -> f64 {
    let scaled = camera_clamp(value) * 255.0;
    let lo = scaled as i32;
    let hi = if lo < 255 { lo + 1 } else { 255 };
    let t = scaled - lo as f64;
    lut[lo as usize] as f64 + (lut[hi as usize] as f64 - lut[lo as usize] as f64) * t
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g.max(b));
    let minc = r.min(g.min(b));
    let l = (maxc + minc) * 0.5;
    let d = maxc - minc;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let mut h;
    if maxc == r {
        h = (g - b) / d % 6.0;
    } else if maxc == g {
        h = (b - r) / d + 2.0;
    } else {
        h = (r - g) / d + 4.0;
    }
    h /= 6.0;
    if h < 0.0 {
        h += 1.0;
    }
    (h, s, l)
}

fn hue_to_rgb(p: f64, q: f64, mut t: f64) -> f64 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    if s <= 1e-6 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    (
        hue_to_rgb(p, q, h + 1.0 / 3.0),
        hue_to_rgb(p, q, h),
        hue_to_rgb(p, q, h - 1.0 / 3.0),
    )
}

fn circular_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).abs();
    if d > 0.5 {
        1.0 - d
    } else {
        d
    }
}

const MIXER_CENTERS: [f64; 8] = [
    0.0,
    30.0 / 360.0,
    60.0 / 360.0,
    120.0 / 360.0,
    180.0 / 360.0,
    240.0 / 360.0,
    270.0 / 360.0,
    300.0 / 360.0,
];

fn point_weight(h: f64, s: f64, l: f64, point: &[f32]) -> f64 {
    let hue_half = (if point[6] > 0.01f32 { point[6] } else { 0.01f32 }) as f64;
    let sat_half = (if point[7] > 0.01f32 { point[7] } else { 0.01f32 }) as f64;
    let lum_half = (if point[8] > 0.01f32 { point[8] } else { 0.01f32 }) as f64;
    let hue_w = 1.0 - circular_distance(h, point[0] as f64) / hue_half;
    let sat_w = 1.0 - (s - point[1] as f64).abs() / sat_half;
    let lum_w = 1.0 - (l - point[2] as f64).abs() / lum_half;
    if hue_w < 0.0 || sat_w < 0.0 || lum_w < 0.0 {
        return 0.0;
    }
    hue_w * sat_w * lum_w
}

/// Curve, Color Mixer, and Color Grading after the basic grade. `toneLut` and the channel LUTs are 256
/// entries. `mixer` is 24 floats: hue, saturation, luminance for eight families, −1…1. Each point color is
/// 9 floats (hue, saturation, luminance, three shifts −1…1, three range half-widths). `grade` is four wheels
/// of hue turns, saturation 0…1, and luminance −1…1. `visualize` darkens pixels outside that point color.
pub fn adjust_camera_raw_curve_color(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    tone_lut: &[f32],
    red_lut: &[f32],
    green_lut: &[f32],
    blue_lut: &[f32],
    refine_saturation: f64,
    mixer: &[f32],
    point_count: i32,
    points: &[f32],
    grade: &[f32],
    blending: f64,
    balance: f64,
    visualize: i32,
) {
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[y * stride + x * 4..y * stride + x * 4 + 4];
            let alpha = p[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            let mut r = 1.0f64.min(p[0] as f64 / alpha);
            let mut g = 1.0f64.min(p[1] as f64 / alpha);
            let mut b = 1.0f64.min(p[2] as f64 / alpha);
            // The tone curve works on red, green and blue alike, as Photoshop's does, so contrast brings color strength
            // with it. Refine Saturation below zero eases toward changing brightness alone (−100), and above zero adds
            // more color.
            let mut curved_r = lut_at(tone_lut, r);
            let mut curved_g = lut_at(tone_lut, g);
            let mut curved_b = lut_at(tone_lut, b);
            if refine_saturation < 0.0 {
                let (mut br, mut bg, mut bb) = (r, g, b);
                scale_luminance(&mut br, &mut bg, &mut bb, lut_at(tone_lut, rec709(r, g, b)));
                let k = -refine_saturation;
                curved_r += (br - curved_r) * k;
                curved_g += (bg - curved_g) * k;
                curved_b += (bb - curved_b) * k;
            } else if refine_saturation > 0.0 {
                let lum = rec709(curved_r, curved_g, curved_b);
                let factor = 1.0 + refine_saturation;
                curved_r = camera_clamp(lum + (curved_r - lum) * factor);
                curved_g = camera_clamp(lum + (curved_g - lum) * factor);
                curved_b = camera_clamp(lum + (curved_b - lum) * factor);
            }
            r = curved_r;
            g = curved_g;
            b = curved_b;
            r = lut_at(red_lut, r);
            g = lut_at(green_lut, g);
            b = lut_at(blue_lut, b);
            let (mut h, mut s, mut l) = rgb_to_hsl(r, g, b);
            let (source_hue, source_sat, source_lum) = (h, s, l);
            let mut hue_delta = 0.0;
            let mut sat_delta = 0.0;
            let mut lum_delta = 0.0;
            let mut weight_sum = 0.0;
            for i in 0..8 {
                let dist = circular_distance(h, MIXER_CENTERS[i]);
                let w = 1.0 - dist / (40.0 / 360.0);
                if w <= 0.0 {
                    continue;
                }
                hue_delta += mixer[i] as f64 * w * (30.0 / 360.0);
                sat_delta += mixer[8 + i] as f64 * w;
                lum_delta += mixer[16 + i] as f64 * w * 0.25;
                weight_sum += w;
            }
            if weight_sum > 1.0 {
                hue_delta /= weight_sum;
                sat_delta /= weight_sum;
                lum_delta /= weight_sum;
            }
            h += hue_delta;
            if h < 0.0 {
                h += 1.0;
            }
            if h >= 1.0 {
                h -= 1.0;
            }
            s = camera_clamp(s * (1.0 + sat_delta));
            l = camera_clamp(l + lum_delta);
            for i in 0..point_count {
                let point = &points[i as usize * 9..i as usize * 9 + 9];
                let w = point_weight(h, s, l, point);
                if w <= 0.0 {
                    continue;
                }
                h += point[3] as f64 * w * (30.0 / 360.0);
                s = camera_clamp(s * (1.0 + point[4] as f64 * w));
                l = camera_clamp(l + point[5] as f64 * w * 0.25);
            }
            if h < 0.0 {
                h += 1.0;
            }
            if h >= 1.0 {
                h -= 1.0;
            }
            let (mut r, mut g, mut b) = hsl_to_rgb(h, s, l);
            // Balance moves the crossover between the shadow and highlight wheels. Toward highlights
            // it has to move down, so more of the picture counts as highlight and the shadow wheel
            // loses its hold; the other sign strengthened the shadow tint it was meant to weaken.
            let split = 0.5 - balance * 0.2;
            let reach = 0.12 + blending * 0.38;
            let mut shadow_w = camera_clamp((split + reach - rec709(r, g, b)) / 0.05f64.max(reach * 2.0));
            let mut highlight_w = camera_clamp((rec709(r, g, b) - (split - reach)) / 0.05f64.max(reach * 2.0));
            let mut mid_w = camera_clamp(1.0 - (rec709(r, g, b) - split).abs() / (0.35 + reach));
            let sum = shadow_w + mid_w + highlight_w;
            if sum > 1e-4 {
                shadow_w /= sum;
                mid_w /= sum;
                highlight_w /= sum;
            }
            let weights = [shadow_w, mid_w, highlight_w, 1.0];
            for wheel in 0..4 {
                let wh = grade[wheel * 3] as f64;
                let ws = grade[wheel * 3 + 1] as f64;
                let wl = grade[wheel * 3 + 2] as f64;
                let w = weights[wheel];
                if w <= 0.0 || (ws <= 0.0 && wl == 0.0) {
                    continue;
                }
                let (cr, cg, cb) = hsl_to_rgb(wh, 1.0, 0.5);
                r = camera_clamp(r + (cr - 0.5) * ws * w * 0.85);
                g = camera_clamp(g + (cg - 0.5) * ws * w * 0.85);
                b = camera_clamp(b + (cb - 0.5) * ws * w * 0.85);
                if wl != 0.0 {
                    let target = camera_clamp(rec709(r, g, b) + wl * 0.25 * w);
                    scale_luminance(&mut r, &mut g, &mut b, target);
                }
            }
            if visualize >= 0
                && visualize < point_count
                && point_weight(
                    source_hue,
                    source_sat,
                    source_lum,
                    &points[visualize as usize * 9..visualize as usize * 9 + 9],
                ) <= 0.05
            {
                r *= 0.35;
                g *= 0.35;
                b *= 0.35;
            }
            write_premultiplied(p, r, g, b, alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_table() -> Vec<u8> {
        (0..256).flat_map(|i| [i as u8, i as u8, i as u8]).collect()
    }

    fn identity_lut() -> Vec<f32> {
        (0..256).map(|i| i as f32 / 255.0).collect()
    }

    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 43 + 17) % 251) as u8).collect()
    }

    #[test]
    fn gradient_map_identity_table_keeps_grays_and_alpha() {
        let mut pixels = vec![128u8, 128, 128, 255, 100, 100, 100, 128, 30, 30, 30, 0];
        let expected = pixels.clone();
        adjust_gradient_map(&mut pixels, 3, 1, 12, &identity_table());
        assert_eq!(pixels, expected);
    }

    #[test]
    fn gradient_map_leaves_fully_transparent_pixels_alone() {
        let mut pixels = vec![0u8, 0, 0, 0, 255, 0, 255, 0];
        let expected = pixels.clone();
        adjust_gradient_map(&mut pixels, 2, 1, 8, &identity_table());
        assert_eq!(pixels, expected);
    }

    #[test]
    fn grain_zero_amount_is_identity_and_transparency_is_fixed() {
        let pixels = pseudo_random_bytes(8 * 8 * 4);
        let mut zero_amount = pixels.clone();
        adjust_grain(&mut zero_amount, 8, 8, 32, 0.0, 2.0, 50.0, 7, 0.0, 0.0, 1.0);
        assert_eq!(zero_amount, pixels);

        let mut transparent = pixels;
        transparent[0..4].copy_from_slice(&[10, 20, 30, 0]);
        let expected = transparent.clone();
        adjust_grain(&mut transparent, 8, 8, 32, 80.0, 2.0, 50.0, 7, 0.0, 0.0, 1.0);
        assert_eq!(&transparent[0..4], &expected[0..4]);
    }

    #[test]
    fn grain_is_stable_for_a_seed_and_changes_with_it() {
        let base: Vec<u8> = (0..4 * 4 * 4).map(|i| ((i * 53 + 47) % 200) as u8 + 40).collect();
        let mut a = base.clone();
        let mut b = base.clone();
        let mut c = base.clone();
        adjust_grain(&mut a, 4, 4, 16, 60.0, 3.0, 40.0, 12345, 0.0, 0.0, 1.0);
        adjust_grain(&mut b, 4, 4, 16, 60.0, 3.0, 40.0, 12345, 0.0, 0.0, 1.0);
        adjust_grain(&mut c, 4, 4, 16, 60.0, 3.0, 40.0, 999, 0.0, 0.0, 1.0);
        assert_eq!(a, b);
        assert_ne!(a, c);
        for pixel in a.chunks_exact(4) {
            assert!(pixel[3] >= 40);
        }
    }

    #[test]
    fn grain_of_a_window_matches_that_part_of_the_whole() {
        let source: Vec<u8> = (0..12 * 12 * 4).map(|i| ((i * 29 + 3) % 220) as u8 + 20).collect();
        let mut full = source.clone();
        adjust_grain(&mut full, 12, 12, 48, 70.0, 2.5, 35.0, 42, 0.0, 0.0, 1.0);
        let mut window = Vec::with_capacity(4 * 4 * 4);
        for y in 4..8 {
            window.extend_from_slice(&source[(y * 12 + 4) * 4..(y * 12 + 8) * 4]);
        }
        adjust_grain(&mut window, 4, 4, 16, 70.0, 2.5, 35.0, 42, 4.0, 4.0, 1.0);
        for y in 0..4 {
            for x in 0..4 {
                let full_at = ((y + 4) * 12 + (x + 4)) * 4;
                let window_at = (y * 4 + x) * 4;
                assert_eq!(
                    &window[window_at..window_at + 4],
                    &full[full_at..full_at + 4]
                );
            }
        }
    }

    #[test]
    fn black_white_pure_red_uses_red_weight() {
        let weights = [0.4f32, 0.6, 0.4, 0.6, 0.2, 0.8];
        let mut pixels = vec![255u8, 0, 0, 255];
        adjust_black_white(&mut pixels, 1, 1, 4, &weights, 0, 0.0, 0.0);
        assert_eq!(&pixels[0..3], &[102, 102, 102]);
        assert_eq!(pixels[3], 255);
    }

    #[test]
    fn black_white_leaves_transparent_pixels_alone() {
        let weights = [0.4f32, 0.6, 0.4, 0.6, 0.2, 0.8];
        let mut pixels = vec![255u8, 0, 0, 0];
        let expected = pixels.clone();
        adjust_black_white(&mut pixels, 1, 1, 4, &weights, 1, 30.0, 0.5);
        assert_eq!(pixels, expected);
    }

    #[test]
    fn color_balance_zero_shifts_is_identity() {
        let mut pixels = vec![64u8, 128, 192, 255, 32, 32, 32, 128];
        let expected = pixels.clone();
        let zeros = [0.0f32; 3];
        adjust_color_balance(&mut pixels, 2, 1, 8, &zeros, &zeros, &zeros, 1);
        assert_eq!(pixels, expected);
    }

    #[test]
    fn camera_raw_clipping_views() {
        let mut highlight_clip = vec![255u8, 255, 255, 255, 0, 0, 0, 255];
        adjust_camera_raw(
            &mut highlight_clip, 2, 1, 8, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            1,
        );
        assert_eq!(&highlight_clip[0..4], &[255, 255, 255, 255]);
        assert_eq!(&highlight_clip[4..8], &[0, 0, 0, 255]);

        let mut shadow_clip = vec![255u8, 255, 255, 255, 0, 0, 0, 255];
        adjust_camera_raw(
            &mut shadow_clip, 2, 1, 8, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2,
        );
        assert_eq!(&shadow_clip[0..4], &[255, 255, 255, 255]);
        assert_eq!(&shadow_clip[4..8], &[0, 0, 0, 255]);
    }

    #[test]
    fn camera_raw_zero_amounts_are_identity() {
        let mut pixels = vec![64u8, 128, 192, 255, 64, 64, 64, 128, 9, 9, 9, 0];
        let expected = pixels.clone();
        adjust_camera_raw(
            &mut pixels, 3, 1, 12, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0,
        );
        assert_eq!(pixels, expected);
    }

    #[test]
    fn clip_overlay_marks_clipped_shadows_blue() {
        let mut pixels = vec![0u8, 0, 0, 255, 255, 255, 255, 255];
        adjust_camera_raw_clip_overlay(&mut pixels, 2, 1, 8, 1, 0);
        assert_eq!(&pixels[0..4], &[0, 0, 166, 255]);
        assert_eq!(&pixels[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn colored_vignette_nonpositive_amount_is_identity() {
        let mut pixels = vec![10u8, 20, 30, 255, 0, 0, 0, 0];
        let expected = pixels.clone();
        adjust_colored_vignette(
            &mut pixels, 2, 1, 8, 0.0, 0.0, 2.0, 1.0, 1, 0.0, 50.0, 50.0, 50.0, 0.0, 1.0, 0.0,
            0.0,
        );
        assert_eq!(pixels, expected);

        let mut negative = expected.clone();
        adjust_colored_vignette(
            &mut negative, 2, 1, 8, 0.0, 0.0, 2.0, 1.0, 1, -10.0, 50.0, 50.0, 50.0, 0.0, 1.0,
            0.0, 0.0,
        );
        assert_eq!(negative, expected);
    }

    #[test]
    fn colored_vignette_fills_clear_pixels_only_when_asked() {
        let mut filled = vec![0u8; 2 * 2 * 4];
        adjust_colored_vignette(
            &mut filled, 2, 2, 8, 0.0, 0.0, 1.0, 1.0, 1, 100.0, 0.0, 0.0, 100.0, 0.0, 1.0,
            0.5, 0.25,
        );
        assert_eq!(&filled[0..4], &[0, 0, 0, 0]);
        assert_eq!(&filled[12..16], &[255, 128, 64, 255]);

        let mut kept = vec![0u8; 2 * 2 * 4];
        adjust_colored_vignette(
            &mut kept, 2, 2, 8, 0.0, 0.0, 1.0, 1.0, 0, 100.0, 0.0, 0.0, 100.0, 0.0, 1.0, 0.5,
            0.25,
        );
        assert_eq!(kept, vec![0u8; 2 * 2 * 4]);
    }

    #[test]
    fn tonal_contrast_zero_amount_is_identity() {
        let pixels = pseudo_random_bytes(4 * 4 * 4);
        let blurred = pixels.clone();
        let mut out = pixels.clone();
        adjust_tonal_contrast(&mut out, &blurred, 4, 4, 16, 16, 0.0, 50.0, 50.0, 50.0);
        assert_eq!(out, pixels);
    }

    #[test]
    fn tonal_contrast_preserves_alpha_and_skips_clear_pixels() {
        let blurred = pseudo_random_bytes(4 * 4 * 4);
        let mut pixels = blurred.clone();
        pixels[0..4].copy_from_slice(&[0, 0, 0, 0]);
        let alphas: Vec<u8> = pixels.chunks_exact(4).map(|p| p[3]).collect();
        adjust_tonal_contrast(&mut pixels, &blurred, 4, 4, 16, 16, 100.0, 80.0, 80.0, 20.0);
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
        let out_alphas: Vec<u8> = pixels.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(out_alphas, alphas);
    }

    #[test]
    fn detail_zero_amounts_is_identity() {
        let pixels = pseudo_random_bytes(4 * 4 * 4);
        let mut out = pixels.clone();
        adjust_camera_raw_detail(
            &mut out, 4, 4, 16, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn sharpen_mask_overlay_masks_flat_image_to_zero() {
        let mut pixels: Vec<u8> = (0..4 * 4).flat_map(|_| [128u8, 128, 128, 255]).collect();
        adjust_camera_raw_sharpen_mask_overlay(&mut pixels, 4, 4, 16, 0.0, 0.0, 0.0, 1.0);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(&pixel[0..3], &[0, 0, 0]);
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn effects_zero_amounts_is_identity() {
        let pixels = pseudo_random_bytes(4 * 4 * 4);
        let mut out = pixels.clone();
        adjust_camera_raw_effects(
            &mut out, 4, 4, 16, 0.0, 0.0, 0.0, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 50.0, 50.0, 50.0,
            0.0, 0, 1.0,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn curve_color_identity_luts_keep_opaque_grays() {
        let lut = identity_lut();
        let mut pixels = vec![128u8, 128, 128, 255, 0, 0, 0, 0];
        let expected = pixels.clone();
        let mixer = [0.0f32; 24];
        let grade = [0.0f32; 12];
        adjust_camera_raw_curve_color(
            &mut pixels, 2, 1, 8, &lut, &lut, &lut, &lut, 0.0, &mixer, 0, &[], &grade, 0.0, 0.0,
            -1,
        );
        assert_eq!(pixels, expected);
    }

    #[test]
    fn optics_no_op_is_identity() {
        let pixels = pseudo_random_bytes(4 * 4 * 4);
        let mut out = pixels.clone();
        adjust_camera_raw_optics(
            &mut out, 4, 4, 16, 0, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn calibration_is_deterministic_and_keeps_alpha_and_clear_pixels() {
        let base = pseudo_random_bytes(6 * 4 * 4);
        let mut a = base.clone();
        let mut b = base.clone();
        adjust_camera_raw_calibration(
            &mut a, 6, 4, 24, 40.0, 20.0, 30.0, -15.0, 25.0, 10.0, -20.0, 3,
        );
        adjust_camera_raw_calibration(
            &mut b, 6, 4, 24, 40.0, 20.0, 30.0, -15.0, 25.0, 10.0, -20.0, 3,
        );
        assert_eq!(a, b);

        let mut transparent = base.clone();
        transparent[0..4].copy_from_slice(&[9, 9, 9, 0]);
        let expected = transparent.clone();
        adjust_camera_raw_calibration(
            &mut transparent, 6, 4, 24, 40.0, 20.0, 30.0, -15.0, 25.0, 10.0, -20.0, 3,
        );
        assert_eq!(&transparent[0..4], &expected[0..4]);
        let out_alphas: Vec<u8> = transparent.chunks_exact(4).map(|p| p[3]).collect();
        let start_alphas: Vec<u8> = expected.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(out_alphas, start_alphas);
    }
}
