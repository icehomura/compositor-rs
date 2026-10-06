//! Image export: the composited canvas written as PNG or JPEG, with the document's resolution
//! (pixels per inch) in the file's own metadata (`ImageExporter`).
//!
//! The original `ImageExporter.render(_:)` composited the whole project through the Core Graphics
//! pipeline. `compositor-rs-io` sits below the renderer, so the composited raster arrives here as an
//! [`ExportRaster`] — the canvas renderer produces it and this module encodes it. The encoding,
//! the resolution metadata, the flatten-over-background JPEG path, the 1,024 px QuickLook preview
//! and the atomic replacement of the destination file are the same as upstream.

use compositor_rs_core::limits;
use compositor_rs_core::Rgba8Image;
use compositor_rs_core::SharedImage;
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use image::codecs::jpeg::{JpegEncoder, PixelDensity};
use image::{ExtendedColorType, ImageFormat};
use png::{BitDepth, ColorType, Encoder, PixelDimensions, Unit};
use std::path::Path;
use std::sync::Arc;

/// `ExportError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportError {
    TooLarge,
    Render,
    Encode,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(
                formatter,
                "Image export supports canvases up to {} megapixels and {} pixels per side.",
                limits::max_surface_megapixels(),
                grouped(limits::MAX_SIDE)
            ),
            Self::Render => formatter.write_str("The canvas could not be rendered. Try a smaller canvas."),
            Self::Encode => formatter.write_str("The image could not be encoded."),
        }
    }
}

impl std::error::Error for ExportError {}

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

/// `ExportRaster`: the composited canvas and the resolution written into the file's metadata.
#[derive(Clone, Debug)]
pub struct ExportRaster {
    pub image: SharedImage,
    /// Pixels per inch; `72` unless the document carries its own resolution.
    pub resolution: f64,
}

impl ExportRaster {
    /// `ExportRaster(image:)` — the default 72 pixels per inch.
    pub fn new(image: SharedImage) -> Self {
        Self {
            image,
            resolution: 72.0,
        }
    }

    /// `ExportRaster(image:resolution:)`.
    pub fn with_resolution(image: SharedImage, resolution: f64) -> Self {
        Self { image, resolution }
    }
}

/// `JPEGOptions`: the export sheet's quality and the color the transparency is flattened onto.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JPEGOptions {
    pub quality: f64,
    pub red: f64,
    pub green: f64,
    pub blue: f64,
}

impl Default for JPEGOptions {
    fn default() -> Self {
        Self {
            quality: 0.85,
            red: 1.0,
            green: 1.0,
            blue: 1.0,
        }
    }
}

/// `JPEGResult`: the encoded file and the preview the sheet shows at 100%.
#[derive(Clone, Debug)]
pub struct JPEGResult {
    pub data: Vec<u8>,
    pub preview: Rgba8Image,
}

/// `QuickLookImages`: the Space-bar preview saved in the project's QuickLook folder.
#[derive(Clone, Debug)]
pub struct QuickLookImages {
    pub preview: Vec<u8>,
}

/// `ImageExporter`.
pub enum ImageExporter {}

impl ImageExporter {
    /// `render(_:)`'s size guard: the canvas an export may flatten.
    pub fn check_canvas_size(width: usize, height: usize) -> Result<(), ExportError> {
        if width < 1
            || height < 1
            || width > limits::MAX_SIDE
            || height > limits::MAX_SIDE
            || width.saturating_mul(height) > limits::MAX_SURFACE_PIXELS
        {
            return Err(ExportError::TooLarge);
        }
        Ok(())
    }

    /// `pngData(_:)`: the canvas as PNG, with the document's resolution in the `pHYs` chunk.
    pub fn png_data(raster: &ExportRaster) -> Result<Vec<u8>, ExportError> {
        Self::check_canvas_size(raster.image.width(), raster.image.height())?;
        encode_png(&raster.image, raster.resolution)
    }

    /// `exportPNG(_:to:)`.
    pub fn export_png(raster: &ExportRaster, url: &Path) -> Result<(), ExportError> {
        let data = Self::png_data(raster)?;
        Self::write(&data, url).map_err(|_| ExportError::Encode)
    }

    /// `write(_:to:)`: the data replaces the file at `url`, never leaving a half-written file
    /// behind (`NSFileCoordinator`'s `.forReplacing` write with `Data.write(options: .atomic)`).
    pub fn write(data: &[u8], url: &Path) -> std::io::Result<()> {
        // A sibling temporary file, so the rename that publishes it stays on one volume.
        let directory = url.parent().filter(|parent| !parent.as_os_str().is_empty());
        let name = url.file_name().map_or_else(std::ffi::OsString::new, |name| std::ffi::OsString::from(name));
        let mut temporary = name;
        temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let temporary = match directory {
            Some(directory) => directory.join(temporary),
            None => Path::new(&temporary).to_path_buf(),
        };
        if let Err(error) = std::fs::write(&temporary, data) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temporary, url) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    /// `jpeg(_:options:)`: the canvas flattened onto `options`' color and encoded, plus the preview
    /// the export sheet shows.
    pub fn jpeg(raster: &ExportRaster, options: &JPEGOptions) -> Result<JPEGResult, ExportError> {
        let image = &raster.image;
        Self::check_canvas_size(image.width(), image.height())?;
        let flattened = flatten(image, options.red, options.green, options.blue);
        let quality = (options.quality.clamp(0.0, 1.0) * 100.0).round() as u8;
        let data = encode_jpeg(&flattened, image.width(), image.height(), quality, Some(raster.resolution))?;
        let preview = preview_of_jpeg(&data, image.width().max(image.height()))?;
        Ok(JPEGResult { data, preview })
    }

    /// `quickLookImages(_:)`: the flattened image on white, a JPEG up to 1,024 px on the long side.
    /// Nil for canvases too large to flatten on every save.
    pub fn quick_look_images(raster: &ExportRaster) -> Option<QuickLookImages> {
        if raster.image.width().saturating_mul(raster.image.height()) > 50_000_000 {
            return None;
        }
        let preview = Self::scaled_jpeg(&raster.image, 1024.0).ok()?;
        Some(QuickLookImages { preview })
    }

    /// `scaledJPEG(_:longSide:)`: the image on white, scaled down to `long_side` at most, as an
    /// 80 %-quality JPEG with no resolution metadata.
    fn scaled_jpeg(image: &Rgba8Image, long_side: f64) -> Result<Vec<u8>, ExportError> {
        let scale = (long_side / image.width().max(image.height()) as f64).min(1.0);
        let width = ((image.width() as f64 * scale).round() as usize).max(1);
        let height = ((image.height() as f64 * scale).round() as usize).max(1);
        let mut canvas = Canvas::from_rgba(Rgba8Image::opaque(width, height, [255, 255, 255, 255]));
        canvas.set_interpolation_quality(InterpolationQuality::High);
        canvas.draw_image(image, compositor_rs_core::Rect::new(0.0, 0.0, width as f64, height as f64));
        let flattened = canvas.into_rgba();
        // The flatten onto opaque white already made every pixel opaque, so the premultiplied
        // channels are the file's straight ones.
        let rgb = flattened
            .data()
            .chunks_exact(compositor_rs_core::RGBA_PIXEL)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect::<Vec<u8>>();
        encode_jpeg(&rgb, width, height, 80, None)
    }
}

/// `encode(_:type:properties:)` for PNG: 8-bit sRGB RGBA with the resolution as `pHYs`
/// (pixels per metre, the only unit the chunk carries).
fn encode_png(image: &Rgba8Image, resolution: f64) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, image.width() as u32, image.height() as u32);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_pixel_dims(pixel_dimensions(resolution));
        let mut writer = encoder.write_header().map_err(|_| ExportError::Encode)?;
        writer.write_image_data(&straight_rgba(image)).map_err(|_| ExportError::Encode)?;
    }
    Ok(out)
}

/// `kCGImagePropertyDPIWidth`/`Height` as a PNG `pHYs` chunk (pixels per metre).
fn pixel_dimensions(resolution: f64) -> Option<PixelDimensions> {
    if !resolution.is_finite() || resolution <= 0.0 {
        return None;
    }
    let per_meter = (resolution / 0.0254).round().clamp(0.0, u32::MAX as f64) as u32;
    Some(PixelDimensions {
        xppu: per_meter,
        yppu: per_meter,
        unit: Unit::Meter,
    })
}

/// PNG stores straight alpha; the canonical buffers are premultiplied (`CGBitmapInfo.premultipliedLast`).
fn straight_rgba(image: &Rgba8Image) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.data().len());
    for pixel in image.pixels() {
        let alpha = pixel[3] as u32;
        if alpha == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        for channel in &pixel[..3] {
            let value = (*channel as u32 * 255 + alpha / 2) / alpha;
            out.push(value.min(255) as u8);
        }
        out.push(pixel[3]);
    }
    out
}

/// The canvas flattened onto a solid color, as the RGB bytes JPEG carries: premultiplied
/// source-over onto an opaque backdrop is `channel + backdrop * (1 - alpha)`.
fn flatten(image: &Rgba8Image, red: f64, green: f64, blue: f64) -> Vec<u8> {
    let background = [red.clamp(0.0, 1.0), green.clamp(0.0, 1.0), blue.clamp(0.0, 1.0)];
    let mut out = Vec::with_capacity(image.width() * image.height() * 3);
    for pixel in image.pixels() {
        let alpha = pixel[3] as f64 / 255.0;
        for channel in 0..3 {
            let value = pixel[channel] as f64 / 255.0 + background[channel] * (1.0 - alpha);
            out.push(compositor_rs_core::color::to_byte(value));
        }
    }
    out
}

/// `encode(_:type:properties:)` for JPEG, with the resolution in the JFIF header.
fn encode_jpeg(rgb: &[u8], width: usize, height: usize, quality: u8, resolution: Option<f64>) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    if let Some(resolution) = resolution {
        if resolution.is_finite() && resolution > 0.0 {
            encoder.set_pixel_density(PixelDensity::dpi(resolution.round().clamp(1.0, u16::MAX as f64) as u16));
        }
    }
    encoder
        .encode(rgb, width as u32, height as u32, ExtendedColorType::Rgb8)
        .map_err(|_| ExportError::Encode)?;
    Ok(out)
}

/// `CGImageSourceCreateThumbnailAtIndex`: the full decoded image, capped to 8,192 px on the long
/// side so the sheet's 100 % view shows the real artifacts without unbounded memory.
fn preview_of_jpeg(data: &[u8], longest: usize) -> Result<Rgba8Image, ExportError> {
    let decoded = image::load_from_memory_with_format(data, ImageFormat::Jpeg).map_err(|_| ExportError::Encode)?;
    let cap = longest.min(8192);
    let (width, height) = (decoded.width() as usize, decoded.height() as usize);
    let scale = (cap as f64 / width.max(height) as f64).min(1.0);
    let target_width = ((width as f64 * scale).round() as usize).max(1);
    let target_height = ((height as f64 * scale).round() as usize).max(1);
    let rgba = decoded.to_rgba8();
    let image = Rgba8Image::from_data(width, height, rgba.into_raw());
    if (target_width, target_height) == (width, height) {
        return Ok(image);
    }
    let mut canvas = Canvas::new_rgba(target_width, target_height);
    canvas.set_interpolation_quality(InterpolationQuality::High);
    canvas.draw_image(&image, compositor_rs_core::Rect::new(0.0, 0.0, target_width as f64, target_height as f64));
    Ok(canvas.into_rgba())
}

/// A shared, immutable canvas — the exporter's canonical input.
pub fn shared(image: Rgba8Image) -> SharedImage {
    Arc::new(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8×8 canvas with a translucent quadrant, so the premultiplication round-trips too.
    fn eight_by_eight() -> Rgba8Image {
        let mut image = Rgba8Image::new(8, 8);
        for y in 0..8 {
            for x in 0..8 {
                if x < 4 && y < 4 {
                    image.set(x, y, [255, 0, 0, 255]);
                } else if x >= 4 && y < 4 {
                    image.set(x, y, [0, 255, 0, 255]);
                } else if x < 4 {
                    // Premultiplied: half-transparent blue, straight (0, 0, 128, 128).
                    image.set(x, y, [0, 0, 64, 128]);
                } else {
                    image.set(x, y, [0, 0, 0, 0]);
                }
            }
        }
        image
    }

    fn raster(resolution: f64) -> ExportRaster {
        ExportRaster::with_resolution(shared(eight_by_eight()), resolution)
    }

    #[test]
    fn export_error_messages_match_the_original() {
        assert_eq!(
            ExportError::TooLarge.to_string(),
            format!(
                "Image export supports canvases up to {} megapixels and 30,000 pixels per side.",
                limits::max_surface_megapixels()
            )
        );
        assert_eq!(
            ExportError::Render.to_string(),
            "The canvas could not be rendered. Try a smaller canvas."
        );
        assert_eq!(ExportError::Encode.to_string(), "The image could not be encoded.");
    }

    #[test]
    fn png_round_trips_an_eight_by_eight_canvas_with_its_resolution() {
        let data = ImageExporter::png_data(&raster(300.0)).expect("encode");
        let decoder = png::Decoder::new(std::io::Cursor::new(&data));
        let reader = decoder.read_info().expect("png header");
        let info = reader.info();
        assert_eq!((info.width, info.height), (8, 8));
        // 300 dpi = 300 / 0.0254 = 11811.02… pixels per metre, the same number ImageIO writes.
        let dimensions = info.pixel_dims.expect("pHYs chunk");
        assert_eq!((dimensions.xppu, dimensions.yppu), (11811, 11811));
        assert_eq!(dimensions.unit, Unit::Meter);
        // The straight-alpha pixels come back exactly as they went in.
        let decoded = image::load_from_memory_with_format(&data, ImageFormat::Png).expect("decode").to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(7, 0).0, [0, 255, 0, 255]);
        assert_eq!(decoded.get_pixel(0, 7).0, [0, 0, 128, 128]);
        assert_eq!(decoded.get_pixel(7, 7).0, [0, 0, 0, 0]);
    }

    #[test]
    fn jpeg_encodes_the_canvas_and_keeps_its_resolution() {
        let raster = raster(300.0);
        let result = ImageExporter::jpeg(&raster, &JPEGOptions::default()).expect("encode");
        assert_eq!(jfif_density(&result.data), Some((1, 300, 300)));
        let decoded = image::load_from_memory_with_format(&result.data, ImageFormat::Jpeg)
            .expect("decode")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (8, 8));
        // Fully opaque: the transparency was flattened onto the options' color.
        assert!(decoded.pixels().all(|pixel| pixel.0[3] == 255));
        assert_eq!((result.preview.width(), result.preview.height()), (8, 8));
    }

    #[test]
    fn jpeg_flattens_transparency_onto_the_requested_color() {
        // Premultiplied: half-transparent white (straight 255 at alpha 128) beside a hole.
        let mut canvas = Rgba8Image::new(2, 2);
        canvas.set(0, 0, [128, 128, 128, 128]);
        let raster = ExportRaster::new(shared(canvas));

        let black = JPEGOptions {
            quality: 1.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
        };
        let dark = ImageExporter::jpeg(&raster, &black).expect("encode");
        let decoded = image::load_from_memory_with_format(&dark.data, ImageFormat::Jpeg)
            .expect("decode")
            .to_rgba8();
        let half = decoded.get_pixel(0, 0).0;
        assert!((120..=136).contains(&half[0]), "half alpha over black: {half:?}");
        let hole = decoded.get_pixel(1, 0).0;
        assert!(hole[0] < 8 && hole[1] < 8 && hole[2] < 8, "the hole is black: {hole:?}");

        let light = ImageExporter::jpeg(&raster, &JPEGOptions::default()).expect("encode");
        let decoded = image::load_from_memory_with_format(&light.data, ImageFormat::Jpeg)
            .expect("decode")
            .to_rgba8();
        assert!(decoded.pixels().all(|pixel| pixel.0[0] > 247), "over white every pixel is white");
    }

    #[test]
    fn png_export_replaces_the_file_atomically() {
        let directory = std::env::temp_dir().join(format!("compositor-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let url = directory.join("canvas.png");
        std::fs::write(&url, b"stale").expect("seed");
        ImageExporter::export_png(&raster(72.0), &url).expect("export");
        let data = std::fs::read(&url).expect("read back");
        assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
        let leftovers: Vec<_> = std::fs::read_dir(&directory)
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn oversized_canvases_are_refused() {
        assert!(ImageExporter::check_canvas_size(8, 8).is_ok());
        assert_eq!(ImageExporter::check_canvas_size(0, 8), Err(ExportError::TooLarge));
        assert_eq!(
            ImageExporter::check_canvas_size(limits::MAX_SIDE + 1, 1),
            Err(ExportError::TooLarge)
        );
        assert_eq!(ImageExporter::check_canvas_size(20_000, 20_000), Err(ExportError::TooLarge));
    }

    #[test]
    fn quick_look_previews_the_canvas_on_white() {
        let small = ExportRaster::new(shared(Rgba8Image::new(8, 8)));
        let preview = ImageExporter::quick_look_images(&small).expect("preview");
        let decoded = image::load_from_memory_with_format(&preview.preview, ImageFormat::Jpeg).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn scaled_jpeg_fits_the_long_side_and_has_no_density() {
        let image = Rgba8Image::opaque(2048, 1024, [10, 20, 30, 255]);
        let data = ImageExporter::scaled_jpeg(&image, 1024.0).expect("encode");
        let decoded = image::load_from_memory_with_format(&data, ImageFormat::Jpeg).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (1024, 512));
        // `scaledJPEG` passes no DPI properties, so the JFIF header keeps the encoder's default.
        assert_eq!(jfif_density(&data), Some((0, 1, 1)));
    }

    /// The JFIF APP0 segment's density unit and values, `None` when the file has no JFIF header.
    fn jfif_density(data: &[u8]) -> Option<(u8, u16, u16)> {
        let mut offset = 2;
        while offset + 4 <= data.len() {
            if data[offset] != 0xFF {
                return None;
            }
            let marker = data[offset + 1];
            let length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            let payload = data.get(offset + 4..offset + 2 + length)?;
            if marker == 0xE0 && payload.starts_with(b"JFIF\0") {
                return Some((
                    payload[7],
                    u16::from_be_bytes([payload[8], payload[9]]),
                    u16::from_be_bytes([payload[10], payload[11]]),
                ));
            }
            offset += 2 + length;
        }
        None
    }

    #[test]
    fn quick_look_refuses_a_canvas_that_is_too_large_to_flatten() {
        // 50 megapixels is the guard; a 60-megapixel raster is refused before any work.
        let raster = ExportRaster::new(shared(Rgba8Image::new(10_000, 6_000)));
        assert!(ImageExporter::quick_look_images(&raster).is_none());
    }
}

