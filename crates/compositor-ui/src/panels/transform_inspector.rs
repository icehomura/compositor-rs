//! The Move tool's options bar: the transform fields, the aspect lock and the sampling menu.
//!
//! Ported from `UI/TransformInspector.swift`. The AppKit text field and its scrub label are the
//! port's [`Input`] plus [`NumericScrub`]; the value rules — a typed, stepped or dragged value shows
//! on the canvas at once and is applied, one undo step, when the field is done with it — are the
//! Swift ones.
//!
//! `HeldModifiers` is not ported as a singleton: the keys held right now are read from the window
//! (`Window::modifiers()`), and the bar repaints on `on_modifiers_changed`. One documented
//! approximation: the Swift ignored modifiers held while a text field was focused
//! (`NSApp.keyWindow?.firstResponder is NSText`), which gpui does not expose to a sibling view, so
//! the port reads the window's modifiers unconditionally.

use compositor_core::geom::{Point, Size};
use compositor_core::layer_transform::{LayerSampling, LayerTransform};
use compositor_session::EditorSession;

use crate::tool_header::{tool_header_bar, TOOL_HEADER_PADDING};
use crate::widgets::numeric_scrub::{arrow_stepped, NumericScrub, Scrubbable as _};

use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::h_flex;
use gpui_kit::*;

/// The width of an X, Y, W or H field (`.frame(width: 85)`).
const FIELD_WIDTH: f32 = 85.0;
/// The Scale field's own width (`.frame(width: 110)`).
const SCALE_WIDTH: f32 = 110.0;
/// The rotation field's width (`.frame(width: 75)`).
const ROTATION_WIDTH: f32 = 75.0;
/// The sampling menu's width (`.frame(width: 170)`).
const SAMPLING_WIDTH: f32 = 170.0;

/// Which transform value a field edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    X,
    Y,
    W,
    H,
    Scale,
    Rotation,
}

/// `TransformInspector.valueFieldOrder`: every field the bar shows.
const FIELDS: [Field; 6] = [Field::X, Field::Y, Field::W, Field::H, Field::Scale, Field::Rotation];

impl Field {
    /// The field's label: the Swift `TransformValueField(label:)`.
    fn label(self) -> &'static str {
        match self {
            Field::X => "X",
            Field::Y => "Y",
            Field::W => "W",
            Field::H => "H",
            Field::Scale => "Scale",
            Field::Rotation => "°",
        }
    }

    fn suffix(self) -> Option<&'static str> {
        match self {
            Field::Scale => Some("%"),
            _ => None,
        }
    }

    /// `range`: -30,000…30,000 unless the field narrows it.
    fn range(self) -> (f64, f64) {
        match self {
            Field::W | Field::H => (1.0, 30_000.0),
            Field::Scale => (0.1, 30_000.0),
            Field::Rotation => (-360.0, 360.0),
            Field::X | Field::Y => (-30_000.0, 30_000.0),
        }
    }

    fn width(self) -> f32 {
        match self {
            Field::Scale => SCALE_WIDTH,
            Field::Rotation => ROTATION_WIDTH,
            _ => FIELD_WIDTH,
        }
    }
}

/// One field's own state: its text box and what the last frame saw.
struct FieldState {
    input: Entity<InputState>,
    last_text: String,
    was_focused: bool,
}

/// The Move tool's transform bar.
pub struct TransformInspector {
    session: Entity<EditorSession>,
    fields: [Option<FieldState>; 6],
}

impl TransformInspector {
    pub fn new(session: Entity<EditorSession>, _cx: &mut Context<Self>) -> Self {
        Self {
            session,
            fields: Default::default(),
        }
    }

    /// `value`: the edit's own draft, else the active layer's transform.
    fn value(&self, cx: &App) -> LayerTransform {
        let session = self.session.read(cx);
        if let Some(edit) = session.transform_edit.as_ref() {
            return edit.draft;
        }
        if let Some(layer) = session.active_layer() {
            return session.edited_transform(layer);
        }
        LayerTransform {
            origin: Point::ZERO,
            size: Size::new(1.0, 1.0),
            ..LayerTransform::default()
        }
    }

    /// `pixelSize`: the layer's pixels, so 100% scale is its own size.
    fn pixel_size(&self, cx: &App) -> Size {
        let session = self.session.read(cx);
        session
            .transform_pixel_size()
            .or_else(|| session.active_layer().map(|layer| layer.transform.size))
            .unwrap_or_else(|| self.value(cx).size)
    }

    /// The value one field shows.
    fn field_value(&self, field: Field, cx: &App) -> f64 {
        let value = self.value(cx);
        match field {
            Field::X => value.origin.x,
            Field::Y => value.origin.y,
            Field::W => value.size.width,
            Field::H => value.size.height,
            Field::Scale => value.scale_percent(self.pixel_size(cx)),
            Field::Rotation => value.rotation,
        }
    }

    /// `change(_:)`: a value typed, stepped or dragged shows on the canvas as it changes, and is
    /// applied without Cancel or Apply — a transform never resamples the layer's pixels — as one
    /// undo step once the field is done with it. An edit already waiting for Apply takes it as part
    /// of that edit.
    fn change(&mut self, update: impl FnOnce(&mut LayerTransform), cx: &mut Context<Self>) {
        self.session.update(cx, |session, _| {
            if session.transform_edit.is_none() {
                session.begin_transform(false);
                if let Some(edit) = session.transform_edit.as_mut() {
                    edit.from_fields = true;
                }
            }
            let Some(mut value) = session.transform_edit.as_ref().map(|edit| edit.draft) else {
                return;
            };
            update(&mut value);
            session.preview_transform(value);
        });
    }

    /// `finish()`: a field done with its value — a drag on its label let go, or the field left.
    fn finish(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |session, _| {
            if session.transform_edit.as_ref().is_some_and(|edit| edit.from_fields) {
                session.commit_transform();
            }
        });
    }

    /// `resize(_:width:)`: W and H, with the aspect lock doubling the change onto the other side.
    fn resize(&mut self, number: f64, width: bool, cx: &mut Context<Self>) {
        let locks = self.session.read(cx).locks_transform_ratio;
        self.change(
            move |value| {
                if number < 1.0 {
                    return;
                }
                if width {
                    if locks && value.size.width != 0.0 {
                        value.size.height *= number / value.size.width;
                    }
                    value.size.width = number;
                } else {
                    if locks && value.size.height != 0.0 {
                        value.size.width *= number / value.size.height;
                    }
                    value.size.height = number;
                }
            },
            cx,
        );
    }

    /// What a number typed, stepped or scrubbed into one field means.
    fn apply_number(&mut self, field: Field, number: f64, cx: &mut Context<Self>) {
        match field {
            Field::W => self.resize(number, true, cx),
            Field::H => self.resize(number, false, cx),
            Field::Scale => {
                if number > 0.0 {
                    let pixel_size = self.pixel_size(cx);
                    self.change(move |transform| *transform = transform.scaled(number, pixel_size), cx);
                }
            }
            // `$0.rotation = $1.truncatingRemainder(dividingBy: 360)`.
            Field::Rotation => self.change(move |transform| transform.rotation = number % 360.0, cx),
            Field::X => self.change(move |transform| transform.origin.x = number, cx),
            Field::Y => self.change(move |transform| transform.origin.y = number, cx),
        }
    }

    /// `TransformValueField.formatted(_:)`: no trailing zeros on a whole number, two decimals
    /// otherwise.
    fn formatted(value: f64) -> String {
        if (value - value.round()).abs() < 0.005 {
            format!("{}", value.round() as i64)
        } else {
            format!("{value:.2}")
        }
    }

    /// `step(_:)` from the label's scrub and the arrow keys: the value is applied and the text
    /// written, since a step is not typing.
    fn step(&mut self, field: Field, value: f64, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_number(field, value, cx);
        self.write_text(field, window, cx);
    }

    /// Writes one field's own value into its text box (`sync()`).
    fn write_text(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let text = Self::formatted(self.field_value(field, cx));
        if let Some(state) = self.fields[field as usize].as_mut() {
            if state.input.read(cx).value().as_ref() != text {
                state
                    .input
                    .update(cx, |input, cx| input.set_value(text.clone(), window, cx));
            }
            state.last_text = text;
        }
    }

    /// The field's own one-frame bookkeeping: live typing while it has the keyboard, the value
    /// applied when it is left, and the text following the value while it does not.
    fn update_field(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.fields[field as usize].as_ref() else {
            return;
        };
        let input = state.input.clone();
        let last_text = state.last_text.clone();
        let was_focused = state.was_focused;
        let value = self.field_value(field, cx);
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        let text = input.read(cx).value().to_string();
        let mut next_text = last_text.clone();
        if focused {
            // `.onChange(of: text) { if focused, let number = Double(text) { change(number) } }`.
            if text != last_text {
                next_text = text.clone();
                if let Ok(number) = text.trim().parse::<f64>() {
                    if number.is_finite() {
                        self.apply_number(field, number, cx);
                    }
                }
            }
        } else {
            // `.onChange(of: focused) { if !focused { finish(); sync() } }`.
            if was_focused {
                self.finish(cx);
            }
            let formatted = Self::formatted(value);
            if formatted != text {
                input.update(cx, |input, cx| input.set_value(formatted.clone(), window, cx));
            }
            next_text = formatted;
        }
        if let Some(state) = self.fields[field as usize].as_mut() {
            state.last_text = next_text;
            state.was_focused = focused;
        }
    }

    /// `(!session.canTransform && session.transformEdit == nil) || session.transformEdit?.corners != nil`.
    fn fields_enabled(&self, cx: &App) -> bool {
        let session = self.session.read(cx);
        (session.can_transform() || session.transform_edit.is_some())
            && !session
                .transform_edit
                .as_ref()
                .is_some_and(|edit| edit.corners.is_some())
    }

    /// One field row: the scrub label, the text box and any suffix.
    fn value_field(&self, field: Field, cx: &mut Context<Self>) -> impl IntoElement {
        let label_entity = cx.entity();
        let end_entity = cx.entity();
        let key_entity = cx.entity();
        let input = self.fields[field as usize].as_ref().map(|state| state.input.clone());
        let value = self.field_value(field, cx);
        let enabled = self.fields_enabled(cx);

        let label = div()
            .text_size(px(12.0))
            .text_color(hsla(0.0, 0.0, 1.0, 0.55))
            .child(field.label())
            .scrubbable(
                format!("transform-{}", field.label()),
                NumericScrub::new(value, 1.0, field.range())
                    .on_change(move |value, window, cx| {
                        label_entity.update(cx, |inspector, cx| inspector.step(field, value, window, cx));
                    })
                    .on_end(move |_, cx| end_entity.update(cx, |inspector, cx| inspector.finish(cx))),
            );

        let mut row = h_flex().items_center().gap(px(4.0)).w(px(field.width()));
        if let Some(input) = input {
            // `arrowSteps(editing:stepper:value:change:)`: Up and Down step while the field is
            // focused; Return or Tab leaves it and hands the keyboard back to the canvas.
            let input_for_key = input.clone();
            row = row.child(label).child(
                div()
                    .id(ElementId::Name(
                        format!("transform-field-{}", field.label()).into(),
                    ))
                    .flex_1()
                    .capture_key_down(move |event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "enter" | "tab" => {
                                key_entity.update(cx, |inspector, cx| {
                                    inspector.finish(cx);
                                    inspector
                                        .session
                                        .update(cx, |session, _| session.canvas_focus_request += 1);
                                });
                                window.blur(cx);
                                cx.stop_propagation();
                            }
                            "up" | "down" => {
                                if !key_entity.read(cx).fields_enabled(cx) {
                                    return;
                                }
                                let value = arrow_stepped(
                                    key_entity.read(cx).field_value(field, cx),
                                    1.0,
                                    event.keystroke.key == "up",
                                    event.keystroke.modifiers,
                                );
                                key_entity.update(cx, |inspector, cx| inspector.step(field, value, window, cx));
                                cx.stop_propagation();
                            }
                            _ => {}
                        }
                    })
                    .aria_label(field.label())
                    // `.accessibilityIdentifier("transform\(label)")`.
                    .child(Input::new(&input_for_key).aria_label(field.label())),
            );
        }
        if let Some(suffix) = field.suffix() {
            row = row.child(
                div()
                    .text_size(px(12.0))
                    .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                    .child(suffix),
            );
        }
        row.when(!enabled, |this| this.opacity(0.5))
    }
}

impl Render for TransformInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The fields' own state is made the first time the bar is drawn, when a window is at hand.
        for field in FIELDS {
            if self.fields[field as usize].is_none() {
                let input = window.use_keyed_state(
                    ElementId::Name(format!("transform-field-{}", field.label()).into()),
                    cx,
                    |window, cx| InputState::new(window, cx),
                );
                self.fields[field as usize] = Some(FieldState {
                    input,
                    last_text: String::new(),
                    was_focused: false,
                });
            }
            self.update_field(field, window, cx);
        }

        let (targets_mask, auto_select, shows_controls, locks_ratio, sampling, pending, has_edit) = {
            let session = self.session.read(cx);
            (
                session.transform_targets_mask(),
                session.transform_auto_select,
                session.shows_transform_controls,
                session.locks_transform_ratio,
                self.value(cx).sampling,
                session.transform_edit.as_ref().is_some_and(|edit| edit.persistent),
                session.transform_edit.is_some(),
            )
        };
        // `HeldModifiers`: Command flips Auto Select while it is held, and the box shows it flipped;
        // Shift does the same for the aspect lock.
        let held = window.modifiers();
        let auto_select_shown = auto_select != held.platform;
        let locks_shown = locks_ratio != held.shift;
        let title = if targets_mask { "Transform Mask" } else { "Transform" };

        let auto_select_entity = cx.entity();
        let show_controls_entity = cx.entity();
        let lock_entity = cx.entity();
        let sampling_entity = cx.entity();
        let flip_h_entity = cx.entity();
        let flip_v_entity = cx.entity();
        let cancel_entity = cx.entity();
        let apply_entity = cx.entity();

        tool_header_bar(12.0)
            .pr(px(TOOL_HEADER_PADDING))
            .child(
                div()
                    .pl(px(TOOL_HEADER_PADDING))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                Switch::new("transform-auto-select")
                    .checked(auto_select_shown)
                    .label("Auto Select")
                    .accessibility_label("Auto Select")
                    .tooltip("Select layers by clicking the canvas. Hold Command to turn it the other way while you click.")
                    .on_change(move |value, window, cx| {
                        // The box shows the flipped value, so the stored one cancels the flip out.
                        let value = *value;
                        let held = window.modifiers().platform;
                        auto_select_entity.update(cx, |inspector, cx| {
                            inspector.session.update(cx, |session, _| {
                                session.transform_auto_select = value != held;
                            });
                            cx.notify();
                        });
                    }),
            )
            .child(
                Switch::new("transform-show-controls")
                    .checked(shows_controls)
                    .label("Show Controls")
                    .accessibility_label("Show Controls")
                    .tooltip("Show the transform box and handles (⌘H). When hidden, drag anywhere to move the layer.")
                    .on_change(move |value, _, cx| {
                        let value = *value;
                        show_controls_entity.update(cx, |inspector, cx| {
                            inspector
                                .session
                                .update(cx, |session, _| session.shows_transform_controls = value);
                            cx.notify();
                        });
                    }),
            )
            .child(
                div()
                    .id(ElementId::Name("transform-fields".into()))
                    .flex_1()
                    .min_w(px(0.0))
                    // `ScrollView(.horizontal)` with the indicators hidden.
                    .overflow_x_scroll()
                    .child(
                        h_flex()
                            .gap(px(12.0))
                            .px(px(TOOL_HEADER_PADDING))
                            .child(self.value_field(Field::X, cx))
                            .child(self.value_field(Field::Y, cx))
                            .child(self.value_field(Field::W, cx))
                            .child(self.value_field(Field::H, cx))
                            .child(
                                // Shift flips the lock while dragging a handle, and the button
                                // shows it flipped.
                                Button::new("transform-lock-ratio")
                                    .icon(Icon::new(IconName::Link))
                                    .selected(locks_shown)
                                    .tooltip("Lock aspect ratio. Hold Shift while dragging a handle to turn it the other way.")
                                    .accessibility_label("Lock aspect ratio")
                                    .on_click(move |event: &ClickEvent, window, cx| {
                                        let shift = event.modifiers().shift || window.modifiers().shift;
                                        lock_entity.update(cx, |inspector, cx| {
                                            inspector.session.update(cx, |session, _| {
                                                session.locks_transform_ratio = !locks_shown != shift;
                                            });
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(self.value_field(Field::Scale, cx))
                            .child(self.value_field(Field::Rotation, cx))
                            .child(
                                Button::new("transform-sampling")
                                    .label(sampling.raw_value())
                                    .icon(Icon::new(IconName::ChevronDown).size(px(9.0)))
                                    .w(px(SAMPLING_WIDTH))
                                    .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
                                        LayerSampling::ALL.into_iter().fold(menu, |menu, option| {
                                            let entity = sampling_entity.clone();
                                            menu.item(
                                                PopupMenuItem::new(option.raw_value())
                                                    .checked(option == sampling)
                                                    .on_click(move |_, _, cx| {
                                                        entity.update(cx, |inspector, cx| {
                                                            inspector.change(
                                                                move |transform| transform.sampling = option,
                                                                cx,
                                                            )
                                                        });
                                                    }),
                                            )
                                        })
                                    }),
                            )
                            .child(
                                Button::new("transform-flip-h")
                                    .label("Flip H")
                                    .on_click(move |_, _, cx| {
                                        flip_h_entity.update(cx, |inspector, cx| {
                                            inspector.change(
                                                |transform| transform.flip_x = !transform.flip_x,
                                                cx,
                                            )
                                        });
                                    }),
                            )
                            .child(
                                Button::new("transform-flip-v")
                                    .label("Flip V")
                                    .on_click(move |_, _, cx| {
                                        flip_v_entity.update(cx, |inspector, cx| {
                                            inspector.change(
                                                |transform| transform.flip_y = !transform.flip_y,
                                                cx,
                                            )
                                        });
                                    }),
                            ),
                    ),
            )
            .child(
                // Only an edit that waits for them — typed values, ⌘T, a distortion — has anything
                // to cancel or apply. Left in place unseen, so Escape and Return still reach a drag
                // in progress.
                h_flex()
                    .gap(px(12.0))
                    .opacity(if pending { 1.0 } else { 0.0 })
                    .child(
                        Button::new("transform-cancel")
                            .label("Cancel")
                            .disabled(!pending || !has_edit)
                            .on_click(move |_, _, cx| {
                                cancel_entity.update(cx, |inspector, cx| {
                                    inspector
                                        .session
                                        .update(cx, |session, _| session.cancel_transform());
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("transform-apply")
                            .label("Apply")
                            .accessibility_label("Apply")
                            .disabled(!pending || !has_edit)
                            .on_click(move |_, _, cx| {
                                apply_entity.update(cx, |inspector, cx| {
                                    inspector
                                        .session
                                        .update(cx, |session, _| session.commit_transform());
                                    cx.notify();
                                });
                            }),
                    ),
            )
    }
}
