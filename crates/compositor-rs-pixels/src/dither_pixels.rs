//! Port of `references/Compositor/Compositor/Rendering/DitherPixels.{h,c}`.
//!
//! Dithering, dot-matrix and glow kernels. `stride` is still bytes per row and must be at least
//! `width * 4`, exactly as the C assumed. The C ran the per-row and per-line runs through
//! libdispatch's `dispatch_apply`; those runs are independent, so they are done with `rayon` here.

use rayon::prelude::*;

/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_ATKINSON: i32 = 0;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_FLOYD_STEINBERG: i32 = 1;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_BAYER_2: i32 = 2;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_BAYER_4: i32 = 3;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_BAYER_8: i32 = 4;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_DOTS: i32 = 5;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_LINES: i32 = 6;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_DIAMONDS: i32 = 7;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_PATTERNS: i32 = 8;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_GLYPHS: i32 = 9;
/// The dither styles, in the order the Filter panel lists them.
pub const DITHER_SCANLINES: i32 = 10;

/// The dither parameters: the port of the C `DitherParams`, same field order and numeric values.
#[derive(Clone, Copy, Debug)]
pub struct DitherParams<'a> {
    /// One of the `DITHER_*` styles.
    pub style: i32,
    /// Tones per channel for diffusion and ordered styles, 2–8. Two is pure 1-bit.
    pub levels: i32,
    /// How much of each pixel's error diffusion passes on, 0–1.
    pub diffusion: f32,
    /// −1…1: darker (more ink) or lighter, and flatter or punchier, before dithering.
    pub density: f32,
    /// −1…1: darker (more ink) or lighter, and flatter or punchier, before dithering.
    pub contrast: f32,
    /// Halftone and glyph cells, in pixels (scanlines: the distance between lines).
    pub cell: i32,
    /// The halftone screen's angle in radians.
    pub angle: f32,
    /// Halftone dots, patterns and glyphs mark the light tones on the dark color instead of the dark on the light.
    pub light_on_dark: i32,
    /// 0: the result is made of `dark` and `light` (straight sRGB). 1: it keeps the image's own colors.
    pub original_colors: i32,
    /// The dark color, 0–255 per channel.
    pub dark: [u8; 3],
    /// The light color, 0–255 per channel.
    pub light: [u8; 3],
    /// Glyphs: the width of the `glyph_count` coverage maps.
    pub glyph_width: i32,
    /// Glyphs: the height of the `glyph_count` coverage maps.
    pub glyph_height: i32,
    /// Glyphs: `glyph_count` coverage maps of `glyph_width` × `glyph_height` bytes (255 is fully inked), from least
    /// inked to most. The image is laid out in cells that size, like lines of monospaced text.
    pub glyphs: &'a [u8],
    /// Glyphs: each map's mean coverage (0–1), from least inked to most.
    pub glyph_coverage: &'a [f32],
    /// Glyphs: how many coverage maps `glyphs` holds.
    pub glyph_count: i32,
    /// Scanlines: how far each line breaks into round dots (0–1).
    pub dots: f32,
    /// Scanlines: how many pixels its wobble pushes it sideways.
    pub wobble: f32,
}

/// `clamp01`.
fn clamp01(v: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}

/// The `(start, end)` runs the C's `in_bands` split `count` items into: one run for under 64 items,
/// else 32 runs as even as they divide.
fn bands(count: usize) -> Vec<(usize, usize)> {
    let band_count = if count < 64 { 1 } else { 32 };
    let size = count.div_ceil(band_count);
    let mut runs = Vec::with_capacity(band_count);
    for band in 0..band_count {
        let start = band * size;
        let end = (start + size).min(count);
        if start < end {
            runs.push((start, end));
        }
    }
    runs
}

/// Density darkens (positive) or lightens as a gamma, so black and white stay put; contrast pivots on mid gray.
fn adjust_tone(v: f32, gamma: f32, contrast: f32) -> f32 {
    let v = clamp01(v).powf(gamma);
    clamp01((v - 0.5) * contrast + 0.5)
}

/// One error-diffusion tap: a neighbor's offset, with its weight over the kernel's divisor.
struct Tap {
    dx: i32,
    dy: i32,
    weight: i32,
}

/// One error-diffusion kernel: neighbors to the right on this row and below, with their weights over `divisor`.
struct Kernel {
    taps: &'static [Tap],
    divisor: f32,
}

/// Atkinson's taps.
const ATKINSON: [Tap; 6] = [
    Tap { dx: 1, dy: 0, weight: 1 },
    Tap { dx: 2, dy: 0, weight: 1 },
    Tap { dx: -1, dy: 1, weight: 1 },
    Tap { dx: 0, dy: 1, weight: 1 },
    Tap { dx: 1, dy: 1, weight: 1 },
    Tap { dx: 0, dy: 2, weight: 1 },
];

/// Floyd–Steinberg's taps.
const FLOYD: [Tap; 4] = [
    Tap { dx: 1, dy: 0, weight: 7 },
    Tap { dx: -1, dy: 1, weight: 3 },
    Tap { dx: 0, dy: 1, weight: 5 },
    Tap { dx: 1, dy: 1, weight: 1 },
];

/// Atkinson passes on only six eighths of the error, which is what gives the Mac's crisp, contrasty look.
fn kernel_for(style: i32) -> Kernel {
    if style == DITHER_ATKINSON {
        Kernel { taps: &ATKINSON, divisor: 8.0 }
    } else {
        Kernel { taps: &FLOYD, divisor: 16.0 }
    }
}

/// Quantizes `v` to `levels` tones.
fn quantize(v: f32, levels: i32) -> f32 {
    let steps = (levels - 1) as f32;
    (clamp01(v) * steps).round() / steps
}

/// Diffuses each plane in serpentine order, so the error's drift doesn't streak to one side.
fn diffuse(plane: &mut [f32], alpha: &[u8], width: usize, height: usize, p: &DitherParams) {
    let k = kernel_for(p.style);
    for y in 0..height {
        let reverse = (y & 1) != 0;
        for i in 0..width {
            let x = if reverse { width - 1 - i } else { i };
            let at = y * width + x;
            if alpha[at] == 0 {
                continue;
            }
            let old = plane[at];
            let q = quantize(old, p.levels);
            plane[at] = q;
            let error = (old - q) * p.diffusion / k.divisor;
            for tap in k.taps {
                let nx = x as i64 + if reverse { -(tap.dx as i64) } else { tap.dx as i64 };
                let ny = y as i64 + tap.dy as i64;
                if nx < 0 || nx >= width as i64 || ny >= height as i64 {
                    continue;
                }
                plane[ny as usize * width + nx as usize] += error * tap.weight as f32;
            }
        }
    }
}

/// The 8 × 8 ordered (Bayer) matrix.
const BAYER8: [u8; 64] = [
    0, 32, 8, 40, 2, 34, 10, 42, 48, 16, 56, 24, 50, 18, 58, 26, 12, 44, 4, 36, 14, 46, 6, 38, 60,
    28, 52, 20, 62, 30, 54, 22, 3, 35, 11, 43, 1, 33, 9, 41, 51, 19, 59, 27, 49, 17, 57, 25, 15, 47,
    7, 39, 13, 45, 5, 37, 63, 31, 55, 23, 61, 29, 53, 21,
];

/// The ordered threshold for a pixel, in [0, 1). Smaller Bayer matrices are the top-left corners of the 8 × 8 one,
/// rescaled, which is how the recursive construction nests them.
fn ordered_threshold(style: i32, x: usize, y: usize) -> f32 {
    match style {
        DITHER_BAYER_2 => {
            const M: [u8; 4] = [0, 2, 3, 1];
            (M[(y & 1) * 2 + (x & 1)] as f32 + 0.5) / 4.0
        }
        DITHER_BAYER_4 => {
            const M: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
            (M[(y & 3) * 4 + (x & 3)] as f32 + 0.5) / 16.0
        }
        _ => (BAYER8[(y & 7) * 8 + (x & 7)] as f32 + 0.5) / 64.0,
    }
}

/// Ordered (Bayer) dithering of one sample.
fn ordered(v: f32, threshold: f32, levels: i32) -> f32 {
    let steps = (levels - 1) as f32;
    let q = (clamp01(v) * steps + threshold).floor();
    (if q > steps { steps } else { q }) / steps
}

/// How much of a halftone cell a point must be covered by before it's marked, for each screen shape. `u` and `v`
/// run from −0.5 to 0.5 across the cell; the shapes grow from its middle as coverage rises.
fn spot(style: i32, u: f32, v: f32) -> f32 {
    let au = u.abs();
    let av = v.abs();
    match style {
        DITHER_DOTS => 3.14159265 * (u * u + v * v),
        DITHER_LINES => av * 2.0,
        _ => au + av,
    }
}

/// Old Mac fill patterns, 8 × 8, one byte per row with the leftmost pixel in the top bit, from sparsest to fullest.
const PATTERNS: [[u8; 8]; 17] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x80, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00],
    [0x88, 0x00, 0x22, 0x00, 0x88, 0x00, 0x22, 0x00],
    [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01],
    [0x88, 0x22, 0x88, 0x22, 0x88, 0x22, 0x88, 0x22],
    [0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00],
    [0x11, 0x22, 0x44, 0x88, 0x11, 0x22, 0x44, 0x88],
    [0xAA, 0x00, 0xAA, 0x00, 0xAA, 0x00, 0xAA, 0x00],
    [0x88, 0x55, 0x22, 0x55, 0x88, 0x55, 0x22, 0x55],
    [0xFF, 0x80, 0x80, 0x80, 0xFF, 0x08, 0x08, 0x08],
    [0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55],
    [0x81, 0x42, 0x24, 0x18, 0x18, 0x24, 0x42, 0x81],
    [0x77, 0xAA, 0xDD, 0xAA, 0x77, 0xAA, 0xDD, 0xAA],
    [0xEE, 0xDD, 0xBB, 0x77, 0xEE, 0xDD, 0xBB, 0x77],
    [0x77, 0xFF, 0xDD, 0xFF, 0x77, 0xFF, 0xDD, 0xFF],
    [0x7F, 0xFF, 0xFF, 0xFF, 0xF7, 0xFF, 0xFF, 0xFF],
    [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
];

/// How many fill patterns there are.
const PATTERN_COUNT: i32 = PATTERNS.len() as i32;

/// Writes one straight-sRGB color into a premultiplied pixel, keeping the pixel's alpha.
fn write_pixel(row: &mut [u8], x: usize, r: f32, g: f32, b: f32) {
    let at = x * 4;
    let a = row[at + 3] as f32 / 255.0;
    row[at] = (clamp01(r) * a * 255.0).round() as u8;
    row[at + 1] = (clamp01(g) * a * 255.0).round() as u8;
    row[at + 2] = (clamp01(b) * a * 255.0).round() as u8;
}

/// Allocates `len` zeroed `f32`s the fallible way, so the kernels can still report out-of-memory like the C did.
fn try_alloc_f32(len: usize) -> Option<Vec<f32>> {
    let mut values: Vec<f32> = Vec::new();
    values.try_reserve_exact(len).ok()?;
    values.resize(len, 0.0);
    Some(values)
}

/// Allocates `len` zeroed bytes the fallible way.
fn try_alloc_u8(len: usize) -> Option<Vec<u8>> {
    let mut values: Vec<u8> = Vec::new();
    values.try_reserve_exact(len).ok()?;
    values.resize(len, 0);
    Some(values)
}

/// Allocates `len` zeroed `i32`s the fallible way.
fn try_alloc_i32(len: usize) -> Option<Vec<i32>> {
    let mut values: Vec<i32> = Vec::new();
    values.try_reserve_exact(len).ok()?;
    values.resize(len, 0);
    Some(values)
}

/// Reads one premultiplied RGBA row into the single gray tone plane (and the alpha plane).
fn tone_row_gray(row: &[u8], tone: &mut [f32], alpha: &mut [u8], width: usize, gamma: f32, contrast: f32) {
    for x in 0..width {
        let at = x * 4;
        alpha[x] = row[at + 3];
        let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
        if row[at + 3] != 0 {
            let scale = 1.0 / row[at + 3] as f32;
            r = row[at] as f32 * scale;
            g = row[at + 1] as f32 * scale;
            b = row[at + 2] as f32 * scale;
        }
        tone[x] = adjust_tone(0.2126 * r + 0.7152 * g + 0.0722 * b, gamma, contrast);
    }
}

/// Reads one premultiplied RGBA row into its three color tone planes, keeping the untouched color in `source`.
fn tone_row_color(
    row: &[u8],
    tone: [&mut [f32]; 3],
    alpha: &mut [u8],
    source: &mut [f32],
    width: usize,
    gamma: f32,
    contrast: f32,
) {
    for x in 0..width {
        let at = x * 4;
        alpha[x] = row[at + 3];
        let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
        if row[at + 3] != 0 {
            let scale = 1.0 / row[at + 3] as f32;
            r = row[at] as f32 * scale;
            g = row[at + 1] as f32 * scale;
            b = row[at + 2] as f32 * scale;
        }
        tone[0][x] = adjust_tone(r, gamma, contrast);
        tone[1][x] = adjust_tone(g, gamma, contrast);
        tone[2][x] = adjust_tone(b, gamma, contrast);
        source[x * 3] = r;
        source[x * 3 + 1] = g;
        source[x * 3 + 2] = b;
    }
}

/// Dithers premultiplied RGBA pixels (4 bytes per pixel, `stride` bytes per row) in place. Alpha is kept and fully
/// transparent pixels are left alone. Returns 0 if working memory couldn't be had.
pub fn dither_apply(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    p: &DitherParams,
) -> i32 {
    let count = width * height;
    if count == 0 {
        return 1;
    }
    let original = p.original_colors != 0;
    let planes: usize = if original { 3 } else { 1 };
    let mut tone = match try_alloc_f32(count * planes) {
        Some(values) => values,
        None => return 0,
    };
    let mut alpha = match try_alloc_u8(count) {
        Some(values) => values,
        None => return 0,
    };
    // The image's own colors, unadjusted: halftone dots and glyphs take them in Original mode.
    let mut source = if original {
        match try_alloc_f32(count * 3) {
            Some(values) => values,
            None => return 0,
        }
    } else {
        Vec::new()
    };

    let gamma = (p.density * 1.5).exp2();
    let contrast = if p.contrast >= 0.0 {
        1.0 / (1.0 - 0.95 * p.contrast)
    } else {
        1.0 + p.contrast
    };

    // Each row writes only its own tone, alpha and source samples, so the rows run at once.
    if original {
        let (tone0, rest) = tone.split_at_mut(count);
        let (tone1, tone2) = rest.split_at_mut(count);
        rgba.par_chunks_mut(stride)
            .zip(tone0.par_chunks_mut(width))
            .zip(tone1.par_chunks_mut(width))
            .zip(tone2.par_chunks_mut(width))
            .zip(alpha.par_chunks_mut(width))
            .zip(source.par_chunks_mut(width * 3))
            .for_each(|(((((row, t0), t1), t2), a), s)| {
                tone_row_color(row, [t0, t1, t2], a, s, width, gamma, contrast);
            });
    } else {
        rgba.par_chunks_mut(stride)
            .zip(tone.par_chunks_mut(width))
            .zip(alpha.par_chunks_mut(width))
            .for_each(|((row, t0), a)| {
                tone_row_gray(row, t0, a, width, gamma, contrast);
            });
    }

    let dark = [
        p.dark[0] as f32 / 255.0,
        p.dark[1] as f32 / 255.0,
        p.dark[2] as f32 / 255.0,
    ];
    let light = [
        p.light[0] as f32 / 255.0,
        p.light[1] as f32 / 255.0,
        p.light[2] as f32 / 255.0,
    ];
    let style = p.style;
    let levels = if p.levels < 2 {
        2
    } else if p.levels > 16 {
        16
    } else {
        p.levels
    };

    if style <= DITHER_BAYER_8 {
        // Diffusion and ordered dithering: each plane is quantized to `levels` tones, then mapped to colors.
        if style <= DITHER_FLOYD_STEINBERG {
            let mut local = *p;
            local.levels = levels;
            for c in 0..planes {
                diffuse(&mut tone[c * count..(c + 1) * count], &alpha, width, height, &local);
            }
        } else {
            for c in 0..planes {
                for y in 0..height {
                    for x in 0..width {
                        let at = y * width + x;
                        if alpha[at] != 0 {
                            let index = c * count + at;
                            tone[index] = ordered(tone[index], ordered_threshold(style, x, y), levels);
                        }
                    }
                }
            }
        }
        for y in 0..height {
            let row = &mut rgba[y * stride..][..width * 4];
            for x in 0..width {
                let at = y * width + x;
                if alpha[at] == 0 {
                    continue;
                }
                if original {
                    write_pixel(row, x, tone[at], tone[count + at], tone[2 * count + at]);
                } else {
                    let t = tone[at];
                    write_pixel(
                        row,
                        x,
                        dark[0] + (light[0] - dark[0]) * t,
                        dark[1] + (light[1] - dark[1]) * t,
                        dark[2] + (light[2] - dark[2]) * t,
                    );
                }
            }
        }
    } else if style == DITHER_SCANLINES {
        // A CRT: each line scans the image, its tone along the line the average of the rows it covers. The beam glows
        // brighter and blooms thicker where the picture is light, and the screen between lines stays dark.
        let spacing = if p.cell < 2 { 2 } else { p.cell as usize };
        let middle = spacing as f32 / 2.0;
        let dots = clamp01(p.dots);
        let lines = (height + spacing - 1) / spacing;
        let screen = dark;
        let phosphor = light;
        // Each line is drawn on its own, so the lines are shared out across the cores.
        let line_bands = bands(lines);
        let mut scans: Vec<Vec<f32>> = Vec::with_capacity(line_bands.len());
        for _ in 0..line_bands.len() {
            match try_alloc_f32(width * planes) {
                Some(scan) => scans.push(scan),
                None => return 0,
            }
        }
        // The lines own disjoint rows, so each band gets its rows up front.
        let mut rows: Vec<&mut [u8]> = Vec::with_capacity(height);
        {
            let mut rest: &mut [u8] = rgba;
            for _ in 0..height {
                let take = stride.min(rest.len());
                let (head, tail) = rest.split_at_mut(take);
                rows.push(head);
                rest = tail;
            }
        }
        let mut band_rows: Vec<Vec<&mut [u8]>> = Vec::with_capacity(line_bands.len());
        for &(first, last) in &line_bands {
            let take = (last * spacing).min(height) - first * spacing;
            band_rows.push(rows.drain(..take).collect());
        }
        line_bands
            .into_par_iter()
            .zip(scans.into_par_iter())
            .zip(band_rows.into_par_iter())
            .for_each(|(((first, last), mut scan), mut band_rows)| {
                let first_row = first * spacing;
                for line in first..last {
                    let top = line * spacing;
                    let bottom = (top + spacing).min(height);
                    // Wobble: each line is pushed sideways, a slow wave down the screen with a quicker one over it,
                    // as a CRT's picture wavers when its sync drifts.
                    let wave =
                        (line as f32 * 0.45).sin() * 0.7 + (line as f32 * 1.7 + 1.3).sin() * 0.3;
                    let shift = (p.wobble * wave).round() as i64;
                    for x in 0..width {
                        let mut sum = [0.0f32; 3];
                        let mut n = 0i32;
                        let sx = x as i64 - shift;
                        if sx >= 0 && sx < width as i64 {
                            let sx = sx as usize;
                            for y in top..bottom {
                                let at = y * width + sx;
                                if alpha[at] == 0 {
                                    continue;
                                }
                                for c in 0..planes {
                                    sum[c] += tone[c * count + at];
                                }
                                n += 1;
                            }
                        }
                        for c in 0..planes {
                            scan[c * width + x] = if n != 0 { sum[c] / n as f32 } else { 0.0 };
                        }
                    }
                    for y in top..bottom {
                        let row = &mut *band_rows[y - first_row];
                        let offset = (((y - top) as f32) + 0.5 - middle).abs();
                        for x in 0..width {
                            if alpha[y * width + x] == 0 {
                                continue;
                            }
                            // Dots: the line breaks into beads, one every line spacing, each lit in the color at its
                            // middle.
                            let along = ((x as f32 + 0.5) % spacing as f32) - middle;
                            let centered = (x as f32 - along * dots).round() as i64;
                            let at = if centered < 0 {
                                0
                            } else if centered as usize >= width {
                                width - 1
                            } else {
                                centered as usize
                            };
                            let (r, g, b, t);
                            if original {
                                r = scan[at];
                                g = scan[width + at];
                                b = scan[2 * width + at];
                                t = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                            } else {
                                t = scan[at];
                                r = screen[0] + (phosphor[0] - screen[0]) * t;
                                g = screen[1] + (phosphor[1] - screen[1]) * t;
                                b = screen[2] + (phosphor[2] - screen[2]) * t;
                            }
                            // The beam is driven brighter than the picture, making up for the dark screen between
                            // lines.
                            let (r, g, b) = (r * 1.35, g * 1.35, b * 1.35);
                            // Half the beam's height: a thin line in the shadows, most of the way across in the
                            // highlights, always leaving dark screen between lines.
                            let beam = middle * (0.2 + 0.5 * clamp01(t).sqrt());
                            let across = along * dots;
                            let distance = (offset * offset + across * across).sqrt();
                            let cover = clamp01(beam - distance + 0.5);
                            // Between the lines, the screen: black in Original, else the dark color.
                            let br = if original { 0.0 } else { screen[0] };
                            let bg = if original { 0.0 } else { screen[1] };
                            let bb = if original { 0.0 } else { screen[2] };
                            write_pixel(
                                row,
                                x,
                                br + (r - br) * cover,
                                bg + (g - bg) * cover,
                                bb + (b - bb) * cover,
                            );
                        }
                    }
                }
            });
    } else {
        // Marks (halftone shapes, patterns, glyphs) cover as much of each spot as the tone calls for. On light, they
        // stand for darkness and are drawn in the dark color; light on dark, the reverse.
        let marks_owned: Vec<f32>;
        let marks: &[f32] = if original {
            let mut luma = match try_alloc_f32(count) {
                Some(values) => values,
                None => return 0,
            };
            for i in 0..count {
                luma[i] = 0.2126 * tone[i] + 0.7152 * tone[count + i] + 0.0722 * tone[2 * count + i];
            }
            marks_owned = luma;
            &marks_owned
        } else {
            &tone[..count]
        };
        let cell = if p.cell < 2 { 2 } else { p.cell };
        let cos_a = p.angle.cos();
        let sin_a = p.angle.sin();
        let (ink, paper) = if p.light_on_dark != 0 { (light, dark) } else { (dark, light) };
        // Glyphs: each cell shares one, picked from the cell's average tone, worked out once per cell.
        let gw = if p.glyph_width < 1 { 1 } else { p.glyph_width as usize };
        let gh = if p.glyph_height < 1 { 1 } else { p.glyph_height as usize };
        let columns = (width + gw - 1) / gw;
        let cell_rows = (height + gh - 1) / gh;
        let mut picked: Vec<i32> = Vec::new();
        if style == DITHER_GLYPHS && p.glyph_count > 0 {
            let glyph_count = p.glyph_count as usize;
            let mut choices = match try_alloc_i32(columns * cell_rows) {
                Some(values) => values,
                None => return 0,
            };
            for row in 0..cell_rows {
                for column in 0..columns {
                    let mut sum = 0.0f32;
                    let mut n = 0i32;
                    for yy in row * gh..((row + 1) * gh).min(height) {
                        for xx in column * gw..((column + 1) * gw).min(width) {
                            let i = yy * width + xx;
                            if alpha[i] != 0 {
                                sum += marks[i];
                                n += 1;
                            }
                        }
                    }
                    let t = if n != 0 { sum / n as f32 } else { 1.0 };
                    let wanted = (if p.light_on_dark != 0 { t } else { 1.0 - t })
                        * p.glyph_coverage[glyph_count - 1];
                    let mut best = 0i32;
                    let mut best_distance = 2.0f32;
                    for g in 0..glyph_count {
                        let d = (p.glyph_coverage[g] - wanted).abs();
                        if d < best_distance {
                            best_distance = d;
                            best = g as i32;
                        }
                    }
                    choices[row * columns + column] = best;
                }
            }
            picked = choices;
        }
        // Original colors: marks take the pixel's own color, on black (light on dark) or white.
        let paper_original = if p.light_on_dark != 0 { 0.0 } else { 1.0 };
        for y in 0..height {
            let row = &mut rgba[y * stride..][..width * 4];
            for x in 0..width {
                let at = y * width + x;
                if alpha[at] == 0 {
                    continue;
                }
                let amount: f32;
                if !picked.is_empty() {
                    let glyph = picked[(y / gh) * columns + x / gw] as usize;
                    amount = p.glyphs[glyph * gw * gh + (y % gh) * gw + x % gw] as f32 / 255.0;
                } else if style == DITHER_PATTERNS {
                    let t = marks[at];
                    let coverage = if p.light_on_dark != 0 { t } else { 1.0 - t };
                    let index = (coverage * (PATTERN_COUNT - 1) as f32).round() as i32;
                    amount = ((PATTERNS[index as usize][y & 7] >> (7 - (x & 7))) & 1) as f32;
                } else {
                    let fx = x as f32 + 0.5;
                    let fy = y as f32 + 0.5;
                    let mut u = (fx * cos_a + fy * sin_a) / cell as f32;
                    let mut v = (-fx * sin_a + fy * cos_a) / cell as f32;
                    u -= u.floor() + 0.5;
                    v -= v.floor() + 0.5;
                    let t = marks[at];
                    amount = if (if p.light_on_dark != 0 { t } else { 1.0 - t }) > spot(style, u, v) {
                        1.0
                    } else {
                        0.0
                    };
                }
                if original {
                    let s = &source[at * 3..at * 3 + 3];
                    write_pixel(
                        row,
                        x,
                        paper_original + (s[0] - paper_original) * amount,
                        paper_original + (s[1] - paper_original) * amount,
                        paper_original + (s[2] - paper_original) * amount,
                    );
                } else {
                    write_pixel(
                        row,
                        x,
                        paper[0] + (ink[0] - paper[0]) * amount,
                        paper[1] + (ink[1] - paper[1]) * amount,
                        paper[2] + (ink[2] - paper[2]) * amount,
                    );
                }
            }
        }
    }
    1
}

/// Turns each `block` × `block` square of premultiplied RGBA pixels into a round dot in its own color on `gap`
/// (straight sRGB), like the lit pixels of a dot-matrix screen. The dot's edge is smoothed and alpha is kept.
pub fn dither_dots(rgba: &mut [u8], width: usize, height: usize, stride: usize, block: i32, gap: &[u8]) {
    if block < 2 {
        return;
    }
    let block = block as usize;
    let radius = block as f32 * 0.42;
    let middle = block as f32 / 2.0;
    for y in 0..height {
        let row = &mut rgba[y * stride..][..width * 4];
        let dy = (y % block) as f32 + 0.5 - middle;
        for x in 0..width {
            let at = x * 4;
            if row[at + 3] == 0 {
                continue;
            }
            let dx = (x % block) as f32 + 0.5 - middle;
            let cover = clamp01(radius - (dx * dx + dy * dy).sqrt() + 0.5);
            if cover >= 1.0 {
                continue;
            }
            for c in 0..3 {
                row[at + c] = ((row[at + c] as f32 * cover
                    + gap[c] as f32 * row[at + 3] as f32 / 255.0 * (1.0 - cover))
                    .round()) as u8;
            }
        }
    }
}

/// Adds `glow` (premultiplied RGBA, same layout) over the pixels at `amount`, never past their own alpha.
pub fn dither_glow(
    rgba: &mut [u8],
    glow: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    amount: f32,
) {
    // Each row is drawn on its own, so the rows are shared out across the cores.
    rgba.par_chunks_mut(stride)
        .zip(glow.par_chunks(stride))
        .take(height)
        .for_each(|(row, light_row)| {
            for x in (0..width * 4).step_by(4) {
                let a = row[x + 3] as f32;
                for c in 0..3 {
                    let v = row[x + c] as f32 + light_row[x + c] as f32 * amount * a / 255.0;
                    row[x + c] = (if v > a { a } else { v }).round() as u8;
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A base parameter set; tests change only what they exercise.
    fn base_params(style: i32) -> DitherParams<'static> {
        DitherParams {
            style,
            levels: 2,
            diffusion: 1.0,
            density: 0.0,
            contrast: 0.0,
            cell: 8,
            angle: 0.0,
            light_on_dark: 0,
            original_colors: 0,
            dark: [0, 0, 0],
            light: [255, 255, 255],
            glyph_width: 1,
            glyph_height: 1,
            glyphs: &[],
            glyph_coverage: &[],
            glyph_count: 0,
            dots: 0.0,
            wobble: 0.0,
        }
    }

    /// A `width` × `height` opaque gray image, as the Swift's `DitherTests` fills one.
    fn gray_image(width: usize, height: usize, gray: u8) -> Vec<u8> {
        (0..width * height).flat_map(|_| [gray, gray, gray, 255]).collect()
    }

    /// One column of a Scanlines render of a flat gray — column 5, as the Swift's
    /// `scanlines(gray:spacing:glow:)` samples — as brightness per row.
    fn scanlines_column(gray: u8, spacing: i32) -> Vec<u8> {
        let (width, height) = (16usize, 32usize);
        let mut pixels = gray_image(width, height, gray);
        let params = DitherParams { cell: spacing, ..base_params(DITHER_SCANLINES) };
        assert_eq!(dither_apply(&mut pixels, width, height, width * 4, &params), 1);
        (0..height).map(|y| pixels[(y * width + 5) * 4]).collect()
    }

    #[test]
    fn style_constants_keep_the_c_values() {
        assert_eq!(
            [
                DITHER_ATKINSON,
                DITHER_FLOYD_STEINBERG,
                DITHER_BAYER_2,
                DITHER_BAYER_4,
                DITHER_BAYER_8,
                DITHER_DOTS,
                DITHER_LINES,
                DITHER_DIAMONDS,
                DITHER_PATTERNS,
                DITHER_GLYPHS,
                DITHER_SCANLINES,
            ],
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
    }

    #[test]
    fn bayer_2_splits_mid_gray() {
        let mut px = vec![128, 128, 128, 255, 128, 128, 128, 255];
        let p = base_params(DITHER_BAYER_2);
        assert_eq!(dither_apply(&mut px, 2, 1, 8, &p), 1);
        assert_eq!(px, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn transparent_pixels_are_left_alone_and_alpha_is_kept() {
        // A fully transparent pixel's color is not touched, even though it is not premultiplied black.
        let mut px = vec![200, 100, 50, 0, 128, 128, 128, 255];
        let p = base_params(DITHER_FLOYD_STEINBERG);
        assert_eq!(dither_apply(&mut px, 2, 1, 8, &p), 1);
        assert_eq!(&px[0..4], &[200, 100, 50, 0]);
        // The opaque pixel quantizes to the light color and keeps its alpha.
        assert_eq!(&px[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn every_style_is_deterministic_and_keeps_alpha() {
        let src: Vec<u8> = (0..12 * 4)
            .map(|i| match i % 4 {
                0 => (i * 7 % 256) as u8,
                1 => (i * 11 % 256) as u8,
                2 => (i * 13 % 256) as u8,
                _ => 255,
            })
            .collect();
        for style in DITHER_ATKINSON..=DITHER_SCANLINES {
            let mut a = src.clone();
            let mut b = src.clone();
            let p = base_params(style);
            assert_eq!(dither_apply(&mut a, 4, 3, 16, &p), 1, "style {style}");
            assert_eq!(dither_apply(&mut b, 4, 3, 16, &p), 1, "style {style}");
            assert_eq!(a, b, "style {style} is not deterministic");
            for i in (0..a.len()).step_by(4) {
                assert_eq!(a[i + 3], src[i + 3], "style {style} changed an alpha");
            }
        }
    }

    #[test]
    fn halftone_marks_are_drawn_in_the_dark_color() {
        // A black image under the dots screen is inked at each cell's middle, while the corners stay paper.
        let mut px = vec![0u8; 8 * 8 * 4];
        for pixel in px.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        let p = base_params(DITHER_DOTS);
        assert_eq!(dither_apply(&mut px, 8, 8, 32, &p), 1);
        assert_eq!(&px[(3 * 8 + 3) * 4..(3 * 8 + 3) * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&px[(0 * 8 + 0) * 4..(0 * 8 + 0) * 4 + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn scanlines_keep_alpha_and_determinism() {
        let src = vec![255u8; 8 * 8 * 4];
        let mut a = src.clone();
        let mut b = src.clone();
        let p = base_params(DITHER_SCANLINES);
        assert_eq!(dither_apply(&mut a, 8, 8, 32, &p), 1);
        assert_eq!(dither_apply(&mut b, 8, 8, 32, &p), 1);
        assert_eq!(a, b);
        for i in (0..a.len()).step_by(4) {
            assert_eq!(a[i + 3], 255);
        }
    }

    #[test]
    fn dither_dots_rounds_a_block_into_a_dot() {
        let mut px = vec![255u8; 4 * 4 * 4];
        dither_dots(&mut px, 4, 4, 16, 4, &[0, 0, 0]);
        // The middle of the 4 × 4 block is inside the dot and stays; the corner fades towards the gap color.
        assert_eq!(&px[5 * 4..5 * 4 + 4], &[255, 255, 255, 255]);
        assert_eq!(&px[0..4], &[15, 15, 15, 255]);
    }

    #[test]
    fn dither_dots_ignores_blocks_under_two() {
        let mut px = vec![1, 2, 3, 4];
        dither_dots(&mut px, 1, 1, 4, 1, &[9, 9, 9]);
        assert_eq!(px, vec![1, 2, 3, 4]);
    }

    #[test]
    fn glow_adds_scaled_light_and_never_passes_alpha() {
        let mut px = vec![10, 20, 30, 100, 200, 200, 200, 255];
        let glow = vec![50, 60, 70, 255, 255, 255, 255, 255];
        dither_glow(&mut px, &glow, 2, 1, 8, 1.0);
        assert_eq!(&px[0..4], &[30, 44, 57, 100]);
        assert_eq!(&px[4..8], &[255, 255, 255, 255]);
    }

    /// Scanlines is a CRT: lines of light on a dark screen, every Line Spacing pixels, thicker and
    /// brighter where the picture is light.
    #[test]
    fn scanlines_are_lines_of_light_that_bloom_with_brightness() {
        let white = scanlines_column(255, 8);
        let gray = scanlines_column(89, 8); // 0.35 of 255
        let black = scanlines_column(0, 8);
        for band in 0..4 {
            let rows = &white[band * 8..band * 8 + 8];
            assert!(
                rows[3] == 255 && rows[4] == 255,
                "a white line is lit through its middle: {rows:?}"
            );
            assert!(
                rows.iter().any(|value| *value < 40),
                "with dark screen between lines: {rows:?}"
            );
        }
        let total = |values: &[u8]| values.iter().map(|value| *value as u32).sum::<u32>();
        assert!(
            total(&gray) * 2 < total(&white),
            "a gray line is thinner and dimmer than a white one"
        );
        assert!(black.iter().all(|value| *value == 0));
    }

    /// Dots break each line into beads: along a lit line's middle, dark gaps come every line spacing.
    #[test]
    fn dots_break_the_lines_into_beads() {
        let (width, height) = (64usize, 16usize);
        let middle = |dots: f32| {
            let mut pixels = gray_image(width, height, 255);
            let params = DitherParams { cell: 8, dots, ..base_params(DITHER_SCANLINES) };
            assert_eq!(dither_apply(&mut pixels, width, height, width * 4, &params), 1);
            (0..width).map(|x| pixels[(4 * width + x) * 4]).collect::<Vec<u8>>()
        };
        assert!(middle(0.0).iter().all(|value| *value == 255));
        let lit = middle(1.0);
        assert!(
            lit[3] == 255 && lit[4] == 255 && lit[0] < 60 && lit[8] < 60,
            "beads every 8 pixels: {lit:?}"
        );
    }

    /// Wobble pushes lines sideways by different amounts: a vertical edge no longer lines up from
    /// line to line.
    #[test]
    fn wobble_moves_lines_sideways() {
        let (width, height) = (64usize, 64usize);
        let edges = |wobble: f32| {
            let mut pixels = vec![0u8; width * height * 4];
            for y in 0..height {
                for x in 32..width {
                    let at = (y * width + x) * 4;
                    pixels[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            let params = DitherParams { cell: 8, wobble, ..base_params(DITHER_SCANLINES) };
            assert_eq!(dither_apply(&mut pixels, width, height, width * 4, &params), 1);
            (0..8)
                .map(|line| {
                    (0..width)
                        .find(|x| pixels[((line * 8 + 4) * width + *x) * 4] > 128)
                        .map(|x| x as i64)
                        .unwrap_or(-1)
                })
                .collect::<BTreeSet<i64>>()
        };
        assert_eq!(edges(0.0), BTreeSet::from([32]));
        let wobbling = edges(12.0);
        assert!(wobbling.len() >= 3, "the edge moves line to line: {wobbling:?}");
    }
}
