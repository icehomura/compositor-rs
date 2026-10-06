//! Radial lens distortion, ported from `references/Compositor/Compositor/Rendering/LensPixels.c`.

/// Radial lens distortion over premultiplied RGBA (4 bytes per pixel, `stride` bytes per row, same
/// layout for source and destination). Each destination pixel samples the source bilinearly at
/// its offset from the image center scaled by (1 - k * r²), where r is that offset relative to the
/// half-diagonal: k > 0 pulls samples inward (straightens barrel distortion, corners crop), k < 0
/// pushes them outward (straightens pincushion distortion, corners turn transparent). Pixels
/// outside the source are transparent. k = 0 copies the source exactly.
pub fn lens_distort(
    source: &[u8],
    destination: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    k: f64,
) {
    let cx = width as f64 * 0.5;
    let cy = height as f64 * 0.5;
    let half_diagonal2 = cx * cx + cy * cy;
    for y in 0..height {
        let dy = y as f64 + 0.5 - cy;
        for x in 0..width {
            let dx = x as f64 + 0.5 - cx;
            let scale = 1.0 - k * (dx * dx + dy * dy) / half_diagonal2;
            // Source position in pixel-center coordinates.
            let sx = cx + dx * scale - 0.5;
            let sy = cy + dy * scale - 0.5;
            let fx0 = sx.floor();
            let fy0 = sy.floor();
            let fx = sx - fx0;
            let fy = sy - fy0;
            let x0 = fx0 as i64;
            let y0 = fy0 as i64;
            let mut sums = [0.0f64; 4];
            for j in 0..2i64 {
                let row = y0 + j;
                if row < 0 || row >= height as i64 {
                    continue;
                }
                let wy = if j != 0 { fy } else { 1.0 - fy };
                if wy == 0.0 {
                    continue;
                }
                let line = row as usize * stride;
                for i in 0..2i64 {
                    let column = x0 + i;
                    if column < 0 || column >= width as i64 {
                        continue;
                    }
                    let weight = wy * (if i != 0 { fx } else { 1.0 - fx });
                    if weight == 0.0 {
                        continue;
                    }
                    let p = line + column as usize * 4;
                    for c in 0..4 {
                        sums[c] += weight * source[p + c] as f64;
                    }
                }
            }
            let out = y * stride + x * 4;
            for c in 0..4 {
                destination[out + c] = sums[c].round() as i64 as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lens_distort_zero_k_copies_exactly() {
        let (width, height) = (4usize, 3usize);
        let stride = width * 4;
        let source: Vec<u8> = (0..stride * height).map(|i| (i * 7 % 251) as u8).collect();
        let mut destination = vec![0u8; source.len()];
        lens_distort(&source, &mut destination, width, height, stride, 0.0);
        assert_eq!(destination, source);
    }

    #[test]
    fn lens_distort_single_pixel_is_identity_for_any_k() {
        let source = [10u8, 20, 30, 40];
        let mut destination = [0u8; 4];
        lens_distort(&source, &mut destination, 1, 1, 4, -1.5);
        assert_eq!(destination, source);
    }

    #[test]
    fn lens_distort_center_pixel_is_identity_for_any_k() {
        let (width, height) = (5usize, 5usize);
        let stride = width * 4;
        let source: Vec<u8> = (0..stride * height).map(|i| (i % 256) as u8).collect();
        let mut destination = vec![0u8; source.len()];
        lens_distort(&source, &mut destination, width, height, stride, 0.35);
        let center = 2 * stride + 2 * 4;
        assert_eq!(&destination[center..center + 4], &source[center..center + 4]);
    }

    #[test]
    fn lens_distort_negative_k_makes_corners_transparent() {
        let (width, height) = (4usize, 4usize);
        let stride = width * 4;
        let source = vec![200u8; stride * height];
        let mut destination = vec![255u8; source.len()];
        lens_distort(&source, &mut destination, width, height, stride, -4.0);
        assert_eq!(&destination[0..4], &[0, 0, 0, 0]);
        assert_eq!(&destination[destination.len() - 4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn lens_distort_positive_k_keeps_center_and_remains_deterministic() {
        let (width, height) = (7usize, 7usize);
        let stride = width * 4;
        let source: Vec<u8> = (0..stride * height).map(|i| (i * 13 % 256) as u8).collect();
        let mut first = vec![0u8; source.len()];
        let mut second = vec![0u8; source.len()];
        lens_distort(&source, &mut first, width, height, stride, 0.5);
        lens_distort(&source, &mut second, width, height, stride, 0.5);
        assert_eq!(first, second);
    }

    /// Ported from
    /// `FilterTests.removeDistortionBendsAboutTheCenterAndOnlyPincushionCorrectionOpensTheCorners`,
    /// whose ±100 comes through Explorer's `distortion / 100 * LENS_STRENGTH`. The kernel's own
    /// k = 0 pixel-exact copy is already `lens_distort_zero_k_copies_exactly`; what was missing is
    /// what the two corrections do at the strengths the panel offers.
    #[test]
    fn remove_distortion_bends_about_the_center_and_only_pincushion_correction_opens_the_corners() {
        use crate::filters::LENS_STRENGTH;

        // An opaque image with a distinct color in each quadrant.
        let (width, height) = (40usize, 30usize);
        let stride = width * 4;
        let colors = [
            [0u8, 128, 255, 255],
            [85, 128, 170, 255],
            [170, 128, 85, 255],
            [255, 128, 0, 255],
        ];
        let mut source = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let quadrant = usize::from(x >= 20) + 2 * usize::from(y >= 15);
                let at = y * stride + x * 4;
                source[at..at + 4].copy_from_slice(&colors[quadrant]);
            }
        }
        let run = |k: f64| {
            let mut result = vec![0u8; source.len()];
            lens_distort(&source, &mut result, width, height, stride, k);
            result
        };
        let alpha = |pixels: &[u8], x: usize, y: usize| pixels[(y * width + x) * 4 + 3];
        // Straightening barrel distortion stretches the edges outward: nothing opens up.
        let barrel = run(100.0 / 100.0 * LENS_STRENGTH);
        for (x, y) in [(0, 0), (39, 0), (0, 29), (39, 29)] {
            assert_eq!(alpha(&barrel, x, y), 255, "corner {x},{y} stays opaque");
        }
        // Straightening pincushion pulls the edges in: the corners turn transparent, the middle
        // stays put.
        let pincushion = run(-100.0 / 100.0 * LENS_STRENGTH);
        for (x, y) in [(0, 0), (39, 0), (0, 29), (39, 29)] {
            assert_eq!(alpha(&pincushion, x, y), 0, "corner {x},{y} opens up");
        }
        let middle = (15 * width + 20) * 4;
        assert_eq!(
            &pincushion[middle..middle + 4],
            &source[middle..middle + 4],
            "the centre pixel is unchanged"
        );
    }
}
