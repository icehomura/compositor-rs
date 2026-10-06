//! The `.comp` package store: the manifest, the PNG assets beside it and the coordinated atomic
//! replacement that keeps a failed save from touching the live project (Swift
//! `IO/ProjectStore.swift`).
//!
//! A package is a directory holding `manifest.json`, an `images/` directory of `<layer UUID>.png`
//! and `<layer UUID>.mask.png` assets, and — when the exporter supplied one — a `QuickLook/`
//! preview. `load` accepts format versions 1–11; `save` writes 11.
//!
//! `save` validates the manifest and every asset before it touches the file system, stages the whole
//! package in a sibling directory and swaps it into place with renames, so a failed validation or a
//! failed write never replaces the live document. Unsupported versions, invalid metadata, missing
//! assets, unsafe paths and oversized data are all rejected.

use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use compositor_core::blend::LayerBlendMode;
use compositor_core::buffer::{Gray8Image, Rgba8Image};
use compositor_core::document::{ProjectLayerRecord, ProjectManifest};
use compositor_core::geom::Rect;
use compositor_core::groups::LayerHierarchy;
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_adjustment::AdjustmentKind;
use compositor_core::limits;
use compositor_core::raster::THUMBNAIL_MAX_SIDE;
use compositor_core::Id;
use compositor_pixels::canvas::{Canvas, InterpolationQuality};
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::image_exporter::QuickLookImages;
use crate::project_manifest::{image_filename, is_supported, mask_filename};

pub use compositor_core::document::{ProjectError, ProjectSnapshot};

/// The largest `manifest.json` a package may hold.
const MANIFEST_MAXIMUM_BYTES: u64 = 4 * 1024 * 1024;

/// The largest encoded asset a package may hold.
const ASSET_MAXIMUM_BYTES: u64 = 512 * 1024 * 1024;

/// The most layers a document may hold.
const MAXIMUM_LAYERS: usize = 10_000;

/// The most guides a document may hold.
const MAXIMUM_GUIDES: usize = 1_000;

/// A guide sits within this many document pixels of the origin.
const GUIDE_LIMIT: f64 = 1_000_000.0;

/// The most live-mask links a chain may run through.
const LIVE_MASK_CHAIN_LIMIT: usize = 256;

/// A layer name may hold this many UTF-8 bytes.
const MAXIMUM_NAME_BYTES: usize = 16_384;

/// The `.comp` store (`ProjectStore`). Reading and writing are independent of any open document, so
/// the store holds no state of its own.
pub struct ProjectStore;

impl Default for ProjectStore {
    fn default() -> Self {
        ProjectStore
    }
}

impl ProjectStore {
    /// `ProjectStore.shared`.
    pub fn shared() -> &'static ProjectStore {
        static SHARED: LazyLock<ProjectStore> = LazyLock::new(|| ProjectStore);
        &SHARED
    }

    pub fn new() -> Self {
        ProjectStore
    }

    /// `save(_:to:quickLook:)`: validates the snapshot, encodes every PNG the manifest names, and
    /// replaces the package at `url` only once the complete new package has been staged.
    pub fn save(&self, snapshot: &ProjectSnapshot, url: &Path, quick_look: Option<&QuickLookImages>) -> Result<(), ProjectError> {
        validate(&snapshot.manifest)?;
        let mut assets: FxHashMap<String, Vec<u8>> = FxHashMap::default();
        let mut pixels = 0usize;
        let mut mask_pixels = 0usize;
        for layer in &snapshot.manifest.layers {
            for is_mask in [false, true] {
                let filename = match if is_mask { &layer.mask_file } else { &layer.image_file } {
                    Some(filename) => filename,
                    None => continue,
                };
                let Some(asset) = (if is_mask { &snapshot.masks } else { &snapshot.images }).get(&layer.id) else {
                    return Err(ProjectError::MissingImage);
                };
                if is_mask {
                    if !matches!(asset.image, PixelImage::Gray(_)) {
                        return Err(ProjectError::Invalid);
                    }
                    check_size(asset.image.width(), asset.image.height(), &mut mask_pixels)?;
                } else {
                    check_size(asset.image.width(), asset.image.height(), &mut pixels)?;
                }
                assets.insert(filename.clone(), encode_asset(&asset.image)?);
            }
        }
        // Pretty-printed with sorted keys, exactly as `JSONEncoder(.prettyPrinted, .sortedKeys)`
        // wrote the manifest (a `Value` sorts its keys).
        let value = serde_json::to_value(&snapshot.manifest).map_err(|_| ProjectError::Encode)?;
        let metadata = serde_json::to_vec_pretty(&value).map_err(|_| ProjectError::Encode)?;
        if metadata.len() as u64 > MANIFEST_MAXIMUM_BYTES {
            return Err(ProjectError::TooLarge);
        }
        write_package(url, &metadata, &assets, quick_look.map(|look| look.preview.as_slice()))
    }

    /// `load(from:)`: reads and validates a package, decoding every asset it names. The open
    /// document is only replaced by the caller once this returns.
    pub fn load(&self, url: &Path) -> Result<ProjectSnapshot, ProjectError> {
        read_package(url)
    }
}

/// `ProjectStore.validate`: everything a manifest must satisfy before it is written or after it is
/// read. The order of the checks decides which error a manifest with several problems reports.
pub fn validate(manifest: &ProjectManifest) -> Result<(), ProjectError> {
    if manifest.format != ProjectManifest::FORMAT {
        return Err(ProjectError::Invalid);
    }
    if !is_supported(manifest.version) {
        return Err(ProjectError::Version(manifest.version));
    }
    if manifest.color_space != ProjectManifest::COLOR_SPACE {
        return Err(ProjectError::Invalid);
    }
    if let Some(resolution) = manifest.resolution {
        if !resolution.is_finite() || !(1.0..=9600.0).contains(&resolution) {
            return Err(ProjectError::Invalid);
        }
    }
    if !(1..=limits::MAX_SIDE).contains(&manifest.width)
        || !(1..=limits::MAX_SIDE).contains(&manifest.height)
        || manifest.layers.len() > MAXIMUM_LAYERS
    {
        return Err(ProjectError::TooLarge);
    }
    for layer in &manifest.layers {
        if let Some(text) = &layer.text {
            // Per-letter colors arrived in version 10, per-letter faces in version 11.
            let runs_fit = (text.color_runs.is_none() || manifest.version >= 10) && (text.font_runs.is_none() || manifest.version >= 11);
            if !(runs_fit
                && text.is_valid()
                && layer.image_file.is_some()
                && layer.is_group != Some(true)
                && layer.adjustment.is_none())
            {
                return Err(ProjectError::Invalid);
            }
        }
        if let Some(adjustment) = &layer.adjustment {
            if !(manifest.version >= 7 && layer.is_group != Some(true) && layer.image_file.is_none() && adjustment.is_valid())
            {
                return Err(ProjectError::Invalid);
            }
            if is_sampled_adjustment(&adjustment.kind) && manifest.version < 9 {
                return Err(ProjectError::Invalid);
            }
        }
        // Layer masks arrived in version 4, folder masks in version 6.
        let mask_version = if layer.is_group == Some(true) { 6 } else { 4 };
        let mask_file_fits = match &layer.mask_file {
            None => true,
            Some(file) => manifest.version >= mask_version && *file == mask_filename(layer.id),
        };
        let mask_enabled_fits = layer.mask_enabled.is_none() || layer.mask_file.is_some();
        let mask_placement_fits = match &layer.mask_placement {
            None => true,
            Some(placement) => placement.is_valid() && layer.mask_file.is_some(),
        };
        if !(mask_file_fits && mask_enabled_fits && mask_placement_fits) {
            return Err(ProjectError::Invalid);
        }
        let opacity = layer.opacity.unwrap_or(1.0);
        let blend = layer.blend_mode.unwrap_or(LayerBlendMode::Normal);
        // Folders took an opacity of their own in version 8, which multiplies into what is inside
        // them; their blend mode is still pass-through, so it stays Normal.
        let appearance_fits = opacity.is_finite()
            && (0.0..=1.0).contains(&opacity)
            && (manifest.version >= 3 || (opacity == 1.0 && blend == LayerBlendMode::Normal));
        let folder_fits = layer.is_group != Some(true) || (blend == LayerBlendMode::Normal && (manifest.version >= 8 || opacity == 1.0));
        if !(appearance_fits && folder_fits) {
            return Err(ProjectError::Invalid);
        }
    }
    LayerHierarchy::validate(&manifest.layers)?;
    validate_live_mask_graph(&manifest.layers)?;
    if manifest.version < 5 && manifest.layers.iter().any(|layer| layer.mask_source_id.is_some()) {
        return Err(ProjectError::Invalid);
    }
    if manifest.version == 1
        && manifest
            .layers
            .iter()
            .any(|layer| layer.parent_id.is_some() || layer.is_group == Some(true))
    {
        return Err(ProjectError::Invalid);
    }
    let mut ids: FxHashSet<Id> = FxHashSet::default();
    for layer in &manifest.layers {
        if !ids.insert(layer.id)
            || !layer.transform.is_valid()
            || layer.name.trim().is_empty()
            || layer.name.len() > MAXIMUM_NAME_BYTES
        {
            return Err(ProjectError::Invalid);
        }
        if let Some(file) = &layer.image_file {
            if *file != image_filename(layer.id) {
                return Err(ProjectError::Invalid);
            }
        }
    }
    if let Some(id) = manifest.active_layer_id {
        if !ids.contains(&id) {
            return Err(ProjectError::Invalid);
        }
    }
    validate_guides(manifest)
}

/// `validateGuides`: guides arrived in version 8; a version-1–7 file cannot contain one.
fn validate_guides(manifest: &ProjectManifest) -> Result<(), ProjectError> {
    let guides = manifest.guides.as_deref().unwrap_or(&[]);
    if manifest.version < 8 {
        return if guides.is_empty() { Ok(()) } else { Err(ProjectError::Invalid) };
    }
    if guides.len() > MAXIMUM_GUIDES {
        return Err(ProjectError::TooLarge);
    }
    let mut ids: FxHashSet<Id> = FxHashSet::default();
    for guide in guides {
        if !ids.insert(guide.id) || !guide.position.is_finite() || guide.position.abs() > GUIDE_LIMIT {
            return Err(ProjectError::Invalid);
        }
    }
    Ok(())
}

/// `LiveMaskGraph.validate`: a live-mask source must be an existing non-group, non-adjustment
/// layer, no chain may run past 256 links, and no chain may close on itself.
fn validate_live_mask_graph(layers: &[ProjectLayerRecord]) -> Result<(), ProjectError> {
    let mut records: FxHashMap<Id, &ProjectLayerRecord> = FxHashMap::default();
    for layer in layers {
        if records.insert(layer.id, layer).is_some() {
            return Err(ProjectError::Invalid);
        }
    }
    for layer in layers {
        let mut path: FxHashSet<Id> = FxHashSet::default();
        let mut current = Some(layer.id);
        while let Some(id) = current {
            if path.len() >= LIVE_MASK_CHAIN_LIMIT || !path.insert(id) {
                return Err(ProjectError::Invalid);
            }
            let Some(record) = records.get(&id) else {
                return Err(ProjectError::Invalid);
            };
            if let Some(source) = record.mask_source_id {
                let source_fits = record.is_group != Some(true)
                    && records
                        .get(&source)
                        .is_some_and(|source| source.is_group != Some(true) && source.adjustment.is_none());
                if !source_fits {
                    return Err(ProjectError::Invalid);
                }
            }
            current = record.mask_source_id;
        }
    }
    Ok(())
}

/// The three adjustment kinds that sample neighboring pixels; they arrived in version 9.
fn is_sampled_adjustment(kind: &AdjustmentKind) -> bool {
    matches!(kind, AdjustmentKind::GaussianBlur | AdjustmentKind::MotionBlur | AdjustmentKind::AddNoise)
}

/// `checkSize`: one surface at a time, within the side limit and the machine-scaled document budget.
fn check_size(width: usize, height: usize, used: &mut usize) -> Result<(), ProjectError> {
    let budget = limits::document_pixel_budget();
    if !(1..=limits::MAX_SIDE).contains(&width)
        || !(1..=limits::MAX_SIDE).contains(&height)
        || width * height > budget.saturating_sub(*used)
    {
        return Err(ProjectError::TooLarge);
    }
    *used += width * height;
    Ok(())
}

/// `readPackage`: everything inside the package, validated in the original's order — directory,
/// manifest size, identity, version, manifest decode, validation, then each asset.
fn read_package(url: &Path) -> Result<ProjectSnapshot, ProjectError> {
    if !fs::metadata(url).map(|metadata| metadata.is_dir()).unwrap_or(false) {
        return Err(ProjectError::Invalid);
    }
    let metadata_url = url.join("manifest.json");
    check_file(&metadata_url, url, MANIFEST_MAXIMUM_BYTES, CheckedFile::Manifest)?;
    let metadata = fs::read(&metadata_url).map_err(unreadable(CheckedFile::Manifest))?;

    /// The two fields the version gate reads before the whole manifest is decoded.
    #[derive(serde::Deserialize)]
    struct Header {
        format: String,
        version: i32,
    }

    let header: Header = serde_json::from_slice(&metadata).map_err(|_| ProjectError::Invalid)?;
    if header.format != ProjectManifest::FORMAT {
        return Err(ProjectError::Invalid);
    }
    if !is_supported(header.version) {
        return Err(ProjectError::Version(header.version));
    }
    let manifest: ProjectManifest = serde_json::from_slice(&metadata).map_err(|_| ProjectError::Invalid)?;
    validate(&manifest)?;

    let mut images: std::collections::HashMap<Id, ImportedImage> = std::collections::HashMap::new();
    let mut masks: std::collections::HashMap<Id, ImportedImage> = std::collections::HashMap::new();
    let mut pixels = 0usize;
    let mut mask_pixels = 0usize;
    for layer in &manifest.layers {
        for is_mask in [false, true] {
            let filename = match if is_mask { &layer.mask_file } else { &layer.image_file } {
                Some(filename) => filename,
                None => continue,
            };
            let file = url.join("images").join(filename);
            check_file(&file, url, ASSET_MAXIMUM_BYTES, CheckedFile::Asset)?;
            let bytes = fs::read(&file).map_err(unreadable(CheckedFile::Asset))?;
            let (image, thumbnail) = decode_asset(&bytes, is_mask, &mut pixels, &mut mask_pixels)?;
            let asset = ImportedImage::new(image, thumbnail, layer.name.clone());
            if is_mask {
                masks.insert(layer.id, asset);
            } else {
                images.insert(layer.id, asset);
            }
        }
    }
    Ok(ProjectSnapshot { manifest, images, masks })
}

/// Which package file a check is about; an unreadable one reports the error the original's
/// exception would eventually reach the reader as.
#[derive(Clone, Copy)]
enum CheckedFile {
    Manifest,
    Asset,
}

fn unreadable(kind: CheckedFile) -> impl Fn(std::io::Error) -> ProjectError {
    move |error| {
        log::debug!("project package file is unreadable: {error}");
        match kind {
            CheckedFile::Manifest => ProjectError::Invalid,
            CheckedFile::Asset => ProjectError::MissingImage,
        }
    }
}

/// `checkFile`: the file must live inside the package once symlinks are resolved, be a regular file
/// that is not itself a symlink, and be no larger than `maximum_bytes`.
fn check_file(file: &Path, package: &Path, maximum_bytes: u64, kind: CheckedFile) -> Result<u64, ProjectError> {
    let root = fs::canonicalize(package).map_err(unreadable(kind))?;
    let resolved = fs::canonicalize(file).map_err(unreadable(kind))?;
    if !resolved.starts_with(&root) {
        return Err(ProjectError::Invalid);
    }
    let metadata = fs::symlink_metadata(file).map_err(unreadable(kind))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum_bytes {
        return Err(ProjectError::TooLarge);
    }
    Ok(metadata.len())
}

/// The pixels of one PNG asset and the thumbnail its panel row shows, decoded from the file's bytes
/// in memory — an image made from a file source stays tied to it, and the next save replaces that
/// file, so an image kept for undo could otherwise read someone else's pixels.
///
/// Layer images are RGBA8; masks are 8-bit grayscale without alpha. The manifest's `checkSize` runs
/// between the header and the pixels, exactly as the original checked the source's properties first.
fn decode_asset(
    bytes: &[u8],
    is_mask: bool,
    pixels: &mut usize,
    mask_pixels: &mut usize,
) -> Result<(PixelImage, PixelImage), ProjectError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(if is_mask {
        png::Transformations::EXPAND
    } else {
        png::Transformations::EXPAND | png::Transformations::ALPHA
    });
    let mut reader = decoder.read_info().map_err(|_| ProjectError::MissingImage)?;
    let (width, height, bit_depth) = {
        let info = reader.info();
        (info.width as usize, info.height as usize, info.bit_depth)
    };
    // ImageIO refused anything deeper than eight bits per component.
    if bit_depth == png::BitDepth::Sixteen {
        return Err(ProjectError::MissingImage);
    }
    if is_mask {
        check_size(width, height, mask_pixels)?;
    } else {
        check_size(width, height, pixels)?;
    }
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let output = reader.next_frame(&mut buffer).map_err(|_| ProjectError::MissingImage)?;
    buffer.truncate(output.buffer_size());

    let image = if is_mask {
        // `LayerMask.isValid`: 8-bit grayscale with no alpha channel.
        if output.color_type != png::ColorType::Grayscale || output.bit_depth != png::BitDepth::Eight {
            return Err(ProjectError::Invalid);
        }
        PixelImage::Gray(Arc::new(Gray8Image::from_data(width, height, buffer)))
    } else {
        let mut rgba = rgba8(&buffer, output.color_type)?;
        // PNG stores straight alpha; the app's pixels are premultiplied.
        premultiply(&mut rgba);
        PixelImage::Rgba(Arc::new(Rgba8Image::from_data(width, height, rgba)))
    };
    let thumbnail = thumbnail(&image);
    Ok((image, thumbnail))
}

/// Any decoded PNG layout as straight RGBA8, the conversion ImageIO performed when it built the
/// app's bitmap.
fn rgba8(buffer: &[u8], color_type: png::ColorType) -> Result<Vec<u8>, ProjectError> {
    Ok(match color_type {
        png::ColorType::Rgba => buffer.to_vec(),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(buffer.len() / 3 * 4);
            for pixel in buffer.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(buffer.len() * 2);
            for pixel in buffer.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(buffer.len() * 4);
            for value in buffer.iter() {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
            rgba
        }
        png::ColorType::Indexed => return Err(ProjectError::MissingImage),
    })
}

/// Straight RGBA8 as the canonical premultiplied pixels (`ImageIO` did this when it decoded).
fn premultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for component in &mut pixel[..3] {
            *component = ((u32::from(*component) * alpha + 127) / 255) as u8;
        }
    }
}

/// The canonical premultiplied pixels as straight RGBA8 for PNG (`ImageIO` did this when it encoded).
fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for component in &mut pixel[..3] {
            *component = ((u32::from(*component) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

/// A thumbnail no larger than 96 pixels on its longest side, drawn with the same high interpolation
/// quality `CGImageSourceCreateThumbnailAtIndex` and `LayerMask.asset` used.
fn thumbnail(image: &PixelImage) -> PixelImage {
    let (width, height) = (image.width(), image.height());
    let factor = 1.0_f64.min(THUMBNAIL_MAX_SIDE / width.max(height).max(1) as f64);
    let thumb_width = ((width as f64 * factor) as usize).max(1);
    let thumb_height = ((height as f64 * factor) as usize).max(1);
    let rect = Rect::new(0.0, 0.0, thumb_width as f64, thumb_height as f64);
    match image {
        PixelImage::Rgba(rgba) => {
            let mut canvas = Canvas::new_rgba(thumb_width, thumb_height);
            canvas.set_interpolation_quality(InterpolationQuality::High);
            canvas.draw_image(rgba, rect);
            PixelImage::Rgba(Arc::new(canvas.into_rgba()))
        }
        PixelImage::Gray(gray) => {
            let mut canvas = Canvas::new_gray(thumb_width, thumb_height);
            canvas.set_interpolation_quality(InterpolationQuality::High);
            // Coverage drawn in white keeps a mask gray: white reveals, black hides.
            canvas.set_fill_gray(1.0);
            canvas.draw_coverage(gray, rect);
            PixelImage::Gray(Arc::new(canvas.into_gray()))
        }
    }
}

/// One asset as the PNG a package stores: RGBA8 for a layer (straight alpha, as PNG requires) and
/// 8-bit grayscale for a mask.
fn encode_asset(image: &PixelImage) -> Result<Vec<u8>, ProjectError> {
    match image {
        PixelImage::Rgba(rgba) => {
            let mut bytes = rgba.data().to_vec();
            unpremultiply(&mut bytes);
            encode_png(rgba.width(), rgba.height(), png::ColorType::Rgba, &bytes)
        }
        PixelImage::Gray(gray) => encode_png(gray.width(), gray.height(), png::ColorType::Grayscale, gray.data()),
    }
}

fn encode_png(width: usize, height: usize, color: png::ColorType, data: &[u8]) -> Result<Vec<u8>, ProjectError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width as u32, height as u32);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| ProjectError::Encode)?;
        writer.write_image_data(data).map_err(|_| ProjectError::Encode)?;
        writer.finish().map_err(|_| ProjectError::Encode)?;
    }
    Ok(bytes)
}

/// Writes the complete package into a sibling staging directory and only then swaps it into place,
/// so a failure at any point leaves the destination exactly as it was.
fn write_package(url: &Path, metadata: &[u8], assets: &FxHashMap<String, Vec<u8>>, preview: Option<&[u8]>) -> Result<(), ProjectError> {
    let parent = match url.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = url
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or(ProjectError::Encode)?;
    // The package is staged beside its destination, as Foundation did; a missing folder is a write
    // failure, not something to create silently.
    if !parent.is_dir() {
        log::debug!("the project package's folder {} does not exist", parent.display());
        return Err(ProjectError::Encode);
    }
    let staging = parent.join(format!(".{name}.staging-{}", Uuid::new_v4()));
    if let Err(error) = stage_package(&staging, metadata, assets, preview) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = swap_into_place(&staging, url, parent, &name) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(())
}

fn stage_package(staging: &Path, metadata: &[u8], assets: &FxHashMap<String, Vec<u8>>, preview: Option<&[u8]>) -> Result<(), ProjectError> {
    let images = staging.join("images");
    fs::create_dir_all(&images).map_err(|error| stage_error(&error))?;
    fs::write(staging.join("manifest.json"), metadata).map_err(|error| stage_error(&error))?;
    for (filename, data) in assets {
        fs::write(images.join(filename), data).map_err(|error| stage_error(&error))?;
    }
    // Quick Look's Space-bar preview reads this by name; loading ignores it.
    if let Some(preview) = preview {
        let quick_look = staging.join("QuickLook");
        fs::create_dir_all(&quick_look).map_err(|error| stage_error(&error))?;
        fs::write(quick_look.join("Preview.jpg"), preview).map_err(|error| stage_error(&error))?;
    }
    Ok(())
}

fn stage_error(error: &std::io::Error) -> ProjectError {
    log::debug!("staging a project package failed: {error}");
    ProjectError::Encode
}

/// The rename swap `NSFileCoordinator(.forReplacing)` plus Foundation's atomic package write gave:
/// the old package is moved aside, the staged one takes its place, and the old one comes back if
/// that fails.
fn swap_into_place(staging: &Path, url: &Path, parent: &Path, name: &str) -> Result<(), ProjectError> {
    let backup = parent.join(format!(".{name}.backup-{}", Uuid::new_v4()));
    let existing = fs::symlink_metadata(url).is_ok();
    if existing {
        fs::rename(url, &backup).map_err(|error| stage_error(&error))?;
    }
    match fs::rename(staging, url) {
        Ok(()) => {
            if existing {
                if let Err(error) = fs::remove_dir_all(&backup) {
                    log::warn!("the replaced project package at {} was left behind: {error}", backup.display());
                }
            }
            Ok(())
        }
        Err(error) => {
            log::error!("replacing the project package at {} failed: {error}", url.display());
            if existing {
                if let Err(restore) = fs::rename(&backup, url) {
                    log::error!("restoring the previous project package at {} failed: {restore}", url.display());
                }
            }
            Err(ProjectError::Encode)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::buffer::{Gray8Image, Rgba8Image};
    use compositor_core::geom::{Point, Size};
    use compositor_core::guides::{CanvasGuide, CanvasGuideAxis};
    use compositor_core::layer_adjustment::LayerAdjustment;
    use compositor_core::layer_effects::{LayerEffects, StrokeEffect};
    use compositor_core::layer_shape::{LayerShapeStyle, ShapeKind};
    use compositor_core::layer_text::{LayerTextColorRun, LayerTextFontRun, LayerTextStyle};
    use compositor_core::layer_transform::{LayerSampling, LayerTransform};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// A scratch directory under the system temporary directory, removed when the test ends.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Scratch {
            let path = std::env::temp_dir().join(format!("compositor-io-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("a scratch directory");
            Scratch { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        /// Every leftover staging or backup directory of a package named `name`.
        fn leftovers(&self, name: &str) -> Vec<String> {
            fs::read_dir(&self.path)
                .expect("the scratch directory")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".staging-") || name.contains(".backup-"))
                .collect::<Vec<_>>()
                .into_iter()
                .filter(|leftover| leftover.contains(name))
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn transform(origin: Point, size: Size) -> LayerTransform {
        LayerTransform {
            origin,
            size,
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: LayerSampling::High,
        }
    }

    /// A layer record with only the fields every version carries.
    fn plain(id: Id, name: &str) -> ProjectLayerRecord {
        ProjectLayerRecord {
            id,
            name: name.to_string(),
            is_visible: true,
            transform: transform(Point::ZERO, Size::new(4.0, 4.0)),
            image_file: Some(image_filename(id)),
            parent_id: None,
            is_group: None,
            opacity: None,
            blend_mode: None,
            mask_file: None,
            mask_enabled: None,
            mask_source_id: None,
            adjustment: None,
            mask_placement: None,
            mask_linked: None,
            shape: None,
            effects: None,
            text: None,
        }
    }

    fn adjustment(kind: AdjustmentKind) -> LayerAdjustment {
        LayerAdjustment::new(kind)
    }

    /// An adjustment record: no image, no group.
    fn adjustment_layer(id: Id, kind: AdjustmentKind) -> ProjectLayerRecord {
        ProjectLayerRecord {
            image_file: None,
            adjustment: Some(adjustment(kind)),
            ..plain(id, "Adjustment")
        }
    }

    fn rgba_asset(image: Rgba8Image, name: &str) -> ImportedImage {
        ImportedImage::new(
            PixelImage::Rgba(Arc::new(image)),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(1, 1))),
            name,
        )
    }

    fn gray_asset(image: Gray8Image, name: &str) -> ImportedImage {
        ImportedImage::new(
            PixelImage::Gray(Arc::new(image)),
            PixelImage::Gray(Arc::new(Gray8Image::new(1, 1))),
            name,
        )
    }

    /// `PixelImage` has no `PartialEq` of its own (the rasters are shared, not copied), so the tests
    /// compare kind, size and bytes.
    fn same_pixels(lhs: &PixelImage, rhs: &PixelImage) -> bool {
        lhs.width() == rhs.width()
            && lhs.height() == rhs.height()
            && match (lhs, rhs) {
                (PixelImage::Rgba(lhs), PixelImage::Rgba(rhs)) => lhs.data() == rhs.data(),
                (PixelImage::Gray(lhs), PixelImage::Gray(rhs)) => lhs.data() == rhs.data(),
                _ => false,
            }
    }

    /// The project the store tests round-trip: groups, masks, a live mask, an adjustment layer,
    /// effects, text with per-letter runs, a shape and guides.
    fn project_snapshot() -> ProjectSnapshot {
        let document_id = Id::new_v4();
        let base_id = Id::new_v4();
        let group_id = Id::new_v4();
        let child_id = Id::new_v4();
        let text_id = Id::new_v4();
        let shape_id = Id::new_v4();
        let adjustment_id = Id::new_v4();

        let base = plain(base_id, "Base");
        let mut group = plain(group_id, "Folder 1");
        group.image_file = None;
        group.is_group = Some(true);
        group.opacity = Some(0.5);
        let mut child = plain(child_id, "Child");
        child.parent_id = Some(group_id);
        child.opacity = Some(0.75);
        child.blend_mode = Some(LayerBlendMode::Multiply);
        child.mask_file = Some(mask_filename(child_id));
        child.mask_enabled = Some(false);
        child.mask_source_id = Some(base_id);
        child.mask_placement = Some(transform(Point::new(1.0, 2.0), Size::new(4.0, 4.0)));
        child.mask_linked = Some(false);

        let mut text = plain(text_id, "Type");
        let mut style = LayerTextStyle { content: "Hello".to_string(), ..LayerTextStyle::default() };
        style.box_size = Some(Size::new(200.0, 100.0));
        style.color_runs = Some(vec![LayerTextColorRun { location: 0, length: 2, red: 1.0, green: 0.0, blue: 0.0 }]);
        style.font_runs = Some(vec![LayerTextFontRun { location: 2, length: 3, font_name: "Helvetica-Bold".to_string() }]);
        text.text = Some(style);

        let mut shape = plain(shape_id, "Shape");
        shape.shape = Some(LayerShapeStyle {
            kind: ShapeKind::Rectangle,
            red: 0.0,
            green: 0.5,
            blue: 1.0,
            corner_radius: 8.0,
            line_width: None,
            start: None,
            end: None,
        });
        shape.effects = Some(LayerEffects {
            stroke: Some(StrokeEffect { enabled: Some(false), ..StrokeEffect::default() }),
            ..Default::default()
        });

        let mut manifest = ProjectManifest::new(
            document_id,
            4,
            4,
            Some(child_id),
            vec![
                base,
                group,
                child,
                text,
                shape,
                adjustment_layer(adjustment_id, AdjustmentKind::Invert),
            ],
        );
        manifest.resolution = Some(300.0);
        manifest.guides = Some(vec![
            CanvasGuide::new(Id::new_v4(), CanvasGuideAxis::Horizontal, 120.0),
            CanvasGuide::new(Id::new_v4(), CanvasGuideAxis::Vertical, -8.0),
        ]);

        let mut images: HashMap<Id, ImportedImage> = HashMap::new();
        // Canonical premultiplied pixels: alpha 255 and a semi-transparent pixel that survives the
        // straight/premultiplied conversion exactly.
        let mut base_pixels = Rgba8Image::new(4, 4);
        base_pixels.set(0, 0, [255, 0, 0, 255]);
        base_pixels.set(1, 0, [64, 0, 0, 128]);
        base_pixels.set(2, 2, [10, 20, 30, 255]);
        images.insert(base_id, rgba_asset(base_pixels, "Base"));
        images.insert(child_id, rgba_asset(Rgba8Image::new(4, 4), "Child"));
        images.insert(text_id, rgba_asset(Rgba8Image::new(4, 4), "Type"));
        images.insert(shape_id, rgba_asset(Rgba8Image::new(4, 4), "Shape"));

        let mut masks: HashMap<Id, ImportedImage> = HashMap::new();
        masks.insert(child_id, gray_asset(Gray8Image::uniform(1, 1, 128), "Child"));

        ProjectSnapshot {
            manifest,
            images,
            masks,
        }
    }

    fn assert_saved_then_loaded(url: &Path, snapshot: &ProjectSnapshot) -> ProjectSnapshot {
        ProjectStore::shared().save(snapshot, url, None).expect("the project saves");
        ProjectStore::shared().load(url).expect("the project loads")
    }

    /// A document with groups, masks, a live mask, an adjustment layer, effects, text and guides
    /// survives a save and a load, pixels and all.
    #[test]
    fn save_and_load_round_trip_a_project() {
        let scratch = Scratch::new();
        let url = scratch.join("Round Trip.comp");
        let snapshot = project_snapshot();
        let loaded = assert_saved_then_loaded(&url, &snapshot);

        assert_eq!(loaded.manifest, snapshot.manifest);
        assert_eq!(loaded.images.len(), snapshot.images.len());
        assert_eq!(loaded.masks.len(), snapshot.masks.len());
        for (id, asset) in &snapshot.images {
            assert!(
                same_pixels(&loaded.images[id].image, &asset.image),
                "layer {id} pixels survive the PNG round trip"
            );
        }
        for (id, asset) in &snapshot.masks {
            assert!(
                same_pixels(&loaded.masks[id].image, &asset.image),
                "mask {id} pixels survive the PNG round trip"
            );
        }

        // The assets are the canonical files, named after their layers.
        assert!(url.join("manifest.json").is_file());
        assert!(url.join("images").join(image_filename(snapshot.manifest.layers[0].id)).is_file());
        assert!(url.join("images").join(mask_filename(snapshot.manifest.layers[2].id)).is_file());
        assert!(!url.join("QuickLook").exists());

        // Thumbnails are 96-pixel-bounded and keep the asset's kind.
        let base = &loaded.images[&snapshot.manifest.layers[0].id];
        assert!(base.image.width() <= 96 && base.image.height() <= 96);
        assert!(base.thumbnail.width() <= 96 && base.thumbnail.height() <= 96);
        assert!(loaded.masks[&snapshot.manifest.layers[2].id].thumbnail.is_mask());
    }

    /// Quick Look's Space-bar preview is written when the exporter supplied one, and only then.
    #[test]
    fn save_writes_the_quick_look_preview_it_is_given() {
        let scratch = Scratch::new();
        let url = scratch.join("Preview.comp");
        let snapshot = project_snapshot();
        let preview = QuickLookImages { preview: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3] };
        ProjectStore::shared()
            .save(&snapshot, &url, Some(&preview))
            .expect("the project saves");
        assert_eq!(fs::read(url.join("QuickLook").join("Preview.jpg")).expect("the preview"), preview.preview);

        // Replacing the package without a preview drops the folder with the old package.
        ProjectStore::shared().save(&snapshot, &url, None).expect("the project saves again");
        assert!(!url.join("QuickLook").exists());
    }

    /// A failed save validates first and never replaces the live package.
    #[test]
    fn a_failed_save_never_replaces_the_live_package() {
        let scratch = Scratch::new();
        let url = scratch.join("Live.comp");
        let snapshot = project_snapshot();
        ProjectStore::shared().save(&snapshot, &url, None).expect("the first save lands");
        let live_manifest = fs::read(url.join("manifest.json")).expect("the live manifest");
        let live_image = fs::read(url.join("images").join(image_filename(snapshot.manifest.layers[0].id)))
            .expect("the live asset");

        // A mask file that does not name its layer: validation refuses the whole save.
        let mut broken = project_snapshot();
        broken.manifest.layers[2].mask_file = Some("elsewhere.png".to_string());
        assert!(matches!(
            ProjectStore::shared().save(&broken, &url, None),
            Err(ProjectError::Invalid)
        ));

        // A layer whose asset is missing: the save stops before writing anything.
        let mut missing = project_snapshot();
        missing.images.clear();
        assert!(matches!(
            ProjectStore::shared().save(&missing, &url, None),
            Err(ProjectError::MissingImage)
        ));

        // A mask that is not gray: invalid.
        let mut wrong_mask = project_snapshot();
        let child_id = wrong_mask.manifest.layers[2].id;
        wrong_mask.masks.insert(child_id, rgba_asset(Rgba8Image::new(1, 1), "Child"));
        assert!(matches!(
            ProjectStore::shared().save(&wrong_mask, &url, None),
            Err(ProjectError::Invalid)
        ));

        // The live package is exactly as it was, with no staging or backup left behind.
        assert_eq!(fs::read(url.join("manifest.json")).expect("the live manifest"), live_manifest);
        assert_eq!(
            fs::read(url.join("images").join(image_filename(snapshot.manifest.layers[0].id))).expect("the live asset"),
            live_image
        );
        assert!(scratch.leftovers("Live.comp").is_empty(), "{:?}", scratch.leftovers("Live.comp"));
    }

    /// The package stores canonical PNGs: RGBA8 with straight alpha for a layer, 8-bit grayscale
    /// for a mask, no metadata, and a layer's premultiplied pixel converted on the way out and back.
    #[test]
    fn written_assets_are_canonical_pngs() {
        let scratch = Scratch::new();
        let url = scratch.join("Layouts.comp");
        let snapshot = project_snapshot();
        ProjectStore::shared().save(&snapshot, &url, None).expect("the project saves");

        let layer_file = fs::read(url.join("images").join(image_filename(snapshot.manifest.layers[0].id)))
            .expect("the layer PNG");
        let (color, depth, width, height, bytes) = read_png(&layer_file);
        assert_eq!((color, depth), (png::ColorType::Rgba, png::BitDepth::Eight));
        assert_eq!((width, height), (4, 4));
        // The canonical premultiplied pixels [255, 0, 0, 255] and [64, 0, 0, 128] are stored at
        // straight alpha, so the half-transparent red reads 128, not 64.
        assert_eq!(bytes[0..4], [255, 0, 0, 255]);
        assert_eq!(bytes[4..8], [128, 0, 0, 128]);

        let mask_file = fs::read(url.join("images").join(mask_filename(snapshot.manifest.layers[2].id)))
            .expect("the mask PNG");
        let (color, depth, width, height, bytes) = read_png(&mask_file);
        assert_eq!((color, depth), (png::ColorType::Grayscale, png::BitDepth::Eight));
        assert_eq!((width, height), (1, 1));
        assert_eq!(bytes, vec![128]);
    }

    /// A PNG's header and pixels, read without transformations.
    fn read_png(bytes: &[u8]) -> (png::ColorType, png::BitDepth, usize, usize, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("a PNG");
        let (color, depth, width, height) = {
            let info = reader.info();
            (info.color_type, info.bit_depth, info.width as usize, info.height as usize)
        };
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let output = reader.next_frame(&mut buffer).expect("PNG pixels");
        buffer.truncate(output.buffer_size());
        (color, depth, width, height, buffer)
    }

    /// A damaged package is refused before anything is decoded.
    #[test]
    fn load_rejects_damaged_packages() {
        let scratch = Scratch::new();
        let store = ProjectStore::shared();

        // Not a package at all.
        assert!(matches!(store.load(&scratch.join("Missing.comp")), Err(ProjectError::Invalid)));
        let file = scratch.join("File.comp");
        fs::write(&file, b"not a package").expect("a file");
        assert!(matches!(store.load(&file), Err(ProjectError::Invalid)));

        // A package without a manifest.
        let empty = scratch.join("Empty.comp");
        fs::create_dir_all(empty.join("images")).expect("an empty package");
        assert!(matches!(store.load(&empty), Err(ProjectError::Invalid)));

        // A manifest that names another format, or a version this app cannot read.
        let snapshot = project_snapshot();
        let url = scratch.join("Version.comp");
        store.save(&snapshot, &url, None).expect("the project saves");
        let manifest = fs::read_to_string(url.join("manifest.json")).expect("the manifest");
        for (from, to) in [("\"format\": \"com.compositor.project\"", "\"format\": \"com.example.other\""),
                           ("\"version\": 11", "\"version\": 12")] {
            let broken = manifest.replacen(from, to, 1);
            assert_ne!(broken, manifest, "the fixture spells {from}");
            fs::write(url.join("manifest.json"), broken).expect("a damaged manifest");
            let error = store.load(&url).expect_err("a damaged manifest is refused");
            match to {
                "\"format\": \"com.example.other\"" => assert!(matches!(error, ProjectError::Invalid)),
                _ => assert!(matches!(error, ProjectError::Version(12))),
            }
        }
        fs::write(url.join("manifest.json"), &manifest).expect("the manifest is restored");

        // A missing asset.
        let base = image_filename(snapshot.manifest.layers[0].id);
        let asset = url.join("images").join(&base);
        let bytes = fs::read(&asset).expect("the asset");
        fs::remove_file(&asset).expect("the asset is removed");
        assert!(matches!(store.load(&url), Err(ProjectError::MissingImage)));
        fs::write(&asset, &bytes).expect("the asset is restored");

        // An asset that is not a PNG.
        fs::write(&asset, b"definitely not a png").expect("a damaged asset");
        assert!(matches!(store.load(&url), Err(ProjectError::MissingImage)));
        fs::write(&asset, &bytes).expect("the asset is restored");

        // An unsafe image file name, caught by validation before any file is opened.
        let unsafe_manifest = manifest.replacen(&base, "../outside.png", 1);
        assert_ne!(unsafe_manifest, manifest);
        fs::write(url.join("manifest.json"), unsafe_manifest).expect("an unsafe manifest");
        assert!(matches!(store.load(&url), Err(ProjectError::Invalid)));
        fs::write(url.join("manifest.json"), &manifest).expect("the manifest is restored");

        // A manifest over the 4 MiB ceiling is refused as too large.
        let big = scratch.join("Big.comp");
        store.save(&snapshot, &big, None).expect("the project saves");
        let mut padded = project_snapshot();
        let mut layers = Vec::new();
        for _ in 0..300 {
            let mut record = plain(Id::new_v4(), "Layer");
            record.name = "x".repeat(16_000);
            // Blank layers hold no asset, so only the manifest's own size can fail here.
            record.image_file = None;
            layers.push(record);
        }
        padded.manifest.active_layer_id = Some(layers[0].id);
        padded.manifest.layers = layers;
        padded.images.clear();
        padded.masks.clear();
        assert!(matches!(store.save(&padded, &big, None), Err(ProjectError::TooLarge)));
        assert!(matches!(store.load(&big).map(|snapshot| snapshot.manifest.layers.len()), Ok(6)));
    }

    /// The size ceilings `checkSize` and `checkFile` apply: side limits, the machine-scaled
    /// document budget, the manifest's 4 MiB and an asset's 512 MiB, and paths inside the package.
    #[test]
    fn size_and_path_ceilings_are_enforced() {
        let mut used = 0usize;
        check_size(1, 1, &mut used).expect("a pixel fits");
        assert_eq!(used, 1);
        assert!(matches!(check_size(0, 1, &mut used), Err(ProjectError::TooLarge)));
        assert!(matches!(check_size(1, limits::MAX_SIDE + 1, &mut used), Err(ProjectError::TooLarge)));
        let mut full = limits::document_pixel_budget();
        assert!(matches!(check_size(2, 1, &mut full), Err(ProjectError::TooLarge)));
        let mut nearly_full = limits::document_pixel_budget() - 1;
        check_size(1, 1, &mut nearly_full).expect("the last pixel fits");
        assert!(matches!(check_size(1, 1, &mut nearly_full), Err(ProjectError::TooLarge)));

        let scratch = Scratch::new();
        let url = scratch.join("Ceilings.comp");
        let snapshot = project_snapshot();
        ProjectStore::shared().save(&snapshot, &url, None).expect("the project saves");
        let manifest = url.join("manifest.json");
        let asset = url.join("images").join(image_filename(snapshot.manifest.layers[0].id));
        assert!(matches!(check_file(&manifest, &url, 1, CheckedFile::Manifest), Err(ProjectError::TooLarge)));
        assert!(matches!(check_file(&asset, &url, 1, CheckedFile::Asset), Err(ProjectError::TooLarge)));
        assert!(check_file(&asset, &url, ASSET_MAXIMUM_BYTES, CheckedFile::Asset).is_ok());
        // Anything outside the package is refused before its size is even considered.
        let outside = scratch.join("outside.png");
        fs::write(&outside, b"x").expect("a file outside the package");
        assert!(matches!(check_file(&outside, &url, ASSET_MAXIMUM_BYTES, CheckedFile::Asset), Err(ProjectError::Invalid)));
        // A directory is not an asset, whatever its size.
        assert!(matches!(check_file(&url.join("images"), &url, ASSET_MAXIMUM_BYTES, CheckedFile::Asset), Err(ProjectError::TooLarge)));
    }

    /// Validation refuses impossible layer trees, oversized documents and version-gated fields.
    #[test]
    fn validation_rejects_invalid_manifests() {
        let valid = project_snapshot().manifest;
        assert!(validate(&valid).is_ok(), "the fixture is valid");

        let mut manifest = valid.clone();
        manifest.format = "com.example.other".to_string();
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.version = 12;
        assert!(matches!(validate(&manifest), Err(ProjectError::Version(12))));
        let mut manifest = valid.clone();
        manifest.version = 0;
        assert!(matches!(validate(&manifest), Err(ProjectError::Version(0))));
        let mut manifest = valid.clone();
        manifest.color_space = "Adobe RGB".to_string();
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.resolution = Some(0.0);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.resolution = Some(f64::NAN);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.resolution = Some(9601.0);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));

        // Side limits, the layer ceiling and the guide ceiling are "too large".
        let mut manifest = valid.clone();
        manifest.width = 0;
        assert!(matches!(validate(&manifest), Err(ProjectError::TooLarge)));
        let mut manifest = valid.clone();
        manifest.height = limits::MAX_SIDE + 1;
        assert!(matches!(validate(&manifest), Err(ProjectError::TooLarge)));
        let mut manifest = valid.clone();
        manifest.layers = vec![plain(Id::new_v4(), "Layer"); 10_001];
        assert!(matches!(validate(&manifest), Err(ProjectError::TooLarge)));
        let mut manifest = valid.clone();
        manifest.guides = Some(vec![CanvasGuide::at(CanvasGuideAxis::Horizontal, 0.0); 1_001]);
        assert!(matches!(validate(&manifest), Err(ProjectError::TooLarge)));
        let mut manifest = valid.clone();
        manifest.guides = Some(vec![CanvasGuide::at(CanvasGuideAxis::Vertical, 1_000_001.0)]);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        let repeated = CanvasGuide::at(CanvasGuideAxis::Vertical, 4.0);
        manifest.guides = Some(vec![repeated; 2]);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));

        // Names, files and identity.
        let mut manifest = valid.clone();
        manifest.layers[0].name = "   \n".to_string();
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[0].name = "x".repeat(16_385);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[0].image_file = Some("base.png".to_string());
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[0].transform.size = Size::new(0.0, 4.0);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.active_layer_id = Some(Id::new_v4());
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        let duplicate = manifest.layers[0].clone();
        manifest.layers.push(duplicate);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[2].mask_file = Some(mask_filename(Id::new_v4()));
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[0].mask_enabled = Some(true);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[0].mask_placement = Some(transform(Point::ZERO, Size::new(4.0, 4.0)));
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));

        // The layer tree: dangling, non-group and cyclic parents, folders with pixels, deep nesting.
        let base_id = valid.layers[0].id;
        let group_id = valid.layers[1].id;
        let mut manifest = valid.clone();
        manifest.layers[2].parent_id = Some(Id::new_v4());
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[2].parent_id = Some(base_id);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[1].parent_id = Some(group_id);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        manifest.layers[1].image_file = Some(image_filename(group_id));
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        let mut layers = Vec::new();
        for _ in 0..66 {
            let id = Id::new_v4();
            let mut group = plain(id, "Folder");
            group.image_file = None;
            group.is_group = Some(true);
            group.parent_id = layers.last().map(|layer: &ProjectLayerRecord| layer.id);
            layers.push(group);
        }
        manifest.layers = layers;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));

        // Live masks: a source that is a group, and a self-link.
        let mut manifest = valid.clone();
        manifest.layers[2].mask_source_id = Some(group_id);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));
        let mut manifest = valid.clone();
        let child_id = manifest.layers[2].id;
        manifest.layers[2].mask_source_id = Some(child_id);
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)));

        // Adjustment layers and text cannot share a record, and an adjustment has no image.
        let mut manifest = valid.clone();
        manifest.layers[3].adjustment = Some(adjustment(AdjustmentKind::Invert));
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "a text layer cannot also be an adjustment");
        let mut manifest = valid.clone();
        let adjustment_id = manifest.layers[5].id;
        manifest.layers[5].image_file = Some(image_filename(adjustment_id));
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "an adjustment layer has no image");
        let mut manifest = valid.clone();
        manifest.layers[1].text = Some(LayerTextStyle::default());
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "a group cannot carry text");

        // Version gates, one version below each feature's arrival. The fixtures carry exactly the
        // fields their version allows, so lowering the version isolates one gate at a time.
        let mut manifest = fixture(7);
        manifest.version = 6;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "adjustment layers arrived in 7");
        let mut manifest = fixture(9);
        manifest.version = 8;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "blurs arrived in 9");
        let mut manifest = fixture(10);
        manifest.version = 9;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "per-letter colors arrived in 10");
        let mut manifest = fixture(11);
        manifest.version = 10;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "per-letter faces arrived in 11");
        let mut manifest = fixture(3);
        manifest.version = 2;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "opacity and blend modes arrived in 3");
        let mut manifest = fixture(4);
        manifest.version = 3;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "masks arrived in 4");
        let mut manifest = fixture(5);
        manifest.version = 4;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "live masks arrived in 5");
        let mut manifest = fixture(6);
        manifest.version = 5;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "folder masks arrived in 6");
        let mut manifest = fixture(8);
        manifest.version = 7;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "guides arrived in 8");
        let mut manifest = fixture(2);
        manifest.version = 1;
        assert!(matches!(validate(&manifest), Err(ProjectError::Invalid)), "folders arrived in 2");
    }

    /// Every format version 1–11 decodes, validates and re-encodes to the same bytes.
    #[test]
    fn version_fixtures_decode_for_1_through_11() {
        for version in 1..=11 {
            let manifest = fixture(version);
            let bytes = serde_json::to_vec_pretty(&manifest).expect("a fixture serializes");
            let decoded: ProjectManifest = serde_json::from_slice(&bytes).expect("a fixture reads back");
            assert_eq!(decoded, manifest, "version {version} round-trips");
            assert!(validate(&decoded).is_ok(), "version {version} validates: {:?}", validate(&decoded).err());
        }
    }

    /// A manifest that uses exactly the fields its version allows.
    fn fixture(version: i32) -> ProjectManifest {
        let document_id = Id::new_v4();
        let base_id = Id::new_v4();
        let mut base = plain(base_id, "Base");

        let group_id = Id::new_v4();
        let child_id = Id::new_v4();
        let mut group = plain(group_id, "Folder 1");
        group.image_file = None;
        let mut child = plain(child_id, "Child");
        if version >= 2 {
            group.is_group = Some(true);
            child.parent_id = Some(group_id);
        }
        if version >= 3 {
            base.blend_mode = Some(LayerBlendMode::Multiply);
            child.opacity = Some(0.75);
            child.blend_mode = Some(LayerBlendMode::Screen);
        }
        if version >= 4 {
            child.mask_file = Some(mask_filename(child_id));
            child.mask_enabled = Some(true);
        }
        if version >= 5 {
            child.mask_source_id = Some(base_id);
        }
        if version >= 6 {
            group.mask_file = Some(mask_filename(group_id));
            group.mask_enabled = Some(false);
        }
        if version >= 8 {
            group.opacity = Some(0.5);
        }
        if version >= 10 {
            let mut style = LayerTextStyle { content: "Hello".to_string(), ..LayerTextStyle::default() };
            style.color_runs = Some(vec![LayerTextColorRun { location: 0, length: 2, red: 0.0, green: 1.0, blue: 0.0 }]);
            if version >= 11 {
                style.font_runs = Some(vec![LayerTextFontRun { location: 2, length: 3, font_name: "Helvetica-Bold".to_string() }]);
            }
            base.text = Some(style);
        }

        let mut layers = vec![base, group, child];
        if version >= 7 {
            let kind = if version >= 9 { AdjustmentKind::GaussianBlur } else { AdjustmentKind::Invert };
            layers.push(adjustment_layer(Id::new_v4(), kind));
        }
        let mut manifest = ProjectManifest::new(document_id, 8, 8, Some(child_id), layers);
        manifest.resolution = Some(300.0);
        if version >= 8 {
            manifest.guides = Some(vec![CanvasGuide::new(Id::new_v4(), CanvasGuideAxis::Horizontal, 40.0)]);
        }
        manifest.version = version;
        manifest
    }
}
