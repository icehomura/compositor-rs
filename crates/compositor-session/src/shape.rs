//! The Shape tool: `Document/ShapeTool.swift` — the draft's lifecycle, its corner radius and line
//! width, the layer a finished shape becomes, and the redraw that keeps a rounded corner's radius
//! when the layer is scaled.
//!
//! The model types (`ShapeKind`, `LayerShapeStyle`, `LayerShape`, `ShapeDraft`) live in
//! `compositor_core::layer_shape`.

use std::sync::Arc;

use compositor_core::buffer::SharedImage;
use compositor_core::document::{ImageLayer, NavigationTool};
use compositor_core::error::CoreError;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_shape::{LayerShape, LayerShapeStyle, ShapeDraft, ShapeKind};
use compositor_core::layer_transform::LayerTransform;
use compositor_core::limits;
use compositor_core::path::{FillRule, Path};
use compositor_core::selection::DragBox;
use compositor_pixels::canvas::Canvas;

use crate::brush::brush_failure;
use crate::clipboard::rgba_thumbnail;
use crate::session::EditorSession;

/// Pixels one shape layer may hold, the same budget as an import (`EditorSession.maxShapePixels`).
pub const MAX_SHAPE_PIXELS: usize = limits::MAX_SURFACE_PIXELS;

/// The width a shape transform's preview may be drawn at, before it is scaled down to the box
/// (`shapeTransformPreview`).
const SHAPE_PREVIEW_MAX_SIDE: f64 = 2048.0;

/// One entry of `shapeTransformPreviewCache`: the size the image was drawn at and the image.
#[derive(Clone, Debug)]
pub struct ShapePreview {
    pub size: Size,
    pub image: SharedImage,
}

impl EditorSession {
    pub fn begin_shape(&mut self, point: Point) {
        if self.tool != NavigationTool::Shape || !self.can_edit_layers() || !(point.x.is_finite() && point.y.is_finite()) {
            return;
        }
        let anchor = Point::new(point.x.round(), point.y.round());
        let mut draft = ShapeDraft::new(self.shape_kind, anchor, Rect::new(anchor.x, anchor.y, 0.0, 0.0));
        draft.corner_radius = if self.shape_kind == ShapeKind::Rectangle {
            self.shape_corner_radius
        } else {
            0.0
        };
        self.shape_draft = Some(draft);
    }

    /// The line being dragged, from where it began to where the pointer is, in document pixels.
    pub fn shape_line_ends(&self) -> Option<(Point, Point)> {
        let draft = self.shape_draft.as_ref()?;
        if draft.kind != ShapeKind::Line {
            return None;
        }
        Some((draft.anchor, draft.end?))
    }

    /// Shift makes a square or circle; Option grows the shape from its center, as in Photoshop.
    pub fn drag_shape(&mut self, point: Point, square: bool, from_center: bool) {
        let Some(mut draft) = self.shape_draft.clone() else { return };
        if !(point.x.is_finite() && point.y.is_finite()) {
            return;
        }
        // Shift on a line snaps its angle to eighths of a turn — flat, upright, or 45° — rather than
        // squaring a box.
        if draft.kind == ShapeKind::Line && square {
            let dx = point.x - draft.anchor.x;
            let dy = point.y - draft.anchor.y;
            let angle = (dy.atan2(dx) / (std::f64::consts::PI / 4.0)).round() * (std::f64::consts::PI / 4.0);
            let length = dx.hypot(dy);
            let snapped = Point::new(draft.anchor.x + angle.cos() * length, draft.anchor.y + angle.sin() * length);
            draft.end = Some(snapped);
            draft.rect = DragBox::rect(draft.anchor, snapped, false, from_center);
            self.shape_draft = Some(draft);
            return;
        }
        if draft.kind == ShapeKind::Line {
            draft.end = Some(point);
        }
        draft.rect = DragBox::rect(draft.anchor, point, square, from_center);
        self.shape_draft = Some(draft);
    }

    pub fn cancel_shape(&mut self) {
        if self.shape_draft.is_some() {
            self.shape_draft = None;
        }
    }

    /// Shift-U (and Tab): the Shape tool steps through Rectangle, Ellipse and Line.
    pub fn toggle_shape_kind(&mut self) {
        self.cancel_shape();
        let kinds = ShapeKind::ALL;
        let index = kinds.iter().position(|kind| *kind == self.shape_kind).unwrap_or(0);
        self.shape_kind = kinds[(index + 1) % kinds.len()];
    }

    /// Fills the dragged shape with the foreground color on a new layer above the active one, in one
    /// undo step. A click without a drag makes nothing; the selection is left alone.
    pub fn finish_shape(&mut self) {
        let Some(draft) = self.shape_draft.take() else { return };
        let mut rect = draft.rect;
        let thickness = self.shape_line_width;
        // A line keeps the two points it was dragged between; the layer is their box with room for
        // the stroke's own thickness (and its round ends) around them.
        let mut ends: Option<(Point, Point)> = None;
        if draft.kind == ShapeKind::Line {
            let from = draft.anchor;
            let to = draft.end.unwrap_or(draft.anchor);
            rect = Rect::new(
                from.x.min(to.x),
                from.y.min(to.y),
                (to.x - from.x).abs(),
                (to.y - from.y).abs(),
            )
            .inset_by(-thickness / 2.0, -thickness / 2.0);
            ends = Some((from, to));
        }
        if !self.can_edit_layers() || self.document.is_none() || rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        if (rect.width() as usize).saturating_mul(rect.height() as usize) > MAX_SHAPE_PIXELS {
            self.brush_error = Some(format!(
                "That shape is too large. A shape can cover up to {} megapixels.",
                limits::max_surface_megapixels()
            ));
            return;
        }
        // The ends as fractions of the box, so a scaled line still runs between the same two places.
        let unit = |point: Point| {
            Point::new(
                if rect.width() > 0.0 { (point.x - rect.min_x()) / rect.width() } else { 0.5 },
                if rect.height() > 0.0 { (point.y - rect.min_y()) / rect.height() } else { 0.5 },
            )
        };
        let start = ends.map(|(start, _)| unit(start));
        let finish = ends.map(|(_, end)| unit(end));
        let foreground = self.foreground_color();
        let image = match EditorSession::shape_image(
            draft.kind,
            Size::new(rect.width(), rect.height()),
            foreground,
            draft.corner_radius,
            thickness,
            start,
            finish,
        ) {
            Ok(image) => image,
            Err(error) => {
                self.brush_error = Some(brush_failure(&error));
                return;
            }
        };
        let style = LayerShapeStyle {
            kind: draft.kind,
            red: foreground.red,
            green: foreground.green,
            blue: foreground.blue,
            corner_radius: draft.corner_radius,
            line_width: if draft.kind == ShapeKind::Line { Some(thickness) } else { None },
            start,
            end: finish,
        };
        let shape = LayerShape {
            style,
            image: image.clone(),
        };
        let name = self.next_shape_name(draft.kind);
        self.add_pixel_layer(
            image,
            Point::new(rect.min_x(), rect.min_y()),
            &name,
            draft.kind.raw_value(),
            false,
            Some(shape),
            None,
        );
    }

    /// "Rectangle 1", "Ellipse 2", … skipping names already in the document.
    pub fn next_shape_name(&self, kind: ShapeKind) -> String {
        let names: rustc_hash::FxHashSet<&str> = self
            .document
            .as_ref()
            .map(|document| document.layers.iter().map(|layer| layer.name.as_str()).collect())
            .unwrap_or_default();
        let mut number = 1;
        while names.contains(format!("{} {}", kind.raw_value(), number).as_str()) {
            number += 1;
        }
        format!("{} {}", kind.raw_value(), number)
    }

    /// A shape layer scaled to a new size draws its shape again at that size, so a rounded corner
    /// keeps its radius instead of stretching. Part of the edit that changed the size.
    pub fn redraw_shape(&mut self, index: usize) {
        let Some(layer) = self.document.as_ref().and_then(|document| document.layers.get(index)) else { return };
        let Some(shape) = layer.live_shape() else { return };
        let Some(asset) = layer.asset.as_ref() else { return };
        let width = 1.max(layer.transform.size.width.round() as i64);
        let height = 1.max(layer.transform.size.height.round() as i64);
        if width == asset.image.width() as i64 && height == asset.image.height() as i64 {
            return;
        }
        if (width as usize).saturating_mul(height as usize) > MAX_SHAPE_PIXELS {
            return;
        }
        let Ok(image) = EditorSession::shape_image(
            shape.style.kind,
            Size::new(width as f64, height as f64),
            shape.style.color(),
            shape.style.corner_radius,
            shape.style.line_width.unwrap_or(0.0),
            shape.style.start,
            shape.style.end,
        ) else {
            return;
        };
        let thumbnail = rgba_thumbnail(&image);
        let name = asset.name.clone();
        let mask_transform = layer.mask_transform();
        let document = self.document.as_mut().expect("the layer came from it");
        // A mask that follows the layer's pixel grid stays exactly where it is while that grid
        // changes size.
        if document.layers[index].mask.as_ref().map(|mask| mask.placement.is_none()) == Some(true) {
            if let Some(mask) = document.layers[index].mask.as_mut() {
                mask.placement = Some(mask_transform);
            }
        }
        let asset = ImportedImage::new(PixelImage::Rgba(image.clone()), thumbnail, name);
        document.layers[index].asset = Some(asset);
        document.layers[index].shape = Some(LayerShape { style: shape.style, image });
    }

    /// While a rounded rectangle is being scaled, the shape drawn at the size it's being dragged to,
    /// so its corners keep their radius during the drag rather than only once it's applied. At most
    /// 2048 pixels across (the radius scales down with it); nil for any other layer, which just
    /// stretches until the redraw at commit.
    pub fn shape_transform_preview(&mut self, layer: &ImageLayer, transform: LayerTransform) -> Option<SharedImage> {
        let shape = layer.live_shape();
        let wanted = self.transform_edit.is_some()
            && shape
                .as_ref()
                .map(|shape| shape.style.kind == ShapeKind::Rectangle && shape.style.corner_radius > 0.0)
                .unwrap_or(false);
        if !wanted {
            if !self.shape_transform_preview_cache.is_empty() && self.transform_edit.is_none() {
                self.shape_transform_preview_cache.clear();
            }
            return None;
        }
        let shape = shape?;
        let size = transform.size;
        if !(size.width >= 1.0 && size.height >= 1.0) {
            return None;
        }
        if (size.width - shape.image.width() as f64).abs() < 0.5 && (size.height - shape.image.height() as f64).abs() < 0.5 {
            return None;
        }
        let factor = (SHAPE_PREVIEW_MAX_SIDE / size.width.max(size.height)).min(1.0);
        let drawn = Size::new(
            1.0f64.max((size.width * factor).round()),
            1.0f64.max((size.height * factor).round()),
        );
        if let Some(cached) = self.shape_transform_preview_cache.get(&layer.id) {
            if cached.size == drawn {
                return Some(cached.image.clone());
            }
        }
        let image = EditorSession::shape_image(
            ShapeKind::Rectangle,
            drawn,
            shape.style.color(),
            shape.style.corner_radius * factor,
            0.0,
            None,
            None,
        )
        .ok()?;
        self.shape_transform_preview_cache
            .insert(layer.id, ShapePreview { size: drawn, image: image.clone() });
        Some(image)
    }

    /// The shape filling its box, anti-aliased where it curves (`shapeImage(_:size:color:cornerRadius:lineWidth:start:end:)`).
    pub fn shape_image(
        kind: ShapeKind,
        size: Size,
        color: compositor_core::color::PaletteColor,
        corner_radius: f64,
        line_width: f64,
        start: Option<Point>,
        end: Option<Point>,
    ) -> Result<SharedImage, CoreError> {
        if !(size.width.is_finite() && size.height.is_finite()) || size.width < 1.0 || size.height < 1.0 {
            return Err(CoreError::TooLarge(limits::max_surface_megapixels()));
        }
        let width = size.width.ceil() as usize;
        let height = size.height.ceil() as usize;
        if width.saturating_mul(height) > MAX_SHAPE_PIXELS {
            return Err(CoreError::TooLarge(limits::max_surface_megapixels()));
        }
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.set_fill_color(color);
        let bounds = Rect::new(0.0, 0.0, size.width, size.height);
        if kind == ShapeKind::Line {
            // Corner to corner, inset by half the thickness so the stroke stays inside the layer.
            let thickness = 1.0f64.max(line_width);
            // The ends sit where they were dragged, as fractions of the box. Older lines (no ends
            // stored) ran corner to corner, inset by half their thickness.
            let inset = bounds.inset_by(thickness.min(size.width) / 2.0, thickness.min(size.height) / 2.0);
            let from = start
                .map(|start| Point::new(start.x * size.width, start.y * size.height))
                .unwrap_or_else(|| Point::new(inset.min_x(), inset.min_y()));
            let to = end
                .map(|end| Point::new(end.x * size.width, end.y * size.height))
                .unwrap_or_else(|| Point::new(inset.max_x(), inset.max_y()));
            canvas.fill_path(&stroke_path(from, to, thickness), FillRule::Winding);
        } else {
            canvas.fill_path(&kind.path(bounds, corner_radius), FillRule::Winding);
        }
        Ok(Arc::new(canvas.into_rgba()))
    }
}

/// `CGContext.strokePath()` for a single line with round caps, as `shapeImage` drew one: the
/// capsule around the segment, as one simple polygon so the fill is exact under either winding rule.
/// Core Graphics' circles are approximated by `radius * 2` chords a half, which stays well inside a
/// tenth of a pixel of the true arc up to the 2048-pixel preview cap.
fn stroke_path(from: Point, to: Point, thickness: f64) -> Path {
    let radius = (thickness / 2.0).max(0.0);
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let length = dx.hypot(dy);
    let mut path = Path::empty();
    if radius <= 0.0 || length <= 1e-9 {
        path.add_ellipse(Rect::new(from.x - radius, from.y - radius, radius * 2.0, radius * 2.0));
        return path;
    }
    let (ux, uy) = (dx / length, dy / length);
    // The normal points along the segment's left side; the capsule runs down one side, around the
    // far cap, back down the other side and around the near cap.
    let (nx, ny) = (-uy * radius, ux * radius);
    let segments = ((radius * 2.0).ceil() as usize).clamp(8, 256);
    path.move_to(Point::new(from.x + nx, from.y + ny));
    path.add_line(Point::new(to.x + nx, to.y + ny));
    let far_base = (ux * radius).atan2(uy * radius);
    for step in 0..=segments {
        let angle = far_base + std::f64::consts::PI * (step as f64 / segments as f64);
        path.add_line(Point::new(to.x + angle.cos() * radius, to.y + angle.sin() * radius));
    }
    path.add_line(Point::new(from.x - nx, from.y - ny));
    let near_base = (-ux * radius).atan2(-uy * radius);
    for step in 0..=segments {
        let angle = near_base + std::f64::consts::PI * (step as f64 / segments as f64);
        path.add_line(Point::new(from.x + angle.cos() * radius, from.y + angle.sin() * radius));
    }
    path.close_subpath();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::color::PaletteColor;

    /// The shape kinds step Rectangle → Ellipse → Line → Rectangle (Tab and Shift-U).
    #[test]
    fn shape_kind_cycles_through_every_kind() {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Shape;
        assert_eq!(session.shape_kind, ShapeKind::Rectangle);
        session.toggle_shape_kind();
        assert_eq!(session.shape_kind, ShapeKind::Ellipse);
        session.toggle_shape_kind();
        assert_eq!(session.shape_kind, ShapeKind::Line);
        session.toggle_shape_kind();
        assert_eq!(session.shape_kind, ShapeKind::Rectangle);
    }

    /// A line's Shift drag snaps its angle to eighths of a turn while keeping its length.
    #[test]
    fn shift_drag_snaps_a_line_angle() {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Shape;
        // Upstream's `makeSession()` opens a document first (`ShapeToolTests.swift:7-13`);
        // `beginShape` requires `canEditLayers`, which requires one (`ShapeTool.swift:71`).
        let mut document = compositor_core::document::CanvasDocument::new(64, 64);
        document.layers = vec![ImageLayer::blank("Layer 1", Size::new(64.0, 64.0))];
        session.document = Some(document);
        session.shape_kind = ShapeKind::Line;
        session.begin_shape(Point::new(10.0, 10.0));
        session.drag_shape(Point::new(40.0, 13.0), true, false);
        let end = session.shape_draft.as_ref().expect("a draft").end.expect("a line end");
        assert_eq!(end.y, 10.0, "a nearly flat drag snaps flat");
        assert!((end.x - 40.13).abs() < 0.2, "the snapped point keeps the drag's length");

        session.drag_shape(Point::new(10.0, 40.0), true, false);
        let end = session.shape_draft.as_ref().expect("a draft").end.expect("a line end");
        assert!((end.x - 10.0).abs() < 1e-9, "a nearly upright drag snaps upright");
    }

    /// Shift squares a rectangle's box; Option grows it from where the drag started.
    #[test]
    fn shift_and_option_shape_the_drag_box() {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Shape;
        // As above, upstream's `makeSession()` has a document before `beginShape`
        // (`ShapeToolTests.swift:7-13`, `ShapeTool.swift:71`).
        let mut document = compositor_core::document::CanvasDocument::new(64, 64);
        document.layers = vec![ImageLayer::blank("Layer 1", Size::new(64.0, 64.0))];
        session.document = Some(document);
        session.begin_shape(Point::new(10.0, 10.0));
        session.drag_shape(Point::new(40.0, 20.0), true, false);
        assert_eq!(
            session.shape_draft.as_ref().expect("a draft").rect,
            Rect::new(10.0, 10.0, 30.0, 30.0)
        );
        session.begin_shape(Point::new(50.0, 50.0));
        session.drag_shape(Point::new(60.0, 55.0), false, true);
        assert_eq!(
            session.shape_draft.as_ref().expect("a draft").rect,
            Rect::new(40.0, 45.0, 20.0, 10.0)
        );
    }

    /// A click without a drag draws nothing: the draft never grew a box.
    #[test]
    fn a_click_leaves_no_shape_layer() {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Shape;
        let mut document = compositor_core::document::CanvasDocument::new(64, 64);
        document.layers = vec![ImageLayer::blank("Layer 1", Size::new(64.0, 64.0))];
        session.document = Some(document);
        session.begin_shape(Point::new(10.0, 10.0));
        session.finish_shape();
        assert!(session.shape_draft.is_none());
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 1);
    }

    /// Names skip the ones already in the document.
    #[test]
    fn shape_names_skip_existing_layers() {
        let mut session = EditorSession::default();
        let mut document = compositor_core::document::CanvasDocument::new(64, 64);
        document.layers = vec![
            ImageLayer::blank("Rectangle 1", Size::new(64.0, 64.0)),
            ImageLayer::blank("Ellipse 1", Size::new(64.0, 64.0)),
            ImageLayer::blank("Rectangle 2", Size::new(64.0, 64.0)),
        ];
        session.document = Some(document);
        assert_eq!(session.next_shape_name(ShapeKind::Rectangle), "Rectangle 3");
        assert_eq!(session.next_shape_name(ShapeKind::Ellipse), "Ellipse 2");
        assert_eq!(session.next_shape_name(ShapeKind::Line), "Line 1");
    }

    /// The line path spans its ends; a rectangle fills its whole box.
    #[test]
    fn shape_images_fill_their_geometry() {
        let rectangle = EditorSession::shape_image(
            ShapeKind::Rectangle,
            Size::new(16.0, 16.0),
            PaletteColor::new(1.0, 0.0, 0.0),
            0.0,
            0.0,
            None,
            None,
        )
        .expect("a 16-pixel rectangle fits");
        assert_eq!(rectangle.get(8, 8), [255, 0, 0, 255]);
        assert_eq!(rectangle.get(0, 0), [255, 0, 0, 255], "a square rectangle fills its corners");
        assert_eq!(rectangle.pixel_count(), 256);

        let rounded = EditorSession::shape_image(
            ShapeKind::Rectangle,
            Size::new(16.0, 16.0),
            PaletteColor::new(1.0, 0.0, 0.0),
            8.0,
            0.0,
            None,
            None,
        )
        .expect("a 16-pixel rounded rectangle fits");
        assert_eq!(rounded.get(0, 0)[3], 0, "a large radius empties the corners");
        assert_eq!(rounded.get(8, 8), [255, 0, 0, 255]);

        let line = EditorSession::shape_image(
            ShapeKind::Line,
            Size::new(16.0, 16.0),
            PaletteColor::new(0.0, 0.0, 1.0),
            0.0,
            2.0,
            None,
            None,
        )
        .expect("a 16-pixel line fits");
        // The default ends run corner to corner, inset by half the thickness.
        assert_ne!(line.get(8, 8), [0, 0, 0, 0]);
        assert_eq!(line.get(15, 1)[3], 0, "away from the stroke there is nothing");
    }

    /// A line's stored ends are fractions of its box, so a scaled line lands where it was dragged.
    #[test]
    fn line_ends_are_stored_as_fractions() {
        let line = EditorSession::shape_image(
            ShapeKind::Line,
            Size::new(20.0, 10.0),
            PaletteColor::BLACK,
            0.0,
            2.0,
            Some(Point::new(0.0, 0.0)),
            Some(Point::new(1.0, 1.0)),
        )
        .expect("a 20×10 line fits");
        assert_ne!(line.get(1, 1)[3], 0, "the stroke starts at the stored start point");
        assert_ne!(line.get(19, 9)[3], 0, "and ends at the stored end point");
        assert_eq!(line.get(15, 1)[3], 0, "away from the stroke there is nothing");
    }
}
