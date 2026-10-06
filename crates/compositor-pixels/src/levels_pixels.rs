//! Port of `references/Compositor/Compositor/Rendering/LevelsPixels.{h,c}`.
//!
//! Levels, histogram and color-cube kernels. Like the C, the level tables hold normalized values
//! (0–1) and the cube holds normalized RGBA entries.

/// Applies the three 256-entry level tables to premultiplied RGBA pixels (4 bytes per pixel).
/// `tables` holds the red, green and blue tables one after another; alpha is kept and fully transparent pixels are
/// left alone.
pub fn levels_apply(pixels: &mut [u8], count: usize, tables: &[f32]) {
    for i in 0..count {
        let p = &mut pixels[i * 4..i * 4 + 4];
        let alpha = p[3] as f32;
        if alpha == 0.0 {
            continue;
        }
        for channel in 0..3 {
            let x = 255.0f32.min(p[channel] as f32 * 255.0 / alpha);
            let lo = x as i32;
            let hi = if lo < 255 { lo + 1 } else { 255 };
            let table = &tables[channel * 256..channel * 256 + 256];
            let result = table[lo as usize] + (table[hi as usize] - table[lo as usize]) * (x - lo as f32);
            p[channel] = alpha.min(0.0f32.max((result * alpha).round())) as u8;
        }
    }
}

/// Tallies a 1024-bin histogram of premultiplied RGBA pixels: bins 0–255 take each pixel's luma weight (a third of
/// its weight per channel), bins 256–511, 512–767 and 768–1023 the red, green and blue channels. `coverage` is an
/// optional per-pixel mask (255 is fully counted); fully transparent pixels are skipped.
pub fn levels_histogram(pixels: &[u8], coverage: Option<&[u8]>, count: usize, bins: &mut [f64]) {
    for i in 0..count {
        let p = &pixels[i * 4..i * 4 + 4];
        if p[3] == 0 {
            continue;
        }
        let weight = p[3] as f64 / 255.0
            * match coverage {
                Some(coverage) => coverage[i] as f64 / 255.0,
                None => 1.0,
            };
        for channel in 0..3 {
            let value = (p[channel] as f64 * 255.0 / p[3] as f64).round().min(255.0) as usize;
            bins[(channel + 1) * 256 + value] += weight;
            bins[value] += weight / 3.0;
        }
    }
}

/// A color lookup through `cube` (`dimension`³ RGBA entries, red varying fastest), blended between the eight nearest
/// entries, on unpremultiplied colors; alpha is kept.
pub fn cube_apply(pixels: &mut [u8], count: usize, cube: &[f32], dimension: i32) {
    debug_assert!(dimension >= 2);
    let scale = (dimension - 1) as f32 / 255.0;
    let dy = dimension as usize;
    let dz = dimension as usize * dimension as usize;
    for i in 0..count {
        let p = &mut pixels[i * 4..i * 4 + 4];
        let alpha = p[3] as f32;
        if alpha == 0.0 {
            continue;
        }
        let mut position = [0.0f32; 3];
        let mut fraction = [0.0f32; 3];
        let mut lo = [0i32; 3];
        for channel in 0..3 {
            position[channel] = 255.0f32.min(p[channel] as f32 * 255.0 / alpha) * scale;
            lo[channel] = position[channel] as i32;
            if lo[channel] > dimension - 2 {
                lo[channel] = dimension - 2;
            }
            fraction[channel] = position[channel] - lo[channel] as f32;
        }
        let base = (lo[0] as usize + lo[1] as usize * dy + lo[2] as usize * dz) * 4;
        let sx = 4usize;
        let sy = dy * 4;
        let sz = dz * 4;
        for channel in 0..3 {
            let c = base + channel;
            let x00 = cube[c] + (cube[c + sx] - cube[c]) * fraction[0];
            let x10 = cube[c + sy] + (cube[c + sy + sx] - cube[c + sy]) * fraction[0];
            let x01 = cube[c + sz] + (cube[c + sz + sx] - cube[c + sz]) * fraction[0];
            let x11 =
                cube[c + sz + sy] + (cube[c + sz + sy + sx] - cube[c + sz + sy]) * fraction[0];
            let y0 = x00 + (x10 - x00) * fraction[1];
            let y1 = x01 + (x11 - x01) * fraction[1];
            let result = y0 + (y1 - y0) * fraction[2];
            p[channel] = alpha.min(0.0f32.max((result * alpha).round())) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level table that maps every value to itself (normalized, as the callers pass them).
    fn identity_tables() -> Vec<f32> {
        (0..768).map(|i| (i % 256) as f32 / 255.0).collect()
    }

    /// The level table that inverts every value: black to white, white to black.
    fn inverted_tables() -> Vec<f32> {
        (0..768).map(|i| (255 - i % 256) as f32 / 255.0).collect()
    }

    #[test]
    fn levels_apply_keeps_opaque_pixels_with_an_identity_table() {
        let mut px = vec![128, 64, 32, 255];
        levels_apply(&mut px, 1, &identity_tables());
        assert_eq!(px, vec![128, 64, 32, 255]);
    }

    #[test]
    fn levels_apply_leaves_transparent_pixels_alone() {
        let mut px = vec![10, 20, 30, 0];
        levels_apply(&mut px, 1, &inverted_tables());
        assert_eq!(px, vec![10, 20, 30, 0]);
    }

    #[test]
    fn levels_apply_inverts_through_the_table() {
        let mut px = vec![255, 255, 255, 255, 0, 0, 0, 255];
        levels_apply(&mut px, 2, &inverted_tables());
        assert_eq!(&px[0..4], &[0, 0, 0, 255]);
        assert_eq!(&px[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn levels_apply_clamps_the_result_to_alpha() {
        // Premultiplied 128 over alpha 64 is out of range once unpremultiplied; the result is clamped back to 64.
        let mut px = vec![128, 128, 128, 64];
        levels_apply(&mut px, 1, &identity_tables());
        assert_eq!(px, vec![64, 64, 64, 64]);
    }

    #[test]
    fn levels_histogram_weights_channels_and_luma() {
        let px = vec![255, 0, 0, 255, 0, 0, 0, 0];
        let mut bins = vec![0.0f64; 4 * 256];
        levels_histogram(&px, None, 2, &mut bins);
        assert_eq!(bins[0], 1.0 / 3.0 + 1.0 / 3.0);
        assert_eq!(bins[255], 1.0 / 3.0);
        assert_eq!(bins[256 + 255], 1.0);
        assert_eq!(bins[512], 1.0);
        assert_eq!(bins[768], 1.0);
        assert_eq!(bins.iter().filter(|v| **v != 0.0).count(), 5);
    }

    #[test]
    fn levels_histogram_scales_by_coverage() {
        let px = vec![255, 0, 0, 255];
        let coverage = vec![51u8];
        let mut bins = vec![0.0f64; 4 * 256];
        levels_histogram(&px, Some(&coverage), 1, &mut bins);
        assert_eq!(bins[256 + 255], 51.0 / 255.0);
        assert_eq!(bins[768], 51.0 / 255.0);
    }

    /// The unit cube: every corner maps to its own color, normalized.
    fn identity_cube() -> Vec<f32> {
        let mut cube = vec![0.0f32; 2 * 2 * 2 * 4];
        for b in 0..2usize {
            for g in 0..2usize {
                for r in 0..2usize {
                    let at = (r + g * 2 + b * 4) * 4;
                    cube[at] = r as f32;
                    cube[at + 1] = g as f32;
                    cube[at + 2] = b as f32;
                    cube[at + 3] = 1.0;
                }
            }
        }
        cube
    }

    #[test]
    fn cube_apply_interpolates_between_the_corners() {
        let mut px = vec![0, 0, 0, 255, 255, 255, 255, 255, 128, 64, 32, 255];
        cube_apply(&mut px, 3, &identity_cube(), 2);
        assert_eq!(
            px,
            vec![0, 0, 0, 255, 255, 255, 255, 255, 128, 64, 32, 255]
        );
    }

    #[test]
    fn cube_apply_leaves_transparent_pixels_alone() {
        let mut px = vec![7, 8, 9, 0];
        cube_apply(&mut px, 1, &identity_cube(), 2);
        assert_eq!(px, vec![7, 8, 9, 0]);
    }
}
