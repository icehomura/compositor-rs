//! The layer mask button in the Layers panel's footer, and the badge the canvas shows while a mask
//! is being viewed by itself.
//!
//! Ported from `UI/LayerMaskMenu.swift`.

use compositor_rs_session::EditorSession;

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::h_flex;
use gpui_kit::*;

/// Adds a mask in one click, as Photoshop's button does: all white, or with a selection, revealing just the
/// selection. Option-click adds the opposite: all black, or hiding the selection.
/// Enable/Disable and Delete live in the layer's context menu.
#[derive(IntoElement)]
pub struct LayerMaskMenu {
    session: Entity<EditorSession>,
}

impl LayerMaskMenu {
    pub fn new(session: Entity<EditorSession>) -> Self {
        Self { session }
    }
}

impl RenderOnce for LayerMaskMenu {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (enabled, help) = {
            let session = self.session.read(cx);
            let enabled = session.can_edit_mask() && session.active_layer().is_none_or(|layer| layer.mask.is_none());
            let help = if session.selection().is_none() {
                "Add layer mask (Option-click for a black mask)"
            } else {
                "Add layer mask revealing the selection (Option-click to hide it)"
            };
            (enabled, help)
        };
        let session = self.session.clone();
        Button::new("layer-mask-menu")
            .icon(Icon::new(IconName::SquareSplitVertical))
            .tooltip(help)
            .accessibility_label("Add layer mask")
            .ghost()
            .disabled(!enabled)
            // `footerHitArea()`: the padding is the clickable area.
            .px(px(8.0))
            .py(px(12.0))
            .on_click(move |event: &ClickEvent, _, cx| {
                // Option-click adds the opposite mask.
                let revealing = !event.modifiers().alt;
                session.update(cx, |session, _| session.add_mask(revealing));
            })
    }
}

/// Over the canvas while it shows a mask by itself: whose mask it is, and a way back to the composite besides
/// Option-clicking the thumbnail again.
#[derive(IntoElement)]
pub struct MaskAloneBadge {
    session: Entity<EditorSession>,
    /// The layer the shown mask belongs to.
    name: SharedString,
}

impl MaskAloneBadge {
    pub fn new(session: Entity<EditorSession>, name: impl Into<SharedString>) -> Self {
        Self {
            session,
            name: name.into(),
        }
    }
}

impl RenderOnce for MaskAloneBadge {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let session = self.session.clone();
        h_flex()
            .items_center()
            .gap(px(7.0))
            .h(px(26.0))
            .pl(px(11.0))
            .pr(px(8.0))
            .rounded(px(13.0))
            .bg(hsla(0.0, 0.0, 0.0, 0.75))
            .border_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.14))
            .child(
                Icon::new(IconName::SquareSplitVertical)
                    .size(px(11.0))
                    .text_color(hsla(0.0, 0.0, 1.0, 1.0)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hsla(0.0, 0.0, 1.0, 1.0))
                    .child("Layer Mask"),
            )
            .child(
                div()
                    // `lineBreakMode = .byTruncatingTail` with a 220-point cap.
                    .max_w(px(220.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.0))
                    .text_color(hsla(0.0, 0.0, 1.0, 0.6))
                    .child(self.name),
            )
            .child(
                Button::new("mask-alone-close")
                    .icon(Icon::new(IconName::X).size(px(9.0)))
                    .tooltip("Show the image again (or Option-click the mask thumbnail)")
                    .accessibility_label("Stop viewing the mask")
                    .ghost()
                    .w(px(18.0))
                    .h(px(18.0))
                    .on_click(move |_, _, cx| {
                        session.update(cx, |session, _| session.views_mask_alone = false);
                    }),
            )
    }
}
