//! The `manifest.json` at the heart of a `.comp` package (`IO/ProjectStore.swift`): what the
//! document is, where every layer sits and which assets belong to it.
//!
//! The value types are shared with the document model (`compositor_rs_core::document`) so the session,
//! the renderer and the store speak one shape; this module is the io-side home the crate re-exports
//! them from, together with the format's constants and asset-naming helpers.
//!
//! The keys are the Swift `Codable` keys — `documentID`, `activeLayerID`, `parentID`,
//! `maskSourceID`, `imageFile`, `maskFile`, … — and the geometry is the CoreGraphics array form
//! (`origin: [x, y]`, `size: [w, h]`), so a manifest round-trips unchanged.

use compositor_rs_core::Id;

pub use compositor_rs_core::document::{ProjectLayerRecord, ProjectManifest};

/// The format version new saves write.
pub const CURRENT: i32 = ProjectManifest::CURRENT;

/// The oldest format version `load` accepts.
pub const FIRST: i32 = ProjectManifest::FIRST;

/// Every version `load` accepts. The package-header check, the manifest check and the error message
/// all read this, so they cannot drift apart when [`CURRENT`] is bumped.
pub fn is_supported(version: i32) -> bool {
    (FIRST..=CURRENT).contains(&version)
}

/// `<layer UUID>.png` — the name a layer's PNG must have under `images/`, in the uppercase spelling
/// `UUID.uuidString` uses.
pub fn image_filename(id: Id) -> String {
    format!("{}.png", id.to_string().to_uppercase())
}

/// `<layer UUID>.mask.png` — the name a layer's or folder's mask PNG must have under `images/`.
pub fn mask_filename(id: Id) -> String {
    format!("{}.mask.png", id.to_string().to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::validate;
    use compositor_rs_core::blend::LayerBlendMode;
    use compositor_rs_core::geom::{Point, Size};
    use compositor_rs_core::guides::{CanvasGuide, CanvasGuideAxis};
    use compositor_rs_core::layer_adjustment::{AdjustmentKind, LayerAdjustment};
    use compositor_rs_core::layer_effects::{LayerEffects, StrokeEffect};
    use compositor_rs_core::layer_shape::{LayerShapeStyle, ShapeKind};
    use compositor_rs_core::layer_text::{LayerTextColorRun, LayerTextFontRun, LayerTextStyle, TextAlignment};
    use compositor_rs_core::layer_transform::{LayerSampling, LayerTransform};

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

    fn record(id: Id, name: &str, transform: LayerTransform) -> ProjectLayerRecord {
        ProjectLayerRecord {
            id,
            name: name.to_string(),
            is_visible: true,
            transform,
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

    /// The manifest the tests round-trip: a full-height shape layer (the live-mask source), a group
    /// holding a masked, clipped, textual layer, and guides — every optional field populated.
    fn loaded_manifest() -> ProjectManifest {
        let document_id = Id::new_v4();
        let group_id = Id::new_v4();
        let layer_id = Id::new_v4();
        let source_id = Id::new_v4();

        let source = record(source_id, "Source", transform(Point::new(-2.0, -2.0), Size::new(200.0, 160.0)));

        let mut group = record(group_id, "Folder 1", transform(Point::new(0.0, 0.0), Size::new(100.0, 80.0)));
        group.image_file = None;
        group.is_group = Some(true);
        group.opacity = Some(0.5);

        let mut layer = record(layer_id, "Layer 1", transform(Point::new(4.0, 6.0), Size::new(64.0, 32.0)));
        layer.parent_id = Some(group_id);
        layer.opacity = Some(0.75);
        layer.blend_mode = Some(LayerBlendMode::Multiply);
        layer.mask_file = Some(mask_filename(layer_id));
        layer.mask_enabled = Some(false);
        layer.mask_source_id = Some(source_id);
        layer.mask_placement = Some(transform(Point::new(1.0, 2.0), Size::new(64.0, 32.0)));
        layer.mask_linked = Some(false);
        layer.shape = Some(LayerShapeStyle {
            kind: ShapeKind::Line,
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            corner_radius: 3.0,
            line_width: Some(2.0),
            start: Some(Point::new(0.25, 0.5)),
            end: Some(Point::new(0.75, 0.5)),
        });
        layer.effects = Some(LayerEffects {
            stroke: Some(StrokeEffect { enabled: Some(false), ..StrokeEffect::default() }),
            ..Default::default()
        });
        let mut text = LayerTextStyle { content: "Hello".to_string(), ..LayerTextStyle::default() };
        text.alignment = TextAlignment::Center;
        text.box_size = Some(Size::new(200.0, 100.0));
        text.color_runs = Some(vec![LayerTextColorRun { location: 0, length: 2, red: 1.0, green: 0.0, blue: 0.0 }]);
        text.font_runs = Some(vec![LayerTextFontRun { location: 2, length: 3, font_name: "Helvetica-Bold".to_string() }]);
        layer.text = Some(text);

        // An adjustment layer: a transform, no image, and the settings of every kind.
        let mut adjustment = record(Id::new_v4(), "Invert", transform(Point::ZERO, Size::new(1920.0, 1080.0)));
        adjustment.image_file = None;
        adjustment.adjustment = Some(LayerAdjustment::new(AdjustmentKind::Invert));

        let mut manifest = ProjectManifest::new(
            document_id,
            1920,
            1080,
            Some(layer_id),
            vec![source, group, layer, adjustment],
        );
        manifest.resolution = Some(300.0);
        manifest.guides = Some(vec![
            CanvasGuide::new(Id::new_v4(), CanvasGuideAxis::Horizontal, 120.0),
            CanvasGuide::new(Id::new_v4(), CanvasGuideAxis::Vertical, -8.0),
        ]);
        manifest
    }

    /// The manifest keys are the Swift `Codable` keys, and the geometry is the CoreGraphics array
    /// form the original files use.
    #[test]
    fn manifest_serializes_the_exact_swift_keys() {
        let manifest = loaded_manifest();
        let value = serde_json::to_value(&manifest).expect("a manifest serializes");
        let object = value.as_object().expect("an object");

        for key in ["format", "version", "colorSpace", "resolution", "documentID", "width", "height", "activeLayerID", "layers", "guides"] {
            assert!(object.contains_key(key), "missing manifest key {key}");
        }
        assert_eq!(object["format"], serde_json::json!("com.compositor.project"));
        assert_eq!(object["version"], serde_json::json!(11));
        assert_eq!(object["colorSpace"], serde_json::json!("sRGB"));
        assert_eq!(object["width"], serde_json::json!(1920));
        assert_eq!(object["documentID"], serde_json::json!(manifest.document_id.to_string()));

        let layers = object["layers"].as_array().expect("an array");
        let group = layers[1].as_object().expect("an object");
        let layer = layers[2].as_object().expect("an object");
        for key in ["id", "name", "isVisible", "transform", "imageFile", "parentID", "opacity", "blendMode", "maskFile", "maskEnabled", "maskSourceID", "maskPlacement", "maskLinked", "shape", "effects", "text"] {
            assert!(layer.contains_key(key), "missing layer key {key}");
        }
        for key in ["id", "name", "isVisible", "transform", "isGroup", "opacity"] {
            assert!(group.contains_key(key), "missing group key {key}");
        }
        // `isGroup` is optional on a record: a plain layer leaves it out, exactly as Swift's
        // `encodeIfPresent` does, while a folder writes it.
        assert!(!layer.contains_key("isGroup"));
        assert_eq!(group["isGroup"], serde_json::json!(true));
        // A folder has no image file, and a plain layer has no adjustment.
        assert!(!group.contains_key("imageFile"));
        assert!(!group.contains_key("maskFile"));
        assert!(!layer.contains_key("adjustment"));

        // The adjustment record carries its kind and no image, exactly as a version-7+ file stores it.
        let adjustment = layers[3].as_object().expect("an object");
        assert!(!adjustment.contains_key("imageFile"));
        assert_eq!(adjustment["adjustment"]["kind"], serde_json::json!("Invert"));
        assert_eq!(adjustment["transform"]["size"], serde_json::json!([1920.0, 1080.0]));

        // Geometry: origin/size as [x, y] / [w, h], exactly like the original files.
        let transform = layer["transform"].as_object().expect("an object");
        assert_eq!(transform["origin"], serde_json::json!([4.0, 6.0]));
        assert_eq!(transform["size"], serde_json::json!([64.0, 32.0]));
        assert_eq!(transform["rotation"], serde_json::json!(0.0));
        assert_eq!(transform["flipX"], serde_json::json!(false));
        assert_eq!(transform["flipY"], serde_json::json!(false));
        assert_eq!(transform["sampling"], serde_json::json!("High quality"));

        let placement = layer["maskPlacement"].as_object().expect("an object");
        assert_eq!(placement["origin"], serde_json::json!([1.0, 2.0]));
        assert_eq!(placement["size"], serde_json::json!([64.0, 32.0]));

        // Non-default appearance and mask metadata keep their spellings and values.
        assert_eq!(layer["opacity"], serde_json::json!(0.75));
        assert_eq!(layer["blendMode"], serde_json::json!("Multiply"));
        assert_eq!(layer["maskEnabled"], serde_json::json!(false));
        assert_eq!(layer["maskLinked"], serde_json::json!(false));
        assert_eq!(layer["maskFile"], serde_json::json!(mask_filename(manifest.layers[2].id)));
        assert_eq!(layer["maskSourceID"], serde_json::json!(manifest.layers[2].mask_source_id.unwrap().to_string()));

        // Guides carry id/axis/position with the lowercase axis spelling.
        let guides = object["guides"].as_array().expect("an array");
        assert_eq!(guides[0]["axis"], serde_json::json!("horizontal"));
        assert_eq!(guides[0]["position"], serde_json::json!(120.0));
        assert!(guides[0].get("id").is_some());
        assert_eq!(guides[1]["axis"], serde_json::json!("vertical"));

        // A nil `activeLayerID` is left out entirely, as Swift's `encodeIfPresent` leaves it.
        let mut value = manifest.clone();
        value.active_layer_id = None;
        let value = serde_json::to_value(&value).expect("a manifest serializes");
        assert!(!value.as_object().expect("an object").contains_key("activeLayerID"));
    }

    /// A document with groups, masks, an adjustment layer, effects, text and guides survives the
    /// round trip through JSON unchanged.
    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = loaded_manifest();
        assert!(validate(&manifest).is_ok(), "the fixture is a valid manifest");
        let bytes = serde_json::to_vec_pretty(&manifest).expect("a manifest serializes");
        let decoded: ProjectManifest = serde_json::from_slice(&bytes).expect("a manifest reads back");
        assert_eq!(decoded, manifest);
        // And again, so a decoded manifest re-encodes to the same bytes.
        let again = serde_json::to_vec_pretty(&decoded).expect("a manifest serializes");
        assert_eq!(again, bytes);
    }

    /// Missing optionals stay missing (older manifests omit them), and `null` reads as missing.
    #[test]
    fn absent_and_null_optionals_decode_as_none() {
        let id = Id::new_v4();
        let json = format!(
            r#"{{"format":"com.compositor.project","version":1,"colorSpace":"sRGB",
                "documentID":"{id}","width":8,"height":8,"activeLayerID":null,
                "layers":[{{"id":"{id}","name":"L","isVisible":true,
                "transform":{{"origin":[0,0],"size":[8,8],"rotation":0,"flipX":false,"flipY":false,"sampling":"Smooth"}},
                "imageFile":"{upper}.png"}}]}}"#,
            id = id,
            upper = id.to_string().to_uppercase()
        );
        let manifest: ProjectManifest = serde_json::from_str(&json).expect("a version-1 manifest reads back");
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.resolution, None);
        assert_eq!(manifest.active_layer_id, None);
        assert_eq!(manifest.guides, None);
        let layer = &manifest.layers[0];
        assert_eq!(layer.parent_id, None);
        assert_eq!(layer.opacity, None);
        assert_eq!(layer.mask_file, None);
        assert_eq!(layer.adjustment, None);
        assert_eq!(layer.transform.sampling, LayerSampling::Smooth);
        assert!(validate(&manifest).is_ok(), "a minimal version-1 manifest validates");
    }

    /// The format's constants and asset names are the ones `load` and `save` compare against.
    #[test]
    fn format_constants_and_asset_names() {
        assert_eq!(ProjectManifest::FORMAT, "com.compositor.project");
        assert_eq!(ProjectManifest::COLOR_SPACE, "sRGB");
        assert_eq!(CURRENT, 11);
        assert!(is_supported(1) && is_supported(11));
        assert!(!is_supported(0) && !is_supported(12));

        let id = Id::parse_str("6f1d3c2a-0b7e-4e8a-9c4d-2a1b3c4d5e6f").unwrap();
        assert_eq!(image_filename(id), "6F1D3C2A-0B7E-4E8A-9C4D-2A1B3C4D5E6F.png");
        assert_eq!(mask_filename(id), "6F1D3C2A-0B7E-4E8A-9C4D-2A1B3C4D5E6F.mask.png");
    }
}
