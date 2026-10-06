//! The Type tool's option bar (port of `UI/TypeControls.swift`): the font face, size, text color,
//! alignment, tracking and leading, with Cancel / Done while text is being edited.

use compositor_core::layer_text::{LayerTextStyle, TextAlignment, TextRange};
use compositor_core::PaletteColor;
use compositor_session::EditorSession;

use crate::canvas::overlays::palette_rgba;
use crate::tool_controls::{unit_suffix, FieldSpec, Fields};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::Icon;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The Size field's range (`range: 1...2000`), and `.frame(width: 52)`.
pub const SIZE_RANGE: (f64, f64) = (1.0, 2000.0);
pub const SIZE_FIELD_WIDTH: f32 = 52.0;
/// Tracking's scrub range (`range: -100...1000`) and `.frame(width: 45)`.
pub const TRACKING_SCRUB_RANGE: (f64, f64) = (-100.0, 1000.0);
pub const TRACKING_FIELD_WIDTH: f32 = 45.0;
/// Leading's scrub range (`range: 0...5000`) and `.frame(width: 52)`.
pub const LEADING_SCRUB_RANGE: (f64, f64) = (0.0, 5000.0);
pub const LEADING_FIELD_WIDTH: f32 = 52.0;
/// `.frame(width: 210)` — the font menu's width.
pub const FONT_PICKER_WIDTH: f32 = 210.0;
/// The alignment buttons' frames (`frame(width: 30, height: 26)`).
pub const ALIGNMENT_WIDTH: f32 = 30.0;
pub const ALIGNMENT_HEIGHT: f32 = 26.0;

/// The Type bar: the font menu and everything that shapes the text, in a horizontal scroller.
pub struct TypeControls {
    session: Entity<EditorSession>,
    fields: Fields,
    /// The color the picker last showed, so `onChange(of: session.colorPicker?.color)` can fire.
    last_picker_color: Option<PaletteColor>,
}

impl TypeControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        let last_picker_color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
        cx.observe(&session, |this, session, cx| {
            // `onChange(of: session.colorPicker?.color) { session.previewTextColor() }`.
            let color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
            if color != this.last_picker_color {
                this.last_picker_color = color;
                session.update(cx, |session, _| session.preview_text_color());
            }
            cx.notify();
        })
        .detach();
        Self {
            session,
            fields: Fields::default(),
            last_picker_color,
        }
    }

    /// The face the menu shows: the draft's uniform face over the selection, the face of the letter
    /// before the caret in an empty selection, or the style's own — `TypeFontPicker`'s binding.
    fn font_name(style: &LayerTextStyle, selection: Option<TextRange>) -> String {
        match selection {
            None => style.font_name.clone(),
            Some(selection) => {
                if selection.length == 0 {
                    // `draft.style.fontName(at: max(0, selection.location - 1))`.
                    style.font_name((selection.location - 1).max(0))
                } else {
                    // No single face: an empty title, so choosing the first letter's face still
                    // applies to the rest.
                    style.uniform_font_name(selection).unwrap_or_default()
                }
            }
        }
    }
}

impl Render for TypeControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (style, selection, has_draft, can_edit_text, enabled) = {
            let session = self.session.read(cx);
            let style = session.current_text_style();
            let selection = session.text_draft.as_ref().map(|draft| draft.selection);
            (
                style,
                selection,
                session.text_draft.is_some(),
                session
                    .active_layer()
                    .and_then(|layer| layer.live_text())
                    .is_some(),
                session.document.is_some() && !session.shows_busy,
            )
        };
        let font_name = Self::font_name(&style, selection);
        let selection = selection.unwrap_or(TextRange::EMPTY);

        // The face menu: every installed face, each drawn in its own name, with the face under the
        // pointer tried on the text (`TypeFontPicker`'s preview steps). The Swift styled each name
        // only when the face could draw it; here every name is set in its face.
        let font_menu = {
            let session = self.session.clone();
            let names = compositor_pixels::text::font_names();
            let multiple = font_name.is_empty();
            let title = if multiple {
                "(Multiple)".to_string()
            } else {
                font_name.clone()
            };
            DropdownButton::new("type-font")
                .button(
                    Button::new("type-font-button")
                        .label(title)
                        .h(px(crate::tool_controls::FIELD_HEIGHT))
                        .text_size(px(crate::tool_header::CONTROL_SIZE)),
                )
                .w(px(FONT_PICKER_WIDTH))
                .disabled(!enabled)
                .dropdown_menu(move |menu, _, _| {
                    let menu = if multiple {
                        menu.item(
                            PopupMenuItem::element(|_, _| div().child("(Multiple)"))
                                .disabled(true),
                        )
                    } else {
                        menu
                    };
                    names.iter().fold(menu, |menu, name| {
                        let face = name.clone();
                        let on_hover_session = session.clone();
                        let on_pick_session = session.clone();
                        let pick_face = face.clone();
                        let checked = *name == font_name;
                        menu.item(
                            PopupMenuItem::element(move |_, _| {
                                let hover_face = face.clone();
                                let hover_session = on_hover_session.clone();
                                div()
                                    .id(SharedString::from(format!("font-face-{face}")))
                                    .font_family(face.clone())
                                    .child(face.clone())
                                    .on_hover(move |hovered, _, cx| {
                                        // `.show(name)` under the pointer, `.revert` when it leaves.
                                        if *hovered {
                                            let face = hover_face.clone();
                                            hover_session.update(cx, |session, _| {
                                                session.preview_font(&face)
                                            });
                                        } else {
                                            hover_session
                                                .update(cx, |session, _| session.end_font_preview());
                                        }
                                    })
                            })
                            .checked(checked)
                            .on_click(move |_, _, cx| {
                                // `.keep`: the text already shows the face under the pointer.
                                on_pick_session
                                    .update(cx, |session, _| session.keep_font_preview());
                                let name = pick_face.clone();
                                on_pick_session.update(cx, |session, _| {
                                    session.change_text_style(|style| style.set_font(&name, selection));
                                });
                            }),
                        )
                    })
                })
        };

        let size_field = self.fields.get("type-size", cx);
        let size_scrub = {
            let session = self.session.clone();
            NumericScrub::new(style.font_size, 1.0, SIZE_RANGE)
                .step(1.0)
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.change_text_style(|style| style.font_size = value);
                    });
                })
        };
        let size_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_text_style(|style| style.font_size = value);
                });
            }
        };

        let color_swatch = {
            let session = self.session.clone();
            Button::new("type-color")
                .tooltip("Text color")
                .accessibility_label("Text color")
                .disabled(!enabled)
                .child(
                    div()
                        .w(px(36.0))
                        .h(px(18.0))
                        .rounded(px(3.0))
                        .border_1()
                        .border_color(hsla(0.0, 0.0, 0.0, 0.5))
                        .bg(Hsla::from(palette_rgba(
                            self.session.read(cx).type_color(),
                        ))),
                )
                .on_click(move |_, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.open_text_color_picker());
                })
        };

        let alignment = {
            let session = self.session.clone();
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .children(TextAlignment::ALL.map(|alignment| {
                    let selected = style.alignment == alignment;
                    let (icon, help) = match alignment {
                        TextAlignment::Left => (IconName::TextAlignStart, "Align left"),
                        TextAlignment::Center => (IconName::TextAlignCenter, "Align center"),
                        TextAlignment::Right => (IconName::TextAlignEnd, "Align right"),
                    };
                    let session = session.clone();
                    Button::new((ElementId::from("type-align"), alignment.raw_value()))
                        .tooltip(help)
                        .accessibility_label(help)
                        .selected(selected)
                        .disabled(!enabled)
                        .w(px(ALIGNMENT_WIDTH))
                        .h(px(ALIGNMENT_HEIGHT))
                        .rounded(px(4.0))
                        .child(Icon::new(icon))
                        .on_click(move |_, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| {
                                session.change_text_style(|style| style.alignment = alignment);
                            });
                        })
                }))
        };

        let tracking_field = self.fields.get("type-tracking", cx);
        let tracking_scrub = {
            let session = self.session.clone();
            NumericScrub::new(style.tracking, 1.0, TRACKING_SCRUB_RANGE)
                .step(1.0)
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.change_text_style(|style| style.tracking = value);
                    });
                })
        };
        let tracking_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_text_style(|style| style.tracking = value);
                });
            }
        };

        let leading_field = self.fields.get("type-leading", cx);
        let leading_scrub = {
            let session = self.session.clone();
            NumericScrub::new(style.leading, 1.0, LEADING_SCRUB_RANGE)
                .step(1.0)
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.change_text_style(|style| style.leading = value);
                    });
                })
        };
        let leading_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_text_style(|style| style.leading = value);
                });
            }
        };

        let actions = if has_draft {
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child({
                    let session = self.session.clone();
                    Button::new("type-cancel")
                        .label("Cancel")
                        .on_click(move |_, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| session.cancel_text());
                        })
                })
                .child({
                    let session = self.session.clone();
                    Button::new("type-done")
                        .label("Done")
                        .on_click(move |_, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| {
                                let _ = session.finish_text();
                            });
                        })
                })
        } else {
            div().flex().items_center().child({
                let session = self.session.clone();
                Button::new("type-edit-text")
                    .label("Edit Text")
                    .disabled(!can_edit_text)
                    .on_click(move |_, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.edit_active_text());
                    })
            })
        };

        let scroller = div()
            .id("type-scroller")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .overflow_x_scroll()
            // `.scrollIndicators(.hidden)`.
            .scrollbar_width(px(0.0))
            .child(
                div()
                    .id("type-font-label")
                    .w(px(FONT_PICKER_WIDTH))
                    .tooltip(|window, cx| {
                        Tooltip::new("Font face, including bold and italic variants").build(window, cx)
                    })
                    .child(font_menu),
            )
            .child(unit_suffix(
                size_field.element(
                    "type-size-field",
                    style.font_size,
                    FieldSpec::new(SIZE_RANGE, 0).fallback(1.0).disabled(!enabled),
                    SIZE_FIELD_WIDTH,
                    size_write,
                    cx,
                ),
                div().child("px").scrubbable("type-size-unit", size_scrub),
            ))
            .child(color_swatch)
            .child(alignment)
            .child(
                div()
                    .child("Tracking")
                    .scrubbable("type-tracking-label", tracking_scrub),
            )
            .child(tracking_field.element(
                "type-tracking-field",
                style.tracking,
                FieldSpec::new((-100_000.0, 100_000.0), 0)
                    .fallback(0.0)
                    .disabled(!enabled),
                TRACKING_FIELD_WIDTH,
                tracking_write,
                cx,
            ))
            .child(
                div()
                    .child("Leading")
                    .scrubbable("type-leading-label", leading_scrub),
            )
            .child(
                leading_field
                    .element(
                        "type-leading-field",
                        style.leading,
                        FieldSpec::new(LEADING_SCRUB_RANGE, 0)
                            // `arrowSteps(value: { session.currentTextStyle.lineHeight }, …)`: the
                            // arrows start from the Auto line height while Leading is 0.
                            .step_from(style.line_height())
                            .fallback(0.0)
                            .placeholder("Auto")
                            .empty_value(0.0)
                            .disabled(!enabled),
                        LEADING_FIELD_WIDTH,
                        leading_write,
                        cx,
                    )
                    .tooltip(|window, cx| {
                        Tooltip::new("Line height, baseline to baseline. Empty or 0 is Auto: 120% of the font size.")
                            .build(window, cx)
                    }),
            );

        tool_header_bar(12.0)
            .child(tool_header_title("Type"))
            .child(div().flex().flex_row().flex_1().min_w(px(0.0)).child(scroller))
            .child(actions)
    }
}
