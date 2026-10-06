//! Sharp reductions of layer images and the high-quality resampler behind Image Size, Canvas Size
//! and export.
//!
//! Port of `Rendering/DownsampleCache.swift` and of the resampling `IO/ImageResizer.swift` and
//! `IO/CanvasResizer.swift` left to Core Graphics. Core Graphics resamples in one step with a filter
//! that only looks at a few neighbouring pixels, so shrinking an image 4× or 8× comes out soft or
//! grainy at any interpolation quality; the cache keeps a chain of halved copies made with Lanczos
//! resampling (`vImageScale_ARGB8888` / `vImageScale_Planar8` with `kvImageHighQualityResampling`),
//! so a draw only ever leaves Core Graphics the last reduction of at most 2×.
//!
//! The Rust port reproduces the `vImageHighQualityResampling` halvings with a separable Lanczos-3
//! kernel over the premultiplied bytes (each channel resampled on its own, exactly as
//! `vImageScale_ARGB8888` treats a premultiplied buffer, with edge extension at the borders), and
//! clamps ringing past a pixel's alpha back to it ([`crate::rgba_clamp_premultiplied`]).
//!
//! Every halving is exactly 2×, so level `k` pixel `i` always covers source pixels `i·2^k ..< (i+1)·2^k`:
//! a piece of an image reduced on its own lines up with the whole image reduced (see `TiledLayerRenderer`).

use std::sync::{Arc, LazyLock, Mutex};

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use compositor_rs_core::buffer::{Gray8Image, Rgba8Image};
use compositor_rs_core::geom::Rect;
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::limits::MAX_SURFACE_PIXELS;

use crate::rgba_clamp_premultiplied;

/// The resampling filters the editor's resolutions use.
///
/// `Bilinear` is Core Graphics' `.low` quality (and the sampler [`crate::canvas`] uses for its two
/// soft settings); `Lanczos` is `.high` / `kvImageHighQualityResampling`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResampleFilter {
    Bilinear,
    Lanczos,
}

/// The Lanczos window's half-width (`Lanczos-3`, the kernel `vImageHighQualityResampling` uses).
const LANCZOS_A: f64 = 3.0;

/// Most halvings ever used; past this Core Graphics does the rest (`DownsampleCache.maxLevel`).
pub const MAX_LEVEL: usize = 6;

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// The Lanczos kernel `sinc(x)·sinc(x/a)` for `|x| < a`, zero past it.
fn lanczos(x: f64) -> f64 {
    if x.abs() >= LANCZOS_A {
        0.0
    } else {
        sinc(x) * sinc(x / LANCZOS_A)
    }
}

/// The source indices and weights one output coordinate reads.
struct Taps {
    taps: Vec<(i64, f64)>,
}

/// Lanczos taps from `src_len` to `dst_len`.
///
/// Shrinking widens the kernel by `src/dst`, the standard antialiasing rescale: output `i`'s center
/// falls at `(i + 0.5) / scale` in source pixel-center coordinates and the kernel's support is
/// `a·filter_scale` source pixels. Enlarging keeps the kernel at its natural width.
fn lanczos_taps(src_len: usize, dst_len: usize) -> Vec<Taps> {
    let scale = dst_len as f64 / src_len as f64;
    let filter_scale = if scale < 1.0 { 1.0 / scale } else { 1.0 };
    let support = LANCZOS_A * filter_scale;
    (0..dst_len)
        .map(|i| {
            let center = (i as f64 + 0.5) / scale;
            let first = (center - support - 0.5).ceil() as i64;
            let last = (center + support - 0.5).floor() as i64;
            let mut taps: Vec<(i64, f64)> = Vec::new();
            for j in first..=last {
                let x = (j as f64 + 0.5 - center) / filter_scale;
                let weight = lanczos(x);
                if weight != 0.0 {
                    taps.push((j, weight));
                }
            }
            let sum: f64 = taps.iter().map(|(_, weight)| weight).sum();
            if sum != 0.0 {
                for (_, weight) in &mut taps {
                    *weight /= sum;
                }
            }
            Taps { taps }
        })
        .collect()
}

/// Two-tap bilinear weights; the sample coordinate clamps to the source's edges.
fn bilinear_taps(src_len: usize, dst_len: usize) -> Vec<Taps> {
    let scale = dst_len as f64 / src_len as f64;
    (0..dst_len)
        .map(|i| {
            let position = (i as f64 + 0.5) / scale - 0.5;
            let base = position.floor();
            let fraction = position - base;
            let mut taps = vec![(base as i64, 1.0 - fraction)];
            if fraction != 0.0 {
                taps.push((base as i64 + 1, fraction));
            }
            Taps { taps }
        })
        .collect()
}

fn taps_for(src_len: usize, dst_len: usize, filter: ResampleFilter) -> Vec<Taps> {
    match filter {
        ResampleFilter::Bilinear => bilinear_taps(src_len, dst_len),
        ResampleFilter::Lanczos => lanczos_taps(src_len, dst_len),
    }
}

fn clamped_index(index: i64, len: usize) -> usize {
    index.clamp(0, len as i64 - 1) as usize
}

/// Resamples a `sw`×`sh` plane (`src_stride` bytes a row, `channels` bytes a pixel) to `dw`×`dh`,
/// separably: the horizontal pass runs over source rows, the vertical over output rows, each output
/// sample's sum independent of the others, so the result is identical single- or multi-threaded.
fn resample_plane(
    source: &[u8],
    src_stride: usize,
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    channels: usize,
    filter: ResampleFilter,
) -> Vec<u8> {
    let horizontal = taps_for(sw, dw, filter);
    let vertical = taps_for(sh, dh, filter);
    let row_bytes = dw * channels;

    // Horizontal pass: each temporary row is one source row resampled.
    let mut middle = vec![0u8; row_bytes * sh];
    middle
        .par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(y, row)| {
            let source_row = &source[y * src_stride..y * src_stride + sw * channels];
            for (x, taps) in horizontal.iter().enumerate() {
                let mut sum = [0.0f64; 4];
                for (index, weight) in &taps.taps {
                    let base = clamped_index(*index, sw) * channels;
                    for channel in 0..channels {
                        sum[channel] += source_row[base + channel] as f64 * weight;
                    }
                }
                for channel in 0..channels {
                    row[x * channels + channel] = sum[channel].round().clamp(0.0, 255.0) as u8;
                }
            }
        });

    // Vertical pass: each output row is one column of the temporary reduced.
    let mut destination = vec![0u8; row_bytes * dh];
    destination
        .par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(y, row)| {
            let mut row_sum = vec![[0.0f64; 4]; dw];
            for (index, weight) in &vertical[y].taps {
                let source_row = &middle[clamped_index(*index, sh) * row_bytes..][..row_bytes];
                for x in 0..dw {
                    let base = x * channels;
                    for channel in 0..channels {
                        row_sum[x][channel] += source_row[base + channel] as f64 * weight;
                    }
                }
            }
            for x in 0..dw {
                for channel in 0..channels {
                    row[x * channels + channel] = row_sum[x][channel].round().clamp(0.0, 255.0) as u8;
                }
            }
        });

    destination
}

/// `image` resampled to `width` × `height` with `filter`, premultiplied ringing clamped back.
pub fn scale_rgba(source: &Rgba8Image, width: usize, height: usize, filter: ResampleFilter) -> Rgba8Image {
    if source.is_empty() || width == 0 || height == 0 {
        return Rgba8Image::new(width.max(1), height.max(1));
    }
    let mut data = resample_plane(source.data(), source.stride(), source.width(), source.height(), width, height, 4, filter);
    rgba_clamp_premultiplied(&mut data);
    Rgba8Image::from_data(width, height, data)
}

/// `image` resampled to `width` × `height` with `filter`.
pub fn scale_gray(source: &Gray8Image, width: usize, height: usize, filter: ResampleFilter) -> Gray8Image {
    if source.is_empty() || width == 0 || height == 0 {
        return Gray8Image::new(width.max(1), height.max(1));
    }
    let data = resample_plane(source.data(), source.stride(), source.width(), source.height(), width, height, 1, filter);
    Gray8Image::from_data(width, height, data)
}

/// `image` resampled to `width` × `height`, keeping its kind.
pub fn scale_image(source: &PixelImage, width: usize, height: usize, filter: ResampleFilter) -> PixelImage {
    match source {
        PixelImage::Rgba(image) => PixelImage::Rgba(Arc::new(scale_rgba(image, width, height, filter))),
        PixelImage::Gray(image) => PixelImage::Gray(Arc::new(scale_gray(image, width, height, filter))),
    }
}

/// `PixelAdjust.thumbnail(of:)`: a copy no larger than `max_side` on its longest side (96 for the
/// Layers panel). The Swift draws it through `BrushRaster.draw`, whose interpolation quality is
/// `.none`, so the nearest source pixel is taken; `Int(CGFloat(width) * factor)` truncates.
pub fn thumbnail(image: &PixelImage, max_side: f64) -> PixelImage {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return image.clone();
    }
    let factor = (max_side / width.max(height) as f64).min(1.0);
    let target_width = ((width as f64 * factor) as usize).max(1);
    let target_height = ((height as f64 * factor) as usize).max(1);
    match image {
        PixelImage::Rgba(source) => PixelImage::Rgba(Arc::new(nearest_rgba(source, target_width, target_height))),
        PixelImage::Gray(source) => PixelImage::Gray(Arc::new(nearest_gray(source, target_width, target_height))),
    }
}

fn nearest_rgba(source: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut result = Rgba8Image::new(width, height);
    for y in 0..height {
        let sy = (((y as f64 + 0.5) * source.height() as f64 / height as f64) as usize).min(source.height() - 1);
        for x in 0..width {
            let sx = (((x as f64 + 0.5) * source.width() as f64 / width as f64) as usize).min(source.width() - 1);
            let pixel = source.get(sx, sy);
            result.set(x, y, pixel);
        }
    }
    result
}

fn nearest_gray(source: &Gray8Image, width: usize, height: usize) -> Gray8Image {
    let mut result = Gray8Image::new(width, height);
    for y in 0..height {
        let sy = (((y as f64 + 0.5) * source.height() as f64 / height as f64) as usize).min(source.height() - 1);
        for x in 0..width {
            let sx = (((x as f64 + 0.5) * source.width() as f64 / width as f64) as usize).min(source.width() - 1);
            result.set(x, y, source.get(sx, sy));
        }
    }
    result
}

struct Entry {
    source: PixelImage,
    levels: Vec<PixelImage>,
    last_use: u64,
}

impl Entry {
    fn pixels(&self) -> usize {
        self.levels.iter().map(|level| level.pixel_count()).sum()
    }
}

struct Inner {
    entries: FxHashMap<(bool, usize), Entry>,
    clock: u64,
}

/// Sharp reductions of layer images (`DownsampleCache`).
///
/// Copies are keyed by image identity — painting makes a new image — and the least recently used are
/// dropped beyond a pixel budget.
pub struct DownsampleCache {
    inner: Mutex<Inner>,
}

/// The image's identity: its kind and the address of its pixels. Holding the source in an entry keeps
/// that allocation alive, so the address cannot be reused while the copies are cached.
fn identity(image: &PixelImage) -> (bool, usize) {
    match image {
        PixelImage::Rgba(image) => (false, image.data().as_ptr() as usize),
        PixelImage::Gray(image) => (true, image.data().as_ptr() as usize),
    }
}

static SHARED: LazyLock<DownsampleCache> = LazyLock::new(|| DownsampleCache {
    inner: Mutex::new(Inner { entries: FxHashMap::default(), clock: 0 }),
});

impl DownsampleCache {
    pub fn shared() -> &'static DownsampleCache {
        &SHARED
    }

    /// Halvings to draw from when an image lands `factor` output pixels per image pixel: the most
    /// that still leave the copy at least that large (0 from half size up).
    pub fn level(factor: f64) -> usize {
        if !factor.is_finite() || factor <= 0.0 || factor >= 0.5 {
            return 0;
        }
        (1.0 / factor).log2().floor().min(MAX_LEVEL as f64) as usize
    }

    /// What to draw when `image` lands `factor` output pixels per image pixel.
    pub fn image(&self, image: &PixelImage, drawn_at: f64) -> PixelImage {
        self.image_at_level(image, Self::level(drawn_at)).0
    }

    /// `image` reduced by `wanted` halvings, and how many were applied (fewer only if one failed).
    /// Each halving rounds up, so the copy reaches up to `2^level − 1` source pixels past the right
    /// and bottom.
    pub fn image_at_level(&self, image: &PixelImage, wanted: usize) -> (PixelImage, usize) {
        if wanted < 1 || !(image.width() > 1 || image.height() > 1) {
            return (image.clone(), 0);
        }
        let key = identity(image);
        let (mut levels, clock) = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner.clock += 1;
            let clock = inner.clock;
            let levels = inner
                .entries
                .get(&key)
                .filter(|entry| identity(&entry.source) == key)
                .map(|entry| entry.levels.clone())
                .unwrap_or_default();
            (levels, clock)
        };
        while levels.len() < wanted {
            let previous = levels.last().unwrap_or(image);
            if !(previous.width() > 1 || previous.height() > 1) {
                break;
            }
            match Self::halve(previous) {
                Some(next) => levels.push(next),
                None => break,
            }
        }
        if levels.is_empty() {
            return (image.clone(), 0);
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            match inner.entries.get_mut(&key) {
                Some(stored) if identity(&stored.source) == key && stored.levels.len() >= levels.len() => {
                    stored.last_use = clock;
                }
                _ => {
                    inner
                        .entries
                        .insert(key, Entry { source: image.clone(), levels: levels.clone(), last_use: clock });
                }
            }
            inner.evict(key);
        }
        let applied = wanted.min(levels.len());
        (levels[applied - 1].clone(), applied)
    }

    /// Exactly half the size, rounded up, with Lanczos resampling. Color images are padded with
    /// transparent pixels first, so edges fade out the same way wherever the image is cut; ringing
    /// past a pixel's alpha is clamped so edges don't glow. Gray masks repeat an odd last row or
    /// column instead.
    pub fn halve(image: &PixelImage) -> Option<PixelImage> {
        let is_mask = image.is_mask();
        let (image_width, image_height) = (image.width(), image.height());
        if image_width == 0 || image_height == 0 {
            return None;
        }
        let width = (image_width + 1) / 2;
        let height = (image_height + 1) / 2;
        let pad = if is_mask { 0 } else { 8 };
        let padded_width = width * 2 + pad * 2;
        let padded_height = height * 2 + pad * 2;
        // `source.clear`: a color context starts transparent, a mask context black — both zero bytes.
        let mut source = match image {
            PixelImage::Rgba(_) => PixelImage::Rgba(Arc::new(Rgba8Image::new(padded_width, padded_height))),
            PixelImage::Gray(_) => PixelImage::Gray(Arc::new(Gray8Image::new(padded_width, padded_height))),
        };
        place_at(&mut source, image, pad, pad);
        if is_mask {
            // An odd last column or row is repeated, so the reduction's edge extension matches the
            // image rather than the zero bytes past it.
            if padded_width > image_width {
                repeat_column(&mut source, image_width - 1, image_height, image_width);
            }
            if padded_height > image_height {
                repeat_row(&mut source, image_height - 1, padded_width, image_height);
            }
        }
        let mut halved = scale_image(&source, padded_width / 2, padded_height / 2, ResampleFilter::Lanczos);
        if !is_mask {
            if let PixelImage::Rgba(rgba) = &mut halved {
                rgba_clamp_premultiplied(Arc::make_mut(rgba).data_mut());
            }
        }
        if is_mask {
            return Some(halved);
        }
        let crop = Rect::new((pad / 2) as f64, (pad / 2) as f64, width as f64, height as f64);
        match halved {
            PixelImage::Rgba(rgba) => rgba.cropped(crop).map(|image| PixelImage::Rgba(Arc::new(image))),
            PixelImage::Gray(gray) => gray.cropped(crop).map(|image| PixelImage::Gray(Arc::new(image))),
        }
    }
}

impl Inner {
    /// Drops least recently used copies until the rest fit the budget.
    fn evict(&mut self, keeping: (bool, usize)) {
        let mut total: usize = self.entries.values().map(|entry| entry.pixels()).sum();
        while total > MAX_SURFACE_PIXELS {
            let oldest = self
                .entries
                .iter()
                .filter(|(key, _)| **key != keeping)
                .min_by_key(|(_, entry)| entry.last_use)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else { break };
            if let Some(removed) = self.entries.remove(&oldest) {
                total -= removed.pixels();
            }
        }
    }
}

/// `BrushRaster.draw`: `image` copied into `destination` at (`x`, `y`), replacing pixels outright,
/// with nearest sampling (interpolation off).
fn place_at(destination: &mut PixelImage, image: &PixelImage, x: usize, y: usize) {
    match (destination, image) {
        (PixelImage::Rgba(destination), PixelImage::Rgba(image)) => {
            let destination = Arc::make_mut(destination);
            for row in 0..image.height() {
                let target = (y + row) * destination.width() + x;
                if y + row >= destination.height() || x + image.width() > destination.width() {
                    break;
                }
                destination.data_mut()[target * 4..target * 4 + image.width() * 4]
                    .copy_from_slice(&image.data()[row * image.stride()..row * image.stride() + image.width() * 4]);
            }
        }
        (PixelImage::Gray(destination), PixelImage::Gray(image)) => {
            let destination = Arc::make_mut(destination);
            for row in 0..image.height() {
                if y + row >= destination.height() || x + image.width() > destination.width() {
                    break;
                }
                let target = (y + row) * destination.width() + x;
                destination.data_mut()[target..target + image.width()]
                    .copy_from_slice(&image.data()[row * image.width()..row * image.width() + image.width()]);
            }
        }
        _ => {}
    }
}

/// Copies a color or mask column into the padding at `x`.
fn repeat_column(image: &mut PixelImage, source_x: usize, height: usize, x: usize) {
    match image {
        PixelImage::Rgba(image) => {
            let image = Arc::make_mut(image);
            for y in 0..height {
                let pixel = image.get(source_x, y);
                image.set(x, y, pixel);
            }
        }
        PixelImage::Gray(image) => {
            let image = Arc::make_mut(image);
            for y in 0..height {
                let value = image.get(source_x, y);
                image.set(x, y, value);
            }
        }
    }
}

/// Copies a color or mask row into the padding at `y`.
fn repeat_row(image: &mut PixelImage, source_y: usize, width: usize, y: usize) {
    match image {
        PixelImage::Rgba(image) => {
            let image = Arc::make_mut(image);
            for x in 0..width {
                let pixel = image.get(x, source_y);
                image.set(x, y, pixel);
            }
        }
        PixelImage::Gray(image) => {
            let image = Arc::make_mut(image);
            for x in 0..width {
                let value = image.get(x, source_y);
                image.set(x, y, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(width: usize, height: usize, pixel: impl Fn(usize, usize) -> [u8; 4]) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, pixel(x, y));
            }
        }
        image
    }

    /// `DownsampleTests.halvingsAreReusedAndOnlyUsedForLargeReductions`' rule, on the level, without
    /// the 1024×512 fixtures.
    #[test]
    fn level_is_zero_from_half_size_up() {
        assert_eq!(DownsampleCache::level(0.6), 0);
        assert_eq!(DownsampleCache::level(0.5), 0);
        assert_eq!(DownsampleCache::level(1.0), 0);
        assert_eq!(DownsampleCache::level(2.0), 0);
        assert_eq!(DownsampleCache::level(0.0), 0);
        assert_eq!(DownsampleCache::level(-0.25), 0);
        assert_eq!(DownsampleCache::level(f64::NAN), 0);
        assert_eq!(DownsampleCache::level(f64::INFINITY), 0);
        // floor(log2(1/factor)), capped at 6.
        assert_eq!(DownsampleCache::level(0.4), 1);
        assert_eq!(DownsampleCache::level(0.3), 1);
        assert_eq!(DownsampleCache::level(0.25), 2);
        assert_eq!(DownsampleCache::level(0.125), 3);
        assert_eq!(DownsampleCache::level(0.0625), 4);
        assert_eq!(DownsampleCache::level(0.001), MAX_LEVEL);
    }

    #[test]
    fn halving_rounds_up_and_reaches_the_cached_copy() {
        let source = PixelImage::Rgba(Arc::new(rgba(1024, 512, |_, _| [10, 20, 30, 255])));
        // Half size and up draws the image itself.
        assert!(identity(&DownsampleCache::shared().image(&source, 0.6)) == identity(&source));
        let eighth = DownsampleCache::shared().image(&source, 0.125);
        assert_eq!((eighth.width(), eighth.height()), (128, 64));
        assert_eq!(DownsampleCache::shared().image(&source, 0.3).width(), 512);
        // Copies are cached by identity, so the same reduction comes back.
        let again = DownsampleCache::shared().image(&source, 0.125);
        assert_eq!(identity(&again), identity(&eighth));
        // An odd side rounds up when halved.
        let odd = PixelImage::Gray(Arc::new(Gray8Image::uniform(5, 3, 200)));
        let halved = DownsampleCache::halve(&odd).expect("halved");
        assert_eq!((halved.width(), halved.height()), (3, 2));
    }

    /// A resize down and back up keeps a flat field flat: Lanczos preserves constants (the weights
    /// sum to one), which is the round-trip property the halving chain relies on.
    #[test]
    fn lanczos_round_trip_preserves_a_flat_field() {
        let source = rgba(8, 8, |_, _| [12, 34, 56, 78]);
        let small = scale_rgba(&source, 3, 3, ResampleFilter::Lanczos);
        for pixel in small.pixels() {
            assert_eq!(pixel, [12, 34, 56, 78]);
        }
        let back = scale_rgba(&small, 8, 8, ResampleFilter::Lanczos);
        for pixel in back.pixels() {
            assert_eq!(pixel, [12, 34, 56, 78]);
        }
        // Gray on the same path.
        let gray = Gray8Image::uniform(6, 6, 137);
        let small = scale_gray(&gray, 2, 4, ResampleFilter::Lanczos);
        let back = scale_gray(&small, 6, 6, ResampleFilter::Lanczos);
        assert!(back.data().iter().all(|value| *value == 137));
    }

    /// Lanczos ringing past a pixel's alpha is clamped back, so no channel sits above it.
    #[test]
    fn resampled_edge_keeps_color_within_alpha() {
        let mut source = rgba(32, 32, |_, _| [0, 0, 0, 0]);
        for y in 8..24 {
            for x in 8..24 {
                source.set(x, y, [255, 255, 255, 128]);
            }
        }
        let small = scale_rgba(&source, 8, 8, ResampleFilter::Lanczos);
        for pixel in small.pixels() {
            assert!(pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3], "{pixel:?}");
        }
    }

    /// A mask halving stays gray and an odd last row/column is repeated rather than left black.
    #[test]
    fn mask_halving_repeats_the_odd_last_column() {
        let mut mask = Gray8Image::new(3, 1);
        mask.set(0, 0, 0);
        mask.set(1, 0, 0);
        mask.set(2, 0, 255);
        let halved = DownsampleCache::halve(&PixelImage::Gray(Arc::new(mask))).expect("halved");
        assert_eq!((halved.width(), halved.height()), (2, 1));
        let gray = halved.as_gray().expect("gray");
        // Right half is white because the odd last column was repeated, not left black.
        assert!(gray.get(1, 0) > gray.get(0, 0));
    }

    #[test]
    fn thumbnail_truncates_to_the_longest_side() {
        let source = PixelImage::Rgba(Arc::new(rgba(400, 200, |x, y| [x as u8, y as u8, 0, 255])));
        let thumb = thumbnail(&source, 96.0);
        assert_eq!((thumb.width(), thumb.height()), (96, 48));
        let small = thumbnail(&PixelImage::Gray(Arc::new(Gray8Image::uniform(10, 20, 7))), 96.0);
        assert_eq!((small.width(), small.height()), (10, 20));
    }
}
