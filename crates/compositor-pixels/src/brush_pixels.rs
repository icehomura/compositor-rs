//! Port of `references/Compositor/Compositor/Rendering/BrushPixels.{c,h}`.
//!
//! The signatures keep the C shape: `stride` is still bytes per row and `width`/`height` stay
//! separate parameters, so the arithmetic can be diffed against the original line for line.

/// Half-open bounds of nonzero alpha in premultiplied RGBA. Empty returns all zero.
///
/// The C wrote the four values into an out array; here they are returned as
/// `[left, top, right, bottom]`, all in pixels, with `right` and `bottom` exclusive.
pub fn brush_alpha_bounds(bytes: &[u8], width: usize, height: usize, stride: usize) -> [usize; 4] {
    let mut left = width;
    let mut right = 0usize;
    let mut top = height;
    let mut bottom = 0usize;
    for y in 0..height {
        let row = &bytes[y * stride..];
        let mut first = 0usize;
        while first < width && row[first * 4 + 3] == 0 {
            first += 1;
        }
        if first == width {
            continue;
        }
        let mut last = width;
        while last > first && row[(last - 1) * 4 + 3] == 0 {
            last -= 1;
        }
        if first < left {
            left = first;
        }
        if last > right {
            right = last;
        }
        if y < top {
            top = y;
        }
        bottom = y + 1;
    }
    [
        if right != 0 { left } else { 0 },
        if right != 0 { top } else { 0 },
        right,
        bottom,
    ]
}

/// Extracts the alpha channel of a premultiplied RGBA buffer into an 8-bit gray buffer.
///
/// `gray` must hold `height * gray_stride` bytes; each row is written at `gray_stride`, so it may
/// be padded.
pub fn layer_extract_alpha(
    rgba: &[u8],
    rgba_stride: usize,
    gray: &mut [u8],
    gray_stride: usize,
    width: usize,
    height: usize,
) {
    for y in 0..height {
        for x in 0..width {
            gray[y * gray_stride + x] = rgba[y * rgba_stride + x * 4 + 3];
        }
    }
}

/// Turns premultiplied RGBA into straight RGBA in place: divides each color channel by the
/// pixel's alpha (rounding to nearest) and then writes an opaque alpha of 255.
///
/// Transparent pixels (alpha 0) become black; colors whose straight value would exceed 255 are
/// clamped.
pub fn layer_unpremultiply_opaque(rgba: &mut [u8], stride: usize, width: usize, height: usize) {
    for y in 0..height {
        let mut p = y * stride;
        for _ in 0..width {
            let a = rgba[p + 3] as u32;
            for c in 0..3 {
                let v = if a != 0 {
                    (rgba[p + c] as u32 * 255 + a / 2) / a
                } else {
                    0
                };
                rgba[p + c] = if v > 255 { 255 } else { v as u8 };
            }
            rgba[p + 3] = 255;
            p += 4;
        }
    }
}

/// Puts an alpha channel back into straight RGBA in place (the inverse of
/// [`layer_unpremultiply_opaque`]): each color channel is multiplied by the alpha of `alpha`
/// (rounding to nearest) and the alpha channel is written from `alpha`.
///
/// `alpha` is 8-bit gray and may be padded: its rows are read at `alpha_stride`.
pub fn layer_restore_alpha(
    rgba: &mut [u8],
    stride: usize,
    alpha: &[u8],
    alpha_stride: usize,
    width: usize,
    height: usize,
) {
    for y in 0..height {
        let mut p = y * stride;
        for x in 0..width {
            let a = alpha[y * alpha_stride + x] as u32;
            for c in 0..3 {
                rgba[p + c] = ((rgba[p + c] as u32 * a + 127) / 255) as u8;
            }
            rgba[p + 3] = a as u8;
            p += 4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brush_alpha_bounds_empty_is_all_zero() {
        let bytes = vec![0u8; 4 * 3];
        assert_eq!(brush_alpha_bounds(&bytes, 3, 1, 12), [0, 0, 0, 0]);
    }

    #[test]
    fn brush_alpha_bounds_ignores_transparent_pixels() {
        // Fully transparent pixels keep their color bytes but must not extend the bounds.
        let mut bytes = vec![0u8; 4 * 3];
        bytes[0] = 255;
        bytes[1] = 255;
        bytes[2] = 255;
        assert_eq!(brush_alpha_bounds(&bytes, 3, 1, 12), [0, 0, 0, 0]);
    }

    #[test]
    fn brush_alpha_bounds_is_half_open_around_nonzero_alpha() {
        let mut bytes = vec![0u8; 4 * 4 * 2];
        for (x, y) in [(1usize, 0usize), (2, 0), (3, 1)] {
            bytes[(y * 4 + x) * 4 + 3] = 255;
        }
        assert_eq!(brush_alpha_bounds(&bytes, 4, 2, 16), [1, 0, 4, 2]);
    }

    #[test]
    fn brush_alpha_bounds_reads_rows_at_stride() {
        // 2 columns with one pixel of padding: stride is 16, not 8.
        let mut bytes = vec![0u8; 2 * 16];
        bytes[0 * 16 + 1 * 4 + 3] = 255;
        bytes[1 * 16 + 0 * 4 + 3] = 255;
        assert_eq!(brush_alpha_bounds(&bytes, 2, 2, 16), [0, 0, 2, 2]);
    }

    #[test]
    fn layer_extract_alpha_copies_the_alpha_row_by_row() {
        let rgba: Vec<u8> = vec![
            1, 2, 3, 4, 5, 6, 7, 8, //
            9, 10, 11, 12, 13, 14, 15, 16,
        ];
        let mut gray = vec![0u8; 2 * 2];
        layer_extract_alpha(&rgba, 8, &mut gray, 2, 2, 2);
        assert_eq!(gray, [4, 8, 12, 16]);
    }

    #[test]
    fn layer_extract_alpha_respects_a_padded_gray_stride() {
        let rgba: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut gray = vec![0u8; 2 * 3];
        layer_extract_alpha(&rgba, 8, &mut gray, 3, 2, 2);
        assert_eq!(gray, [4, 8, 0, 12, 16, 0]);
    }

    #[test]
    fn layer_unpremultiply_opaque_divides_by_alpha_and_opacifies() {
        let mut rgba = vec![64, 32, 16, 128, 0, 0, 0, 0, 10, 20, 30, 255];
        layer_unpremultiply_opaque(&mut rgba, 12, 3, 1);
        assert_eq!(rgba, [128, 64, 32, 255, 0, 0, 0, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn layer_unpremultiply_opaque_clamps_overshoot() {
        let mut rgba = vec![255, 255, 255, 1];
        layer_unpremultiply_opaque(&mut rgba, 4, 1, 1);
        assert_eq!(rgba, [255, 255, 255, 255]);
    }

    #[test]
    fn layer_restore_alpha_multiplies_with_rounding() {
        let mut rgba = vec![200, 100, 50, 255];
        let alpha = [128u8];
        layer_restore_alpha(&mut rgba, 4, &alpha, 1, 1, 1);
        assert_eq!(rgba, [100, 50, 25, 128]);
    }

    #[test]
    fn layer_restore_alpha_is_opaque_identity() {
        let mut rgba = vec![200, 100, 50, 128];
        let alpha = [255u8];
        layer_restore_alpha(&mut rgba, 4, &alpha, 1, 1, 1);
        assert_eq!(rgba, [200, 100, 50, 255]);
    }

    #[test]
    fn unpremultiply_then_restore_round_trips() {
        let alpha = [128u8, 255, 200, 0];
        let mut rgba = vec![
            64, 32, 16, 128, 10, 20, 30, 255, 100, 50, 25, 200, 0, 0, 0, 0,
        ];
        layer_unpremultiply_opaque(&mut rgba, 16, 4, 1);
        layer_restore_alpha(&mut rgba, 16, &alpha, 4, 4, 1);
        assert_eq!(
            rgba,
            [64, 32, 16, 128, 10, 20, 30, 255, 100, 50, 25, 200, 0, 0, 0, 0]
        );
    }
}
