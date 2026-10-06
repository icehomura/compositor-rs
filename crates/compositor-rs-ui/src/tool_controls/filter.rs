//! The open filter's panel: its settings, Preview, and Cancel / OK (port of `UI/FilterSheet.swift`),
//! with the two subviews that file carries: `GradientMapControls` and `UI/CurvesControls.swift`.
//!
//! **The label width.** The Swift measures every title with a `GeometryReader` and a
//! `LabelWidthKey` preference, then frames each label at `max(60, widest)`. gpui has no preference
//! pass, so the port shapes the titles the current filter is about to draw (`shown_titles`) on this
//! frame and takes the widest; the measurement uses the same font and size the labels are drawn
//! with, so every slider still starts and ends in the same place.
//!
//! **Sliders.** A colored track is [`CameraRawSlider`], the port of the Swift one. A plain track is
//! the component's `Slider`, which carries its value in a [`SliderState`]: the panel keeps one per
//! control and follows the setting each frame, the way the Swift `Slider(value:in:)` binding did.
//! The Swift's logarithmic sliders mapped the setting through `log`/`exp` over
//! `log(lower)...log(upper)`; gpui's slider has that mapping built in (`SliderScale::Logarithmic`),
//! and its percentage-to-value curve is the same `lower * (upper / lower)^fraction` the Swift
//! computed, so the state holds the setting itself.
//!
//! Escape and Return are the app's (the Swift's `.configuredNativeShortcut`), as for the other
//! sheets: this view adds no key handling of its own.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use compositor_rs_core::image_ops::{
    BackgroundQuality, DitherColors, DitherPixelShape, DitherSettings, DitherStyle, FilterKind,
    FilterSettings,
};
use compositor_rs_core::layer_adjustment::{
    AdjustmentColor, BlackWhiteSettings, ColorBalanceSettings, CurvePoint, CurvesSettings,
    ExposureSettings, GrainSettings, LevelsChannel,
};
use compositor_rs_core::PaletteColor;
use compositor_rs_session::EditorSession;

use crate::panels::floating_panel::FloatingPanelController;
use crate::tool_controls::camera_raw::CameraRawControls;
use crate::tool_controls::{menu_picker, segmented_picker, unit_suffix, FieldSpec, Fields, FIELD_HEIGHT};
use crate::tool_header::CONTROL_SIZE;
use crate::widgets::gradient_slider::{srgb, CameraRawSlider, CameraRawSliderTrack};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderScale, SliderState, SliderValue};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The panel's width (`.frame(width: 380)`).
pub const WIDTH: f32 = 380.0;
/// The narrowest label column (`max(60, widest)`).
pub const MIN_LABEL_WIDTH: f32 = 60.0;

/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// A `control` row's `HStack(spacing: 10)`.
const ROW_SPACING: f32 = 10.0;
/// The footer `HStack`'s default spacing.
const FOOTER_SPACING: f32 = 8.0;
/// A subview's own `VStack(spacing: 12)`.
const GROUP_SPACING: f32 = 12.0;
/// A label's text size. The Swift's labels are the sheet's body text (13 points on macOS); the
/// port draws sheet labels at the shared control size, as the other sheets do, which is what the
/// number fields beside them use.
const LABEL_SIZE: f32 = CONTROL_SIZE;
/// `.font(.headline)`: 13-point semibold on macOS.
const HEADLINE_SIZE: f32 = 13.0;
/// `.font(.callout)`: 12 points.
const CALLOUT_SIZE: f32 = 12.0;
/// `.font(.caption)`: 10 points.
const CAPTION_SIZE: f32 = 10.0;
/// `.frame(width: 56)` on every number field.
const FIELD_WIDTH: f32 = 56.0;
/// The swatches' `.frame(width: 24, height: 24)`.
const SWATCH_SIZE: f32 = 24.0;
/// `RoundedRectangle(cornerRadius: 6, style: .continuous)`.
const SWATCH_RADIUS: f32 = 6.0;
/// The vignette color row's `Text("Color").frame(width: 95, alignment: .leading)`.
const VIGNETTE_LABEL_WIDTH: f32 = 95.0;
/// The curve editor's `.frame(height: 260)`.
const CURVE_HEIGHT: f32 = 260.0;
/// The curve editor's grid, `for i in 0...4`.
const CURVE_GRID_STEPS: usize = 4;
/// A curve handle's `.frame(width: 8, height: 8)`.
const CURVE_POINT_SIZE: f32 = 8.0;
/// How close a press must be to take a handle (`hypot(...) < 14`).
const CURVE_HIT_RADIUS: f64 = 14.0;
/// The most handles a curve holds (`p.count < 32`).
const CURVE_MAX_POINTS: usize = 32;
/// The smallest gap a dragged handle keeps from its neighbors (`p[i±1].x ∓ 1`).
const CURVE_POINT_GAP: f64 = 1.0;
/// The gradient bar's `.frame(height: 20)`.
const GRADIENT_HEIGHT: f32 = 20.0;
/// Its `RoundedRectangle(cornerRadius: 4, style: .continuous)`.
const GRADIENT_RADIUS: f32 = 4.0;

/// The secondary label color of `.foregroundStyle(.secondary)`.
const SECONDARY: Hsla = hsla(0.0, 0.0, 1.0, 0.55);
/// SwiftUI's `.orange`, the preview error's color.
const ORANGE: Hsla = hsla(35.0 / 360.0, 1.0, 0.5, 1.0);
/// `Color.white`.
const OPAQUE_WHITE: Hsla = hsla(0.0, 0.0, 1.0, 1.0);
/// `Color.black`.
const OPAQUE_BLACK: Hsla = hsla(0.0, 0.0, 0.0, 1.0);

/// `Color(.sRGB, red:green:blue:)`: an adjustment color as the panel draws it.
fn adjustment_hsla(color: AdjustmentColor) -> Hsla {
    Hsla::from(srgb(color.red as f32, color.green as f32, color.blue as f32))
}

/// `let step = pow(10, Double(decimals))`: what a value is rounded to the reciprocal of.
fn step_of(decimals: usize) -> f64 {
    10f64.powi(decimals as i32)
}

/// The quantum a value is held in: `1 / step`.
fn quantum_of(decimals: usize) -> f64 {
    1.0 / step_of(decimals)
}

/// Whether the slider already shows `value`, to the precision both hold it in — the slider's own
/// `(value / quantum).round() * quantum` against the setting. Comparing the two quantized lets the
/// frame after a write find them equal, instead of pushing the same value back for ever.
fn slider_shows(shown: f64, value: f64, step: f64) -> bool {
    (value * step).round() == (shown * step).round()
}

/// The curve editor's `onChanged` coordinates: the pointer inside the editor as 0…255 in and out,
/// or `None` while the editor has no size yet.
fn curve_coordinates(local_x: f32, local_y: f32, width: f32, height: f32) -> Option<(f64, f64)> {
    if !(width > 0.0 && height > 0.0) {
        return None;
    }
    let x = ((local_x / width) as f64 * 255.0).clamp(0.0, 255.0);
    let y = (255.0 - (local_y / height) as f64 * 255.0).clamp(0.0, 255.0);
    Some((x, y))
}

/// `update(_:)`: the open edit's settings with `change` applied, written back through
/// `updateFilter(_:preview:)`. A commit in flight swallows the change, which is what the panel's
/// `.disabled(edit?.committing == true)` did.
fn update_filter_settings(
    session: &Entity<EditorSession>,
    cx: &mut App,
    change: impl FnOnce(&mut FilterSettings),
) {
    session.update(cx, |session, cx| {
        let (mut settings, preview) = match session.filter_edit.as_ref() {
            Some(edit) if !edit.committing => (edit.settings.clone(), edit.preview),
            _ => return,
        };
        change(&mut settings);
        session.update_filter(settings, preview);
        // The panel and the window's other views observe the session; this is how a slider step
        // reaches the canvas preview.
        cx.notify();
    });
}

/// The titles of the controls the panel shows for `kind` and `settings`: what the Swift measured
/// through `LabelWidthKey` on the current frame. Only the labels on screen count, so the tinted
/// Black & White controls, the advanced Remove Background ones and the style-dependent Dither ones
/// are in the list only while they are up; Camera Raw's panel measures its own.
pub fn shown_titles(kind: FilterKind, settings: &FilterSettings) -> Vec<&'static str> {
    match kind {
        // `CurvesControls`, `GradientMapControls` and the two paragraphs have no `control` rows.
        FilterKind::Curves
        | FilterKind::GradientMap
        | FilterKind::CameraRaw
        | FilterKind::ContentAwareFill => Vec::new(),
        FilterKind::Exposure => vec!["Exposure", "Offset", "Gamma"],
        FilterKind::BlackWhite => {
            let mut titles = vec!["Reds", "Yellows", "Greens", "Cyans", "Blues", "Magentas"];
            if settings.black_white.tint {
                titles.push("Hue");
                titles.push("Saturation");
            }
            titles
        }
        FilterKind::ColorBalance => vec![
            "Cyan / Red",
            "Magenta / Green",
            "Yellow / Blue",
            "Cyan / Red",
            "Magenta / Green",
            "Yellow / Blue",
            "Cyan / Red",
            "Magenta / Green",
            "Yellow / Blue",
        ],
        FilterKind::Grain => vec!["Amount", "Size", "Roughness"],
        FilterKind::RemoveBackground => {
            let mut titles = Vec::new();
            if settings.background_quality == BackgroundQuality::Advanced {
                titles.extend(["Refine", "Contrast", "Shift Edge"]);
            }
            titles
        }
        FilterKind::GaussianBlur => vec!["Radius"],
        FilterKind::MotionBlur => vec!["Angle", "Distance"],
        FilterKind::AddNoise => vec!["Amount"],
        FilterKind::Dither => {
            let dither = &settings.dither;
            let mut titles = Vec::new();
            if dither.style.uses_pixel_size() {
                titles.push("Pixel Size");
            }
            if dither.style == DitherStyle::Ascii {
                titles.push("Text Size");
            }
            if dither.style == DitherStyle::Scanlines {
                titles.extend(["Line Spacing", "Glow", "Dots", "Wobble"]);
            }
            if dither.style.is_halftone() {
                titles.push("Cell Size");
                titles.push("Angle");
            }
            if dither.style.has_tones() {
                titles.push("Tones");
            }
            if dither.style.diffuses() {
                titles.push("Diffusion");
            }
            titles.push("Density");
            titles.push("Contrast");
            titles
        }
        FilterKind::Vignette => vec!["Amount", "Midpoint", "Roundness", "Feather", "Highlights"],
        FilterKind::BloomGlow => vec!["Amount", "Radius"],
        FilterKind::TonalContrast => vec!["Amount", "Shadows", "Midtones", "Highlights", "Radius"],
        FilterKind::LensCorrection => vec!["Remove Distortion"],
    }
}

/// The swatch of `swatch(_:help:action:)` and of `GradientMapControls`' own: a 24-point rounded
/// square in the color, with a 1.5-point white stroke inset by 1 and a 1-point black stroke on the
/// edge (`shape.inset(by: 1).strokeBorder(.white, lineWidth: 1.5)` and `.strokeBorder(.black)`).
fn swatch_shape(color: AdjustmentColor) -> Div {
    div()
        .relative()
        .flex_none()
        .size(px(SWATCH_SIZE))
        .rounded(px(SWATCH_RADIUS))
        .border_1()
        .border_color(OPAQUE_BLACK)
        .bg(adjustment_hsla(color))
        .child(
            div()
                .absolute()
                .inset(px(1.0))
                .rounded(px(SWATCH_RADIUS - 1.0))
                .border(px(1.5))
                .border_color(OPAQUE_WHITE),
        )
}

/// An element id made from a name and a key: gpui has no `From<(&str, &str)>` for `ElementId`, so
/// ids with two parts go through a `SharedString`.
fn element_id(name: &'static str, key: &str) -> SharedString {
    SharedString::from(format!("{name}-{key}"))
}

/// One `control(_:_:range:unit:decimals:logarithmic:track:)` row.
#[derive(Clone)]
struct ControlSpec {
    /// The row's element id, the field's id and the key of its plain slider.
    id: &'static str,
    /// The label, which is also the colored slider's help.
    title: &'static str,
    /// The setting's value.
    get: fn(&FilterSettings) -> f64,
    /// Writes the setting.
    set: fn(&mut FilterSettings, f64),
    /// The slider's range (`range:`).
    range: (f64, f64),
    /// The field's unit (`unitSuffix(_:)`).
    unit: &'static str,
    /// The field's fraction digits (`fractionLength(0...decimals)`).
    decimals: usize,
    /// Whether the plain slider maps through `log`/`exp` (`logarithmic:`).
    logarithmic: bool,
    /// The colored track, when the row draws the Swift Camera Raw slider.
    track: Option<CameraRawSliderTrack>,
    /// The row's `.help(_:)`, when it has one.
    help: Option<&'static str>,
}

impl ControlSpec {
    fn new(
        id: &'static str,
        title: &'static str,
        get: fn(&FilterSettings) -> f64,
        set: fn(&mut FilterSettings, f64),
        range: (f64, f64),
        unit: &'static str,
        decimals: usize,
        logarithmic: bool,
    ) -> Self {
        Self {
            id,
            title,
            get,
            set,
            range,
            unit,
            decimals,
            logarithmic,
            track: None,
            help: None,
        }
    }

    /// `track:`.
    fn track(mut self, track: CameraRawSliderTrack) -> Self {
        self.track = Some(track);
        self
    }

    /// The `.help(_:)` the Swift hung on the returned row.
    fn help(mut self, help: &'static str) -> Self {
        self.help = Some(help);
        self
    }

    /// `pow(10, Double(decimals))`.
    fn step(&self) -> f64 {
        step_of(self.decimals)
    }
}

/// A plain slider's state, and what its value means.
struct SliderRow {
    state: Entity<SliderState>,
    spec: ControlSpec,
}

/// The open filter's panel: its settings, Preview, and Cancel / OK.
pub struct FilterSheet {
    session: Entity<EditorSession>,
    /// The number fields, made on their first frame (`Fields`).
    fields: Fields,
    /// The plain sliders, made on their first frame: SwiftUI's `Slider(value:in:)` is a binding,
    /// so the port keeps one state per control and follows the setting.
    sliders: HashMap<&'static str, SliderRow>,
    /// The widest label of the controls on screen (`@State labelWidth`), measured each frame.
    label_width: f32,
    /// Dither's Characters field, made when the ASCII style is first shown.
    characters: Option<Entity<InputState>>,
    /// `GradientMapControls`, built when the Gradient Map panel is open.
    gradient_map: Option<Entity<GradientMapControls>>,
    /// `CurvesControls`, built when the Curves panel is open.
    curves: Option<Entity<CurvesControls>>,
    /// Camera Raw's controls, built when that panel is open.
    camera_raw: Option<Entity<CameraRawControls>>,
    /// The color the picker had when it last reported, so `.onChange(of: session.colorPicker?.color)`
    /// can see it move.
    picker_color: Option<PaletteColor>,
}

impl FilterSheet {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        let picker_color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
        cx.observe(&session, |sheet, session, cx| {
            // `.onChange(of: session.colorPicker?.color) { … }`: the three filters a picker can be
            // tinting preview their working color while it is open.
            let color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
            if color != sheet.picker_color {
                sheet.picker_color = color;
                session.update(cx, |session, _| {
                    session.preview_gradient_map_color();
                    session.preview_vignette_color();
                    session.preview_dither_color();
                });
            }
            cx.notify();
        })
        .detach();
        Self {
            session,
            fields: Fields::default(),
            sliders: HashMap::new(),
            label_width: MIN_LABEL_WIDTH,
            characters: None,
            gradient_map: None,
            curves: None,
            camera_raw: None,
            picker_color,
        }
    }

    /// `edit?.kind ?? .gaussianBlur`.
    fn kind(&self, cx: &App) -> FilterKind {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.kind)
            .unwrap_or(FilterKind::GaussianBlur)
    }

    /// `edit?.settings ?? FilterSettings()`.
    fn settings(&self, cx: &App) -> FilterSettings {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.settings.clone())
            .unwrap_or_default()
    }

    /// `edit?.preview ?? true`.
    fn preview(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.preview)
            .unwrap_or(true)
    }

    /// `edit?.committing == true`.
    fn committing(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.committing)
    }

    /// `edit?.preparing == true`.
    fn preparing(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.preparing)
    }

    /// `edit?.previewError`.
    fn preview_error(&self, cx: &App) -> Option<String> {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .and_then(|edit| edit.preview_error.clone())
    }

    /// `Self.resetting(_:in:)`: the setting put back to its filter's default, as a double-click on
    /// a colored slider does.
    pub fn resetting(
        settings: &FilterSettings,
        get: fn(&FilterSettings) -> f64,
        set: fn(&mut FilterSettings, f64),
    ) -> FilterSettings {
        let mut value = settings.clone();
        set(&mut value, get(&FilterSettings::default()));
        value
    }

    /// `cyanRedTrack`.
    pub fn cyan_red_track() -> CameraRawSliderTrack {
        CameraRawSliderTrack::Opposing(srgb(0.10, 0.72, 0.80), srgb(0.86, 0.18, 0.20))
    }

    /// `magentaGreenTrack`.
    pub fn magenta_green_track() -> CameraRawSliderTrack {
        CameraRawSliderTrack::Opposing(srgb(0.80, 0.22, 0.70), srgb(0.24, 0.70, 0.30))
    }

    /// `yellowBlueTrack`.
    pub fn yellow_blue_track() -> CameraRawSliderTrack {
        CameraRawSliderTrack::Opposing(srgb(0.95, 0.82, 0.18), srgb(0.22, 0.40, 0.92))
    }

    /// `update(_:)`, with the panel redrawn afterwards: the rows themselves depend on the settings
    /// (a tinted Black & White adds two, a Dither style swaps its controls).
    fn update(&mut self, change: impl FnOnce(&mut FilterSettings), cx: &mut Context<Self>) {
        update_filter_settings(&self.session, cx, change);
        cx.notify();
    }

    /// `flag(_:)`'s setter: one of the panel's switches.
    fn set_flag(&mut self, set: fn(&mut FilterSettings, bool), value: bool, cx: &mut Context<Self>) {
        self.update(|settings| set(settings, value), cx);
    }

    /// `Toggle("Preview", isOn:)`: the getter is the edit's own flag, and the setter passes the
    /// panel's current settings through `updateFilter(_:preview:)`.
    fn set_preview(&mut self, preview: bool, cx: &mut Context<Self>) {
        let Some(settings) = self
            .session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.settings.clone())
        else {
            return;
        };
        self.session.update(cx, |session, cx| {
            session.update_filter(settings, preview);
            cx.notify();
        });
        cx.notify();
    }

    /// The plain slider for `spec`, made on its first frame and subscribed to once.
    fn slider_state(&mut self, spec: &ControlSpec, value: f64, cx: &mut Context<Self>) -> Entity<SliderState> {
        if let Some(row) = self.sliders.get(spec.id) {
            return row.state.clone();
        }
        let quantum = quantum_of(spec.decimals) as f32;
        let scale = if spec.logarithmic {
            SliderScale::Logarithmic
        } else {
            SliderScale::Linear
        };
        let state = cx.new(|_| {
            SliderState::new()
                .min(spec.range.0 as f32)
                .max(spec.range.1 as f32)
                .scale(scale)
                .step(quantum)
                .default_value(value as f32)
        });
        let id = spec.id;
        cx.subscribe(
            &state,
            move |sheet: &mut Self, _: Entity<SliderState>, event: &SliderEvent, cx| {
                let SliderEvent::Change(SliderValue::Single(shown)) = event else {
                    return;
                };
                sheet.slider_changed(id, f64::from(*shown), cx);
            },
        )
        .detach();
        self.sliders.insert(
            spec.id,
            SliderRow {
                state: state.clone(),
                spec: spec.clone(),
            },
        );
        state
    }

    /// A slider's value: `((logarithmic ? exp(value) : value) * step).rounded() / step`, written
    /// only when it moves.
    fn slider_changed(&mut self, id: &'static str, value: f64, cx: &mut Context<Self>) {
        let Some(spec) = self.sliders.get(id).map(|row| row.spec.clone()) else {
            return;
        };
        let step = spec.step();
        let next = (value * step).round() / step;
        if (spec.get)(&self.settings(cx)) == next {
            return;
        }
        update_filter_settings(&self.session, cx, |settings| (spec.set)(settings, next));
        cx.notify();
    }

    /// The panel's width (`frame(width: isCameraRaw ? 440 : 380)`).
    fn width(&self, kind: FilterKind) -> f32 {
        if kind == FilterKind::CameraRaw {
            FloatingPanelController::DOCKED_WIDTH
        } else {
            WIDTH
        }
    }

    /// `control(_:_:range:unit:decimals:logarithmic:track:)`: a slider plus its exact field. The
    /// title is the scrub target and, on a colored row, the double-click reset as well.
    fn control(
        &mut self,
        spec: ControlSpec,
        value: f64,
        disabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label_width = self.label_width;
        let step = spec.step();
        let session = self.session.clone();
        let reset: Rc<dyn Fn(&mut Window, &mut App)> = Rc::new({
            let session = session.clone();
            let get = spec.get;
            let set = spec.set;
            move |_window: &mut Window, cx: &mut App| {
                update_filter_settings(&session, cx, |settings| {
                    *settings = Self::resetting(settings, get, set);
                });
            }
        });

        let mut label = div()
            .id(element_id("filter-label", spec.id))
            .flex_none()
            .w(px(label_width))
            .text_size(px(LABEL_SIZE))
            .child(spec.title);
        if spec.track.is_some() {
            // `.onTapGesture(count: 2) { if track != nil { reset() } }`.
            let reset = reset.clone();
            label = label.cursor(CursorStyle::PointingHand).on_click(move |event, window, cx| {
                if event.click_count() >= 2 {
                    reset(window, cx);
                }
            });
        }
        let label: AnyElement = if disabled {
            label.opacity(0.5).into_any_element()
        } else {
            // `.scrubbable(sensitivity: 1 / step, value:…, range:…)`.
            let session = session.clone();
            let set = spec.set;
            let scrub = NumericScrub::new(value, 1.0 / step, spec.range).on_change(move |value, _window, cx| {
                update_filter_settings(&session, cx, |settings| set(settings, value));
            });
            label
                .scrubbable(element_id("filter-scrub", spec.id), scrub)
                .into_any_element()
        };

        let slider: AnyElement = match spec.track.clone() {
            Some(track) => {
                let help = format!("{}. Double-click to reset.", spec.title);
                let session = session.clone();
                let set = spec.set;
                let reset = reset.clone();
                let camera = CameraRawSlider::new(spec.id, value, spec.range, track, help)
                    .on_change(move |value, _window, cx| {
                        // `(value * step).rounded() / step`.
                        let value = (value * step).round() / step;
                        update_filter_settings(&session, cx, |settings| set(settings, value));
                    })
                    .on_reset(move |window, cx| reset(window, cx));
                div()
                    .flex_1()
                    .when(disabled, |this| this.opacity(0.5))
                    .child(camera)
                    .into_any_element()
            }
            None => {
                let state = self.slider_state(&spec, value, cx);
                // The slider follows the setting, whichever control moved it.
                if let SliderValue::Single(shown) = state.read(cx).value() {
                    if !slider_shows(f64::from(shown), value, step) {
                        state.update(cx, |state, cx| state.set_value(value as f32, window, cx));
                    }
                }
                Slider::new(&state)
                    .horizontal()
                    .flex_1()
                    .disabled(disabled)
                    .into_any_element()
            }
        };

        let field = self.fields.get(spec.id, cx);
        let write = {
            let session = session.clone();
            let set = spec.set;
            move |value: f64, _window: &mut Window, cx: &mut App| {
                update_filter_settings(&session, cx, |settings| set(settings, value));
            }
        };
        let element = field.element(
            spec.id,
            value,
            FieldSpec::new(spec.range, spec.decimals).disabled(disabled),
            FIELD_WIDTH,
            write,
            cx,
        );
        let unit = div().text_size(px(LABEL_SIZE)).child(spec.unit);
        let row = h_flex()
            .items_center()
            .gap(px(ROW_SPACING))
            .child(label)
            .child(slider)
            .child(unit_suffix(element, unit));
        match spec.help {
            Some(help) => row
                .id(element_id("filter-row", spec.id))
                .tooltip(move |window, cx| Tooltip::new(help).build(window, cx))
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// A `Toggle`: the switch with its label and, when it has one, its help.
    fn switch_row(
        id: &'static str,
        title: &'static str,
        help: Option<&'static str>,
        checked: bool,
        disabled: bool,
        set: fn(&mut FilterSettings, bool),
        cx: &mut Context<Self>,
    ) -> Switch {
        let entity = cx.entity();
        let switch = Switch::new(id)
            .label(title)
            .accessibility_label(title)
            .checked(checked)
            .disabled(disabled)
            .on_change(move |value, _, cx| {
                let value = *value;
                entity.update(cx, |sheet, cx| sheet.set_flag(set, value, cx));
            });
        match help {
            Some(help) => switch.tooltip(help),
            None => switch,
        }
    }

    /// `swatch(_:help:action:)`: one of the app's own swatches (`.buttonStyle(.plain)`).
    fn swatch(
        id: SharedString,
        color: AdjustmentColor,
        help: &'static str,
        label: Option<&'static str>,
        disabled: bool,
        action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .flex_none()
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .when_some(label, |this, label| this.aria_label(label))
            .tooltip(move |window, cx| Tooltip::new(help).build(window, cx))
            .when(!disabled, |this| this.on_click(move |_, window, cx| action(window, cx)))
            .child(swatch_shape(color))
            .into_any_element()
    }

    /// Dither's Style menu: `DitherStyle.groups`, with a line between groups and the current style
    /// checked, as the Swift menu-style `Picker` shows them.
    fn dither_style_picker(&self, style: DitherStyle, disabled: bool, cx: &mut Context<Self>) -> DropdownButton {
        let entity = cx.entity();
        let on_select: Rc<dyn Fn(DitherStyle, &mut Window, &mut App)> = Rc::new(move |value, _, cx| {
            entity.update(cx, |sheet, cx| {
                sheet.update(|settings| settings.dither.style = value, cx);
            });
        });
        DropdownButton::new("filter-dither-style")
            .button(
                Button::new("filter-dither-style-button")
                    .label(style.raw_value())
                    .h(px(FIELD_HEIGHT))
                    .text_size(px(CONTROL_SIZE)),
            )
            .disabled(disabled)
            .dropdown_menu(move |menu, _, _| {
                DitherStyle::GROUPS.iter().enumerate().fold(menu, |menu, (index, group)| {
                    let menu = if index > 0 { menu.separator() } else { menu };
                    group.iter().fold(menu, |menu, candidate| {
                        let on_select = on_select.clone();
                        let candidate = *candidate;
                        menu.item(
                            PopupMenuItem::new(candidate.raw_value())
                                .checked(candidate == style)
                                .on_click(move |_, window, cx| on_select(candidate, window, cx)),
                        )
                    })
                })
            })
    }

    /// Dither's Characters field: `TextField("Characters", text:)` writes the setting on every
    /// keystroke, and shows the setting again while it does not have the keyboard.
    fn characters_field(
        &mut self,
        characters: &str,
        disabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = match &self.characters {
            Some(state) => state.clone(),
            None => {
                let state = cx.new(|cx| InputState::new(window, cx).default_value(characters));
                self.characters = Some(state.clone());
                state
            }
        };
        let focused = state.read(cx).focus_handle(cx).is_focused(window);
        let typed = state.read(cx).value().to_string();
        if focused {
            if typed != characters {
                self.update(|settings| settings.dither.characters = typed, cx);
            }
        } else if typed != characters {
            let text = characters.to_string();
            state.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        div()
            .flex_1()
            .child(
                Input::new(&state)
                    .aria_label("Characters")
                    .font_family(cx.theme().mono_font_family.clone())
                    .disabled(disabled),
            )
            .into_any_element()
    }

    /// `ditherControls`: the Style menu and every control the chosen style shows.
    fn dither_controls(
        &mut self,
        settings: &FilterSettings,
        committing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let dither = settings.dither.clone();
        let style = dither.style;
        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(self.dither_style_picker(style, committing, cx).into_any_element());
        if style.uses_pixel_size() {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-pixel-size",
                        "Pixel Size",
                        |settings| settings.dither.pixel_size,
                        |settings, value| settings.dither.pixel_size = value,
                        DitherSettings::PIXEL_SIZE_RANGE,
                        "px",
                        0,
                        false,
                    )
                    .help("Make each dithered pixel this many pixels across, for a chunky old-screen look"),
                    dither.pixel_size,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if style == DitherStyle::Ascii {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-text-size",
                        "Text Size",
                        |settings| settings.dither.text_size,
                        |settings, value| settings.dither.text_size = value,
                        DitherSettings::TEXT_SIZE_RANGE,
                        "px",
                        0,
                        false,
                    )
                    .help("The height of each line of characters"),
                    dither.text_size,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if style == DitherStyle::Scanlines {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-line-spacing",
                        "Line Spacing",
                        |settings| settings.dither.line_spacing,
                        |settings, value| settings.dither.line_spacing = value,
                        DitherSettings::LINE_SPACING_RANGE,
                        "px",
                        0,
                        false,
                    )
                    .help("How far apart the screen's lines are"),
                    dither.line_spacing,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-glow",
                        "Glow",
                        |settings| settings.dither.glow,
                        |settings, value| settings.dither.glow = value,
                        (0.0, 100.0),
                        "%",
                        0,
                        false,
                    )
                    .help("Light blooming around the lines, like a CRT's phosphors"),
                    dither.glow,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-dots",
                        "Dots",
                        |settings| settings.dither.dots,
                        |settings, value| settings.dither.dots = value,
                        (0.0, 100.0),
                        "%",
                        0,
                        false,
                    )
                    .help("Break the lines into glowing beads"),
                    dither.dots,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-wobble",
                        "Wobble",
                        |settings| settings.dither.wobble,
                        |settings, value| settings.dither.wobble = value,
                        DitherSettings::WOBBLE_RANGE,
                        "px",
                        0,
                        false,
                    )
                    .help("Make the lines waver sideways down the screen, like a CRT losing sync"),
                    dither.wobble,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if style.is_halftone() {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-cell-size",
                        "Cell Size",
                        |settings| settings.dither.cell_size,
                        |settings, value| settings.dither.cell_size = value,
                        DitherSettings::CELL_SIZE_RANGE,
                        "px",
                        0,
                        false,
                    ),
                    dither.cell_size,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-angle",
                        "Angle",
                        |settings| settings.dither.angle,
                        |settings, value| settings.dither.angle = value,
                        (-90.0, 90.0),
                        "°",
                        0,
                        false,
                    ),
                    dither.angle,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if style == DitherStyle::Ascii {
            let field = self.characters_field(&dither.characters, committing, window, cx);
            rows.push(
                div()
                    .id("filter-dither-characters")
                    .tooltip(|window, cx| {
                        Tooltip::new("The characters to draw with, in any order: each spot gets the one whose ink best matches its tone").build(window, cx)
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(div().text_size(px(LABEL_SIZE)).child("Characters"))
                            .child(field),
                    )
                    .into_any_element(),
            );
        }
        if style.has_tones() {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-levels",
                        "Tones",
                        |settings| settings.dither.levels,
                        |settings, value| settings.dither.levels = value,
                        DitherSettings::LEVELS_RANGE,
                        "",
                        0,
                        false,
                    )
                    .help("Tones per channel: 2 is pure black and white"),
                    dither.levels,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if style.diffuses() {
            rows.push(
                self.control(
                    ControlSpec::new(
                        "filter-dither-diffusion",
                        "Diffusion",
                        |settings| settings.dither.diffusion,
                        |settings, value| settings.dither.diffusion = value,
                        (0.0, 100.0),
                        "%",
                        0,
                        false,
                    )
                    .help("How much of each pixel's error spreads to its neighbors. Less gives flatter areas"),
                    dither.diffusion,
                    committing,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        rows.push(
            self.control(
                ControlSpec::new(
                    "filter-dither-density",
                    "Density",
                    |settings| settings.dither.density,
                    |settings, value| settings.dither.density = value,
                    (-100.0, 100.0),
                    "",
                    0,
                    false,
                )
                .help("More ink (darker) or less before dithering"),
                dither.density,
                committing,
                window,
                cx,
            )
            .into_any_element(),
        );
        rows.push(
            self.control(
                ControlSpec::new(
                    "filter-dither-contrast",
                    "Contrast",
                    |settings| settings.dither.contrast,
                    |settings, value| settings.dither.contrast = value,
                    (-100.0, 100.0),
                    "",
                    0,
                    false,
                ),
                dither.contrast,
                committing,
                window,
                cx,
            )
            .into_any_element(),
        );
        // A menu, like Style: the three choices as segments are wider than the panel, which then
        // flips between squeezing the row and wrapping it, resizing itself at every slider step.
        rows.push(
            menu_picker(
                "filter-dither-colors",
                DitherColors::ALL.into_iter().map(|colors| (colors, colors.raw_value())),
                dither.colors,
                {
                    let entity = cx.entity();
                    move |value, _, cx| {
                        entity.update(cx, |sheet, cx| {
                            sheet.update(|settings| settings.dither.colors = value, cx);
                        });
                    }
                },
            )
            .disabled(committing)
            .into_any_element(),
        );
        if dither.colors == DitherColors::TwoColors {
            let dark = {
                let session = self.session.clone();
                move |_: &mut Window, cx: &mut App| {
                    session.update(cx, |session, _| session.open_dither_color_picker(false))
                }
            };
            let light = {
                let session = self.session.clone();
                move |_: &mut Window, cx: &mut App| {
                    session.update(cx, |session, _| session.open_dither_color_picker(true))
                }
            };
            rows.push(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().text_size(px(LABEL_SIZE)).child("Dark"))
                    .child(Self::swatch(
                        element_id("filter-dither", "dark"),
                        dither.dark,
                        "Choose the dark color",
                        None,
                        committing,
                        dark,
                    ))
                    .child(div().pl(px(10.0)).text_size(px(LABEL_SIZE)).child("Light"))
                    .child(Self::swatch(
                        element_id("filter-dither", "light"),
                        dither.light,
                        "Choose the light color",
                        None,
                        committing,
                        light,
                    ))
                    .into_any_element(),
            );
        }
        if dither.pixel_size > 1.0 && style.uses_pixel_size() {
            rows.push(
                div()
                    .id("filter-dither-pixel-shape")
                    .tooltip(|window, cx| {
                        Tooltip::new("Draw each chunky pixel as a solid square, or as a round dot like a dot-matrix screen").build(window, cx)
                    })
                    .child(
                        menu_picker(
                            "filter-dither-pixel-shape-picker",
                            DitherPixelShape::ALL
                                .into_iter()
                                .map(|shape| (shape, shape.raw_value())),
                            dither.pixel_shape,
                            {
                                let entity = cx.entity();
                                move |value, _, cx| {
                                    entity.update(cx, |sheet, cx| {
                                        sheet.update(|settings| settings.dither.pixel_shape = value, cx);
                                    });
                                }
                            },
                        )
                        .disabled(committing),
                    )
                    .into_any_element(),
            );
        }
        if style.draws_marks() {
            rows.push(
                Self::switch_row(
                    "filter-dither-light-on-dark",
                    "Light on Dark",
                    Some("Draw the marks for the light tones on the dark color, like a glowing screen"),
                    dither.light_on_dark,
                    committing,
                    |settings, value| settings.dither.light_on_dark = value,
                    cx,
                )
                .into_any_element(),
            );
        }
        rows
    }

    /// `.onPreferenceChange(LabelWidthKey.self) { labelWidth = max(60, $0) }`, measured here: the
    /// port has no preference pass, so the widest title the kind is about to draw is shaped on this
    /// frame, with the font the labels are drawn in.
    fn measure_label_width(&self, titles: &[&'static str], window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let font = Font {
            family: cx.theme().font_family.clone(),
            ..Font::default()
        };
        let widest = titles.iter().fold(0.0_f32, |widest, title| {
            let run = TextRun {
                len: title.len(),
                font: font.clone(),
                color: OPAQUE_WHITE,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::new_static(*title),
                px(LABEL_SIZE),
                &[run],
                None,
            );
            widest.max(f32::from(shaped.width()))
        });
        MIN_LABEL_WIDTH.max(widest)
    }
}

impl Render for FilterSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.kind(cx);
        let settings = self.settings(cx);
        // `.onPreferenceChange(LabelWidthKey.self)`: the widest title of the rows about to be drawn.
        self.label_width = self.measure_label_width(&shown_titles(kind, &settings), window, cx);
        let committing = self.committing(cx);
        let preview = self.preview(cx);
        let preparing = self.preparing(cx);
        let preview_error = self.preview_error(cx);
        let limited = {
            let session = self.session.read(cx);
            session.adjustment_original.is_none() && session.selection().is_some()
        };
        let is_camera_raw = kind == FilterKind::CameraRaw;

        let mut rows: Vec<AnyElement> = Vec::new();
        match kind {
            FilterKind::Curves => {
                let view = match &self.curves {
                    Some(view) => view.clone(),
                    None => {
                        let view = cx.new(|cx| CurvesControls::new(self.session.clone(), cx));
                        self.curves = Some(view.clone());
                        view
                    }
                };
                rows.push(div().w_full().child(view).into_any_element());
            }
            FilterKind::Exposure => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-exposure", "Exposure", |s| s.exposure.exposure, |s, value| s.exposure.exposure = value, ExposureSettings::EXPOSURE_RANGE, "", 2, false),
                        settings.exposure.exposure,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-offset", "Offset", |s| s.exposure.offset, |s, value| s.exposure.offset = value, ExposureSettings::OFFSET_RANGE, "", 4, false),
                        settings.exposure.offset,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-gamma", "Gamma", |s| s.exposure.gamma, |s, value| s.exposure.gamma = value, ExposureSettings::GAMMA_RANGE, "", 2, true),
                        settings.exposure.gamma,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::GradientMap => {
                let view = match &self.gradient_map {
                    Some(view) => view.clone(),
                    None => {
                        let view = cx.new(|cx| GradientMapControls::new(self.session.clone(), cx));
                        self.gradient_map = Some(view.clone());
                        view
                    }
                };
                rows.push(div().w_full().child(view).into_any_element());
            }
            FilterKind::BlackWhite => {
                let black_white = settings.black_white;
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-reds", "Reds", |s| s.black_white.reds, |s, value| s.black_white.reds = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(0.0)),
                        black_white.reds,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-yellows", "Yellows", |s| s.black_white.yellows, |s, value| s.black_white.yellows = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(60.0)),
                        black_white.yellows,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-greens", "Greens", |s| s.black_white.greens, |s, value| s.black_white.greens = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(120.0)),
                        black_white.greens,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-cyans", "Cyans", |s| s.black_white.cyans, |s, value| s.black_white.cyans = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(180.0)),
                        black_white.cyans,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-blues", "Blues", |s| s.black_white.blues, |s, value| s.black_white.blues = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(240.0)),
                        black_white.blues,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-black-white-magentas", "Magentas", |s| s.black_white.magentas, |s, value| s.black_white.magentas = value, BlackWhiteSettings::RANGE, "%", 0, false)
                            .track(CameraRawSliderTrack::Luminance(300.0)),
                        black_white.magentas,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    Self::switch_row(
                        "filter-black-white-tint",
                        "Tint",
                        Some("Color the result while keeping its tones, for a sepia or a cyanotype"),
                        black_white.tint,
                        committing,
                        |settings, value| settings.black_white.tint = value,
                        cx,
                    )
                    .into_any_element(),
                );
                if black_white.tint {
                    rows.push(
                        self.control(
                            ControlSpec::new("filter-black-white-hue", "Hue", |s| s.black_white.tint_hue, |s, value| s.black_white.tint_hue = value, (0.0, 360.0), "°", 0, false)
                                .track(CameraRawSliderTrack::Plain),
                            black_white.tint_hue,
                            committing,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                    rows.push(
                        self.control(
                            ControlSpec::new("filter-black-white-saturation", "Saturation", |s| s.black_white.tint_saturation, |s, value| s.black_white.tint_saturation = value, (0.0, 100.0), "%", 0, false)
                                .track(CameraRawSliderTrack::Saturation(black_white.tint_hue)),
                            black_white.tint_saturation,
                            committing,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                }
            }
            FilterKind::CameraRaw => {
                let view = match &self.camera_raw {
                    Some(view) => view.clone(),
                    None => {
                        let view = cx.new(|cx| CameraRawControls::new(self.session.clone(), cx));
                        self.camera_raw = Some(view.clone());
                        view
                    }
                };
                // `.frame(maxHeight: .infinity, alignment: .top)`.
                rows.push(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .items_start()
                        .child(view)
                        .into_any_element(),
                );
            }
            FilterKind::ColorBalance => {
                let balance = settings.color_balance;
                rows.push(headline("Shadows").into_any_element());
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-shadow-cyan-red", "Cyan / Red", |s| s.color_balance.shadow_cyan_red, |s, value| s.color_balance.shadow_cyan_red = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::cyan_red_track()),
                        balance.shadow_cyan_red,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-shadow-magenta-green", "Magenta / Green", |s| s.color_balance.shadow_magenta_green, |s, value| s.color_balance.shadow_magenta_green = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::magenta_green_track()),
                        balance.shadow_magenta_green,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-shadow-yellow-blue", "Yellow / Blue", |s| s.color_balance.shadow_yellow_blue, |s, value| s.color_balance.shadow_yellow_blue = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::yellow_blue_track()),
                        balance.shadow_yellow_blue,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(headline("Midtones").into_any_element());
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-mid-cyan-red", "Cyan / Red", |s| s.color_balance.mid_cyan_red, |s, value| s.color_balance.mid_cyan_red = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::cyan_red_track()),
                        balance.mid_cyan_red,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-mid-magenta-green", "Magenta / Green", |s| s.color_balance.mid_magenta_green, |s, value| s.color_balance.mid_magenta_green = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::magenta_green_track()),
                        balance.mid_magenta_green,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-mid-yellow-blue", "Yellow / Blue", |s| s.color_balance.mid_yellow_blue, |s, value| s.color_balance.mid_yellow_blue = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::yellow_blue_track()),
                        balance.mid_yellow_blue,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(headline("Highlights").into_any_element());
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-highlight-cyan-red", "Cyan / Red", |s| s.color_balance.highlight_cyan_red, |s, value| s.color_balance.highlight_cyan_red = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::cyan_red_track()),
                        balance.highlight_cyan_red,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-highlight-magenta-green", "Magenta / Green", |s| s.color_balance.highlight_magenta_green, |s, value| s.color_balance.highlight_magenta_green = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::magenta_green_track()),
                        balance.highlight_magenta_green,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-balance-highlight-yellow-blue", "Yellow / Blue", |s| s.color_balance.highlight_yellow_blue, |s, value| s.color_balance.highlight_yellow_blue = value, ColorBalanceSettings::RANGE, "", 0, false)
                            .track(Self::yellow_blue_track()),
                        balance.highlight_yellow_blue,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    Self::switch_row(
                        "filter-balance-preserve-luminosity",
                        "Preserve Luminosity",
                        Some("Put each pixel's brightness back afterwards, so only the color moves"),
                        balance.preserve_luminosity,
                        committing,
                        |settings, value| settings.color_balance.preserve_luminosity = value,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::Grain => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-grain-amount", "Amount", |s| s.grain.amount, |s, value| s.grain.amount = value, GrainSettings::AMOUNT_RANGE, "", 0, false),
                        settings.grain.amount,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-grain-size", "Size", |s| s.grain.size, |s, value| s.grain.size = value, GrainSettings::SIZE_RANGE, "px", 1, true),
                        settings.grain.size,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-grain-roughness", "Roughness", |s| s.grain.roughness, |s, value| s.grain.roughness = value, GrainSettings::ROUGHNESS_RANGE, "", 0, false),
                        settings.grain.roughness,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::RemoveBackground => {
                rows.push(
                    paragraph(
                        "Hide the background behind a layer mask, keeping the foreground subjects. The pixels stay, so the background can be painted back at any time.",
                        CALLOUT_SIZE,
                        SECONDARY,
                    )
                    .into_any_element(),
                );
                rows.push(
                    div()
                        .id("filter-background-quality")
                        .tooltip(|window, cx| {
                            Tooltip::new("Basic is quick; Advanced refines the mask against the layer's own detail, for hair and fur").build(window, cx)
                        })
                        .child(
                            segmented_picker(
                                "filter-background-quality-picker",
                                BackgroundQuality::ALL
                                    .into_iter()
                                    .map(|quality| (quality, quality.raw_value())),
                                settings.background_quality,
                                {
                                    let entity = cx.entity();
                                    move |value, _, cx| {
                                        entity.update(cx, |sheet, cx| {
                                            sheet.update(|settings| settings.background_quality = value, cx);
                                        });
                                    }
                                },
                            )
                            .disabled(committing),
                        )
                        .into_any_element(),
                );
                if settings.background_quality == BackgroundQuality::Advanced {
                    rows.push(
                        self.control(
                            ControlSpec::new("filter-refine-edges", "Refine", |s| s.refine_edges, |s, value| s.refine_edges = value, (0.0, 40.0), "px", 0, false)
                                .help("Pull the mask onto the image's own edges, which recovers hair and fur"),
                            settings.refine_edges,
                            committing,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                    rows.push(
                        self.control(
                            ControlSpec::new("filter-matte-contrast", "Contrast", |s| s.matte_contrast, |s, value| s.matte_contrast = value, (0.0, 100.0), "%", 0, false)
                                .help("Clear the haze that leaves background showing through thin areas"),
                            settings.matte_contrast,
                            committing,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                    rows.push(
                        self.control(
                            ControlSpec::new("filter-shift-edge", "Shift Edge", |s| s.shift_edge, |s, value| s.shift_edge = value, (-10.0, 10.0), "px", 0, false)
                                .help("Shrink the mask to drop the rim of background color around the subject, or grow it"),
                            settings.shift_edge,
                            committing,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    );
                }
            }
            FilterKind::ContentAwareFill => {
                rows.push(paragraph("Fill the selection using surrounding pixels from this layer.", CALLOUT_SIZE, SECONDARY).into_any_element());
            }
            FilterKind::GaussianBlur => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-radius", "Radius", |s| s.radius, |s, value| s.radius = value, (0.1, 250.0), "px", 1, true),
                        settings.radius,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::MotionBlur => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-angle", "Angle", |s| s.angle, |s, value| s.angle = value, (-90.0, 90.0), "°", 0, false),
                        settings.angle,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-distance", "Distance", |s| s.distance, |s, value| s.distance = value, (1.0, 2000.0), "px", 0, true),
                        settings.distance,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::AddNoise => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-noise-amount", "Amount", |s| s.amount, |s, value| s.amount = value, (0.1, 400.0), "%", 1, true),
                        settings.amount,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    h_flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(div().text_size(px(LABEL_SIZE)).child("Distribution"))
                        .child(
                            segmented_picker(
                                "filter-noise-distribution",
                                [(false, "Uniform"), (true, "Gaussian")],
                                settings.gaussian,
                                {
                                    let entity = cx.entity();
                                    move |value, _, cx| {
                                        entity.update(cx, |sheet, cx| {
                                            sheet.set_flag(|settings, value| settings.gaussian = value, value, cx);
                                        });
                                    }
                                },
                            )
                            .disabled(committing),
                        )
                        .into_any_element(),
                );
                rows.push(
                    Self::switch_row(
                        "filter-noise-monochromatic",
                        "Monochromatic",
                        None,
                        settings.monochromatic,
                        committing,
                        |settings, value| settings.monochromatic = value,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::Dither => rows.extend(self.dither_controls(&settings, committing, window, cx)),
            FilterKind::Vignette => {
                let session = self.session.clone();
                rows.push(
                    h_flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(VIGNETTE_LABEL_WIDTH)).text_size(px(LABEL_SIZE)).child("Color"))
                        .child(Self::swatch(
                            element_id("filter-vignette", "color"),
                            settings.vignette_color,
                            "Choose the vignette color",
                            None,
                            committing,
                            move |_, cx| {
                                session.update(cx, |session, _| session.open_vignette_color_picker());
                            },
                        ))
                        .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-vignette-amount", "Amount", |s| s.vignette_amount, |s, value| s.vignette_amount = value, (0.0, 100.0), "%", 0, false)
                            .help("Blend the chosen color into the edges while keeping the center unchanged"),
                        settings.vignette_amount,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-vignette-midpoint", "Midpoint", |s| s.vignette_midpoint, |s, value| s.vignette_midpoint = value, (0.0, 100.0), "%", 0, false),
                        settings.vignette_midpoint,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-vignette-roundness", "Roundness", |s| s.vignette_roundness, |s, value| s.vignette_roundness = value, (-100.0, 100.0), "", 0, false),
                        settings.vignette_roundness,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-vignette-feather", "Feather", |s| s.vignette_feather, |s, value| s.vignette_feather = value, (0.0, 100.0), "%", 0, false),
                        settings.vignette_feather,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-vignette-highlights", "Highlights", |s| s.vignette_highlights, |s, value| s.vignette_highlights = value, (0.0, 100.0), "%", 0, false)
                            .help("Protect bright areas near the edge"),
                        settings.vignette_highlights,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::BloomGlow => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-bloom-amount", "Amount", |s| s.bloom_amount, |s, value| s.bloom_amount = value, (0.0, 100.0), "%", 0, false),
                        settings.bloom_amount,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-bloom-radius", "Radius", |s| s.bloom_radius, |s, value| s.bloom_radius = value, (1.0, 150.0), "px", 0, true),
                        settings.bloom_radius,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::TonalContrast => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-tonal-amount", "Amount", |s| s.tonal_amount, |s, value| s.tonal_amount = value, (0.0, 100.0), "%", 0, false),
                        settings.tonal_amount,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-tonal-shadows", "Shadows", |s| s.tonal_shadows, |s, value| s.tonal_shadows = value, (-100.0, 100.0), "%", 0, false),
                        settings.tonal_shadows,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-tonal-midtones", "Midtones", |s| s.tonal_midtones, |s, value| s.tonal_midtones = value, (-100.0, 100.0), "%", 0, false),
                        settings.tonal_midtones,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-tonal-highlights", "Highlights", |s| s.tonal_highlights, |s, value| s.tonal_highlights = value, (-100.0, 100.0), "%", 0, false),
                        settings.tonal_highlights,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.control(
                        ControlSpec::new("filter-tonal-radius", "Radius", |s| s.tonal_radius, |s, value| s.tonal_radius = value, (1.0, 100.0), "px", 0, true),
                        settings.tonal_radius,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            FilterKind::LensCorrection => {
                rows.push(
                    self.control(
                        ControlSpec::new("filter-distortion", "Remove Distortion", |s| s.distortion, |s, value| s.distortion = value, (-100.0, 100.0), "", 0, false),
                        settings.distortion,
                        committing,
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    paragraph(
                        "Positive straightens lines that bow outward (barrel); negative, lines that bow inward (pincushion).",
                        CALLOUT_SIZE,
                        SECONDARY,
                    )
                    .into_any_element(),
                );
            }
        }

        let mut content = v_flex()
            .items_start()
            .gap(px(SPACING))
            .p(px(PADDING))
            .w(px(self.width(kind)))
            .children(rows);

        // `Toggle("Preview", isOn: Binding(get: { edit?.preview ?? true }, set: …))`.
        content = content.child(
            h_flex().w_full().items_center().child(
                Switch::new("filter-preview")
                    .label("Preview")
                    .accessibility_label("Preview")
                    .checked(preview)
                    .disabled(committing)
                    .on_change({
                        let entity = cx.entity();
                        move |value, _, cx| {
                            let value = *value;
                            entity.update(cx, |sheet, cx| sheet.set_preview(value, cx));
                        }
                    }),
            ),
        );
        if let Some(error) = preview_error {
            content = content.child(
                div()
                    .text_size(px(CALLOUT_SIZE))
                    .text_color(ORANGE)
                    .child(error),
            );
        }
        if limited {
            content = content.child(
                div()
                    .text_size(px(CALLOUT_SIZE))
                    .text_color(SECONDARY)
                    .child("Limited to the selection"),
            );
        }
        content = content.child(Separator::horizontal());

        // While the preview is being worked out (Remove Background's mask, Content-Aware Fill) OK
        // waits, so the panel says what it is waiting for rather than showing a disabled button and
        // nothing else. Only the slow filters say so: a quick preview (Dither, a blur) toggling this
        // at every slider step would make the panel flicker as it grows and shrinks.
        let working = committing || (preparing && kind.is_automatic());
        let mut footer = h_flex()
            .w_full()
            .items_center()
            .gap(px(FOOTER_SPACING))
            .child(
                Button::new("filter-cancel")
                    .label("Cancel")
                    .disabled(committing)
                    .on_click({
                        let entity = cx.entity();
                        move |_, _, cx| {
                            entity.update(cx, |sheet, cx| {
                                sheet.session.update(cx, |session, cx| {
                                    session.cancel_filter();
                                    cx.notify();
                                });
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(div().flex_1());
        if working {
            footer = footer.child(Spinner::new().small()).child(
                div()
                    .text_size(px(CALLOUT_SIZE))
                    .text_color(SECONDARY)
                    .child(if committing { "Applying…" } else { "Working…" }),
            );
        }
        footer = footer.child(
            Button::new("filter-ok")
                .label("OK")
                .primary()
                .disabled(kind.is_automatic() && (preparing || self.preview_error(cx).is_some()))
                .on_click({
                    let entity = cx.entity();
                    move |_, _, cx| {
                        entity.update(cx, |sheet, cx| {
                            sheet.session.update(cx, |session, cx| {
                                session.commit_filter();
                                cx.notify();
                            });
                            cx.notify();
                        });
                    }
                }),
        );
        content = content.child(footer);

        content
            // `.disabled(edit?.committing == true)`.
            .when(committing, |this| this.opacity(0.5))
            .when(is_camera_raw, |this| this.h_full())
            .when(!is_camera_raw, |this| this.flex_none())
    }
}

/// A paragraph of the panel's own wording: `.fixedSize(horizontal: false, vertical: true)`, so it
/// wraps to the panel's width.
fn paragraph(text: &str, size: f32, color: Hsla) -> Div {
    div().text_size(px(size)).text_color(color).child(text.to_string())
}

/// `Text("Shadows").font(.headline)`: a Color Balance section's name.
fn headline(text: &'static str) -> Div {
    div()
        .text_size(px(HEADLINE_SIZE))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text)
}

/// `GradientMapControls`: Gradient Map's two colors, the gradient they make, and Reverse. The
/// colors are swatches like the tool rail's, and open the app's own color picker.
struct GradientMapControls {
    session: Entity<EditorSession>,
}

impl GradientMapControls {
    fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }

    /// `settings.gradientMap`.
    fn settings(&self, cx: &App) -> compositor_rs_core::layer_adjustment::GradientMapSettings {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.settings.gradient_map)
            .unwrap_or_default()
    }

    /// `pick(_:)`: the app's color picker on one end.
    fn pick(&mut self, highlights: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, _| session.open_gradient_map_color_picker(highlights));
        cx.notify();
    }

    /// `$settings.reversed`.
    fn set_reversed(&mut self, value: bool, cx: &mut Context<Self>) {
        update_filter_settings(&self.session, cx, |settings| settings.gradient_map.reversed = value);
        cx.notify();
    }

    /// `swatch(_:_:action:)`: the color, its name, and the picker.
    fn swatch(&self, title: &'static str, value: AdjustmentColor, highlights: bool, disabled: bool, cx: &mut Context<Self>) -> AnyElement {
        // `.help("Choose the \(title.lowercased()) color")` and `.accessibilityLabel("\(title) color")`.
        let help: &'static str = if highlights {
            "Choose the highlights color"
        } else {
            "Choose the shadows color"
        };
        let label: &'static str = if highlights {
            "Highlights color"
        } else {
            "Shadows color"
        };
        let entity = cx.entity();
        let swatch = div()
            .id(element_id("filter-gradient-map", title))
            .flex_none()
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .aria_label(label)
            .tooltip(move |window, cx| Tooltip::new(help).build(window, cx))
            .when(!disabled, |this| {
                this.on_click(move |_, _, cx| {
                    entity.update(cx, |controls, cx| controls.pick(highlights, cx));
                })
            })
            .child(swatch_shape(value));
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .child(swatch)
            .child(div().text_size(px(LABEL_SIZE)).child(title))
            .into_any_element()
    }
}

impl Render for GradientMapControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings(cx);
        let committing = self
            .session
            .read(cx)
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.committing);
        let entity = cx.entity();
        let (dark, light) = settings.ends();
        v_flex()
            .items_start()
            .gap(px(GROUP_SPACING))
            .child(
                // `LinearGradient(colors: [color(ends.dark), color(ends.light)], startPoint: .leading, …)`.
                div()
                    .h(px(GRADIENT_HEIGHT))
                    .w_full()
                    .rounded(px(GRADIENT_RADIUS))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 0.0, 0.35))
                    .bg(linear_gradient(
                        90.0,
                        linear_color_stop(adjustment_hsla(dark), 0.0),
                        linear_color_stop(adjustment_hsla(light), 1.0),
                    )),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(20.0))
                    .child(self.swatch("Shadows", settings.shadows, false, committing, cx))
                    .child(self.swatch("Highlights", settings.highlights, true, committing, cx)),
            )
            .child(
                Switch::new("filter-gradient-map-reverse")
                    .label("Reverse")
                    .accessibility_label("Reverse")
                    .checked(settings.reversed)
                    .disabled(committing)
                    .on_change(move |value, _, cx| {
                        let value = *value;
                        entity.update(cx, |controls, cx| controls.set_reversed(value, cx));
                    }),
            )
    }
}

/// `CurvesControls`: the channel menu, the tone curve editor, its readout, and the two buttons
/// (port of `UI/CurvesControls.swift`).
struct CurvesControls {
    session: Entity<EditorSession>,
    /// `@State private var selected: Int?`: the handle the readout and Remove point act on.
    selected: Option<usize>,
    /// `@State private var dragging: Int?`: the handle the current drag holds.
    dragging: Option<usize>,
    /// The channel the previous frame drew, for `.onChange(of: settings.channel)`.
    channel: LevelsChannel,
    /// The editor's rectangle, recorded while it paints so a drag reads a position from it.
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl CurvesControls {
    fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let channel = session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.settings.curves.channel)
            .unwrap_or(LevelsChannel::Rgb);
        Self {
            session,
            selected: None,
            dragging: None,
            channel,
            bounds: Rc::new(Cell::new(Bounds::default())),
        }
    }

    /// `settings.curves`.
    fn settings(&self, cx: &App) -> CurvesSettings {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.settings.curves.clone())
            .unwrap_or_default()
    }

    /// `settings.channels[settings.channel.index] = points`.
    fn write_points(&mut self, channel: usize, points: Vec<CurvePoint>, cx: &mut Context<Self>) {
        update_filter_settings(&self.session, cx, |settings| settings.curves.channels[channel] = points);
        cx.notify();
    }

    /// `Picker("Channel")`: switching clears the selection and the drag.
    fn pick_channel(&mut self, channel: LevelsChannel, cx: &mut Context<Self>) {
        update_filter_settings(&self.session, cx, |settings| settings.curves.channel = channel);
        self.selected = None;
        self.dragging = None;
        cx.notify();
    }

    /// The drag gesture's `onChanged`, at a point of the editor.
    fn drag_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let element = self.bounds.get();
        let local_x = f32::from(position.x) - f32::from(element.left());
        let local_y = f32::from(position.y) - f32::from(element.top());
        let Some((x, y)) = curve_coordinates(
            local_x,
            local_y,
            f32::from(element.size.width),
            f32::from(element.size.height),
        ) else {
            return;
        };
        let settings = self.settings(cx);
        let channel = settings.channel.index();
        let mut points = settings.channels[channel].clone();
        if self.dragging.is_none() {
            // The nearest handle within 14, else a new one where the press landed.
            let nearest = points
                .iter()
                .enumerate()
                .map(|(index, point)| (index, ((point.x - x).powi(2) + (point.y - y).powi(2)).sqrt()))
                .min_by(|(_, left), (_, right)| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
                .filter(|(_, distance)| *distance < CURVE_HIT_RADIUS)
                .map(|(index, _)| index);
            if let Some(index) = nearest {
                self.dragging = Some(index);
            } else if points.len() < CURVE_MAX_POINTS
                && x > 1.0
                && x < 254.0
                && points.iter().all(|point| (point.x - x).abs() > CURVE_POINT_GAP)
            {
                points.push(CurvePoint { x, y });
                points.sort_by(|left, right| left.x.partial_cmp(&right.x).unwrap_or(std::cmp::Ordering::Equal));
                self.dragging = points.iter().position(|point| point.x == x);
            }
        }
        let Some(index) = self.dragging else {
            return;
        };
        if index >= points.len() {
            return;
        }
        self.selected = Some(index);
        points[index].y = y;
        if index > 0 && index < points.len() - 1 {
            points[index].x = (points[index + 1].x - CURVE_POINT_GAP).min((points[index - 1].x + CURVE_POINT_GAP).max(x));
        }
        self.write_points(channel, points, cx);
    }

    /// `Button("Remove point")`: the selected interior handle goes.
    fn remove_point(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected else {
            return;
        };
        let settings = self.settings(cx);
        let channel = settings.channel.index();
        let mut points = settings.channels[channel].clone();
        if selected > 0 && selected < points.len().saturating_sub(1) {
            points.remove(selected);
            self.write_points(channel, points, cx);
            self.selected = None;
        }
    }

    /// `Button("Reset curve")`: `[CurvePoint(x: 0, y: 0), CurvePoint(x: 255, y: 255)]`.
    fn reset_curve(&mut self, cx: &mut Context<Self>) {
        let channel = self.settings(cx).channel.index();
        self.write_points(
            channel,
            vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 255.0, y: 255.0 }],
            cx,
        );
        self.selected = None;
    }
}

impl Render for CurvesControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings(cx);
        // `.onChange(of: settings.channel) { _, _ in selected = nil; dragging = nil }`.
        if settings.channel != self.channel {
            self.channel = settings.channel;
            self.selected = None;
            self.dragging = None;
        }
        let committing = self
            .session
            .read(cx)
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.committing);
        let channel_index = settings.channel.index();
        let points = settings.channels[channel_index].clone();
        let handles = points.clone();
        let curve = settings.clone();
        let selected = self.selected;
        let accent = cx.theme().primary;
        let mono = cx.theme().mono_font_family.clone();
        let bounds = self.bounds.clone();

        let editor = {
            let entity = cx.entity();
            let down = {
                let entity = entity.clone();
                move |event: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                    entity.update(cx, |controls, cx| controls.drag_at(event.position, cx));
                }
            };
            let moved = {
                let entity = entity.clone();
                move |event: &MouseMoveEvent, _window: &mut Window, cx: &mut App| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    entity.update(cx, |controls, cx| controls.drag_at(event.position, cx));
                }
            };
            let up = {
                let entity = entity.clone();
                move |_event: &MouseUpEvent, _window: &mut Window, cx: &mut App| {
                    entity.update(cx, |controls, cx| {
                        controls.dragging = None;
                        cx.notify();
                    });
                }
            };
            let up_out = {
                let entity = entity.clone();
                move |_event: &MouseUpEvent, _window: &mut Window, cx: &mut App| {
                    entity.update(cx, |controls, cx| {
                        controls.dragging = None;
                        cx.notify();
                    });
                }
            };
            div()
                .id("filter-curves-editor")
                .relative()
                .w_full()
                .child(
                    canvas(
                        {
                            let bounds = bounds.clone();
                            move |element_bounds, _, _| bounds.set(element_bounds)
                        },
                        move |element_bounds, _, window, _| {
                            let width = f32::from(element_bounds.size.width);
                            let height = f32::from(element_bounds.size.height);
                            let position = |value: &CurvePoint| {
                                point(
                                    element_bounds.left() + px((value.x / 255.0) as f32 * width),
                                    element_bounds.top() + px((1.0 - value.y / 255.0) as f32 * height),
                                )
                            };
                            // `grid`: five lines each way, white at 12%.
                            let mut grid = PathBuilder::stroke(px(1.0));
                            for step in 0..=CURVE_GRID_STEPS {
                                let fraction = step as f32 / CURVE_GRID_STEPS as f32;
                                grid.move_to(point(
                                    element_bounds.left() + px(fraction * width),
                                    element_bounds.top(),
                                ));
                                grid.line_to(point(
                                    element_bounds.left() + px(fraction * width),
                                    element_bounds.bottom(),
                                ));
                                grid.move_to(point(
                                    element_bounds.left(),
                                    element_bounds.top() + px(fraction * height),
                                ));
                                grid.line_to(point(
                                    element_bounds.right(),
                                    element_bounds.top() + px(fraction * height),
                                ));
                            }
                            if let Ok(path) = grid.build() {
                                window.paint_path(path, hsla(0.0, 0.0, 1.0, 0.12));
                            }
                            // `line`: the channel's curve, white at 2 points.
                            let mut line = PathBuilder::stroke(px(2.0));
                            for x in 0..=255 {
                                let value = curve.value(f64::from(x), channel_index);
                                let handle = position(&CurvePoint { x: f64::from(x), y: value });
                                if x == 0 {
                                    line.move_to(handle);
                                } else {
                                    line.line_to(handle);
                                }
                            }
                            if let Ok(path) = line.build() {
                                window.paint_path(path, OPAQUE_WHITE);
                            }
                            for (index, handle) in handles.iter().enumerate() {
                                let center = position(handle);
                                let mut quad = fill(
                                    Bounds {
                                        origin: point(
                                            center.x - px(CURVE_POINT_SIZE / 2.0),
                                            center.y - px(CURVE_POINT_SIZE / 2.0),
                                        ),
                                        size: size(px(CURVE_POINT_SIZE), px(CURVE_POINT_SIZE)),
                                    },
                                    if selected == Some(index) { accent } else { OPAQUE_WHITE },
                                );
                                quad.corner_radii = Corners {
                                    top_left: px(CURVE_POINT_SIZE / 2.0),
                                    top_right: px(CURVE_POINT_SIZE / 2.0),
                                    bottom_right: px(CURVE_POINT_SIZE / 2.0),
                                    bottom_left: px(CURVE_POINT_SIZE / 2.0),
                                };
                                window.paint_quad(quad);
                            }
                        },
                    )
                    .h(px(CURVE_HEIGHT))
                    .w_full()
                    .bg(hsla(0.0, 0.0, 0.0, 0.35)),
                )
                .on_mouse_down(MouseButton::Left, down)
                .on_mouse_move(moved)
                .on_mouse_up(MouseButton::Left, up)
                .on_mouse_up_out(MouseButton::Left, up_out)
        };

        let channel_picker = menu_picker(
            "filter-curves-channel",
            LevelsChannel::ALL
                .into_iter()
                .map(|channel| (channel, channel.raw_value())),
            settings.channel,
            {
                let entity = cx.entity();
                move |value, _, cx| {
                    entity.update(cx, |controls, cx| controls.pick_channel(value, cx));
                }
            },
        );

        let selected_point = self.selected.filter(|index| *index < points.len()).map(|index| points[index]);
        let remove_enabled = self.selected.is_some_and(|selected| {
            selected > 0 && selected < points.len().saturating_sub(1)
        }) && !committing;
        let mut readout = h_flex().w_full().items_center().gap(px(FOOTER_SPACING));
        if let Some(selected_point) = selected_point {
            // `.monospacedDigit()`: gpui has no tabular-digit switch, so the readout uses the
            // theme's monospaced face.
            readout = readout.child(
                div()
                    .text_size(px(LABEL_SIZE))
                    .font_family(mono.clone())
                    .child(format!(
                        "Input {} · Output {}",
                        selected_point.x as i64, selected_point.y as i64
                    )),
            );
        }
        readout = readout
            .child(div().flex_1())
            .child(
                Button::new("filter-curves-remove-point")
                    .label("Remove point")
                    .disabled(!remove_enabled)
                    .on_click({
                        let entity = cx.entity();
                        move |_, _, cx| {
                            entity.update(cx, |controls, cx| controls.remove_point(cx));
                        }
                    }),
            );

        v_flex()
            .items_start()
            .gap(px(GROUP_SPACING))
            .w_full()
            .child(channel_picker.disabled(committing))
            .child(editor)
            .child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(SECONDARY)
                    .child("Click to add a point. Drag to adjust."),
            )
            .child(readout)
            .child(
                Button::new("filter-curves-reset")
                    .label("Reset curve")
                    .disabled(committing)
                    .on_click({
                        let entity = cx.entity();
                        move |_, _, cx| {
                            entity.update(cx, |controls, cx| controls.reset_curve(cx));
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use compositor_rs_core::image_ops::{BackgroundQuality, DitherSettings, DitherStyle, FilterKind, FilterSettings};

    use crate::widgets::gradient_slider::{srgb, CameraRawSliderTrack};

    use super::{curve_coordinates, shown_titles, slider_shows, step_of, FilterSheet};

    #[test]
    fn a_control_step_is_the_decimals_power_of_ten() {
        assert_eq!(step_of(0), 1.0);
        assert_eq!(step_of(1), 10.0);
        assert_eq!(step_of(2), 100.0);
        assert_eq!(step_of(4), 10_000.0);
        // `(value * step).rounded() / step` at two decimals.
        assert_eq!((0.1234 * step_of(2)).round() / step_of(2), 0.12);
        assert_eq!((0.125 * step_of(2)).round() / step_of(2), 0.13);
    }

    #[test]
    fn resetting_puts_one_setting_back_to_its_filter_default() {
        let mut settings = FilterSettings::default();
        settings.radius = 40.0;
        settings.dither.pixel_size = 9.0;
        let reset = FilterSheet::resetting(&settings, |settings| settings.radius, |settings, value| {
            settings.radius = value
        });
        assert_eq!(reset.radius, 1.0);
        assert_eq!(reset.dither.pixel_size, 9.0);
        let reset = FilterSheet::resetting(
            &settings,
            |settings| settings.dither.pixel_size,
            |settings, value| settings.dither.pixel_size = value,
        );
        assert_eq!(reset.dither.pixel_size, 2.0);
        assert_eq!(reset.radius, 40.0);
    }

    #[test]
    fn the_label_list_follows_what_the_kind_shows() {
        let settings = FilterSettings::default();
        assert_eq!(shown_titles(FilterKind::GaussianBlur, &settings), vec!["Radius"]);
        assert_eq!(
            shown_titles(FilterKind::MotionBlur, &settings),
            vec!["Angle", "Distance"]
        );
        assert_eq!(shown_titles(FilterKind::CameraRaw, &settings), Vec::<&str>::new());
        assert_eq!(shown_titles(FilterKind::Curves, &settings), Vec::<&str>::new());
        // Black & White measures its six families, and Hue and Saturation once Tint is on.
        let mut tinted = FilterSettings::default();
        assert_eq!(shown_titles(FilterKind::BlackWhite, &tinted).len(), 6);
        tinted.black_white.tint = true;
        assert_eq!(
            shown_titles(FilterKind::BlackWhite, &tinted),
            vec!["Reds", "Yellows", "Greens", "Cyans", "Blues", "Magentas", "Hue", "Saturation"]
        );
        // Remove Background only measures the Advanced controls.
        assert_eq!(shown_titles(FilterKind::RemoveBackground, &settings), Vec::<&str>::new());
        let mut advanced = FilterSettings::default();
        advanced.background_quality = BackgroundQuality::Advanced;
        assert_eq!(
            shown_titles(FilterKind::RemoveBackground, &advanced),
            vec!["Refine", "Contrast", "Shift Edge"]
        );
        // Dither's controls follow the style: Atkinson diffuses and has tones.
        assert_eq!(
            shown_titles(FilterKind::Dither, &settings),
            vec!["Pixel Size", "Tones", "Diffusion", "Density", "Contrast"]
        );
        let ascii = FilterSettings {
            dither: DitherSettings {
                style: DitherStyle::Ascii,
                ..DitherSettings::default()
            },
            ..FilterSettings::default()
        };
        assert_eq!(
            shown_titles(FilterKind::Dither, &ascii),
            vec!["Text Size", "Density", "Contrast"]
        );
        let scanlines = FilterSettings {
            dither: DitherSettings {
                style: DitherStyle::Scanlines,
                ..DitherSettings::default()
            },
            ..FilterSettings::default()
        };
        assert_eq!(
            shown_titles(FilterKind::Dither, &scanlines),
            vec!["Line Spacing", "Glow", "Dots", "Wobble", "Density", "Contrast"]
        );
        let halftone = FilterSettings {
            dither: DitherSettings {
                style: DitherStyle::Dots,
                ..DitherSettings::default()
            },
            ..FilterSettings::default()
        };
        assert_eq!(
            shown_titles(FilterKind::Dither, &halftone),
            vec!["Pixel Size", "Cell Size", "Angle", "Density", "Contrast"]
        );
    }

    #[test]
    fn a_colored_track_carries_the_swift_colors() {
        assert_eq!(
            FilterSheet::cyan_red_track(),
            CameraRawSliderTrack::Opposing(srgb(0.10, 0.72, 0.80), srgb(0.86, 0.18, 0.20))
        );
        assert_eq!(
            FilterSheet::magenta_green_track(),
            CameraRawSliderTrack::Opposing(srgb(0.80, 0.22, 0.70), srgb(0.24, 0.70, 0.30))
        );
        assert_eq!(
            FilterSheet::yellow_blue_track(),
            CameraRawSliderTrack::Opposing(srgb(0.95, 0.82, 0.18), srgb(0.22, 0.40, 0.92))
        );
    }

    #[test]
    fn a_slider_and_its_setting_agree_to_the_quantum() {
        let step = step_of(1);
        assert!(slider_shows(1.5, 1.5, step));
        // f32 round-tripping of the slider's own quantization still reads as the same value.
        assert!(slider_shows(1.6000001, 1.6, step));
        assert!(!slider_shows(1.2, 1.5, step));
    }

    #[test]
    fn the_curve_editor_maps_a_pointer_into_the_curve() {
        // The editor's bottom-left corner is black in, black out.
        assert_eq!(curve_coordinates(0.0, 200.0, 200.0, 200.0), Some((0.0, 0.0)));
        // Its top-right corner is white in, white out.
        assert_eq!(curve_coordinates(200.0, 0.0, 200.0, 200.0), Some((255.0, 255.0)));
        // The middle is the middle.
        assert_eq!(curve_coordinates(100.0, 100.0, 200.0, 200.0), Some((127.5, 127.5)));
        // A pointer outside the editor still lands on the curve's edge.
        assert_eq!(curve_coordinates(-40.0, 260.0, 200.0, 200.0), Some((0.0, 0.0)));
        // And an editor that has not been laid out yet gives nothing.
        assert_eq!(curve_coordinates(10.0, 10.0, 0.0, 200.0), None);
    }
}

