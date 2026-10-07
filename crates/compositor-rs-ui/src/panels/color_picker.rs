//! Photoshop-style color picker: saturation/brightness field, vertical hue strip, new/current
//! preview, RGB and hex entry.
//!
//! Ported from `UI/ColorPickerSheet.swift`. The Swift showed it in a movable `NSPanel` that floated
//! above the document and above any sheet, so the canvas stayed visible and could be clicked to
//! sample a color; closing the panel with its title-bar button cancelled. The port's
//! [`ColorPickerPanelController`] hosts the same sheet in whichever surface is on top: one of the
//! editor's floating panels while the editor is, and a dialog of the window's own layer while a
//! dialog is.
//!
//! Two platform notes. The Swift `DragGesture` on the two fields is gpui's drag payload, so the
//! fields' own bounds are recorded while they paint and the pointer's travel is measured inside
//! them. And the Swift's `DialogColorSwatch` heard the working color through `onChange`, which the
//! port cannot: the picker itself tells the dialog as the color moves
//! ([`ColorPickerSheet::set_hsb`]), which is the same observable behaviour.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::palette::{ColorPickerTarget, PickerHSB};
use compositor_rs_session::EditorSession;

use crate::canvas::overlays::palette_rgba;
use crate::panels::floating_panel::{FloatingPanelController, FloatingPanelPlacement};
use crate::widgets::numeric_scrub::{arrow_stepped, NumericScrub, Scrubbable as _};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

/// The saturation/brightness field's side (`fieldSize`).
pub const FIELD_SIZE: f32 = 256.0;
/// The column the OK/Cancel buttons and the fields live in (`.frame(width: 180)`).
const COLUMN_WIDTH: f32 = 180.0;
/// The sheet's `padding(20)`.
const PANEL_PADDING: f32 = 20.0;
/// The sheet's `HStack(spacing: 14)`.
const COLUMN_GAP: f32 = 14.0;
/// The hue strip's own width (`stripWidth`).
const STRIP_WIDTH: f32 = 20.0;
/// The strip's horizontal padding, 7 points on each side.
const STRIP_PADDING: f32 = 7.0;
/// The color preview's side (`.frame(width: 64, height: 64)`).
const PREVIEW_SIZE: f32 = 64.0;
/// The hex field's width (`.frame(width: 84)`).
const HEX_WIDTH: f32 = 84.0;
/// A channel field's width (`.frame(width: 52)`).
const CHANNEL_WIDTH: f32 = 52.0;
/// `stride(from: 360.0, through: 0, by: -60)`: the hue strip's stops, top to bottom.
const HUE_STOPS: [f64; 7] = [360.0, 300.0, 240.0, 180.0, 120.0, 60.0, 0.0];

/// The sheet's own width: padding, field, gap, strip, gap, column, padding. The hosts size
/// themselves to it, as the AppKit panel took the sheet's `fittingSize`.
const PICKER_WIDTH: f32 = PANEL_PADDING * 2.0 + FIELD_SIZE + COLUMN_GAP + STRIP_WIDTH + STRIP_PADDING * 2.0 + COLUMN_GAP + COLUMN_WIDTH;
/// What the dialog host adds above the sheet: its own `pt(8)`, the title's line, and the card's
/// `gap(8)`. Only used to centre the card.
const DIALOG_TITLE_HEIGHT: f32 = 34.0;
/// The sheet's own height: its padding around the field.
const PICKER_HEIGHT: f32 = PANEL_PADDING * 2.0 + FIELD_SIZE;

/// The empty view a field's drag carries: dragging sets the color, so no ghost is shown.
struct PickerDrag;

struct PickerDragView;

impl Render for PickerDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The picker's sheet: the fields, the preview and the OK/Cancel pair.
pub struct ColorPickerSheet {
    session: Entity<EditorSession>,
    /// `finish(_:)`: true on OK, false on Cancel.
    finish: Option<Box<dyn FnOnce(bool, &mut Window, &mut App)>>,
    /// The hex draft and its field (`hexDraft`, `hexFocused`).
    hex_field: Entity<InputState>,
    hex_was_focused: bool,
    /// The R, G and B fields.
    channels: [Entity<InputState>; 3],
    channel_was_focused: [bool; 3],
    channel_last_text: [String; 3],
}

impl ColorPickerSheet {
    pub fn new(
        session: Entity<EditorSession>,
        finish: impl FnOnce(bool, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hex = session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| picker.color().hex())
            .unwrap_or_else(|| PaletteColor::BLACK.hex());
        let hex_field = cx.new(|cx| InputState::new(window, cx).placeholder("Hex").default_value(hex));
        let channel_text: [String; 3] = {
            let color = Self::session_color(&session, cx);
            [channel_text(color.red), channel_text(color.green), channel_text(color.blue)]
        };
        let channels = [
            cx.new(|cx| InputState::new(window, cx).placeholder("R").default_value(channel_text[0].clone())),
            cx.new(|cx| InputState::new(window, cx).placeholder("G").default_value(channel_text[1].clone())),
            cx.new(|cx| InputState::new(window, cx).placeholder("B").default_value(channel_text[2].clone())),
        ];
        Self {
            session,
            finish: Some(Box::new(finish)),
            hex_field,
            hex_was_focused: false,
            channels,
            channel_was_focused: [false; 3],
            channel_last_text: channel_text,
        }
    }

    fn session_color(session: &Entity<EditorSession>, cx: &App) -> PaletteColor {
        session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| picker.color())
            .unwrap_or(PaletteColor::BLACK)
    }

    /// `state.color`.
    fn color(&self, cx: &App) -> PaletteColor {
        Self::session_color(&self.session, cx)
    }

    /// `state.hsb`.
    fn hsb(&self, cx: &App) -> PickerHSB {
        self.session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| picker.hsb)
            .unwrap_or_else(|| PickerHSB::from_color(PaletteColor::BLACK))
    }

    /// `state.hsb = newValue`: nothing is written to the palette until OK, but a dialog watching the
    /// picker hears the working color as it moves (`previewDialogColor`).
    fn set_hsb(&mut self, hsb: PickerHSB, cx: &mut Context<Self>) {
        self.session.update(cx, |session, _| {
            if let Some(picker) = session.color_picker.as_mut() {
                picker.hsb = hsb;
            }
            session.preview_dialog_color();
        });
        cx.notify();
    }

    /// One channel row's binding: `hsb.setRGB(rgb)` after the channel is written.
    fn set_channel(&mut self, channel: usize, value: f64, cx: &mut Context<Self>) {
        let mut rgb = self.color(cx);
        let value = (value.round().clamp(0.0, 255.0)) / 255.0;
        match channel {
            0 => rgb.red = value,
            1 => rgb.green = value,
            _ => rgb.blue = value,
        }
        let mut hsb = self.hsb(cx);
        hsb.set_rgb(rgb);
        self.set_hsb(hsb, cx);
    }

    /// `commitHex()`: the hex field's text becomes the color, then the field shows `color.hex`.
    fn commit_hex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.hex_field.read(cx).value().to_string();
        if let Some(parsed) = PaletteColor::from_hex(&text) {
            let mut hsb = self.hsb(cx);
            hsb.set_rgb(parsed);
            self.set_hsb(hsb, cx);
        }
        let text = self.color(cx).hex();
        self.hex_field
            .update(cx, |field, cx| field.set_value(text, window, cx));
    }

    /// The saturation/brightness field: `LinearGradient(colors: [.white, hue])` sideways, then
    /// `LinearGradient(colors: [.clear, .black])` downwards, with the marker on top.
    fn saturation_brightness_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hsb = self.hsb(cx);
        let hue_color = palette_rgba(PickerHSB::new(hsb.hue, 1.0, 1.0).rgb());
        let entity = cx.entity();
        let sv_bounds: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
        let press_bounds = sv_bounds.clone();
        let drag_bounds = sv_bounds.clone();
        let entity_for_keys = cx.entity();

        let marker = div()
            .absolute()
            .left(px(hsb.saturation as f32 * FIELD_SIZE - 6.0))
            .top(px((1.0 - hsb.brightness) as f32 * FIELD_SIZE - 6.0))
            .size(px(12.0))
            .rounded_full()
            .border(px(1.5))
            .border_color(hsla(0.0, 0.0, 1.0, 1.0))
            // `.background(Circle().strokeBorder(.black, lineWidth: 0.75).padding(-0.75))`: a
            // black hairline just outside the white ring, so the marker also reads on white. The
            // background view is layout-neutral in SwiftUI, so the 12×12 frame stays — the ring
            // overflows by 0.75 on each side.
            .child(
                div()
                    .absolute()
                    .top(px(-0.75))
                    .left(px(-0.75))
                    .size(px(13.5))
                    .rounded_full()
                    .border(px(0.75))
                    .border_color(hsla(0.0, 0.0, 0.0, 1.0)),
            );

        div()
            .id(ElementId::Name("color-picker-sv-field".into()))
            .relative()
            .size(px(FIELD_SIZE))
            .overflow_hidden()
            .bg(linear_gradient(
                90.0,
                linear_color_stop(hsla(0.0, 0.0, 1.0, 1.0), 0.0),
                linear_color_stop(hue_color, 1.0),
            ))
            .border_1()
            .border_color(hsla(0.0, 0.0, 0.0, 0.6))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(hsla(0.0, 0.0, 0.0, 0.0), 0.0),
                        linear_color_stop(hsla(0.0, 0.0, 0.0, 1.0), 1.0),
                    )),
            )
            // The field's own rectangle, so a drag knows what 0…1 means.
            .child(
                canvas(
                    {
                        let cell = sv_bounds.clone();
                        move |bounds, _, _| cell.set(bounds)
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(marker)
            .on_mouse_down(MouseButton::Left, {
                let entity = entity_for_keys.clone();
                move |event: &MouseDownEvent, _, cx| {
                    let bounds = press_bounds.get();
                    set_saturation_brightness(&entity, bounds, event.position, cx);
                }
            })
            .on_drag(PickerDrag, |_, _, _, cx| cx.new(|_| PickerDragView))
            .on_drag_move::<PickerDrag>(move |event: &DragMoveEvent<PickerDrag>, _, cx| {
                let bounds = drag_bounds.get();
                set_saturation_brightness(&entity, bounds, event.event.position, cx);
            })
    }

    /// The hue strip: seven stops top to bottom, with the two arrows beside it.
    fn hue_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hue = self.hsb(cx).hue;
        // `HueArrow().fill(.primary)`.
        let primary = cx.theme().foreground;
        let marker_y = (1.0 - hue / 360.0) as f32 * FIELD_SIZE;
        let entity = cx.entity();
        let strip_bounds: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
        let press_bounds = strip_bounds.clone();
        let drag_bounds = strip_bounds.clone();

        let segments: Vec<AnyElement> = HUE_STOPS
            .windows(2)
            .map(|pair| {
                let from = palette_rgba(PickerHSB::new(pair[0], 1.0, 1.0).rgb());
                let to = palette_rgba(PickerHSB::new(pair[1], 1.0, 1.0).rgb());
                div()
                    .flex_1()
                    .w_full()
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(from, 0.0),
                        linear_color_stop(to, 1.0),
                    ))
                    .into_any_element()
            })
            .collect();

        div()
            .id(ElementId::Name("color-picker-hue-strip".into()))
            .relative()
            .w(px(STRIP_WIDTH + STRIP_PADDING * 2.0))
            .h(px(FIELD_SIZE))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(STRIP_PADDING))
                    .w(px(STRIP_WIDTH))
                    .h(px(FIELD_SIZE))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 0.0, 0.6))
                    .children(segments),
            )
            // `HStack(spacing: stripWidth) { HueArrow(); HueArrow().scaleEffect(x: -1) }`,
            // `.offset(y: markerY - 5)`.
            .child(
                h_flex()
                    .absolute()
                    .top(px(marker_y - 5.0))
                    .left_0()
                    .gap(px(STRIP_WIDTH))
                    .child(hue_arrow(false, primary))
                    .child(hue_arrow(true, primary)),
            )
            .child(
                canvas(
                    {
                        let cell = strip_bounds.clone();
                        move |bounds, _, _| cell.set(bounds)
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                move |event: &MouseDownEvent, _, cx| {
                    let bounds = press_bounds.get();
                    set_hue(&entity, bounds, event.position, cx);
                }
            })
            .on_drag(PickerDrag, |_, _, _, cx| cx.new(|_| PickerDragView))
            .on_drag_move::<PickerDrag>(move |event: &DragMoveEvent<PickerDrag>, _, cx| {
                let bounds = drag_bounds.get();
                set_hue(&entity, bounds, event.event.position, cx);
            })
    }

    /// `preview`: the new color over a black hairline.
    fn preview(&self, cx: &App) -> impl IntoElement {
        div()
            .size(px(PREVIEW_SIZE))
            .rounded(px(5.0))
            .bg(palette_rgba(self.color(cx)))
            .border_1()
            .border_color(hsla(0.0, 0.0, 0.0, 0.6))
    }

    /// One channel row: the scrubbable label and the number field.
    fn channel_row(&self, channel: usize, label: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let color = self.color(cx);
        let value = match channel {
            0 => color.red,
            1 => color.green,
            _ => color.blue,
        };
        let entity = cx.entity();
        let key_entity = cx.entity();
        let key_entity_for_step = cx.entity();
        let field = self.channels[channel].clone();
        let field_for_key = field.clone();
        let accessibility = match channel {
            0 => "Red",
            1 => "Green",
            _ => "Blue",
        };

        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(14.0))
                    .text_size(px(12.0))
                    .child(label)
                    .scrubbable(
                        format!("color-picker-channel-{label}"),
                        NumericScrub::new((value * 255.0).round(), 1.0, (0.0, 255.0)).on_change(
                            move |value, _, cx| {
                                entity.update(cx, |sheet, cx| sheet.set_channel(channel, value, cx));
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .w(px(CHANNEL_WIDTH))
                    .capture_key_down(move |event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "up" || event.keystroke.key == "down" {
                            let color = key_entity.read(cx).color(cx);
                            let value = match channel {
                                0 => color.red,
                                1 => color.green,
                                _ => color.blue,
                            } * 255.0;
                            let next = arrow_stepped(
                                value.round(),
                                1.0,
                                event.keystroke.key == "up",
                                event.keystroke.modifiers,
                            );
                            key_entity_for_step.update(cx, |sheet, cx| sheet.set_channel(channel, next, cx));
                            cx.stop_propagation();
                        }
                    })
                    .child(Input::new(&field_for_key).aria_label(accessibility)),
            )
    }

    /// The hex row: `#` and the monospaced draft field.
    fn hex_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .capture_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" {
                    entity.update(cx, |sheet, cx| sheet.commit_hex(window, cx));
                    cx.stop_propagation();
                }
            })
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().w(px(14.0)).text_size(px(12.0)).child("#"))
                    .child(
                        div()
                            .w(px(HEX_WIDTH))
                            .font_family("monospace")
                            .child(Input::new(&self.hex_field).aria_label("Hex color")),
                    ),
            )
    }

    /// `.onChange(of: color) { if !hexFocused { hexDraft = new.hex } }` and the channel fields'
    /// text, which follow the color while they are not being typed in.
    fn sync_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let color = self.color(cx);
        let hex_focused = self.hex_field.read(cx).focus_handle(cx).is_focused(window);
        if self.hex_was_focused && !hex_focused {
            // `.onChange(of: hexFocused) { if !hexFocused { commitHex() } }`.
            self.commit_hex(window, cx);
        }
        self.hex_was_focused = hex_focused;
        if !hex_focused {
            let text = color.hex();
            if self.hex_field.read(cx).value().as_ref() != text {
                self.hex_field
                    .update(cx, |field, cx| field.set_value(text, window, cx));
            }
        }
        for channel in 0..3 {
            let value = match channel {
                0 => color.red,
                1 => color.green,
                _ => color.blue,
            };
            let text = channel_text(value);
            let field = self.channels[channel].clone();
            let focused = field.read(cx).focus_handle(cx).is_focused(window);
            if focused {
                // The field's binding is a number: typing applies it as it goes.
                let typed = field.read(cx).value().to_string();
                if typed != self.channel_last_text[channel] {
                    self.channel_last_text[channel] = typed.clone();
                    if let Ok(number) = typed.trim().parse::<f64>() {
                        if number.is_finite() {
                            self.set_channel(channel, number, cx);
                        }
                    }
                }
            } else {
                if self.channel_was_focused[channel] {
                    // Clearing the field puts the color's own number back.
                    field.update(cx, |field, cx| field.set_value(text.clone(), window, cx));
                } else if field.read(cx).value().as_ref() != text {
                    field.update(cx, |field, cx| field.set_value(text.clone(), window, cx));
                }
                self.channel_last_text[channel] = text;
            }
            self.channel_was_focused[channel] = focused;
        }
    }

    /// `finish(_:)`, once.
    fn close(&mut self, commit: bool, window: &mut Window, cx: &mut App) {
        if let Some(finish) = self.finish.take() {
            finish(commit, window, cx);
        }
    }
}

/// `String(Int((color.red * 255).rounded()))`.
fn channel_text(value: f64) -> String {
    format!("{}", (value * 255.0).round() as i64)
}

/// The two field drags' shared arithmetic: a window point inside the field, as fractions.
fn field_fractions(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f32)> {
    if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
        return None;
    }
    let x = (f32::from(position.x) - f32::from(bounds.origin.x)) / f32::from(bounds.size.width);
    let y = (f32::from(position.y) - f32::from(bounds.origin.y)) / f32::from(bounds.size.height);
    Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)))
}

/// `hsb.saturation = x; hsb.brightness = 1 - y`.
fn set_saturation_brightness(
    entity: &Entity<ColorPickerSheet>,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    cx: &mut App,
) {
    let Some((x, y)) = field_fractions(bounds, position) else {
        return;
    };
    entity.update(cx, |sheet, cx| {
        let mut hsb = sheet.hsb(cx);
        hsb.saturation = f64::from(x);
        hsb.brightness = 1.0 - f64::from(y);
        sheet.set_hsb(hsb, cx);
    });
}

/// `hsb.hue = (1 - y) * 360`.
fn set_hue(
    entity: &Entity<ColorPickerSheet>,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    cx: &mut App,
) {
    let Some((_, y)) = field_fractions(bounds, position) else {
        return;
    };
    entity.update(cx, |sheet, cx| {
        let mut hsb = sheet.hsb(cx);
        hsb.hue = (1.0 - f64::from(y)) * 360.0;
        sheet.set_hsb(hsb, cx);
    });
}

/// `HueArrow`: a 7×10 triangle filled `.primary`, `mirrored` for the right-hand one
/// (`.scaleEffect(x: -1)`).
fn hue_arrow(mirrored: bool, color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = bounds.size.width;
            let height = bounds.size.height;
            let (left, right) = if mirrored {
                (bounds.origin.x + width, bounds.origin.x)
            } else {
                (bounds.origin.x, bounds.origin.x + width)
            };
            let mut path = PathBuilder::fill();
            path.move_to(point(left, bounds.origin.y));
            path.line_to(point(right, bounds.origin.y + height / 2.0));
            path.line_to(point(left, bounds.origin.y + height));
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w(px(7.0))
    .h(px(10.0))
}

impl Render for ColorPickerSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_fields(window, cx);
        let dialog = self
            .session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| matches!(picker.target, ColorPickerTarget::Dialog { .. }))
            .unwrap_or(false);

        let ok_entity = cx.entity();
        let cancel_entity = cx.entity();

        h_flex()
            .items_start()
            .gap(px(COLUMN_GAP))
            .p(px(PANEL_PADDING))
            .child(self.saturation_brightness_field(cx))
            .child(self.hue_strip(cx))
            .child(
                v_flex()
                    .w(px(COLUMN_WIDTH))
                    .h(px(FIELD_SIZE))
                    .items_start()
                    .child(
                        h_flex()
                            .items_start()
                            .gap(px(16.0))
                            .child(self.preview(cx))
                            .child(
                                v_flex()
                                    .w(px(90.0))
                                    .gap(px(8.0))
                                    .child(
                                        Button::new("color-picker-ok")
                                            .label("OK")
                                            .w_full()
                                            .on_click(move |_, window, cx| {
                                                ok_entity.update(cx, |sheet, cx| sheet.close(true, window, cx));
                                            }),
                                    )
                                    .child(
                                        Button::new("color-picker-cancel")
                                            .label("Cancel")
                                            .w_full()
                                            .on_click(move |_, window, cx| {
                                                cancel_entity.update(cx, |sheet, cx| sheet.close(false, window, cx));
                                            }),
                                    ),
                            ),
                    )
                    // `Spacer(minLength: 12)`.
                    .child(div().flex_1().min_h(px(12.0)))
                    .child(
                        v_flex()
                            .gap(px(6.0))
                            .child(self.channel_row(0, "R", cx))
                            .child(self.channel_row(1, "G", cx))
                            .child(self.channel_row(2, "B", cx))
                            .child(self.hex_row(cx)),
                    )
                    // A dialog covers the canvas, so there's nothing to sample.
                    .when(!dialog, |this| {
                        this.child(
                            div()
                                .pt(px(8.0))
                                .text_size(px(12.0))
                                .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                                .child("Click the canvas to sample"),
                        )
                    }),
            )
    }
}

/// The editor's canvas row, in window coordinates (`ContentView` centres its panels in the same box).
/// A palette can only be edited while a document is open, so the tool header is up in both hosts and
/// the row is the same either way: below the header's divider, above the status bar's.
fn canvas_row(viewport_height: f32) -> (f32, f32) {
    let top = f32::from(gpui_kit::component::TITLE_BAR_HEIGHT)
        + crate::workspace::TAB_STRIP_HEIGHT
        + crate::tool_header::TOOL_HEADER_HEIGHT
        + crate::content_view::DIVIDER_HEIGHT;
    let height = (viewport_height
        - top
        - crate::content_view::DIVIDER_HEIGHT
        - crate::toolbar::status_bar::HEIGHT)
        .max(0.0);
    (top, height)
}

/// The picker's own window, the port of `ColorPickerPanelController`: first opened centered on the
/// canvas, afterwards wherever it was last left, and closed by its title-bar button.
///
/// The Swift put the sheet in one `NSPanel` that floated above the document and above any sheet. The
/// port has two hosts for that one panel, chosen by what is on top of the editor:
///
/// * with the editor on top the picker is one of the editor's floating panels ([`Self::sync`]), which
///   is the only host that leaves the canvas clickable — sampling from it is the picker's point;
/// * with a dialog on top — a picker a dialog's own swatch opened, or any picker while a dialog came
///   up — it is a dialog of the window's own layer ([`Self::sync_dialog`]), because the editor's
///   panels are drawn below that layer, where the dialog's overlay dims and blocks them.
///
/// A dialog can neither be dragged nor keep the canvas clickable, so the panel is the host that
/// matches the Swift's panel; the dialog host only stands in for it above a modal, where the Swift's
/// own hint says there is nothing to sample.
pub struct ColorPickerPanelController {
    panel: FloatingPanelController,
    /// The title the editor panel shows, `None` while it is hidden.
    title: Option<String>,
    /// The target the window-level dialog shows, `None` while it is hidden.
    dialog: Option<ColorPickerTarget>,
    /// Whether the picker has already taken its own dialog down: the one thing that tells a pop
    /// already made from one still to make, so the dialog underneath is never popped by mistake.
    dialog_taken_down: Rc<Cell<bool>>,
}

impl Default for ColorPickerPanelController {
    fn default() -> Self {
        Self::new()
    }
}

impl ColorPickerPanelController {
    pub const IDENTIFIER: &'static str = "colorPickerPanel";

    pub fn new() -> Self {
        Self {
            panel: FloatingPanelController::new(Self::IDENTIFIER),
            title: None,
            dialog: None,
            dialog_taken_down: Rc::new(Cell::new(false)),
        }
    }

    /// `show(_:session:)` for a color the canvas can be sampled for: shown as one of the editor's
    /// floating panels while the editor is on top, and put away as soon as the picker closes.
    pub fn sync(
        &mut self,
        session: Entity<EditorSession>,
        dialog_up: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = match session.read(cx).color_picker.as_ref() {
            Some(picker) if !dialog_up && !matches!(picker.target, ColorPickerTarget::Dialog { .. }) => {
                Some(picker.target.title())
            }
            _ => None,
        };
        let Some(title) = title else {
            self.title = None;
            self.panel.close(window, cx);
            return;
        };
        // Picking another swatch re-shows the panel where it was left, as the Swift's `onChange` did.
        if self.panel.is_visible() && self.title.as_deref() == Some(title.as_str()) {
            return;
        }
        self.title = Some(title.clone());
        // The panel's close button cancels, as the Swift's did.
        let closing = session.clone();
        self.panel.set_on_close(move |_, cx| {
            if closing.read(cx).color_picker.is_some() {
                closing.update(cx, |session, _| session.close_color_picker(false));
            }
        });
        let finishing = session.clone();
        let sheet = cx.new(|cx| {
            ColorPickerSheet::new(
                session.clone(),
                move |commit, _, cx| {
                    finishing.update(cx, |session, _| session.close_color_picker(commit));
                },
                window,
                cx,
            )
        });
        self.panel
            .show(title, sheet, FloatingPanelPlacement::Automatic, window, cx);
    }

    /// Ties the window's dialog layer to the picker: the host to use while the picker has to float
    /// above a dialog, and taken down again when the picker closes.
    pub fn sync_dialog(
        &mut self,
        session: Option<Entity<EditorSession>>,
        dialog_up: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let target = session.as_ref().and_then(|session| {
            session
                .read(cx)
                .color_picker
                .as_ref()
                .map(|picker| picker.target.clone())
        });
        let target = target.filter(|target| {
            dialog_up || matches!(target, ColorPickerTarget::Dialog { .. })
        });
        if target == self.dialog {
            return;
        }
        // The picker's own dialog is taken down by the picker (its OK, its Cancel, its close button
        // and Escape all pop it as they close it). A picker the session clears from anywhere else is
        // taken down here instead.
        let was_open = self.dialog.take().is_some();
        if was_open && !self.dialog_taken_down.replace(false) {
            window.close_dialog(cx);
        }
        let Some(session) = session.filter(|_| target.is_some()) else {
            return;
        };
        self.dialog = target;
        self.open_dialog(session, window, cx);
    }

    /// Shows the picker in the window's dialog layer: a dialog of its own, with no overlay (the picker
    /// is not modal) and no footer (`ColorPickerSheet` has its own OK and Cancel).
    fn open_dialog(&mut self, session: Entity<EditorSession>, window: &mut Window, cx: &mut App) {
        let Some(title) = session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| picker.target.title())
        else {
            return;
        };
        let taken_down = Rc::new(Cell::new(false));
        self.dialog_taken_down = taken_down.clone();
        let finishing = session.clone();
        let finish_flag = taken_down.clone();
        let sheet = cx.new(|cx| {
            ColorPickerSheet::new(
                session.clone(),
                move |commit, window, cx| {
                    finishing.update(cx, |session, _| session.close_color_picker(commit));
                    if !finish_flag.replace(true) {
                        window.close_dialog(cx);
                    }
                },
                window,
                cx,
            )
        });
        let (row_top, row_height) = canvas_row(f32::from(window.viewport_size().height));
        let margin_top = row_top + ((row_height - PICKER_HEIGHT - DIALOG_TITLE_HEIGHT) / 2.0).max(0.0);
        let cancel_session = session.clone();
        let cancel_flag = taken_down.clone();
        let close_session = session.clone();
        let close_flag = taken_down.clone();
        let dialog_sheet = sheet.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            // The builder runs on every frame, so the handlers it installs clone what they use.
            let cancelling = cancel_session.clone();
            let cancel_flag = cancel_flag.clone();
            let closing = close_session.clone();
            let close_flag = close_flag.clone();
            let sheet = dialog_sheet.clone();
            dialog
                .title(div().pl(px(12.0)).child(title.clone()))
                // The dialog's own padding would double the sheet's, and only its overlay would dim
                // the editor the picker samples.
                .p(px(0.0))
                .pt(px(8.0))
                .w(px(PICKER_WIDTH))
                .margin_top(px(margin_top))
                .overlay(false)
                .overlay_closable(false)
                // Escape and the close button are both a cancel: they close the picker through the
                // session and take the dialog down with them, while it is still the dialog on top.
                .on_cancel(move |_, window, cx| {
                    cancelling.update(cx, |session, _| session.close_color_picker(false));
                    if !cancel_flag.replace(true) {
                        window.close_dialog(cx);
                    }
                    // Never let the library pop a second time: that would reach the dialog below.
                    false
                })
                .on_close(move |_, window, cx| {
                    closing.update(cx, |session, _| session.close_color_picker(false));
                    if !close_flag.replace(true) {
                        window.close_dialog(cx);
                    }
                })
                .content(move |content, _, _| content.child(sheet.clone()))
        });
    }

    pub fn close(&mut self, window: &mut Window, cx: &mut App) {
        self.title = None;
        self.panel.close(window, cx);
    }

    pub fn is_visible(&self) -> bool {
        self.panel.is_visible() || self.dialog.is_some()
    }

    /// The panel for the editor to draw, in the row the editor's other panels are centred in.
    pub fn render(
        &mut self,
        row_top: f32,
        row_height: f32,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.panel.render(row_top, row_height, window, cx)
    }

    /// Returns keyboard focus to the picker after a click on the canvas sampled a color
    /// (`ColorPickerPanelController.refocus()`). The dialog host needs no such call: the dialog keeps
    /// its own focus trap.
    pub fn refocus(window: &mut Window, cx: &mut App) {
        FloatingPanelController::refocus(Self::IDENTIFIER, window, cx);
    }
}

/// A dialog's color swatch, drawn as the brush's: clicking it opens the app's picker on `color`,
/// which follows the working color as it moves and keeps the one chosen. `close_picker` puts the
/// picker away with the dialog.
#[derive(IntoElement)]
pub struct DialogColorSwatch {
    session: Entity<EditorSession>,
    title: String,
    color: PaletteColor,
    change: Rc<std::cell::RefCell<Box<dyn FnMut(PaletteColor)>>>,
}

impl DialogColorSwatch {
    pub fn new(
        session: Entity<EditorSession>,
        title: impl Into<String>,
        color: PaletteColor,
        change: impl FnMut(PaletteColor) + 'static,
    ) -> Self {
        Self {
            session,
            title: title.into(),
            color,
            change: Rc::new(std::cell::RefCell::new(Box::new(change))),
        }
    }

    /// `closePicker(_:)`: a dialog that disappears while the picker is open closes it, committing.
    pub fn close_picker(session: &Entity<EditorSession>, cx: &mut App) {
        if session.read(cx).picking_for_dialog() {
            session.update(cx, |session, _| session.close_color_picker(true));
        }
    }
}

impl RenderOnce for DialogColorSwatch {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let session = self.session.clone();
        let change = self.change.clone();
        let title = self.title.clone();
        let color = self.color;
        Button::new(ElementId::Name(format!("dialog-color-swatch-{title}").into()))
            .ghost()
            .p(px(0.0))
            .w(px(34.0))
            .h(px(18.0))
            .rounded(px(4.0))
            .bg(palette_rgba(color))
            .border_1()
            .border_color(hsla(0.0, 0.0, 0.0, 1.0))
            .accessibility_label(title.clone())
            .tooltip(title.clone())
            // `shape.inset(by: 1).strokeBorder(.white, lineWidth: 1)`.
            .child(
                div()
                    .absolute()
                    .inset(px(1.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 1.0)),
            )
            .on_click(move |_, _, cx| {
                let change = change.clone();
                session.update(cx, |session, _| {
                    session.open_dialog_color_picker(
                        title.clone(),
                        color,
                        Box::new(move |color| {
                            let mut change = change.borrow_mut();
                            (**change)(color);
                        }),
                    );
                });
            })
    }
}
