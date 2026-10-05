//! The Type tool: `Document/TypeTool.swift`'s `extension EditorSession` — the draft's lifecycle
//! (`beginText`, `applyText`, `finishText`, `cancelText`), the paragraph box, previewing a face, and
//! the style edits that flow through `changeTextStyle` (the commands the on-canvas editor and the
//! Type bar call: color, font, tracking, leading, alignment).
//!
//! The model types (`LayerTextStyle`, `LayerText`, `TextDraft`, the color/font runs) live in
//! `compositor_core::layer_text`. The layout and glyph rasterization that Swift did with
//! `NSLayoutManager`/`NSAttributedString` live in `compositor_pixels::text`: `textAttributes` and
//! `attributedText` have no separate Rust form — their per-run fonts, colors, paragraph line height,
//! alignment and tracking are the rasterizer's input, and `textBoxSize`/`textImage` delegate to it
//! after the same size checks.
//!
//! **Async → sync (docs/PORTING.md §4).** `applyText` threw rather than awaited; the rasterizer is
//! synchronous here, so the commit is inline and the same undo names ("Edit Text", "New Text
//! Layer", "Fill Text") and failure strings result.

use std::sync::Arc;

use compositor_core::buffer::SharedImage;
use compositor_core::color::PaletteColor;
use compositor_core::document::{NavigationTool, ProjectError};
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_text::{LayerText, LayerTextStyle, TextDraft};
use compositor_core::limits;
use compositor_pixels::text;

use crate::clipboard::rgba_thumbnail;
use crate::session::EditorSession;

/// `Int.formatted()`: the grouping the size-limit message spells out ("30,000").
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

/// The user-visible failure for a rasterizer refusal: `ProjectError.invalid`/`.tooLarge`'s wording,
/// as the Swift's `catch` showed them.
fn text_failure(error: text::TextError) -> String {
    match error {
        text::TextError::Invalid => ProjectError::Invalid.to_string(),
        text::TextError::TooLarge => ProjectError::TooLarge.to_string(),
    }
}

impl EditorSession {
    pub fn begin_text(&mut self, point: Point, new_layer: bool) {
        if !self.can_edit_layers()
            || self.text_draft.is_some()
            || self.document.is_none()
            || !(point.x.is_finite() && point.y.is_finite())
        {
            return;
        }
        let visible = self.document.as_ref().expect("checked above").effective_visible_ids();
        let target = if new_layer {
            None
        } else {
            self.document
                .as_ref()
                .expect("checked above")
                .layers
                .iter()
                .rev()
                .find(|layer| {
                    visible.contains(&layer.id) && layer.live_text().is_some() && layer.transform.contains(point)
                })
                .cloned()
        };
        if let Some(target) = target.as_ref() {
            self.select_layer(Some(target.id));
        }
        let mut style = target
            .as_ref()
            .and_then(|layer| layer.live_text())
            .map(|text| text.style)
            .unwrap_or_else(|| self.text_defaults.clone());
        if target.is_none() {
            style.content = String::new();
            style.color_runs = None;
            style.font_runs = None;
            // New text starts in the foreground color, the same as every other tool that lays down
            // color.
            if !self.is_mask_selected {
                let foreground = self.foreground_color();
                style.red = foreground.red;
                style.green = foreground.green;
                style.blue = foreground.blue;
            }
            // A click makes point text: no box of its own, so what is typed decides how big the layer
            // is. Dragging a box out instead (`begin_text_in`) sets boxSize, and so does resizing one
            // by its handles.
            style.box_size = None;
        }
        self.tool = NavigationTool::Type;
        // A click puts new text's first baseline on the pointer, starting at it, as Photoshop's does.
        // A fixed line height leaves its extra room above the letters, so the baseline sits the
        // font's descent up from the bottom of the line.
        let descent = text::face_metrics(&style.font_name, style.font_size).descent;
        let baseline = LayerTextStyle::PADDING + style.line_height() - descent;
        let origin = match target.as_ref() {
            Some(target) => target.origin(),
            None => Point::new(point.x - LayerTextStyle::PADDING, point.y - baseline),
        };
        let document_id = self.document.as_ref().expect("checked above").id;
        self.text_draft = Some(TextDraft::new(
            document_id,
            target.as_ref().map(|layer| layer.id),
            origin,
            target.as_ref().map(|layer| layer.transform),
            style,
        ));
    }

    /// The Type tool's first click on an existing text layer opens it, rather than making a new one.
    pub fn edit_active_text(&mut self) {
        if !self.can_edit_layers() || self.text_draft.is_some() || self.document.is_none() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else { return };
        let Some(text) = layer.live_text() else { return };
        self.tool = NavigationTool::Type;
        let document_id = self.document.as_ref().expect("checked above").id;
        self.text_draft = Some(TextDraft::new(
            document_id,
            Some(layer.id),
            layer.origin(),
            Some(layer.transform),
            text.style,
        ));
    }

    /// Applies a draft: a new text layer, or the layer being edited, as one undo step. False (with
    /// the draft left open) when the document changed under it, the style is invalid, or the pixels
    /// could not be made.
    pub fn apply_text(&mut self, draft: TextDraft) -> bool {
        if self.document.as_ref().map(|document| document.id) != Some(draft.document_id) || !draft.style.is_valid() {
            return false;
        }
        let pending = self.text_draft.take();
        if !self.can_edit_layers() {
            self.text_draft = pending;
            return false;
        }
        // `defer { if !succeeded { textDraft = pending } }`: a refused apply leaves the draft open.
        let mut succeeded = false;
        let result = self.apply_text_inner(&draft, &mut succeeded);
        if !succeeded {
            self.text_draft = pending;
        }
        result
    }

    fn apply_text_inner(&mut self, draft: &TextDraft, succeeded: &mut bool) -> bool {
        if draft.layer_id.is_none() && draft.style.content.trim().is_empty() {
            *succeeded = true;
            return true;
        }
        let image = match EditorSession::text_image(&draft.style) {
            Ok(image) => image,
            Err(error) => {
                self.brush_error = Some(text_failure(error));
                return false;
            }
        };
        let text = LayerText {
            style: draft.style.clone(),
            image: image.clone(),
        };
        if let Some(id) = draft.layer_id {
            let Some(index) = self
                .document
                .as_ref()
                .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
            else {
                return false;
            };
            let Some(layer) = self.document.as_ref().expect("the index came from it").layers.get(index).cloned() else {
                return false;
            };
            let Some(live) = layer.live_text() else { return false };
            let Some(asset) = layer.asset.as_ref() else { return false };
            if live.style == draft.style && (draft.transform.is_none() || draft.transform == Some(layer.transform)) {
                *succeeded = true;
                return true;
            }
            let thumbnail = rgba_thumbnail(&image);
            let mut transform = draft.transform.unwrap_or(layer.transform);
            // Keep the transformed upper-left corner and the user's scale, rotation and flips.
            let anchor = transform.point(Point::ZERO);
            if draft.transform.is_none() || draft.style.box_size.is_none() {
                transform.size = Size::new(
                    image.width() as f64 * transform.size.width / asset.image.width() as f64,
                    image.height() as f64 * transform.size.height / asset.image.height() as f64,
                );
                let moved = transform.point(Point::ZERO);
                transform.origin.x += anchor.x - moved.x;
                transform.origin.y += anchor.y - moved.y;
            }
            if !transform.is_valid() {
                self.brush_error = Some(ProjectError::TooLarge.to_string());
                return false;
            }
            self.begin_edit("Edit Text");
            let mask_transform = layer.mask_transform();
            let document = self.document.as_mut().expect("checked above");
            if document.layers[index]
                .mask
                .as_ref()
                .map(|mask| mask.placement.is_none())
                .unwrap_or(false)
            {
                if let Some(mask) = document.layers[index].mask.as_mut() {
                    mask.placement = Some(mask_transform);
                }
            }
            document.layers[index].asset = Some(ImportedImage::new(PixelImage::Rgba(image.clone()), thumbnail, asset.name.clone()));
            document.layers[index].text = Some(text);
            document.layers[index].transform = transform;
            self.end_edit();
        } else {
            let name = EditorSession::layer_name(&draft.style.content);
            self.add_pixel_layer(image, draft.origin, &name, "New Text Layer", false, None, Some(text));
        }
        *succeeded = true;
        self.text_defaults = draft.style.clone();
        self.text_defaults.color_runs = None;
        self.text_defaults.font_runs = None;
        self.text_draft = None;
        self.canvas_focus_request += 1;
        true
    }

    pub fn finish_text(&mut self) -> bool {
        let Some(draft) = self.text_draft.clone() else { return true };
        self.apply_text(draft)
    }

    pub fn cancel_text(&mut self) {
        self.text_draft = None;
        self.canvas_focus_request += 1;
    }

    /// A dragged-out paragraph box: the draft starts at the box's origin with its fixed bounds, so
    /// what is typed wraps inside it.
    pub fn begin_text_in(&mut self, rect: Rect) {
        if !self.can_edit_layers() || self.text_draft.is_some() || !(rect.width().is_finite() && rect.height().is_finite()) {
            return;
        }
        let mut style = self.text_defaults.clone();
        style.box_size = Some(Size::new(
            16.0f64.max(rect.width().round()),
            16.0f64.max(rect.height().round()),
        ));
        if !style.box_is_valid() {
            self.brush_error = Some(format!(
                "That text box exceeds the {}-pixel or {}-megapixel limit.",
                grouped(limits::MAX_SIDE),
                limits::max_surface_megapixels()
            ));
            return;
        }
        self.begin_text(Point::new(rect.min_x(), rect.min_y()), true);
        // A dragged box is exactly where it was drawn.
        if let Some(draft) = self.text_draft.as_mut() {
            draft.origin = Point::new(rect.min_x(), rect.min_y());
            draft.style.box_size = style.box_size;
        }
    }

    /// Paints a text layer's letters in `color`, keeping it editable text. Used by Fill with
    /// Foreground/Background; false when the layer isn't live text or its pixels couldn't be
    /// redrawn, so the caller fills as usual.
    pub fn recolor_text(&mut self, id: compositor_core::Id, color: PaletteColor) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
        else {
            return false;
        };
        let Some(layer) = self.document.as_ref().expect("the index came from it").layers.get(index).cloned() else {
            return false;
        };
        let Some(live) = layer.live_text() else { return false };
        let Some(asset) = layer.asset.as_ref() else { return false };
        let mut style = live.style.clone();
        if style.red == color.red && style.green == color.green && style.blue == color.blue && style.color_runs.is_none() {
            return true;
        }
        style.set_color(color, compositor_core::layer_text::TextRange::EMPTY);
        if !style.is_valid() {
            return false;
        }
        let Ok(image) = EditorSession::text_image(&style) else { return false };
        let thumbnail = rgba_thumbnail(&image);
        self.finish_opacity_edit();
        self.begin_edit("Fill Text");
        let document = self.document.as_mut().expect("checked above");
        document.layers[index].asset = Some(ImportedImage::new(PixelImage::Rgba(image.clone()), thumbnail, asset.name.clone()));
        document.layers[index].text = Some(LayerText { style, image });
        self.end_edit();
        true
    }

    pub fn current_text_style(&self) -> LayerTextStyle {
        if let Some(draft) = self.text_draft.as_ref() {
            return draft.style.clone();
        }
        if let Some(style) = self.active_layer().and_then(|layer| layer.live_text()).map(|text| text.style) {
            return style;
        }
        self.text_defaults.clone()
    }

    /// While the font menu is open, the text being edited shows the face under the pointer;
    /// `end_font_preview` puts it back. Only text already being edited: a selected text layer isn't
    /// opened for a preview.
    pub fn preview_font(&mut self, name: &str) {
        let Some(draft) = self.text_draft.clone() else { return };
        let original = self.font_preview_original.clone().unwrap_or_else(|| draft.style.clone());
        self.font_preview_original = Some(original.clone());
        let mut style = original;
        style.set_font(name, draft.selection);
        if !style.is_valid() || style == draft.style {
            return;
        }
        let mut draft = draft;
        draft.style = style;
        self.text_draft = Some(draft);
    }

    /// The previewed face was chosen: keep the text as it shows, rather than putting it back and
    /// applying it again.
    pub fn keep_font_preview(&mut self) {
        self.font_preview_original = None;
    }

    pub fn end_font_preview(&mut self) {
        let Some(original) = self.font_preview_original.take() else { return };
        if let Some(draft) = self.text_draft.clone() {
            if draft.style != original {
                let mut draft = draft;
                draft.style = original;
                self.text_draft = Some(draft);
            }
        }
    }

    /// Every style change the Type bar and the on-canvas editor make: it applies to the open draft,
    /// or opens the active text layer, or lands in the defaults for the next text. An invalid result
    /// leaves the previous style in place.
    pub fn change_text_style(&mut self, change: impl FnOnce(&mut LayerTextStyle)) {
        if self.text_draft.is_none() && self.active_layer().map(|layer| layer.live_text().is_some()).unwrap_or(false) {
            self.edit_active_text();
        }
        if let Some(original) = self.text_draft.clone() {
            let mut draft = original.clone();
            change(&mut draft.style);
            if draft.style.is_valid() {
                self.text_draft = Some(draft);
            } else {
                self.text_draft = Some(original);
            }
        } else {
            let mut style = self.text_defaults.clone();
            change(&mut style);
            if style.is_valid() {
                self.text_defaults = style;
            }
        }
    }

    /// A text layer's name: its first words on one line. Line breaks and runs of spaces become
    /// single spaces, so a paragraph never makes the row in the Layers panel taller than one line.
    pub fn layer_name(content: &str) -> String {
        let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if flattened.is_empty() {
            "Text".to_string()
        } else {
            flattened.chars().take(40).collect()
        }
    }

    /// How big the text layer is: a fixed box's bounds, or what point text measures plus its padding.
    /// A caret's worth of width so an empty line still has somewhere to type.
    pub fn text_box_size(style: &LayerTextStyle) -> Size {
        text::text_box_size(style)
    }

    /// The text as pixels, with the Swift's own size checks.
    pub fn text_image(style: &LayerTextStyle) -> Result<SharedImage, text::TextError> {
        if !style.is_valid() {
            return Err(text::TextError::Invalid);
        }
        let size = EditorSession::text_box_size(style);
        let width = size.width.ceil();
        let height = size.height.ceil();
        if !(width.is_finite() && height.is_finite())
            || width < 1.0
            || height < 1.0
            || width > limits::MAX_SIDE_EXTENT
            || height > limits::MAX_SIDE_EXTENT
            || width * height > limits::MAX_SURFACE_EXTENT
        {
            return Err(text::TextError::TooLarge);
        }
        text::text_image(style).map(Arc::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::document::CanvasDocument;
    use compositor_core::layer_text::TextRange;

    fn session() -> EditorSession {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Type;
        let mut document = CanvasDocument::new(64, 64);
        document.layers = vec![compositor_core::document::ImageLayer::blank("Layer 1", Size::new(64.0, 64.0))];
        session.document = Some(document);
        session
    }

    /// A layer name is the text's first words on one line, at most 40 characters.
    #[test]
    fn layer_names_flatten_and_truncate() {
        assert_eq!(EditorSession::layer_name("Hello world"), "Hello world");
        assert_eq!(EditorSession::layer_name("Hello\nworld"), "Hello world");
        assert_eq!(EditorSession::layer_name("  a   b  \n c "), "a b c");
        assert_eq!(EditorSession::layer_name("   \n  "), "Text");
        assert_eq!(EditorSession::layer_name(""), "Text");
        let long = "x".repeat(60);
        assert_eq!(EditorSession::layer_name(&long).chars().count(), 40);
    }

    /// A draft left empty makes no layer and applies cleanly.
    #[test]
    fn an_empty_new_draft_applies_without_a_layer() {
        let mut session = session();
        session.begin_text(Point::new(10.0, 10.0), true);
        let draft = session.text_draft.clone().expect("a draft");
        assert_eq!(draft.style.content, "");
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 1);
        assert!(session.apply_text(draft));
        assert!(session.text_draft.is_none());
    }

    /// Cancelling closes the draft and hands the keys back to the canvas.
    #[test]
    fn cancel_text_closes_the_draft() {
        let mut session = session();
        session.begin_text(Point::new(10.0, 10.0), true);
        assert!(session.text_draft.is_some());
        let focus = session.canvas_focus_request;
        session.cancel_text();
        assert!(session.text_draft.is_none());
        assert_eq!(session.canvas_focus_request, focus + 1);
    }

    /// With no draft open, a style change lands in the defaults; an invalid one is refused.
    #[test]
    fn text_style_changes_land_in_the_defaults() {
        let mut session = session();
        session.change_text_style(|style| style.font_size = 24.0);
        assert_eq!(session.text_defaults.font_size, 24.0);
        session.change_text_style(|style| style.font_size = 0.5);
        assert_eq!(session.text_defaults.font_size, 24.0, "an invalid style leaves the previous one");
    }

    /// An open draft hears the change and keeps the caret's selection.
    #[test]
    fn text_style_changes_follow_the_open_draft() {
        let mut session = session();
        session.begin_text(Point::new(10.0, 10.0), true);
        let id = session.text_draft.as_ref().expect("a draft").id;
        session.change_text_style(|style| style.tracking = 4.0);
        let draft = session.text_draft.as_ref().expect("a draft");
        assert_eq!(draft.id, id);
        assert_eq!(draft.style.tracking, 4.0);
        assert_eq!(session.text_defaults.tracking, 0.0, "the defaults wait for the apply");
    }

    /// The font menu previews a face on the open text and puts it back on Cancel; choosing it keeps
    /// the previewed face.
    #[test]
    fn font_preview_round_trips() {
        let mut session = session();
        session.begin_text(Point::new(10.0, 10.0), true);
        session.text_draft.as_mut().expect("a draft").selection = TextRange::new(0, 0);
        session.preview_font("Courier");
        assert_eq!(session.text_draft.as_ref().expect("a draft").style.font_name, "Courier");
        session.end_font_preview();
        assert_eq!(session.text_draft.as_ref().expect("a draft").style.font_name, "Helvetica");

        session.preview_font("Courier");
        session.keep_font_preview();
        assert_eq!(session.text_draft.as_ref().expect("a draft").style.font_name, "Courier");
    }

    /// Point text's box is what it measures plus the padding; a fixed box answers with its own size.
    #[test]
    fn text_box_size_pads_point_text_and_honors_a_fixed_box() {
        let mut style = LayerTextStyle::default();
        style.content = "Hi".to_string();
        let point = EditorSession::text_box_size(&style);
        assert!(point.width >= 16.0 && point.height >= 16.0);

        style.box_size = Some(Size::new(200.0, 120.0));
        assert_eq!(EditorSession::text_box_size(&style), Size::new(200.0, 120.0));
    }

    /// A dragged-out box must be at least 16 pixels on a side, and is refused past the limits.
    #[test]
    fn paragraph_boxes_are_clamped_and_refused() {
        let mut session = session();
        session.begin_text_in(Rect::new(4.0, 5.0, 4.0, 3.0));
        let draft = session.text_draft.as_ref().expect("a draft");
        assert_eq!(draft.origin, Point::new(4.0, 5.0));
        assert_eq!(draft.style.box_size, Some(Size::new(16.0, 16.0)));

        let mut session = session();
        session.begin_text_in(Rect::new(0.0, 0.0, 40_000.0, 20.0));
        assert!(session.text_draft.is_none());
        assert_eq!(
            session.brush_error.as_deref(),
            Some("That text box exceeds the 30,000-pixel or 200-megapixel limit.")
        );
    }
}
