//! Levels: the panel that tunes the open layer's tonal range channel by channel
//! (port of `UI/LevelsSheet.swift`).
//!
//! The sheet reads and writes the open edit's settings, so sampling from the canvas and its own
//! controls always agree. The histogram is drawn here from the edit's own bins, with the Swift's
//! display scaling (`LevelsHistogramDisplay.scale(for:)`) and the channel's color; the input and
//! output marks are painted triangles dragged through gpui's drag payload, standing in for the
//! Swift `DragGesture(minimumDistance: 0)`. The five numbers use the port's [`Input`] with the
//! scrub label `NumericScrub` gives it.
//!
//! Escape and Return are the canvas's (see `canvas_view`), as they were in the Swift: this view
//! adds no key handling of its own.

use std::cell::Cell;
use std::rc::Rc;

use compositor_core::layer_adjustment::{LevelRange, LevelsChannel, LevelsSettings};
use compositor_pixels::adjustments::{LevelsAuto, LevelsHistogramDisplay, LevelsSample};
use compositor_session::EditorSession;

use crate::tool_controls::menu_picker;
use crate::tool_header::CONTROL_SIZE;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The sheet's own width (`.frame(width: 440)`).
const WIDTH: f32 = 440.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// The histogram's height (`.frame(height: 150)`).
const HISTOGRAM_HEIGHT: f32 = 150.0;
/// The handle rows' height (`.frame(height: 20)`).
const HANDLES_HEIGHT: f32 = 20.0;
/// The output gradient bar's height (`.frame(height: 14)`).
const OUTPUT_BAR_HEIGHT: f32 = 14.0;
/// A number field's width (`.frame(width: 80)`).
const FIELD_WIDTH: f32 = 80.0;
/// The channel picker's width (`.frame(width: 180)`).
const CHANNEL_WIDTH: f32 = 180.0;
/// A handle's grab frame: `Image(...).frame(width: 22, height: 20)`.
const HANDLE_FRAME: f32 = 22.0;
/// The 12-point `triangle.fill` a handle draws inside that frame.
const HANDLE_MARK: f32 = 12.0;
/// The mark's center line (`.position(x:, y: 9)`).
const HANDLE_Y: f32 = 9.0;
/// A histogram's bin count (`Array(repeating: 0, count: 256)`).
const BINS: usize = 256;

/// The five number fields, in the sheet's order; the name is also the field's label.
const FIELD_NAMES: [&str; 5] = [
    "Input black",
    "Gamma",
    "Input white",
    "Output black",
    "Output white",
];
/// Each field's fraction digits (`field(_:_:decimals:)`).
const FIELD_DECIMALS: [usize; 5] = [0, 2, 0, 0, 0];
/// The histogram's `.help(...)`: display scaling, not clipping.
const HISTOGRAM_HELP: &str = "Linear histogram with automatic vertical scaling. Tall spikes may extend beyond the graph; all tones from 0 to 255 remain included.";

/// The secondary label color of `.foregroundStyle(.secondary)`.
const SECONDARY: Hsla = hsla(0.0, 0.0, 1.0, 0.55);
/// The black mark of a handle row (`Color.black`).
const MARKER_BLACK: Hsla = hsla(0.0, 0.0, 0.0, 1.0);
/// The white mark of a handle row (`Color.white`).
const MARKER_WHITE: Hsla = hsla(0.0, 0.0, 1.0, 1.0);
/// The middle mark, the Gamma handle (`Color.gray`).
const MARKER_GRAY: Hsla = hsla(0.0, 0.0, 0.5, 1.0);

/// The color the histogram fills with, per channel (`switch settings.channel`).
fn histogram_color(channel: LevelsChannel) -> Hsla {
    match channel {
        LevelsChannel::Rgb => hsla(0.0, 0.0, 0.5, 1.0),
        LevelsChannel::Red => hsla(0.0, 1.0, 0.5, 1.0),
        LevelsChannel::Green => hsla(1.0 / 3.0, 1.0, 0.5, 1.0),
        LevelsChannel::Blue => hsla(2.0 / 3.0, 1.0, 0.5, 1.0),
    }
}

/// `let gammaPosition = current.black + (current.white - current.black) * pow(0.5, current.gamma)`.
fn gamma_position(range: LevelRange) -> f64 {
    range.black + (range.white - range.black) * 0.5f64.powf(range.gamma)
}

/// The Swift `field(_:_:decimals:)` range: `0.1...9.99` for Gamma, `0...255` for the rest.
fn field_range(index: usize) -> (f64, f64) {
    if index == 1 {
        (0.1, 9.99)
    } else {
        (0.0, 255.0)
    }
}

/// A field's text (`format: .number.precision(.fractionLength(decimals))`).
fn field_text(value: f64, decimals: usize) -> String {
    if decimals == 0 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.decimals$}")
    }
}

/// The mark a press takes: the closest one whose 22-point frame holds the pointer, later marks
/// winning ties as SwiftUI's stacked `contentShape(Rectangle())` frames did.
fn grabbed_handle(positions: &[f64], view_x: f32, width: f32) -> Option<usize> {
    let mut best = None;
    let mut best_distance = f32::INFINITY;
    for (index, position) in positions.iter().enumerate() {
        let center = (*position / 255.0) as f32 * width;
        let distance = (center - view_x).abs();
        if distance <= HANDLE_FRAME / 2.0 && distance <= best_distance {
            best = Some(index);
            best_distance = distance;
        }
    }
    best
}

/// The value a pointer position on a handle row means, in 0…255
/// (`min(255, max(0, drag.location.x / geometry.size.width * 255))`).
fn handle_value(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f64)> {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return None;
    }
    let view_x = f32::from(position.x) - f32::from(bounds.origin.x);
    Some((view_x, f64::from((view_x / width * 255.0).clamp(0.0, 255.0))))
}

/// `Image(systemName: "triangle.fill").font(.system(size: 12))`, with the Swift
/// `.shadow(color: .gray, radius: 0.5)` drawn as a gray hairline before the fill.
fn paint_handle(window: &mut Window, bounds: Bounds<Pixels>, position: f64, color: Hsla) {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return;
    }
    let x = f32::from(bounds.origin.x) + (position / 255.0) as f32 * width;
    let y = f32::from(bounds.origin.y) + HANDLE_Y;
    let half = HANDLE_MARK / 2.0;
    let height = HANDLE_MARK * 5.0 / 6.0;
    let mut outline = PathBuilder::stroke(px(1.0));
    outline.move_to(point(px(x), px(y - height / 2.0)));
    outline.line_to(point(px(x - half), px(y + height / 2.0)));
    outline.line_to(point(px(x + half), px(y + height / 2.0)));
    outline.close();
    if let Ok(outline) = outline.build() {
        window.paint_path(outline, MARKER_GRAY);
    }
    let mut mark = PathBuilder::fill();
    mark.move_to(point(px(x), px(y - height / 2.0)));
    mark.line_to(point(px(x - half), px(y + height / 2.0)));
    mark.line_to(point(px(x + half), px(y + height / 2.0)));
    mark.close();
    if let Ok(mark) = mark.build() {
        window.paint_path(mark, color);
    }
}

/// One number field's own state: its text box and what the last frame saw.
struct FieldState {
    input: Entity<InputState>,
    was_focused: bool,
    last_text: String,
}

/// The empty view a handle drag carries: dragging a mark moves a number, not a payload.
struct HandleDragView;

/// The payload type of the input and output rows' drags.
struct HandleDrag;

impl Render for HandleDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The Levels sheet.
pub struct LevelsSheet {
    session: Entity<EditorSession>,
    /// The mark the pointer holds in the input row: 0 black, 1 gamma, 2 white (`dragging`).
    input_drag: Option<usize>,
    /// The mark the pointer holds in the output row: 0 black, 1 white.
    output_drag: Option<usize>,
    /// Each row's rectangle, recorded while it paints so a drag reads a position from it.
    input_bounds: Rc<Cell<Bounds<Pixels>>>,
    output_bounds: Rc<Cell<Bounds<Pixels>>>,
    fields: [Option<FieldState>; FIELD_NAMES.len()],
}

impl LevelsSheet {
    /// `LevelsSheet(session:)`.
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            input_drag: None,
            output_drag: None,
            input_bounds: Rc::new(Cell::new(Bounds::default())),
            output_bounds: Rc::new(Cell::new(Bounds::default())),
            fields: Default::default(),
        }
    }

    /// `edit?.settings ?? LevelsSettings()`.
    fn settings(&self, cx: &App) -> LevelsSettings {
        self.session
            .read(cx)
            .levels
            .as_ref()
            .map(|edit| edit.settings)
            .unwrap_or_default()
    }

    /// `edit?.preview ?? true`.
    fn preview(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .levels
            .as_ref()
            .map(|edit| edit.preview)
            .unwrap_or(true)
    }

    /// `edit?.committing == true`.
    fn committing(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .levels
            .as_ref()
            .is_some_and(|edit| edit.committing)
    }

    /// `update(_:)`: writes the settings through, previewing with the edit's own Preview switch.
    fn update(&mut self, change: impl FnOnce(&mut LevelsSettings), cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            let preview = session.levels.as_ref().map(|edit| edit.preview).unwrap_or(true);
            let mut settings = session
                .levels
                .as_ref()
                .map(|edit| edit.settings)
                .unwrap_or_default();
            change(&mut settings);
            session.update_levels(&settings, preview);
            cx.notify();
        });
        cx.notify();
    }

    /// The `Binding`'s `get`: one of the current range's five values.
    fn field_value(&self, index: usize, cx: &App) -> f64 {
        let range = self.settings(cx).current();
        match index {
            0 => range.black,
            1 => range.gamma,
            2 => range.white,
            3 => range.output_black,
            _ => range.output_white,
        }
    }

    /// The `Binding`'s `set`: `var range = $0.current; range[keyPath] = newValue; $0.current = range`.
    fn set_field(&mut self, index: usize, value: f64, cx: &mut Context<Self>) {
        self.update(
            |settings| {
                let mut range = settings.current();
                match index {
                    0 => range.black = value,
                    1 => range.gamma = value,
                    2 => range.white = value,
                    3 => range.output_black = value,
                    _ => range.output_white = value,
                }
                settings.set_current(range);
            },
            cx,
        );
    }

    /// The input row's three marks: black, the gamma position, white.
    fn input_marks(&self, range: LevelRange) -> [(f64, Hsla); 3] {
        [
            (range.black, MARKER_BLACK),
            (gamma_position(range), MARKER_GRAY),
            (range.white, MARKER_WHITE),
        ]
    }

    /// The output row's two marks: black and white.
    fn output_marks(range: LevelRange) -> [(f64, Hsla); 2] {
        [(range.output_black, MARKER_BLACK), (range.output_white, MARKER_WHITE)]
    }

    /// One mark drag's shared arithmetic: which mark the press takes, and where it is dragged to.
    fn begin_drag(&mut self, output: bool, position: Point<Pixels>, cx: &mut Context<Self>) {
        let range = self.settings(cx).current();
        let marks: Vec<f64> = if output {
            Self::output_marks(range).iter().map(|(value, _)| *value).collect()
        } else {
            self.input_marks(range).iter().map(|(value, _)| *value).collect()
        };
        let bounds = if output {
            self.output_bounds.get()
        } else {
            self.input_bounds.get()
        };
        let Some((view_x, _)) = handle_value(bounds, position) else {
            return;
        };
        let Some(index) = grabbed_handle(&marks, view_x, f32::from(bounds.size.width)) else {
            return;
        };
        if output {
            self.output_drag = Some(index);
        } else {
            self.input_drag = Some(index);
        }
        self.drag_to(output, index, position, cx);
    }

    /// `DragGesture.onChanged`, for the mark the press took.
    fn drag_to(&mut self, output: bool, index: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let bounds = if output {
            self.output_bounds.get()
        } else {
            self.input_bounds.get()
        };
        let Some((_, x)) = handle_value(bounds, position) else {
            return;
        };
        self.set_handle(output, index, x, cx);
    }

    /// The drag's own value rule, handle by handle.
    fn set_handle(&mut self, output: bool, index: usize, x: f64, cx: &mut Context<Self>) {
        self.update(
            |settings| {
                let mut range = settings.current();
                if output {
                    if index == 0 {
                        range.output_black = x.round();
                    } else {
                        range.output_white = x.round();
                    }
                } else if index == 0 {
                    range.black = (range.white - 1.0).min(x.round());
                } else if index == 2 {
                    range.white = (range.black + 1.0).max(x.round());
                } else {
                    let fraction = ((x - range.black) / (range.white - range.black))
                        .min(0.999)
                        .max(0.001);
                    range.gamma = fraction.ln() / 0.5f64.ln();
                }
                settings.set_current(range);
            },
            cx,
        );
    }

    /// `onEnded { dragging = nil }`.
    fn end_drag(&mut self, output: bool, cx: &mut Context<Self>) {
        if output {
            self.output_drag = None;
        } else {
            self.input_drag = None;
        }
        cx.notify();
    }

    /// The row's own drag flag, so a drag that started on one row is not continued on the other.
    fn drag_index(&self, output: bool) -> Option<usize> {
        if output {
            self.output_drag
        } else {
            self.input_drag
        }
    }

    /// The number fields' text boxes, made the first time the sheet is drawn, when a window is at
    /// hand.
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..FIELD_NAMES.len() {
            if self.fields[index].is_some() {
                continue;
            }
            let input = window.use_keyed_state(("levels-field", index), cx, |window, cx| {
                InputState::new(window, cx)
            });
            input.update(cx, |state, cx| state.set_text_align(TextAlign::Right, cx));
            self.fields[index] = Some(FieldState {
                input,
                was_focused: false,
                last_text: String::new(),
            });
        }
    }

    /// The fields' own bookkeeping: a typed number is applied as the field is typed in, and the
    /// text follows the value while the field does not have the keyboard.
    fn sync_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..FIELD_NAMES.len() {
            let Some(state) = self.fields[index].as_ref() else {
                continue;
            };
            let field = state.input.clone();
            let was_focused = state.was_focused;
            let typed = field.read(cx).value().to_string();
            let focused = field.read(cx).focus_handle(cx).is_focused(window);
            if focused {
                let last_text = self.fields[index]
                    .as_ref()
                    .map(|state| state.last_text.clone())
                    .unwrap_or_default();
                if typed != last_text {
                    if let Some(state) = self.fields[index].as_mut() {
                        state.last_text = typed.clone();
                    }
                    if let Ok(number) = typed.trim().parse::<f64>() {
                        if number.is_finite() {
                            self.set_field(index, number, cx);
                        }
                    }
                }
            } else {
                let text = field_text(self.field_value(index, cx), FIELD_DECIMALS[index]);
                let stale = self.fields[index]
                    .as_ref()
                    .map(|state| state.last_text != text)
                    .unwrap_or(false);
                if was_focused || stale {
                    // Leaving the field puts its value's own text back.
                    if typed != text {
                        field.update(cx, |field, cx| field.set_value(text.clone(), window, cx));
                    }
                    if let Some(state) = self.fields[index].as_mut() {
                        state.last_text = text;
                    }
                }
            }
            if let Some(state) = self.fields[index].as_mut() {
                state.was_focused = focused;
            }
        }
    }

    /// `histogram`: the channel's bins, scaled by [`LevelsHistogramDisplay::scale`] and filled in
    /// the channel's color, with the loading caption over it until the bins land.
    fn histogram_view(&self, cx: &App) -> Stateful<Div> {
        let session = self.session.read(cx);
        let channel = session
            .levels
            .as_ref()
            .map(|edit| edit.settings.channel)
            .unwrap_or(LevelsChannel::Rgb);
        let bins = session
            .levels
            .as_ref()
            .map(|edit| edit.histogram[channel.index()].clone())
            .unwrap_or_else(|| vec![0.0; BINS]);
        let ready = session
            .levels
            .as_ref()
            .is_some_and(|edit| edit.histogram_ready);
        let peak = LevelsHistogramDisplay::scale(&bins);
        let color = histogram_color(channel);
        div()
            .id("levels-histogram")
            .relative()
            .w_full()
            .h(px(HISTOGRAM_HEIGHT))
            .bg(hsla(0.0, 0.0, 0.0, 0.25))
            // `.accessibilityLabel("Original \(settings.channel.rawValue) histogram")`.
            .aria_label(format!("Original {} histogram", channel.raw_value()))
            .tooltip(|window, cx| Tooltip::new(HISTOGRAM_HELP).build(window, cx))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        if !(peak > 0.0) {
                            return;
                        }
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let left = f32::from(bounds.origin.x);
                        let top = f32::from(bounds.origin.y);
                        let bin_width = width / BINS as f32;
                        for (index, bin) in bins.iter().enumerate().take(BINS) {
                            if !bin.is_finite() {
                                continue;
                            }
                            let bar = height * (bin / peak).min(1.0).max(0.0) as f32;
                            if bar <= 0.0 {
                                continue;
                            }
                            let rect = Bounds {
                                origin: point(px(left + bin_width * index as f32), px(top + height - bar)),
                                size: size(px(bin_width + 0.1), px(bar)),
                            };
                            window.paint_quad(fill(rect, color));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            // `.overlay(alignment: .topLeading) { if edit?.histogramReady != true { ... } }`.
            .when(!ready, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .p(px(8.0))
                        .text_size(px(CONTROL_SIZE))
                        .child("Loading histogram…"),
                )
            })
    }

    /// `handles(output:)`: the two rows of draggable marks under the histogram and the output bar.
    fn handles(&self, output: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let range = self.settings(cx).current();
        let marks: Vec<(f64, Hsla)> = if output {
            Self::output_marks(range).to_vec()
        } else {
            self.input_marks(range).to_vec()
        };
        let cell = if output {
            self.output_bounds.clone()
        } else {
            self.input_bounds.clone()
        };
        div()
            .id(if output {
                "levels-output-handles"
            } else {
                "levels-input-handles"
            })
            .relative()
            .w_full()
            .h(px(HANDLES_HEIGHT))
            .child(
                canvas(
                    move |bounds, _, _| {
                        cell.set(bounds);
                        marks
                    },
                    |bounds, marks: Vec<(f64, Hsla)>, window, _| {
                        for (position, color) in marks {
                            paint_handle(window, bounds, position, color);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.begin_drag(output, event.position, cx);
                }),
            )
            .on_drag(HandleDrag, |_, _, _, cx| cx.new(|_| HandleDragView))
            .on_drag_move::<HandleDrag>(
                cx.listener(move |this, event: &DragMoveEvent<HandleDrag>, _, cx| {
                    let Some(index) = this.drag_index(output) else {
                        return;
                    };
                    this.drag_to(output, index, event.event.position, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.end_drag(output, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.end_drag(output, cx)),
            )
    }

    /// `field(_:_:decimals:)`: the scrubbable name over its number box.
    fn field(&self, index: usize, committing: bool, cx: &mut Context<Self>) -> Div {
        let name = FIELD_NAMES[index];
        let value = self.field_value(index, cx);
        let (lower, upper) = field_range(index);
        let sensitivity = if FIELD_DECIMALS[index] == 0 { 1.0 } else { 0.01 };
        let entity = cx.entity();
        let label = div()
            .text_size(px(CONTROL_SIZE))
            .text_color(SECONDARY)
            .child(name);
        let label: AnyElement = if committing {
            label.into_any_element()
        } else {
            label
                .scrubbable(
                    ("levels-field", index),
                    NumericScrub::new(value, sensitivity, (lower, upper)).on_change(
                        move |value, _, cx| {
                            entity.update(cx, |sheet, cx| sheet.set_field(index, value, cx));
                        },
                    ),
                )
                .into_any_element()
        };
        let input = self.fields[index].as_ref().map(|state| state.input.clone());
        v_flex()
            .items_start()
            .gap(px(5.0))
            .child(label)
            .when_some(input, |this, input| {
                this.child(div().w(px(FIELD_WIDTH)).child(
                    // `.accessibilityIdentifier("levels\(name.replacingOccurrences(of: " ", with: ""))")`.
                    Input::new(&input)
                        .aria_label(name)
                        .accessibility_id(format!("levels{}", name.replace(' ', "")))
                        .disabled(committing),
                ))
            })
    }
}

impl Render for LevelsSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_fields(window, cx);
        self.sync_fields(window, cx);

        let (settings, committing, histogram_ready, sample_mode, preview, footer) = {
            let session = self.session.read(cx);
            let settings = session
                .levels
                .as_ref()
                .map(|edit| edit.settings)
                .unwrap_or_default();
            let committing = session.levels.as_ref().is_some_and(|edit| edit.committing);
            let histogram_ready = session
                .levels
                .as_ref()
                .is_some_and(|edit| edit.histogram_ready);
            let sample_mode = session.levels.as_ref().and_then(|edit| edit.sample_mode);
            let preview = session.levels.as_ref().map(|edit| edit.preview).unwrap_or(true);
            // The histogram is of the underlying pixels while a filter's own layer is open.
            let footer = if session.adjustment_original.is_some() {
                "Underlying pixels · alpha-weighted histogram"
            } else if session.selection().is_none() {
                "Original pixels · alpha-weighted histogram"
            } else {
                "Original pixels · selection and alpha-weighted histogram"
            };
            (settings, committing, histogram_ready, sample_mode, preview, footer)
        };
        let channel = settings.channel;

        let channel_entity = cx.entity();
        let channel_picker = {
            let entity = channel_entity.clone();
            menu_picker(
                "levels-channel",
                LevelsChannel::ALL
                    .into_iter()
                    .map(|channel| (channel, channel.raw_value())),
                channel,
                move |channel, _, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.update(move |settings| settings.channel = channel, cx)
                    });
                },
            )
            .w(px(CHANNEL_WIDTH))
            .disabled(committing)
        };

        let cancel_entity = cx.entity();
        let ok_entity = cx.entity();
        let reset_entity = cx.entity();
        let preview_entity = cx.entity();

        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .items_start()
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().text_size(px(CONTROL_SIZE)).child("Channel"))
                    .child(channel_picker),
            )
            .child(
                v_flex()
                    .w_full()
                    .child(self.histogram_view(cx))
                    .child(self.handles(false, cx)),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .child(self.field(0, committing, cx))
                    .child(self.field(1, committing, cx))
                    .child(self.field(2, committing, cx)),
            )
            .child(
                v_flex()
                    .w_full()
                    .child(
                        // `LinearGradient(colors: [.black, .white], startPoint: .leading, endPoint: .trailing)`.
                        div()
                            .w_full()
                            .h(px(OUTPUT_BAR_HEIGHT))
                            .bg(linear_gradient(
                                90.0,
                                linear_color_stop(hsla(0.0, 0.0, 0.0, 1.0), 0.0),
                                linear_color_stop(hsla(0.0, 0.0, 1.0, 1.0), 1.0),
                            )),
                    )
                    .child(self.handles(true, cx)),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .child(self.field(3, committing, cx))
                    .child(self.field(4, committing, cx)),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().text_size(px(CONTROL_SIZE)).text_color(SECONDARY).child("Sample"))
                    .children(LevelsSample::ALL.into_iter().enumerate().map(|(index, mode)| {
                        let entity = cx.entity();
                        let armed = sample_mode == Some(mode);
                        Button::new(("levels-sample", index))
                            .label(mode.raw_value())
                            .icon(Icon::new(IconName::Pipette))
                            .selected(armed)
                            .disabled(committing)
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        if let Some(edit) = session.levels.as_mut() {
                                            edit.sample_mode =
                                                if edit.sample_mode == Some(mode) { None } else { Some(mode) };
                                        }
                                        session.brush_revision += 1;
                                        cx.notify();
                                    });
                                    cx.notify();
                                });
                            })
                    })),
            )
            .when_some(sample_mode, |this, mode| {
                this.child(
                    div()
                        .text_size(px(CONTROL_SIZE))
                        .text_color(SECONDARY)
                        .child(format!(
                            "Click the original layer to set {}. Click the eyedropper again to stop.",
                            mode.raw_value().to_lowercase()
                        )),
                )
            })
            .child(
                v_flex()
                    .items_start()
                    .gap(px(6.0))
                    .child(div().text_size(px(CONTROL_SIZE)).text_color(SECONDARY).child("Auto"))
                    .child(
                        h_flex()
                            .gap(px(8.0))
                            .children(LevelsAuto::ALL.into_iter().enumerate().map(|(index, mode)| {
                                let entity = cx.entity();
                                Button::new(("levels-auto", index))
                                    .label(mode.raw_value())
                                    .disabled(committing || !histogram_ready)
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |sheet, cx| {
                                            sheet.session.update(cx, |session, cx| {
                                                session.auto_levels(mode);
                                                cx.notify();
                                            });
                                            cx.notify();
                                        });
                                    })
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .child(
                        Switch::new("levels-preview")
                            .checked(preview)
                            .label("Preview")
                            .accessibility_label("Preview")
                            .disabled(committing)
                            .on_change(move |value, _, cx| {
                                let value = *value;
                                preview_entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        let settings = session
                                            .levels
                                            .as_ref()
                                            .map(|edit| edit.settings)
                                            .unwrap_or_default();
                                        session.update_levels(&settings, value);
                                        cx.notify();
                                    });
                                    cx.notify();
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("levels-reset")
                            .label("Reset")
                            .disabled(committing)
                            .on_click(move |_, _, cx| {
                                reset_entity.update(cx, |sheet, cx| {
                                    // `edit?.sampleMode = nil; update { $0 = LevelsSettings() }`.
                                    sheet.session.update(cx, |session, _| {
                                        if let Some(edit) = session.levels.as_mut() {
                                            edit.sample_mode = None;
                                        }
                                    });
                                    sheet.update(|settings| *settings = LevelsSettings::default(), cx);
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .text_size(px(CONTROL_SIZE))
                    .text_color(SECONDARY)
                    .child(footer),
            )
            .child(Separator::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        Button::new("levels-cancel")
                            .label("Cancel")
                            .disabled(committing)
                            .on_click(move |_, _, cx| {
                                cancel_entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        session.cancel_levels();
                                        cx.notify();
                                    });
                                    cx.notify();
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .when(committing, |this| this.child(Spinner::new().small()))
                    .child(
                        Button::new("levels-ok")
                            .label("OK")
                            .primary()
                            .disabled(committing)
                            .on_click(move |_, _, cx| {
                                ok_entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        session.commit_levels();
                                        cx.notify();
                                    });
                                    cx.notify();
                                });
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // The builtin `#[test]`: the file's `use gpui_kit::*;` glob would otherwise shadow it with the
    // toolkit's `test` attribute macro (enabled by the test-support dev-dependency).
    use ::core::prelude::v1::test;
    use super::*;

    #[test]
    fn a_field_writes_its_own_fraction_digits() {
        assert_eq!(field_text(0.0, 0), "0");
        assert_eq!(field_text(12.4, 0), "12");
        assert_eq!(field_text(255.0, 2), "255.00");
        assert_eq!(field_text(1.0, 2), "1.00");
        assert_eq!(field_text(0.5, 2), "0.50");
    }

    #[test]
    fn gamma_sits_where_the_swift_formula_put_it() {
        let range = LevelRange {
            black: 0.0,
            gamma: 1.0,
            white: 255.0,
            ..LevelRange::default()
        };
        assert_eq!(gamma_position(range), 127.5);
        let range = LevelRange {
            black: 0.0,
            gamma: 2.0,
            white: 255.0,
            ..LevelRange::default()
        };
        assert_eq!(gamma_position(range), 63.75);
    }

    #[test]
    fn a_press_takes_the_nearest_mark_it_lands_on() {
        let marks = [0.0, 127.5, 255.0];
        // Inside the 11-point frame of the left mark.
        assert_eq!(grabbed_handle(&marks, 3.0, 255.0), Some(0));
        // The middle mark's frame reaches 116.5…138.5 on a 255-wide row.
        assert_eq!(grabbed_handle(&marks, 130.0, 255.0), Some(1));
        // Between frames: nothing answers, as a press between SwiftUI's frames did.
        assert_eq!(grabbed_handle(&marks, 110.0, 255.0), None);
        // Ties go to the mark drawn on top, the later one.
        assert_eq!(grabbed_handle(&[100.0, 110.0], 105.0, 255.0), Some(1));
    }

    #[test]
    fn the_handle_grab_frame_is_the_swift_one() {
        assert_eq!(field_range(0), (0.0, 255.0));
        assert_eq!(field_range(1), (0.1, 9.99));
        assert_eq!(HANDLE_FRAME / 2.0, 11.0);
    }
}
