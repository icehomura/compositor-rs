//! The color palette at the foot of the tool rail (port of `UI/ColorPaletteControls.swift`): the
//! foreground and background swatches, the swap and reset buttons beside them, and the mask-color
//! popover that stands in for the color picker while a mask is selected.
//!
//! The Swift opened the app's `ColorPickerPanelController` for a swatch press; here the floating
//! Color Picker panel watches `session.color_picker` itself, so this view only opens it.

use compositor_session::EditorSession;

use crate::canvas::overlays::palette_rgba;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The swatch's side (`swatchSize`).
pub const SWATCH_SIZE: f32 = 24.0;
/// How far the background swatch is offset from the foreground one (`swatchOffset`).
pub const SWATCH_OFFSET: f32 = 12.0;

/// The tool rail's palette controls.
pub struct ColorPaletteControls {
    session: Entity<EditorSession>,
    /// Which swatch's mask color is being chosen (`choosingMaskBackground`): `None`, foreground
    /// (`Some(false)`) or background (`Some(true)`).
    choosing_mask_background: Option<bool>,
    /// `session.isMaskSelected` as of the last frame, so the Swift `onChange` can be noticed.
    was_mask_selected: bool,
}

impl ColorPaletteControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        let was_mask_selected = session.read(cx).is_mask_selected;
        cx.observe(&session, |this, session, cx| {
            // `onChange(of: session.isMaskSelected)`: the popover closes, and an open color picker
            // closes without committing.
            let masked = session.read(cx).is_mask_selected;
            if masked != this.was_mask_selected {
                this.was_mask_selected = masked;
                this.choosing_mask_background = None;
                if masked {
                    session.update(cx, |session, _| session.close_color_picker(false));
                }
                cx.notify();
            }
        })
        .detach();
        Self {
            session,
            choosing_mask_background: None,
            was_mask_selected,
        }
    }

    /// One swatch (`swatch(background:)`): a 6-point rounded square of the palette color, with a
    /// white inner stroke and a black outer one.
    fn swatch(&self, background: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let color = self.session.read(cx).palette_color(background);
        let label = if background {
            "Background color"
        } else {
            "Foreground color"
        };
        let session = self.session.clone();
        let entity = cx.entity();
        div()
            .absolute()
            .when(background, |this| {
                this.left(px(SWATCH_OFFSET)).top(px(SWATCH_OFFSET))
            })
            .when(!background, |this| this.left_0().top_0())
            .child(
                Button::new(ElementId::NamedInteger(
                    "palette-swatch".into(),
                    u64::from(background),
                ))
                    .tooltip(label)
                    .accessibility_label(label)
                    .child(
                        div()
                            .w(px(SWATCH_SIZE))
                            .h(px(SWATCH_SIZE))
                            .rounded(px(6.0))
                            .border_2()
                            .border_color(hsla(0.0, 0.0, 1.0, 1.0))
                            .bg(Hsla::from(palette_rgba(color)))
                            .child(
                                // The continuous rounded rectangle's black outer stroke.
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(hsla(0.0, 0.0, 0.0, 1.0)),
                            ),
                    )
                    .on_click(move |_, _, cx| {
                        if session.read(cx).is_mask_selected {
                            // `choosingMaskBackground = background`.
                            entity.update(cx, |this, cx| {
                                this.choosing_mask_background = Some(background);
                                cx.notify();
                            });
                            return;
                        }
                        session.update(cx, |session, _| session.open_color_picker(background));
                    }),
            )
    }

    /// The mask popover: "Mask background" or "Mask foreground", and the two colors.
    fn mask_popover(&self, background: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.clone();
        let entity = cx.entity();
        let choose = move |white: bool, cx: &mut App| {
            session.update(cx, |session, _| {
                session.set_palette_color(
                    if white {
                        compositor_core::PaletteColor::WHITE
                    } else {
                        compositor_core::PaletteColor::BLACK
                    },
                    background,
                );
            });
            entity.update(cx, |this, cx| {
                this.choosing_mask_background = None;
                cx.notify();
            });
        };
        let choose_black = choose.clone();
        let choose_white = choose;
        div()
            .absolute()
            .left(px(SWATCH_SIZE + SWATCH_OFFSET + 4.0))
            .top_0()
            .p(px(16.0))
            .rounded(px(8.0))
            .bg(cx.theme().tokens.popover)
            .border_1()
            .border_color(cx.theme().tokens.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if background {
                                "Mask background"
                            } else {
                                "Mask foreground"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                Button::new("mask-black")
                                    .label("Black · Hide")
                                    .on_click(move |_, _, cx| choose_black(false, cx)),
                            )
                            .child(
                                Button::new("mask-white")
                                    .label("White · Reveal")
                                    .on_click(move |_, _, cx| choose_white(true, cx)),
                            ),
                    ),
            )
    }
}

impl Render for ColorPaletteControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.session.read(cx).can_edit_palette();
        let choosing = self.choosing_mask_background;

        let swap = {
            let session = self.session.clone();
            Button::new("palette-swap")
                .ghost()
                .tooltip("Swap foreground and background (X)")
                .accessibility_label("Swap colors")
                .child(
                    Icon::new(IconName::ArrowLeftRight)
                        .w(px(9.0))
                        .h(px(9.0))
                        // `.rotationEffect(.degrees(45))`.
                        .rotate(Radians(std::f32::consts::FRAC_PI_4)),
                )
                .disabled(!enabled)
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, _| session.swap_palette_colors());
                })
        };

        let reset = {
            let session = self.session.clone();
            Button::new("palette-reset")
                .ghost()
                .tooltip("Default colors (D)")
                .accessibility_label("Default colors")
                .child(Icon::new(IconName::RotateCcw).w(px(7.5)).h(px(7.5)))
                .disabled(!enabled)
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, _| session.reset_palette_colors());
                })
        };

        div()
            .relative()
            .flex_none()
            .w(px(SWATCH_SIZE + SWATCH_OFFSET))
            .h(px(SWATCH_SIZE + SWATCH_OFFSET))
            .child(self.swatch(true, cx))
            .child(self.swatch(false, cx))
            .child(
                // `.offset(x: swatchSize + 3, y: -3)`.
                div()
                    .absolute()
                    .left(px(SWATCH_SIZE + 3.0))
                    .top(px(-3.0))
                    .w(px(12.0))
                    .h(px(12.0))
                    .child(swap),
            )
            .child(
                // `.offset(x: -1, y: swatchSize + 3)`.
                div()
                    .absolute()
                    .left(px(-1.0))
                    .top(px(SWATCH_SIZE + 3.0))
                    .w(px(12.0))
                    .h(px(12.0))
                    .child(reset),
            )
            .children(choosing.map(|background| self.mask_popover(background, cx)))
    }
}
