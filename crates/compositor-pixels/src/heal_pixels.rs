//! Spot healing, ported from `references/Compositor/Compositor/Rendering/HealPixels.c`.

/// Outside the spot and its ring: neither solved nor used as a boundary.
const OUTSIDE: u8 = 0;
/// A pixel of the ring around the spot, whose value is held fixed.
const RING: u8 = 1;
/// A pixel of the spot itself, to be solved.
const HOLE: u8 = 2;

/// Half-open bounds of nonzero bytes in a gray bitmap; all zero when empty.
pub fn heal_coverage_bounds(
    gray: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    bounds: &mut [i64; 4],
) {
    let mut x0 = width as i64;
    let mut y0 = height as i64;
    let mut x1 = 0i64;
    let mut y1 = 0i64;
    for y in 0..height {
        let row = y * stride;
        for x in 0..width {
            if gray[row + x] == 0 {
                continue;
            }
            if (x as i64) < x0 {
                x0 = x as i64;
            }
            if x as i64 + 1 > x1 {
                x1 = x as i64 + 1;
            }
            if (y as i64) < y0 {
                y0 = y as i64;
            }
            if y as i64 + 1 > y1 {
                y1 = y as i64 + 1;
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        x0 = 0;
        y0 = 0;
        x1 = 0;
        y1 = 0;
    }
    bounds[0] = x0;
    bounds[1] = y0;
    bounds[2] = x1;
    bounds[3] = y1;
}

/// A 32-bit avalanche hash.
fn heal_hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    x
}

/// A deterministic value in `[0, 1)` from a key.
fn heal_unit(key: u32) -> f64 {
    (heal_hash(key) >> 8) as f64 / 16777216.0
}

/// Mean squared difference between the ring around the spot and the ring around the patch
/// offset by (dx, dy). Infinite when the patch would overlap the spot or leave the image.
#[allow(clippy::too_many_arguments)]
fn heal_score(
    rgba: &[u8],
    stride: usize,
    role: &[u8],
    wx0: i64,
    wy0: i64,
    ww: i64,
    wh: i64,
    dx: i64,
    dy: i64,
    w: i64,
    h: i64,
) -> f64 {
    if dx.abs() < ww && dy.abs() < wh {
        return f64::INFINITY;
    }
    if wx0 + dx < 0 || wy0 + dy < 0 || wx0 + ww + dx > w || wy0 + wh + dy > h {
        return f64::INFINITY;
    }
    let mut sum = 0.0f64;
    let mut n = 0i64;
    for y in 0..wh {
        for x in 0..ww {
            if role[(y * ww + x) as usize] != RING {
                continue;
            }
            let t = (wy0 + y) as usize * stride + (wx0 + x) as usize * 4;
            let s = (wy0 + y + dy) as usize * stride + (wx0 + x + dx) as usize * 4;
            for c in 0..4 {
                let d = rgba[t + c] as f64 - rgba[s + c] as f64;
                sum += d * d;
            }
            n += 1;
        }
    }
    if n != 0 { sum / n as f64 } else { f64::INFINITY }
}

/// Solves for smooth values over HOLE pixels, fixed to the RING values around them. A coarser
/// copy is solved first and used as the starting point, so large spots settle in few passes.
fn heal_solve(value: &mut [f32], role: &[u8], w: i64, h: i64, depth: i32) {
    let mut iterations = 300;
    if w > 32 && h > 32 && depth < 16 {
        let cw = (w + 1) / 2;
        let ch = (h + 1) / 2;
        let mut coarse = vec![0.0f32; (cw * ch * 4) as usize];
        let mut coarse_role = vec![0u8; (cw * ch) as usize];
        for y in 0..ch {
            for x in 0..cw {
                let mut known = 0i32;
                let mut hole = 0i32;
                let mut known_sum = [0.0f32; 4];
                let mut hole_sum = [0.0f32; 4];
                for j in 0..2i64 {
                    for i in 0..2i64 {
                        let fx = x * 2 + i;
                        let fy = y * 2 + j;
                        if fx >= w || fy >= h {
                            continue;
                        }
                        let p = (fy * w + fx) as usize;
                        if role[p] == RING {
                            known += 1;
                            for c in 0..4 {
                                known_sum[c] += value[p * 4 + c];
                            }
                        } else if role[p] == HOLE {
                            hole += 1;
                            for c in 0..4 {
                                hole_sum[c] += value[p * 4 + c];
                            }
                        }
                    }
                }
                let q = (y * cw + x) as usize;
                if known != 0 {
                    coarse_role[q] = RING;
                    for c in 0..4 {
                        coarse[q * 4 + c] = known_sum[c] / known as f32;
                    }
                } else if hole != 0 {
                    coarse_role[q] = HOLE;
                    for c in 0..4 {
                        coarse[q * 4 + c] = hole_sum[c] / hole as f32;
                    }
                }
            }
        }
        heal_solve(&mut coarse, &coarse_role, cw, ch, depth + 1);
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) as usize;
                let q = ((y / 2) * cw + x / 2) as usize;
                if role[p] == HOLE && coarse_role[q] == HOLE {
                    value[p * 4..p * 4 + 4].copy_from_slice(&coarse[q * 4..q * 4 + 4]);
                }
            }
        }
        iterations = 40;
    }
    let omega = 1.8f32;
    for _ in 0..iterations {
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) as usize;
                if role[p] != HOLE {
                    continue;
                }
                let mut sum = [0.0f32; 4];
                let mut n = 0i32;
                let neighbors = [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)];
                for (nx, ny) in neighbors {
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        continue;
                    }
                    let q = (ny * w + nx) as usize;
                    if role[q] == OUTSIDE {
                        continue;
                    }
                    for c in 0..4 {
                        sum[c] += value[q * 4 + c];
                    }
                    n += 1;
                }
                if n == 0 {
                    continue;
                }
                for c in 0..4 {
                    value[p * 4 + c] += omega * (sum[c] / n as f32 - value[p * 4 + c]);
                }
            }
        }
    }
}

/// Spot healing, in place, over premultiplied RGBA (4 bytes per pixel, `stride` bytes per row).
/// `coverage` (width * height bytes, 0–255) marks what to heal.
///   mode 0, Content-Aware: copies texture from the nearby patch whose surrounding ring of pixels
///           best matches the ring around the spot;
///   mode 1, Create Texture: fills smoothly from the spot's edges and adds grain matching the
///           detail around it;
///   mode 2, Proximity Match: like 0, but takes the closest good patch.
/// Copied texture is blended so it meets the surrounding tone exactly: the difference along the
/// spot's edge is spread smoothly across it (a membrane fill). The result replaces the original
/// by coverage × opacity. Returns 0, or -1 when memory runs out.
pub fn spot_heal(
    rgba: &mut [u8],
    coverage: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    opacity: f32,
    mode: i32,
    seed: u32,
) -> i32 {
    let w = width as i64;
    let h = height as i64;
    let mut bounds = [0i64; 4];
    heal_coverage_bounds(coverage, width, height, width, &mut bounds);
    if bounds[2] <= bounds[0] {
        return 0;
    }
    let bw = bounds[2] - bounds[0];
    let bh = bounds[3] - bounds[1];
    let size = if bw > bh { bw } else { bh };
    let mut ring = size / 8;
    if ring < 2 {
        ring = 2;
    }
    if ring > 16 {
        ring = 16;
    }
    // Work box: the spot plus its ring, clipped to the image.
    let wx0 = if bounds[0] - ring < 0 { 0 } else { bounds[0] - ring };
    let wy0 = if bounds[1] - ring < 0 { 0 } else { bounds[1] - ring };
    let wx1 = if bounds[2] + ring > w { w } else { bounds[2] + ring };
    let wy1 = if bounds[3] + ring > h { h } else { bounds[3] + ring };
    let ww = wx1 - wx0;
    let wh = wy1 - wy0;
    let wn = ww * wh;

    let mut role = vec![0u8; wn as usize];
    let mut near = vec![0u8; wn as usize];
    let mut prefix = vec![0i64; (if ww > wh { ww } else { wh }) as usize + 1];
    let mut value = vec![0.0f32; (wn * 4) as usize];
    for y in 0..wh {
        for x in 0..ww {
            role[(y * ww + x) as usize] =
                if coverage[(wy0 + y) as usize * width + (wx0 + x) as usize] != 0 {
                    HOLE
                } else {
                    OUTSIDE
                };
        }
    }
    // The ring: pixels within `ring` of the spot (a square dilation, row pass then column pass).
    for y in 0..wh {
        prefix[0] = 0;
        for x in 0..ww {
            prefix[(x + 1) as usize] =
                prefix[x as usize] + (role[(y * ww + x) as usize] == HOLE) as i64;
        }
        for x in 0..ww {
            let lo = if x - ring < 0 { 0 } else { x - ring };
            let hi = if x + ring + 1 > ww { ww } else { x + ring + 1 };
            near[(y * ww + x) as usize] = (prefix[hi as usize] - prefix[lo as usize] > 0) as u8;
        }
    }
    for x in 0..ww {
        prefix[0] = 0;
        for y in 0..wh {
            prefix[(y + 1) as usize] = prefix[y as usize] + near[(y * ww + x) as usize] as i64;
        }
        for y in 0..wh {
            let lo = if y - ring < 0 { 0 } else { y - ring };
            let hi = if y + ring + 1 > wh { wh } else { y + ring + 1 };
            if role[(y * ww + x) as usize] == OUTSIDE && prefix[hi as usize] - prefix[lo as usize] > 0
            {
                role[(y * ww + x) as usize] = RING;
            }
        }
    }
    let mut ring_count = 0i64;
    for p in 0..wn as usize {
        if role[p] == RING {
            ring_count += 1;
        }
    }
    if ring_count == 0 {
        return 0;
    }

    // Source patch for Content-Aware and Proximity Match.
    let mut ox = 0i64;
    let mut oy = 0i64;
    let mut have_source = false;
    if mode != 1 {
        const FACTORS: [f64; 5] = [1.05, 1.35, 1.75, 2.25, 2.8];
        let count = if mode == 2 { 2 } else { 5 };
        let mut best = f64::INFINITY;
        for f in 0..count {
            for a in 0..24i64 {
                let angle = a as f64 * std::f64::consts::PI / 12.0;
                let dx = (angle.cos() * FACTORS[f as usize] * ww as f64).round() as i64;
                let dy = (angle.sin() * FACTORS[f as usize] * wh as f64).round() as i64;
                let mut score = heal_score(rgba, stride, &role, wx0, wy0, ww, wh, dx, dy, w, h);
                if !score.is_finite() {
                    continue;
                }
                // nearer patches win ties
                score *= if mode == 2 { 1.0 + 0.6 * f as f64 } else { 1.0 + 0.1 * f as f64 };
                if score < best {
                    best = score;
                    ox = dx;
                    oy = dy;
                }
            }
        }
        if best.is_finite() {
            // Fine-tune the alignment so repeating texture lines up.
            let cx = ox;
            let cy = oy;
            let mut refined = heal_score(rgba, stride, &role, wx0, wy0, ww, wh, cx, cy, w, h);
            for j in -3..=3i64 {
                for i in -3..=3i64 {
                    let score = heal_score(rgba, stride, &role, wx0, wy0, ww, wh, cx + i, cy + j, w, h);
                    if score < refined {
                        refined = score;
                        ox = cx + i;
                        oy = cy + j;
                    }
                }
            }
            have_source = true;
        }
    }

    // Membrane: the edge difference between the original and the patch (or the original itself
    // for a smooth fill), spread across the spot.
    let mut mean = [0.0f64; 4];
    let mut detail = [0.0f64; 3];
    for y in 0..wh {
        for x in 0..ww {
            let p = (y * ww + x) as usize;
            if role[p] != RING {
                for c in 0..4 {
                    value[p * 4 + c] = 0.0;
                }
                continue;
            }
            let ix = wx0 + x;
            let iy = wy0 + y;
            let t = iy as usize * stride + ix as usize * 4;
            let s = if have_source {
                Some((iy + oy) as usize * stride + (ix + ox) as usize * 4)
            } else {
                None
            };
            for c in 0..4 {
                let source_value = match s {
                    Some(s) => rgba[s + c] as f32,
                    None => 0.0,
                };
                value[p * 4 + c] = rgba[t + c] as f32 - source_value;
                mean[c] += value[p * 4 + c] as f64;
            }
            if !have_source {
                // Fine detail around the spot: each pixel against the average of its neighbours.
                for c in 0..3 {
                    let mut around = 0.0f64;
                    let mut n = 0i64;
                    let offsets = [(ix - 1, iy), (ix + 1, iy), (ix, iy - 1), (ix, iy + 1)];
                    for (nx, ny) in offsets {
                        if nx < 0 || ny < 0 || nx >= w || ny >= h {
                            continue;
                        }
                        around += rgba[ny as usize * stride + nx as usize * 4 + c] as f64;
                        n += 1;
                    }
                    if n != 0 {
                        let d = rgba[t + c] as f64 - around / n as f64;
                        detail[c] += d * d;
                    }
                }
            }
        }
    }
    for c in 0..4 {
        mean[c] /= ring_count as f64;
    }
    for p in 0..wn as usize {
        if role[p] == HOLE {
            for c in 0..4 {
                value[p * 4 + c] = mean[c] as f32;
            }
        }
    }
    heal_solve(&mut value, &role, ww, wh, 0);
    for c in 0..3 {
        detail[c] = (detail[c] / ring_count as f64).sqrt() * 0.9;
    }

    for y in 0..wh {
        for x in 0..ww {
            let p = (y * ww + x) as usize;
            if role[p] != HOLE {
                continue;
            }
            let ix = wx0 + x;
            let iy = wy0 + y;
            let t = iy as usize * stride + ix as usize * 4;
            let s = if have_source {
                Some((iy + oy) as usize * stride + (ix + ox) as usize * 4)
            } else {
                None
            };
            let amount = coverage[iy as usize * width + ix as usize] as f64 / 255.0 * opacity as f64;
            let mut grain = 0.0f64;
            if !have_source {
                let key = heal_hash(seed ^ heal_hash((iy * w + ix) as u32));
                let u1 = heal_unit(key);
                let u2 = heal_unit(key ^ 0x68e31da4);
                grain = (-2.0 * (1.0 - u1).ln()).sqrt()
                    * (2.0 * std::f64::consts::PI * u2).cos();
            }
            let mut out = [0.0f64; 4];
            for c in 0..4 {
                let source_value = match s {
                    Some(s) => rgba[s + c] as f64,
                    None => 0.0,
                };
                let healed =
                    source_value + value[p * 4 + c] as f64 + if c < 3 { grain * detail[c] } else { 0.0 };
                out[c] = rgba[t + c] as f64 + (healed - rgba[t + c] as f64) * amount;
            }
            let alpha = if out[3] < 0.0 {
                0.0
            } else if out[3] > 255.0 {
                255.0
            } else {
                out[3]
            };
            rgba[t + 3] = alpha.round() as i64 as u8;
            let alpha = rgba[t + 3] as f64;
            for c in 0..3 {
                let v = if out[c] < 0.0 {
                    0.0
                } else if out[c] > alpha {
                    alpha
                } else {
                    out[c]
                };
                rgba[t + c] = v.round() as i64 as u8;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coverage_rect(
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
    ) -> Vec<u8> {
        let mut mask = vec![0u8; width * height];
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                mask[y * width + x] = 255;
            }
        }
        mask
    }

    fn textured(width: usize, height: usize) -> Vec<u8> {
        let stride = width * 4;
        let mut rgba = vec![0u8; stride * height];
        let mut state: u32 = 0x1234_5678;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let v = (state >> 24) as u8;
                let i = y * stride + x * 4;
                rgba[i] = v;
                rgba[i + 1] = v / 2;
                rgba[i + 2] = 255 - v;
                rgba[i + 3] = 255;
            }
        }
        rgba
    }

    #[test]
    fn heal_coverage_bounds_empty_is_all_zero() {
        let gray = [0u8; 16];
        let mut bounds = [9i64; 4];
        heal_coverage_bounds(&gray, 4, 4, 4, &mut bounds);
        assert_eq!(bounds, [0, 0, 0, 0]);
    }

    #[test]
    fn heal_coverage_bounds_single_pixel_is_half_open() {
        let mut gray = [0u8; 12];
        gray[1 * 4 + 2] = 7;
        let mut bounds = [0i64; 4];
        heal_coverage_bounds(&gray, 4, 3, 4, &mut bounds);
        assert_eq!(bounds, [2, 1, 3, 2]);
    }

    #[test]
    fn heal_coverage_bounds_respects_stride() {
        let mut gray = [0u8; 12];
        gray[6 + 1] = 255;
        let mut bounds = [0i64; 4];
        heal_coverage_bounds(&gray, 2, 2, 6, &mut bounds);
        assert_eq!(bounds, [1, 1, 2, 2]);
    }

    #[test]
    fn spot_heal_empty_coverage_returns_zero_and_leaves_pixels() {
        let (width, height) = (8usize, 8usize);
        let stride = width * 4;
        let original = textured(width, height);
        let mut rgba = original.clone();
        let coverage = vec![0u8; width * height];
        assert_eq!(spot_heal(&mut rgba, &coverage, width, height, stride, 1.0, 0, 1), 0);
        assert_eq!(rgba, original);
    }

    #[test]
    fn spot_heal_uniform_image_is_unchanged_in_every_mode() {
        let (width, height) = (24usize, 24usize);
        let stride = width * 4;
        let coverage = coverage_rect(width, height, 9, 9, 6, 6);
        for mode in 0..3 {
            let mut rgba = vec![0u8; stride * height];
            for p in rgba.chunks_exact_mut(4) {
                p.copy_from_slice(&[60, 120, 180, 255]);
            }
            let original = rgba.clone();
            assert_eq!(
                spot_heal(&mut rgba, &coverage, width, height, stride, 1.0, mode, 7),
                0
            );
            assert_eq!(rgba, original, "mode {mode}");
        }
    }

    #[test]
    fn spot_heal_preserves_alpha_of_a_uniform_semitransparent_image() {
        let (width, height) = (16usize, 16usize);
        let stride = width * 4;
        let coverage = coverage_rect(width, height, 6, 6, 4, 4);
        let mut rgba = vec![0u8; stride * height];
        for p in rgba.chunks_exact_mut(4) {
            p.copy_from_slice(&[100, 100, 100, 128]);
        }
        let original = rgba.clone();
        assert_eq!(
            spot_heal(&mut rgba, &coverage, width, height, stride, 0.5, 1, 3),
            0
        );
        assert_eq!(rgba, original);
        assert_eq!(rgba[6 * stride + 6 * 4 + 3], 128);
    }

    #[test]
    fn spot_heal_leaves_uncovered_pixels_untouched() {
        let (width, height) = (20usize, 20usize);
        let stride = width * 4;
        let original = textured(width, height);
        let mut rgba = original.clone();
        let coverage = coverage_rect(width, height, 7, 7, 5, 5);
        assert_eq!(
            spot_heal(&mut rgba, &coverage, width, height, stride, 1.0, 1, 42),
            0
        );
        for y in 0..height {
            for x in 0..width {
                if (7..12).contains(&x) && (7..12).contains(&y) {
                    continue;
                }
                let i = y * stride + x * 4;
                assert_eq!(&rgba[i..i + 4], &original[i..i + 4], "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn spot_heal_is_deterministic_for_a_seed() {
        let (width, height) = (20usize, 20usize);
        let stride = width * 4;
        let coverage = coverage_rect(width, height, 7, 7, 5, 5);
        let mut first = textured(width, height);
        let mut second = first.clone();
        assert_eq!(
            spot_heal(&mut first, &coverage, width, height, stride, 1.0, 1, 42),
            0
        );
        assert_eq!(
            spot_heal(&mut second, &coverage, width, height, stride, 1.0, 1, 42),
            0
        );
        assert_eq!(first, second);
    }
}
