//! The Shape tool's option bar (port of `UI/ShapeControls.swift`): the kind picker and, for a line
//! or a rectangle, its width or corner radius, then the fill swatch.

use compositor_rs_core::layer_shape::ShapeKind;
use compositor_rs_session::EditorSession;

use crate::canvas::overlays::palette_rgba;
use crate::tool_controls::{segmented_picker, unit_suffix, FieldSpec, Fields};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `Width`'s field range and the width its slider covers (`scrubbable(…, range: 1...5000)`,
/// `Slider(…, in: 1...100)`).
pub const LINE_RANGE: (f64, f64) = (1.0, 5000.0);
pub const LINE_SLIDER_RANGE: (f64, f64) = (1.0, 100.0);
/// What a line's width falls back to when the field holds no number (`: 4`).
pub const LINE_FALLBACK: f64 = 4.0;
/// `Radius`'s field range and the radius its slider covers (0...5000 and 0...200).
pub const RADIUS_RANGE: (f64, f64) = (0.0, 5000.0);
pub const RADIUS_SLIDER_RANGE: (f64, f64) = (0.0, 200.0);
/// `.frame(width: 100)` — the slider's width.
pub const SLIDER_WIDTH: f32 = 100.0;
/// `.frame(width: 48)` — the number field's width.
pub const FIELD_WIDTH: f32 = 48.0;

/// The Shape bar: the kind, the line's width or the rectangle's corner radius, and the fill swatch.
pub struct ShapeControls {
    session: Entity<EditorSession>,
    fields: Fields,
}

impl ShapeControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
        }
    }
}

impl Render for ShapeControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (kind, line_width, corner_radius, foreground, enabled) = {
            let session = self.session.read(cx);
            (
                session.shape_kind,
                session.shape_line_width,
                session.shape_corner_radius,
                session.foreground_color(),
                session.document.is_some() && !session.shows_busy,
            )
        };

        let kind_picker = {
            let session = self.session.clone();
            div()
                .id("shape-kind")
                .tooltip(|window, cx| {
                    Tooltip::new("Shift-U (or Tab) steps through Rectangle, Ellipse and Line")
                        .build(window, cx)
                })
                .child(
                    segmented_picker(
                        "shape-kind-picker",
                        ShapeKind::ALL.map(|kind| (kind, kind.raw_value())),
                        kind,
                        move |kind, _, cx| {
                            session.update(cx, |session, _| {
                                // `session.cancelShape(); session.shapeKind = kind`.
                                session.cancel_shape();
                                session.shape_kind = kind;
                            });
                        },
                    )
                    .disabled(!enabled),
                )
        };

        let width_row = (kind == ShapeKind::Line).then(|| {
            let field = self.fields.get("shape-line-width", cx);
            let scrub = {
                let session = self.session.clone();
                NumericScrub::new(line_width, 1.0, LINE_RANGE).on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.shape_line_width = value);
                })
            };
            let slider = {
                let session = self.session.clone();
                let slider = CameraRawSlider::plain(
                    "shape-line-width-slider",
                    // `Slider(value: Binding(get: { min(100, session.shapeLineWidth) }, …))`.
                    line_width.min(LINE_SLIDER_RANGE.1),
                    LINE_SLIDER_RANGE,
                    "Line width",
                )
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    // `set: { session.shapeLineWidth = $0.rounded() }`.
                    session.update(cx, |session, _| session.shape_line_width = value.round());
                });
                div()
                    .w(px(SLIDER_WIDTH))
                    .when(!enabled, |this| this.opacity(0.5))
                    .child(slider)
            };
            let write = {
                let session = self.session.clone();
                move |value: f64, _: &mut Window, cx: &mut App| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.shape_line_width = value);
                }
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().child("Width").scrubbable("shape-line-width-label", scrub))
                .child(slider)
                .child(unit_suffix(
                    field.element(
                        "shape-line-width-field",
                        line_width,
                        FieldSpec::new(LINE_RANGE, 0)
                            .fallback(LINE_FALLBACK)
                            .disabled(!enabled),
                        FIELD_WIDTH,
                        write,
                        cx,
                    ),
                    div().child("px"),
                ))
        });

        let radius_row = (kind == ShapeKind::Rectangle).then(|| {
            let field = self.fields.get("shape-corner-radius", cx);
            let scrub = {
                let session = self.session.clone();
                NumericScrub::new(corner_radius, 1.0, RADIUS_RANGE).on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.shape_corner_radius = value);
                })
            };
            let slider = {
                let session = self.session.clone();
                let slider = CameraRawSlider::plain(
                    "shape-corner-radius-slider",
                    corner_radius.min(RADIUS_SLIDER_RANGE.1),
                    RADIUS_SLIDER_RANGE,
                    "Corner radius",
                )
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.shape_corner_radius = value.round());
                });
                div()
                    .w(px(SLIDER_WIDTH))
                    .when(!enabled, |this| this.opacity(0.5))
                    .child(slider)
            };
            let write = {
                let session = self.session.clone();
                move |value: f64, _: &mut Window, cx: &mut App| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.shape_corner_radius = value);
                }
            };
            div()
                .id("shape-corner-radius-row")
                .flex()
                .items_center()
                .gap(px(6.0))
                .tooltip(|window, cx| {
                    Tooltip::new("Round the rectangle's corners by this many pixels; 0 keeps them square")
                        .build(window, cx)
                })
                .child(div().child("Radius").scrubbable("shape-radius-label", scrub))
                .child(slider)
                .child(unit_suffix(
                    field.element(
                        "shape-corner-radius-field",
                        corner_radius,
                        FieldSpec::new(RADIUS_RANGE, 0)
                            .fallback(0.0)
                            .disabled(!enabled),
                        FIELD_WIDTH,
                        write,
                        cx,
                    ),
                    div().child("px"),
                ))
        });

        let fill = {
            let session = self.session.clone();
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child("Fill")
                .child(
                    Button::new("shape-fill")
                        .disabled(!enabled)
                        .tooltip("Shapes fill with the foreground color; click to change it")
                        .child(
                            div()
                                .w(px(36.0))
                                .h(px(18.0))
                                .rounded(px(3.0))
                                .border_1()
                                .border_color(hsla(0.0, 0.0, 0.0, 0.5))
                                .bg(Hsla::from(palette_rgba(foreground))),
                        )
                        .on_click(move |_, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| session.open_color_picker(false));
                        }),
                )
        };

        tool_header_bar(12.0)
            .child(tool_header_title("Shape"))
            .child(kind_picker)
            .children(width_row)
            .children(radius_row)
            .child(fill)
            .child(tool_header_spacer())
    }
}
