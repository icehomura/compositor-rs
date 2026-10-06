//! The tool headers: the option bar each tool shows above the canvas.
//!
//! Every bar is the Swift view of the same name, laid out with the shared chrome in
//! [`crate::tool_header`]: a 42-point bar with 18 points of horizontal padding, a 13-point
//! semibold title and 12-point controls. The number fields the bars are full of share
//! [`NumberField`] here: the Swift `TextField`s wrote their binding on every keystroke and
//! clamped in it, and `ArrowStepper` let Up and Down step a focused field by one step (ten with
//! Shift), so the port's field does both.

pub mod brush;
pub mod camera_raw;
pub mod clone_stamp;
pub mod color_palette;
pub mod crop;
pub mod filter;
pub mod gradient;
pub mod lasso;
pub mod navigation;
pub mod shape;
pub mod type_tool;

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::widgets::numeric_scrub::arrow_step_amount;

/// The height of a rounded number field (the Swift `.roundedBorder` text field at 12 points).
pub const FIELD_HEIGHT: f32 = 20.0;
/// A number field's corner radius.
pub const FIELD_RADIUS: f32 = 4.0;
/// The gap between a field and its unit (`HStack(spacing: 2)`).
pub const UNIT_SPACING: f32 = 2.0;
/// The width of the gap a tool bar leaves between its controls (`HStack(spacing: 12)`).
pub const CONTROL_SPACING: f32 = 12.0;
/// A bar's `HStack(spacing: 5)` or `HStack(spacing: 6)` row around a field.
pub const TIGHT_SPACING: f32 = 5.0;

/// The value written as Swift's `.number.precision(.fractionLength(0...decimals))` writes it: at
/// most `decimals` fraction digits, with trailing zeros — and a trailing point — taken off.
pub fn format_number(value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if text.is_empty() || text == "-" || text == "-0" {
        text = "0".to_string();
    }
    text
}

/// The number a typed string holds, or `None` when it is not one (the `Double(text)` the Swift
/// `onSubmit`/`applyZoom` paths used).
pub fn parse_number(text: &str) -> Option<f64> {
    text.trim()
        .replace('%', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// A value held inside a range, as every Swift field's binding did with `min`/`max`.
pub fn clamped(value: f64, range: (f64, f64)) -> f64 {
    value.clamp(range.0.min(range.1), range.0.max(range.1))
}

/// What a number field's value means: how it is written, the range its binding keeps it in, and
/// what a non-number falls back to (`$0.isFinite ? min(2000, max(1, $0)) : 40`).
#[derive(Clone, Copy)]
pub struct FieldSpec {
    /// The most fraction digits the field shows (Swift's `fractionLength(0...decimals)`).
    pub decimals: usize,
    /// The range typing and stepping are held inside.
    pub range: (f64, f64),
    /// What one arrow press adds (`ArrowStepper`'s step).
    pub step: f64,
    /// What a value that is not a number becomes (the Swift bindings' `:` fallback).
    pub fallback: f64,
    /// A placeholder shown while the field is empty.
    pub placeholder: Option<&'static str>,
    /// The value the field draws as empty (`Leading`'s 0, which shows its "Auto" prompt).
    pub empty_value: Option<f64>,
    /// Whether the field is dimmed and takes no input (`disabled(session.showsBusy)`).
    pub disabled: bool,
    /// What an arrow press starts from, when that is not the field's own value: Leading steps from
    /// the Auto line height (`arrowSteps(value: { session.currentTextStyle.lineHeight }, …)`).
    pub step_from: Option<f64>,
}

impl FieldSpec {
    /// A field over `range` showing at most `decimals` fraction digits, stepping by one, with the
    /// range's near end as the non-number fallback.
    pub fn new(range: (f64, f64), decimals: usize) -> Self {
        Self {
            decimals,
            range,
            step: 1.0,
            fallback: range.0,
            placeholder: None,
            empty_value: None,
            disabled: false,
            step_from: None,
        }
    }

    /// What an arrow press starts from, when that is not the field's own value.
    pub fn step_from(mut self, value: f64) -> Self {
        self.step_from = Some(value);
        self
    }

    /// Dims the field and stops it taking input, as the Swift `.disabled(_:)` did.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// What one arrow press adds.
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// What a value that is not a number becomes.
    pub fn fallback(mut self, value: f64) -> Self {
        self.fallback = value;
        self
    }

    /// A placeholder for an empty field.
    pub fn placeholder(mut self, text: &'static str) -> Self {
        self.placeholder = Some(text);
        self
    }

    /// The value drawn as an empty field.
    pub fn empty_value(mut self, value: f64) -> Self {
        self.empty_value = Some(value);
        self
    }

    /// The value a typed or stepped number becomes: the binding's clamp, or the fallback when it
    /// is not a number at all.
    pub fn committed(&self, value: f64) -> f64 {
        if value.is_finite() {
            clamped(value, self.range)
        } else {
            self.fallback
        }
    }

    /// The text the field shows for `value` — empty at a declared [`empty_value`].
    ///
    /// [`empty_value`]: Self::empty_value
    pub fn text(&self, value: f64) -> String {
        if self.empty_value == Some(value) {
            return String::new();
        }
        format_number(value, self.decimals)
    }
}

/// How a field's edit ended, for the fields that act on it: Return applied it, Escape put it back,
/// and losing focus committed it in place — the Swift `onSubmit`/`onExitCommand`/focus-change pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditEnd {
    Return,
    Escape,
    Blur,
}

/// The text one field is being typed into (`TextField`'s own text), and whether it holds focus.
#[derive(Default)]
pub struct FieldText {
    text: String,
    editing: bool,
    /// The listener watching the field's focus handle, so the edit ends when the keyboard leaves
    /// the field. gpui has no blur handler on an element, so the handle is watched instead; the
    /// listener the last frame registered is kept here, and dropping it unregisters it.
    blur: Option<Subscription>,
}

impl Render for FieldText {
    fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// One number field: the focus handle its keys arrive on and the buffer it is typed into.
pub struct NumberField {
    focus: FocusHandle,
    text: Entity<FieldText>,
}

impl Clone for NumberField {
    fn clone(&self) -> Self {
        Self {
            focus: self.focus.clone(),
            text: self.text.clone(),
        }
    }
}

/// The fields a tool header owns, made the first time each one is drawn: the Swift views kept one
/// `@FocusState`/`@State` per field for as long as the bar was on screen.
#[derive(Default)]
pub struct Fields {
    entries: HashMap<&'static str, NumberField>,
}

impl Fields {
    /// The field named `id`, made on its first frame.
    pub fn get<V: 'static>(&mut self, id: &'static str, cx: &mut Context<V>) -> NumberField {
        self.entries
            .entry(id)
            .or_insert_with(|| NumberField {
                focus: cx.focus_handle(),
                text: cx.new(|_| FieldText::default()),
            })
            .clone()
    }

    /// Whether the field named `id` is being typed into.
    pub fn is_editing(&self, id: &str, cx: &App) -> bool {
        self.entries
            .get(id)
            .is_some_and(|field| field.text.read(cx).editing)
    }
}

/// One free-text field (`TextField("Characters", text: $… )`): the same chrome as [`NumberField`],
/// but what is typed is handed back as it is — the sheets that hold a plain string keep it
/// themselves and decide what it means.
pub struct TextField {
    focus: FocusHandle,
    text: Entity<FieldText>,
}

impl Clone for TextField {
    fn clone(&self) -> Self {
        Self {
            focus: self.focus.clone(),
            text: self.text.clone(),
        }
    }
}

/// The text fields a view owns, made the first time each one is drawn.
#[derive(Default)]
pub struct TextFields {
    entries: HashMap<&'static str, TextField>,
}

impl TextFields {
    /// The field named `id`, made on its first frame.
    pub fn get<V: 'static>(&mut self, id: &'static str, cx: &mut Context<V>) -> TextField {
        self.entries
            .entry(id)
            .or_insert_with(|| TextField {
                focus: cx.focus_handle(),
                text: cx.new(|_| FieldText::default()),
            })
            .clone()
    }
}

impl TextField {
    /// Takes the keyboard, the way the sheet's field did on appear (`onAppear { focused = true }`).
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// The field's element: a rounded box showing `value`, which becomes editable on a click and
    /// reports every keystroke through `on_change` — the way the Swift bindings wrote as they went.
    /// Return and Escape are reported through `on_end` as well, for the sheets that act on them.
    pub fn element<V: 'static>(
        &self,
        id: &'static str,
        value: &str,
        width: f32,
        on_change: impl Fn(String, &mut Window, &mut App) + 'static,
        on_end: impl Fn(EditEnd, &mut Window, &mut App) + 'static,
        cx: &mut Context<V>,
    ) -> Stateful<Div> {
        let view = cx.entity();
        let value = value.to_string();
        let on_change: Rc<dyn Fn(String, &mut Window, &mut App)> = Rc::new(on_change);
        let on_end: Rc<dyn Fn(EditEnd, &mut Window, &mut App)> = Rc::new(on_end);
        let notify = {
            let view = view.clone();
            move |cx: &mut App| view.update(cx, |_, cx| cx.notify())
        };

        let editing = self.text.read(cx).editing;
        let shown = if editing {
            self.text.read(cx).text.clone()
        } else {
            value.clone()
        };

        let keys = {
            let text = self.text.clone();
            let on_change = on_change.clone();
            let on_end = on_end.clone();
            let notify = notify.clone();
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                match event.keystroke.key.as_str() {
                    "enter" | "return" | "escape" => {
                        let end = if event.keystroke.key == "escape" {
                            EditEnd::Escape
                        } else {
                            EditEnd::Return
                        };
                        text.update(cx, |text, _| text.editing = false);
                        on_end(end, window, cx);
                    }
                    "backspace" | "delete" => {
                        text.update(cx, |text, _| {
                            text.text.pop();
                        });
                        let typed = text.read(cx).text.clone();
                        on_change(typed, window, cx);
                    }
                    _ => {
                        let Some(typed) = event.keystroke.key_char.clone() else {
                            return;
                        };
                        if typed.is_empty() || typed.chars().any(char::is_control) {
                            return;
                        }
                        text.update(cx, |text, _| text.text.push_str(&typed));
                        let typed = text.read(cx).text.clone();
                        on_change(typed, window, cx);
                    }
                }
                notify(cx);
                cx.stop_propagation();
            }
        };

        let start_editing = {
            let text = self.text.clone();
            let notify = notify.clone();
            let focus = self.focus.clone();
            let value = value.clone();
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                window.focus(&focus, cx);
                text.update(cx, |text, _| {
                    text.text = value.clone();
                    text.editing = true;
                });
                notify(cx);
            }
        };

        let blur = {
            let text = self.text.clone();
            let notify = notify.clone();
            let on_end = on_end.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                if !text.read(cx).editing {
                    return;
                }
                text.update(cx, |text, _| text.editing = false);
                on_end(EditEnd::Blur, window, cx);
                notify(cx);
            })
        };

        let theme = cx.theme().clone();
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .w(px(width))
            .h(px(FIELD_HEIGHT))
            .px(px(4.0))
            .rounded(px(FIELD_RADIUS))
            .border_1()
            .border_color(theme.tokens.input)
            .text_size(px(crate::tool_header::CONTROL_SIZE))
            .text_color(theme.tokens.foreground)
            .aria_label(id)
            .track_focus(&self.focus)
            .tab_index(0)
            .on_mouse_down(MouseButton::Left, start_editing)
            .on_key_down(keys)
            .child(div().flex_none().child(shown))
            .child(BlurWatcher {
                focus: self.focus.clone(),
                text: self.text.clone(),
                on_blur: blur,
            })
    }
}

/// The listener that ends a field's edit when the keyboard leaves it — the counterpart of the
/// Swift `onExitCommand` and focus binding. gpui has no blur handler on an element, so the field's
/// own focus handle is watched instead. It draws nothing; it sits in the field's tree so that its
/// listener stays registered for as long as the field is on screen.
struct BlurWatcher {
    focus: FocusHandle,
    text: Entity<FieldText>,
    on_blur: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl IntoElement for BlurWatcher {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for BlurWatcher {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { focus, text, on_blur } = self;
        let subscription =
            window.on_focus_out(&focus, cx, move |_: FocusOutEvent, window, cx| {
                on_blur(window, cx);
            });
        text.update(cx, |text, _| text.blur = Some(subscription));
        Empty
    }
}

impl NumberField {
    /// The field's element: a rounded box showing `value`, right-aligned as the Swift fields were,
    /// which becomes editable on a click and reports every value it lands on through `write`.
    pub fn element<V: 'static>(
        &self,
        id: &'static str,
        value: f64,
        spec: FieldSpec,
        width: f32,
        write: impl Fn(f64, &mut Window, &mut App) + 'static,
        cx: &mut Context<V>,
    ) -> Stateful<Div> {
        self.element_with_release(id, value, spec, width, write, |_, _, _| {}, cx)
    }

    /// The same, with a listener told how an edit ended — the Zoom field hands the keyboard back
    /// to the canvas on Return and Escape (`releaseFocus`).
    pub fn element_with_release<V: 'static>(
        &self,
        id: &'static str,
        value: f64,
        spec: FieldSpec,
        width: f32,
        write: impl Fn(f64, &mut Window, &mut App) + 'static,
        on_end: impl Fn(EditEnd, &mut Window, &mut App) + 'static,
        cx: &mut Context<V>,
    ) -> Stateful<Div> {
        let on_end: Rc<dyn Fn(EditEnd, &mut Window, &mut App)> = Rc::new(on_end);
        let view = cx.entity();
        let write: Rc<dyn Fn(f64, &mut Window, &mut App)> = Rc::new(write);
        let notify = {
            let view = view.clone();
            move |cx: &mut App| view.update(cx, |_, cx| cx.notify())
        };

        let buffered = self.text.read(cx).text.clone();
        let editing = self.text.read(cx).editing;
        let shown = if editing {
            buffered.clone()
        } else {
            spec.text(value)
        };

        let commit = {
            let text = self.text.clone();
            let write = write.clone();
            let notify = notify.clone();
            let on_end = on_end.clone();
            move |end: EditEnd, window: &mut Window, cx: &mut App| {
                let typed = text.read(cx).text.clone();
                let next = match spec.empty_value {
                    Some(empty) if typed.trim().is_empty() => empty,
                    _ => spec.committed(parse_number(&typed).unwrap_or(f64::NAN)),
                };
                text.update(cx, |text, _| text.editing = false);
                write(next, window, cx);
                on_end(end, window, cx);
                notify(cx);
            }
        };

        let revert = {
            let text = self.text.clone();
            let notify = notify.clone();
            let on_end = on_end.clone();
            move |window: &mut Window, cx: &mut App| {
                text.update(cx, |text, _| text.editing = false);
                on_end(EditEnd::Escape, window, cx);
                notify(cx);
            }
        };

        let keys = {
            let text = self.text.clone();
            let write = write.clone();
            let notify = notify.clone();
            let commit = commit.clone();
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| match event.keystroke.key.as_str() {
                "up" | "down" => {
                    // `ArrowStepper.listen`: one step, or ten with Shift; the field's own binding
                    // keeps the result in range.
                    let amount = arrow_step_amount(spec.step, event.keystroke.modifiers);
                    let up = event.keystroke.key == "up";
                    let from = spec.step_from.unwrap_or(value);
                    let next = spec.committed(from + if up { amount } else { -amount });
                    text.update(cx, |text, _| text.text = spec.text(next));
                    write(next, window, cx);
                    notify(cx);
                    cx.stop_propagation();
                }
                "enter" | "return" => {
                    commit(EditEnd::Return, window, cx);
                    cx.stop_propagation();
                }
                "escape" => {
                    revert(window, cx);
                    cx.stop_propagation();
                }
                "backspace" | "delete" => {
                    text.update(cx, |text, _| {
                        text.text.pop();
                    });
                    notify(cx);
                    cx.stop_propagation();
                }
                _ => {
                    let Some(typed) = event.keystroke.key_char.clone() else {
                        return;
                    };
                    if typed.is_empty() || !typed.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
                        return;
                    }
                    text.update(cx, |text, _| text.text.push_str(&typed));
                    notify(cx);
                    cx.stop_propagation();
                }
            }
        };

        let start_editing = {
            let text = self.text.clone();
            let notify = notify.clone();
            let focus = self.focus.clone();
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                window.focus(&focus, cx);
                text.update(cx, |text, _| {
                    text.text = spec.text(value);
                    text.editing = true;
                });
                notify(cx);
            }
        };

        // Losing focus commits what is typed in place (`EditEnd::Blur`).
        let blur = {
            let text = self.text.clone();
            let notify = notify.clone();
            let commit = commit.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                if !text.read(cx).editing {
                    return;
                }
                commit(EditEnd::Blur, window, cx);
                if text.read(cx).editing {
                    text.update(cx, |text, _| text.editing = false);
                    notify(cx);
                }
            })
        };

        let theme = cx.theme().clone();
        let caret_visible = editing;
        let mut field = div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .w(px(width))
            .h(px(FIELD_HEIGHT))
            .px(px(4.0))
            .rounded(px(FIELD_RADIUS))
            .border_1()
            .border_color(theme.tokens.input)
            .text_size(px(crate::tool_header::CONTROL_SIZE))
            .text_color(if spec.disabled {
                theme.tokens.muted_foreground
            } else {
                theme.tokens.foreground
            })
            .aria_label(id)
            .aria_numeric_value(value)
            .when(spec.disabled, |this| this.opacity(0.5))
            .when(!spec.disabled, |this| {
                this.track_focus(&self.focus)
                    .tab_index(0)
                    .on_mouse_down(MouseButton::Left, start_editing)
                    .on_key_down(keys)
            })
            .child(div().flex_none().child(shown));
        if caret_visible {
            field = field.child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(12.0))
                    .bg(theme.tokens.foreground),
            );
        }
        field.child(BlurWatcher {
            focus: self.focus.clone(),
            text: self.text.clone(),
            on_blur: blur,
        })
    }
}

/// The Swift `unitSuffix(_:)`: the field and its unit 2 points apart, so they read as one value.
pub fn unit_suffix(field: impl IntoElement, unit: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(UNIT_SPACING))
        .child(field)
        .child(unit)
}

/// A bar's segmented `Picker`: one selected segment per choice, in the given order.
pub fn segmented_picker<V: PartialEq + Clone + 'static>(
    id: &'static str,
    choices: impl IntoIterator<Item = (V, &'static str)>,
    selected: V,
    on_select: impl Fn(V, &mut Window, &mut App) + 'static,
) -> SegmentedPicker<V> {
    SegmentedPicker {
        id,
        choices: choices.into_iter().collect(),
        selected,
        on_select: Rc::new(on_select),
        disabled: false,
    }
}

/// The element [`segmented_picker`] builds; [`disabled`](Self::disabled) dims it and stops its
/// segments answering, as the Swift `.disabled(_:)` did.
pub struct SegmentedPicker<V> {
    id: &'static str,
    choices: Vec<(V, &'static str)>,
    selected: V,
    on_select: Rc<dyn Fn(V, &mut Window, &mut App)>,
    disabled: bool,
}

impl<V: PartialEq + Clone + 'static> SegmentedPicker<V> {
    /// Dims the row and stops it answering clicks.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl<V: PartialEq + Clone + 'static> IntoElement for SegmentedPicker<V> {
    type Element = gpui_kit::ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        gpui_kit::ViewElement::new(self)
    }
}

impl<V: PartialEq + Clone + 'static> RenderOnce for SegmentedPicker<V> {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            id,
            choices,
            selected,
            on_select,
            disabled,
        } = self;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(1.0))
            .flex_none()
            .when(disabled, |this| this.opacity(0.5))
            .children(choices.into_iter().map(move |(value, label)| {
                let on_select = on_select.clone();
                let is_selected = value == selected;
                Button::new((ElementId::from(id), label))
                    .label(label)
                    .selected(is_selected)
                    .disabled(disabled)
                    .h(px(FIELD_HEIGHT))
                    .px(px(6.0))
                    .rounded(px(FIELD_RADIUS))
                    .text_size(px(crate::tool_header::CONTROL_SIZE))
                    .on_click(move |_, window, cx| on_select(value.clone(), window, cx))
            }))
            .child(Empty)
    }
}

/// A bar's menu-style `Picker` (the Swift `Picker` without `.pickerStyle(.segmented)`): the current
/// choice is the button's label, and the menu holds one item per choice.
pub fn menu_picker<V: PartialEq + Clone + 'static>(
    id: &'static str,
    choices: impl IntoIterator<Item = (V, &'static str)>,
    selected: V,
    on_select: impl Fn(V, &mut Window, &mut App) + 'static,
) -> DropdownButton {
    let choices: Vec<(V, &'static str)> = choices.into_iter().collect();
    let current = choices
        .iter()
        .find(|(value, _)| *value == selected)
        .map(|(_, label)| *label)
        .unwrap_or_default();
    let items = choices.clone();
    let on_select = Rc::new(on_select);
    let selected = selected.clone();
    DropdownButton::new(id)
        .button(
            Button::new((ElementId::from(id), "picker"))
                .label(current)
                .h(px(FIELD_HEIGHT))
                .text_size(px(crate::tool_header::CONTROL_SIZE)),
        )
        .dropdown_menu(move |menu, _, _| {
            items
                .iter()
                .fold(menu, |menu, (value, label)| {
                    let on_select = on_select.clone();
                    let value = value.clone();
                    let checked = value == selected;
                    menu.item(
                        PopupMenuItem::new(*label)
                            .checked(checked)
                            .on_click(move |_, window, cx| on_select(value.clone(), window, cx)),
                    )
                })
        })
}

#[cfg(test)]
mod tests {
    // The parent's `gpui_kit::*` glob brings in an attribute macro named `test` (the dev-dependency's
    // test support), which would shadow the built-in `#[test]`; import the names under test instead.
    use super::{format_number, parse_number, FieldSpec};

    #[test]
    fn a_field_shows_at_most_its_fraction_digits() {
        assert_eq!(format_number(40.0, 0), "40");
        assert_eq!(format_number(0.5, 1), "0.5");
        assert_eq!(format_number(5.0, 1), "5");
        assert_eq!(format_number(-12.25, 2), "-12.25");
        assert_eq!(format_number(0.0, 2), "0");
        assert_eq!(format_number(12.0, 2), "12");
    }

    #[test]
    fn a_typed_number_is_read_or_refused() {
        assert_eq!(parse_number(" 12 "), Some(12.0));
        assert_eq!(parse_number("12%"), Some(12.0));
        assert_eq!(parse_number("1.5"), Some(1.5));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("nope"), None);
    }

    #[test]
    fn a_non_number_falls_back_the_way_the_swift_binding_did() {
        let size = FieldSpec::new((1.0, 2000.0), 0).fallback(40.0);
        assert_eq!(size.committed(f64::NAN), 40.0);
        assert_eq!(size.committed(5000.0), 2000.0);
        assert_eq!(size.committed(0.0), 1.0);
        assert_eq!(size.text(40.0), "40");
    }

    #[test]
    fn an_empty_value_draws_nothing_and_reads_back_empty() {
        let leading = FieldSpec::new((0.0, 5000.0), 0).empty_value(0.0).placeholder("Auto");
        assert_eq!(leading.text(0.0), "");
        assert_eq!(leading.text(24.0), "24");
        assert_eq!(leading.committed(0.0), 0.0);
    }
}
