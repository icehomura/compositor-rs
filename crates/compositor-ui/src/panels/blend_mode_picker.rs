//! The blend mode menu beside the Opacity field: Photoshop's modes, grouped as Photoshop groups them
//! — darkening, lightening, contrast, comparative, component — with a line between each group, the
//! current mode checked, and the canvas previewing the mode under the pointer.
//!
//! Ported from `UI/BlendModePicker.swift`. The AppKit `NSPopUpButton` becomes a [`Popover`] with the
//! same items and separators; its `menu(_:willHighlight:)` preview is the row's `on_hover`.

use compositor_core::blend::LayerBlendMode;
use compositor_core::Id;
use compositor_session::EditorSession;

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::v_flex;
use gpui_kit::*;

/// The blend mode menu.
pub struct BlendModePicker {
    session: Entity<EditorSession>,
    open: bool,
    /// The layer whose mode the open menu is previewing (`Coordinator.layerID`).
    layer_id: Option<Id>,
}

impl BlendModePicker {
    pub fn new(session: Entity<EditorSession>, _cx: &mut Context<Self>) -> Self {
        Self {
            session,
            open: false,
            layer_id: None,
        }
    }

    /// The mode the layer itself carries: what the button's title shows, even while a preview runs.
    fn current(&self, cx: &App) -> LayerBlendMode {
        self.session
            .read(cx)
            .active_layer()
            .map(|layer| layer.blend_mode)
            .unwrap_or_default()
    }

    /// `Coordinator.choose(_:)`: the mode under the pointer is committed and the preview cleared.
    fn choose(&mut self, mode: LayerBlendMode, cx: &mut Context<Self>) {
        let same_layer = self.session.read(cx).active_layer_id == self.layer_id;
        if !same_layer {
            return;
        }
        self.session.update(cx, |session, _| {
            session.set_layer_blend_mode(mode);
            session.preview_blend_mode(None, None);
            session.refresh_canvas_preview();
        });
        self.layer_id = None;
        self.open = false;
        cx.notify();
    }

    /// `menuWillOpen` / `menuDidClose`: the preview belongs to the layer the menu was opened on, and
    /// leaving the menu — by choosing, by cancelling or by clicking away — restores the layer's mode.
    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.layer_id = self.session.read(cx).active_layer_id;
        } else {
            self.layer_id = None;
            self.session.update(cx, |session, _| session.preview_blend_mode(None, None));
        }
        self.open = open;
        cx.notify();
    }

    /// `menu(_:willHighlight:)`: the highlighted mode is shown on the canvas while the menu is open.
    fn highlight(&self, mode: LayerBlendMode, cx: &mut Context<Self>) {
        if self.layer_id.is_none() {
            return;
        }
        self.session.update(cx, |session, _| {
            session.preview_blend_mode(Some(mode), self.layer_id);
        });
    }

    /// One row of the menu, with the line that separates it from the group above.
    fn rows(&self, current: LayerBlendMode, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, group) in LayerBlendMode::groups().iter().enumerate() {
            if index > 0 {
                rows.push(
                    div()
                        .h(px(1.0))
                        .my(px(4.0))
                        .w_full()
                        .bg(hsla(0.0, 0.0, 1.0, 0.12))
                        .into_any_element(),
                );
            }
            for mode in group.iter() {
                rows.push(self.mode_row(*mode, current, cx).into_any_element());
            }
        }
        rows
    }

    /// One row of the menu.
    fn mode_row(&self, mode: LayerBlendMode, current: LayerBlendMode, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id(ElementId::Name(format!("blend-mode-{}", mode.raw_value()).into()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(24.0))
            .px(px(6.0))
            .rounded(px(4.0))
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(hsla(0.0, 0.0, 1.0, 0.08)))
            .child(
                // The current mode shows a check, as NSPopUpButton's menu does.
                div()
                    .w(px(12.0))
                    .child(if mode == current {
                        Icon::new(IconName::Check)
                            .size(px(11.0))
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    }),
            )
            .child(div().flex_1().text_size(px(12.0)).child(mode.raw_value()))
            .on_hover({
                let entity = entity.clone();
                move |hovered, _, cx| {
                    if *hovered {
                        entity.update(cx, |picker, cx| picker.highlight(mode, cx));
                    }
                }
            })
            .on_click(move |_, _, cx| {
                entity.update(cx, |picker, cx| picker.choose(mode, cx));
            })
    }
}

impl Render for BlendModePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current(cx);
        let enabled = self.session.read(cx).can_edit_appearance();
        let entity = cx.entity();

        Popover::new("blend-mode-picker")
            .open(self.open)
            .trigger(
                Button::new("blend-mode-picker-trigger")
                    .label(current.raw_value())
                    .icon(Icon::new(IconName::ChevronDown).size(px(9.0)))
                    .tooltip("Blend mode")
                    .accessibility_label("Blend mode")
                    // A capsule like the SwiftUI buttons and menus (`roundedControls`).
                    .rounded_full()
                    .disabled(!enabled),
            )
            .on_open_change({
                let entity = entity.clone();
                move |open, _, cx| {
                    let open = *open;
                    entity.update(cx, |picker, cx| picker.set_open(open, cx));
                }
            })
            .content({
                let entity = entity.clone();
                move |_, _, cx| {
                    // The content is built whenever the popover opens, so the rows are made here.
                    let rows = entity.update(cx, |picker, cx| picker.rows(current, cx));
                    v_flex()
                        .w(px(180.0))
                        .gap(px(1.0))
                        .children(rows)
                        .into_any_element()
                }
            })
            .into_any_element()
    }
}
