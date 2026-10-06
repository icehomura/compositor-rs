//! The document rulers and their corner (`UI/CanvasRulers.swift`).
//!
//! The AppKit `CanvasRulerNSView` is a gpui element here: it paints its ticks and labels into a
//! `canvas` child and pulls guides out of the session with the same pointer handling the Swift view
//! had. `CanvasRuler` itself — the strip's thickness and the 1-2-5 tick arithmetic — is the
//! session's port (`compositor_session::guides`), re-exported here for the content view's layout.
//!
//! **Substitution.** gpui has no glyph rotation (no rotated-text primitives, no element transform
//! outside SVG icons), so the vertical ruler's labels are drawn unrotated at the tick instead of
//! the Swift view's -90° rotation; their text, font size, color and position along the strip are
//! otherwise the Swift ones.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use compositor_core::geom::Point;
use compositor_core::guides::CanvasGuideAxis;
use compositor_session::EditorSession;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
// gpui's own point, in window pixels, as opposed to core's `Point` (view space, CGFloat points).
use gpui_kit::Point as WindowPoint;

pub use compositor_session::guides::CanvasRuler;

use crate::canvas::overlays::gray_rgba;

/// The ruler strip's background (`NSColor(white: 0.2, alpha: 1)`).
const STRIP_GRAY: f32 = 0.2;
/// The separator line along the canvas side (`NSColor(white: 0.08, alpha: 1)`).
const EDGE_GRAY: f32 = 0.08;
/// A tick (`NSColor(white: 0.62, alpha: 1)`).
const TICK_GRAY: f32 = 0.62;
/// A label (`NSColor(white: 0.78, alpha: 1)`).
const LABEL_GRAY: f32 = 0.78;
/// The corner's diagonal (`Color.white.opacity(0.28)`).
const CORNER_LINE_ALPHA: f32 = 0.28;
/// The labels' font size (`NSFont.monospacedDigitSystemFont(ofSize: 8, weight: .regular)`).
const LABEL_SIZE: f32 = 8.0;
/// A major tick's length, and the mid and minor ticks' (`length: isMajor ? 8 : isMid ? 5 : 3`).
const MAJOR_TICK: f32 = 8.0;
const MID_TICK: f32 = 5.0;
const MINOR_TICK: f32 = 3.0;

/// `CanvasRulerCorner`: the 18×18 square between the two rulers, with its white diagonal.
#[derive(IntoElement)]
pub struct CanvasRulerCorner;

impl CanvasRulerCorner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CanvasRulerCorner {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for CanvasRulerCorner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // `Rectangle().fill(Color(white: 0.2)).overlay(alignment: .bottomTrailing) { Path … }`
        // with `.frame(width: thickness, height: thickness)`.
        div()
            .flex_none()
            .w(px(CanvasRuler::THICKNESS as f32))
            .h(px(CanvasRuler::THICKNESS as f32))
            .bg(gray_rgba(STRIP_GRAY, 1.0))
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, _| {
                        let thickness = CanvasRuler::THICKNESS as f32;
                        let mut builder = PathBuilder::stroke(px(1.0));
                        builder.move_to(point(
                            bounds.origin.x + px(5.0),
                            bounds.origin.y + px(thickness - 4.0),
                        ));
                        builder.line_to(point(
                            bounds.origin.x + px(thickness - 4.0),
                            bounds.origin.y + px(5.0),
                        ));
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, gray_rgba(1.0, CORNER_LINE_ALPHA));
                        }
                    },
                )
                .size_full(),
            )
    }
}

/// `CanvasRulerView`: a strip along the canvas that draws the document's ticks and pulls guides.
#[derive(IntoElement)]
pub struct CanvasRulerView {
    session: Entity<EditorSession>,
    axis: CanvasGuideAxis,
}

impl CanvasRulerView {
    pub fn new(session: Entity<EditorSession>, axis: CanvasGuideAxis) -> Self {
        Self { session, axis }
    }
}

impl RenderOnce for CanvasRulerView {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let session = self.session;
        let axis = self.axis;
        let horizontal = axis == CanvasGuideAxis::Horizontal;
        // The strip's own origin in the window, so pointer events become canvas-local points.
        // `CanvasRulerNSView` asked the canvas view to convert for it; the layout puts the canvas
        // exactly one strip thickness down-right of the rulers' shared corner, so the conversion is
        // that constant offset.
        let origin = Rc::new(Cell::new(point(px(0.0), px(0.0))));

        let paint_origin = origin.clone();
        let down_origin = origin.clone();
        let move_origin = origin.clone();
        let up_origin = origin.clone();
        let down_session = session.clone();
        let move_session = session.clone();
        let up_session = session.clone();

        div()
            .relative()
            .flex_none()
            .when(horizontal, |this| {
                this.h(px(CanvasRuler::THICKNESS as f32)).flex_1()
            })
            .when(!horizontal, |this| this.w(px(CanvasRuler::THICKNESS as f32)))
            .bg(gray_rgba(STRIP_GRAY, 1.0))
            .cursor(if horizontal {
                CursorStyle::ResizeUpDown
            } else {
                CursorStyle::ResizeLeftRight
            })
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        paint_origin.set(bounds.origin);
                    },
                    move |bounds, _, window, cx| {
                        paint_ruler(&session, axis, bounds, window, cx);
                    },
                )
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _window, cx| {
                // `window?.makeFirstResponder(canvasView())`: the canvas takes the keyboard the way
                // a changed `canvasFocusRequest` makes `CanvasView` focus itself.
                down_session.update(cx, |session, _| session.canvas_focus_request += 1);
                let canvas_point = canvas_point(event.position, down_origin.get(), axis);
                let position = down_session.read_with(cx, |session, _| {
                    session.guide_position_for(axis, canvas_point)
                });
                let Some(position) = position else {
                    return;
                };
                down_session.update(cx, |session, _| session.begin_guide_creation(axis, position));
            })
            .on_mouse_move(move |event: &MouseMoveEvent, _window, cx| {
                let canvas_point = canvas_point(event.position, move_origin.get(), axis);
                let position = move_session.read_with(cx, |session, _| {
                    if session.guide_drag.is_none() {
                        return None;
                    }
                    session.guide_position_for(axis, canvas_point)
                });
                if let Some(position) = position {
                    move_session.update(cx, |session, _| session.move_guide_drag(position));
                }
            })
            .on_mouse_up(MouseButton::Left, move |event: &MouseUpEvent, _window, cx| {
                let canvas_point = canvas_point(event.position, up_origin.get(), axis);
                up_session.update(cx, |session, _| {
                    if session.guide_drag.is_none() {
                        return;
                    }
                    let delete = session.is_over_ruler(canvas_point);
                    session.finish_guide_drag(delete);
                });
            })
    }
}

/// A window-space pointer position as a canvas-local point
/// (`canvas.convert(event.locationInWindow, from: nil)`): everything one strip thickness past the
/// shared corner.
fn canvas_point(
    position: WindowPoint<Pixels>,
    ruler_origin: WindowPoint<Pixels>,
    axis: CanvasGuideAxis,
) -> Point {
    let local_x = position.x - ruler_origin.x;
    let local_y = position.y - ruler_origin.y;
    if axis == CanvasGuideAxis::Horizontal {
        // The horizontal ruler shares the canvas's left edge; the canvas sits one strip below it.
        Point::new(
            f64::from(local_x),
            f64::from(local_y) - CanvasRuler::THICKNESS,
        )
    } else {
        // The vertical ruler shares the canvas's top edge; the canvas sits one strip to its right.
        Point::new(
            f64::from(local_x) - CanvasRuler::THICKNESS,
            f64::from(local_y),
        )
    }
}

/// `CanvasRulerNSView.draw(_:)`: the ticks, the major labels and the canvas-side separator.
fn paint_ruler(
    session: &Entity<EditorSession>,
    axis: CanvasGuideAxis,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let session = session.read(cx);
    let Some(document) = session.document.as_ref() else {
        return;
    };
    let document_size = document.size();
    // A value, so the session borrow ends here: the labels' paint needs `cx` mutably.
    let viewport = session.viewport;
    let scale = viewport.points_per_pixel();
    let step = CanvasRuler::major_step(scale);
    let minor = step / 10.0;
    let hairline = 1.0 / window.scale_factor().max(1.0) as f32;
    let width = f64::from(bounds.size.width);
    let height = f64::from(bounds.size.height);

    let (start, end) = if axis == CanvasGuideAxis::Horizontal {
        (
            viewport
                .document_point(Point::new(0.0, 0.0), document_size)
                .x,
            viewport
                .document_point(Point::new(width, 0.0), document_size)
                .x,
        )
    } else {
        (
            viewport
                .document_point(Point::new(0.0, 0.0), document_size)
                .y,
            viewport
                .document_point(Point::new(0.0, height), document_size)
                .y,
        )
    };
    let first = (start.min(end) / minor).floor() * minor;
    let last = (start.max(end) / minor).ceil() * minor;
    if !(minor > 0.0 && last.is_finite() && first.is_finite()) {
        return;
    }

    let tick_color = gray_rgba(TICK_GRAY, 1.0);
    let label_color: Hsla = gray_rgba(LABEL_GRAY, 1.0).into();
    // `NSFont.monospacedDigitSystemFont(ofSize: 8, weight: .regular)`: the system face with
    // tabular figures.
    let label_font = Font {
        features: FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])),
        ..font(".SystemUIFont")
    };
    let font_size = px(LABEL_SIZE);
    let line_height = px(LABEL_SIZE);

    let mut value = first;
    while value <= last + 0.001 {
        let view = if axis == CanvasGuideAxis::Horizontal {
            viewport
                .view_point(Point::new(value, 0.0), document_size)
                .x
        } else {
            viewport
                .view_point(Point::new(0.0, value), document_size)
                .y
        };
        let remainder = (value % step).abs();
        let is_major = remainder < 0.001 || (remainder - step).abs() < 0.001;
        let is_mid = !is_major && (value % (step / 2.0)).abs() < 0.001;
        let length = if is_major {
            MAJOR_TICK
        } else if is_mid {
            MID_TICK
        } else {
            MINOR_TICK
        };
        if axis == CanvasGuideAxis::Horizontal {
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        bounds.origin.x + px(view as f32 - hairline / 2.0),
                        bounds.origin.y + px(f32::from(bounds.size.height) - length),
                    ),
                    size(px(hairline), px(length)),
                ),
                tick_color,
            ));
        } else {
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        bounds.origin.x + px(f32::from(bounds.size.width) - length),
                        bounds.origin.y + px(view as f32 - hairline / 2.0),
                    ),
                    size(px(length), px(hairline)),
                ),
                tick_color,
            ));
        }
        if is_major {
            let label = CanvasRuler::label(value);
            let run = TextRun {
                len: label.len(),
                font: label_font.clone(),
                color: label_color,
                ..TextRun::default()
            };
            let line = window
                .text_system()
                .shape_line(label.into(), font_size, &[run], None);
            if axis == CanvasGuideAxis::Horizontal {
                let origin = point(
                    bounds.origin.x + px(view as f32 + 2.0),
                    bounds.origin.y + line.ascent,
                );
                let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
            } else {
                // The Swift rotates the label -90° here (drawn downward from the tick); gpui has no
                // glyph rotation, so it is drawn unrotated at the same spot along the strip.
                let origin = point(
                    bounds.origin.x + px(1.0),
                    bounds.origin.y + px(view as f32 + 2.0) + line.ascent,
                );
                let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
            }
        }
        value += minor;
    }

    let edge = gray_rgba(EDGE_GRAY, 1.0);
    if axis == CanvasGuideAxis::Horizontal {
        window.paint_quad(fill(
            Bounds::new(
                point(
                    bounds.origin.x,
                    bounds.origin.y + px(f32::from(bounds.size.height) - hairline),
                ),
                size(px(f32::from(bounds.size.width)), px(hairline)),
            ),
            edge,
        ));
    } else {
        window.paint_quad(fill(
            Bounds::new(
                point(
                    bounds.origin.x + px(f32::from(bounds.size.width) - hairline),
                    bounds.origin.y,
                ),
                size(px(hairline), px(f32::from(bounds.size.height))),
            ),
            edge,
        ));
    }
}
