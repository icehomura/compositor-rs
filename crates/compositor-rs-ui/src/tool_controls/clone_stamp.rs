//! The Clone Stamp's tool-rail icon and its options in the Brush bar (port of
//! `UI/BrushControls.swift`).
//!
//! The bar itself is [`crate::tool_controls::brush::BrushControls`]; the two pieces here are the
//! icon SF Symbols has none of — so the port draws it, as the Swift did — and the `Aligned` /
//! `Sample` pair the bar shows only for this tool.

use compositor_rs_session::EditorSession;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::tool_controls::segmented_picker;

/// The box the rail's icons are drawn in (the Swift `Canvas` fills the frame it is given).
/// The side of the rail's own icons (`.frame(width: 18, height: 18)` in the Swift).
pub const ICON_SIZE: f32 = 18.0;

/// A rubber stamp for the tool rail (SF Symbols has none): round handle, neck, body, and pad.
pub struct CloneStampToolIcon;

impl CloneStampToolIcon {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CloneStampToolIcon {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for CloneStampToolIcon {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for CloneStampToolIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _, window, _| {
                let width = f32::from(bounds.size.width);
                let height = f32::from(bounds.size.height);
                // `.foreground`: the rail button's own text color, as the SF Symbols beside it use.
                let color = window.text_style().color;
                let shape = |x: f32, y: f32, shape_width: f32, shape_height: f32, radius: f32| {
                    let origin = point(
                        bounds.left() + px(x * width),
                        bounds.top() + px(y * height),
                    );
                    let size = size(px(shape_width * width), px(shape_height * height));
                    let mut quad = fill(Bounds { origin, size }, color);
                    quad.corner_radii = Corners {
                        top_left: px(radius * width),
                        top_right: px(radius * width),
                        bottom_right: px(radius * width),
                        bottom_left: px(radius * width),
                    };
                    quad
                };
                // The handle: an ellipse across the top.
                window.paint_quad(shape(0.33, 0.02, 0.34, 0.30, 0.15));
                // The neck down to the body.
                window.paint_quad(shape(0.43, 0.28, 0.14, 0.28, 0.0));
                // The body, with its rounded shoulders.
                window.paint_quad(shape(0.12, 0.54, 0.76, 0.22, 0.08));
                // The pad.
                window.paint_quad(shape(0.06, 0.82, 0.88, 0.12, 0.0));
            },
        )
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
    }
}

/// The Clone Stamp's own options (`if session.tool == .cloneStamp`): the Aligned toggle and the
/// Sample choice, with the Swift's help texts.
pub fn clone_stamp_options(
    session: Entity<EditorSession>,
    aligned: bool,
    sample_all_layers: bool,
) -> impl IntoElement {
    let toggle = {
        let session = session.clone();
        Checkbox::new("clone-aligned")
            .label("Aligned")
            .checked(aligned)
            .tooltip("Keep the source moving with the brush between strokes; off starts every stroke at the source point")
            .on_click(move |checked, _, cx| {
                let checked = *checked;
                let session = session.clone();
                session.update(cx, |session, _| session.clone_settings.aligned = checked);
            })
    };
    let sample = {
        let session = session.clone();
        div().id("clone-sample").tooltip(|window, cx| {
            Tooltip::new("Copy from the active layer only, or from every visible layer as shown").build(window, cx)
        }).child(
            segmented_picker(
                "clone-sample-picker",
                [(false, "This Layer"), (true, "All Layers")],
                sample_all_layers,
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.clone_settings.sample_all_layers = value);
                },
            ),
        )
    };
    div()
        .flex()
        .items_center()
        .gap(px(crate::tool_controls::CONTROL_SPACING))
        .child(toggle)
        .child(sample)
}

/// The note the bar shows while no clone source has been set
/// (`Text("Option-click to set the source")`).
pub fn clone_source_hint() -> impl IntoElement {
    div().child("Option-click to set the source")
}
