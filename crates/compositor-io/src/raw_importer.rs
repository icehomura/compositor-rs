//! Camera RAW: recognizing a RAW file, its frame size, the camera's own white balance, and the
//! develop itself (`RawImporter`).
//!
//! The original drove `CIRAWFilter`, whose controls are Kelvin and a "boost" amount. The port's
//! [`compositor_pixels::camera_raw`] grade works on already-decoded pixels and its temperature/tint
//! are *relative offsets*, not Kelvin (its own docs say so). The mapping between the two lives in
//! [`camera_settings`] and is documented there: the develop sheet keeps its exact settings, the
//! pixels differ by however much the two white-balance models differ. The decode itself is
//! `rawler` (demosaic, camera white balance, sRGB), and the camera's as-shot reading has no Kelvin
//! in `rawler`'s metadata, so the develop sheet starts neutral at
//! [`DEFAULT_TEMPERATURE`] — the value the camera's own balance already produced.

use compositor_core::imported_image::ImageImportError;
use compositor_core::Rgba8Image;
use compositor_pixels::camera_raw::CameraRawSettings;
use compositor_pixels::resample::{scale_rgba, ResampleFilter};
use parking_lot::Mutex;
use rawler::decoders::RawDecodeParams;
use rawler::imgop::develop::RawDevelop;
use rawler::rawsource::RawSource;
use rawler::get_decoder;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub use compositor_pixels::camera_raw::{RawDevelopSettings, DEFAULT_TEMPERATURE};

/// `RawImporter`.
pub enum RawImporter {}

impl RawImporter {
    /// Every camera RAW the system can develop — 30 formats, from Canon and Nikon to DNG — rather
    /// than a list of vendors that would need extending with each new camera. The extension decides
    /// first (as `UTType(filenameExtension:).conforms(to: .rawImage)` did), and the container magic
    /// catches a RAW whose extension was changed or is missing.
    pub fn matches(url: &Path) -> bool {
        if let Some(extension) = url.extension().and_then(|extension| extension.to_str()) {
            if RAW_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
                return true;
            }
        }
        magic_matches(url)
    }

    /// The camera's own white balance, which is where the sliders start. `rawler` records the
    /// camera's balance as channel multipliers rather than Kelvin, and the decoded frame already
    /// carries it, so the reading is the neutral [`DEFAULT_TEMPERATURE`].
    pub fn as_shot(url: &Path) -> Option<RawDevelopSettings> {
        let source = RawSource::new(url).ok()?;
        get_decoder(&source).ok()?;
        Some(RawDevelopSettings::as_shot(DEFAULT_TEMPERATURE, 0.0))
    }

    /// The developed image. `limit` caps the long edge for the preview the sheet shows while the
    /// sliders move; the import itself passes `None` and gets the full frame.
    pub fn develop(url: &Path, settings: &RawDevelopSettings, limit: Option<f64>) -> Result<Rgba8Image, ImageImportError> {
        let frame = develop_frame(url)?;
        grade(&frame, settings, limit)
    }

    /// The frame's size without developing it, so an oversized file is refused before the work.
    ///
    /// `CGImageSourceCopyPropertiesAtIndex` read this from the metadata; `rawler` has no
    /// metadata-only entry point for the sensor geometry, so the decoder is asked for a *dummy*
    /// frame: the container is parsed and the output buffer is allocated, but the sensor data is
    /// never decompressed.
    pub fn pixel_size(url: &Path) -> Option<(usize, usize)> {
        let source = RawSource::new(url).ok()?;
        let decoder = get_decoder(&source).ok()?;
        let image = decoder.raw_image(&source, &RawDecodeParams::default(), true).ok()?;
        Some((image.width, image.height))
    }
}

/// One context for every develop: building a `CIContext` allocated GPU resources, and the sheet
/// develops again on each slider move.
///
/// Developing a RAW is seconds of work, so only one runs at a time and the caller waits its turn.
/// Without this a dragged slider starts a render per pixel moved and they all pile up.
///
/// The preview keeps its decoded frame between renders, which is what makes the sliders feel live:
/// decoding the file again costs about 1.5 s, while changing exposure or white balance on a frame
/// that already exists costs nothing measurable.
pub struct Queue {
    cached: Mutex<Option<Cached>>,
}

struct Cached {
    url: PathBuf,
    image: Rgba8Image,
}

impl Queue {
    pub fn shared() -> &'static Queue {
        static QUEUE: LazyLock<Queue> = LazyLock::new(|| Queue {
            cached: Mutex::new(None),
        });
        &QUEUE
    }

    /// `develop(_:settings:limit:)`: with no limit the frame is developed outright; with one, the
    /// decoded frame is reused, graded, and scaled down for the sheet's preview.
    pub fn develop(&self, url: &Path, settings: &RawDevelopSettings, limit: Option<f64>) -> Option<Rgba8Image> {
        let Some(limit) = limit else {
            return RawImporter::develop(url, settings, None).ok();
        };
        // The lock is what serializes develops; it is held for the whole decode on purpose.
        let mut cached = self.cached.lock();
        let frame = match cached.as_ref() {
            Some(entry) if entry.url == url => entry.image.clone(),
            _ => {
                let image = develop_frame(url).ok()?;
                *cached = Some(Cached {
                    url: url.to_path_buf(),
                    image: image.clone(),
                });
                image
            }
        };
        grade(&frame, settings, Some(limit)).ok()
    }

    /// Lets go of the decoded frame when the sheet closes.
    pub fn release(&self) {
        *self.cached.lock() = None;
    }
}

/// The decoded, camera-balanced frame in sRGB — `CIRAWFilter`'s cached decode.
fn develop_frame(url: &Path) -> Result<Rgba8Image, ImageImportError> {
    let raw = rawler::decode_file(url).map_err(|_| ImageImportError::Unreadable)?;
    let intermediate = RawDevelop::default()
        .develop_intermediate(&raw)
        .map_err(|_| ImageImportError::Unreadable)?;
    let developed = intermediate.to_dynamic_image().ok_or(ImageImportError::Unreadable)?;
    let rgba = developed.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    Ok(Rgba8Image::from_data(width, height, rgba.into_raw()))
}

/// The develop settings applied to the frame, then the optional draft scale.
fn grade(frame: &Rgba8Image, settings: &RawDevelopSettings, limit: Option<f64>) -> Result<Rgba8Image, ImageImportError> {
    let graded = camera_settings(settings)
        .apply(frame, None, 1.0, 0, -1, false)
        .map_err(|_| ImageImportError::Unreadable)?;
    let longest = frame.width().max(frame.height()) as f64;
    match limit {
        Some(limit) if limit > 0.0 && longest > limit => {
            let scale = limit / longest;
            let width = ((frame.width() as f64 * scale).round() as usize).max(1);
            let height = ((frame.height() as f64 * scale).round() as usize).max(1);
            // `CIRAWFilter.isDraftModeEnabled` — a fast reduction, Core Graphics' `.low` filter.
            Ok(scale_rgba(&graded, width, height, ResampleFilter::Bilinear))
        }
        _ => Ok(graded),
    }
}

/// The develop sheet's settings in the kernel's terms.
///
/// `exposure` is stops in both. `temperature` is Kelvin here and a ±100 relative offset there, so
/// the camera's reading becomes 0 and the slider's swing is a share of it; positive stays warmer,
/// which is the direction [`CameraRawSettings`]' gains move red against blue. `tint` is relative in
/// both. `boostAmount`'s tone curve has no counterpart, so it rides the closest single control,
/// `contrast` (1 → 0, 0 → −100): an approximation of Apple's curve, not a clone of it.
fn camera_settings(settings: &RawDevelopSettings) -> CameraRawSettings {
    let as_shot = if settings.as_shot_temperature > 0.0 {
        settings.as_shot_temperature
    } else {
        DEFAULT_TEMPERATURE
    };
    let mut camera = CameraRawSettings::default();
    camera.exposure = settings.exposure as f64;
    camera.temperature = ((settings.temperature - as_shot) as f64 / as_shot as f64 * 100.0).clamp(-100.0, 100.0);
    camera.tint = (settings.tint as f64).clamp(-100.0, 100.0);
    camera.contrast = ((settings.boost as f64 - 1.0) * 100.0).clamp(-100.0, 0.0);
    camera
}

/// The extensions `UTType.rawImage` covers that `rawler` can decode.
const RAW_EXTENSIONS: [&str; 36] = [
    "3fr", "ari", "arw", "bay", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "eip", "erf", "fff", "gpr", "iiq",
    "k25", "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "ptx", "pxn", "raf", "raw", "rw2", "rwl",
    "sr2", "srf", "srw", "x3f",
];

/// The container magics `rawler`'s own decoders sniff, for a RAW whose extension is missing.
fn magic_matches(url: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(url) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut header = [0u8; 16];
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    let header = &header[..filled];
    // ARRIRAW, Apple QuickTake, Fujifilm RAF, Sigma X3F, Minolta MRW, Canon CIFF and Canon CR3.
    header.starts_with(b"ARRI")
        || header.starts_with(b"qktk")
        || header.starts_with(b"qktn")
        || header.starts_with(b"FUJIFILM")
        || header.starts_with(b"FOVb")
        || header.starts_with(b"\0MRM")
        || (header.len() >= 14 && header[6..14] == *b"HEAPCCDR")
        || (header.len() >= 12 && header[4..8] == *b"ftyp" && header[8..12] == *b"crx ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn develop_settings_default_to_the_camera_reading() {
        let settings = RawDevelopSettings::default();
        assert_eq!(settings.temperature, 5000.0);
        assert_eq!(settings.as_shot_temperature, 5000.0);
        assert_eq!(settings.tint, 0.0);
        assert_eq!(settings.boost, 1.0);
        assert!(settings.is_as_shot());
    }

    #[test]
    fn reset_returns_to_the_camera_reading() {
        let mut settings = RawDevelopSettings::as_shot(5200.0, -8.0);
        settings.exposure = 1.5;
        settings.boost = 0.25;
        settings.temperature = 7000.0;
        assert!(!settings.is_as_shot());
        settings.reset();
        assert!(settings.is_as_shot());
        assert_eq!(settings.temperature, 5200.0);
        assert_eq!(settings.tint, -8.0);
        assert_eq!(settings.as_shot_temperature, 5200.0);
        assert_eq!(settings.as_shot_tint, -8.0);
    }

    #[test]
    fn the_as_shot_reading_maps_to_a_neutral_grade() {
        let camera = camera_settings(&RawDevelopSettings::default());
        assert_eq!(camera.exposure, 0.0);
        assert_eq!(camera.temperature, 0.0);
        assert_eq!(camera.tint, 0.0);
        assert_eq!(camera.contrast, 0.0);
        assert!(camera.is_identity());
    }

    #[test]
    fn the_develop_sliders_map_onto_the_kernel() {
        let camera = camera_settings(&RawDevelopSettings {
            exposure: 2.0,
            temperature: 6000.0,
            tint: 20.0,
            boost: 0.0,
            as_shot_temperature: 5000.0,
            as_shot_tint: 0.0,
        });
        assert_eq!(camera.exposure, 2.0);
        // 1000 K over the camera's 5000 K is a fifth of the relative swing.
        assert_eq!(camera.temperature, 20.0);
        assert_eq!(camera.tint, 20.0);
        // Boost 0 is Apple's flat, neutral curve: the kernel's fully negative contrast.
        assert_eq!(camera.contrast, -100.0);
        // A cooler swing stays on the negative side of the same scale.
        let cooler = camera_settings(&RawDevelopSettings {
            temperature: 4000.0,
            as_shot_temperature: 5000.0,
            ..RawDevelopSettings::default()
        });
        assert_eq!(cooler.temperature, -20.0);
    }

    #[test]
    fn raw_files_are_recognized_by_extension_and_by_magic() {
        assert!(RawImporter::matches(Path::new("photo.CR2")));
        assert!(RawImporter::matches(Path::new("photo.dng")));
        assert!(RawImporter::matches(Path::new("scan.raf")));
        assert!(!RawImporter::matches(Path::new("photo.png")));
        assert!(!RawImporter::matches(Path::new("photo.heic")));
    }

    #[test]
    fn the_queue_serves_the_full_develop_without_a_limit() {
        // A missing file cannot be developed, and both paths report that the same way.
        let settings = RawDevelopSettings::default();
        assert!(RawImporter::develop(Path::new("nope.cr2"), &settings, None).is_err());
        assert!(Queue::shared().develop(Path::new("nope.cr2"), &settings, None).is_none());
        assert!(Queue::shared().develop(Path::new("nope.cr2"), &settings, Some(800.0)).is_none());
        Queue::shared().release();
    }
}
