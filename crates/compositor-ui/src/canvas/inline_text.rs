//! The on-canvas text editor (`Rendering/InlineTextEditor.swift`).
//!
//! A native text system on the canvas: its logical bounds are layer pixels and the containing view
//! supplies the zoom. The Swift view's glyphs were clear — the canvas draws the text as the layer's
//! own pixels underneath — and only the box, its handles, the overflow marker and the caret and
//! selection marks were visible here.
//!
//! `NSLayoutManager`'s wrapping, caret, selection and hit-testing maths live in
//! `compositor_pixels::text` ([`TextLayout`]), so this module owns the editor's geometry (the Swift
//! `synchronize(_:)`, `handle(at:)`, `edgeReach`, the resize drag) and paints what the AppKit view
//! drew: the accent box, the eight handles, the plus in the bottom-right handle when text overflows
//! its box, the caret and the selection highlight.
//!
//! **Substitutions.** gpui cannot rotate an element or its glyphs, so the box and its handles are
//! drawn unrotated at the draft transform's anchor (the Swift set `frameRotation`); the text surface
//! itself is drawn from `TextLayout` rather than an `NSTextView`, so marked text/IME and the
//! `NSTextView` undo stack are not reproduced — the draft's content, caret and selection are edited
//! through [`EditorSession::change_text_style`] and the draft's `selection`.

use compositor_core::geom::{Point, Rect, Size};
use compositor_core::layer_text::{LayerTextStyle, TextDraft, TextRange};
use compositor_core::layer_transform::LayerTransform;
use compositor_core::viewport::CanvasViewport;
use compositor_pixels::text::TextLayout;
use compositor_session::EditorSession;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use super::overlays::canvas_lines::{fill_rect, stroke_polyline, stroke_rect, view_to_window};

/// The editor's handle size at 1:1 (`handleSize = 6` before the zoom and layer-scale division).
pub const HANDLE_SIZE: f64 = 6.0;
/// How far past an edge counts as that edge, as a multiple of the handle (`10 / 6`).
const EDGE_REACH_FACTOR: f64 = 10.0 / 6.0;
/// The overflow marker's arms (`arm = handleSize * 0.42`).
const PLUS_ARM: f64 = 0.42;

/// The on-canvas editor's shown geometry, the port of `InlineTextEditor`'s stored properties.
#[derive(Clone, Debug, Default)]
pub struct InlineTextEditor {
    /// The draft the editor is showing (`draftID`).
    draft_id: Option<compositor_core::Id>,
    /// The size the text is laid out in, in document pixels (`logicalSize`).
    logical_size: Size,
    /// The transform the editor is actually showing (`shownTransform`): point text grows as it is
    /// typed, so this is not always the draft's own transform.
    shown_transform: Option<LayerTransform>,
    /// The handle size in the box's own units (`handleSize`).
    handle_size: f64,
    /// What the box is measured from, cached like `measuredStyle`/`measuredSize`.
    measured_style: Option<LayerTextStyle>,
    measured_size: Size,
    /// The view scale and anchor the geometry was built at (`Geometry`).
    scale: f64,
    anchor: Point,
}

impl InlineTextEditor {
    pub fn new() -> Self {
        Self {
            logical_size: Size::new(360.0, 160.0),
            handle_size: HANDLE_SIZE,
            ..Self::default()
        }
    }

    pub fn draft_id(&self) -> Option<compositor_core::Id> {
        self.draft_id
    }

    pub fn logical_size(&self) -> Size {
        self.logical_size
    }

    pub fn shown_transform(&self) -> Option<LayerTransform> {
        self.shown_transform
    }

    pub fn handle_size(&self) -> f64 {
        self.handle_size
    }

    /// `synchronize(_ draft:)`: the editor's geometry for a draft, reading the layer it edits.
    pub fn synchronize(
        &mut self,
        draft: &TextDraft,
        layer: Option<&compositor_core::document::ImageLayer>,
        viewport: &CanvasViewport,
        document_size: Size,
    ) {
        let fresh = self.draft_id != Some(draft.id);
        self.draft_id = Some(draft.id);
        let style = &draft.style;
        // Point text has no box: it is as big as what has been typed, growing as it is typed.
        if let Some(box_size) = style.box_size {
            self.logical_size = box_size;
        } else {
            if self.measured_style.as_ref() != Some(style) {
                self.measured_size = EditorSession::text_box_size(style);
                self.measured_style = Some(style.clone());
            }
            self.logical_size = self.measured_size;
        }
        let mut transform = draft.transform.unwrap_or_else(|| LayerTransform {
            origin: draft.origin,
            size: self.logical_size,
            ..LayerTransform::default()
        });
        // Point text already on a layer grows as it is typed too, keeping whatever scale the layer
        // was given.
        if style.box_size.is_none() && draft.transform.is_some() {
            if let Some(asset) = layer.and_then(|layer| layer.asset.as_ref()) {
                let image_width = asset.image.width() as f64;
                if image_width > 0.0 {
                    let factor = transform.size.width / image_width;
                    // A rotated layer turns about its center, so growing it swings its corner away
                    // and the text drifts as it is typed. The top-left corner is put back where it
                    // was, which is where the commit leaves it too.
                    let anchor = transform.point(Point::ZERO);
                    transform.size = Size::new(
                        self.logical_size.width * factor,
                        self.logical_size.height * factor,
                    );
                    let moved = transform.point(Point::ZERO);
                    transform.origin.x += anchor.x - moved.x;
                    transform.origin.y += anchor.y - moved.y;
                }
            }
        }
        self.shown_transform = Some(transform);
        self.scale = viewport.points_per_pixel();
        self.anchor = viewport.view_point(transform.point(Point::ZERO), document_size);
        self.handle_size = 2.0f64.max(
            HANDLE_SIZE / 0.01f64.max(self.scale * transform.size.width / self.logical_size.width),
        );
        let _ = fresh;
    }

    /// The editor's frame in view points (`frame`): anchored at the transform's top-left corner,
    /// sized by the logical size at the view scale.
    pub fn frame(&self) -> Option<Rect> {
        let transform = self.shown_transform?;
        Some(Rect::new(
            self.anchor.x,
            self.anchor.y,
            transform.size.width * self.scale,
            transform.size.height * self.scale,
        ))
    }

    /// How far either side of an edge counts as that edge, in the box's own units (`edgeReach`).
    /// Capped so a small box keeps a middle to type in.
    pub fn edge_reach(&self) -> f64 {
        let bounds = self.logical_size;
        (self.handle_size * EDGE_REACH_FACTOR).min(bounds.width.min(bounds.height) / 3.0)
    }

    /// The edge or corner at a point, in handle order (`handle(at:)`): a band along each edge, as
    /// the Move tool's box has, rather than only the handle squares. Nil anywhere else — the text.
    pub fn handle_at(&self, point: Point) -> Option<usize> {
        let reach = self.edge_reach();
        let bounds = self.logical_size;
        let left = point.x <= reach;
        let right = point.x >= bounds.width - reach;
        let top = point.y <= reach;
        let bottom = point.y >= bounds.height - reach;
        if !(point.x >= -reach
            && point.x <= bounds.width + reach
            && point.y >= -reach
            && point.y <= bounds.height + reach)
        {
            return None;
        }
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(0),
            (_, true, true, _) => Some(2),
            (_, true, _, true) => Some(4),
            (true, _, _, true) => Some(6),
            (_, _, true, _) => Some(1),
            (_, true, _, _) => Some(3),
            (_, _, _, true) => Some(5),
            (true, _, _, _) => Some(7),
            _ => None,
        }
    }

    /// The arrows for the edge or corner a handle resizes, turned with the text box
    /// (`handleCursor(_:)`).
    pub fn handle_cursor(&self, index: usize, rotation: f64) -> CursorStyle {
        // `.topLeft, .top, .topRight, .right`, then the same four turned.
        const ORDERED: [CursorStyle; 4] = [
            CursorStyle::ResizeUpLeftDownRight,
            CursorStyle::ResizeUpDown,
            CursorStyle::ResizeUpRightDownLeft,
            CursorStyle::ResizeLeftRight,
        ];
        const POSITIONS: [usize; 8] = [0, 1, 2, 3, 0, 1, 2, 3];
        let turns = (((rotation / 45.0).round() as i64) % 8 + 8) % 8;
        let position = (POSITIONS[index] + turns as usize) % 4;
        ORDERED[position]
    }

    /// Whether the text doesn't fit its box, marked by a plus in the bottom-right handle
    /// (`draw(_:)`'s `NSMaxRange(range) < layout.numberOfGlyphs`).
    pub fn text_overflows(&self, style: &LayerTextStyle) -> bool {
        style.box_size.is_some() && TextLayout::layout(style).truncated()
    }
}

/// A draft being resized by one of the editor's handles (`resize`).
#[derive(Clone)]
pub struct TextResize {
    pub handle: usize,
    pub draft: TextDraft,
    pub transform: LayerTransform,
    pub start: Point,
}

impl TextResize {
    /// `mouseDragged`: the draft's box and transform after dragging the handle to `point`
    /// (document pixels). Nil when the box or transform would be invalid.
    pub fn dragged(
        &self,
        point: Point,
        logical_size: Size,
        shown_transform: LayerTransform,
    ) -> Option<TextDraft> {
        let old = self.transform;
        let dx = point.x - self.start.x;
        let dy = point.y - self.start.y;
        let local_x = dx * old.radians().cos() + dy * old.radians().sin();
        let local_y = -dx * old.radians().sin() + dy * old.radians().cos();
        let unit = LayerTransform::HANDLES[self.handle];
        let mut left = 0.0f64;
        let mut top = 0.0f64;
        let mut right = old.size.width;
        let mut bottom = old.size.height;
        let source = self.draft.style.box_size.unwrap_or(logical_size);
        let min_width = 16.0 * old.size.width / source.width;
        let min_height = 16.0 * old.size.height / source.height;
        if unit.x == 0.0 {
            left = local_x.min(right - min_width);
        }
        if unit.x == 1.0 {
            right = (left + min_width).max(right + local_x);
        }
        if unit.y == 0.0 {
            top = local_y.min(bottom - min_height);
        }
        if unit.y == 1.0 {
            bottom = (top + min_height).max(bottom + local_y);
        }
        let mut draft = self.draft.clone();
        draft.style.box_size = Some(Size::new(
            ((right - left) * source.width / old.size.width).round(),
            ((bottom - top) * source.height / old.size.height).round(),
        ));
        if !draft.style.box_is_valid() {
            return None;
        }
        let box_size = draft.style.box_size.expect("just set");
        let mut transform = shown_transform;
        transform.size = Size::new(
            box_size.width * old.size.width / source.width,
            box_size.height * old.size.height / source.height,
        );
        let anchor = old.point(Point::new(left / old.size.width, top / old.size.height));
        let current = transform.point(Point::ZERO);
        transform.origin.x += anchor.x - current.x;
        transform.origin.y += anchor.y - current.y;
        if !transform.is_valid() {
            return None;
        }
        draft.origin = transform.origin;
        draft.transform = Some(transform);
        Some(draft)
    }
}

/// The on-canvas editor over the composite, shown while a text draft is open.
pub fn inline_text_editor(session: Entity<EditorSession>) -> InlineTextEditorView {
    InlineTextEditorView { session }
}

#[derive(IntoElement)]
pub struct InlineTextEditorView {
    session: Entity<EditorSession>,
}

impl RenderOnce for InlineTextEditorView {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let session = self.session;
        let accent = cx.theme().primary;
        // The editor's geometry is rebuilt every render, exactly as `synchronize(_:)` did.
        let editor = {
            let session = session.read(cx);
            session.text_draft.as_ref().map(|draft| {
                let document = session.document.as_ref();
                let layer = document.and_then(|document| {
                    draft
                        .layer_id
                        .and_then(|id| document.layers.iter().find(|layer| layer.id == id))
                });
                let mut editor = InlineTextEditor::new();
                editor.synchronize(
                    draft,
                    layer,
                    &session.viewport,
                    document.map(|document| document.size()).unwrap_or(Size::ZERO),
                );
                (editor, draft.style.clone(), draft.selection)
            })
        };
        let Some((editor, style, selection)) = editor else {
            return div().absolute().size_full().into_any_element();
        };
        let layout = TextLayout::layout(&style);
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let session = session.read(cx);
                let Some(transform) = editor.shown_transform() else {
                    return;
                };
                let origin = bounds.origin;
                let scale = session.viewport.points_per_pixel();
                let frame = editor.frame().unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
                let handle_size = editor.handle_size();
                let accent: Rgba = accent.into();
                let white: Rgba = gpui_kit::white().into();
                // `draw(_:)`: the accent box inset by handleSize / 12, stroked at handleSize / 6.
                let inset = handle_size / 12.0;
                let box_rect = Rect::new(
                    frame.min_x() + inset,
                    frame.min_y() + inset,
                    frame.width() - inset * 2.0,
                    frame.height() - inset * 2.0,
                );
                stroke_rect(window, origin, box_rect, (handle_size / 6.0) as f32, accent);
                for unit in LayerTransform::HANDLES {
                    let rect = Rect::new(
                        unit.x * frame.width() - handle_size / 2.0,
                        unit.y * frame.height() - handle_size / 2.0,
                        handle_size,
                        handle_size,
                    );
                    let rect = Rect::new(
                        frame.min_x() + rect.min_x(),
                        frame.min_y() + rect.min_y(),
                        rect.width(),
                        rect.height(),
                    );
                    fill_rect(window, origin, rect, white);
                    stroke_rect(window, origin, rect, 1.0, accent);
                }
                // Text that doesn't fit is marked by a plus drawn in the bottom-right handle, as in
                // Photoshop.
                if editor.text_overflows(&style) {
                    let unit = LayerTransform::HANDLES[4];
                    let center = Point::new(
                        frame.min_x() + unit.x * frame.width(),
                        frame.min_y() + unit.y * frame.height(),
                    );
                    let arm = handle_size * PLUS_ARM;
                    let window_center = view_to_window(origin, center);
                    let black: Rgba = gpui_kit::black().into();
                    stroke_polyline(
                        window,
                        &[
                            point(window_center.x - px(arm as f32), window_center.y),
                            point(window_center.x + px(arm as f32), window_center.y),
                        ],
                        false,
                        (handle_size / 6.0) as f32,
                        black,
                    );
                    stroke_polyline(
                        window,
                        &[
                            point(window_center.x, window_center.y - px(arm as f32)),
                            point(window_center.x, window_center.y + px(arm as f32)),
                        ],
                        false,
                        (handle_size / 6.0) as f32,
                        black,
                    );
                }
                // The caret and the selection the canvas draws beneath the clear glyphs.
                let padding = LayerTextStyle::PADDING;
                for rect in layout.selection_rects(selection) {
                    let view = Rect::new(
                        frame.min_x() + (rect.min_x() + padding) * scale,
                        frame.min_y() + (rect.min_y() + padding) * scale,
                        rect.width() * scale,
                        rect.height() * scale,
                    );
                    fill_rect(
                        window,
                        origin,
                        view,
                        Rgba {
                            a: 0.45,
                            ..accent
                        },
                    );
                }
                if selection.length == 0 {
                    let caret = layout.caret_rect(selection.location);
                    let color = style.color(if selection.location > 0 {
                        selection.location - 1
                    } else {
                        0
                    });
                    let caret_rect = Rect::new(
                        frame.min_x() + (caret.min_x() + padding) * scale,
                        frame.min_y() + (caret.min_y() + padding) * scale,
                        caret.width().max(1.0) * scale,
                        caret.height() * scale,
                    );
                    fill_rect(window, origin, caret_rect, srgb(color));
                }
                let _ = transform;
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
}

/// A [`compositor_core::color::PaletteColor`] as gpui's [`Rgba`], fully opaque (`color(at:)`).
fn srgb(color: compositor_core::color::PaletteColor) -> Rgba {
    Rgba {
        r: color.red as f32,
        g: color.green as f32,
        b: color.blue as f32,
        a: 1.0,
    }
}

/// The draft's selection as a [`TextRange`] (`textView.selectedRange()`).
pub fn draft_selection(draft: &TextDraft) -> TextRange {
    draft.selection
}
