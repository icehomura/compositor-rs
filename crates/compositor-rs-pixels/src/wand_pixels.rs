//! Port of `references/Compositor/Compositor/Rendering/WandPixels.{c,h}`.
//!
//! `wand_mask` is the Magic Wand, `color_range_mask` is Select > Color Range, and `wand_trace`
//! turns a selection mask into the pixel-edge outlines a marching-ants path is drawn from.
//! Signatures keep the C shape: `stride` is still bytes per row and `width`/`height` stay
//! separate parameters, so the arithmetic can be diffed against the original line for line.

/// Pixel-edge directions, clockwise on screen (y grows downward): east, south, west, north.
const EAST: u8 = 1;
const SOUTH: u8 = 2;
const WEST: u8 = 4;
const NORTH: u8 = 8;

/// Outlines with more pixel edges than this are refused: the path would be too slow to draw.
const WAND_EDGE_LIMIT: usize = 8_000_000;

/// The C's `-1` and `-2` results of `wand_trace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WandTraceError {
    /// The C returned `-1`: a buffer could not be allocated. This also covers the C's
    /// `width >= INT32_MAX || height >= INT32_MAX` guard. Rust aborts instead of returning null
    /// on allocation failure, so the variant exists for API parity with the original.
    OutOfMemory,
    /// The C returned `-2`: the outline has more than [`WAND_EDGE_LIMIT`] (8_000_000) pixel
    /// edges, which is too detailed to be worth drawing.
    TooDetailed,
}

impl std::fmt::Display for WandTraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WandTraceError::OutOfMemory => write!(f, "wand trace ran out of memory"),
            WandTraceError::TooDetailed => write!(f, "wand trace outline is too detailed"),
        }
    }
}

impl std::error::Error for WandTraceError {}

/// Magic Wand match over premultiplied RGBA (4 bytes per pixel, `stride` bytes per row, rows
/// top-down). The reference color is the average over a (2 * radius + 1)² square around the
/// seed, clipped to the image. A pixel matches when every channel, alpha included, is within
/// `tolerance` of it. Contiguous fills 4-connected from the seed (nothing when the seed itself
/// doesn't match); otherwise every matching pixel. Writes 255 for selected, 0 elsewhere, into
/// `mask` (width * height bytes). Returns the number selected, or -1 when memory runs out.
///
/// The C's `-1` (allocation failure) is unreachable here: Rust aborts rather than returning null,
/// so the return value is always the selected count.
pub fn wand_mask(
    rgba: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    seed_x: usize,
    seed_y: usize,
    radius: usize,
    tolerance: i32,
    contiguous: i32,
    mask: &mut [u8],
) -> i64 {
    if width == 0 || height == 0 {
        return 0;
    }
    mask[..width * height].fill(0);
    if seed_x >= width || seed_y >= height {
        return 0;
    }
    let x0 = seed_x.saturating_sub(radius);
    let x1 = seed_x.saturating_add(radius).min(width - 1);
    let y0 = seed_y.saturating_sub(radius);
    let y1 = seed_y.saturating_add(radius).min(height - 1);
    let mut sums = [0u64; 4];
    let mut samples = 0u64;
    for y in y0..=y1 {
        for x in x0..=x1 {
            samples += 1;
            let p = y * stride + x * 4;
            for c in 0..4 {
                sums[c] += rgba[p + c] as u64;
            }
        }
    }
    let mut reference = [0i32; 4];
    for c in 0..4 {
        reference[c] = ((sums[c] + samples / 2) / samples) as i32;
    }

    let mut count = 0i64;
    if contiguous == 0 {
        for y in 0..height {
            let out = y * width;
            for x in 0..width {
                if wand_matches(&rgba[y * stride + x * 4..][..4], &reference, tolerance) {
                    mask[out + x] = 255;
                    count += 1;
                }
            }
        }
        return count;
    }

    // Scanline flood fill: each popped seed fills its whole horizontal run, then pushes one
    // seed per matching run in the rows directly above and below it.
    let mut stack: Vec<(usize, usize)> = Vec::with_capacity(4096);
    stack.push((seed_x, seed_y));
    while let Some((x, y)) = stack.pop() {
        let row = y * stride;
        let out = y * width;
        if mask[out + x] != 0 || !wand_matches(&rgba[row + x * 4..][..4], &reference, tolerance) {
            continue;
        }
        let mut left = x;
        while left > 0
            && mask[out + left - 1] == 0
            && wand_matches(&rgba[row + (left - 1) * 4..][..4], &reference, tolerance)
        {
            left -= 1;
        }
        let mut right = x;
        while right + 1 < width
            && mask[out + right + 1] == 0
            && wand_matches(&rgba[row + (right + 1) * 4..][..4], &reference, tolerance)
        {
            right += 1;
        }
        mask[out + left..=out + right].fill(255);
        count += (right - left + 1) as i64;
        for side in 0..2 {
            if (side == 0 && y == 0) || (side == 1 && y + 1 >= height) {
                continue;
            }
            let ny = if side == 0 { y - 1 } else { y + 1 };
            let nrow = ny * stride;
            let nout = ny * width;
            let mut in_run = false;
            for nx in left..=right {
                let candidate = mask[nout + nx] == 0
                    && wand_matches(&rgba[nrow + nx * 4..][..4], &reference, tolerance);
                if candidate && !in_run {
                    stack.push((nx, ny));
                }
                in_run = candidate;
            }
        }
    }
    count
}

/// Whether every channel of the RGBA pixel `p`, alpha included, is within `tolerance` of the
/// reference color.
fn wand_matches(p: &[u8], reference: &[i32; 4], tolerance: i32) -> bool {
    for c in 0..4 {
        let d = p[c] as i32 - reference[c];
        if d < tolerance.wrapping_neg() || d > tolerance {
            return false;
        }
    }
    true
}

/// Headings, clockwise on screen (y grows downward): east, south, west, north.
fn turn_right(d: u8) -> u8 {
    if d == NORTH {
        EAST
    } else {
        d << 1
    }
}

/// Headings, counterclockwise on screen (y grows downward): east, south, west, north.
fn turn_left(d: u8) -> u8 {
    if d == EAST {
        NORTH
    } else {
        d >> 1
    }
}

/// Outline of the nonzero pixels of `mask`, along pixel edges, as closed loops of corner points
/// (x, y pairs in pixel-edge coordinates). Outer boundaries run clockwise and holes
/// counterclockwise in top-left coordinates, so the winding rule fills exactly those pixels.
///
/// `mask` is `width * height` bytes (no stride). The C returned `points` (2 * pointCount values)
/// and `loops` (each loop's corner count) through malloc'd out pointers and a status code; here
/// the status becomes the `Result`:
///
/// * `Ok(Some((points, loops)))` is the C's `0`. `points` holds `2 * pointCount` values as
///   `[x0, y0, x1, y1, …]` and `loops[i]` is how many corners loop `i` has.
/// * `Ok(None)` is the C's early `0` for a zero-sized image, where it left both out pointers
///   null with zero counts.
/// * `Err(WandTraceError::TooDetailed)` is the C's `-2`.
/// * `Err(WandTraceError::OutOfMemory)` is the C's `-1`.
pub fn wand_trace(
    mask: &[u8],
    width: usize,
    height: usize,
) -> Result<Option<(Vec<i32>, Vec<usize>)>, WandTraceError> {
    if width == 0 || height == 0 {
        return Ok(None);
    }
    if width >= i32::MAX as usize || height >= i32::MAX as usize {
        return Err(WandTraceError::OutOfMemory);
    }
    // Each vertex of the (width + 1) × (height + 1) grid records the directed boundary edges
    // leaving it: a selected pixel's unselected sides, walked clockwise around the pixel.
    let stride = width + 1;
    let vertices = stride * (height + 1);
    let mut edges = 0usize;
    let mut out = vec![0u8; vertices];
    for y in 0..height {
        let row = &mask[y * width..];
        for x in 0..width {
            if row[x] == 0 {
                continue;
            }
            if y == 0 || mask[(y - 1) * width + x] == 0 {
                out[y * stride + x] |= EAST;
                edges += 1;
            }
            if x + 1 == width || row[x + 1] == 0 {
                out[y * stride + x + 1] |= SOUTH;
                edges += 1;
            }
            if y + 1 == height || mask[(y + 1) * width + x] == 0 {
                out[(y + 1) * stride + x + 1] |= WEST;
                edges += 1;
            }
            if x == 0 || row[x - 1] == 0 {
                out[(y + 1) * stride + x] |= NORTH;
                edges += 1;
            }
        }
        if edges > WAND_EDGE_LIMIT {
            return Err(WandTraceError::TooDetailed);
        }
    }

    let mut points: Vec<i32> = Vec::new();
    let mut loops: Vec<usize> = Vec::new();
    for start in 0..vertices {
        while out[start] != 0 {
            let first = points.len() / 2;
            let mut v = start;
            let mut heading = 0u8;
            let mut initial = 0u8;
            loop {
                let bits = out[v];
                // Where two loops meet at a corner, turning right keeps them apart.
                let d = if heading == 0 {
                    bits & bits.wrapping_neg()
                } else if bits & turn_right(heading) != 0 {
                    turn_right(heading)
                } else if bits & heading != 0 {
                    heading
                } else if bits & turn_left(heading) != 0 {
                    turn_left(heading)
                } else {
                    bits & bits.wrapping_neg()
                };
                if d == 0 {
                    break;
                }
                out[v] &= !d;
                if d != heading {
                    points.push((v % stride) as i32);
                    points.push((v / stride) as i32);
                }
                if heading == 0 {
                    initial = d;
                }
                heading = d;
                v = if d == EAST {
                    v + 1
                } else if d == WEST {
                    v - 1
                } else if d == SOUTH {
                    v + stride
                } else {
                    v - stride
                };
                if v == start {
                    break;
                }
            }
            // The start is a corner unless the loop arrives on the heading it left with.
            if heading == initial && points.len() / 2 > first {
                points.drain(first * 2..first * 2 + 2);
            }
            loops.push(points.len() / 2 - first);
        }
    }
    Ok(Some((points, loops)))
}

/// Whether the straight sRGB color `rgb` is within `fuzziness` of any of the `count` colors in
/// `colors` (3 bytes each).
fn color_near(rgb: &[i32; 3], colors: &[u8], count: i32, fuzziness: i32) -> bool {
    for i in 0..count {
        let c = &colors[i as usize * 3..i as usize * 3 + 3];
        if (rgb[0] - c[0] as i32).abs() <= fuzziness
            && (rgb[1] - c[1] as i32).abs() <= fuzziness
            && (rgb[2] - c[2] as i32).abs() <= fuzziness
        {
            return true;
        }
    }
    false
}

/// Select > Color Range over premultiplied RGBA (as above). A pixel matches when every color
/// channel is within `fuzziness` of one of the `include_count` colors in `include` and of none
/// in `exclude` (straight sRGB, 3 bytes each). Transparent pixels never match. With `invert`, the
/// pixels that don't match are selected instead (transparent pixels included). Writes 255 for
/// selected, 0 elsewhere, into `mask` (width * height bytes), and returns the number selected.
pub fn color_range_mask(
    rgba: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    include: &[u8],
    include_count: i32,
    exclude: &[u8],
    exclude_count: i32,
    fuzziness: i32,
    invert: i32,
    mask: &mut [u8],
) -> i64 {
    let mut count = 0i64;
    for y in 0..height {
        let row = y * stride;
        let out = y * width;
        for x in 0..width {
            let px = &rgba[row + x * 4..][..4];
            let mut matches = 0i32;
            if px[3] != 0 {
                let mut rgb = [0i32; 3];
                for c in 0..3 {
                    rgb[c] = (px[c] as i32 * 255 + px[3] as i32 / 2) / px[3] as i32;
                }
                matches = (color_near(&rgb, include, include_count, fuzziness)
                    && !color_near(&rgb, exclude, exclude_count, fuzziness))
                    as i32;
            }
            if invert != 0 {
                matches = (matches == 0) as i32;
            }
            mask[out + x] = if matches != 0 { 255 } else { 0 };
            count += matches as i64;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
        assert_eq!(pixels.len(), width * height);
        pixels.iter().flat_map(|p| p.iter().copied()).collect()
    }

    fn gray(value: u8, alpha: u8) -> [u8; 4] {
        [value, value, value, alpha]
    }

    #[test]
    fn wand_mask_selects_a_uniform_image() {
        let (w, h) = (3usize, 2usize);
        let rgba = image(w, h, &[gray(100, 255); 6]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 1, 1, 0, 0, 1, &mut mask), 6);
        assert!(mask.iter().all(|&v| v == 255));
    }

    #[test]
    fn wand_mask_reference_is_the_clipped_radius_average() {
        let (w, h) = (3usize, 1usize);
        let rgba = image(w, h, &[gray(100, 255), gray(100, 255), gray(200, 255)]);
        let mut mask = vec![0u8; w * h];
        // Radius 1 around x = 0 clips to x ∈ [0, 1], so the reference is (100 + 100) / 2 = 100.
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 1, 0, 1, &mut mask), 2);
        assert_eq!(mask, [255, 255, 0]);
    }

    #[test]
    fn wand_mask_matches_the_alpha_channel_too() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[gray(100, 255), gray(100, 128)]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 0, 1, &mut mask), 1);
        assert_eq!(mask, [255, 0]);
    }

    #[test]
    fn wand_mask_tolerance_is_inclusive() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[gray(100, 255), gray(105, 255)]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 5, 1, &mut mask), 2);
        assert_eq!(mask, [255, 255]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 4, 1, &mut mask), 1);
        assert_eq!(mask, [255, 0]);
    }

    #[test]
    fn wand_mask_contiguous_stops_at_non_matching_pixels() {
        let (w, h) = (3usize, 1usize);
        let rgba = image(w, h, &[gray(100, 255), gray(200, 255), gray(100, 255)]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 0, 1, &mut mask), 1);
        assert_eq!(mask, [255, 0, 0]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 0, 0, &mut mask), 2);
        assert_eq!(mask, [255, 0, 255]);
    }

    #[test]
    fn wand_mask_is_four_connected() {
        let (w, h) = (2usize, 2usize);
        let rgba = image(
            w,
            h,
            &[gray(100, 255), gray(0, 255), gray(0, 255), gray(100, 255)],
        );
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 0, 1, &mut mask), 1);
        assert_eq!(mask, [255, 0, 0, 0]);
    }

    #[test]
    fn wand_mask_fills_around_a_blocked_center() {
        let (w, h) = (3usize, 3usize);
        let mut pixels = [gray(100, 255); 9];
        pixels[4] = gray(200, 255);
        let rgba = image(w, h, &pixels);
        let mut mask = vec![0u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, 0, 1, &mut mask), 8);
        assert_eq!(mask, [255, 255, 255, 255, 0, 255, 255, 255, 255]);
    }

    #[test]
    fn wand_mask_is_deterministic() {
        let (w, h) = (4usize, 4usize);
        let mut pixels = [gray(100, 255); 16];
        pixels[5] = gray(101, 255);
        pixels[6] = gray(200, 255);
        let rgba = image(w, h, &pixels);
        let mut a = vec![0u8; w * h];
        let mut b = vec![0u8; w * h];
        let ca = wand_mask(&rgba, w, h, w * 4, 0, 0, 1, 1, 1, &mut a);
        let cb = wand_mask(&rgba, w, h, w * 4, 0, 0, 1, 1, 1, &mut b);
        assert_eq!(ca, cb);
        assert_eq!(a, b);
    }

    #[test]
    fn wand_mask_ignores_out_of_range_seeds_and_empty_images() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[gray(100, 255), gray(100, 255)]);
        // The C clears the mask before validating the seed.
        let mut mask = vec![7u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 2, 0, 0, 0, 0, &mut mask), 0);
        assert_eq!(mask, [0, 0]);
        let mut mask = vec![7u8; w * h];
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 1, 0, 0, 1, &mut mask), 0);
        assert_eq!(mask, [0, 0]);
        let mut empty: Vec<u8> = Vec::new();
        assert_eq!(wand_mask(&rgba, 0, 1, 0, 0, 0, 0, 0, 0, &mut empty), 0);
    }

    #[test]
    fn wand_mask_reads_rows_at_stride() {
        // One pixel of padding per row: stride is 12, not 8.
        let rgba: Vec<u8> = vec![
            100, 100, 100, 255, 100, 100, 100, 255, 9, 9, 9, 9, //
            100, 100, 100, 255, 200, 200, 200, 255, 9, 9, 9, 9,
        ];
        let mut mask = vec![0u8; 2 * 2];
        assert_eq!(wand_mask(&rgba, 2, 2, 12, 0, 0, 0, 0, 1, &mut mask), 3);
        assert_eq!(mask, [255, 255, 255, 0]);
    }

    #[test]
    fn color_range_mask_matches_within_fuzziness() {
        let (w, h) = (2usize, 2usize);
        let rgba = image(
            w,
            h,
            &[
                [10, 20, 30, 255],
                [200, 200, 200, 255],
                [12, 22, 32, 255],
                [0, 0, 0, 255],
            ],
        );
        let mut mask = vec![0u8; w * h];
        let include = [10u8, 20, 30];
        let count = color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 2, 0, &mut mask);
        assert_eq!(count, 2);
        assert_eq!(mask, [255, 0, 255, 0]);
    }

    #[test]
    fn color_range_mask_exclude_beats_include() {
        let (w, h) = (3usize, 1usize);
        let rgba = image(
            w,
            h,
            &[[10, 20, 30, 255], [14, 20, 30, 255], [200, 200, 200, 255]],
        );
        let mut mask = vec![0u8; w * h];
        // Both include and exclude use the same fuzziness, so 14 is inside both boxes and the
        // exclusion wins, while 10 is only inside the include box.
        let include = [10u8, 20, 30];
        let exclude = [18u8, 20, 30];
        let count = color_range_mask(
            &rgba,
            w,
            h,
            w * 4,
            &include,
            1,
            &exclude,
            1,
            5,
            0,
            &mut mask,
        );
        assert_eq!(count, 1);
        assert_eq!(mask, [255, 0, 0]);
    }

    #[test]
    fn color_range_mask_unpremultiplies_before_matching() {
        // Premultiplied (64, 64, 64, 128) is straight 128, so it matches a gray of 128 exactly.
        let (w, h) = (1usize, 1usize);
        let rgba = image(w, h, &[[64, 64, 64, 128]]);
        let mut mask = vec![0u8; w * h];
        let include = [128u8, 128, 128];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 0, 0, &mut mask),
            1
        );
        assert_eq!(mask, [255]);
        let mut mask = vec![0u8; w * h];
        let include = [127u8, 127, 127];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 0, 0, &mut mask),
            0
        );
        assert_eq!(mask, [0]);
    }

    #[test]
    fn color_range_mask_never_matches_transparent_pixels() {
        let (w, h) = (1usize, 1usize);
        let rgba = image(w, h, &[[0, 0, 0, 0]]);
        let include = [0u8, 0, 0];
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 0, 0, &mut mask),
            0
        );
        assert_eq!(mask, [0]);
        // ... unless inverted, which flips after the transparency test.
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 0, 1, &mut mask),
            1
        );
        assert_eq!(mask, [255]);
    }

    #[test]
    fn color_range_mask_invert_selects_the_rest() {
        let (w, h) = (3usize, 1usize);
        let rgba = image(
            w,
            h,
            &[[10, 20, 30, 255], [200, 200, 200, 255], [10, 20, 30, 255]],
        );
        let include = [10u8, 20, 30];
        let mut mask = vec![0u8; w * h];
        let count = color_range_mask(&rgba, w, h, w * 4, &include, 1, &[], 0, 0, 1, &mut mask);
        assert_eq!(count, 1);
        assert_eq!(mask, [0, 255, 0]);
    }

    #[test]
    fn color_range_mask_ignores_negative_counts() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[[10, 20, 30, 255], [40, 50, 60, 255]]);
        let mut mask = vec![0u8; w * h];
        // The C's `for (int i = 0; i < count; ++i)` runs zero times for a negative count.
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &[], -1, &[], -1, 0, 0, &mut mask),
            0
        );
        assert_eq!(mask, [0, 0]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &[], -1, &[], -1, 0, 1, &mut mask),
            2
        );
        assert_eq!(mask, [255, 255]);
    }

    #[test]
    fn wand_mask_negative_tolerance_selects_nothing() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[gray(0, 0), gray(255, 255)]);
        let mut mask = vec![0u8; w * h];
        // With a negative tolerance the C's `d < -tolerance || d > tolerance` rejects every
        // channel delta (it would require d >= -tolerance and d <= tolerance at once), so not
        // even the seed matches and the flood fill selects nothing.
        assert_eq!(wand_mask(&rgba, w, h, w * 4, 0, 0, 0, -1, 1, &mut mask), 0);
        assert_eq!(mask, [0, 0]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            wand_mask(&rgba, w, h, w * 4, 0, 0, 0, -255, 0, &mut mask),
            0
        );
        assert_eq!(mask, [0, 0]);
    }

    #[test]
    fn color_range_mask_with_no_include_colors() {
        let (w, h) = (2usize, 1usize);
        let rgba = image(w, h, &[[10, 20, 30, 255], [40, 50, 60, 255]]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &[], 0, &[], 0, 0, 0, &mut mask),
            0
        );
        assert_eq!(mask, [0, 0]);
        let mut mask = vec![0u8; w * h];
        assert_eq!(
            color_range_mask(&rgba, w, h, w * 4, &[], 0, &[], 0, 0, 1, &mut mask),
            2
        );
        assert_eq!(mask, [255, 255]);
    }

    #[test]
    fn wand_trace_traces_a_single_pixel_clockwise() {
        let mask = [0, 0, 0, 0, 1, 0, 0, 0, 0];
        let (points, loops) = wand_trace(&mask, 3, 3).unwrap().unwrap();
        assert_eq!(points, [1, 1, 2, 1, 2, 2, 1, 2]);
        assert_eq!(loops, [4]);
    }

    #[test]
    fn wand_trace_runs_holes_counterclockwise() {
        let mask = [1, 1, 1, 1, 0, 1, 1, 1, 1];
        let (points, loops) = wand_trace(&mask, 3, 3).unwrap().unwrap();
        assert_eq!(points, [0, 0, 3, 0, 3, 3, 0, 3, 1, 1, 1, 2, 2, 2, 2, 1]);
        assert_eq!(loops, [4, 4]);
    }

    #[test]
    fn wand_trace_collapses_straight_edges() {
        let mask = [1, 1, 1, 1, 1];
        let (points, loops) = wand_trace(&mask, 5, 1).unwrap().unwrap();
        assert_eq!(points, [0, 0, 5, 0, 5, 1, 0, 1]);
        assert_eq!(loops, [4]);
    }

    #[test]
    fn wand_trace_handles_disjoint_regions() {
        let mask = [1, 0, 0, 0, 1];
        let (points, loops) = wand_trace(&mask, 5, 1).unwrap().unwrap();
        assert_eq!(points, [0, 0, 1, 0, 1, 1, 0, 1, 4, 0, 5, 0, 5, 1, 4, 1]);
        assert_eq!(loops, [4, 4]);
    }

    #[test]
    fn wand_trace_handles_empty_inputs() {
        assert_eq!(wand_trace(&[], 0, 0), Ok(None));
        assert_eq!(wand_trace(&[], 0, 4), Ok(None));
        let mask = [0u8; 4];
        assert_eq!(wand_trace(&mask, 2, 2), Ok(Some((Vec::new(), Vec::new()))));
    }

    #[test]
    fn wand_trace_is_deterministic() {
        let mask = [1, 1, 0, 1, 0, 1, 1, 1, 1];
        let first = wand_trace(&mask, 3, 3).unwrap();
        let second = wand_trace(&mask, 3, 3).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn wand_trace_refuses_outlines_over_the_edge_limit() {
        // A checkerboard gives every selected pixel four boundary edges; 2000 × 2001 pixels
        // yield 8_004_000 edges, past the 8_000_000 limit.
        let (w, h) = (2000usize, 2001usize);
        let mut mask = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                if (x + y) % 2 == 0 {
                    mask[y * w + x] = 1;
                }
            }
        }
        assert_eq!(wand_trace(&mask, w, h), Err(WandTraceError::TooDetailed));
    }
}
