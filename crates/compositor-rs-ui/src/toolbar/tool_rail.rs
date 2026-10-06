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

    /// The SF Symbol the rail draws for a tool, with the Marquee's icon following its shape.
    pub fn symbol(tool: NavigationTool, marquee_ellipse: bool) -> IconName {
        if tool == NavigationTool::Marquee && marquee_ellipse {
            return IconName::CircleDashed;
        }
        match tool {
            NavigationTool::Type => IconName::Type,
            NavigationTool::Eyedropper => IconName::Pipette,
            NavigationTool::Marquee => IconName::SquareDashed,
            NavigationTool::Lasso => IconName::Lasso,
            NavigationTool::Wand => IconName::WandSparkles,
            NavigationTool::Brush => IconName::Paintbrush,
            NavigationTool::SpotHealing => IconName::Bandage,
            NavigationTool::CloneStamp => IconName::Stamp,
            NavigationTool::Blur => IconName::Droplet,
            NavigationTool::Gradient => IconName::Contrast,
            NavigationTool::Shape => IconName::Shapes,
            NavigationTool::Crop => IconName::Crop,
            NavigationTool::Move => IconName::MoveDiagonal,
            NavigationTool::Hand => IconName::Hand,
            NavigationTool::Zoom | NavigationTool::Idle => IconName::Search,
        }
    }
}

impl Render for ToolRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let current = session.tool;
        let lasso_polygonal = session.lasso_kind == LassoKind::Polygonal;
        let wand_object = session.wand_mode == WandMode::Object;
        let marquee_ellipse = session.marquee_kind == LassoKind::Ellipse;

        let buttons = Self::tools()
            .map(|tool| {
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
                    Some(icon) => button.child(icon),
                    None => button.icon(Icon::new(Self::symbol(tool, marquee_ellipse))),
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
