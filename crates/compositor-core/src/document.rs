//! The document model: `ImageLayer`, `CanvasDocument` and the navigation tools, ported from
//! `Document/EditorSession.swift`, plus the flat layer record the project manifest stores
//! (`IO/ProjectStore.swift`), which `groups::LayerHierarchy` reads and validates.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use crate::blend::LayerBlendMode;
use crate::geom::{Point, Size};
use crate::guides::CanvasGuide;
use crate::imported_image::{ImportedImage, PixelImage};
use crate::layer_adjustment::LayerAdjustment;
use crate::layer_effects::LayerEffects;
use crate::layer_mask::LayerMask;
use crate::layer_shape::{LayerShape, LayerShapeStyle};
use crate::layer_text::{LayerText, LayerTextStyle};
use crate::layer_transform::{LayerSampling, LayerTransform};
use crate::limits::MAX_SIDE;
use crate::selection::DocumentSelection;
use crate::Id;

/// `LayerTransform(origin:size:)` — the Swift memberwise initializer's defaults: upright, unflipped,
/// high-quality sampling.
pub(crate) fn transform_at(origin: Point, size: Size) -> LayerTransform {
    LayerTransform {
        origin,
        size,
        rotation: 0.0,
        flip_x: false,
        flip_y: false,
        sampling: LayerSampling::High,
    }
}

/// Swift's `lhs.asset?.image === rhs.asset?.image`: the two shared rasters are the same allocation.
pub(crate) fn pixels_identical(lhs: &PixelImage, rhs: &PixelImage) -> bool {
    match (lhs, rhs) {
        (PixelImage::Rgba(a), PixelImage::Rgba(b)) => Arc::ptr_eq(a, b),
        (PixelImage::Gray(a), PixelImage::Gray(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// One layer of the document: an image, a blank canvas, a folder, an adjustment, a shape or a text.
///
/// Equality mirrors Swift's `ImageLayer.==`, which compares the asset's *image* by identity rather
/// than by value (the pixels are shared, not copied).
///
/// The JSON key spellings are the manifest's (`isVisible`, `parentID`, `maskSourceID`, …), but the
/// fields that carry pixels — `asset`, `mask`, `shape`, `text` — are skipped: the project stores
/// those as PNGs under `images/`, named by [`ProjectLayerRecord`], which is the record the manifest
/// actually reads and writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLayer {
    pub id: Id,
    /// The layer's pixels. Nil on a blank layer until painting begins, and on folders and adjustments.
    #[serde(skip)]
    pub asset: Option<ImportedImage>,
    pub transform: LayerTransform,
    pub name: String,
    pub is_visible: bool,
    #[serde(skip_serializing_if = "Option::is_none", rename = "parentID")]
    pub parent_id: Option<Id>,
    pub is_group: bool,
    pub opacity: f64,
    pub blend_mode: LayerBlendMode,
    #[serde(skip_serializing_if = "Option::is_none", rename = "maskSourceID")]
    pub mask_source_id: Option<Id>,
    #[serde(skip)]
    pub mask: Option<LayerMask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment: Option<LayerAdjustment>,
    /// Set on layers the Shape tool made; see `liveShape`.
    #[serde(skip)]
    pub shape: Option<LayerShape>,
    /// A stroke and drop shadow drawn around the layer, kept apart from its pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<LayerEffects>,
    #[serde(skip)]
    pub text: Option<LayerText>,
}

impl PartialEq for ImageLayer {
    /// Swift's custom `==`: the asset's image is compared by identity, everything else by value.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.is_visible == other.is_visible
            && self.transform == other.transform
            && same_asset(&self.asset, &other.asset)
            && self.parent_id == other.parent_id
            && self.is_group == other.is_group
            && self.opacity == other.opacity
            && self.blend_mode == other.blend_mode
            && self.mask == other.mask
            && self.mask_source_id == other.mask_source_id
            && self.adjustment == other.adjustment
            && self.shape == other.shape
            && self.text == other.text
            && self.effects == other.effects
    }
}

/// `lhs.asset?.image === rhs.asset?.image`: nil on both sides compares equal.
fn same_asset(lhs: &Option<ImportedImage>, rhs: &Option<ImportedImage>) -> bool {
    match (lhs, rhs) {
        (None, None) => true,
        (Some(a), Some(b)) => pixels_identical(&a.image, &b.image),
        _ => false,
    }
}

impl ImageLayer {
    /// `init(asset:origin:)`: places an imported image at `origin`, sized to the image itself.
    pub fn from_asset(asset: ImportedImage, origin: Point) -> Self {
        let name = asset.name.clone();
        let size = Size::new(asset.image.width() as f64, asset.image.height() as f64);
        ImageLayer {
            id: crate::new_id(),
            asset: Some(asset),
            transform: transform_at(origin, size),
            name,
            is_visible: true,
            parent_id: None,
            is_group: false,
            opacity: 1.0,
            blend_mode: LayerBlendMode::Normal,
            mask_source_id: None,
            mask: None,
            adjustment: None,
            shape: None,
            effects: None,
            text: None,
        }
    }

    /// `init(name:blankSize:)`: an empty layer. Pixels are allocated when painting begins, not when
    /// the layer is added.
    pub fn blank(name: impl Into<String>, blank_size: Size) -> Self {
        ImageLayer {
            id: crate::new_id(),
            asset: None,
            transform: transform_at(Point::ZERO, blank_size),
            name: name.into(),
            is_visible: true,
            parent_id: None,
            is_group: false,
            opacity: 1.0,
            blend_mode: LayerBlendMode::Normal,
            mask_source_id: None,
            mask: None,
            adjustment: None,
            shape: None,
            effects: None,
            text: None,
        }
    }

    /// `init(id:asset:name:isVisible:transform:parentID:isGroup:opacity:blendMode:mask:maskSourceID:adjustment:shape:effects:text:)`
    /// — the memberwise initializer every other caller uses, with the Swift default values spelled out.
    #[allow(clippy::too_many_arguments)]
    pub fn with_id(
        id: Id,
        asset: Option<ImportedImage>,
        name: String,
        is_visible: bool,
        transform: LayerTransform,
        parent_id: Option<Id>,
        is_group: bool,
        opacity: f64,
        blend_mode: LayerBlendMode,
        mask: Option<LayerMask>,
        mask_source_id: Option<Id>,
        adjustment: Option<LayerAdjustment>,
        shape: Option<LayerShape>,
        effects: Option<LayerEffects>,
        text: Option<LayerText>,
    ) -> Self {
        ImageLayer {
            id,
            asset,
            transform,
            name,
            is_visible,
            parent_id,
            is_group,
            opacity,
            blend_mode,
            mask_source_id,
            mask,
            adjustment,
            shape,
            effects,
            text,
        }
    }

    /// The layer's top-left corner in document pixels.
    pub fn origin(&self) -> Point {
        self.transform.origin
    }

    /// The layer's unrotated bounds.
    pub fn size(&self) -> Size {
        self.transform.size
    }
}

/// The open canvas and its layers (`CanvasDocument`).
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasDocument {
    pub id: Id,
    pub width: usize,
    pub height: usize,
    /// Pixels per inch, 1–9600; 72 unless Image Size changed it.
    pub resolution: f64,
    /// Bottom to top.
    pub layers: Vec<ImageLayer>,
    /// User-placed alignment lines. Saved with the project; undo covers them.
    pub guides: Vec<CanvasGuide>,
    /// Part of the document so undo/redo covers selection changes. Not saved to disk.
    pub selection: Option<DocumentSelection>,
}

impl CanvasDocument {
    /// `init(width:height:)` with the Swift defaults: a fresh id, no layers, 72 pixels/inch.
    pub fn new(width: usize, height: usize) -> Self {
        CanvasDocument {
            id: crate::new_id(),
            width,
            height,
            resolution: 72.0,
            layers: Vec::new(),
            guides: Vec::new(),
            selection: None,
        }
    }

    /// `init(id:width:height:layers:resolution:guides:)` — the load path.
    pub fn with_id(
        id: Id,
        width: usize,
        height: usize,
        layers: Vec<ImageLayer>,
        resolution: f64,
        guides: Vec<CanvasGuide>,
    ) -> Self {
        CanvasDocument {
            id,
            width,
            height,
            resolution,
            layers,
            guides,
            selection: None,
        }
    }

    /// The canvas in document pixels.
    pub fn size(&self) -> Size {
        Size::new(self.width as f64, self.height as f64)
    }

    /// Geometry limit; raster memory limits are established with image import. The text field's
    /// surrounding whitespace is ignored, and anything outside `1...DocumentLimits.maxSide` is not a
    /// dimension.
    pub fn valid_dimension(value: &str) -> Option<usize> {
        let dimension: i64 = value.trim().parse().ok()?;
        if (1..=MAX_SIDE as i64).contains(&dimension) {
            Some(dimension as usize)
        } else {
            None
        }
    }
}

/// Every tool the rail can select (`NavigationTool`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NavigationTool {
    Move,
    Marquee,
    Lasso,
    Wand,
    Crop,
    Brush,
    SpotHealing,
    CloneStamp,
    Blur,
    Gradient,
    Shape,
    Type,
    Eyedropper,
    Hand,
    Zoom,
    /// No tool (A): nothing in the tool rail is selected and canvas clicks do nothing.
    Idle,
}

impl NavigationTool {
    /// `CaseIterable` order: the rail's order, with the idle tool last.
    pub const ALL: [NavigationTool; 16] = [
        NavigationTool::Move,
        NavigationTool::Marquee,
        NavigationTool::Lasso,
        NavigationTool::Wand,
        NavigationTool::Crop,
        NavigationTool::Brush,
        NavigationTool::SpotHealing,
        NavigationTool::CloneStamp,
        NavigationTool::Blur,
        NavigationTool::Gradient,
        NavigationTool::Shape,
        NavigationTool::Type,
        NavigationTool::Eyedropper,
        NavigationTool::Hand,
        NavigationTool::Zoom,
        NavigationTool::Idle,
    ];

    /// The raw string is the case name, as Swift's `String` raw values are.
    pub fn raw_value(self) -> &'static str {
        match self {
            NavigationTool::Move => "move",
            NavigationTool::Marquee => "marquee",
            NavigationTool::Lasso => "lasso",
            NavigationTool::Wand => "wand",
            NavigationTool::Crop => "crop",
            NavigationTool::Brush => "brush",
            NavigationTool::SpotHealing => "spotHealing",
            NavigationTool::CloneStamp => "cloneStamp",
            NavigationTool::Blur => "blur",
            NavigationTool::Gradient => "gradient",
            NavigationTool::Shape => "shape",
            NavigationTool::Type => "type",
            NavigationTool::Eyedropper => "eyedropper",
            NavigationTool::Hand => "hand",
            NavigationTool::Zoom => "zoom",
            NavigationTool::Idle => "idle",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.raw_value() == value)
    }

    /// Tools that paint with the brush tip, sharing its size, hardness, opacity, and keys.
    pub fn is_brush_tool(self) -> bool {
        matches!(
            self,
            NavigationTool::Brush | NavigationTool::SpotHealing | NavigationTool::CloneStamp | NavigationTool::Blur
        )
    }

    /// Tools that draw and edit selections, sharing modifiers, moving, and nudging.
    pub fn is_selection_tool(self) -> bool {
        matches!(self, NavigationTool::Marquee | NavigationTool::Lasso | NavigationTool::Wand)
    }

    /// The SF Symbol the rail draws.
    pub fn symbol(self) -> &'static str {
        match self {
            NavigationTool::Type => "textformat",
            NavigationTool::Eyedropper => "eyedropper",
            NavigationTool::Marquee => "rectangle.dashed",
            NavigationTool::Lasso => "lasso",
            NavigationTool::Wand => "wand.and.stars",
            NavigationTool::Brush => "paintbrush.pointed",
            NavigationTool::SpotHealing => "bandage",
            NavigationTool::CloneStamp => "seal",
            NavigationTool::Blur => "drop",
            NavigationTool::Gradient => "square.bottomhalf.filled",
            NavigationTool::Shape => "square.on.circle",
            NavigationTool::Crop => "crop",
            NavigationTool::Move => "arrow.up.left.and.arrow.down.right",
            NavigationTool::Hand => "hand.draw",
            // The chain's final else: Zoom — and, as upstream, the idle tool with it.
            NavigationTool::Zoom | NavigationTool::Idle => "magnifyingglass",
        }
    }

    /// The rail button's label and tooltip, verbatim from upstream.
    pub fn label(self) -> &'static str {
        match self {
            NavigationTool::Type => "Type (T)",
            NavigationTool::Eyedropper => "Eyedropper (I)",
            NavigationTool::Marquee => "Marquee (M)",
            NavigationTool::Lasso => "Lasso (L)",
            NavigationTool::Wand => "Magic (W) · Tab switches Wand and Object",
            NavigationTool::Brush => "Brush (B) · Eraser (E)",
            NavigationTool::SpotHealing => "Spot Healing Brush (J)",
            NavigationTool::CloneStamp => "Clone Stamp (S) · Option-click sets the source",
            NavigationTool::Blur => "Smear (R)",
            NavigationTool::Gradient => "Gradient (G)",
            NavigationTool::Shape => "Shape (U) · Shift-U switches Rectangle/Ellipse",
            NavigationTool::Crop => "Crop (C)",
            NavigationTool::Move => "Move / Transform (V)",
            NavigationTool::Hand => "Hand (H)",
            // As upstream, the chain's final else covers Zoom and the idle tool.
            NavigationTool::Zoom | NavigationTool::Idle => "Zoom (Z)",
        }
    }
}

/// The format version new saves write (Swift `ProjectManifest.current`).
pub const PROJECT_FORMAT_CURRENT: i32 = 11;

/// The oldest format version `load` still accepts (Swift `ProjectManifest.supported.lowerBound`).
pub const PROJECT_FORMAT_FIRST: i32 = 1;

/// A layer as the project manifest stores it (Swift `ProjectLayerRecord`). The manifest keys are the
/// Swift `Codable` keys: `imageFile`, `maskFile` and `maskEnabled` name the PNGs under `images/`,
/// and `parentID`/`maskSourceID` keep their capitalised `ID`.
///
/// The optionals are missing from older manifests, exactly as `decodeIfPresent` leaves them nil.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLayerRecord {
    pub id: Id,
    pub name: String,
    pub is_visible: bool,
    pub transform: LayerTransform,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "parentID")]
    pub parent_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_group: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blend_mode: Option<LayerBlendMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "maskSourceID")]
    pub mask_source_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment: Option<LayerAdjustment>,
    /// A mask moved apart from its layer: where it sits on the document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_placement: Option<LayerTransform>,
    /// Nil (older projects) is linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_linked: Option<bool>,
    /// A shape layer's shape, drawn again when the layer is scaled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<LayerShapeStyle>,
    /// The stroke and drop shadow drawn around the layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<LayerEffects>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<LayerTextStyle>,
}

impl ProjectLayerRecord {
    /// `ProjectLayerRecord(id:name:isVisible:transform:imageFile:)` — the Swift memberwise initializer,
    /// whose remaining fields all default to nil.
    pub fn new(id: Id, name: String, is_visible: bool, transform: LayerTransform, image_file: Option<String>) -> Self {
        ProjectLayerRecord {
            id,
            name,
            is_visible,
            transform,
            image_file,
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
}

/// The project manifest a `.comp` package stores (`ProjectManifest`, `IO/ProjectStore.swift`).
///
/// Every key is required on the way in except the optionals, exactly as Swift's synthesized
/// `Decodable` reads them; a missing `format`, `version` or `colorSpace` is a damaged manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectManifest {
    pub format: String,
    pub version: i32,
    pub color_space: String,
    /// Older version-1 projects default to 72 pixels/inch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<f64>,
    #[serde(rename = "documentID")]
    pub document_id: Id,
    pub width: usize,
    pub height: usize,
    #[serde(skip_serializing_if = "Option::is_none", rename = "activeLayerID")]
    pub active_layer_id: Option<Id>,
    pub layers: Vec<ProjectLayerRecord>,
    /// Alignment guides. Missing on versions 1–7.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guides: Option<Vec<CanvasGuide>>,
}

impl ProjectManifest {
    /// The `format` string every manifest carries.
    pub const FORMAT: &'static str = "com.compositor.project";
    /// The color space every manifest declares.
    pub const COLOR_SPACE: &'static str = "sRGB";
    /// The format version new saves write (Swift `ProjectManifest.current`).
    pub const CURRENT: i32 = PROJECT_FORMAT_CURRENT;
    /// The oldest format version `load` still accepts (Swift `ProjectManifest.supported.lowerBound`).
    pub const FIRST: i32 = PROJECT_FORMAT_FIRST;

    /// `init(documentID:width:height:activeLayerID:layers:)` with the Swift defaults: the current
    /// format and version, sRGB, no resolution and no guides.
    pub fn new(
        document_id: Id,
        width: usize,
        height: usize,
        active_layer_id: Option<Id>,
        layers: Vec<ProjectLayerRecord>,
    ) -> Self {
        ProjectManifest {
            format: Self::FORMAT.to_string(),
            version: Self::CURRENT,
            color_space: Self::COLOR_SPACE.to_string(),
            resolution: None,
            document_id,
            width,
            height,
            active_layer_id,
            layers,
            guides: None,
        }
    }
}

/// A project as it was read: the manifest and the pixels its layers and masks name
/// (`ProjectSnapshot`, `IO/ProjectStore.swift`). The masks are keyed the same way as the images.
#[derive(Clone, Debug)]
pub struct ProjectSnapshot {
    pub manifest: ProjectManifest,
    pub images: HashMap<Id, ImportedImage>,
    pub masks: HashMap<Id, ImportedImage>,
}

/// Why a project package could not be read or written (`ProjectError`). The messages are the ones
/// `ProjectStore` shows.
#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("This is not a valid Compositor project, or its metadata is damaged.")]
    Invalid,
    #[error("This project uses format version {}. This app supports versions {}-{}.", .0, PROJECT_FORMAT_FIRST, PROJECT_FORMAT_CURRENT)]
    Version(i32),
    #[error("An image inside the project is missing or damaged. The current document has not been replaced.")]
    MissingImage,
    #[error("This project exceeds the supported canvas, layer, file-size, or {}-megapixel document limit.", crate::limits::document_budget_megapixels())]
    TooLarge,
    #[error("An image could not be saved. The previous project has not been replaced.")]
    Encode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guides::CanvasGuideAxis;

    #[test]
    fn image_layer_serializes_the_manifest_key_spellings() {
        let parent = crate::new_id();
        let mut layer = ImageLayer::blank("Layer 1", Size::new(10.0, 20.0));
        layer.parent_id = Some(parent);
        layer.mask_source_id = Some(parent);

        let value = serde_json::to_value(&layer).expect("an ImageLayer serializes");
        let object = value.as_object().expect("an object");
        for key in ["id", "name", "isVisible", "transform", "parentID", "isGroup", "opacity", "blendMode", "maskSourceID"] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert!(!object.contains_key("parentId"), "Swift spells it parentID");
        assert!(!object.contains_key("maskSourceId"), "Swift spells it maskSourceID");
        assert_eq!(object["parentID"], serde_json::json!(parent.to_string()));
        assert_eq!(object["isGroup"], serde_json::json!(false));
        assert_eq!(object["blendMode"], serde_json::json!("Normal"));

        // Nothing optional set: the key is left out entirely, as Swift's encoder leaves nil out.
        layer.parent_id = None;
        layer.mask_source_id = None;
        let value = serde_json::to_value(&layer).expect("an ImageLayer serializes");
        let object = value.as_object().expect("an object");
        assert!(!object.contains_key("parentID"));
        assert!(!object.contains_key("maskSourceID"));
        assert!(!object.contains_key("adjustment"));
        assert!(!object.contains_key("effects"));

        let round_trip: ImageLayer = serde_json::from_value(value).expect("an ImageLayer reads back");
        assert_eq!(round_trip, layer);
    }

    #[test]
    fn project_layer_record_round_trips_the_manifest_keys() {
        let id = crate::new_id();
        let record = ProjectLayerRecord {
            id,
            name: "Layer 1".to_string(),
            is_visible: true,
            transform: transform_at(Point::ZERO, Size::new(10.0, 20.0)),
            image_file: Some(format!("{}.png", id.to_string().to_uppercase())),
            parent_id: None,
            is_group: Some(true),
            opacity: None,
            blend_mode: None,
            mask_file: Some(format!("{}.mask.png", id.to_string().to_uppercase())),
            mask_enabled: Some(false),
            mask_source_id: None,
            adjustment: None,
            mask_placement: None,
            mask_linked: None,
            shape: None,
            effects: None,
            text: None,
        };

        let value = serde_json::to_value(&record).expect("a record serializes");
        let object = value.as_object().expect("an object");
        assert_eq!(
            object["imageFile"],
            serde_json::json!(format!("{}.png", id.to_string().to_uppercase()))
        );
        assert_eq!(
            object["maskFile"],
            serde_json::json!(format!("{}.mask.png", id.to_string().to_uppercase()))
        );
        assert_eq!(object["maskEnabled"], serde_json::json!(false));
        assert_eq!(object["isGroup"], serde_json::json!(true));
        for absent in ["parentID", "opacity", "blendMode", "maskSourceID", "maskLinked", "maskPlacement"] {
            assert!(!object.contains_key(absent), "nil {absent} must be omitted");
        }

        // `parentID` and `maskSourceID` read back from their Swift spellings.
        let mut object = serde_json::to_value(&record).expect("a record serializes").as_object().unwrap().clone();
        object.remove("imageFile");
        object.insert("parentID".to_string(), serde_json::json!(id.to_string()));
        object.insert("maskSourceID".to_string(), serde_json::json!(id.to_string()));
        let record: ProjectLayerRecord =
            serde_json::from_value(serde_json::Value::Object(object)).expect("the manifest record reads back");
        assert_eq!(record.parent_id, Some(id));
        assert_eq!(record.mask_source_id, Some(id));
        assert_eq!(record.is_group, Some(true));
        assert_eq!(record.opacity, None);
        assert_eq!(record.mask_enabled, Some(false));
        assert_eq!(record.image_file, None);
    }

    #[test]
    fn project_manifest_round_trips_the_package_keys() {
        let document_id = crate::new_id();
        let layers = vec![ProjectLayerRecord::new(
            document_id,
            "Layer 1".to_string(),
            true,
            transform_at(Point::ZERO, Size::new(4.0, 4.0)),
            None,
        )];
        let mut manifest = ProjectManifest::new(document_id, 4, 4, Some(document_id), layers);
        manifest.resolution = Some(300.0);
        manifest.guides = Some(vec![CanvasGuide::at(CanvasGuideAxis::Vertical, 2.0)]);

        let value = serde_json::to_value(&manifest).expect("a manifest serializes");
        let object = value.as_object().expect("an object");
        assert_eq!(object["format"], serde_json::json!("com.compositor.project"));
        assert_eq!(object["version"], serde_json::json!(PROJECT_FORMAT_CURRENT));
        assert_eq!(object["colorSpace"], serde_json::json!("sRGB"));
        assert_eq!(object["documentID"], serde_json::json!(document_id.to_string()));
        assert_eq!(object["activeLayerID"], serde_json::json!(document_id.to_string()));
        assert_eq!(object["resolution"], serde_json::json!(300.0));
        assert_eq!(object["guides"][0]["axis"], serde_json::json!("vertical"));
        assert!(!object.contains_key("documentId"), "Swift spells it documentID");
        assert!(!object.contains_key("activeLayerId"), "Swift spells it activeLayerID");

        let round_trip: ProjectManifest = serde_json::from_value(value).expect("a manifest reads back");
        assert_eq!(round_trip, manifest);

        // Nil optionals are left out, as Swift's `encodeIfPresent` leaves them.
        manifest.resolution = None;
        manifest.guides = None;
        manifest.active_layer_id = None;
        let value = serde_json::to_value(&manifest).expect("a manifest serializes");
        let object = value.as_object().expect("an object");
        assert!(!object.contains_key("resolution"));
        assert!(!object.contains_key("guides"));
        assert!(!object.contains_key("activeLayerID"), "a nil active layer is omitted, not null");

        // A manifest missing the required keys is damaged, not defaulted: Swift's synthesized decoder
        // reads them with `decode`, and only the optionals with `decodeIfPresent`.
        let minimal = format!("{{\"documentID\":\"{document_id}\",\"width\":4,\"height\":4,\"activeLayerID\":null,\"layers\":[]}}");
        assert!(serde_json::from_str::<ProjectManifest>(&minimal).is_err());
    }

    #[test]
    fn canvas_document_dimensions_and_size() {
        assert_eq!(CanvasDocument::valid_dimension("12"), Some(12));
        assert_eq!(CanvasDocument::valid_dimension(" 30000 "), Some(MAX_SIDE));
        assert_eq!(CanvasDocument::valid_dimension("+7"), Some(7));
        assert_eq!(CanvasDocument::valid_dimension("0"), None);
        assert_eq!(CanvasDocument::valid_dimension("30001"), None);
        assert_eq!(CanvasDocument::valid_dimension("-5"), None);
        assert_eq!(CanvasDocument::valid_dimension("nine"), None);
        assert_eq!(CanvasDocument::valid_dimension(""), None);
        // Ported from CompositorTests.CompositorTests.dimensionValidation.
        assert_eq!(CanvasDocument::valid_dimension("1.5"), None, "a fractional size is not a dimension");
        assert_eq!(
            CanvasDocument::valid_dimension("9999999999999999999999"),
            None,
            "an overflowing number is not a dimension"
        );

        let document = CanvasDocument::new(64, 32);
        assert_eq!(document.size(), Size::new(64.0, 32.0));
        assert_eq!(document.resolution, 72.0);
        assert!(document.layers.is_empty());
    }

    #[test]
    fn navigation_tool_helpers_match_upstream() {
        assert_eq!(NavigationTool::ALL.len(), 16);
        assert_eq!(NavigationTool::ALL[0], NavigationTool::Move);
        assert_eq!(NavigationTool::ALL[15], NavigationTool::Idle);

        for tool in [NavigationTool::Brush, NavigationTool::SpotHealing, NavigationTool::CloneStamp, NavigationTool::Blur] {
            assert!(tool.is_brush_tool(), "{} is a brush tool", tool.raw_value());
        }
        assert!(!NavigationTool::Gradient.is_brush_tool());
        for tool in [NavigationTool::Marquee, NavigationTool::Lasso, NavigationTool::Wand] {
            assert!(tool.is_selection_tool(), "{} is a selection tool", tool.raw_value());
        }
        assert!(!NavigationTool::Move.is_selection_tool());

        assert_eq!(NavigationTool::Move.symbol(), "arrow.up.left.and.arrow.down.right");
        assert_eq!(NavigationTool::Wand.symbol(), "wand.and.stars");
        // The tool chain's final else covers both.
        assert_eq!(NavigationTool::Zoom.symbol(), "magnifyingglass");
        assert_eq!(NavigationTool::Idle.symbol(), "magnifyingglass");
        assert_eq!(NavigationTool::Zoom.label(), "Zoom (Z)");
        assert_eq!(NavigationTool::Idle.label(), "Zoom (Z)");
        assert_eq!(NavigationTool::Wand.label(), "Magic (W) · Tab switches Wand and Object");
        assert_eq!(NavigationTool::CloneStamp.label(), "Clone Stamp (S) · Option-click sets the source");

        assert_eq!(NavigationTool::from_raw("spotHealing"), Some(NavigationTool::SpotHealing));
        assert_eq!(NavigationTool::SpotHealing.raw_value(), "spotHealing");
        assert_eq!(NavigationTool::from_raw("nope"), None);
        assert_eq!(serde_json::to_value(NavigationTool::CloneStamp).unwrap(), serde_json::json!("cloneStamp"));
    }

    #[test]
    fn image_layer_init_helpers_follow_the_swift_initializers() {
        let blank = ImageLayer::blank("Layer 1", Size::new(20.0, 10.0));
        assert_eq!(blank.name, "Layer 1");
        assert!(blank.asset.is_none());
        assert_eq!(blank.origin(), Point::ZERO);
        assert_eq!(blank.size(), Size::new(20.0, 10.0));
        assert!(blank.is_visible);
        assert!(!blank.is_group);
        assert_eq!(blank.opacity, 1.0);
        assert_eq!(blank.blend_mode, LayerBlendMode::Normal);

        // Identity, not value: two assets over the same raster are the same asset.
        let raster: crate::buffer::SharedImage = Arc::new(crate::buffer::Rgba8Image::new(4, 4));
        let asset = ImportedImage::new(PixelImage::Rgba(raster.clone()), PixelImage::Rgba(raster.clone()), "Photo");
        let first = ImageLayer::from_asset(asset.clone(), Point::new(3.0, 4.0));
        let mut second = first.clone();
        assert_eq!(first, second);
        assert_eq!(first.name, "Photo");
        assert_eq!(first.size(), Size::new(4.0, 4.0));
        assert_eq!(first.origin(), Point::new(3.0, 4.0));

        second.name = "Renamed".to_string();
        assert_ne!(first, second, "a name change is a difference");

        let other = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(crate::buffer::Rgba8Image::new(4, 4))),
                PixelImage::Rgba(Arc::new(crate::buffer::Rgba8Image::new(4, 4))),
                "Photo",
            ),
            Point::new(3.0, 4.0),
        );
        let mut same_name = other.clone();
        same_name.id = first.id;
        assert_ne!(first, same_name, "different pixels are different layers even under one id");
    }
}
