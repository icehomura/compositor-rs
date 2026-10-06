//! The PSD reader's value types: failures, the conversion report, the parsed document and its layer
//! records, ported from `IO/PSD/PSDTypes.swift`.

use compositor_rs_core::blend::LayerBlendMode;
use compositor_rs_core::buffer::{SharedGray, SharedImage};
use compositor_rs_core::geom::Rect;
use compositor_rs_core::imported_image::ImageImportError;
use compositor_rs_core::layer_adjustment::LayerAdjustment;
use compositor_rs_core::layer_shape::LayerShapeStyle;
use uuid::Uuid;

use crate::psd::text;

/// `PSDError`: the reader's own failures. Each message is the user-facing text the Swift localized
/// description produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PSDError {
    Truncated,
    UnsupportedVersion,
    UnsupportedColorMode,
    UnsupportedDepth,
    UnsupportedCompression,
}

impl std::fmt::Display for PSDError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PSDError::Truncated => {
                formatter.write_str("The Photoshop file could not be read. It may be damaged or incomplete.")
            }
            PSDError::UnsupportedVersion => {
                formatter.write_str("This Photoshop file uses a format version Compositor can’t read.")
            }
            PSDError::UnsupportedColorMode => {
                formatter.write_str("Only 8-bit RGB Photoshop files can be imported.")
            }
            PSDError::UnsupportedDepth => {
                formatter.write_str("Only 8-bit RGB Photoshop files can be imported.")
            }
            PSDError::UnsupportedCompression => {
                formatter.write_str("This Photoshop file uses a layer compression method that isn’t supported.")
            }
        }
    }
}

impl std::error::Error for PSDError {}

/// Everything `PSDReader::read` can report: a PSD failure or an import-level one (`ImageImportError`),
/// with the same user-facing text the Swift error carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PSDReadError {
    PSD(PSDError),
    Import(ImageImportError),
}

impl std::fmt::Display for PSDReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PSDReadError::PSD(error) => error.fmt(formatter),
            PSDReadError::Import(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PSDReadError {}

impl From<PSDError> for PSDReadError {
    fn from(error: PSDError) -> Self {
        PSDReadError::PSD(error)
    }
}

impl From<ImageImportError> for PSDReadError {
    fn from(error: ImageImportError) -> Self {
        PSDReadError::Import(error)
    }
}

/// A note about what an import changed: which layer, and what happened to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PSDConversion {
    pub id: Uuid,
    pub layer_name: String,
    pub message: String,
}

impl PSDConversion {
    pub fn new(layer_name: impl Into<String>, message: impl Into<String>) -> Self {
        PSDConversion { id: Uuid::new_v4(), layer_name: layer_name.into(), message: message.into() }
    }

    pub fn with_id(id: Uuid, layer_name: impl Into<String>, message: impl Into<String>) -> Self {
        PSDConversion { id, layer_name: layer_name.into(), message: message.into() }
    }
}

/// The parsed file: the canvas, its resolution and its records.
#[derive(Clone, Debug)]
pub struct PSDDocument {
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    /// Bottom to top, including folders. Hidden section dividers are not stored.
    pub layers: Vec<PSDRecord>,
}

/// The Photoshop layer types the reader recognizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PSDLayerKind {
    Raster,
    Group,
    Adjustment,
    Text,
    SmartObject,
    Effects,
    Vector,
    Other,
}

/// One parsed layer record, before it becomes an `ImageLayer`.
#[derive(Clone, Debug)]
pub struct PSDRecord {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub is_group: bool,
    pub is_visible: bool,
    pub opacity: f64,
    pub blend_key: String,
    pub clipping: bool,
    pub cropped_to_canvas: bool,
    pub bounds: Rect,
    pub image: Option<SharedImage>,
    pub mask: Option<SharedGray>,
    /// Where `mask` sits on the document, and the value everywhere outside it: Photoshop stores only the part of a
    /// mask that isn't that default.
    pub mask_bounds: Rect,
    pub mask_default: u8,
    pub mask_enabled: bool,
    pub mask_linked: bool,
    pub adjustment: Option<LayerAdjustment>,
    pub kind: PSDLayerKind,
    pub shape: Option<LayerShapeStyle>,
    pub shape_notes: Vec<String>,
    /// Parsed Photoshop type, when the `TySh` block maps onto an editable text layer.
    pub text: Option<text::Source>,
}

impl Default for PSDRecord {
    fn default() -> Self {
        PSDRecord {
            id: Uuid::nil(),
            parent_id: None,
            name: String::new(),
            is_group: false,
            is_visible: true,
            opacity: 1.0,
            blend_key: "norm".to_string(),
            clipping: false,
            cropped_to_canvas: false,
            bounds: Rect::ZERO,
            image: None,
            mask: None,
            mask_bounds: Rect::ZERO,
            mask_default: 255,
            mask_enabled: true,
            mask_linked: true,
            adjustment: None,
            kind: PSDLayerKind::Raster,
            shape: None,
            shape_notes: Vec::new(),
            text: None,
        }
    }
}

impl PSDRecord {
    pub fn new(id: Uuid, name: impl Into<String>) -> Self {
        PSDRecord { id, name: name.into(), ..Default::default() }
    }

    /// `PSDRecord.blendMode`: the record's blend key mapped onto Compositor's blend modes.
    pub fn blend_mode(&self) -> Option<LayerBlendMode> {
        psd_blend_mode(&self.blend_key)
    }
}

/// `LayerBlendMode.fromPSD(_:)`.
pub fn psd_blend_mode(key: &str) -> Option<LayerBlendMode> {
    match key {
        "norm" => Some(LayerBlendMode::Normal),
        "mul " => Some(LayerBlendMode::Multiply),
        "scrn" => Some(LayerBlendMode::Screen),
        "over" => Some(LayerBlendMode::Overlay),
        "sLit" => Some(LayerBlendMode::SoftLight),
        "dark" => Some(LayerBlendMode::Darken),
        "lite" => Some(LayerBlendMode::Lighten),
        "diff" => Some(LayerBlendMode::Difference),
        "div " => Some(LayerBlendMode::ColorDodge),
        "idiv" => Some(LayerBlendMode::ColorBurn),
        "hue " => Some(LayerBlendMode::Hue),
        "sat " => Some(LayerBlendMode::Saturation),
        "colr" => Some(LayerBlendMode::Color),
        "lum " => Some(LayerBlendMode::Luminosity),
        "lbrn" => Some(LayerBlendMode::LinearBurn),
        "lddg" => Some(LayerBlendMode::LinearDodge),
        "hLit" => Some(LayerBlendMode::HardLight),
        "vLit" => Some(LayerBlendMode::VividLight),
        "lLit" => Some(LayerBlendMode::LinearLight),
        "pLit" => Some(LayerBlendMode::PinLight),
        "hMix" => Some(LayerBlendMode::HardMix),
        "smud" => Some(LayerBlendMode::Exclusion),
        "fsub" => Some(LayerBlendMode::Subtract),
        "fdiv" => Some(LayerBlendMode::Divide),
        // Dissolve, Darker Color and Lighter Color are deliberately absent: Compositor has no
        // equivalent, so they fall through to Normal and say so in the conversion report.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_keys_map_like_the_swift_table() {
        assert_eq!(psd_blend_mode("norm"), Some(LayerBlendMode::Normal));
        assert_eq!(psd_blend_mode("mul "), Some(LayerBlendMode::Multiply));
        assert_eq!(psd_blend_mode("idiv"), Some(LayerBlendMode::ColorBurn));
        assert_eq!(psd_blend_mode("fdiv"), Some(LayerBlendMode::Divide));
        // Deliberately absent: they fall through to Normal, with a report note.
        assert_eq!(psd_blend_mode("diss"), None);
        assert_eq!(psd_blend_mode("dkCl"), None);
        assert_eq!(psd_blend_mode("lgCl"), None);
        assert_eq!(psd_blend_mode("pass"), None);
        let mut record = PSDRecord::new(Uuid::nil(), "Layer");
        record.blend_key = "scrn".to_string();
        assert_eq!(record.blend_mode(), Some(LayerBlendMode::Screen));
    }

    #[test]
    fn error_text_matches_the_swift_descriptions() {
        assert_eq!(
            PSDError::Truncated.to_string(),
            "The Photoshop file could not be read. It may be damaged or incomplete."
        );
        assert_eq!(
            PSDError::UnsupportedVersion.to_string(),
            "This Photoshop file uses a format version Compositor can’t read."
        );
        assert_eq!(PSDError::UnsupportedColorMode.to_string(), "Only 8-bit RGB Photoshop files can be imported.");
        assert_eq!(PSDError::UnsupportedDepth.to_string(), "Only 8-bit RGB Photoshop files can be imported.");
        assert_eq!(
            PSDError::UnsupportedCompression.to_string(),
            "This Photoshop file uses a layer compression method that isn’t supported."
        );
        assert_eq!(
            PSDReadError::Import(ImageImportError::TooLarge).to_string(),
            ImageImportError::TooLarge.to_string()
        );
    }
}
