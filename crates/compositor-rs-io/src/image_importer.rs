//! Image import (`ImageImporter`): JPEG, PNG, TIFF, BMP and GIF decoded into the canonical
//! premultiplied sRGB raster, SVG rasterized at import time, and the merged composite of a
//! flattened Photoshop file.
//!
//! Substitutions against the original:
//!
//! * `CGImageSource`/`ImageIO` became the `image` crate. It sniffs the file's own type (the
//!   extension is never trusted, exactly as `CGImageSourceGetType` behaved), refuses anything
//!   outside the formats the editor accepts *before* touching the pixels, and the EXIF orientation
//!   is applied the way `CIImage.oriented(forExifOrientation:)` did.
//! * `NSImage`'s SVG rendering became `resvg`/`usvg`.
//! * **HEIC.** No supported Rust decoder reads HEIC/HEIF, so a HEIC file is rejected as soon as the
//!   ISO-BMFF brand identifies it, with the unchanged `ImageImportError::unsupported` message
//!   ("Choose a JPEG, PNG, HEIC, TIFF, or Photoshop (PSD) file."). A HEIC import needs a platform
//!   decoder (Windows: `Windows.Graphics.Imaging`/WIC); none is linked in this workspace. Every
//!   other behavior of the import path is preserved.
//! * The flattened-Photoshop branch reads the merged composite section of the PSD/PSB directly.
//!   It supports what the editor's own Photoshop reader supports — 8-bit gray, RGB and CMYK with
//!   raw or PackBits (RLE) image data — because ImageIO accepted the same files' composites; ZIP
//!   image data and 16/32-bit files are reported as unreadable.

use compositor_rs_core::imported_image::{ImageImportError, ImportedImage, PixelImage};
use compositor_rs_core::{limits, Rect, Rgba8Image, Size};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::path::Path;
use std::sync::Arc;
use usvg::Tree;

use crate::psd::builder::PSDDocumentBuilder;
use crate::psd::channel_coder;
use crate::psd::reader::PSDReader;
use crate::psd::types::{PSDDocument, PSDReadError};

/// `ImageImporter`.
pub enum ImageImporter {}

impl ImageImporter {
    /// An SVG drawn once into pixels, by `resvg` in place of macOS's own SVG renderer: fitted to
    /// `fitting` (the canvas) when there is one, otherwise at the size the file declares. It comes
    /// in as an ordinary image layer, so it doesn't stay vector.
    pub fn decode_svg(url: &Path, fitting: Option<Size>, remaining_pixels: usize) -> Result<ImportedImage, ImageImportError> {
        let data = std::fs::read(url).map_err(|_| ImageImportError::Unreadable)?;
        let options = usvg::Options::default();
        let tree = Tree::from_data(&data, &options).map_err(|_| ImageImportError::Unreadable)?;
        let declared = tree.size();
        if !(declared.width() > 0.0) || !(declared.height() > 0.0) {
            return Err(ImageImportError::Unreadable);
        }
        let scale = match fitting {
            Some(fitting) => (fitting.width / declared.width() as f64).min(fitting.height / declared.height() as f64),
            None => 1.0,
        };
        let width = ((declared.width() as f64 * scale).round() as usize).max(1);
        let height = ((declared.height() as f64 * scale).round() as usize).max(1);
        check_size(width, height, remaining_pixels)?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width as u32, height as u32).ok_or(ImageImportError::Unreadable)?;
        let transform = resvg::tiny_skia::Transform::from_scale(
            width as f32 / declared.width(),
            height as f32 / declared.height(),
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia's pixels are premultiplied sRGB RGBA8, rows top-down: already the canonical layout.
        let image = Rgba8Image::from_data(width, height, pixmap.data().to_vec());
        Ok(asset(image, url))
    }

    /// `flattenedPhotoshop`: a PSD or PSB with no layer records (only a background), read as its
    /// merged image.
    pub fn decode(url: &Path, remaining_pixels: usize, flattened_photoshop: bool) -> Result<ImportedImage, ImageImportError> {
        let header = read_header(url)?;
        match sniff(&header) {
            Kind::Png | Kind::Jpeg | Kind::Tiff | Kind::Bmp | Kind::Gif => decode_raster(url, remaining_pixels),
            Kind::Photoshop if flattened_photoshop => {
                let image = merged_photoshop(url, remaining_pixels)?;
                Ok(asset(image, url))
            }
            // HEIC is a real, documented counterpart of the original's `.heic` acceptance: the
            // unchanged `unsupported` message, because no Rust decoder in this workspace reads it.
            Kind::Heic | Kind::Photoshop | Kind::Svg | Kind::Unknown => Err(ImageImportError::Unsupported),
        }
    }

    /// `loadPhotoshop(_:remainingPixels:)`.
    pub fn load_photoshop(url: &Path, remaining_pixels: usize) -> Result<PSDDocument, PSDReadError> {
        PSDReader::read(url, remaining_pixels)
    }

    /// `photoshopAssets(_:)`.
    pub fn photoshop_assets(document: &PSDDocument) -> rustc_hash::FxHashMap<uuid::Uuid, ImportedImage> {
        PSDDocumentBuilder::assets(document)
    }
}

/// `ImportedImage(image:thumbnail:name:)` for a freshly decoded raster.
fn asset(image: Rgba8Image, url: &Path) -> ImportedImage {
    let thumbnail = thumbnail(&image);
    ImportedImage::new(
        PixelImage::Rgba(Arc::new(image)),
        PixelImage::Rgba(Arc::new(thumbnail)),
        name_of(url),
    )
}

/// `PixelAdjust.thumbnail(of:)`: at most 96 px on the long side, sampled without interpolation
/// (`BrushRaster.draw` set `interpolationQuality = .none`), `Int(...)` truncating the scaled size.
fn thumbnail(image: &Rgba8Image) -> Rgba8Image {
    let factor = (96.0 / image.width().max(image.height()) as f64).min(1.0);
    let width = ((image.width() as f64 * factor) as usize).max(1);
    let height = ((image.height() as f64 * factor) as usize).max(1);
    let mut canvas = Canvas::new_rgba(width, height);
    canvas.set_interpolation_quality(InterpolationQuality::None);
    canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
    canvas.into_rgba()
}

/// `url.deletingPathExtension().lastPathComponent`.
fn name_of(url: &Path) -> String {
    url.file_stem().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// The two ceilings every import is held to (`ImageImportError.tooLarge`).
fn check_size(width: usize, height: usize, remaining_pixels: usize) -> Result<(), ImageImportError> {
    if width > limits::MAX_SIDE || height > limits::MAX_SIDE || width.saturating_mul(height) > remaining_pixels {
        return Err(ImageImportError::TooLarge);
    }
    Ok(())
}

/// The file's first bytes, enough for every signature the importer recognizes.
fn read_header(url: &Path) -> Result<[u8; 32], ImageImportError> {
    use std::io::Read;
    let mut file = std::fs::File::open(url).map_err(|_| ImageImportError::Unreadable)?;
    let mut header = [0u8; 32];
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ImageImportError::Unreadable),
        }
    }
    Ok(header)
}

/// The container types the importer recognizes, by content rather than by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Png,
    Jpeg,
    Tiff,
    Bmp,
    Gif,
    Photoshop,
    Heic,
    Svg,
    Unknown,
}

/// `CGImageSourceGetType`: what the file actually holds.
fn sniff(header: &[u8]) -> Kind {
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Kind::Png;
    }
    if header.starts_with(b"\xff\xd8\xff") {
        return Kind::Jpeg;
    }
    if header.starts_with(b"II*\0") || header.starts_with(b"MM\0*") {
        return Kind::Tiff;
    }
    if header.starts_with(b"BM") {
        return Kind::Bmp;
    }
    if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return Kind::Gif;
    }
    if header.starts_with(b"8BPS") {
        return Kind::Photoshop;
    }
    if is_heic(header) {
        return Kind::Heic;
    }
    if is_svg(header) {
        return Kind::Svg;
    }
    Kind::Unknown
}

/// HEIC/HEIF are ISO-BMFF files whose `ftyp` brand says so. Canon CR3 is BMFF too, but its brand is
/// `crx `, which is camera RAW and never reaches this importer.
fn is_heic(header: &[u8]) -> bool {
    if header.len() < 12 || &header[4..8] != b"ftyp" {
        return false;
    }
    let brand = &header[8..12];
    matches!(
        brand,
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1" | b"msf1"
    )
}

/// An SVG's own signature; `decode` never takes this branch (the caller routes `.svg` to
/// [`ImageImporter::decode_svg`]), but the sniff stays complete.
fn is_svg(header: &[u8]) -> bool {
    let text = header.strip_prefix(b"\xef\xbb\xbf").unwrap_or(header);
    let start = text.iter().position(|byte| !byte.is_ascii_whitespace());
    let text = match start {
        Some(start) => &text[start..],
        None => return false,
    };
    text.starts_with(b"<svg") || text.starts_with(b"<?xml")
}

/// `ImageIO`'s raster path: read the pixel properties first (so an oversized file is refused before
/// the work), then decode through the `image` crate.
fn decode_raster(url: &Path, remaining_pixels: usize) -> Result<ImportedImage, ImageImportError> {
    let reader = ImageReader::open(url)
        .map_err(|_| ImageImportError::Unreadable)?
        .with_guessed_format()
        .map_err(|_| ImageImportError::Unreadable)?;
    let format = reader.format();
    if !matches!(
        format,
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Tiff | ImageFormat::Bmp | ImageFormat::Gif)
    ) {
        return Err(ImageImportError::Unsupported);
    }
    let (width, height) = reader.into_dimensions().map_err(|_| ImageImportError::Unreadable)?;
    check_size(width as usize, height as usize, remaining_pixels)?;
    let reader = ImageReader::open(url)
        .map_err(|_| ImageImportError::Unreadable)?
        .with_guessed_format()
        .map_err(|_| ImageImportError::Unreadable)?;
    let mut decoder = reader.into_decoder().map_err(|_| ImageImportError::Unreadable)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|_| ImageImportError::Unreadable)?;
    decoded.apply_orientation(orientation);
    let straight = decoded.into_rgba8();
    let (width, height) = (straight.width() as usize, straight.height() as usize);
    let image = premultiply(width, height, straight.into_raw());
    Ok(asset(image, url))
}

/// `CGBitmapInfo.premultipliedLast`: the canonical buffers carry premultiplied alpha, the decoders
/// hand over straight sRGB.
fn premultiply(width: usize, height: usize, straight: Vec<u8>) -> Rgba8Image {
    let mut data = straight;
    for pixel in data.chunks_exact_mut(compositor_rs_core::RGBA_PIXEL) {
        let alpha = pixel[3] as u32;
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * alpha + 127) / 255) as u8;
        }
    }
    Rgba8Image::from_data(width, height, data)
}

// MARK: - Flattened Photoshop

/// The merged composite of a PSD/PSB: the header, then the image-data section past the (possibly
/// empty) layer records.
///
/// The channel planes are unpacked by the same `PSDChannelCoder` the layered reader uses — the
/// composite's image-data section carries the identical raw and PackBits layouts, with each
/// channel's row-count table in front of the channel data instead of in front of each channel.
/// Photoshop's own writer emits 8-bit raw or RLE for a composite, which is what the editor's
/// Photoshop support accepts throughout; a ZIP-compressed image-data section or a 16/32-bit file is
/// reported as unreadable rather than guessed at.
fn merged_photoshop(url: &Path, remaining_pixels: usize) -> Result<Rgba8Image, ImageImportError> {
    let data = std::fs::read(url).map_err(|_| ImageImportError::Unreadable)?;
    let mut cursor = Cursor::new(&data);
    if cursor.take(4)? != b"8BPS" {
        return Err(ImageImportError::Unreadable);
    }
    let version = cursor.u16()?;
    if version != 1 && version != 2 {
        return Err(ImageImportError::Unreadable);
    }
    let large_document = version == 2;
    cursor.skip(6)?;
    let channels = cursor.u16()? as usize;
    let height = cursor.u32()? as usize;
    let width = cursor.u32()? as usize;
    let depth = cursor.u16()?;
    let color_mode = cursor.u16()?;
    if width == 0 || height == 0 {
        return Err(ImageImportError::Unreadable);
    }
    check_size(width, height, remaining_pixels)?;
    // The editor's Photoshop support is 8-bit; the merged composite follows it.
    if depth != 8 {
        return Err(ImageImportError::Unreadable);
    }
    let color_channels = match color_mode {
        1 => 1, // Grayscale
        3 => 3, // RGB
        4 => 4, // CMYK
        _ => return Err(ImageImportError::Unreadable),
    };
    if channels < color_channels || channels > color_channels + 1 {
        return Err(ImageImportError::Unreadable);
    }
    // Color mode data, image resources, layer and mask information.
    let color_mode_data = cursor.u32()? as usize;
    cursor.skip(color_mode_data)?;
    let image_resources = cursor.u32()? as usize;
    cursor.skip(image_resources)?;
    let layers = if large_document { cursor.u64()? as usize } else { cursor.u32()? as usize };
    cursor.skip(layers)?;
    let compression = cursor.u16()?;
    let plane_len = width * height;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(channels);
    match compression {
        0 => {
            for _ in 0..channels {
                planes.push(cursor.take(plane_len)?.to_vec());
            }
        }
        1 => {
            let entry = if large_document { 4 } else { 2 };
            let table = cursor.take(channels * height * entry)?.to_vec();
            let mut lengths = Vec::with_capacity(channels);
            for channel in 0..channels {
                let counts = &table[channel * height * entry..(channel + 1) * height * entry];
                let mut total = 0usize;
                for row in 0..height {
                    let start = row * entry;
                    let value = if entry == 4 {
                        u32::from_be_bytes([counts[start], counts[start + 1], counts[start + 2], counts[start + 3]]) as usize
                    } else {
                        u16::from_be_bytes([counts[start], counts[start + 1]]) as usize
                    };
                    total = total.checked_add(value).ok_or(ImageImportError::Unreadable)?;
                }
                lengths.push(total);
            }
            for channel in 0..channels {
                // The decoder reads one channel's counts and its PackBits data, so the two are put
                // back together in that order.
                let mut channel_data = table[channel * height * entry..(channel + 1) * height * entry].to_vec();
                channel_data.extend_from_slice(cursor.take(lengths[channel])?);
                let plane = channel_coder::decode(1, width, height, &channel_data, large_document, None)
                    .map_err(|_| ImageImportError::Unreadable)?;
                planes.push(plane);
            }
        }
        _ => return Err(ImageImportError::Unreadable),
    }
    let alpha = planes.get(color_channels).cloned().unwrap_or_else(|| vec![255u8; plane_len]);
    let (red, green, blue) = match color_mode {
        1 => {
            let gray = planes[0].clone();
            (gray.clone(), gray.clone(), gray)
        }
        4 => {
            // A naive CMYK conversion; the original leaned on ImageIO's color management here.
            let key = &planes[3];
            let mut separated = [vec![0u8; plane_len], vec![0u8; plane_len], vec![0u8; plane_len]];
            for index in 0..plane_len {
                for channel in 0..3 {
                    separated[channel][index] = 255 - (planes[channel][index] as u16 + key[index] as u16).min(255) as u8;
                }
            }
            let [red, green, blue] = separated;
            (red, green, blue)
        }
        _ => (planes[0].clone(), planes[1].clone(), planes[2].clone()),
    };
    Ok(channel_coder::rgba_image(width, height, &red, &green, &blue, &alpha))
}

/// A bounds-checked big-endian reader over the Photoshop sections.
struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ImageImportError> {
        let data = self.data;
        let end = self.offset.checked_add(count).ok_or(ImageImportError::Unreadable)?;
        let slice = data.get(self.offset..end).ok_or(ImageImportError::Unreadable)?;
        self.offset = end;
        Ok(slice)
    }

    fn skip(&mut self, count: usize) -> Result<(), ImageImportError> {
        self.take(count).map(|_| ())
    }

    fn u16(&mut self) -> Result<u16, ImageImportError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ImageImportError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, ImageImportError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary file the test owns; the directory is removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let directory = std::env::temp_dir().join(format!("compositor-import-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).expect("temp dir");
            let path = directory.join(name);
            std::fs::write(&path, bytes).expect("write fixture");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(directory) = self.0.parent() {
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }

    fn eight_by_eight() -> Rgba8Image {
        let mut image = Rgba8Image::new(8, 8);
        for y in 0..8 {
            for x in 0..8 {
                image.set(x, y, [255, 0, 0, 255]);
            }
        }
        image.set(0, 0, [0, 255, 0, 255]);
        image
    }

    fn png_bytes(image: &Rgba8Image) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, image.width() as u32, image.height() as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().expect("header").write_image_data(image.data()).expect("pixels");
        }
        out
    }

    #[test]
    fn png_round_trips_into_premultiplied_pixels_and_a_small_thumbnail() {
        let fixture = Scratch::new("canvas.png", &png_bytes(&eight_by_eight()));
        let asset = ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).expect("decode");
        assert_eq!(asset.name, "canvas");
        let image = asset.image.as_rgba().expect("rgba");
        assert_eq!((image.width(), image.height()), (8, 8));
        assert_eq!(image.get(0, 0), [0, 255, 0, 255]);
        let thumbnail = asset.thumbnail.as_rgba().expect("rgba thumbnail");
        assert_eq!((thumbnail.width(), thumbnail.height()), (8, 8));
    }

    #[test]
    fn jpeg_decodes_and_the_thumbnail_is_capped_at_96() {
        // The JPEG encoder takes RGB, so the fixture drops the alpha channel.
        let rgb = (0..400 * 100).flat_map(|_| [10u8, 20, 30]).collect::<Vec<u8>>();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&rgb, 400, 100, image::ExtendedColorType::Rgb8)
            .expect("encode");
        let fixture = Scratch::new("photo.jpg", &jpeg);
        let asset = ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).expect("decode");
        let image = asset.image.as_rgba().expect("rgba");
        assert_eq!((image.width(), image.height()), (400, 100));
        let thumbnail = asset.thumbnail.as_rgba().expect("rgba thumbnail");
        assert_eq!((thumbnail.width(), thumbnail.height()), (96, 24));
    }

    #[test]
    fn bmp_decodes_through_the_image_crate() {
        let rgb = (0..8 * 8).flat_map(|_| [255u8, 0, 0]).collect::<Vec<u8>>();
        let mut bmp = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut bmp)
            .encode(&rgb, 8, 8, image::ExtendedColorType::Rgb8)
            .expect("encode bmp");
        let fixture = Scratch::new("canvas.bmp", &bmp);
        let asset = ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).expect("decode bmp");
        assert_eq!(asset.image.as_rgba().expect("rgba").get(0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn oversized_imports_are_refused_before_decoding() {
        let fixture = Scratch::new("canvas.png", &png_bytes(&eight_by_eight()));
        assert_eq!(
            ImageImporter::decode(fixture.path(), 63, false).unwrap_err(),
            ImageImportError::TooLarge
        );
    }

    #[test]
    fn the_end_of_the_budget_is_spent_pixel_by_pixel() {
        let fixture = Scratch::new("canvas.png", &png_bytes(&eight_by_eight()));
        assert!(ImageImporter::decode(fixture.path(), 64, false).is_ok());
    }

    #[test]
    fn heic_is_rejected_with_the_unchanged_unsupported_error() {
        let mut heic = vec![0u8, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0u8; 16]);
        let fixture = Scratch::new("photo.heic", &heic);
        assert_eq!(
            ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).unwrap_err(),
            ImageImportError::Unsupported
        );
    }

    #[test]
    fn damage_and_unknown_types_are_rejected() {
        let fixture = Scratch::new("broken.png", b"\x89PNG\r\n\x1a\ngarbage");
        assert_eq!(
            ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).unwrap_err(),
            ImageImportError::Unreadable
        );
        let webp = Scratch::new("photo.webp", b"RIFF\x00\x00\x00\x00WEBPVP8 ");
        assert_eq!(
            ImageImporter::decode(webp.path(), limits::document_pixel_budget(), false).unwrap_err(),
            ImageImportError::Unsupported
        );
        assert_eq!(
            ImageImporter::decode(Path::new("does-not-exist.png"), limits::document_pixel_budget(), false).unwrap_err(),
            ImageImportError::Unreadable
        );
    }

    #[test]
    fn svg_is_rasterized_at_the_declared_size_or_fitted_to_the_canvas() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#ff0000"/></svg>"##;
        let fixture = Scratch::new("mark.svg", svg);
        let asset = ImageImporter::decode_svg(fixture.path(), None, limits::document_pixel_budget()).expect("decode svg");
        let image = asset.image.as_rgba().expect("rgba");
        assert_eq!((image.width(), image.height()), (20, 10));
        // Fitted to a 40×40 canvas: the 2:1 aspect ratio is kept, the long side becomes 40.
        let fitted = ImageImporter::decode_svg(fixture.path(), Some(Size::new(40.0, 40.0)), limits::document_pixel_budget())
            .expect("decode svg fitted");
        let fitted = fitted.image.as_rgba().expect("rgba");
        assert_eq!((fitted.width(), fitted.height()), (40, 20));
        // A budget below the raster size is refused.
        assert_eq!(
            ImageImporter::decode_svg(fixture.path(), None, 199).unwrap_err(),
            ImageImportError::TooLarge
        );
    }

    #[test]
    fn flattened_photoshop_reads_the_merged_composite() {
        let fixture = Scratch::new("flat.psd", &flat_rgb_psd(2, 2, &[[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]));
        let asset = ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), true).expect("decode psd");
        assert_eq!(asset.name, "flat");
        let image = asset.image.as_rgba().expect("rgba");
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(image.get(1, 0), [0, 255, 0, 255]);
        assert_eq!(image.get(0, 1), [0, 0, 255, 255]);
        assert_eq!(image.get(1, 1), [255, 255, 255, 255]);
        // Without `flattenedPhotoshop`, a PSD is not part of the accepted set.
        assert_eq!(
            ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), false).unwrap_err(),
            ImageImportError::Unsupported
        );
    }

    /// A minimal RGB PSD header with empty color mode/resources/layer sections.
    fn rgb_psd_header(width: usize, height: usize, depth: u16) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"8BPS");
        data.extend_from_slice(&1u16.to_be_bytes()); // version
        data.extend_from_slice(&[0u8; 6]);
        data.extend_from_slice(&3u16.to_be_bytes()); // channels
        data.extend_from_slice(&(height as u32).to_be_bytes());
        data.extend_from_slice(&(width as u32).to_be_bytes());
        data.extend_from_slice(&depth.to_be_bytes());
        data.extend_from_slice(&3u16.to_be_bytes()); // RGB
        data.extend_from_slice(&0u32.to_be_bytes()); // color mode data
        data.extend_from_slice(&0u32.to_be_bytes()); // image resources
        data.extend_from_slice(&0u32.to_be_bytes()); // layer and mask information
        data
    }

    /// A flattened RGB PSD whose image data is raw (compression 0).
    fn flat_rgb_psd(width: usize, height: usize, pixels: &[[u8; 3]]) -> Vec<u8> {
        let mut data = rgb_psd_header(width, height, 8);
        data.extend_from_slice(&0u16.to_be_bytes()); // compression
        for channel in 0..3 {
            data.extend(pixels.iter().map(|pixel| pixel[channel]));
        }
        data
    }

    /// The same, PackBits-compressed (compression 1): the row counts for all channels, then each
    /// channel's rows as a single literal run.
    fn flat_rgb_psd_rle(width: usize, height: usize, pixels: &[[u8; 3]]) -> Vec<u8> {
        let mut data = rgb_psd_header(width, height, 8);
        data.extend_from_slice(&1u16.to_be_bytes()); // compression
        for _ in 0..3 * height {
            data.extend_from_slice(&(width as u16 + 1).to_be_bytes());
        }
        for channel in 0..3 {
            for row in 0..height {
                data.push((width - 1) as u8);
                for column in 0..width {
                    data.push(pixels[row * width + column][channel]);
                }
            }
        }
        data
    }

    #[test]
    fn flattened_photoshop_reads_packbits_data() {
        let pixels = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
        let fixture = Scratch::new("flat.psd", &flat_rgb_psd_rle(2, 2, &pixels));
        let asset = ImageImporter::decode(fixture.path(), limits::document_pixel_budget(), true).expect("decode psd");
        let image = asset.image.as_rgba().expect("rgba");
        assert_eq!(image.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(image.get(1, 0), [0, 255, 0, 255]);
        assert_eq!(image.get(0, 1), [0, 0, 255, 255]);
        assert_eq!(image.get(1, 1), [255, 255, 255, 255]);
    }

    #[test]
    fn flattened_photoshop_refuses_depths_and_compressions_it_cannot_read() {
        let mut deep = rgb_psd_header(2, 2, 16);
        deep.extend_from_slice(&0u16.to_be_bytes());
        deep.extend_from_slice(&[0u8; 24]);
        let deep = Scratch::new("deep.psd", &deep);
        assert_eq!(
            ImageImporter::decode(deep.path(), limits::document_pixel_budget(), true).unwrap_err(),
            ImageImportError::Unreadable
        );
        let mut zipped = rgb_psd_header(2, 2, 8);
        zipped.extend_from_slice(&2u16.to_be_bytes()); // ZIP: not a layout we guess at
        zipped.extend_from_slice(&[0u8; 12]);
        let zipped = Scratch::new("zipped.psd", &zipped);
        assert_eq!(
            ImageImporter::decode(zipped.path(), limits::document_pixel_budget(), true).unwrap_err(),
            ImageImportError::Unreadable
        );
        // A flat photo file with empty layer records still owes its size to the header.
        let mut truncated = rgb_psd_header(2, 2, 8);
        truncated.extend_from_slice(&0u16.to_be_bytes());
        let truncated = Scratch::new("cut.psd", &truncated);
        assert_eq!(
            ImageImporter::decode(truncated.path(), limits::document_pixel_budget(), true).unwrap_err(),
            ImageImportError::Unreadable
        );
    }
}
