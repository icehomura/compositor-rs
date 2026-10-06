//! The Hand and Zoom tools' header (port of `UI/NavigationToolHeader.swift`): the tool's name and,
//! for Zoom, the percentage field the canvas is zoomed to.

use compositor_rs_core::document::NavigationTool;
use compositor_rs_session::EditorSession;

use crate::tool_controls::{EditEnd, FieldSpec, Fields};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};

use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The zoom the field's binding holds (`0.1…3200` percent).
pub const ZOOM_RANGE: (f64, f64) = (0.1, 3200.0);

/// The header the Hand and Zoom tools share: `Pan` or `Zoom`, and the Zoom percentage field.
pub struct NavigationToolHeader {
    session: Entity<EditorSession>,
    fields: Fields,
}

impl NavigationToolHeader {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
        }
    }
}

impl Render for NavigationToolHeader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, zoom_percent, enabled) = {
            let session = self.session.read(cx);
            (
                if session.tool == NavigationTool::Hand {
                    "Pan"
                } else {
                    "Zoom"
                },
                session.viewport.zoom() * 100.0,
                session.document.is_some() && !session.shows_busy,
            )
        };

        let field = if title == "Zoom" {
            let field = self.fields.get("zoom", cx);
            let write = {
                let session = self.session.clone();
                move |percent: f64, _: &mut Window, cx: &mut App| {
                    session.update(cx, |session, _| {
                        // `applyZoom`: a positive value, and nothing while the project is busy.
                        if !session.is_project_busy {
                            session.zoom(percent / 100.0, None);
                        }
                    });
                }
            };
            let release = {
                let session = self.session.clone();
                move |end: EditEnd, _: &mut Window, cx: &mut App| {
                    // `releaseFocus`: the canvas takes the focus back on Return or Escape, so a
                    // tool's key works straight away.
                    if matches!(end, EditEnd::Return | EditEnd::Escape) {
                        session.update(cx, |session, _| session.canvas_focus_request += 1);
                    }
                }
            };
            Some(
                field
                    .element_with_release(
                        "zoom-percent",
                        zoom_percent,
                        FieldSpec::new(ZOOM_RANGE, 2).step(1.0).fallback(zoom_percent),
                        72.0,
                        write,
                        release,
                        cx,
                    )
                    .tooltip({
                        let help = "Zoom percentage (0.1–3200%). Press Return to apply.";
                        move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(help).build(window, cx)
                        }
                    })
                    .aria_label("Zoom percentage")
                    .when(!enabled, |this| this.opacity(0.5)),
            )
        } else {
            None
        };

        tool_header_bar(12.0)
            .child(tool_header_title(title))
            .children(field)
            .child(tool_header_spacer())
    }
}
