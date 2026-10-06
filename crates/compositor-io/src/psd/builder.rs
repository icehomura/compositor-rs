//! Turns parsed PSD records into `ImageLayer`s and the conversion report, ported from
//! `IO/PSD/PSDDocumentBuilder.swift`.

use std::sync::Arc;

use compositor_core::blend::LayerBlendMode;
use compositor_core::buffer::Gray8Image;
use compositor_core::document::ImageLayer;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_shape::LayerShape;
use compositor_core::layer_text::LayerText;
use compositor_core::layer_transform::LayerTransform;
use compositor_pixels::adjustments;
use compositor_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_pixels::raster::Raster;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::psd::text as psd_text;
use crate::psd::types::{PSDConversion, PSDDocument, PSDLayerKind, PSDRecord};

/// The imported PSD, ready to become a document.
pub struct PSDImport {
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    pub layers: Vec<ImageLayer>,
    pub conversions: Vec<PSDConversion>,
}

/// The document builder: assets, layers, masks and the conversion report.
pub struct PSDDocumentBuilder;

impl PSDDocumentBuilder {
    pub fn assets(document: &PSDDocument) -> FxHashMap<Uuid, ImportedImage> {
        let mut result: FxHashMap<Uuid, ImportedImage> = FxHashMap::default();
        for record in &document.layers {
            let Some(image) = &record.image else { continue };
            result.insert(record.id, imported(image, &record.name));
        }
        result
    }

    pub fn make_import(document: &PSDDocument, assets: &FxHashMap<Uuid, ImportedImage>) -> PSDImport {
        let mut conversions: Vec<PSDConversion> = Vec::new();
        let mut layers: Vec<ImageLayer> = Vec::new();
        let canvas = Size::new(document.width as f64, document.height as f64);
        for record in &document.layers {
            if record.cropped_to_canvas {
                conversions.push(PSDConversion::new(
                    &record.name,
                    "Cropped to the canvas so the file fits in memory. Pixels outside the canvas weren't imported.",
                ));
            }
            let rendered_text = record.text.as_ref().and_then(|source| psd_text::render(source).ok());
            let mut notes: Vec<String> = Vec::new();
            if record.kind == PSDLayerKind::Text {
                match (&record.text, &rendered_text) {
                    (Some(source), Some(_)) => {
                        notes.extend(source.notes.iter().cloned());
                        if let Some(missing) = psd_text::missing_font_note(&source.style.font_name) {
                            notes.push(missing);
                        }
                    }
                    _ => notes.push(psd_text::RASTERIZED_NOTE.to_string()),
                }
            }
            if record.kind == PSDLayerKind::SmartObject {
                notes.push("The smart object was rasterized. Linked contents can’t be edited.".to_string());
            }
            if record.kind == PSDLayerKind::Effects {
                notes.push("Layer effects were discarded, so the appearance may differ.".to_string());
            }
            if record.kind == PSDLayerKind::Vector {
                if record.shape.is_some() {
                    notes.extend(record.shape_notes.iter().cloned());
                } else {
                    notes.push("Vector shape was rasterized to pixels.".to_string());
                }
            }
            if record.kind == PSDLayerKind::Other {
                notes.push("This Photoshop layer type isn’t supported and was imported as pixels.".to_string());
            }
            if record.is_group {
                if record.blend_key != "pass" && record.blend_key != "norm" {
                    notes.push(format!(
                        "Folder blend mode “{}” isn’t supported. The folder will be pass-through.",
                        record.blend_key
                    ));
                }
            } else if record.blend_mode().is_none() && record.blend_key != "pass" {
                notes.push(format!(
                    "Blend mode “{}” isn’t supported and will be applied as Normal.",
                    record.blend_key.trim()
                ));
            }
            if record.kind == PSDLayerKind::Adjustment {
                if record.adjustment.is_none() {
                    notes.push("This adjustment type isn’t supported and was skipped.".to_string());
                } else {
                    notes.push("Adjustment parameters may not match Photoshop exactly.".to_string());
                }
            }
            for note in notes {
                conversions.push(PSDConversion::new(&record.name, note));
            }
            if record.kind == PSDLayerKind::Adjustment && record.adjustment.is_none() {
                continue;
            }
            let opacity = record.opacity.clamp(0.0, 1.0);
            let mut layer: ImageLayer;
            if record.is_group {
                // Folders carry an opacity of their own (1.1.6), which multiplies into what's inside
                // them just as Photoshop's group opacity does.
                layer = ImageLayer::with_id(
                    record.id,
                    None,
                    record.name.clone(),
                    record.is_visible,
                    LayerTransform { origin: Point::ZERO, size: canvas, ..Default::default() },
                    record.parent_id,
                    true,
                    opacity,
                    LayerBlendMode::Normal,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                );
            } else if let Some(adjustment) = &record.adjustment {
                layer = ImageLayer::with_id(
                    record.id,
                    None,
                    record.name.clone(),
                    record.is_visible,
                    LayerTransform { origin: Point::ZERO, size: canvas, ..Default::default() },
                    record.parent_id,
                    false,
                    opacity,
                    record.blend_mode().unwrap_or(LayerBlendMode::Normal),
                    None,
                    None,
                    Some(adjustment.clone()),
                    None,
                    None,
                    None,
                );
            } else if let (Some((image, transform)), Some(source)) = (&rendered_text, &record.text) {
                let asset = imported(image, &record.name);
                layer = ImageLayer::with_id(
                    record.id,
                    Some(asset),
                    record.name.clone(),
                    record.is_visible,
                    *transform,
                    record.parent_id,
                    false,
                    opacity,
                    record.blend_mode().unwrap_or(LayerBlendMode::Normal),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(LayerText { style: source.style.clone(), image: image.clone() }),
                );
            } else if let Some(image) = &record.image {
                let asset = assets.get(&record.id).cloned().unwrap_or_else(|| imported(image, &record.name));
                let origin = Point::new(record.bounds.min_x(), record.bounds.min_y());
                let size = if record.bounds.size.width > 0.0 && record.bounds.size.height > 0.0 {
                    record.bounds.size
                } else {
                    Size::new(image.width() as f64, image.height() as f64)
                };
                layer = ImageLayer::with_id(
                    record.id,
                    Some(asset),
                    record.name.clone(),
                    record.is_visible,
                    LayerTransform { origin, size, ..Default::default() },
                    record.parent_id,
                    false,
                    opacity,
                    record.blend_mode().unwrap_or(LayerBlendMode::Normal),
                    None,
                    None,
                    None,
                    record.shape.as_ref().map(|style| LayerShape { style: style.clone(), image: image.clone() }),
                    None,
                    None,
                );
            } else {
                layer = ImageLayer::with_id(
                    record.id,
                    None,
                    record.name.clone(),
                    record.is_visible,
                    LayerTransform { origin: Point::ZERO, size: canvas, ..Default::default() },
                    record.parent_id,
                    false,
                    opacity,
                    record.blend_mode().unwrap_or(LayerBlendMode::Normal),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                );
            }
            if let Some(mask) = &record.mask {
                if let Some(grid) = Self::mask_on_layer_grid(mask, record, &layer, canvas) {
                    if let Ok(asset) = LayerMask::asset(PixelImage::Gray(Arc::new(grid))) {
                        layer.mask = Some(LayerMask::with_placement(asset, record.mask_enabled, None, record.mask_linked));
                    } else {
                        conversions.push(PSDConversion::new(
                            &record.name,
                            "The layer mask couldn’t be converted to 8-bit grayscale and was skipped.",
                        ));
                    }
                } else {
                    conversions.push(PSDConversion::new(
                        &record.name,
                        "The layer mask couldn’t be converted to 8-bit grayscale and was skipped.",
                    ));
                }
            }
            layers.push(layer);
        }

        let id_to_index: FxHashMap<Uuid, usize> =
            layers.iter().enumerate().map(|(index, layer)| (layer.id, index)).collect();
        let mut base_for_parent: FxHashMap<Option<Uuid>, Uuid> = FxHashMap::default();
        for record in &document.layers {
            let Some(index) = id_to_index.get(&record.id).copied() else { continue };
            if record.clipping {
                let base = base_for_parent.get(&record.parent_id).copied();
                let valid = base.is_some_and(|source| {
                    layers.iter().any(|layer| layer.id == source && !layer.is_group && layer.adjustment.is_none())
                });
                match base {
                    Some(source) if valid => layers[index].mask_source_id = Some(source),
                    _ => conversions.push(PSDConversion::new(
                        &record.name,
                        "This clipping mask’s base isn’t supported, so clipping was skipped.",
                    )),
                }
            } else if layers
                .get(index)
                .is_some_and(|layer| !layer.is_group && layer.adjustment.is_none())
            {
                base_for_parent.insert(record.parent_id, record.id);
            } else {
                base_for_parent.remove(&record.parent_id);
            }
        }

        PSDImport { width: document.width, height: document.height, resolution: document.resolution, layers, conversions }
    }

    /// A PSD layer mask on the layer's own pixel grid, as Compositor's masks are: the stored patch drawn where it sits
    /// on the document, and Photoshop's default value everywhere else. The patch alone, stretched over the layer, would
    /// put the mask in the wrong place. Adjustment layers and folders cover the canvas.
    pub fn mask_on_layer_grid(patch: &Gray8Image, record: &PSDRecord, layer: &ImageLayer, canvas: Size) -> Option<Gray8Image> {
        let grid = match &layer.asset {
            Some(asset) => Size::new(asset.image.width() as f64, asset.image.height() as f64),
            None => canvas,
        };
        let placed = Rect::from_origin_size(layer.transform.origin, layer.transform.size);
        if grid.width < 1.0
            || grid.height < 1.0
            || placed.width() <= 0.0
            || placed.height() <= 0.0
            || record.mask_bounds.width() <= 0.0
            || record.mask_bounds.height() <= 0.0
        {
            return Some(patch.clone());
        }
        let scale_x = grid.width / placed.width();
        let scale_y = grid.height / placed.height();
        let rect = Rect::new(
            (record.mask_bounds.min_x() - placed.min_x()) * scale_x,
            (record.mask_bounds.min_y() - placed.min_y()) * scale_y,
            record.mask_bounds.width() * scale_x,
            record.mask_bounds.height() * scale_y,
        );
        let width = grid.width as usize;
        let height = grid.height as usize;
        // Already the layer's grid: nothing to place.
        if rect.integral() == Rect::from_origin_size(Point::ZERO, grid) && patch.width() == width && patch.height() == height {
            return Some(patch.clone());
        }
        let mut target = Canvas::new_gray(width, height);
        target.set_fill_gray(record.mask_default as f64 / 255.0);
        target.fill_rect(Rect::new(0.0, 0.0, width as f64, height as f64));
        target.set_interpolation_quality(InterpolationQuality::None);
        Raster::draw(&PixelImage::Gray(Arc::new(patch.clone())), rect, true, &mut target);
        Some(target.into_gray())
    }
}

/// `PixelAdjust.thumbnail(of:)` for the asset's pixels: a preview no larger than 96 px.
fn imported(image: &compositor_core::SharedImage, name: &str) -> ImportedImage {
    let thumbnail = adjustments::thumbnail(image);
    ImportedImage::new(PixelImage::Rgba(image.clone()), PixelImage::Rgba(Arc::new(thumbnail)), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::buffer::Rgba8Image;

    fn raster_record(name: &str, bounds: Rect) -> PSDRecord {
        let width = bounds.width().max(1.0) as usize;
        let height = bounds.height().max(1.0) as usize;
        let mut record = PSDRecord::new(Uuid::new_v4(), name);
        record.bounds = bounds;
        record.image = Some(Arc::new(Rgba8Image::from_data(width, height, vec![128u8; 4 * width * height])));
        record
    }

    #[test]
    fn records_become_layers_with_their_bounds_blend_and_opacity() {
        let mut record = raster_record("Photo", Rect::new(10.0, 20.0, 4.0, 2.0));
        record.blend_key = "mul ".to_string();
        record.opacity = 0.5;
        let document = PSDDocument { width: 8, height: 8, resolution: 300.0, layers: vec![record] };
        let import = PSDDocumentBuilder::make_import(&document, &FxHashMap::default());
        assert_eq!((import.width, import.height), (8, 8));
        assert_eq!(import.resolution, 300.0);
        assert!(import.conversions.is_empty());
        assert_eq!(import.layers.len(), 1);
        let layer = &import.layers[0];
        assert_eq!(layer.name, "Photo");
        assert_eq!(layer.transform.origin, Point::new(10.0, 20.0));
        assert_eq!(layer.transform.size, Size::new(4.0, 2.0));
        assert_eq!(layer.blend_mode, LayerBlendMode::Multiply);
        assert_eq!(layer.opacity, 0.5);
        let asset = layer.asset.as_ref().expect("asset");
        assert_eq!(asset.name, "Photo");
        assert!(matches!(asset.image, PixelImage::Rgba(_)));
        assert!(layer.mask.is_none());
    }

    #[test]
    fn a_mask_patch_is_placed_on_the_layers_own_grid() {
        let mut record = raster_record("Masked", Rect::new(10.0, 20.0, 4.0, 2.0));
        record.mask = Some(Arc::new(Gray8Image::from_data(2, 2, vec![0, 0, 0, 0])));
        record.mask_bounds = Rect::new(12.0, 20.0, 2.0, 2.0);
        record.mask_default = 255;
        let document = PSDDocument { width: 8, height: 8, resolution: 72.0, layers: vec![record] };
        let import = PSDDocumentBuilder::make_import(&document, &FxHashMap::default());
        assert!(import.conversions.is_empty());
        let layer = &import.layers[0];
        let mask = layer.mask.as_ref().expect("mask");
        assert!(mask.is_enabled && mask.is_linked);
        let gray = mask.asset.image.as_gray().expect("gray mask");
        assert_eq!((gray.width(), gray.height()), (4, 2));
        // Outside the stored patch Photoshop's default value shows: the patch covers x 2..4 of
        // both rows, so the left half reads the default.
        assert_eq!(gray.get(0, 0), 255);
        assert_eq!(gray.get(1, 1), 255);
        // The patch lands where it sat on the document.
        assert_eq!(gray.get(2, 0), 0);
        assert_eq!(gray.get(3, 0), 0);
        assert_eq!(gray.get(3, 1), 0);
    }

    #[test]
    fn unsupported_adjustments_are_reported_and_skipped() {
        let mut record = PSDRecord::new(Uuid::new_v4(), "Levels");
        record.kind = PSDLayerKind::Adjustment;
        record.bounds = Rect::from_origin_size(Point::ZERO, Size::new(8.0, 8.0));
        let document = PSDDocument { width: 8, height: 8, resolution: 72.0, layers: vec![record] };
        let import = PSDDocumentBuilder::make_import(&document, &FxHashMap::default());
        assert!(import.layers.is_empty());
        assert_eq!(import.conversions.len(), 1);
        assert_eq!(import.conversions[0].layer_name, "Levels");
        assert_eq!(import.conversions[0].message, "This adjustment type isn’t supported and was skipped.");
    }

    #[test]
    fn unsupported_blend_modes_fall_back_to_normal_with_a_note() {
        let mut record = raster_record("Dissolved", Rect::new(0.0, 0.0, 2.0, 2.0));
        record.blend_key = "diss".to_string();
        let document = PSDDocument { width: 4, height: 4, resolution: 72.0, layers: vec![record] };
        let import = PSDDocumentBuilder::make_import(&document, &FxHashMap::default());
        let layer = &import.layers[0];
        assert_eq!(layer.blend_mode, LayerBlendMode::Normal);
        assert_eq!(import.conversions.len(), 1);
        assert_eq!(import.conversions[0].message, "Blend mode “diss” isn’t supported and will be applied as Normal.");
    }
}
