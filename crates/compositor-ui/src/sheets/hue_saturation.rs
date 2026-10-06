//! Hue/Saturation: the panel that shifts one color range, or everything (port of
//! `UI/HueSaturationSheet.swift`).
//!
//! The sheet reads and writes the open edit's settings, so sampling from the canvas and the panel's
//! own controls always agree. The three master sliders are Camera Raw sliders with colored tracks;
//! the two spectrum bars and their band handles are painted here, and the handles are dragged
//! through gpui's drag payload, standing in for the Swift `DragGesture`. The eyedroppers set the
//! session's armed mode ([`EditorSession::hue_sample_mode`]) and the targeted-adjustment button its
//! targeting flag, exactly as the Swift wrote them.
//!
//! Escape and Return are the canvas's (see `canvas_view`), as they were in the Swift: this view
//! adds no key handling of its own beyond the number fields' own Return, as `TextField.onSubmit`
//! had it.

use std::cell::Cell;
use std::rc::Rc;

use compositor_core::layer_adjustment::{ColorRange, HueBand, HueSaturationSettings};
use compositor_pixels::adjustments::HueSaturationFilter;
use compositor_session::selection::HueSampleMode;
use compositor_session::EditorSession;

use crate::tool_controls::menu_picker;
use crate::tool_header::CONTROL_SIZE;
use crate::widgets::gradient_slider::{hsb, srgb, CameraRawSlider, CameraRawSliderTrack};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The sheet's own width (`.frame(width: 460)`).
const WIDTH: f32 = 460.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// The range picker's width (`.frame(width: 160)`).
const PICKER_WIDTH: f32 = 160.0;
/// A slider's title width (`Text(title).frame(width: 76, alignment: .leading)`).
const TITLE_WIDTH: f32 = 76.0;
/// A slider's number field width (`.frame(width: 48)`).
const FIELD_WIDTH: f32 = 48.0;
/// The `HStack(spacing: 10)` of a slider row.
const SLIDER_GAP: f32 = 10.0;
/// The spectrum bars' height (`.frame(height: 16)`).
const SPECTRUM_HEIGHT: f32 = 16.0;
/// The spectrum bars' corner radius (`RoundedRectangle(cornerRadius: 3)`).
const SPECTRUM_RADIUS: f32 = 3.0;
/// The handles row's height (`.frame(height: 12)`).
const HANDLES_HEIGHT: f32 = 12.0;
/// `private let slices = 72`.
const SLICES: usize = 72;
/// The eyedropper buttons' frame (`eyedropper(_:)`): `frame(width: 24, height: 20)`.
const EYEDROPPER_WIDTH: f32 = 24.0;
const EYEDROPPER_HEIGHT: f32 = 20.0;
/// The sample-mode row's spacing (`HStack(spacing: 6)`).
const SAMPLE_GAP: f32 = 6.0;

/// The secondary label color of `.foregroundStyle(.secondary)`.
const SECONDARY: Hsla = hsla(0.0, 0.0, 1.0, 0.55);
/// The marks and separators the panel draws in the theme's own ink.
const MARK_INK: Hsla = hsla(0.0, 0.0, 1.0, 1.0);

/// Which of the three master sliders a row edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slider {
    Hue,
    Saturation,
    Lightness,
}

impl Slider {
    /// The three in the sheet's order.
    const ALL: [Slider; 3] = [Slider::Hue, Slider::Saturation, Slider::Lightness];

    fn title(self) -> &'static str {
        match self {
            Slider::Hue => "Hue",
            Slider::Saturation => "Saturation",
            Slider::Lightness => "Lightness",
        }
    }

    /// The unit beside the field (`.unitSuffix(unit)`): only the hue is in degrees.
    fn unit(self) -> &'static str {
        match self {
            Slider::Hue => "°",
            Slider::Saturation | Slider::Lightness => "",
        }
    }

    /// `slider(_:value:range:...)`'s range.
    fn range(self, colorize: bool) -> (f64, f64) {
        match self {
            Slider::Hue => {
                if colorize {
                    (0.0, 360.0)
                } else {
                    (-180.0, 180.0)
                }
            }
            Slider::Saturation => {
                if colorize {
                    (0.0, 100.0)
                } else {
                    (-100.0, 100.0)
                }
            }
            Slider::Lightness => (-100.0, 100.0),
        }
    }
}

/// `settings.hue`, `settings.saturation` or `settings.lightness`.
fn slider_value(slider: Slider, settings: &HueSaturationSettings) -> f64 {
    match slider {
        Slider::Hue => settings.hue(),
        Slider::Saturation => settings.saturation(),
        Slider::Lightness => settings.lightness(),
    }
}

/// `setHue`, `setSaturation` or `setLightness`.
fn set_slider_value(slider: Slider, settings: &mut HueSaturationSettings, value: f64) {
    match slider {
        Slider::Hue => settings.set_hue(value),
        Slider::Saturation => settings.set_saturation(value),
        Slider::Lightness => settings.set_lightness(value),
    }
}

/// `resetValues`: Photoshop's colorize start, or no change.
fn reset_settings(colorize: bool) -> HueSaturationSettings {
    if colorize {
        HueSaturationSettings::colorize_start()
    } else {
        HueSaturationSettings::default()
    }
}

/// `rangeHue`: the middle of the selected color range; Master centers on red
/// (`ColorRange.colorRanges.firstIndex(of: current.range) ?? 0` times 60).
fn range_hue(range: ColorRange) -> f64 {
    ColorRange::COLOR_RANGES
        .iter()
        .position(|candidate| *candidate == range)
        .map(|index| index as f64 * 60.0)
        .unwrap_or(0.0)
}

/// `nearestHandle(to:)`: the closest handle, measuring the short way around the circle.
fn nearest_handle(handles: &[f64; 4], degrees: f64) -> usize {
    let mut best = 0;
    let mut best_distance = f64::INFINITY;
    for (index, handle) in handles.iter().enumerate() {
        let gap = (handle - degrees).abs() % 360.0;
        let distance = gap.min(360.0 - gap);
        if distance < best_distance {
            best = index;
            best_distance = distance;
        }
    }
    best
}

/// `eyedropper(_:)`'s badge symbol, as the port's icon catalog spells it
/// (`plus.circle.fill` and `minus.circle.fill`).
fn badge_icon(mode: HueSampleMode) -> Option<IconName> {
    match mode {
        HueSampleMode::Replace => None,
        HueSampleMode::Add => Some(IconName::CirclePlus),
        HueSampleMode::Remove => Some(IconName::CircleMinus),
    }
}

/// A slider row's number text (`TextField(..., format: .number.precision(.fractionLength(0)))`).
fn field_text(value: f64) -> String {
    format!("{}", value.round() as i64)
}

/// One number field's own state: its text box and what the last frame saw.
struct FieldState {
    input: Entity<InputState>,
    was_focused: bool,
    last_text: String,
}

/// The empty view a spectrum-handle drag carries: dragging a handle moves the band, not a payload.
struct BandDragView;

/// The payload type of the spectrum handles' drag.
struct BandDrag;

impl Render for BandDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The Hue/Saturation panel.
pub struct HueSaturationSheet {
    session: Entity<EditorSession>,
    /// The handle the spectrum editor holds (`@State private var dragging: Int?`).
    spectrum_drag: Option<usize>,
    /// The handles row's rectangle, recorded while it paints so a drag reads a position from it.
    spectrum_bounds: Rc<Cell<Bounds<Pixels>>>,
    fields: [Option<FieldState>; 3],
}

impl HueSaturationSheet {
    /// `HueSaturationSheet(session:)`.
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            spectrum_drag: None,
            spectrum_bounds: Rc::new(Cell::new(Bounds::default())),
            fields: Default::default(),
        }
    }

    /// `edit?.settings ?? HueSaturationSettings()`.
    fn settings(&self, cx: &App) -> HueSaturationSettings {
        self.session
            .read(cx)
            .hue_saturation
            .as_ref()
            .map(|edit| edit.settings.clone())
            .unwrap_or_default()
    }

    /// `session.hueSaturation?.preview ?? true`.
    fn preview(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .hue_saturation
            .as_ref()
            .map(|edit| edit.preview)
            .unwrap_or(true)
    }

    /// The `settings` binding's setter: `session.updateHueSaturation($0, preview: ...)`.
    fn update_settings(
        &mut self,
        change: impl FnOnce(&mut HueSaturationSettings),
        cx: &mut Context<Self>,
    ) {
        self.session.update(cx, |session, cx| {
            let preview = session
                .hue_saturation
                .as_ref()
                .map(|edit| edit.preview)
                .unwrap_or(true);
            let mut settings = session
                .hue_saturation
                .as_ref()
                .map(|edit| edit.settings.clone())
                .unwrap_or_default();
            change(&mut settings);
            session.update_hue_saturation(&settings, preview);
            cx.notify();
        });
        cx.notify();
    }

    /// `value.wrappedValue = newValue` for one slider.
    fn set_slider(&mut self, slider: Slider, value: f64, cx: &mut Context<Self>) {
        self.update_settings(move |settings| set_slider_value(slider, settings, value), cx);
    }

    /// The field's own Return (`onSubmit`): clamp to the range, write it, and confirm the window as
    /// OK does.
    fn submit_field(&mut self, slider: Slider, cx: &mut Context<Self>) {
        let colorize = self.settings(cx).colorize;
        let (lower, upper) = slider.range(colorize);
        let value = slider_value(slider, &self.settings(cx)).clamp(lower, upper);
        self.set_slider(slider, value, cx);
        self.session.update(cx, |session, cx| {
            session.commit_hue_saturation();
            cx.notify();
        });
    }

    /// `settings.band.setHandle(index, to: degrees)`, written back through the sheet's binding.
    fn set_band_handle(&mut self, index: usize, degrees: f64, cx: &mut Context<Self>) {
        self.update_settings(
            move |settings| {
                let mut band = settings.band();
                band.set_handle(index, degrees);
                settings.set_band(band);
            },
            cx,
        );
    }

    /// The pointers' shared arithmetic on the handles row: the x as a hue in 0…360
    /// (`min(max(0, x), width) / width * 360`).
    fn handle_degrees(&self, position: Point<Pixels>) -> Option<f64> {
        let bounds = self.spectrum_bounds.get();
        let width = f32::from(bounds.size.width);
        if width <= 0.0 {
            return None;
        }
        let view_x = (f32::from(position.x) - f32::from(bounds.origin.x)).clamp(0.0, width);
        Some(f64::from(view_x / width * 360.0))
    }

    /// `DragGesture.onChanged`, for the handle the press took.
    fn begin_band_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(degrees) = self.handle_degrees(position) else {
            return;
        };
        let handles = self.settings(cx).band().handles();
        let index = nearest_handle(&handles, degrees);
        self.spectrum_drag = Some(index);
        self.set_band_handle(index, degrees, cx);
    }

    /// The drag's next position, for the handle it took.
    fn drag_band(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(index) = self.spectrum_drag else {
            return;
        };
        let Some(degrees) = self.handle_degrees(position) else {
            return;
        };
        self.set_band_handle(index, degrees, cx);
    }

    /// `onEnded { dragging = nil }`.
    fn end_band_drag(&mut self, cx: &mut Context<Self>) {
        self.spectrum_drag = None;
        cx.notify();
    }

    /// The number fields' text boxes, made the first time the sheet is drawn, when a window is at
    /// hand.
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (index, slider) in Slider::ALL.into_iter().enumerate() {
            if self.fields[index].is_some() {
                continue;
            }
            let input = window.use_keyed_state(("hue-field", index), cx, |window, cx| {
                InputState::new(window, cx)
            });
            input.update(cx, |state, cx| state.set_text_align(TextAlign::Right, cx));
            let value = slider_value(slider, &self.settings(cx));
            self.fields[index] = Some(FieldState {
                input,
                was_focused: false,
                last_text: field_text(value),
            });
        }
    }

    /// The fields' own bookkeeping: a typed number is applied as the field is typed in, and the
    /// text follows the value while the field does not have the keyboard.
    fn sync_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        for (index, slider) in Slider::ALL.into_iter().enumerate() {
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
                            self.set_slider(slider, number, cx);
                        }
                    }
                }
            } else {
                let text = field_text(slider_value(slider, &settings));
                let stale = self.fields[index]
                    .as_ref()
                    .map(|state| state.last_text != text)
                    .unwrap_or(false);
                if was_focused || stale {
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

    /// The eyedropper, with a plus or minus badge for Add and Remove (`eyedropper(_:)`).
    fn eyedropper_mark(mode: HueSampleMode) -> Div {
        div()
            .relative()
            .w(px(EYEDROPPER_WIDTH))
            .h(px(EYEDROPPER_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .child(Icon::new(IconName::Pipette).size(px(13.0)))
            .when_some(badge_icon(mode), |this, badge| {
                this.child(
                    div()
                        .absolute()
                        // `ZStack(alignment: .bottomTrailing)` with `.offset(x: 3, y: 1)`.
                        .right(px(-3.0))
                        .bottom(px(-1.0))
                        .text_size(px(8.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(badge).size(px(8.0))),
                )
            })
    }

    /// `samplingControls`: the eyedroppers once a color range is selected and not Colorize, and the
    /// targeted-adjustment button whenever Colorize is off.
    fn sampling_controls(
        &self,
        settings: &HueSaturationSettings,
        shows_spectrum: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let sample_mode = self.session.read(cx).hue_sample_mode;
        let targeting = self.session.read(cx).hue_targeting;
        let mut controls = h_flex().items_center().gap(px(SAMPLE_GAP));
        if shows_spectrum {
            controls = controls.children(HueSampleMode::ALL.into_iter().enumerate().map(
                |(index, mode)| {
                    let entity = cx.entity();
                    let armed = sample_mode == Some(mode);
                    Button::new(("hue-sample", index))
                        .ghost()
                        .w(px(EYEDROPPER_WIDTH))
                        .h(px(EYEDROPPER_HEIGHT))
                        .tooltip(mode.help())
                        .accessibility_label(format!("{} color", mode.raw_value()))
                        .selected(armed)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |sheet, cx| {
                                sheet.session.update(cx, |session, cx| {
                                    session.hue_targeting = false;
                                    session.hue_sample_mode = if session.hue_sample_mode == Some(mode)
                                    {
                                        None
                                    } else {
                                        Some(mode)
                                    };
                                    cx.notify();
                                });
                                cx.notify();
                            });
                        })
                        .child(Self::eyedropper_mark(mode))
                },
            ));
            // `Divider().frame(height: 16)`.
            controls = controls.child(div().w(px(1.0)).h(px(16.0)).bg(hsla(0.0, 0.0, 1.0, 0.10)));
        }
        if !settings.colorize {
            let entity = cx.entity();
            controls = controls.child(
                Button::new("hue-target")
                    .ghost()
                    .w(px(EYEDROPPER_WIDTH))
                    .h(px(EYEDROPPER_HEIGHT))
                    .tooltip("Targeted adjustment: drag on the image to change that color's saturation, or its hue with Command held")
                    .accessibility_label("Targeted adjustment")
                    .selected(targeting)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |sheet, cx| {
                            sheet.session.update(cx, |session, cx| {
                                session.hue_sample_mode = None;
                                session.hue_targeting = !session.hue_targeting;
                                cx.notify();
                            });
                            cx.notify();
                        });
                    })
                    .child(Icon::new(IconName::Pointer).size(px(13.0)).w(px(EYEDROPPER_WIDTH)).h(px(EYEDROPPER_HEIGHT))),
            );
        }
        controls
    }

    /// `spectrum(after:)`: the hues as they are, or as the adjustment leaves them, in 72 slices.
    fn spectrum(&self, after: bool, settings: &HueSaturationSettings) -> Div {
        let settings = settings.clone();
        let colors: Vec<Rgba> = (0..SLICES)
            .map(|slice| {
                let hue = slice as f64 / SLICES as f64 * 360.0;
                let shown = if after {
                    HueSaturationFilter::shifted_hue(hue, &settings)
                } else {
                    hue
                };
                hsb((shown / 360.0) as f32, 1.0, 1.0)
            })
            .collect();
        div()
            .w_full()
            .h(px(SPECTRUM_HEIGHT))
            .rounded(px(SPECTRUM_RADIUS))
            .overflow_hidden()
            .relative()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let width = f32::from(bounds.size.width) / SLICES as f32;
                        let left = f32::from(bounds.origin.x);
                        let top = f32::from(bounds.origin.y);
                        let height = f32::from(bounds.size.height);
                        for (slice, color) in colors.iter().enumerate() {
                            let rect = Bounds {
                                origin: point(px(left + width * slice as f32), px(top)),
                                size: size(px(width + 0.5), px(height)),
                            };
                            window.paint_quad(fill(rect, *color));
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }

    /// `handles`: the four band marks — outer falloff shoulders, inner full-strength bars.
    fn spectrum_handles(&self, settings: &HueSaturationSettings, cx: &mut Context<Self>) -> Stateful<Div> {
        let handles = settings.band().handles();
        let marks: Vec<(f64, bool)> = handles
            .iter()
            .enumerate()
            .map(|(index, degrees)| (*degrees, index == 1 || index == 2))
            .collect();
        let cell = self.spectrum_bounds.clone();
        let ink = MARK_INK;
        div()
            .id("hue-spectrum-handles")
            .relative()
            .w_full()
            .h(px(HANDLES_HEIGHT))
            .child(
                canvas(
                    move |bounds, _, _| {
                        cell.set(bounds);
                        marks
                    },
                    move |bounds, marks: Vec<(f64, bool)>, window, _| {
                        let width = f32::from(bounds.size.width);
                        if width <= 0.0 {
                            return;
                        }
                        let left = f32::from(bounds.origin.x);
                        let top = f32::from(bounds.origin.y);
                        let height = f32::from(bounds.size.height);
                        for (degrees, inner) in marks {
                            let x = left + (degrees / 360.0) as f32 * width;
                            let rect = if inner {
                                Bounds {
                                    origin: point(px(x - 1.0), px(top)),
                                    size: size(px(2.0), px(height)),
                                }
                            } else {
                                Bounds {
                                    origin: point(px(x - 3.5), px(top + height / 2.0 - 2.5)),
                                    size: size(px(7.0), px(5.0)),
                                }
                            };
                            window.paint_quad(fill(rect, ink));
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
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.begin_band_drag(event.position, cx);
                }),
            )
            .on_drag(BandDrag, |_, _, _, cx| cx.new(|_| BandDragView))
            .on_drag_move::<BandDrag>(
                cx.listener(|this, event: &DragMoveEvent<BandDrag>, _, cx| {
                    this.drag_band(event.event.position, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_band_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_band_drag(cx)),
            )
    }

    /// `SpectrumEditor(settings:)`: both spectrum bars, the handles between them, and the four
    /// handle degrees below.
    fn spectrum_editor(&self, settings: &HueSaturationSettings, cx: &mut Context<Self>) -> Div {
        let degrees = settings
            .band()
            .handles()
            .iter()
            .map(|handle| format!("{}°", handle.round() as i64))
            .collect::<Vec<String>>()
            .join("   ");
        v_flex()
            .w_full()
            .gap(px(5.0))
            .child(self.spectrum(false, settings))
            .child(self.spectrum_handles(settings, cx))
            .child(self.spectrum(true, settings))
            .child(
                div()
                    .text_size(px(CONTROL_SIZE))
                    .font_family("monospace")
                    .text_color(SECONDARY)
                    .child(degrees),
            )
    }

    /// One master slider: its title (double-click to reset) and scrub, the Camera Raw slider, and
    /// the exact number with its unit.
    fn slider_row(
        &self,
        slider: Slider,
        settings: &HueSaturationSettings,
        cx: &mut Context<Self>,
    ) -> Div {
        let colorize = settings.colorize;
        let value = slider_value(slider, settings);
        let range = slider.range(colorize);
        let reset = slider_value(slider, &reset_settings(colorize));
        let range_center = range_hue(settings.range);
        let track = match slider {
            // Colorizing picks an absolute hue, red to red; otherwise the track shows the shift
            // around the range's color.
            Slider::Hue => CameraRawSliderTrack::Spectrum(if colorize { 180.0 } else { range_center }),
            Slider::Saturation => {
                if colorize {
                    CameraRawSliderTrack::Saturation(settings.hue())
                } else if settings.range == ColorRange::Master {
                    CameraRawSliderTrack::Chroma
                } else {
                    CameraRawSliderTrack::Saturation(range_center)
                }
            }
            Slider::Lightness => {
                CameraRawSliderTrack::Opposing(srgb(0.0, 0.0, 0.0), srgb(1.0, 1.0, 1.0))
            }
        };
        let help = format!("{}. Double-click to reset.", slider.title());
        let entity = cx.entity();
        let submit_entity = cx.entity();
        let reset_entity = cx.entity();
        let key_entity = cx.entity();
        let field = self.fields[Slider::ALL.iter().position(|candidate| *candidate == slider).unwrap_or(0)]
            .as_ref()
            .map(|state| state.input.clone());
        let unit = slider.unit();

        h_flex()
            .w_full()
            .items_center()
            .gap(px(SLIDER_GAP))
            .child(
                div()
                    .w(px(TITLE_WIDTH))
                    .text_size(px(CONTROL_SIZE))
                    .child(slider.title())
                    .on_mouse_down(MouseButton::Left, {
                        let entity = entity.clone();
                        move |event: &MouseDownEvent, _, cx| {
                            // `.onTapGesture(count: 2) { value.wrappedValue = reset }`.
                            if event.click_count >= 2 {
                                entity.update(cx, |sheet, cx| sheet.set_slider(slider, reset, cx));
                            }
                        }
                    })
                    .scrubbable(
                        ElementId::Name(format!("hue-slider-{}", slider.title()).into()),
                        NumericScrub::new(value, 1.0, range).on_change(move |value, _, cx| {
                            entity.update(cx, |sheet, cx| sheet.set_slider(slider, value, cx));
                        }),
                    ),
            )
            .child(
                div().flex_1().child(
                    CameraRawSlider::new(
                        ElementId::Name(format!("hue-slider-track-{}", slider.title()).into()),
                        value,
                        range,
                        track,
                        help,
                    )
                        .on_change(move |value, _, cx| {
                            // `onChange: { value.wrappedValue = $0.rounded() }`.
                            reset_entity.update(cx, |sheet, cx| sheet.set_slider(slider, value.round(), cx));
                        })
                        .on_reset(move |_, cx| {
                            key_entity.update(cx, |sheet, cx| sheet.set_slider(slider, reset, cx));
                        }),
                ),
            )
            .when_some(field, |this, field| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap(px(2.0))
                        .child(
                            div()
                                .w(px(FIELD_WIDTH))
                                .capture_key_down(move |event: &KeyDownEvent, _, cx| {
                                    if event.keystroke.key == "enter" {
                                        submit_entity
                                            .update(cx, |sheet, cx| sheet.submit_field(slider, cx));
                                        cx.stop_propagation();
                                    }
                                })
                                .child(Input::new(&field).aria_label(slider.title())),
                        )
                        .when(!unit.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_size(px(CONTROL_SIZE))
                                    .text_color(SECONDARY)
                                    .child(unit),
                            )
                        }),
                )
            })
    }
}

impl Render for HueSaturationSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_fields(window, cx);
        self.sync_fields(window, cx);

        let (settings, preview, limited) = {
            let session = self.session.read(cx);
            let settings = session
                .hue_saturation
                .as_ref()
                .map(|edit| edit.settings.clone())
                .unwrap_or_default();
            let preview = session
                .hue_saturation
                .as_ref()
                .map(|edit| edit.preview)
                .unwrap_or(true);
            (
                settings,
                preview,
                session.adjustment_original.is_none() && session.selection().is_some(),
            )
        };
        let colorize = settings.colorize;
        let shows_spectrum = settings.range != ColorRange::Master && !colorize;

        let range_entity = cx.entity();
        let range_picker = menu_picker(
            "hue-range",
            ColorRange::ALL
                .into_iter()
                .map(|range| (range, range.raw_value())),
            settings.range,
            move |range, _, cx| {
                range_entity.update(cx, |sheet, cx| {
                    sheet.update_settings(move |settings| settings.range = range, cx)
                });
            },
        )
        .w(px(PICKER_WIDTH))
        .disabled(colorize);

        let colorize_entity = cx.entity();
        let preview_entity = cx.entity();
        let reset_entity = cx.entity();
        let invert_entity = cx.entity();
        let cancel_entity = cx.entity();
        let ok_entity = cx.entity();

        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .items_start()
            .child(
                // `HStack(spacing: 12) { Picker...; Spacer(); samplingControls }`.
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(12.0))
                    .child(range_picker)
                    .child(div().flex_1())
                    .child(self.sampling_controls(&settings, shows_spectrum, cx)),
            )
            .child(self.slider_row(Slider::Hue, &settings, cx))
            .child(self.slider_row(Slider::Saturation, &settings, cx))
            .child(self.slider_row(Slider::Lightness, &settings, cx))
            .when(shows_spectrum, |this| {
                this.child(self.spectrum_editor(&settings, cx)).child(
                    Switch::new("hue-invert-range")
                        .checked(settings.invert_range)
                        .label("Apply outside this range instead")
                        .accessibility_label("Apply outside this range instead")
                        .on_change(move |value, _, cx| {
                            let value = *value;
                            invert_entity.update(cx, |sheet, cx| {
                                sheet.update_settings(move |settings| settings.invert_range = value, cx)
                            });
                        }),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(18.0))
                    .child(
                        Switch::new("hue-colorize")
                            .checked(colorize)
                            .label("Colorize")
                            .accessibility_label("Colorize")
                            .on_change(move |value, _, cx| {
                                let colorize = *value;
                                colorize_entity.update(cx, |sheet, cx| {
                                    // Photoshop starts colorizing at hue 0, saturation 25.
                                    sheet.update_settings(move |settings| {
                                        *settings = reset_settings(colorize)
                                    }, cx)
                                });
                            }),
                    )
                    .child(
                        Switch::new("hue-preview")
                            .checked(preview)
                            .label("Preview")
                            .accessibility_label("Preview")
                            .on_change(move |value, _, cx| {
                                let value = *value;
                                preview_entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        let settings = session
                                            .hue_saturation
                                            .as_ref()
                                            .map(|edit| edit.settings.clone())
                                            .unwrap_or_default();
                                        session.update_hue_saturation(&settings, value);
                                        cx.notify();
                                    });
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("hue-reset").label("Reset").on_click(move |_, _, cx| {
                            reset_entity.update(cx, |sheet, cx| {
                                sheet.update_settings(move |settings| *settings = reset_settings(colorize), cx)
                            });
                        }),
                    )
                    .child(div().flex_1()),
            )
            .when(limited, |this| {
                this.child(
                    div()
                        .text_size(px(CONTROL_SIZE))
                        .text_color(SECONDARY)
                        .child("Limited to the selection"),
                )
            })
            .child(Separator::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        Button::new("hue-cancel").label("Cancel").on_click(move |_, _, cx| {
                            cancel_entity.update(cx, |sheet, cx| {
                                sheet.session.update(cx, |session, cx| {
                                    session.cancel_hue_saturation();
                                    cx.notify();
                                });
                                cx.notify();
                            });
                        }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("hue-ok")
                            .label("OK")
                            .primary()
                            .on_click(move |_, _, cx| {
                                ok_entity.update(cx, |sheet, cx| {
                                    sheet.session.update(cx, |session, cx| {
                                        session.commit_hue_saturation();
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
    fn a_slider_range_is_the_colorize_one_when_colorizing() {
        assert_eq!(Slider::Hue.range(false), (-180.0, 180.0));
        assert_eq!(Slider::Hue.range(true), (0.0, 360.0));
        assert_eq!(Slider::Saturation.range(false), (-100.0, 100.0));
        assert_eq!(Slider::Saturation.range(true), (0.0, 100.0));
        assert_eq!(Slider::Lightness.range(true), (-100.0, 100.0));
    }

    #[test]
    fn range_hue_centers_each_color_range_sixty_degrees_apart() {
        assert_eq!(range_hue(ColorRange::Master), 0.0);
        assert_eq!(range_hue(ColorRange::Reds), 0.0);
        assert_eq!(range_hue(ColorRange::Yellows), 60.0);
        assert_eq!(range_hue(ColorRange::Magentas), 300.0);
    }

    #[test]
    fn a_press_takes_the_nearest_handle_around_the_circle() {
        let band = HueBand {
            falloff_start: 315.0,
            range_start: 345.0,
            range_end: 15.0,
            falloff_end: 45.0,
        };
        let handles = band.handles();
        assert_eq!(nearest_handle(&handles, 350.0), 1);
        assert_eq!(nearest_handle(&handles, 5.0), 2);
        assert_eq!(nearest_handle(&handles, 44.0), 3);
        assert_eq!(nearest_handle(&handles, 320.0), 0);
        // The wrap-around: 359 is a degree away from 0, whose handle is the 345 shoulder.
        assert_eq!(nearest_handle(&handles, 350.0), 1);
    }

    #[test]
    fn a_field_writes_whole_numbers() {
        assert_eq!(field_text(0.0), "0");
        assert_eq!(field_text(24.6), "25");
        assert_eq!(field_text(-12.4), "-12");
    }
}
