//! The immutable pixels a layer or a mask carries (`ImportedImage`) and the import failures
//! (`ImageImportError`).
//!
//! Swift's `ImportedImage` holds `CGImage`s: premultiplied sRGB RGBA8 for an image layer, 8-bit
//! device gray for a layer mask. `PixelImage` keeps that distinction in the port, exactly as the
//! `CGImage` color space and `bitsPerPixel` did.

use crate::buffer::{Gray8Image, Rgba8Image, SharedGray, SharedImage};
use crate::geom::Rect;
use crate::limits;
use crate::raster::RasterSnapshot;

/// Either an RGBA raster (image layers, exports) or an 8-bit gray raster (layer masks).
///
/// The variants carry the shared, immutable rasters the document, history and renderer pass around;
/// masks are gray, so an RGBA-only type would silently misrepresent them.
#[derive(Clone, Debug)]
pub enum PixelImage {
    Rgba(SharedImage),
    Gray(SharedGray),
}

impl PixelImage {
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

    /// True for the 8-bit gray rasters a layer mask carries.
    pub fn is_mask(&self) -> bool {
        matches!(self, Self::Gray(_))
    }

    pub fn pixel_count(&self) -> usize {
        match self {
            Self::Rgba(image) => image.pixel_count(),
            Self::Gray(image) => image.pixel_count(),
        }
    }

    pub fn as_rgba(&self) -> Option<&Rgba8Image> {
        match self {
            Self::Rgba(image) => Some(image),
            Self::Gray(_) => None,
        }
    }

    pub fn as_gray(&self) -> Option<&Gray8Image> {
        match self {
            Self::Rgba(_) => None,
            Self::Gray(image) => Some(image),
        }
    }

    /// A sub-image of the same kind, `None` for a rectangle that is outside or empty
    /// (`CGImage.cropping(to:)`).
    pub fn cropped(&self, rect: Rect) -> Option<Self> {
        match self {
            Self::Rgba(image) => image.cropped(rect).map(|image| Self::Rgba(std::sync::Arc::new(image))),
            Self::Gray(image) => image.cropped(rect).map(|image| Self::Gray(std::sync::Arc::new(image))),
        }
    }
}

impl From<SharedImage> for PixelImage {
    fn from(image: SharedImage) -> Self {
        Self::Rgba(image)
    }
}

impl From<SharedGray> for PixelImage {
    fn from(image: SharedGray) -> Self {
        Self::Gray(image)
    }
}

/// Swift `ImportedImage`: the immutable pixels an image layer, a mask or an export shows, a small
/// thumbnail for the panels, the asset's name, and — once it has been painted — its sparse raster.
///
/// `image` and `thumbnail` share immutable rasters, so a clone is only a handful of `Arc`s. The
/// Swift identity compares (`asset.image === rhs.asset.image`) are `Arc::ptr_eq`, which is why this
/// type deliberately has no `PartialEq` of its own.
#[derive(Clone, Debug)]
pub struct ImportedImage {
    pub image: PixelImage,
    pub thumbnail: PixelImage,
    pub name: String,
    pub raster: Option<RasterSnapshot>,
}

impl ImportedImage {
    /// `ImportedImage(image:thumbnail:name:)` — an asset that has not been painted yet.
    pub fn new(image: PixelImage, thumbnail: PixelImage, name: impl Into<String>) -> Self {
        Self { image, thumbnail, name: name.into(), raster: None }
    }

    /// `ImportedImage(image:thumbnail:name:raster:)` — an asset carrying its sparse raster.
    pub fn with_raster(image: PixelImage, thumbnail: PixelImage, name: impl Into<String>, raster: Option<RasterSnapshot>) -> Self {
        Self { image, thumbnail, name: name.into(), raster }
    }
}

/// Why an import did not produce an image (`ImageImportError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageImportError {
    Unreadable,
    Unsupported,
    TooLarge,
}

impl std::fmt::Display for ImageImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable => formatter.write_str("The image could not be read. It may be damaged or unavailable."),
            Self::Unsupported => formatter.write_str("Choose a JPEG, PNG, HEIC, TIFF, or Photoshop (PSD) file."),
            Self::TooLarge => write!(
                formatter,
                "This import exceeds the current {}-megapixel document budget or {}-pixel side limit.",
                limits::document_budget_megapixels(),
                grouped(limits::MAX_SIDE)
            ),
        }
    }
}

impl std::error::Error for ImageImportError {}

/// `Int.formatted()`: the same number with the en-US grouping the message spells out ("30,000").
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_import_error_messages_match_the_original() {
        assert_eq!(
            ImageImportError::Unreadable.to_string(),
            "The image could not be read. It may be damaged or unavailable."
        );
        assert_eq!(
            ImageImportError::Unsupported.to_string(),
            "Choose a JPEG, PNG, HEIC, TIFF, or Photoshop (PSD) file."
        );
        assert_eq!(
            ImageImportError::TooLarge.to_string(),
            format!(
                "This import exceeds the current {}-megapixel document budget or 30,000-pixel side limit.",
                limits::document_budget_megapixels()
            )
        );
    }

    #[test]
    fn grouped_uses_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(300), "300");
        assert_eq!(grouped(30_000), "30,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn pixel_image_reports_its_kind_and_size() {
        let rgba: SharedImage = std::sync::Arc::new(Rgba8Image::new(4, 3));
        let gray: SharedGray = std::sync::Arc::new(Gray8Image::new(2, 5));
        let rgba = PixelImage::Rgba(rgba);
        let gray = PixelImage::Gray(gray);
        assert_eq!((rgba.width(), rgba.height(), rgba.pixel_count()), (4, 3, 12));
        assert_eq!((gray.width(), gray.height(), gray.pixel_count()), (2, 5, 10));
        assert!(!rgba.is_mask() && gray.is_mask());
        assert!(rgba.as_rgba().is_some() && rgba.as_gray().is_none());
        assert!(gray.as_gray().is_some() && gray.as_rgba().is_none());
    }

    #[test]
    fn pixel_image_crops_within_bounds_only() {
        let image = PixelImage::Rgba(std::sync::Arc::new(Rgba8Image::new(8, 8)));
        let cropped = image.cropped(Rect::new(2.0, 3.0, 4.0, 2.0)).expect("inside");
        assert_eq!((cropped.width(), cropped.height()), (4, 2));
        assert!(cropped.as_rgba().is_some());
        assert!(image.cropped(Rect::new(6.0, 6.0, 4.0, 4.0)).is_none());
        assert!(image.cropped(Rect::new(0.0, 0.0, 0.0, 4.0)).is_none());
    }
}
