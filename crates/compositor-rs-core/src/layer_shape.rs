//! The Shape tool's model: the shape kinds, the style a shape layer keeps so it can be drawn again at
//! another size, the shape layer itself and the draft being dragged out.
//!
//! Ported from `Document/ShapeTool.swift` (the model types; the session commands and the rasterization
//! live elsewhere).

use crate::buffer::SharedImage;
use crate::color::PaletteColor;
use crate::geom::{CGFloat, Point, Rect};
use crate::path::Path;
use serde::{Deserialize, Serialize};

/// The kinds of shape the Shape tool draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShapeKind {
    /// `Rectangle`.
    #[serde(rename = "Rectangle")]
    Rectangle,
    /// `Ellipse`.
    #[serde(rename = "Ellipse")]
    Ellipse,
    /// `Line`.
    #[serde(rename = "Line")]
    Line,
}

impl ShapeKind {
    /// Every kind, in the order the Shape tool steps through them.
    pub const ALL: [ShapeKind; 3] = [ShapeKind::Rectangle, ShapeKind::Ellipse, ShapeKind::Line];

    /// The manifest's raw string.
    pub const fn raw_value(self) -> &'static str {
        match self {
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Line => "Line",
        }
    }

    /// The kind a manifest's raw string names.
    pub fn from_raw(value: &str) -> Option<ShapeKind> {
        match value {
            "Rectangle" => Some(ShapeKind::Rectangle),
            "Ellipse" => Some(ShapeKind::Ellipse),
            "Line" => Some(ShapeKind::Line),
            _ => None,
        }
    }

    /// The shape filling `rect`. A rectangle's corners round by `corner_radius`, at most half its shorter
    /// side (so a large radius makes a pill); ellipses ignore it. A line runs corner to corner and is
    /// stroked, not filled (see `line_path`).
    pub fn path(&self, rect: Rect, corner_radius: CGFloat) -> Path {
        if *self == ShapeKind::Ellipse {
            return Path::ellipse(rect);
        }
        let radius = corner_radius.max(0.0).min(rect.width() / 2.0).min(rect.height() / 2.0);
        if radius <= 0.0 {
            return Path::rect(rect);
        }
        Path::rounded_rect(rect, radius)
    }
}

/// What a shape layer draws, kept so the shape can be drawn again at a new size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerShapeStyle {
    pub kind: ShapeKind,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    /// Document pixels, whatever size the shape is scaled to.
    pub corner_radius: CGFloat,
    /// A line's thickness, and its two ends as fractions of the layer's box (0–1), so the line lands on exactly the
    /// points it was dragged between and still redraws correctly at another size. Nil on other shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_width: Option<CGFloat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Point>,
}

impl LayerShapeStyle {
    /// The style's straight sRGB color.
    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }
}

/// A layer made with the Shape tool. Its pixels are an ordinary raster, so it clips, masks, blends and filters like
/// any layer; `image` is the raster the shape drew. Once anything else changes those pixels (painting, a filter),
/// the layer's image is no longer this one and the layer is plain pixels from then on.
#[derive(Clone, Debug)]
pub struct LayerShape {
    pub style: LayerShapeStyle,
    pub image: SharedImage,
}

impl PartialEq for LayerShape {
    fn eq(&self, other: &Self) -> bool {
        self.style == other.style && std::sync::Arc::ptr_eq(&self.image, &other.image)
    }
}

impl LayerShape {
    /// The shape for a loaded project: both the style and its pixels have to be there.
    pub fn loaded(style: Option<LayerShapeStyle>, image: Option<SharedImage>) -> Option<LayerShape> {
        match (style, image) {
            (Some(style), Some(image)) => Some(LayerShape { style, image }),
            _ => None,
        }
    }
}

impl crate::document::ImageLayer {
    /// The shape this layer still is: nil once its pixels were edited some other way.
    pub fn live_shape(&self) -> Option<LayerShape> {
        let shape = self.shape.as_ref()?;
        let crate::imported_image::PixelImage::Rgba(image) = &self.asset.as_ref()?.image else {
            return None;
        };
        if std::sync::Arc::ptr_eq(&image, &shape.image) {
            Some(shape.clone())
        } else {
            None
        }
    }
}

/// A shape being dragged out with the Shape tool, in whole document pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeDraft {
    pub kind: ShapeKind,
    pub anchor: Point,
    pub rect: Rect,
    /// Where a line is being dragged to, so its ends stay exactly where they were put.
    pub end: Option<Point>,
    /// Document pixels, fixed when the drag starts; rectangles only.
    pub corner_radius: CGFloat,
}

impl ShapeDraft {
    /// A drag that has just started: no end yet, and no corner radius until the tool sets one.
    pub fn new(kind: ShapeKind, anchor: Point, rect: Rect) -> Self {
        ShapeDraft { kind, anchor, rect, end: None, corner_radius: 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn line_style() -> LayerShapeStyle {
        LayerShapeStyle {
            kind: ShapeKind::Line,
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            corner_radius: 0.0,
            line_width: Some(4.0),
            start: Some(Point::new(0.25, 0.5)),
            end: Some(Point::new(0.75, 0.5)),
        }
    }

    fn rectangle_style() -> LayerShapeStyle {
        LayerShapeStyle {
            kind: ShapeKind::Rectangle,
            red: 0.0,
            green: 0.5,
            blue: 1.0,
            corner_radius: 8.0,
            line_width: None,
            start: None,
            end: None,
        }
    }

    #[test]
    fn kind_raw_values_and_order_match_the_manifest() {
        assert_eq!(ShapeKind::ALL, [ShapeKind::Rectangle, ShapeKind::Ellipse, ShapeKind::Line]);
        for kind in ShapeKind::ALL {
            assert_eq!(ShapeKind::from_raw(kind.raw_value()), Some(kind));
        }
        assert_eq!(ShapeKind::from_raw("square"), None);
        assert_eq!(serde_json::to_string(&ShapeKind::Ellipse).unwrap(), "\"Ellipse\"");
        assert_eq!(serde_json::from_str::<ShapeKind>("\"Line\"").unwrap(), ShapeKind::Line);
    }

    /// The manifest keys a shape style writes, and the CoreGraphics array form of its points.
    #[test]
    fn line_style_serializes_the_manifest_keys() {
        let value = serde_json::to_value(line_style()).unwrap();
        assert_eq!(value["kind"], "Line");
        assert_eq!(value["red"], 1.0);
        assert_eq!(value["green"], 0.0);
        assert_eq!(value["blue"], 0.0);
        assert_eq!(value["cornerRadius"], 0.0);
        assert_eq!(value["lineWidth"], 4.0);
        assert_eq!(value["start"], serde_json::json!([0.25, 0.5]));
        assert_eq!(value["end"], serde_json::json!([0.75, 0.5]));
        assert_eq!(serde_json::from_value::<LayerShapeStyle>(value).unwrap(), line_style());
    }

    #[test]
    fn shapes_other_than_lines_omit_the_line_fields() {
        let value = serde_json::to_value(rectangle_style()).unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("lineWidth"));
        assert!(!object.contains_key("start"));
        assert!(!object.contains_key("end"));
        assert_eq!(rectangle_style().color(), PaletteColor::new(0.0, 0.5, 1.0));
        assert_eq!(serde_json::from_value::<LayerShapeStyle>(value).unwrap(), rectangle_style());
    }

    #[test]
    fn a_shape_is_the_same_only_while_its_pixels_are() {
        let image = Arc::new(crate::buffer::Rgba8Image::new(2, 2));
        let shape = LayerShape { style: rectangle_style(), image: image.clone() };
        assert_eq!(shape, LayerShape { style: rectangle_style(), image: image.clone() });
        assert!(shape == LayerShape::loaded(Some(rectangle_style()), Some(image)).unwrap());
        // Other pixels, or another style, is another shape.
        let other = Arc::new(crate::buffer::Rgba8Image::new(2, 2));
        assert!(shape != LayerShape { style: rectangle_style(), image: other });
        let mut edited = rectangle_style();
        edited.corner_radius = 4.0;
        assert!(LayerShape::loaded(Some(edited), None).is_none());
        assert!(LayerShape::loaded(None, Some(Arc::new(crate::buffer::Rgba8Image::new(1, 1)))).is_none());
    }

    #[test]
    fn a_fresh_draft_has_no_end_and_no_radius() {
        let draft = ShapeDraft::new(ShapeKind::Rectangle, Point::new(10.0, 10.0), Rect::new(10.0, 10.0, 0.0, 0.0));
        assert_eq!(draft.end, None);
        assert_eq!(draft.corner_radius, 0.0);
        assert_eq!(draft.kind, ShapeKind::Rectangle);
    }
}
