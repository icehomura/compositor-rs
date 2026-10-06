//! Image Size: the modal that resamples the document's layers to new pixel dimensions and sets its
//! print resolution (port of `UI/ImageSizeSheet.swift`).
//!
//! The sheet only collects the options; the caller runs `EditorSession::change_image_size` with
//! them, as the Swift `ProjectController.imageSize` did around the hosted view.

use compositor_rs_core::canvas_size::CanvasUnit;
use compositor_rs_core::layer_transform::LayerSampling;
use compositor_rs_core::limits::{max_surface_megapixels, MAX_SIDE, MAX_SIDE_EXTENT, MAX_SURFACE_EXTENT};
use compositor_rs_session::canvas_ops::ImageSizeOptions;
use compositor_rs_session::EditorSession;

use crate::tool_controls::{format_number, menu_picker, parse_number};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::{h_flex, v_flex, WindowExt as _};
use gpui_kit::*;

/// The sheet's content width, `.frame(width: 430)`.
const WIDTH: f32 = 430.0;
/// The component dialog's own padding, 16 points on each side.
const DIALOG_PADDING: f32 = 16.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The `VStack(alignment: .leading, spacing: 18)`.
const SPACING: f32 = 18.0;
/// A size label's `frame(width: 75, alignment: .leading)`.
const LABEL_WIDTH: f32 = 75.0;
/// The Resolution field's own limits: `(1...9600).contains(resolution)` and its
/// `scrubbable(range: 1...9600, step: 1)`.
const RESOLUTION_RANGE: (f64, f64) = (1.0, 9600.0);
/// The error orange the Swift's `.orange` is (SwiftUI orange, #FF9500).
const ORANGE: Hsla = hsla(0.075, 1.0, 0.5, 1.0);

/// What the sheet hands back when it closes (`finish(ImageSizeOptions?)`).
pub type ImageSizeFinish = Box<dyn FnOnce(Option<ImageSizeOptions>, &mut Window, &mut App)>;

/// Which of the sheet's three fields a value belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Width,
    Height,
    Resolution,
}

impl Field {
    /// The field's label.
    fn label(self) -> &'static str {
        match self {
            Field::Width => "Width",
            Field::Height => "Height",
            Field::Resolution => "Resolution",
        }
    }

    /// The field's element id.
    fn id(self) -> &'static str {
        match self {
            Field::Width => "image-size-width",
            Field::Height => "image-size-height",
            Field::Resolution => "image-size-resolution",
        }
    }
}

/// One field's text box and the text the last frame saw of it, as the port's other property fields
/// keep them.
struct FieldState {
    input: Entity<InputState>,
    last_text: String,
}

/// Image Size's own editable state (`@State private var width/height/resolution/lastResolution/
/// locked/resample/unit/sampling`) with the Swift view's own math.
#[derive(Clone, Debug, PartialEq)]
struct ImageSizeState {
    original_width: usize,
    original_height: usize,
    width: f64,
    height: f64,
    resolution: f64,
    /// The last usable resolution. Print sizes scale from it, so passing through a zero or negative
    /// entry doesn't lose them.
    last_resolution: f64,
    locked: bool,
    resample: bool,
    unit: CanvasUnit,
    sampling: LayerSampling,
}

impl ImageSizeState {
    /// `init(document:finish:)`: the document's pixels and resolution, aspect locked, high-quality
    /// sampling.
    fn new(width: usize, height: usize, resolution: f64) -> Self {
        Self {
            original_width: width,
            original_height: height,
            width: width as f64,
            height: height as f64,
            resolution,
            last_resolution: resolution,
            locked: true,
            resample: true,
            unit: CanvasUnit::Pixels,
            sampling: LayerSampling::High,
        }
    }

    /// `valid`: a usable resolution and pixel size, and — while resampling — a surface inside the
    /// limit.
    fn valid(&self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.resolution.is_finite()
            && (RESOLUTION_RANGE.0..=RESOLUTION_RANGE.1).contains(&self.resolution)
            && (1.0..=MAX_SIDE_EXTENT).contains(&self.width.round())
            && (1.0..=MAX_SIDE_EXTENT).contains(&self.height.round())
            && (!self.resample || self.width.round() * self.height.round() <= MAX_SURFACE_EXTENT)
    }

    /// `display(_:original:)`: the value the field shows for a pixel count.
    fn display(&self, pixels: f64, original: f64) -> f64 {
        match self.unit {
            CanvasUnit::Percent => pixels / original * 100.0,
            CanvasUnit::Inches => pixels / self.resolution,
            CanvasUnit::Centimeters => pixels / self.resolution * 2.54,
            CanvasUnit::Pixels => pixels,
        }
    }

    /// `dimension(isWidth:)`'s get.
    fn displayed(&self, is_width: bool) -> f64 {
        if is_width {
            self.display(self.width, self.original_width as f64)
        } else {
            self.display(self.height, self.original_height as f64)
        }
    }

    /// `dimension(isWidth:)`'s set: a typed value lands in the unit's own scale, the aspect lock
    /// moves the other side, and without resampling the value moves the resolution instead.
    fn set_dimension(&mut self, value: f64, is_width: bool) {
        if !value.is_finite() || value <= 0.0 {
            return;
        }
        if matches!(self.unit, CanvasUnit::Inches | CanvasUnit::Centimeters)
            && !(self.resolution.is_finite() && self.resolution > 0.0)
        {
            return;
        }
        if !self.resample {
            let pixels = if is_width { self.width } else { self.height };
            let multiplier = if self.unit == CanvasUnit::Centimeters {
                2.54
            } else {
                1.0
            };
            self.resolution = pixels / value * multiplier;
            // The Resolution field's `onChange` sees the new value too: the last usable one follows.
            if self.resolution.is_finite() && self.resolution > 0.0 {
                self.last_resolution = self.resolution;
            }
            return;
        }
        let original = if is_width {
            self.original_width
        } else {
            self.original_height
        } as f64;
        let pixels = match self.unit {
            CanvasUnit::Percent => value / 100.0 * original,
            CanvasUnit::Inches => value * self.resolution,
            CanvasUnit::Centimeters => value / 2.54 * self.resolution,
            CanvasUnit::Pixels => value,
        };
        if is_width {
            if self.locked {
                self.height = pixels * self.height / self.width;
            }
            self.width = pixels;
        } else {
            if self.locked {
                self.width = pixels * self.width / self.height;
            }
            self.height = pixels;
        }
    }

    /// The Resolution field's `.onChange(of: resolution)`: print sizes scale with the resolution
    /// while resampling, and the last usable value follows it. The binding itself takes any
    /// formatter output, so the field's own value is written before the guard.
    fn set_resolution(&mut self, value: f64) {
        self.resolution = value;
        if !value.is_finite() || value <= 0.0 {
            return;
        }
        if self.resample && matches!(self.unit, CanvasUnit::Inches | CanvasUnit::Centimeters) {
            self.width *= value / self.last_resolution;
            self.height *= value / self.last_resolution;
        }
        self.last_resolution = value;
        self.resolution = value;
    }

    /// `Toggle("Resample", isOn: $resample)`'s `onChange`: turning it off restores the document's
    /// pixels, locks the aspect and moves to a print unit.
    fn set_resample(&mut self, enabled: bool) {
        self.resample = enabled;
        if !enabled {
            self.width = self.original_width as f64;
            self.height = self.original_height as f64;
            self.locked = true;
            if matches!(self.unit, CanvasUnit::Pixels | CanvasUnit::Percent) {
                self.unit = CanvasUnit::Inches;
            }
        }
    }

    /// `canScrubDimensions`: a print unit needs a usable resolution to scrub at all.
    fn can_scrub_dimensions(&self) -> bool {
        !matches!(self.unit, CanvasUnit::Inches | CanvasUnit::Centimeters)
            || (self.resolution.is_finite() && self.resolution > 0.0)
    }

    /// `scrubRange(isWidth:)`: the values the drag may land on, in the field's own unit.
    fn scrub_range(&self, is_width: bool) -> (f64, f64) {
        if !self.can_scrub_dimensions() {
            return (0.0, 0.0);
        }
        let pixels = if is_width { self.width } else { self.height };
        let other = if is_width { self.height } else { self.width };
        let original = if is_width {
            self.original_width
        } else {
            self.original_height
        } as f64;
        if !self.resample {
            let multiplier = if self.unit == CanvasUnit::Centimeters {
                2.54
            } else {
                1.0
            };
            return (
                pixels * multiplier / 9600.0,
                pixels * multiplier,
            );
        }
        let minimum = if self.locked {
            1.0f64.max(pixels / other)
        } else {
            1.0
        };
        let dimension_limit = if self.locked {
            30_000.0f64.min(30_000.0 * pixels / other)
        } else {
            30_000.0
        };
        let area_limit = if self.locked {
            (100_000_000.0 * pixels / other).sqrt()
        } else {
            100_000_000.0 / other
        };
        let maximum = minimum.max(dimension_limit.min(area_limit));
        (
            self.display(minimum, original),
            self.display(maximum, original),
        )
    }

    /// `scrubSensitivity(isWidth:)`: how far one point of drag travel moves the field's own unit.
    fn scrub_sensitivity(&self, is_width: bool) -> f64 {
        if !self.can_scrub_dimensions() {
            return 0.0;
        }
        if !self.resample {
            return if self.unit == CanvasUnit::Centimeters {
                0.0254
            } else {
                0.01
            };
        }
        let original = if is_width {
            self.original_width
        } else {
            self.original_height
        } as f64;
        match self.unit {
            CanvasUnit::Percent => 100.0 / original,
            CanvasUnit::Inches => 1.0 / self.resolution,
            CanvasUnit::Centimeters => 2.54 / self.resolution,
            CanvasUnit::Pixels => 1.0,
        }
    }

    /// The units the picker offers
    /// (`units.filter { resample || ($0 != "Pixels" && $0 != "Percent") }`).
    fn units(&self) -> Vec<(CanvasUnit, &'static str)> {
        CanvasUnit::ALL
            .into_iter()
            .filter(|unit| {
                self.resample || !matches!(unit, CanvasUnit::Pixels | CanvasUnit::Percent)
            })
            .map(|unit| (unit, unit.raw_value()))
            .collect()
    }
}

/// The Image Size sheet's state: the fields and what the current switches describe.
pub struct ImageSizeSheet {
    session: Entity<EditorSession>,
    state: ImageSizeState,
    /// The three fields, built on the first render — the first time a window is available — as the
    /// Swift's `@State` bindings are their own storage.
    width_field: Option<FieldState>,
    height_field: Option<FieldState>,
    resolution_field: Option<FieldState>,
    finish: Option<ImageSizeFinish>,
}

impl ImageSizeSheet {
    /// A fresh sheet with the Swift's `@State` defaults; the document's size and resolution are
    /// read from the session by [`Self::open`] (`init(document:finish:)`).
    pub fn new(session: Entity<EditorSession>, finish: ImageSizeFinish) -> Self {
        Self {
            session,
            state: ImageSizeState::new(1, 1, 72.0),
            width_field: None,
            height_field: None,
            resolution_field: None,
            finish: Some(finish),
        }
    }

    /// The options Resize hands back
    /// (`ImageSizeOptions(width: Int(width.rounded()), height: …, resolution: resolution, sampling: sampling)`).
    pub fn options(&self) -> ImageSizeOptions {
        let mut options = ImageSizeOptions::new(
            self.state.width.round() as usize,
            self.state.height.round() as usize,
            self.state.resolution,
        );
        options.sampling = self.state.sampling;
        options
    }

    /// Resize is enabled while the dimensions, resolution and surface are usable (`valid`).
    pub fn valid(&self) -> bool {
        self.state.valid()
    }

    /// Opens the sheet as a modal dialog and returns the view it renders.
    pub fn open(
        session: Entity<EditorSession>,
        finish: impl FnOnce(Option<ImageSizeOptions>, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let view = cx.new(|_| Self::new(session, Box::new(finish)));
        view.update(cx, |sheet, cx| sheet.start(cx));
        window.open_dialog(cx, {
            let view = view.clone();
            move |dialog, _, _| {
                let ok = view.clone();
                let cancel = view.clone();
                dialog
                    .title("Image Size")
                    .w(px(WIDTH + DIALOG_PADDING * 2.0))
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("Resize")
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

    /// `init(document:finish:)`'s body: the document's pixels and resolution.
    fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(options) = self.session.read(cx).image_size_options() {
            self.state = ImageSizeState::new(options.width, options.height, options.resolution);
        }
    }

    /// Hands the outcome to the caller, once (`finish(_:)`).
    fn close(&mut self, options: Option<ImageSizeOptions>, window: &mut Window, cx: &mut App) {
        if let Some(finish) = self.finish.take() {
            finish(options, window, cx);
        }
    }

    /// The fields, once the window exists (`@State`'s own storage).
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.width_field.is_some() {
            return;
        }
        let width = cx.new(|cx| InputState::new(window, cx));
        let height = cx.new(|cx| InputState::new(window, cx));
        let resolution = cx.new(|cx| InputState::new(window, cx));
        self.width_field = Some(FieldState {
            input: width,
            last_text: String::new(),
        });
        self.height_field = Some(FieldState {
            input: height,
            last_text: String::new(),
        });
        self.resolution_field = Some(FieldState {
            input: resolution,
            last_text: String::new(),
        });
        self.write_texts(window, cx);
    }

    /// One field's own state.
    fn field(&self, field: Field) -> Option<&FieldState> {
        match field {
            Field::Width => self.width_field.as_ref(),
            Field::Height => self.height_field.as_ref(),
            Field::Resolution => self.resolution_field.as_ref(),
        }
    }

    /// One field's own state, mutably.
    fn field_mut(&mut self, field: Field) -> Option<&mut FieldState> {
        match field {
            Field::Width => self.width_field.as_mut(),
            Field::Height => self.height_field.as_mut(),
            Field::Resolution => self.resolution_field.as_mut(),
        }
    }

    /// The value one field shows.
    fn field_value(&self, field: Field) -> f64 {
        match field {
            Field::Width => self.state.displayed(true),
            Field::Height => self.state.displayed(false),
            Field::Resolution => self.state.resolution,
        }
    }

    /// Writes one field's value into its text box, leaving the field being typed into alone
    /// (`sync()`).
    fn write_field_text(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let text = format_number(self.field_value(field), 3);
        let Some(state) = self.field_mut(field) else {
            return;
        };
        if state.input.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        if state.input.read(cx).value().as_ref() != text {
            state
                .input
                .update(cx, |input, cx| input.set_value(text.clone(), window, cx));
        }
        state.last_text = text;
    }

    /// Every field.
    fn write_texts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.write_field_text(Field::Width, window, cx);
        self.write_field_text(Field::Height, window, cx);
        self.write_field_text(Field::Resolution, window, cx);
    }

    /// The field's own one-frame bookkeeping: live typing while it has the keyboard, the text
    /// following the value while it does not.
    fn update_field(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.field(field) else {
            return;
        };
        let input = state.input.clone();
        let last_text = state.last_text.clone();
        let text = input.read(cx).value().to_string();
        if input.read(cx).focus_handle(cx).is_focused(window) {
            // `.onChange(of: text) { if focused, let number = Double(text) { change(number) } }`.
            if text != last_text {
                if let Some(number) = parse_number(&text) {
                    match field {
                        Field::Width => self.state.set_dimension(number, true),
                        Field::Height => self.state.set_dimension(number, false),
                        Field::Resolution => self.state.set_resolution(number),
                    }
                    // The aspect lock or a print unit may have moved the other fields, which follow.
                    self.write_texts(window, cx);
                }
                if let Some(state) = self.field_mut(field) {
                    state.last_text = text;
                }
            }
        } else {
            // `.onChange(of: focused) { if !focused { sync() } }`.
            self.write_field_text(field, window, cx);
        }
    }

    /// A scrub drag on a size label (`dimension(isWidth:)`'s set, through the scrub's own range).
    fn scrub_dimension(
        &mut self,
        value: f64,
        is_width: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.set_dimension(value, is_width);
        self.write_texts(window, cx);
        cx.notify();
    }

    /// A scrub drag on the Resolution label (`$resolution` through `1...9600`).
    fn scrub_resolution(&mut self, value: f64, window: &mut Window, cx: &mut Context<Self>) {
        self.state.set_resolution(value);
        self.write_texts(window, cx);
        cx.notify();
    }

    /// `Picker("Units", selection: $unit)`, with the print-only filter.
    fn unit_row(&self, cx: &mut Context<Self>) -> Div {
        let entity = cx.entity();
        let unit = self.state.unit;
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child("Units"))
            .child(menu_picker(
                "image-size-units",
                self.state.units(),
                unit,
                move |unit, window, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.state.unit = unit;
                        sheet.write_texts(window, cx);
                        cx.notify();
                    });
                },
            ))
    }

    /// One size row: the 75-point scrub label and its field.
    fn dimension_row(&self, is_width: bool, cx: &mut Context<Self>) -> Div {
        let field = if is_width { Field::Width } else { Field::Height };
        let label = field.label();
        let entity = cx.entity();
        let label_element: AnyElement = if self.state.can_scrub_dimensions() {
            div()
                .w(px(LABEL_WIDTH))
                .child(label)
                .scrubbable(
                    field.id(),
                    NumericScrub::new(
                        self.state.displayed(is_width),
                        self.state.scrub_sensitivity(is_width),
                        self.state.scrub_range(is_width),
                    )
                    .step(1.0)
                    .on_change(move |value, window, cx| {
                        entity.update(cx, |sheet, cx| {
                            sheet.scrub_dimension(value, is_width, window, cx)
                        });
                    }),
                )
                .into_any_element()
        } else {
            // `.disabled(!canScrubDimensions)`: the label takes no drag and dims.
            div()
                .w(px(LABEL_WIDTH))
                .opacity(0.5)
                .child(label)
                .into_any_element()
        };
        let mut row = h_flex().items_center().gap(px(8.0)).child(label_element);
        if let Some(input) = self.field(field).map(|state| state.input.clone()) {
            row = row.child(div().flex_1().child(Input::new(&input).aria_label(label)));
        }
        row
    }

    /// `Toggle("Lock aspect ratio", isOn: $locked).disabled(!resample)`.
    fn lock_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
        let entity = cx.entity();
        Checkbox::new("image-size-lock")
            .label("Lock aspect ratio")
            .checked(self.state.locked)
            .disabled(!self.state.resample)
            .on_change(move |checked, _, cx| {
                let locked = *checked;
                entity.update(cx, |sheet, cx| {
                    sheet.state.locked = locked;
                    cx.notify();
                });
            })
    }

    /// The Resolution row: the scrubbable title, the field and its unit.
    fn resolution_row(&self, cx: &mut Context<Self>) -> Div {
        let entity = cx.entity();
        let label = div().child("Resolution").scrubbable(
            "image-size-resolution-label",
            NumericScrub::new(self.state.resolution, 1.0, RESOLUTION_RANGE)
                .step(1.0)
                .on_change(move |value, window, cx| {
                    entity.update(cx, |sheet, cx| sheet.scrub_resolution(value, window, cx));
                }),
        );
        let mut row = h_flex().items_center().gap(px(8.0)).child(label);
        if let Some(input) = self.resolution_field.as_ref().map(|state| state.input.clone()) {
            row = row.child(div().flex_1().child(Input::new(&input).aria_label("Resolution")));
        }
        row.child(
            div()
                .text_color(hsla(0.0, 0.0, 1.0, 0.6))
                .child("pixels/inch"),
        )
    }

    /// `Toggle("Resample", isOn: $resample)`, whose `onChange` restores the document's pixels and
    /// moves to a print unit when it is turned off.
    fn resample_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
        let entity = cx.entity();
        Checkbox::new("image-size-resample")
            .label("Resample")
            .checked(self.state.resample)
            .on_change(move |checked, window, cx| {
                let enabled = *checked;
                entity.update(cx, |sheet, cx| {
                    sheet.state.set_resample(enabled);
                    sheet.write_texts(window, cx);
                    cx.notify();
                });
            })
    }

    /// `Picker("Sampling", selection: $sampling)`.
    fn sampling_row(&self, cx: &mut Context<Self>) -> Div {
        let entity = cx.entity();
        let sampling = self.state.sampling;
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child("Sampling"))
            .child(menu_picker(
                "image-size-sampling",
                LayerSampling::ALL.map(|option| (option, option.raw_value())),
                sampling,
                move |sampling, _, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.state.sampling = sampling;
                        cx.notify();
                    });
                },
            ))
    }
}

impl Render for ImageSizeSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The fields' own state is made the first time the sheet is drawn, when a window is at hand.
        self.ensure_fields(window, cx);
        for field in [Field::Width, Field::Height, Field::Resolution] {
            self.update_field(field, window, cx);
        }

        let valid = self.valid();
        let secondary = hsla(0.0, 0.0, 1.0, 0.6);
        let mut sheet = v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .child(div().text_lg().child("Image Size"))
            .child(
                div()
                    .text_color(secondary)
                    .child(format!(
                        "Current: {} × {} pixels",
                        self.state.original_width, self.state.original_height
                    )),
            )
            .child(self.unit_row(cx))
            .child(self.dimension_row(true, cx))
            .child(self.dimension_row(false, cx))
            .child(self.lock_checkbox(cx))
            .child(self.resolution_row(cx))
            .child(self.resample_checkbox(cx));
        if self.state.resample {
            sheet = sheet
                .child(self.sampling_row(cx))
                .child(
                    div()
                        .text_color(secondary)
                        .child("Resizes layer pixels and applies existing transforms. Undo restores the originals."),
                );
        } else {
            sheet = sheet.child(
                div()
                    .text_color(secondary)
                    .child("Only print dimensions and resolution change. Pixels stay unchanged."),
            );
        }
        sheet.child(if valid {
            div()
                .text_color(secondary)
                .child(format!(
                    "Result: {} × {} pixels",
                    self.state.width.round() as i64,
                    self.state.height.round() as i64
                ))
                .into_any_element()
        } else {
            div()
                .text_color(ORANGE)
                .child(format!(
                    "Use 1–{} pixels per side, up to {} megapixels, and 1–9,600 pixels/inch.",
                    grouped(MAX_SIDE as i64),
                    max_surface_megapixels()
                ))
                .into_any_element()
        })
    }
}

/// `Int.formatted()`: grouped digits (`30,000`).
fn grouped(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut result = String::with_capacity(digits.len() * 4 / 3 + 1);
    if value < 0 {
        result.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{grouped, Field, ImageSizeSheet, ImageSizeState};

    use compositor_rs_core::canvas_size::CanvasUnit;
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::layer_transform::LayerSampling;
    use compositor_rs_core::limits::MAX_SIDE;
    use compositor_rs_session::EditorSession;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, TestAppContext, TestSupportExt as _, WindowBounds, WindowOptions, point,
        px, size,
    };

    #[test]
    fn the_sheet_starts_from_the_documents_size() {
        let state = ImageSizeState::new(1920, 1080, 300.0);
        assert_eq!((state.width, state.height), (1920.0, 1080.0));
        assert_eq!(state.last_resolution, 300.0);
        assert!(state.locked);
        assert!(state.resample);
        assert_eq!(state.unit, CanvasUnit::Pixels);
        assert_eq!(state.sampling, LayerSampling::High);
        assert!(state.valid());
    }

    #[test]
    fn the_picker_hides_the_pixel_units_while_resampling_is_off() {
        let mut state = ImageSizeState::new(100, 100, 72.0);
        let units: Vec<CanvasUnit> = state.units().into_iter().map(|(unit, _)| unit).collect();
        assert_eq!(units, CanvasUnit::ALL.to_vec());
        state.resample = false;
        let units: Vec<CanvasUnit> = state.units().into_iter().map(|(unit, _)| unit).collect();
        assert_eq!(units, [CanvasUnit::Inches, CanvasUnit::Centimeters]);
        let labels: Vec<&str> = state.units().into_iter().map(|(_, label)| label).collect();
        assert_eq!(labels, ["Inches", "Centimeters"]);
    }

    /// `dimension(isWidth:)`'s set, in every unit and with the aspect lock.
    #[test]
    fn typed_values_land_in_their_own_unit() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.set_dimension(800.0, true);
        assert_eq!((state.width, state.height), (800.0, 400.0));

        state.unit = CanvasUnit::Percent;
        state.set_dimension(50.0, true);
        assert_eq!((state.width, state.height), (500.0, 250.0));

        state.unit = CanvasUnit::Inches;
        state.set_dimension(4.0, false);
        assert_eq!(state.height, 400.0);
        assert_eq!(state.width, 800.0);

        state.unit = CanvasUnit::Centimeters;
        state.set_dimension(25.4, true);
        assert_eq!(state.width, 1000.0);
        assert_eq!(state.height, 500.0);

        // A non-positive or non-finite value changes nothing.
        state.set_dimension(0.0, true);
        state.set_dimension(f64::NAN, true);
        assert_eq!(state.width, 1000.0);
    }

    /// Without resampling the typed value moves the resolution instead of the pixels.
    #[test]
    fn a_print_size_moves_the_resolution_without_resampling() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.resample = false;
        state.unit = CanvasUnit::Inches;
        state.set_dimension(10.0, true);
        assert_eq!(state.resolution, 100.0);
        assert_eq!(state.width, 1000.0);

        state.set_dimension(5.0, true);
        assert_eq!(state.resolution, 200.0);
        assert_eq!(state.width, 1000.0);

        state.unit = CanvasUnit::Centimeters;
        state.set_dimension(10.0, true);
        assert_eq!(state.resolution, 254.0);
    }

    /// `.onChange(of: resolution)`: print sizes scale while resampling, and the last usable value
    /// follows the field.
    #[test]
    fn a_resolution_change_scales_the_print_size() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.unit = CanvasUnit::Inches;
        state.set_resolution(200.0);
        assert_eq!((state.width, state.height), (2000.0, 1000.0));
        assert_eq!(state.last_resolution, 200.0);

        // A zero or negative entry is taken by the field — so Resize turns off — but nothing
        // scales and the last usable value is kept.
        state.set_resolution(0.0);
        assert_eq!(state.resolution, 0.0);
        assert!(!state.valid());
        state.set_resolution(-5.0);
        assert_eq!(state.resolution, -5.0);
        assert_eq!(state.last_resolution, 200.0);

        // Outside a print unit the pixels stay put.
        state.unit = CanvasUnit::Pixels;
        state.set_resolution(300.0);
        assert_eq!((state.width, state.height), (2000.0, 1000.0));
        assert_eq!(state.last_resolution, 300.0);
    }

    /// `Toggle("Resample")`'s `onChange`.
    #[test]
    fn turning_resampling_off_restores_the_document_and_locks_the_aspect() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.set_dimension(800.0, true);
        state.unit = CanvasUnit::Percent;
        state.locked = false;
        state.set_resample(false);
        assert!(!state.resample);
        assert_eq!((state.width, state.height), (1000.0, 500.0));
        assert!(state.locked);
        assert_eq!(state.unit, CanvasUnit::Inches);

        // A print unit is left where it is.
        state.set_resample(true);
        state.unit = CanvasUnit::Centimeters;
        state.set_resample(false);
        assert_eq!(state.unit, CanvasUnit::Centimeters);
    }

    /// `valid`: the resolution range, the side limit and — while resampling — the surface limit.
    #[test]
    fn validity_keeps_every_limit() {
        let mut state = ImageSizeState::new(1000, 500, 72.0);
        assert!(state.valid());
        state.resolution = 0.5;
        assert!(!state.valid());
        state.resolution = 9601.0;
        assert!(!state.valid());
        state.resolution = 9600.0;
        assert!(state.valid());
        state.width = 30_001.0;
        assert!(!state.valid());
        state.width = 30_000.0;
        assert!(state.valid());
        // The surface limit only applies while resampling.
        state.width = 30_000.0;
        state.height = 30_000.0;
        assert!(!state.valid());
        state.resample = false;
        assert!(state.valid());
    }

    /// `canScrubDimensions`, `scrubRange` and `scrubSensitivity`.
    #[test]
    fn scrubbing_needs_a_usable_resolution_in_a_print_unit() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.unit = CanvasUnit::Inches;
        assert!(state.can_scrub_dimensions());
        assert_eq!(state.scrub_sensitivity(true), 0.01);
        // The locked range keeps the ratio (minimum 2) and stays under the surface limit
        // (sqrt(100,000,000 × 2) = 14142.1356…), written in inches.
        let (lower, upper) = state.scrub_range(true);
        assert_eq!(lower, 0.02);
        assert!((upper - 141.4213562373095).abs() < 1e-9);

        state.resolution = 0.0;
        assert!(!state.can_scrub_dimensions());
        assert_eq!(state.scrub_sensitivity(true), 0.0);
        assert_eq!(state.scrub_range(true), (0.0, 0.0));

        // Without resampling the range is the print size the resolution may move within.
        state.resolution = 100.0;
        state.resample = false;
        assert_eq!(state.scrub_sensitivity(true), 0.01);
        assert_eq!(state.scrub_range(true), (1000.0 / 9600.0, 1000.0));
        state.unit = CanvasUnit::Centimeters;
        assert_eq!(state.scrub_sensitivity(true), 0.0254);
        assert_eq!(state.scrub_range(true), (1000.0 * 2.54 / 9600.0, 2540.0));
    }

    /// The locked pixel range keeps the original ratio and stays under the surface limit.
    #[test]
    fn a_locked_scrub_keeps_the_ratio_and_the_surface_limit() {
        let mut state = ImageSizeState::new(1000, 500, 100.0);
        state.locked = true;
        // The other side's limit: 500/1000 = 0.5 → minimum 2, and the surface limit allows
        // sqrt(100,000,000 × 2) = 14142.1356…, below 30,000.
        let (lower, upper) = state.scrub_range(true);
        assert_eq!(lower, 2.0);
        assert!((upper - 14142.135623730951).abs() < 1e-9);

        state.locked = false;
        assert_eq!(state.scrub_range(true), (1.0, 30_000.0));
        assert_eq!(state.scrub_sensitivity(true), 1.0);
        assert_eq!(state.scrub_sensitivity(false), 1.0);
    }

    #[test]
    fn the_limit_message_groups_its_digits() {
        assert_eq!(grouped(MAX_SIDE as i64), "30,000");
    }

    #[test]
    fn every_field_keeps_its_label() {
        assert_eq!(Field::Width.label(), "Width");
        assert_eq!(Field::Height.label(), "Height");
        assert_eq!(Field::Resolution.label(), "Resolution");
    }

    /// The document's size and resolution, through a real window.
    #[gpui_kit::test]
    fn the_sheet_starts_from_the_document_and_hands_back_its_options(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let session = cx.update(|cx| {
            let session = cx.new(|_| EditorSession::new());
            session.update(cx, |session, _| {
                session.document = Some(CanvasDocument::new(1200, 800));
            });
            session
        });
        let window = cx.update(|cx| {
            let bounds = gpui_kit::Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(700.), px(700.)),
            };
            let (window, _content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| gpui_kit::EmptyView),
            )
            .expect("open the test window");
            window
        });
        // `open` calls `window.open_dialog`, which needs the window's Root installed — gpui
        // only installs it after `open_window`'s build closure returns.
        let sheet = cx
            .update_window(window, |_, window, cx| {
                ImageSizeSheet::open(session.clone(), |_, _, _| {}, window, cx)
            })
            .expect("open the sheet");

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let (width, height, resolution) = {
                let state = &sheet.read(cx).state;
                (state.width, state.height, state.resolution)
            };
            assert_eq!((width, height, resolution), (1200.0, 800.0, 72.0));
            assert!(sheet.read(cx).valid());

            let options = sheet.read(cx).options();
            assert_eq!((options.width, options.height), (1200, 800));
            assert_eq!(options.resolution, 72.0);
            assert_eq!(options.sampling, LayerSampling::High);

            // The aspect lock carries a typed width onto the height, as the binding does.
            sheet.update(cx, |sheet, cx| {
                sheet.state.set_dimension(600.0, true);
                cx.notify();
            });
            let options = sheet.read(cx).options();
            assert_eq!((options.width, options.height), (600, 400));

            // A surface past the limit turns Resize off.
            sheet.update(cx, |sheet, cx| {
                sheet.state.width = 30_000.0;
                sheet.state.height = 30_000.0;
                cx.notify();
            });
            assert!(!sheet.read(cx).valid());
        })
        .unwrap();
    }
}
