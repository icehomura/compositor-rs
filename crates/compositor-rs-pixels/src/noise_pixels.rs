//! Port of `references/Compositor/Compositor/Rendering/NoisePixels.{h,c}`.
//!
//! `stride` is still bytes per row and must be at least `width * 4`, exactly as the C assumed.

/// A well-mixed 32-bit hash, so neighbouring pixels get unrelated values.
fn noise_hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Uniform in [0, 1).
fn noise_unit(key: u32) -> f32 {
    (noise_hash(key) >> 8) as f32 * (1.0 / 16777216.0)
}

/// Adds noise to the color of premultiplied RGBA pixels (4 bytes per pixel, `stride` bytes per
/// row), leaving alpha untouched and fully transparent pixels alone. `amount` is Photoshop's
/// percentage: uniform noise spans ±amount% of half the range, Gaussian noise has a standard
/// deviation of two thirds of that. Monochromatic adds the same value to all three channels.
/// Each pixel's noise depends only on its position and `seed`, so the same seed gives the same grain.
pub fn noise_add(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    amount: f32,
    gaussian: i32,
    monochromatic: i32,
    seed: u32,
) {
    noise_add_at(
        rgba,
        width,
        height,
        stride,
        amount,
        gaussian,
        monochromatic,
        seed,
        0,
        0,
    );
}

/// `noise_add`, with the noise's origin offset into the document: a tile drawn at `origin_x`, `origin_y` gets the
/// same grain as the corresponding part of the whole image.
pub fn noise_add_at(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    amount: f32,
    gaussian: i32,
    monochromatic: i32,
    seed: u32,
    origin_x: i64,
    origin_y: i64,
) {
    let spread = amount / 100.0 * 127.5;
    for y in 0..height {
        let row = &mut rgba[y * stride..][..width * 4];
        for x in 0..width {
            let at = x * 4;
            let alpha = row[at + 3];
            if alpha == 0 {
                continue;
            }
            let px = (origin_x + x as i64) as u32;
            let py = (origin_y + y as i64) as u32;
            let base = noise_hash(seed ^ noise_hash(px.wrapping_mul(0x9e37_79b9) ^ noise_hash(py.wrapping_mul(0x85eb_ca6b))));
            for c in 0..3 {
                let key = if monochromatic != 0 {
                    base
                } else {
                    base.wrapping_add((c as u32).wrapping_mul(0x9e37_79b9))
                };
                let n = if gaussian != 0 {
                    // Box–Muller: two uniform values make one normally distributed one.
                    let u1 = noise_unit(key);
                    let u2 = noise_unit(key ^ 0x68e3_1da4);
                    (-2.0 * (1.0 - u1).ln()).sqrt() * (6.2831853 * u2).cos() * spread * (2.0 / 3.0)
                } else {
                    (noise_unit(key) * 2.0 - 1.0) * spread
                };
                let value = row[at + c] as f32 * 255.0 / alpha as f32 + n;
                let value = if value < 0.0 {
                    0.0
                } else if value > 255.0 {
                    255.0
                } else {
                    value
                };
                row[at + c] = (value * alpha as f32 / 255.0).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opaque image whose color varies with the position.
    fn image(width: usize, height: usize) -> Vec<u8> {
        let mut px = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let at = (y * width + x) * 4;
                px[at] = (x * 31 % 256) as u8;
                px[at + 1] = (y * 17 % 256) as u8;
                px[at + 2] = ((x + y) * 7 % 256) as u8;
                px[at + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn noise_is_deterministic_and_depends_on_the_seed() {
        let src = image(8, 8);
        let mut a = src.clone();
        let mut b = src.clone();
        let mut c = src.clone();
        noise_add(&mut a, 8, 8, 32, 40.0, 0, 0, 7);
        noise_add(&mut b, 8, 8, 32, 40.0, 0, 0, 7);
        noise_add(&mut c, 8, 8, 32, 40.0, 0, 0, 8);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn gaussian_noise_is_deterministic() {
        let src = image(6, 6);
        let mut a = src.clone();
        let mut b = src.clone();
        noise_add(&mut a, 6, 6, 24, 60.0, 1, 0, 5);
        noise_add(&mut b, 6, 6, 24, 60.0, 1, 0, 5);
        assert_eq!(a, b);
        assert_ne!(a, src);
    }

    #[test]
    fn zero_amount_leaves_pixels_alone() {
        let src = image(4, 4);
        let mut px = src.clone();
        noise_add(&mut px, 4, 4, 16, 0.0, 0, 0, 99);
        assert_eq!(px, src);
    }

    #[test]
    fn transparent_pixels_and_alpha_are_untouched() {
        let mut px = vec![9, 8, 7, 0, 10, 20, 30, 200];
        noise_add(&mut px, 2, 1, 8, 100.0, 0, 0, 3);
        assert_eq!(&px[0..4], &[9, 8, 7, 0]);
        assert_eq!(px[7], 200);
    }

    #[test]
    fn monochromatic_adds_the_same_value_to_every_channel() {
        let mut px = vec![100, 100, 100, 255];
        noise_add(&mut px, 1, 1, 4, 50.0, 0, 1, 42);
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn noise_changes_a_flat_image() {
        let mut px = vec![128u8; 4 * 4 * 4];
        for pixel in px.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        let src = px.clone();
        noise_add(&mut px, 4, 4, 16, 80.0, 0, 1, 11);
        assert_ne!(px, src);
    }

    /// Ported from `FilterTests.addNoiseChangesColorButNeverAlphaAndMonochromaticKeepsGrays`. The
    /// session's `FilterSettings` round trip stays in `compositor-rs-session`; the seed-7 determinism is
    /// already `noise_is_deterministic_and_depends_on_the_seed`, the untouched transparent pixels
    /// `transparent_pixels_and_alpha_are_untouched`, and the monochromatic equality
    /// `monochromatic_adds_the_same_value_to_every_channel`; what was missing is the grain's range
    /// and spread over a half-opaque field and the per-channel difference of color noise.
    #[test]
    fn add_noise_changes_color_but_never_alpha_and_monochromatic_keeps_grays() {
        // Left half opaque mid gray, right half transparent.
        let (width, height) = (32usize, 8usize);
        let stride = width * 4;
        let mut gray = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let at = y * stride + x * 4;
                if x < 16 {
                    gray[at] = 128;
                    gray[at + 1] = 128;
                    gray[at + 2] = 128;
                    gray[at + 3] = 255;
                }
            }
        }
        let mut color = gray.clone();
        noise_add(&mut color, width, height, stride, 10.0, 0, 0, 7);
        let mut reds = std::collections::HashSet::new();
        let mut differs_per_channel = false;
        for y in 0..height {
            for x in 0..16 {
                let at = y * stride + x * 4;
                assert_eq!(color[at + 3], 255, "alpha never changes");
                assert!(
                    (112..=144).contains(&color[at]),
                    "gray 128 with amount 10: {}",
                    color[at]
                );
                reds.insert(color[at]);
                differs_per_channel |= color[at] != color[at + 1];
            }
        }
        assert!(reds.len() > 5, "the grain varies: {reds:?}");
        assert!(differs_per_channel, "color noise differs per channel");
        let mut mono = gray.clone();
        noise_add(&mut mono, width, height, stride, 10.0, 1, 1, 7);
        for y in 0..height {
            for x in 0..16 {
                let at = y * stride + x * 4;
                assert_eq!(
                    (mono[at], mono[at + 1], mono[at + 2], mono[at + 3]),
                    (mono[at], mono[at], mono[at], 255),
                    "monochromatic keeps grays"
                );
            }
        }
    }

    #[test]
    fn noise_add_at_matches_noise_add_at_the_origin() {
        let src = image(5, 3);
        let mut a = src.clone();
        let mut b = src.clone();
        noise_add(&mut a, 5, 3, 20, 25.0, 1, 0, 11);
        noise_add_at(&mut b, 5, 3, 20, 25.0, 1, 0, 11, 0, 0);
        assert_eq!(a, b);
    }

    #[test]
    fn noise_add_at_uses_the_document_origin() {
        // A tile at x = 4 sees the same grain as the corresponding part of the full image.
        let mut tile = vec![0u8; 4 * 4];
        let mut full = vec![0u8; 8 * 4];
        for pixel in tile.chunks_exact_mut(4).chain(full.chunks_exact_mut(4)) {
            pixel[0] = 200;
            pixel[1] = 100;
            pixel[2] = 50;
            pixel[3] = 255;
        }
        noise_add_at(&mut tile, 4, 1, 16, 30.0, 0, 0, 3, 4, 0);
        noise_add_at(&mut full, 8, 1, 32, 30.0, 0, 0, 3, 0, 0);
        assert_eq!(&tile[..], &full[16..32]);
    }
}
