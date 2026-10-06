//! The status bar at the foot of the editor: the zoom, the canvas size, what the document is, and
//! the line of tool hints on the right — or what the app is busy with.
//!
//! Ported from `ContentView.statusBar` in `Compositor/ContentView.swift`.

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::layer_shape::ShapeKind;
use compositor_rs_core::selection::{LassoKind, WandMode};
use compositor_rs_pixels::warp::{BlurToolMode, BrushToolMode};
use compositor_rs_session::EditorSession;

use gpui_kit::*;

/// The status bar's text color: SwiftUI's `.secondary` on the editor's dark background.
pub const SECONDARY: Hsla = hsla(0.0, 0.0, 1.0, 0.55);

/// The status bar's height, `frame(height: 30)`.
pub const HEIGHT: f32 = 30.0;

/// The zoom readout's column, `frame(width: 62, alignment: .leading)`.
pub const ZOOM_WIDTH: f32 = 62.0;

/// `Text(session.viewport.zoom, format: .percent.precision(.fractionLength(0...1)))`: a whole
/// percent shows no fraction, anything else one digit.
pub fn zoom_percent_text(zoom: f64) -> String {
    let percent = zoom * 100.0;
    if (percent - percent.round()).abs() < 0.000_001 {
        format!("{}%", percent.round() as i64)
    } else {
        format!("{percent:.1}%")
    }
}

/// The tool hint on the right of the bar, chosen by tool and mode exactly as the Swift chain does.
pub fn status_hint(session: &EditorSession) -> String {
    match session.tool {
        NavigationTool::Marquee => {
            if session.marquee_kind == LassoKind::Ellipse {
                "Drag an ellipse · Shift add · Option subtract · Shift again mid-drag circle · Drag inside to move · Delete clears · ⌘D deselect".to_string()
            } else {
                "Drag a rectangle · Shift add · Option subtract · Shift again mid-drag square · Drag inside to move · ⌘-drag moves pixels · Delete clears · ⌘D deselect".to_string()
            }
        }
        NavigationTool::Wand => {
            if session.wand_mode == WandMode::Object {
                "Click an object to select its outline · Tab for Wand · Shift add · Option subtract · Drag inside to move · ⌘-drag moves pixels · Delete clears · ⌘D deselect".to_string()
            } else {
                "Click to select similar colors · Tab for Object · Shift add · Option subtract · Drag inside to move · ⌘-drag moves pixels · Delete clears · ⌘D deselect".to_string()
            }
        }
        NavigationTool::Lasso => {
            if session.lasso_kind == LassoKind::Freehand {
                "Drag to select · Drag inside to move · Shift add · Option subtract · Delete clears · ⌥⌫/⌘⌫ fill · ⌘D deselect".to_string()
            } else {
                "Click corners · Click start, double-click or Enter to close · Delete removes corner · Escape cancel".to_string()
            }
        }
        NavigationTool::Brush => {
            let lead = if session.brush_mode == BrushToolMode::Erase {
                "Drag to erase"
            } else {
                "Drag to paint"
            };
            format!("{lead} · [ ] size · Shift-[ ] hardness · 1–0 opacity · Escape cancel · Space to pan")
        }
        NavigationTool::Blur => {
            let lead = match session.blur_mode {
                BlurToolMode::Blur => "Drag to soften",
                BlurToolMode::Smudge => "Drag to smudge",
                BlurToolMode::Liquify => "Drag to push pixels",
            };
            format!("{lead} · [ ] size · Shift-[ ] hardness · 1–0 strength · Space to pan")
        }
        NavigationTool::CloneStamp => {
            "Option-click to set the source · Drag to clone · [ ] size · Shift-[ ] hardness · 1–0 opacity · Space to pan".to_string()
        }
        NavigationTool::SpotHealing => {
            "Drag over blemishes to heal · [ ] size · Shift-[ ] hardness · Escape cancel · Space to pan".to_string()
        }
        NavigationTool::Type => {
            "Drag a text box · Click text to edit · Drag box handles to resize · ⌘Return finish · Escape cancel".to_string()
        }
        NavigationTool::Shape => {
            let constraint = match session.shape_kind {
                ShapeKind::Line => "45°",
                ShapeKind::Rectangle => "square",
                ShapeKind::Ellipse => "circle",
            };
            format!("Drag to draw a shape on a new layer · Shift {constraint} · Option from center · Shift-U or Tab for the next shape · Escape cancel · Space to pan")
        }
        NavigationTool::Gradient => {
            "Drag to draw · Drag ends to adjust · Shift 45° · 1–0 opacity · Enter apply · Escape cancel".to_string()
        }
        NavigationTool::Crop => "Drag to crop · Enter apply · Escape cancel · Space to pan".to_string(),
        NavigationTool::Move => {
            "Drag to move · Handles to resize · Circle to rotate · 1–0 layer opacity · Space to pan".to_string()
        }
        NavigationTool::Hand => "Drag to pan · Pinch to zoom".to_string(),
        NavigationTool::Idle => {
            "No tool selected · Press a tool's key to pick one · Space to pan".to_string()
        }
        NavigationTool::Zoom => {
            "Click to zoom in · Option-click to zoom out · Drag right or left to zoom smoothly · Space to pan".to_string()
        }
        NavigationTool::Eyedropper => "Click to sample a color · Option-click samples the background".to_string(),
    }
}

/// The bar itself, on the component library's `StatusBar`.
pub struct StatusBar {
    session: Entity<EditorSession>,
}

impl StatusBar {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let left: AnyElement = match session.document.as_ref() {
            Some(document) => div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(16.0))
                .child(
                    div()
                        .w(px(ZOOM_WIDTH))
                        .child(zoom_percent_text(session.viewport.zoom())),
                )
                .child(div().child(format!("{} × {} px", document.width, document.height)))
                .child(div().child("sRGB · Transparent"))
                .into_any_element(),
            None => div().child("Ready when you are").into_any_element(),
        };
        let right: AnyElement = if session.shows_busy {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(progress_indicator())
                .child(div().child("Working…"))
                .into_any_element()
        } else if session.is_importing {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(progress_indicator())
                .child(div().child("Importing images…"))
                .into_any_element()
        } else {
            div().child(status_hint(session)).into_any_element()
        };

        gpui_kit::component::status_bar::StatusBar::new()
            .text_size(px(11.0))
            .text_color(SECONDARY)
            .left(left)
            .right(right)
    }
}

/// `ProgressView().controlSize(.mini)`: the component library's small spinner.
fn progress_indicator() -> impl IntoElement {
    use gpui_kit::component::Sizable as _;
    gpui_kit::component::spinner::Spinner::new().small()
}

#[cfg(test)]
mod tests {
    // `use super::*` would also bring in the toolkit's `test` attribute macro (gpui-kit's test-support
    // exports one), which shadows the built-in `#[test]`; these tests are the built-in's.
    use ::core::prelude::v1::test;
    use super::*;

    #[test]
    fn zoom_text_keeps_the_swift_precision() {
        assert_eq!(zoom_percent_text(1.0), "100%");
        assert_eq!(zoom_percent_text(0.125), "12.5%");
        assert_eq!(zoom_percent_text(1.0 / 6.0), "16.7%");
        assert_eq!(zoom_percent_text(8.0), "800%");
        assert_eq!(zoom_percent_text(0.5), "50%");
    }
}
