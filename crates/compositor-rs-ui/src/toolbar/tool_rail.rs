//! The tool rail down the left edge: every tool but the idle one, in the Swift order, with the
//! marquee's, lasso's, wand's and clone stamp's icons following their modes, and the color palette
//! controls at its foot.
//!
//! Ported from `ContentView.toolRail` in `Compositor/ContentView.swift`.

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::selection::{LassoKind, WandMode};
use compositor_rs_session::EditorSession;

use crate::tool_controls::color_palette::ColorPaletteControls;
use crate::tool_controls::gradient::GradientToolIcon;
use crate::tool_controls::lasso::{ObjectSelectionToolIcon, PolygonalLassoToolIcon};
use crate::tool_controls::clone_stamp::CloneStampToolIcon;
use crate::widgets::indicatorless_scroll::{IndicatorlessScrollView, RAIL_WIDTH};

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::*;

/// The rail's width (`frame(width: 56)`).
pub const WIDTH: f32 = 56.0;
/// A tool button's box, `frame(width: 36, height: 36)`.
pub const BUTTON: f32 = 36.0;
/// The corner radius of a tool button's background, `RoundedRectangle(cornerRadius: 7)`.
pub const BUTTON_RADIUS: f32 = 7.0;
/// The space between the rail's buttons (`VStack(spacing: 10)`).
pub const SPACING: f32 = 10.0;

/// The tool rail.
pub struct ToolRail {
    session: Entity<EditorSession>,
    palette: Entity<ColorPaletteControls>,
}

impl ToolRail {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let palette = cx.new(|cx| ColorPaletteControls::new(session.clone(), cx));
        Self { session, palette }
    }

    /// Every tool the rail shows: `NavigationTool.allCases.filter { $0 != .idle }`.
    pub fn tools() -> impl Iterator<Item = NavigationTool> {
        NavigationTool::ALL
            .into_iter()
            .filter(|tool| *tool != NavigationTool::Idle)
    }

    /// The Lucide mark that stands in for an SF Symbol name.
    ///
    /// The names are the ones `EditorSession::symbol(for:)` and `ContentView.toolRail` hand out, so
    /// the mode-following stays in the session where the Swift keeps it.
    pub fn sf_symbol(symbol: &str) -> IconName {
        match symbol {
            "textformat" => IconName::Type,
            "eyedropper" => IconName::Pipette,
            "rectangle.dashed" => IconName::SquareDashed,
            "circle.dashed" => IconName::CircleDashed,
            "lasso" => IconName::Lasso,
            "wand.and.stars" => IconName::WandSparkles,
            "paintbrush.pointed" => IconName::Paintbrush,
            "bandage" => IconName::Bandage,
            "seal" => IconName::Stamp,
            "drop" => IconName::Droplet,
            "square.bottomhalf.filled" => IconName::Contrast,
            "square.on.circle" => IconName::Shapes,
            "crop" => IconName::Crop,
            "arrow.up.left.and.arrow.down.right" => IconName::MoveDiagonal,
            "hand.draw" => IconName::Hand,
            "eraser" => IconName::Eraser,
            // `Symbol`'s final else, which covers Zoom and the idle tool.
            _ => IconName::Search,
        }
    }

    /// The rail's icon for a tool: `session.symbol(for:)` — the Brush shows the eraser in Erase mode
    /// — except the Marquee, whose icon follows its shape (a dashed circle in Ellipse mode).
    pub fn symbol(
        session: &EditorSession,
        tool: NavigationTool,
        marquee_ellipse: bool,
    ) -> IconName {
        if tool == NavigationTool::Marquee && marquee_ellipse {
            return IconName::CircleDashed;
        }
        Self::sf_symbol(&session.symbol(tool))
    }
}

impl Render for ToolRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let current = session.tool;
        let lasso_polygonal = session.lasso_kind == LassoKind::Polygonal;
        let wand_object = session.wand_mode == WandMode::Object;
        let marquee_ellipse = session.marquee_kind == LassoKind::Ellipse;

        // Read the icons up front: `EditorSession::symbol` borrows the session, and the buttons below
        // need `cx` mutably for their listeners.
        let icons = Self::tools()
            .map(|tool| Self::symbol(session, tool, marquee_ellipse))
            .collect::<Vec<_>>();

        let buttons = Self::tools()
            .zip(icons)
            .map(|(tool, icon)| {
                let label = tool.label();
                // The icons that aren't an SF Symbol: the Gradient, Clone Stamp, polygonal Lasso
                // and Object Selection marks the Swift draws itself.
                let drawn_icon: Option<AnyElement> = if tool == NavigationTool::Gradient {
                    Some(GradientToolIcon::new().into_any_element())
                } else if tool == NavigationTool::CloneStamp {
                    Some(CloneStampToolIcon::new().into_any_element())
                } else if tool == NavigationTool::Lasso && lasso_polygonal {
                    Some(PolygonalLassoToolIcon::new().into_any_element())
                } else if tool == NavigationTool::Wand && wand_object {
                    Some(ObjectSelectionToolIcon::new().into_any_element())
                } else {
                    None
                };

                let button = Button::new(format!("tool-rail-{}", tool.raw_value()))
                    .tooltip(label)
                    .accessibility_label(label)
                    .selected(current == tool)
                    .w(px(BUTTON))
                    .h(px(BUTTON))
                    .rounded(px(BUTTON_RADIUS))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session.update(cx, |session, _| session.select_tool(tool));
                    }));
                // `Button::icon` takes an icon, not an element: the SF Symbols go through its
                // icon slot, the marks Swift draws itself ride as the button's content.
                match drawn_icon {
                    Some(drawn) => button.child(drawn),
                    // `.font(.system(size: 17))`: the SF Symbols beside the drawn marks are 17 pt.
                    None => button.icon(Icon::new(icon).size(px(17.0))),
                }
            })
            .collect::<Vec<_>>();

        IndicatorlessScrollView::rail("tool-rail").child(
            div()
                .flex()
                .flex_col()
                .gap(px(SPACING))
                .pt(px(16.0))
                .pb(px(12.0))
                .w(px(WIDTH))
                .children(buttons)
                .child(div().pt(px(8.0)).child(self.palette.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` would also bring in the toolkit's `test` attribute macro (gpui-kit's test-support
    // exports one), which shadows the built-in `#[test]`; these tests are the built-in's.
    use ::core::prelude::v1::test;
    use super::*;
    use compositor_rs_pixels::warp::BrushToolMode;

    #[test]
    fn the_brush_shows_the_eraser_in_erase_mode() {
        let mut session = EditorSession::default();
        assert_eq!(
            ToolRail::symbol(&session, NavigationTool::Brush, false),
            IconName::Paintbrush
        );
        session.brush_mode = BrushToolMode::Erase;
        assert_eq!(
            ToolRail::symbol(&session, NavigationTool::Brush, false),
            IconName::Eraser
        );
    }

    #[test]
    fn the_marquee_follows_its_shape() {
        let session = EditorSession::default();
        assert_eq!(
            ToolRail::symbol(&session, NavigationTool::Marquee, false),
            IconName::SquareDashed
        );
        assert_eq!(
            ToolRail::symbol(&session, NavigationTool::Marquee, true),
            IconName::CircleDashed
        );
    }

    /// Every symbol `EditorSession::symbol(for:)` can return has a mark of its own, so no tool falls
    /// through to the final else (the magnifying glass, which only the Zoom tool earns).
    #[test]
    fn every_tool_symbol_has_a_mark() {
        let session = EditorSession::default();
        for tool in NavigationTool::ALL {
            let name = session.symbol(tool);
            if tool == NavigationTool::Zoom || tool == NavigationTool::Idle {
                assert_eq!(name, "magnifyingglass");
                continue;
            }
            assert_ne!(
                ToolRail::sf_symbol(&name),
                IconName::Search,
                "{name} fell through to the final else"
            );
        }
    }
}
