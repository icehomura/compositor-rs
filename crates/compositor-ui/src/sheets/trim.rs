//! Trim: the modal that cuts transparent or single-colored borders off the document
//! (port of `UI/TrimSheet.swift`).
//!
//! The sheet only collects the options; the caller runs `EditorSession::trim` with them, as the
//! Swift `ProjectController.trim` did around the hosted view.

use compositor_pixels::trim::{TrimBasedOn, TrimOptions};

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{h_flex, v_flex, StyledExt as _, WindowExt as _};
use gpui_kit::*;

/// The sheet's content width, `.frame(width: 320)`.
const WIDTH: f32 = 320.0;
/// The component dialog's own padding, 16 points on each side.
const DIALOG_PADDING: f32 = 16.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;

/// What the sheet hands back when it closes: the options, or `None` on Cancel (`finish(TrimOptions?)`).
pub type TrimFinish = Box<dyn FnOnce(Option<TrimOptions>, &mut Window, &mut App)>;

/// The Trim sheet's state: what the scan treats as background and which edges it may cut.
pub struct TrimSheet {
    based_on: TrimBasedOn,
    trim_top: bool,
    trim_bottom: bool,
    trim_left: bool,
    trim_right: bool,
    finish: Option<TrimFinish>,
}

impl TrimSheet {
    /// A fresh sheet: transparent pixels, all four edges on, as the Swift `@State` defaults are.
    pub fn new(finish: impl FnOnce(Option<TrimOptions>, &mut Window, &mut App) + 'static) -> Self {
        Self {
            based_on: TrimBasedOn::TransparentPixels,
            trim_top: true,
            trim_bottom: true,
            trim_left: true,
            trim_right: true,
            finish: Some(Box::new(finish)),
        }
    }

    /// The options the current switches describe (`TrimOptions(basedOn:top:bottom:left:right:)`).
    pub fn options(&self) -> TrimOptions {
        TrimOptions::new(
            self.based_on,
            self.trim_top,
            self.trim_bottom,
            self.trim_left,
            self.trim_right,
            0,
        )
    }

    /// OK is enabled while at least one edge is set to be trimmed (`.disabled(!trimTop && …)`).
    pub fn valid(&self) -> bool {
        self.options().trims_any()
    }

    /// Hands the outcome to the caller, once.
    fn close(&mut self, options: Option<TrimOptions>, window: &mut Window, cx: &mut App) {
        if let Some(finish) = self.finish.take() {
            finish(options, window, cx);
        }
    }

    /// Opens the sheet as a modal dialog and returns the view it renders.
    pub fn open(
        finish: impl FnOnce(Option<TrimOptions>, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let view = cx.new(|_| Self::new(finish));
        window.open_dialog(cx, {
            let view = view.clone();
            move |dialog, _, _| {
                let ok = view.clone();
                let cancel = view.clone();
                dialog
                    .title("Trim")
                    .w(px(WIDTH + DIALOG_PADDING * 2.0))
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("OK")
                            .cancel_text("Cancel")
                            .ok_variant(ButtonVariant::Primary),
                    )
                    .on_ok(move |_, window, cx| {
                        let mut confirmed = false;
                        ok.update(cx, |sheet, cx| {
                            if sheet.valid() {
                                let options = sheet.options();
                                sheet.close(Some(options), window, cx);
                                confirmed = true;
                            }
                        });
                        confirmed
                    })
                    .on_cancel(move |_, window, cx| {
                        cancel.update(cx, |sheet, cx| sheet.close(None, window, cx));
                        true
                    })
                    .content({
                        let view = view.clone();
                        move |content, _, _| content.child(view.clone())
                    })
            }
        });
        view
    }
}

impl Render for TrimSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(18.0))
            .child(
                v_flex()
                    .gap(px(8.0))
                    .child(div().text_size(px(17.0)).font_weight(FontWeight::BOLD).child("Trim"))
                    .child(
                        v_flex()
                            .gap(px(8.0))
                            .child(div().font_semibold().child("Based On"))
                            .child(
                                RadioGroup::vertical("trim-based-on")
                                    .selected_index(Some(self.based_on_index()))
                                    .children(TrimBasedOn::ALL.map(|option| {
                                        Radio::new(option.raw_value())
                                            .label(option.raw_value())
                                    }))
                                    .on_change({
                                        let entity = entity.clone();
                                        move |index, _, cx| {
                                            let option = TrimBasedOn::ALL[*index];
                                            entity.update(cx, |sheet, cx| {
                                                sheet.based_on = option;
                                                cx.notify();
                                            });
                                        }
                                    }),
                            ),
                    ),
            )
            .child(Separator::horizontal())
            .child(
                v_flex()
                    .gap(px(8.0))
                    .child(div().font_semibold().child("Trim Away"))
                    .child(
                        v_flex()
                            .gap(px(8.0))
                            .child(
                                h_flex()
                                    .gap(px(24.0))
                                    .child(self.edge_checkbox(&entity, Edge::Top))
                                    .child(self.edge_checkbox(&entity, Edge::Bottom)),
                            )
                            .child(
                                h_flex()
                                    .gap(px(24.0))
                                    .child(self.edge_checkbox(&entity, Edge::Left))
                                    .child(self.edge_checkbox(&entity, Edge::Right)),
                            ),
                    ),
            )
            .child(Separator::horizontal())
    }
}

impl TrimSheet {
    fn based_on_index(&self) -> usize {
        TrimBasedOn::ALL
            .iter()
            .position(|option| *option == self.based_on)
            .unwrap_or(0)
    }

    /// One of the four "Trim Away" toggles.
    fn edge_checkbox(&self, entity: &Entity<Self>, edge: Edge) -> Checkbox {
        let (label, checked) = match edge {
            Edge::Top => ("Top", self.trim_top),
            Edge::Bottom => ("Bottom", self.trim_bottom),
            Edge::Left => ("Left", self.trim_left),
            Edge::Right => ("Right", self.trim_right),
        };
        Checkbox::new(label)
            .label(label)
            .checked(checked)
            .on_change({
                let entity = entity.clone();
                move |checked, _, cx| {
                    let checked = *checked;
                    entity.update(cx, |sheet, cx| {
                        match edge {
                            Edge::Top => sheet.trim_top = checked,
                            Edge::Bottom => sheet.trim_bottom = checked,
                            Edge::Left => sheet.trim_left = checked,
                            Edge::Right => sheet.trim_right = checked,
                        }
                        cx.notify();
                    });
                }
            })
    }
}

/// Which of the four edges a checkbox controls.
#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    // The builtin `#[test]`: the file's `use gpui_kit::*;` glob would otherwise shadow it with the
    // toolkit's `test` attribute macro (enabled by the test-support dev-dependency).
    use ::core::prelude::v1::test;
    use super::*;

    fn sheet() -> TrimSheet {
        TrimSheet::new(|_, _, _| {})
    }

    #[test]
    fn defaults_match_the_swift_state() {
        let sheet = sheet();
        assert_eq!(sheet.based_on, TrimBasedOn::TransparentPixels);
        assert!(sheet.trim_top && sheet.trim_bottom && sheet.trim_left && sheet.trim_right);
        assert!(sheet.valid());
    }

    #[test]
    fn ok_needs_one_edge() {
        let mut sheet = sheet();
        sheet.trim_top = false;
        sheet.trim_bottom = false;
        sheet.trim_left = false;
        assert!(sheet.valid());
        sheet.trim_right = false;
        assert!(!sheet.valid());
    }

    #[test]
    fn options_carry_every_switch() {
        let mut sheet = sheet();
        sheet.based_on = TrimBasedOn::BottomRightPixelColor;
        sheet.trim_left = false;
        let options = sheet.options();
        assert_eq!(options.based_on, TrimBasedOn::BottomRightPixelColor);
        assert!(options.top && options.bottom && options.right);
        assert!(!options.left);
        assert_eq!(options.tolerance, 0);
    }
}
