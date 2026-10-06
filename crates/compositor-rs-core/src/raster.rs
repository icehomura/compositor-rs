//! The immutable sparse raster: paint commits share untouched tiles with their source, and a
//! contiguous backing is materialized only when a consumer (export or an image-processing
//! operation) actually asks for its bytes — never on mouse-up.
//!
//! Port of `Rendering/RasterSnapshot.swift`. Swift's `CGImage` becomes `buffer::Rgba8Image` for
//! color and `buffer::Gray8Image` for masks; a `CGContext` drawing target becomes a
//! [`RasterTarget`]. `BrushRaster.context`/`BrushRaster.draw` (`Document/BrushStroke.swift`) are
//! ported in `compositor-rs-pixels`; the nearest-neighbour placement this file needs is implemented
//! here, since a snapshot must be able to compose itself.

use crate::buffer::{Gray8Image, Rgba8Image, GRAY_PIXEL, RGBA_PIXEL, TILE};
use crate::geom::{Cell, Point, Rect};
use crate::imported_image::{ImportedImage, PixelImage};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// The pixels a patch — or a snapshot's base — holds: color for a layer, coverage for a mask.
///
/// The same enum as [`PixelImage`] (`imported_image::PixelImage`), under the name the raster and
/// the brush engine use for it.
pub type PatchImage = PixelImage;

/// A complete replacement tile of a sparse raster (`BrushPatch`): the rectangle it covers and the
/// pixels to place there.
#[derive(Clone, Debug)]
pub struct BrushPatch {
    pub rect: Rect,
    pub image: PatchImage,
}

impl BrushPatch {
    pub fn new(rect: Rect, image: PatchImage) -> Self {
        Self { rect, image }
    }
}

/// A bitmap a snapshot composes into — the port of the `CGContext` `BrushRaster.context` made.
pub enum RasterTarget<'a> {
    /// Premultiplied sRGB RGBA8 (`CGContext(mask: false)`).
    Rgba(&'a mut Rgba8Image),
    /// 8-bit gray (`CGContext(mask: true)`).
    Gray(&'a mut Gray8Image),
}

impl RasterTarget<'_> {
    pub fn width(&self) -> usize {
        match self {
            Self::Rgba(image) => image.width(),
            Self::Gray(image) => image.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Self::Rgba(image) => image.height(),
            Self::Gray(image) => image.height(),
        }
    }

    /// Fills `rect` with a gray value 0…1, as `setFillColor(gray:alpha: 1)` plus `fill` do; a mask's
    /// background past its pixels. Drawing into a color target writes the same value as opaque gray.
    fn fill_rect(&mut self, rect: Rect, value: f64) {
        let byte = (value * 255.0).round().clamp(0.0, 255.0) as u8;
        let (width, height) = (self.width(), self.height());
        match self {
            Self::Rgba(target) => covered_pixels(rect, width, height, |x, y| target.set(x, y, [byte, byte, byte, 255])),
            Self::Gray(target) => covered_pixels(rect, width, height, |x, y| target.set(x, y, byte)),
        }
    }

    /// Draws `image` into `dest`, clipped to `clip`, with nearest-neighbour sampling and Core
    /// Graphics' `.copy` blend mode: the destination pixels are replaced outright, transparent
    /// source pixels included (`BrushRaster.draw`, whose antialiasing is off and whose sampling is
    /// `interpolationQuality = .none`).
    ///
    /// A gray coverage image placed in a color target becomes opaque gray, and a color image placed
    /// in a gray target contributes its coverage (the red channel), which is what the single gray
    /// brush context upstream always received.
    fn blit(&mut self, image: &PatchImage, dest: Rect, clip: Rect) {
        let (source_width, source_height) = (image.width(), image.height());
        if source_width == 0 || source_height == 0 || dest.is_null() || dest.is_empty() {
            return;
        }
        let clip = clip.intersection(dest);
        if clip.is_null() || clip.is_empty() {
            return;
        }
        let (width, height) = (self.width(), self.height());
        let sx = |x: usize| -> usize {
            (((x as f64 + 0.5 - dest.min_x()) / dest.width() * source_width as f64).floor().max(0.0) as usize).min(source_width - 1)
        };
        let sy = |y: usize| -> usize {
            (((y as f64 + 0.5 - dest.min_y()) / dest.height() * source_height as f64).floor().max(0.0) as usize)
                .min(source_height - 1)
        };
        match self {
            Self::Rgba(target) => covered_pixels(clip, width, height, |x, y| {
                let (ix, iy) = (sx(x), sy(y));
                let pixel = match image {
                    PixelImage::Rgba(source) => source.get(ix, iy),
                    PixelImage::Gray(source) => {
                        let value = source.get(ix, iy);
                        [value, value, value, 255]
                    }
                };
                target.set(x, y, pixel);
            }),
            Self::Gray(target) => covered_pixels(clip, width, height, |x, y| {
                let (ix, iy) = (sx(x), sy(y));
                let value = match image {
                    PixelImage::Gray(source) => source.get(ix, iy),
                    PixelImage::Rgba(source) => source.get(ix, iy)[0],
                };
                target.set(x, y, value);
            }),
        }
    }
}

/// Runs `body` over every pixel of a `width` × `height` bitmap whose center falls inside `rect`.
///
/// Core Graphics' hard-edged rule with antialiasing and interpolation off: an edge landing on a
/// pixel center excludes that pixel (half-open), and pixels outside the bitmap are skipped.
fn covered_pixels(rect: Rect, width: usize, height: usize, mut body: impl FnMut(usize, usize)) {
    if rect.is_null() || rect.is_empty() || width == 0 || height == 0 {
        return;
    }
    let x0 = rect.min_x().max(0.0).floor() as usize;
    let x1 = (rect.max_x().min(width as f64).ceil() as usize).min(width);
    let y0 = rect.min_y().max(0.0).floor() as usize;
    let y1 = (rect.max_y().min(height as f64).ceil() as usize).min(height);
    for y in y0..y1 {
        let center_y = y as f64 + 0.5;
        if center_y < rect.min_y() || center_y >= rect.max_y() {
            continue;
        }
        for x in x0..x1 {
            let center_x = x as f64 + 0.5;
            if center_x < rect.min_x() || center_x >= rect.max_x() {
                continue;
            }
            body(x, y);
        }
    }
}

/// A thumbnail no larger than 96 pixels on its longest side (`RasterSnapshot.thumbnail()`).
pub const THUMBNAIL_MAX_SIDE: f64 = 96.0;

/// Immutable sparse raster. Paint commits share untouched tiles with their source.
///
/// A contiguous image backing is materialized only when a consumer (export or an image-processing
/// operation) actually requests its bytes, never on mouse-up.
#[derive(Debug)]
pub struct RasterSnapshot {
    pub width: usize,
    pub height: usize,
    pub base: Option<PatchImage>,
    pub base_rect: Rect,
    pub patches: Vec<BrushPatch>,
    pub is_mask: bool,
    /// A mask's value past its base and patches, set once as the snapshot is made.
    pub fill: f64,
    /// Where this raster's halving grids start (see `TiledLayerRenderer`): its base's origin, or for
    /// a raster painted from nothing, the grid of the stroke that made it — carried across commits
    /// so they never shift.
    pub alignment: Point,
    materialized: Mutex<Option<PatchImage>>,
}

impl Clone for RasterSnapshot {
    /// Swift's `RasterSnapshot` is a class, so a copy is another reference to the same raster; here
    /// the shared rasters and the materialized backing are `Arc`s, so a clone only bumps counts.
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            base: self.base.clone(),
            base_rect: self.base_rect,
            patches: self.patches.clone(),
            is_mask: self.is_mask,
            fill: self.fill,
            alignment: self.alignment,
            materialized: Mutex::new(self.materialized.lock().clone()),
        }
    }
}

impl RasterSnapshot {
    /// `RasterSnapshot(width:height:base:baseRect:patches:isMask:alignment:)`. `alignment` defaults
    /// to `base_rect`'s origin; `fill` starts at 1 and is set by [`RasterSnapshot::replacing`].
    pub fn new(
        width: usize,
        height: usize,
        base: Option<PatchImage>,
        base_rect: Rect,
        patches: Vec<BrushPatch>,
        is_mask: bool,
        alignment: Option<Point>,
    ) -> Self {
        Self {
            width,
            height,
            base,
            base_rect,
            patches,
            is_mask,
            fill: 1.0,
            alignment: alignment.unwrap_or_else(|| Point::new(base_rect.min_x(), base_rect.min_y())),
            materialized: Mutex::new(None),
        }
    }

    /// `isMask ? 1 : 4`: the bytes a materialized pixel takes.
    pub fn bytes_per_pixel(&self) -> usize {
        if self.is_mask {
            GRAY_PIXEL
        } else {
            RGBA_PIXEL
        }
    }

    /// New patches are complete replacement tiles, including transparent pixels. Split older patches
    /// at their edges to keep the display list disjoint and flat.
    pub fn replacing(
        source: Option<&ImportedImage>,
        source_rect: Rect,
        patches: &[BrushPatch],
        crop: Rect,
        is_mask: bool,
        fill: f64,
    ) -> RasterSnapshot {
        let old = source.and_then(|source| source.raster.as_ref());
        let dx = source_rect.min_x() - crop.min_x();
        let dy = source_rect.min_y() - crop.min_y();
        let mut current: Vec<BrushPatch> = old
            .map(|old| {
                old.patches
                    .iter()
                    .map(|patch| BrushPatch::new(patch.rect.offset_by(dx, dy), patch.image.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let additions: Vec<BrushPatch> = patches
            .iter()
            .map(|patch| BrushPatch::new(patch.rect.offset_by(-crop.min_x(), -crop.min_y()), patch.image.clone()))
            .collect();
        // Spatial indexing keeps the handoff proportional to touched tiles, rather than comparing
        // every old tile with every new tile on a large document.
        let mut buckets: FxHashMap<Cell, Vec<usize>> = FxHashMap::default();
        for (index, addition) in additions.iter().enumerate() {
            for cell in cells(addition.rect) {
                buckets.entry(cell).or_default().push(index);
            }
        }
        current = current
            .into_iter()
            .flat_map(|patch| {
                // The Swift `Set` has no defined iteration order; indices ascend so the split is
                // deterministic.
                let mut candidates: Vec<usize> = cells(patch.rect).into_iter().filter_map(|cell| buckets.get(&cell)).flatten().copied().collect();
                candidates.sort_unstable();
                candidates.dedup();
                let mut pieces = vec![patch];
                for index in candidates {
                    let addition = &additions[index];
                    let mut next = Vec::with_capacity(pieces.len());
                    for piece in pieces {
                        let overlap = piece.rect.intersection(addition.rect);
                        if overlap.is_null() || overlap.is_empty() {
                            next.push(piece);
                            continue;
                        }
                        let rect = piece.rect;
                        let splits = [
                            Rect::new(rect.min_x(), rect.min_y(), rect.width(), overlap.min_y() - rect.min_y()),
                            Rect::new(rect.min_x(), overlap.max_y(), rect.width(), rect.max_y() - overlap.max_y()),
                            Rect::new(rect.min_x(), overlap.min_y(), overlap.min_x() - rect.min_x(), overlap.height()),
                            Rect::new(overlap.max_x(), overlap.min_y(), rect.max_x() - overlap.max_x(), overlap.height()),
                        ];
                        for split in splits {
                            if split.width() > 0.0 && split.height() > 0.0 {
                                if let Some(image) = piece.image.cropped(split.offset_by(-rect.min_x(), -rect.min_y())) {
                                    next.push(BrushPatch::new(split, image));
                                }
                            }
                        }
                    }
                    pieces = next;
                }
                pieces
            })
            .collect();
        current.extend(additions.iter().cloned());
        let bounds = Rect::new(0.0, 0.0, crop.width(), crop.height());
        let patches: Vec<BrushPatch> = current
            .into_iter()
            .filter_map(|patch| {
                let rect = patch.rect.intersection(bounds);
                if rect.is_null() || rect.is_empty() {
                    return None;
                }
                if rect == patch.rect {
                    return Some(patch);
                }
                patch.image.cropped(rect.offset_by(-patch.rect.min_x(), -patch.rect.min_y())).map(|image| BrushPatch::new(rect, image))
            })
            .collect();
        let base = match old {
            Some(old) => old.base.clone(),
            None => source.map(|source| source.image.clone()),
        };
        let base_rect = match old {
            Some(old) => old.base_rect.offset_by(dx, dy),
            None => source_rect.offset_by(-crop.min_x(), -crop.min_y()),
        };
        let alignment = Point::new(
            source_rect.min_x() + old.map(|old| old.alignment.x).unwrap_or(0.0) - crop.min_x(),
            source_rect.min_y() + old.map(|old| old.alignment.y).unwrap_or(0.0) - crop.min_y(),
        );
        let mut result = RasterSnapshot::new(
            crop.width().max(0.0) as usize,
            crop.height().max(0.0) as usize,
            base,
            base_rect,
            patches,
            is_mask,
            Some(alignment),
        );
        result.fill = fill;
        result
    }

    /// The snapshot's own pixel grid, `(0, 0, width, height)`.
    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f64, self.height as f64)
    }

    /// Draws only the pixels inside `rect` into `target`, this raster's grid stretched over that
    /// rectangle (`draw(in:context:)`).
    pub fn compose(&self, target: &mut RasterTarget<'_>, rect: Rect) {
        if rect.is_null() || rect.is_empty() {
            return;
        }
        let visible = rect.intersection(Rect::new(0.0, 0.0, target.width() as f64, target.height() as f64));
        if visible.is_null() || visible.is_empty() {
            return;
        }
        if self.is_mask {
            // Past its original extent a grown mask is its background: white reveals, black hides.
            target.fill_rect(visible, self.fill);
        }
        let sx = rect.width() / self.width as f64;
        let sy = rect.height() / self.height as f64;
        let mapped = |source: Rect| {
            Rect::new(
                rect.min_x() + source.min_x() * sx,
                rect.min_y() + source.min_y() * sy,
                source.width() * sx,
                source.height() * sy,
            )
        };
        if let Some(base) = &self.base {
            let destination = mapped(self.base_rect);
            let overlap = destination.intersection(visible);
            if !overlap.is_null() && !overlap.is_empty() && destination.width() > 0.0 && destination.height() > 0.0 {
                let base_width = base.width() as f64;
                let base_height = base.height() as f64;
                let crop = Rect::new(
                    (overlap.min_x() - destination.min_x()) / destination.width() * base_width,
                    (overlap.min_y() - destination.min_y()) / destination.height() * base_height,
                    overlap.width() / destination.width() * base_width,
                    overlap.height() / destination.height() * base_height,
                )
                .integral();
                if let Some(image) = base.cropped(crop) {
                    let placed = Rect::new(
                        destination.min_x() + crop.min_x() / base_width * destination.width(),
                        destination.min_y() + crop.min_y() / base_height * destination.height(),
                        crop.width() / base_width * destination.width(),
                        crop.height() / base_height * destination.height(),
                    );
                    target.blit(&image, placed, visible);
                }
            }
        }
        for patch in &self.patches {
            let placed = mapped(patch.rect);
            if placed.intersects(visible) {
                target.blit(&patch.image, placed, visible);
            }
        }
    }

    /// [`RasterSnapshot::compose`] into a color target.
    pub fn compose_rgba(&self, target: &mut Rgba8Image, rect: Rect) {
        self.compose(&mut RasterTarget::Rgba(target), rect);
    }

    /// [`RasterSnapshot::compose`] into a gray target.
    pub fn compose_gray(&self, target: &mut Gray8Image, rect: Rect) {
        self.compose(&mut RasterTarget::Gray(target), rect);
    }

    /// The snapshot's pixels in one contiguous raster, of the snapshot's own kind — color for a
    /// layer, coverage for a mask (`makeImage()`). Materialized once and cached.
    pub fn materialize(&self) -> PatchImage {
        let mut materialized = self.materialized.lock();
        if let Some(image) = materialized.as_ref() {
            return image.clone();
        }
        let image = if self.is_mask {
            let mut target = Gray8Image::new(self.width, self.height);
            self.compose_gray(&mut target, self.bounds());
            PatchImage::Gray(Arc::new(target))
        } else {
            let mut target = Rgba8Image::new(self.width, self.height);
            self.compose_rgba(&mut target, self.bounds());
            PatchImage::Rgba(Arc::new(target))
        };
        *materialized = Some(image.clone());
        image
    }

    /// Whether the contiguous backing has been built yet (`hasMaterializedPixels`).
    pub fn has_materialized_pixels(&self) -> bool {
        self.materialized.lock().is_some()
    }

    /// A thumbnail of the snapshot's own kind, no larger than `max_side` on its longest side
    /// (`thumbnail()`, 96 by default).
    pub fn thumbnail(&self, max_side: f64) -> PatchImage {
        let factor = 1.0f64.min(max_side / self.width.max(self.height) as f64);
        let width = 1.max((self.width as f64 * factor) as usize);
        let height = 1.max((self.height as f64 * factor) as usize);
        let rect = Rect::new(0.0, 0.0, width as f64, height as f64);
        if self.is_mask {
            let mut target = Gray8Image::new(width, height);
            self.compose_gray(&mut target, rect);
            PatchImage::Gray(Arc::new(target))
        } else {
            let mut target = Rgba8Image::new(width, height);
            self.compose_rgba(&mut target, rect);
            PatchImage::Rgba(Arc::new(target))
        }
    }
}

/// The 256-pixel cells a rectangle touches (`RasterSnapshot.cells`).
fn cells(rect: Rect) -> Vec<Cell> {
    if rect.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let first_x = (rect.min_x() / TILE).floor() as i32;
    let last_x = (rect.max_x() / TILE).ceil() as i32 - 1;
    let first_y = (rect.min_y() / TILE).floor() as i32;
    let last_y = (rect.max_y() / TILE).ceil() as i32 - 1;
    for y in first_y..=last_y {
        for x in first_x..=last_x {
            result.push([x, y]);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_patch(rect: Rect, value: u8) -> BrushPatch {
        BrushPatch::new(
            rect,
            PatchImage::Gray(Arc::new(Gray8Image::uniform(rect.width() as usize, rect.height() as usize, value))),
        )
    }

    fn color_patch(rect: Rect, pixel: [u8; 4]) -> BrushPatch {
        BrushPatch::new(
            rect,
            PatchImage::Rgba(Arc::new(Rgba8Image::opaque(rect.width() as usize, rect.height() as usize, pixel))),
        )
    }

    /// The Swift identity compare `lhs.image === rhs.image`.
    fn same_rgba(left: &PatchImage, right: &PatchImage) -> bool {
        match (left, right) {
            (PatchImage::Rgba(left), PatchImage::Rgba(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    fn same_gray(left: &PatchImage, right: &PatchImage) -> bool {
        match (left, right) {
            (PatchImage::Gray(left), PatchImage::Gray(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    fn snapshot(width: usize, height: usize, base: Option<PatchImage>, base_rect: Rect, patches: Vec<BrushPatch>, is_mask: bool) -> RasterSnapshot {
        RasterSnapshot::new(width, height, base, base_rect, patches, is_mask, None)
    }

    #[test]
    fn new_defaults_fill_and_alignment() {
        let raster = snapshot(10, 20, None, Rect::new(3.0, 4.0, 10.0, 20.0), Vec::new(), false);
        assert_eq!(raster.fill, 1.0);
        assert_eq!(raster.alignment, Point::new(3.0, 4.0));
        assert_eq!(raster.bytes_per_pixel(), RGBA_PIXEL);
        assert_eq!(snapshot(1, 1, None, Rect::ZERO, Vec::new(), true).bytes_per_pixel(), GRAY_PIXEL);
    }

    #[test]
    fn replacing_splits_an_overlapped_patch_into_disjoint_tiles() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::opaque(256, 256, [10, 20, 30, 255])));
        let old = snapshot(
            512,
            512,
            None,
            Rect::new(0.0, 0.0, 512.0, 512.0),
            vec![BrushPatch::new(Rect::new(0.0, 0.0, 256.0, 256.0), image)],
            false,
        );
        let source = ImportedImage::with_raster(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(512, 512))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
            "Layer",
            Some(old),
        );
        let replacement = [color_patch(Rect::new(128.0, 128.0, 128.0, 128.0), [200, 0, 0, 255])];
        let result = RasterSnapshot::replacing(
            Some(&source),
            Rect::new(0.0, 0.0, 512.0, 512.0),
            &replacement,
            Rect::new(0.0, 0.0, 512.0, 512.0),
            false,
            1.0,
        );
        assert_eq!((result.width, result.height), (512, 512));
        assert!(!result.is_mask);
        // The old tile splits around the replacement: the two bands above and to its left survive;
        // the zero-height/width slices are dropped.
        let rects: Vec<Rect> = result.patches.iter().map(|patch| patch.rect).collect();
        assert_eq!(
            rects,
            vec![
                Rect::new(0.0, 0.0, 256.0, 128.0),
                Rect::new(0.0, 128.0, 128.0, 128.0),
                Rect::new(128.0, 128.0, 128.0, 128.0),
            ]
        );
        for patch in &result.patches {
            let image = patch.image.as_rgba().expect("color patch");
            assert_eq!((image.width() as f64, image.height() as f64), (patch.rect.width(), patch.rect.height()));
        }
        // The surviving pieces are disjoint from the replacement.
        for patch in &result.patches[..2] {
            assert!(patch.rect.intersection(rects[2]).is_empty());
        }
    }

    #[test]
    fn replacing_leaves_patches_in_distant_cells_shared() {
        let far = color_patch(Rect::new(10.0, 10.0, 100.0, 100.0), [1, 2, 3, 255]);
        let near = color_patch(Rect::new(1300.0, 1300.0, 100.0, 100.0), [4, 5, 6, 255]);
        let old = snapshot(2048, 2048, None, Rect::new(0.0, 0.0, 2048.0, 2048.0), vec![far.clone(), near], false);
        let source = ImportedImage::with_raster(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(2048, 2048))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
            "Layer",
            Some(old),
        );
        let replacement = [color_patch(Rect::new(1310.0, 1310.0, 40.0, 40.0), [9, 9, 9, 255])];
        let result = RasterSnapshot::replacing(
            Some(&source),
            Rect::new(0.0, 0.0, 2048.0, 2048.0),
            &replacement,
            Rect::new(0.0, 0.0, 2048.0, 2048.0),
            false,
            1.0,
        );
        let kept_far = result.patches.iter().find(|patch| patch.rect == far.rect).expect("untouched tile");
        assert!(same_rgba(&kept_far.image, &far.image));
        // The overlapping tile was split; the replacement comes last and wins.
        assert!(result.patches.len() > 2);
        let added = result.patches.last().expect("a patch");
        assert!(same_rgba(&added.image, &replacement[0].image));
    }

    #[test]
    fn replacing_crops_patches_and_base_to_the_new_bounds() {
        let base = PixelImage::Rgba(Arc::new(Rgba8Image::opaque(100, 100, [7, 7, 7, 255])));
        let old = snapshot(
            100,
            100,
            Some(base.clone()),
            // The base rect is local to the snapshot's own grid, like `CGRect(origin: .zero, size:)`.
            Rect::new(0.0, 0.0, 100.0, 100.0),
            vec![color_patch(Rect::new(0.0, 0.0, 100.0, 100.0), [0, 0, 0, 255])],
            false,
        );
        let source = ImportedImage::with_raster(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(100, 100))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
            "Layer",
            Some(old),
        );
        // Crop 60 wide and 40 tall off the old 100 × 100 raster, shifted by (10, 20).
        let result = RasterSnapshot::replacing(
            Some(&source),
            Rect::new(100.0, 100.0, 100.0, 100.0),
            &[],
            Rect::new(110.0, 120.0, 60.0, 40.0),
            false,
            1.0,
        );
        assert_eq!((result.width, result.height), (60, 40));
        assert_eq!(result.patches.len(), 1);
        // The tile is clamped into the new grid: the part left of and above the crop is dropped.
        assert_eq!(result.patches[0].rect, Rect::new(0.0, 0.0, 60.0, 40.0));
        assert_eq!(
            (result.patches[0].image.as_rgba().unwrap().width(), result.patches[0].image.as_rgba().unwrap().height()),
            (60, 40)
        );
        // The base is carried across and shifted into the new grid.
        let carried = result.base.as_ref().expect("base");
        assert!(same_rgba(carried, &base));
        assert_eq!(result.base_rect, Rect::new(-10.0, -20.0, 100.0, 100.0));
        assert_eq!(result.alignment, Point::new(-10.0, -20.0));
    }

    #[test]
    fn replacing_adopts_the_source_image_as_base_when_there_was_no_raster() {
        let source = ImportedImage::new(
            PixelImage::Rgba(Arc::new(Rgba8Image::opaque(4, 4, [1, 2, 3, 255]))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
            "Imported",
        );
        let result = RasterSnapshot::replacing(
            Some(&source),
            Rect::new(5.0, 6.0, 4.0, 4.0),
            &[gray_patch(Rect::new(5.0, 6.0, 4.0, 4.0), 128)],
            Rect::new(5.0, 6.0, 10.0, 10.0),
            true,
            0.0,
        );
        assert_eq!((result.width, result.height), (10, 10));
        assert!(result.is_mask);
        assert_eq!(result.fill, 0.0);
        assert_eq!(result.base_rect, Rect::new(0.0, 0.0, 4.0, 4.0));
        // Nothing was carried over, so the alignment is the source rect in the new grid.
        assert_eq!(result.alignment, Point::new(0.0, 0.0));
        assert_eq!(result.patches.len(), 1);
        assert_eq!(result.patches[0].rect, Rect::new(0.0, 0.0, 4.0, 4.0));
        assert_eq!(result.patches[0].image.as_gray().unwrap().get(0, 0), 128);
        let base = result.base.as_ref().expect("the source image becomes the base");
        assert_eq!(base.width(), 4);
    }

    #[test]
    fn thumbnail_halves_to_96_in_the_longest_side() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::opaque(512, 256, [3, 4, 5, 255])));
        let raster = snapshot(512, 256, Some(image), Rect::new(0.0, 0.0, 512.0, 256.0), Vec::new(), false);
        let thumbnail = raster.thumbnail(THUMBNAIL_MAX_SIDE);
        assert_eq!((thumbnail.width(), thumbnail.height()), (96, 48));
        assert_eq!(thumbnail.as_rgba().unwrap().get(0, 0), [3, 4, 5, 255]);

        // Smaller than the cap: kept at its own size.
        let small = snapshot(10, 4, None, Rect::new(0.0, 0.0, 10.0, 4.0), Vec::new(), false);
        let thumbnail = small.thumbnail(THUMBNAIL_MAX_SIDE);
        assert_eq!((thumbnail.width(), thumbnail.height()), (10, 4));

        // Never smaller than one pixel.
        let empty = snapshot(0, 0, None, Rect::new(0.0, 0.0, 0.0, 0.0), Vec::new(), false);
        let thumbnail = empty.thumbnail(THUMBNAIL_MAX_SIDE);
        assert_eq!((thumbnail.width(), thumbnail.height()), (1, 1));
    }

    #[test]
    fn thumbnail_of_a_mask_is_gray() {
        let raster = snapshot(
            200,
            100,
            None,
            Rect::new(0.0, 0.0, 200.0, 100.0),
            vec![gray_patch(Rect::new(0.0, 0.0, 200.0, 100.0), 255)],
            true,
        );
        let thumbnail = raster.thumbnail(THUMBNAIL_MAX_SIDE);
        assert!(thumbnail.is_mask());
        assert_eq!((thumbnail.width(), thumbnail.height()), (96, 48));
        assert_eq!(thumbnail.as_gray().unwrap().get(0, 0), 255);
    }

    #[test]
    fn materialize_caches_the_raster_in_its_own_kind() {
        let raster = snapshot(
            4,
            2,
            None,
            Rect::new(0.0, 0.0, 4.0, 2.0),
            vec![gray_patch(Rect::new(0.0, 0.0, 4.0, 2.0), 64)],
            true,
        );
        assert!(!raster.has_materialized_pixels());
        let image = raster.materialize();
        assert!(raster.has_materialized_pixels());
        assert_eq!(image.as_gray().expect("mask materializes gray").get(3, 1), 64);
        assert!(same_gray(&image, &raster.materialize()));
    }

    #[test]
    fn compose_grows_a_mask_with_its_fill_background() {
        let raster = snapshot(
            4,
            4,
            None,
            Rect::new(0.0, 0.0, 2.0, 2.0),
            vec![gray_patch(Rect::new(0.0, 0.0, 2.0, 2.0), 0)],
            true,
        );
        let mut target = Gray8Image::new(4, 4);
        raster.compose_gray(&mut target, raster.bounds());
        // The patch covers the top-left 2 × 2 in black; the mask's fill reveals everything else.
        assert_eq!(target.get(0, 0), 0);
        assert_eq!(target.get(3, 3), 255);
    }

    #[test]
    fn compose_replaces_destination_pixels_with_transparent_ones() {
        let pixel = [10, 20, 30, 255];
        let raster = snapshot(
            2,
            1,
            Some(PixelImage::Rgba(Arc::new(Rgba8Image::opaque(2, 1, pixel)))),
            Rect::new(0.0, 0.0, 2.0, 1.0),
            vec![BrushPatch::new(
                Rect::new(1.0, 0.0, 1.0, 1.0),
                PixelImage::Rgba(Arc::new(Rgba8Image::from_data(1, 1, vec![0, 0, 0, 0]))),
            )],
            false,
        );
        let mut target = Rgba8Image::opaque(2, 1, [255, 255, 255, 255]);
        raster.compose_rgba(&mut target, raster.bounds());
        assert_eq!(target.get(0, 0), pixel);
        assert_eq!(target.get(1, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn compose_blits_the_base_at_its_rect_and_scales_by_nearest() {
        let base = PixelImage::Rgba(Arc::new(Rgba8Image::from_data(2, 1, vec![1, 0, 0, 255, 2, 0, 0, 255])));
        // The base sits at (1, 1) and the raster is 4 × 2, so the grid scales by 2.
        let raster = snapshot(4, 2, Some(base), Rect::new(0.5, 0.5, 1.0, 0.5), Vec::new(), false);
        let mut target = Rgba8Image::new(8, 4);
        raster.compose_rgba(&mut target, Rect::new(0.0, 0.0, 8.0, 4.0));
        // The base's two pixels land at destination (1, 1) and (2, 1); pixel (3, 1) is past the base.
        assert_eq!(target.get(1, 1), [1, 0, 0, 255]);
        assert_eq!(target.get(2, 1), [2, 0, 0, 255]);
        assert_eq!(target.get(3, 1), [0, 0, 0, 0]);
        assert_eq!(target.get(0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn cells_cover_every_touched_tile() {
        assert_eq!(cells(Rect::new(0.0, 0.0, 256.0, 256.0)), vec![[0, 0]]);
        assert_eq!(cells(Rect::new(0.0, 0.0, 257.0, 1.0)), vec![[0, 0], [1, 0]]);
        assert_eq!(cells(Rect::new(255.0, 255.0, 2.0, 2.0)), vec![[0, 0], [1, 0], [0, 1], [1, 1]]);
        assert!(cells(Rect::new(0.0, 0.0, 0.0, 10.0)).is_empty());
    }
}
