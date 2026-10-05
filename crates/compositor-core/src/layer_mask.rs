//! `LayerMask`: the immutable, normalized layer-local coverage a layer shows through. White reveals,
//! black hides, and intermediate gray is soft coverage.
//!
//! The rasterizing helpers the Swift declared on the type (`placed`, `drawSmooth`, `clipImage`) need the
//! drawing engine, which core cannot depend on; they live in `compositor-render` as free functions that
//! take the same values. Everything that is pure data, geometry or buffer arithmetic is here.

use crate::limits::MAX_SIDE;
use crate::buffer::{Gray8Image, SharedGray};
use crate::document::{ImageLayer, ProjectLayerRecord, ProjectSnapshot};
use crate::error::CoreError;
use crate::geom::{AffineTransform, CGFloat, Point};
use crate::imported_image::{ImportedImage, PixelImage};
use crate::layer_transform::LayerTransform;
use parking_lot::Mutex;
use std::sync::{Arc, LazyLock};

/// A layer's (or folder's) mask.
#[derive(Clone, Debug)]
pub struct LayerMask {
    pub asset: ImportedImage,
    pub is_enabled: bool,
    /// Where the mask sits on the document once it has been moved apart from its layer; `None` while it
    /// covers the layer's own pixel grid (and follows every change to it).
    pub placement: Option<LayerTransform>,
    /// Linked, layer and mask move together; unlinked, each transforms on its own, as in Photoshop.
    pub is_linked: bool,
}

impl PartialEq for LayerMask {
    /// Compared by pixel identity, exactly as the Swift's `===` on the backing `CGImage` compared.
    fn eq(&self, other: &Self) -> bool {
        pixels_identical(&self.asset.image, &other.asset.image)
            && self.is_enabled == other.is_enabled
            && self.placement == other.placement
            && self.is_linked == other.is_linked
    }
}

/// `Arc::ptr_eq` for either raster kind: the Swift compared backing images by identity.
fn pixels_identical(lhs: &PixelImage, rhs: &PixelImage) -> bool {
    match (lhs, rhs) {
        (PixelImage::Rgba(a), PixelImage::Rgba(b)) => Arc::ptr_eq(a, b),
        (PixelImage::Gray(a), PixelImage::Gray(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

impl Eq for LayerMask {}

impl LayerMask {
    pub fn new(asset: ImportedImage) -> Self {
        Self {
            asset,
            is_enabled: true,
            placement: None,
            is_linked: true,
        }
    }

    pub fn with_placement(asset: ImportedImage, is_enabled: bool, placement: Option<LayerTransform>, is_linked: bool) -> Self {
        Self {
            asset,
            is_enabled,
            placement,
            is_linked,
        }
    }

    /// The mask's pixels while enabled, `None` while disabled — a disabled mask stays embedded and
    /// editable but does not affect compositing.
    pub fn enabled_image(&self) -> Option<&PixelImage> {
        self.is_enabled.then_some(&self.asset.image)
    }

    /// The same mask with new pixels (in its own grid), still enabled or not, linked or not, and where it
    /// sits.
    pub fn replacing(&self, asset: ImportedImage) -> LayerMask {
        LayerMask {
            asset,
            is_enabled: self.is_enabled,
            placement: self.placement,
            is_linked: self.is_linked,
        }
    }

    /// A mask must be 8-bit gray without alpha, one channel a pixel.
    pub fn is_valid(image: &PixelImage) -> bool {
        matches!(image, PixelImage::Gray(_))
    }

    /// A uniform 1×1 mask, the cheapest way to add a reveal-all or hide-all mask before anything is
    /// painted into it.
    pub fn solid(revealing: bool) -> Option<LayerMask> {
        let value = if revealing { 255 } else { 0 };
        let image = Gray8Image::uniform(1, 1, value);
        let shared: SharedGray = Arc::new(image);
        let pixel = PixelImage::Gray(shared);
        Some(LayerMask::new(ImportedImage::new(pixel.clone(), pixel, "Layer Mask")))
    }

    /// `LayerMask.asset(from:)`: the mask's asset, with a thumbnail at most 96 px on the long side: white where the mask is
    /// non-zero, black where it is not — the result the Swift got by clipping to the mask and filling
    /// white.
    pub fn asset(image: PixelImage) -> Result<ImportedImage, CoreError> {
        if !Self::is_valid(&image) {
            return Err(CoreError::Message("a layer mask must be an 8-bit grayscale image".into()));
        }
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(CoreError::Message("a layer mask is larger than the document limits allow".into()));
        }
        let gray = image
            .as_gray()
            .ok_or_else(|| CoreError::Message("a layer mask must be an 8-bit grayscale image".into()))?;
        let factor = (96.0 / width.max(height) as CGFloat).min(1.0);
        let thumb_width = ((width as CGFloat * factor) as usize).max(1);
        let thumb_height = ((height as CGFloat * factor) as usize).max(1);
        let mut thumbnail = Gray8Image::new(thumb_width, thumb_height);
        for y in 0..thumb_height {
            for x in 0..thumb_width {
                let source_x = ((x as CGFloat + 0.5) / thumb_width as CGFloat * width as CGFloat) as usize;
                let source_y = ((y as CGFloat + 0.5) / thumb_height as CGFloat * height as CGFloat) as usize;
                let source_x = source_x.min(width - 1);
                let source_y = source_y.min(height - 1);
                let value = if gray.get(source_x, source_y) != 0 { 255 } else { 0 };
                thumbnail.set(x, y, value);
            }
        }
        Ok(ImportedImage::new(image, PixelImage::Gray(Arc::new(thumbnail)), "Layer Mask"))
    }

    /// Where the mask sits once its layer moves from `old` to `new`: carried along when linked (still
    /// covering the layer, or its own placement moved the same way); left where it was on the document
    /// when unlinked.
    pub fn placement_moving_layer(&self, old: &LayerTransform, new: &LayerTransform) -> Option<LayerTransform> {
        // A uniform mask looks the same wherever it sits.
        if self.asset.image.width() <= 1 && self.asset.image.height() <= 1 {
            return None;
        }
        let moved = if self.is_linked {
            self.placement.map(|placement| placement.following(*old, *new))
        } else {
            Some(self.placement.unwrap_or(*old))
        };
        moved.filter(|transform| !transform.same_placement(*new))
    }

    /// What a mask shows beyond its pixels once placed apart from its layer: white or black, whichever
    /// most of its edge is (read from the small thumbnail) — so a reveal-all mask keeps revealing and a
    /// hide-all mask keeps hiding.
    pub fn background(thumbnail: &Gray8Image) -> CGFloat {
        let (width, height) = (thumbnail.width(), thumbnail.height());
        if width == 0 || height == 0 {
            return 1.0;
        }
        let mut total = 0usize;
        let mut count = 0usize;
        for y in 0..height {
            for x in 0..width {
                if y == 0 || y == height - 1 || x == 0 || x == width - 1 {
                    total += thumbnail.get(x, y) as usize;
                    count += 1;
                }
            }
        }
        if count == 0 {
            return 1.0;
        }
        if total * 2 >= count * 255 {
            1.0
        } else {
            0.0
        }
    }

    /// The transform that places a `mask_width` × `mask_height` mask grid on the document according to
    /// `placement`, expressed in the coordinate system of a `width` × `height` grid stretched over
    /// `layer`. This is the map `placed(width:height:layer:placement:maskWidth:maskHeight:background:compose:)`
    /// concatenated before drawing; the drawing itself is `compositor-render`'s.
    pub fn placement_in_layer(
        layer: &LayerTransform,
        placement: &LayerTransform,
        width: usize,
        height: usize,
        mask_width: usize,
        mask_height: usize,
    ) -> AffineTransform {
        let placement_map = crate::layer_transform::pixel_to_document(placement, mask_width, mask_height);
        let layer_map = crate::layer_transform::pixel_to_document(layer, width, height);
        placement_map.concatenating(layer_map.inverted())
    }
}

/// Masks resampled into their layers' grids (`LayerMask::clip_image`), so redraws reuse them; the least
/// recently used go beyond a few entries or a pixel budget. Shared across the app, as in the Swift.
pub struct MaskPlacementCache {
    entries: Mutex<Vec<Entry>>,
    clock: Mutex<u64>,
}

struct Entry {
    mask: SharedGray,
    placement: LayerTransform,
    layer: LayerTransform,
    width: usize,
    height: usize,
    image: SharedGray,
    last_use: u64,
}

/// The pixel ceiling the cache keeps before evicting, and the entry ceiling beside it.
const MASK_CACHE_PIXELS: usize = 64_000_000;
const MASK_CACHE_ENTRIES: usize = 8;

impl MaskPlacementCache {
    pub fn shared() -> &'static MaskPlacementCache {
        static SHARED: LazyLock<MaskPlacementCache> = LazyLock::new(|| MaskPlacementCache {
            entries: Mutex::new(Vec::new()),
            clock: Mutex::new(0),
        });
        &SHARED
    }

    /// The cached image for this exact mask, placement, layer and size, or `build`'s result stored for
    /// next time. `None` from `build` is not cached.
    pub fn image(
        &self,
        mask: &SharedGray,
        placement: &LayerTransform,
        layer: &LayerTransform,
        width: usize,
        height: usize,
        build: impl FnOnce() -> Option<SharedGray>,
    ) -> Option<SharedGray> {
        let stamp = {
            let mut clock = self.clock.lock();
            *clock += 1;
            *clock
        };
        {
            let mut entries = self.entries.lock();
            if let Some(index) = entries.iter().position(|entry| {
                Arc::ptr_eq(&entry.mask, mask)
                    && entry.placement == *placement
                    && entry.layer == *layer
                    && entry.width == width
                    && entry.height == height
            }) {
                entries[index].last_use = stamp;
                return Some(entries[index].image.clone());
            }
        }
        let image = build()?;
        if width * height > MASK_CACHE_PIXELS {
            return Some(image);
        }
        let mut entries = self.entries.lock();
        entries.push(Entry {
            mask: mask.clone(),
            placement: *placement,
            layer: *layer,
            width,
            height,
            image: image.clone(),
            last_use: stamp,
        });
        while entries.len() > MASK_CACHE_ENTRIES
            || entries.iter().map(|entry| entry.width * entry.height).sum::<usize>() > MASK_CACHE_PIXELS
        {
            let Some(oldest) = entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_use)
                .map(|(index, _)| index)
            else {
                break;
            };
            entries.remove(oldest);
        }
        Some(image)
    }
}

/// A folder's mask, clipping the layers inside the folder. Folders are pass-through — the layers inside
/// are drawn straight onto what is below, never composited as a unit — so a folder mask applies to each
/// of those layers, multiplied with the layer's own mask and the masks of any folders further out.
#[derive(Clone, Debug)]
pub struct FolderMaskClip {
    pub image: SharedGray,
    pub transform: LayerTransform,
}

impl FolderMaskClip {
    /// The placement transform a caller `concatenate`s before clipping to the mask, and its inverse
    /// afterwards — the two halves of the Swift's `apply(scale:center:in:)`. The clipping itself is
    /// `Canvas::clip_to_image` in `compositor-pixels`, driven by `compositor-render`.
    pub fn placement(&self, scale: CGFloat, center: Point) -> (AffineTransform, AffineTransform) {
        let placement = AffineTransform::translation(center.x, center.y)
            .rotated_by(self.transform.radians())
            .scaled_by(if self.transform.flip_x { -1.0 } else { 1.0 }, if self.transform.flip_y { -1.0 } else { 1.0 });
        (placement, placement.inverted())
    }

    /// The mask's rectangle in the placement's own space, `scale` applied.
    pub fn rect(&self, scale: CGFloat) -> crate::geom::Rect {
        let width = self.transform.size.width * scale;
        let height = self.transform.size.height * scale;
        crate::geom::Rect::new(-width / 2.0, -height / 2.0, width, height)
    }
}

/// The canvas's last preview of an unlinked mask being distorted on its own.
#[derive(Clone, Debug)]
pub struct MaskDistortPreviewCache {
    pub corners: Vec<Point>,
    pub draft: LayerTransform,
    pub mask: SharedGray,
    pub layer: LayerTransform,
    pub result: Option<SharedGray>,
}

impl ProjectSnapshot {
    /// The mask a manifest layer record describes, if it has one and its asset came in.
    pub fn mask(&self, layer: &ProjectLayerRecord) -> Option<LayerMask> {
        let _ = layer.mask_file.as_ref()?;
        let asset = self.masks.get(&layer.id)?;
        Some(LayerMask {
            asset: asset.clone(),
            is_enabled: layer.mask_enabled.unwrap_or(true),
            placement: layer.mask_placement,
            is_linked: layer.mask_linked.unwrap_or(true),
        })
    }
}

impl ImageLayer {
    /// Where the mask's pixels sit on the document: its own placement, else the layer's.
    pub fn mask_transform(&self) -> LayerTransform {
        self.mask.as_ref().and_then(|mask| mask.placement).unwrap_or(self.transform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Rgba8Image;
    use crate::geom::{Point, Size};

    fn gray(width: usize, height: usize, value: u8) -> ImportedImage {
        let image = Gray8Image::uniform(width, height, value);
        ImportedImage::new(PixelImage::Gray(Arc::new(image)), PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, value))), "Layer Mask")
    }

    #[test]
    fn a_mask_must_be_gray_and_eight_bit() {
        assert!(LayerMask::is_valid(&PixelImage::Gray(Arc::new(Gray8Image::new(4, 4)))));
        assert!(!LayerMask::is_valid(&PixelImage::Rgba(Arc::new(Rgba8Image::new(4, 4)))));
    }

    #[test]
    fn solid_masks_are_one_pixel() {
        let revealing = LayerMask::solid(true).unwrap();
        assert_eq!(revealing.asset.image.width(), 1);
        assert_eq!(revealing.asset.image.as_gray().unwrap().get(0, 0), 255);
        assert_eq!(LayerMask::solid(false).unwrap().asset.image.as_gray().unwrap().get(0, 0), 0);
    }

    #[test]
    fn a_uniform_mask_looks_the_same_wherever_it_sits() {
        let mask = LayerMask::new(gray(1, 1, 255));
        let old = LayerTransform::default();
        let mut new = old;
        new.origin = Point::new(20.0, 30.0);
        assert_eq!(mask.placement_moving_layer(&old, &new), None);
    }

    #[test]
    fn an_unlinked_mask_stays_where_it_was() {
        let mut mask = LayerMask::new(gray(8, 8, 255));
        mask.is_linked = false;
        let old = LayerTransform {
            origin: Point::new(10.0, 10.0),
            size: Size::new(8.0, 8.0),
            ..Default::default()
        };
        let mut new = old;
        new.origin = Point::new(40.0, 10.0);
        assert_eq!(mask.placement_moving_layer(&old, &new), Some(old));
    }

    #[test]
    fn background_follows_the_edge_majority() {
        let white = Gray8Image::uniform(8, 8, 255);
        let black = Gray8Image::uniform(8, 8, 0);
        assert_eq!(LayerMask::background(&white), 1.0);
        assert_eq!(LayerMask::background(&black), 0.0);
        let mut mostly_white = Gray8Image::uniform(8, 8, 0);
        for x in 0..8 {
            mostly_white.set(x, 0, 255);
            mostly_white.set(x, 7, 255);
        }
        for y in 0..8 {
            mostly_white.set(0, y, 255);
            mostly_white.set(7, y, 255);
        }
        assert_eq!(LayerMask::background(&mostly_white), 1.0);
    }
}
