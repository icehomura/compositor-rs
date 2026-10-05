//! CPU pixel kernels: the ports of `references/Compositor/Compositor/Rendering/*.c` and of the Swift
//! pixel paths that ran on Metal or Core Image. Every kernel works on the canonical buffers
//! (`Rgba8Image`: premultiplied sRGB RGBA8, top-down; `Gray8Image`: 8-bit gray).
//!
//! The C signatures are preserved deliberately — `stride` is still bytes per row — so the arithmetic in
//! this crate can be diffed against the original `*.c` line for line.

pub mod adjust_pixels;
pub mod blend;
pub mod brush_pixels;
pub mod content_fill;
pub mod dither_pixels;
pub mod heal_pixels;
pub mod lens_pixels;
pub mod levels_pixels;
pub mod noise_pixels;
pub mod raster;
pub mod wand_pixels;

/// `rgba_clamp_premultiplied`: after resampling with a filter that rings (Lanczos), premultiplied colors
/// can exceed their alpha; this clamps each channel back to its pixel's alpha.
pub fn rgba_clamp_premultiplied(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        pixel[0] = pixel[0].min(alpha);
        pixel[1] = pixel[1].min(alpha);
        pixel[2] = pixel[2].min(alpha);
    }
}
