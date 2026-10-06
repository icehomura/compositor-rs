//! The platform services a session runs on: the `.comp` package, the flattened render, image
//! decoding, RAW develop, Photoshop reading and live-mask baking.
//!
//! Ports the host half of the calls the Swift session made directly — `ProjectStore.shared.load/save`,
//! `ImageExporter.shared.render/quickLookImages/pngData/exportPNG/write`, `ImageImporter.shared.decode/
//! decodeSVG/loadPhotoshop/photoshopAssets`, `PSDReader.matches`, `RawImporter` and its `Queue`,
//! `PSDDocumentBuilder.makeImport` and `LiveMaskBaker.bake`. `compositor-session` cannot depend on
//! `compositor-io` (`docs/PORTING.md`'s crate order), so every one of those calls goes through
//! [`SessionHost`]; this is that trait's production implementation and the only place the session and
//! the file work meet.

use std::path::Path;
use std::sync::Arc;

use compositor_core::document::{CanvasDocument, ProjectError, ProjectSnapshot};
use compositor_core::geom::{Rect, Size};
use compositor_core::imported_image::{ImageImportError, ImportedImage, PixelImage};
use compositor_core::raster::{RasterSnapshot, THUMBNAIL_MAX_SIDE};
use compositor_core::{Id, SharedImage};
use compositor_io::image_exporter::{ExportRaster, ImageExporter, QuickLookImages};
use compositor_io::image_importer::ImageImporter;
use compositor_io::project_store::ProjectStore;
use compositor_io::psd::builder::PSDDocumentBuilder;
use compositor_io::psd::reader::PSDReader;
use compositor_io::raw_importer::{Queue, RawImporter};
use compositor_pixels::camera_raw::RawDevelopSettings;
use compositor_pixels::canvas::Canvas;
use compositor_pixels::resample::thumbnail;
use compositor_render::composite::Composite;
use compositor_render::live_mask_renderer::{LiveMaskBaker, LiveMaskGraph};
use compositor_session::projects::{PhotoshopImport, PhotoshopRead, PSDConversion, QuickLookPreview, SessionHost};
use compositor_session::EditorSession;

/// The platform services every tab's session runs on.
pub struct AppHost;

impl AppHost {
    /// The services themselves are process-wide singletons (`ProjectStore.shared`,
    /// `RawImporter.Queue.shared`), so the host carries no state of its own.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AppHost {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionHost for AppHost {
    /// `ProjectStore.shared.load(from:)`.
    fn load_project(&self, url: &Path) -> Result<ProjectSnapshot, String> {
        ProjectStore::shared().load(url).map_err(|error| error.to_string())
    }

    /// `ProjectStore.shared.save(_:to:quickLook:)`.
    fn save_project(&self, snapshot: &ProjectSnapshot, to: &Path, quick_look: Option<QuickLookPreview>) -> Result<(), String> {
        let quick_look = quick_look.map(|preview| QuickLookImages { preview });
        ProjectStore::shared()
            .save(snapshot, to, quick_look.as_ref())
            .map_err(|error| error.to_string())
    }

    /// `ImageExporter.shared.quickLookImages(_:)`: the flattened preview a save stores for Quick Look,
    /// or nil for a canvas too large to flatten.
    fn quick_look_images(&self, snapshot: &ProjectSnapshot) -> Option<QuickLookPreview> {
        // The original refused a canvas this size before rendering it; `ImageExporter::quick_look_images`
        // holds the same 50-megapixel limit, this keeps the work the early guard saves.
        if snapshot.manifest.width.saturating_mul(snapshot.manifest.height) > 50_000_000 {
            return None;
        }
        let raster = flattened(snapshot).ok()?;
        ImageExporter::quick_look_images(&raster).map(|images| images.preview)
    }

    /// `ImageExporter.shared.render(_:)`: the document flattened, which the adjustment dialog samples
    /// and the JPEG sheet previews.
    fn render(&self, snapshot: &ProjectSnapshot) -> Result<RasterSnapshot, String> {
        let raster = flattened(snapshot)?;
        let width = raster.image.width();
        let height = raster.image.height();
        Ok(RasterSnapshot::new(
            width,
            height,
            Some(PixelImage::Rgba(raster.image.clone())),
            Rect::new(0.0, 0.0, width as f64, height as f64),
            Vec::new(),
            false,
            None,
        ))
    }

    /// `ImageExporter.shared.exportPNG(_:to:)`.
    fn export_png(&self, snapshot: &ProjectSnapshot, to: &Path) -> Result<(), String> {
        let raster = flattened(snapshot)?;
        ImageExporter::export_png(&raster, to).map_err(|error| error.to_string())
    }

    /// `ImageExporter.shared.write(_:to:)`: the already-encoded bytes the JPEG sheet produced.
    fn write_bytes(&self, data: &[u8], to: &Path) -> Result<(), String> {
        ImageExporter::write(data, to).map_err(|error| error.to_string())
    }

    /// `FileManager.default.fileExists(atPath:)`, generalized to a URL.
    fn file_exists(&self, url: &Path) -> bool {
        url.exists()
    }

    /// `ImageImporter.shared.decode(_:remainingPixels:flattenedPhotoshop:)`.
    fn decode_image(&self, url: &Path, remaining_pixels: usize, flattened_photoshop: bool) -> Result<ImportedImage, String> {
        ImageImporter::decode(url, remaining_pixels, flattened_photoshop).map_err(|error| error.to_string())
    }

    /// `ImageImporter.shared.decodeSVG(_:fitting:remainingPixels:)`.
    fn decode_svg(&self, url: &Path, fitting: Option<Size>, remaining_pixels: usize) -> Result<ImportedImage, String> {
        ImageImporter::decode_svg(url, fitting, remaining_pixels).map_err(|error| error.to_string())
    }

    /// `PSDReader.matches(_:)`.
    fn matches_psd(&self, url: &Path) -> bool {
        PSDReader::matches(url)
    }

    /// `ImageImporter.shared.loadPhotoshop`, `photoshopAssets` and `PSDDocumentBuilder.makeImport`,
    /// which the session called in that order.
    fn read_photoshop(&self, url: &Path, remaining_pixels: usize) -> Result<PhotoshopRead, String> {
        let parsed = ImageImporter::load_photoshop(url, remaining_pixels).map_err(|error| error.to_string())?;
        // Only a background: Photoshop wrote no layer records, just the merged image, so that is what
        // comes in, as one layer (`parsed.layers.isEmpty`).
        if parsed.layers.is_empty() {
            return Ok(PhotoshopRead::BackgroundOnly);
        }
        let assets = ImageImporter::photoshop_assets(&parsed);
        let imported = PSDDocumentBuilder::make_import(&parsed, &assets);
        Ok(PhotoshopRead::Imported(Box::new(PhotoshopImport {
            layers: imported.layers,
            width: imported.width,
            height: imported.height,
            resolution: imported.resolution,
            conversions: imported
                .conversions
                .into_iter()
                .map(|conversion| PSDConversion {
                    id: conversion.id,
                    layer_name: conversion.layer_name,
                    message: conversion.message,
                })
                .collect(),
        })))
    }

    /// `RawImporter.matches(_:)`.
    fn matches_raw(&self, url: &Path) -> bool {
        RawImporter::matches(url)
    }

    /// `RawImporter.pixelSize(_:)`.
    fn raw_pixel_size(&self, url: &Path) -> Option<Size> {
        RawImporter::pixel_size(url).map(|(width, height)| Size::new(width as f64, height as f64))
    }

    /// `RawImporter.asShot(_:)`.
    fn raw_as_shot(&self, url: &Path) -> Option<RawDevelopSettings> {
        RawImporter::as_shot(url)
    }

    /// `RawImporter.Queue.shared.develop(_:settings:limit:)`, thumbnail included.
    fn develop_raw(&self, url: &Path, settings: &RawDevelopSettings, limit: Option<usize>) -> Result<ImportedImage, String> {
        let image = Queue::shared()
            .develop(url, settings, limit.map(|limit| limit as f64))
            .ok_or_else(|| ImageImportError::Unreadable.to_string())?;
        let shared: SharedImage = Arc::new(image);
        let thumbnail = thumbnail(&PixelImage::Rgba(shared.clone()), THUMBNAIL_MAX_SIDE);
        Ok(ImportedImage::new(PixelImage::Rgba(shared), thumbnail, file_stem(url)))
    }

    /// `RawImporter.Queue.shared.release()`, which the Swift `finishRawDevelop` called.
    fn release_raw_develop(&self) {
        Queue::shared().release();
    }

    /// `LiveMaskBaker.bake(_:target:)`.
    fn bake_live_mask(&self, snapshot: &ProjectSnapshot, target: Id) -> Result<Option<ImportedImage>, String> {
        LiveMaskBaker::bake(snapshot, target).map_err(|error| error.to_string())
    }
}

/// `ImageExporter.shared.render(_:)`: the canvas composited at its own size, with the document's
/// resolution, ready for the exporters. The `Err` is the message the Swift render threw
/// (`error.localizedDescription`).
fn flattened(snapshot: &ProjectSnapshot) -> Result<ExportRaster, String> {
    let width = snapshot.manifest.width;
    let height = snapshot.manifest.height;
    ImageExporter::check_canvas_size(width, height).map_err(|error| error.to_string())?;
    for layer in &snapshot.manifest.layers {
        if (layer.image_file.is_some() && !snapshot.images.contains_key(&layer.id))
            || (layer.mask_file.is_some() && !snapshot.masks.contains_key(&layer.id))
        {
            return Err(ProjectError::MissingImage.to_string());
        }
    }
    LiveMaskGraph::validate(&snapshot.manifest.layers).map_err(|error| error.to_string())?;
    let document = document_for(snapshot);
    let mut canvas = Canvas::new_rgba(width, height);
    Composite::draw(&document, 1.0, &|point| point, &mut canvas);
    Ok(ExportRaster::with_resolution(Arc::new(canvas.into_rgba()), snapshot.manifest.resolution.unwrap_or(72.0)))
}

/// The document a snapshot describes. `ProjectSnapshot` → layers lives in `install_project` — the
/// mapping an open uses — so borrowing it through a throwaway session renders a snapshot exactly as
/// the canvas it was captured from.
fn document_for(snapshot: &ProjectSnapshot) -> CanvasDocument {
    let mut session = EditorSession::new();
    session.install_project(snapshot, Path::new(""));
    session
        .document
        .take()
        .unwrap_or_else(|| CanvasDocument::new(snapshot.manifest.width, snapshot.manifest.height))
}

/// `url.deletingPathExtension().lastPathComponent`: the name a developed RAW comes in with.
fn file_stem(url: &Path) -> String {
    url.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default()
}
