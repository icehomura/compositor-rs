//! Sharp reductions of layer images (`DownsampleCache`), as the renderer names them.
//!
//! Ported from `Rendering/DownsampleCache.swift`. The cache itself — the chain of halved copies,
//! the Lanczos reduction (`resample::halve`, with the color padding and gray edge replication), the
//! identity keying and the least-recently-used pixel budget — lives in
//! [`compositor_pixels::resample::DownsampleCache`]; this module is the render-side facade with the
//! Swift names (`level(for:)`, `image(_:drawnAt:)`, `image(_:level:)`, `halve(_:)`) that
//! `LayerRenderer` and `TiledLayerRenderer` call, so there is one cache state and one eviction order.
//!
//! Every halving is exactly 2×, so level `k` pixel `i` always covers source pixels `i·2^k ..< (i+1)·2^k`:
//! a piece of an image reduced on its own lines up with the whole image reduced (see
//! `TiledLayerRenderer`).

use std::sync::LazyLock;

use compositor_core::imported_image::PixelImage;
use compositor_core::limits::MAX_SURFACE_PIXELS;

/// The `Arc` pointer behind a shared image, tagged with its variant so an RGBA and a gray raster that
/// happen to share an address never collide.
pub(crate) fn image_identity(image: &PixelImage) -> (u8, usize) {
    match image {
        PixelImage::Rgba(image) => (0, std::sync::Arc::as_ptr(image) as usize),
        PixelImage::Gray(image) => (1, std::sync::Arc::as_ptr(image) as usize),
    }
}

/// `stored.source === image`: same variant, same shared allocation.
pub(crate) fn same_image(lhs: &PixelImage, rhs: &PixelImage) -> bool {
    match (lhs, rhs) {
        (PixelImage::Rgba(a), PixelImage::Rgba(b)) => std::sync::Arc::ptr_eq(a, b),
        (PixelImage::Gray(a), PixelImage::Gray(b)) => std::sync::Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// The shared cache of halved copies, under the names the Swift renderer used.
pub struct DownsampleCache {
    inner: &'static compositor_pixels::resample::DownsampleCache,
}

static SHARED: LazyLock<DownsampleCache> = LazyLock::new(|| DownsampleCache {
    inner: compositor_pixels::resample::DownsampleCache::shared(),
});

impl Default for DownsampleCache {
    fn default() -> Self {
        Self::shared().clone()
    }
}

impl Clone for DownsampleCache {
    fn clone(&self) -> Self {
        DownsampleCache { inner: self.inner }
    }
}

impl DownsampleCache {
    /// Pixels of halved copies kept at once (about 400 MB of RGBA).
    pub const PIXEL_BUDGET: usize = MAX_SURFACE_PIXELS;
    /// Most halvings ever used; past this Core Graphics does the rest.
    pub const MAX_LEVEL: usize = compositor_pixels::resample::MAX_LEVEL;

    pub fn shared() -> &'static DownsampleCache {
        &SHARED
    }

    /// Halvings to draw from when an image lands `factor` output pixels per image pixel: the most that
    /// still leave the copy at least that large (0 from half size up).
    pub fn level_for(factor: f64) -> usize {
        compositor_pixels::resample::DownsampleCache::level(factor)
    }

    /// What to draw when `image` lands `factor` output pixels per image pixel.
    pub fn image(&self, image: &PixelImage, drawn_at: f64) -> PixelImage {
        self.inner.image(image, drawn_at)
    }

    /// `image` reduced by `level` halvings, and how many were applied (fewer only if one failed). Each
    /// halving rounds up, so the copy reaches up to 2^level − 1 source pixels past the right and bottom.
    pub fn image_at_level(&self, image: &PixelImage, wanted: usize) -> (PixelImage, usize) {
        self.inner.image_at_level(image, wanted)
    }

    /// Exactly half the size, rounded up, with Lanczos resampling: the composing primitive.
    ///
    /// Color images are padded with transparent pixels first, so edges fade out the same way wherever
    /// the image is cut; ringing past a pixel's alpha is clamped so edges don't glow. Gray masks repeat
    /// an odd last row or column instead.
    pub fn halve(image: &PixelImage) -> Option<PixelImage> {
        compositor_pixels::resample::DownsampleCache::halve(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::buffer::{Gray8Image, Rgba8Image};
    use std::sync::Arc;

    fn rgba(width: usize, height: usize) -> PixelImage {
        PixelImage::Rgba(Arc::new(Rgba8Image::new(width, height)))
    }

    #[test]
    fn level_for_matches_the_swift_log2_rule() {
        assert_eq!(DownsampleCache::level_for(1.0), 0);
        // 0.5 and up: no halving (the guard rejects factor >= 0.5).
        assert_eq!(DownsampleCache::level_for(0.5), 0);
        assert_eq!(DownsampleCache::level_for(0.75), 0);
        assert_eq!(DownsampleCache::level_for(0.49), 1);
        assert_eq!(DownsampleCache::level_for(0.25), 2);
        assert_eq!(DownsampleCache::level_for(0.2), 2);
        assert_eq!(DownsampleCache::level_for(0.1), 3);
        assert_eq!(DownsampleCache::level_for(0.05), 4);
        assert_eq!(DownsampleCache::level_for(0.02), 5);
        assert_eq!(DownsampleCache::level_for(0.01), 6);
        // Past the ceiling the rest is left to the final resample.
        assert_eq!(DownsampleCache::level_for(0.001), DownsampleCache::MAX_LEVEL);
        // Not finite, not positive: no halving.
        assert_eq!(DownsampleCache::level_for(0.0), 0);
        assert_eq!(DownsampleCache::level_for(-1.0), 0);
        assert_eq!(DownsampleCache::level_for(f64::NAN), 0);
        assert_eq!(DownsampleCache::level_for(f64::INFINITY), 0);
    }

    #[test]
    fn image_at_level_halves_and_reports_the_applied_level() {
        let cache = DownsampleCache::shared();
        let image = rgba(64, 48);
        let (reduced, applied) = cache.image_at_level(&image, 3);
        assert_eq!(applied, 3);
        assert_eq!((reduced.width(), reduced.height()), (8, 6));

        // One halving of an odd size rounds up.
        let odd = rgba(9, 5);
        let (reduced, applied) = cache.image_at_level(&odd, 1);
        assert_eq!(applied, 1);
        assert_eq!((reduced.width(), reduced.height()), (5, 3));

        // Level 0 is the image itself.
        let (same, applied) = cache.image_at_level(&image, 0);
        assert_eq!(applied, 0);
        assert_eq!(same.width(), 64);
    }

    #[test]
    fn a_one_pixel_image_cannot_be_halved_further() {
        let cache = DownsampleCache::shared();
        let image = rgba(1, 1);
        let (same, applied) = cache.image_at_level(&image, 6);
        assert_eq!(applied, 0);
        assert!(same_image(&same, &image));

        // 2×2 becomes 1×1 with a single halving and then stops.
        let small = rgba(2, 2);
        let (reduced, applied) = cache.image_at_level(&small, 6);
        assert_eq!(applied, 1);
        assert_eq!((reduced.width(), reduced.height()), (1, 1));
    }

    #[test]
    fn the_same_raster_is_reused_not_recomputed() {
        let cache = DownsampleCache::shared();
        let image = rgba(32, 32);
        let (first, _) = cache.image_at_level(&image, 2);
        let (second, _) = cache.image_at_level(&image, 2);
        // The cached copy is shared, so both draws read the same halving.
        assert!(same_image(&first, &second));

        // A different raster with the same dimensions is its own chain.
        let other = rgba(32, 32);
        let (other_first, _) = cache.image_at_level(&other, 2);
        assert!(!same_image(&first, &other_first));
    }

    #[test]
    fn levels_are_stable_across_calls() {
        let cache = DownsampleCache::shared();
        let image = rgba(16, 16);
        let (two, _) = cache.image_at_level(&image, 2);
        let (three, applied) = cache.image_at_level(&image, 3);
        // The longer chain grew from the shorter one: it still ends in a fresh level-3 copy.
        assert_eq!(applied, 3);
        assert_eq!((three.width(), three.height()), (2, 2));
        assert!(!same_image(&two, &three));
        // The level-2 copy of the longer chain is the same copy the shorter chain returned.
        let (again, applied) = cache.image_at_level(&image, 2);
        assert_eq!(applied, 2);
        assert!(same_image(&two, &again));
    }

    #[test]
    fn drawn_at_uses_the_halving_that_still_covers_the_target() {
        let cache = DownsampleCache::shared();
        let image = rgba(64, 64);
        // Drawn at 1:1 → no halving.
        assert!(same_image(&cache.image(&image, 1.0), &image));
        // Drawn at 1/4 → two halvings.
        let quarter = cache.image(&image, 0.25);
        assert_eq!((quarter.width(), quarter.height()), (16, 16));
        // Drawn at 0.4 → one halving (log2(2.5) = 1.32).
        let (one, applied) = cache.image_at_level(&image, DownsampleCache::level_for(0.4));
        assert_eq!(applied, 1);
        assert_eq!(one.width(), 32);
    }

    /// Half size and up draws the image itself; large reductions use the halvings, and the copies
    /// come back cached.
    #[test]
    fn halvings_are_reused_and_only_used_for_large_reductions() {
        let cache = DownsampleCache::shared();
        let source = rgba(1024, 512);
        assert!(
            same_image(&cache.image(&source, 0.6), &source),
            "half size and up draws the image itself"
        );
        let eighth = cache.image(&source, 0.125);
        assert_eq!((eighth.width(), eighth.height()), (128, 64));
        assert_eq!(
            cache.image(&source, 0.3).width(),
            512,
            "0.3 uses the half, leaving less than 2x to the final resample"
        );
        assert!(
            same_image(&cache.image(&source, 0.125), &eighth),
            "copies are cached"
        );
    }

    /// Shrinking a translucent image keeps every channel within its alpha, and a mask stays gray
    /// without an alpha channel.
    #[test]
    fn translucent_edges_stay_valid_and_masks_stay_gray() {
        let mut translucent = Rgba8Image::new(512, 512);
        for y in 128..384 {
            for x in 128..384 {
                translucent.set(x, y, [128, 128, 128, 128]);
            }
        }
        let shrunk = DownsampleCache::shared().image(&PixelImage::Rgba(Arc::new(translucent)), 0.25);
        let shrunk = shrunk.as_rgba().expect("a color image");
        assert!(
            shrunk
                .pixels()
                .all(|pixel| pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]),
            "no color above its alpha"
        );

        let mut mask = Gray8Image::new(512, 512);
        for y in 0..512 {
            for x in 0..256 {
                mask.set(x, y, 255);
            }
        }
        let level = DownsampleCache::shared().image(&PixelImage::Gray(Arc::new(mask)), 0.25);
        assert_eq!((level.width(), level.height()), (128, 128));
        assert!(level.as_gray().is_some(), "a mask stays gray");
    }
}
