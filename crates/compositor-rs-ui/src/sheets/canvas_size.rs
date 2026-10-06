//! Canvas Size: the modal that changes the document's canvas rectangle and fills what it adds
//! (port of `UI/CanvasSizeSheet.swift`).
//!
//! The sheet only collects the options; the caller runs `EditorSession::change_canvas_size` with
//! them, as the Swift `ProjectController.canvasSize` did around the hosted view.

use std::cell::Cell;
use std::rc::Rc;

use compositor_rs_core::canvas_size::{
    CanvasExtensionColor, CanvasSizeDraft, CanvasSizeOptions, CanvasUnit,
};
use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::limits::MAX_SIDE;
use compositor_rs_session::EditorSession;

use crate::canvas::overlays::palette_rgba;
use crate::tool_controls::{format_number, menu_picker, parse_number};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariant};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::{h_flex, v_flex, WindowExt as _};
use gpui_kit::*;

/// The sheet's content width, `.frame(width: 450)`.
const WIDTH: f32 = 450.0;
/// The component dialog's own padding, 16 points on each side.
const DIALOG_PADDING: f32 = 16.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// A size label's `frame(width: 60, alignment: .leading)`.
const LABEL_WIDTH: f32 = 60.0;
/// The anchor grid's `horizontalSpacing: 3, verticalSpacing: 3`.
const GRID_SPACING: f32 = 3.0;
/// `Image(systemName:).frame(width: 25, height: 25)`.
const ANCHOR_SIZE: f32 = 25.0;
/// The circle glyph inside an anchor cell, where the SF Symbol drew one.
const ANCHOR_DOT: f32 = 11.0;
/// The anchor hint's `.padding(.top, 28)`.
const HINT_TOP: f32 = 28.0;
/// The swatch: `frame(width: 34, height: 18)` in a `RoundedRectangle(cornerRadius: 4)`, its white
/// border inset by one.
const SWATCH_WIDTH: f32 = 34.0;
const SWATCH_HEIGHT: f32 = 18.0;
const SWATCH_RADIUS: f32 = 4.0;

/// The error orange the Swift's `.orange` is (SwiftUI orange, #FF9500).
const ORANGE: Hsla = hsla(0.075, 1.0, 0.5, 1.0);

/// `anchorNames`, row-major: top-left through bottom-right.
const ANCHOR_NAMES: [&str; 9] = [
    "Top left",
    "Top center",
    "Top right",
    "Middle left",
    "Center",
    "Middle right",
    "Bottom left",
    "Bottom center",
    "Bottom right",
];

/// What the sheet hands back when it closes: the options, or `None` on Cancel
/// (`finish(CanvasSizeOptions?)`).
pub type CanvasSizeFinish = Box<dyn FnOnce(Option<CanvasSizeOptions>, &mut Window, &mut App)>;

/// The Canvas extension menu's choices (`extensionChoice`), in the Swift's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExtensionChoice {
    Transparent,
    Foreground,
    Background,
    Black,
    White,
    Custom,
}

impl ExtensionChoice {
    /// `ForEach(["Transparent", "Foreground", "Background", "Black", "White", "Custom"])`.
    const ALL: [ExtensionChoice; 6] = [
        ExtensionChoice::Transparent,
        ExtensionChoice::Foreground,
        ExtensionChoice::Background,
        ExtensionChoice::Black,
        ExtensionChoice::White,
        ExtensionChoice::Custom,
    ];

    /// The menu label, which is the Swift's own string.
    fn label(self) -> &'static str {
        match self {
            ExtensionChoice::Transparent => "Transparent",
            ExtensionChoice::Foreground => "Foreground",
            ExtensionChoice::Background => "Background",
            ExtensionChoice::Black => "Black",
            ExtensionChoice::White => "White",
            ExtensionChoice::Custom => "Custom",
        }
    }
}

/// One size field's text box and the text the last frame saw of it, as the port's other property
/// fields keep them.
struct FieldState {
    input: Entity<InputState>,
    last_text: String,
}

/// The Canvas Size sheet's state: the draft, where the extension lands and what fills it.
pub struct CanvasSizeSheet {
    session: Entity<EditorSession>,
    /// `@State draft`, read from the document by [`Self::open`].
    draft: CanvasSizeDraft,
    /// `@State anchor = 4`: the 3×3 grid's selected cell, row-major.
    anchor: usize,
    extension_choice: ExtensionChoice,
    /// `@State customColor = PaletteColor.white`, shared with the picker's callback so the swatch
    /// follows its working color as the Swift `@Binding` did.
    custom_color: Rc<Cell<PaletteColor>>,
    /// `session.foregroundColor` / `session.backgroundColor`, taken in `init`.
    foreground: PaletteColor,
    background: PaletteColor,
    /// The width and height fields, built on the first render — the first time a window is
    /// available — as the Swift's `@State` bindings are their own storage.
    width_field: Option<FieldState>,
    height_field: Option<FieldState>,
    /// The picker color the sheet last saw, so `.onChange(of: session.colorPicker?.color)` can see
    /// it move.
    picker_color: Option<PaletteColor>,
    finish: Option<CanvasSizeFinish>,
}

impl CanvasSizeSheet {
    /// A fresh sheet with the Swift's `@State` defaults. The draft and the two palette swatches are
    /// read from the session by [`Self::open`] (`init(document:session:finish:)`); until then the
    /// draft is a 1×1 canvas at the document's 72 pixels/inch.
    pub fn new(session: Entity<EditorSession>, finish: CanvasSizeFinish) -> Self {
        Self {
            session,
            draft: CanvasSizeDraft::new(1, 1, 72.0),
            anchor: 4,
            extension_choice: ExtensionChoice::Transparent,
            custom_color: Rc::new(Cell::new(PaletteColor::WHITE)),
            foreground: PaletteColor::BLACK,
            background: PaletteColor::WHITE,
            width_field: None,
            height_field: None,
            picker_color: None,
            finish: Some(finish),
        }
    }

    /// The draft the sheet is editing (`draft`).
    pub fn draft(&self) -> &CanvasSizeDraft {
        &self.draft
    }

    /// The options OK hands back
    /// (`CanvasSizeOptions(width: Int(draft.width.rounded()), height: …, anchor: anchor, fill: fill)`).
    pub fn options(&self) -> CanvasSizeOptions {
        let mut options = CanvasSizeOptions::new(
            self.draft.width.round() as usize,
            self.draft.height.round() as usize,
        );
        options.anchor = self.anchor;
        options.fill = self.fill();
        options
    }

    /// OK is enabled while the draft's dimensions are usable (`draft.valid`).
    pub fn valid(&self) -> bool {
        self.draft.valid()
    }

    /// `fill`: the extension color, or `None` for Transparent.
    fn fill(&self) -> Option<CanvasExtensionColor> {
        extension_fill(
            self.extension_choice,
            self.foreground,
            self.background,
            self.custom_color.get(),
        )
    }

    /// Opens the sheet as a modal dialog and returns the view it renders.
    pub fn open(
        session: Entity<EditorSession>,
        finish: impl FnOnce(Option<CanvasSizeOptions>, &mut Window, &mut App) + 'static,
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
                    .title("Canvas Size")
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
                                sheet.close_picker(cx);
                                let options = sheet.options();
                                sheet.close(Some(options), window, cx);
                                confirmed = true;
                            }
                        });
                        confirmed
                    })
                    .on_cancel(move |_, window, cx| {
                        cancel.update(cx, |sheet, cx| {
                            sheet.close_picker(cx);
                            sheet.close(None, window, cx);
                        });
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

    /// `init(document:session:finish:)`'s body, run once the view exists: the draft and the two
    /// swatches come from the session, and the sheet follows the dialog's own picker as the Swift's
    /// `DialogColorSwatch` did.
    fn start(&mut self, cx: &mut Context<Self>) {
        let (draft, foreground, background, picker_color) = {
            let session = self.session.read(cx);
            (
                session.canvas_size_draft(),
                session.foreground_color(),
                session.background_color,
                session.color_picker.as_ref().map(|picker| picker.color()),
            )
        };
        if let Some(draft) = draft {
            self.draft = draft;
        }
        self.foreground = foreground;
        self.background = background;
        self.picker_color = picker_color;
        let session = self.session.clone();
        cx.observe(&session, |sheet, session, cx| sheet.follow_picker(session, cx))
            .detach();
    }

    /// `.onChange(of: session.colorPicker?.color) { session.previewDialogColor() }`: while the
    /// dialog's picker is open, every move of its working color is handed to the sheet's swatch.
    fn follow_picker(&mut self, session: Entity<EditorSession>, cx: &mut Context<Self>) {
        let color = session
            .read(cx)
            .color_picker
            .as_ref()
            .map(|picker| picker.color());
        if color != self.picker_color {
            self.picker_color = color;
            if session.read(cx).picking_for_dialog() {
                session.update(cx, |session, _| session.preview_dialog_color());
            }
        }
        cx.notify();
    }

    /// `DialogColorSwatch.closePicker(session)`: puts the picker away with the dialog.
    fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).picking_for_dialog() {
            self.session
                .update(cx, |session, _| session.close_color_picker(true));
        }
    }

    /// Hands the outcome to the caller, once (`finish(_:)`).
    fn close(&mut self, options: Option<CanvasSizeOptions>, window: &mut Window, cx: &mut App) {
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
        self.width_field = Some(FieldState {
            input: width,
            last_text: String::new(),
        });
        self.height_field = Some(FieldState {
            input: height,
            last_text: String::new(),
        });
        self.write_texts(window, cx);
    }

    /// One axis' field state.
    fn field(&self, width_axis: bool) -> Option<&FieldState> {
        if width_axis {
            self.width_field.as_ref()
        } else {
            self.height_field.as_ref()
        }
    }

    /// Writes one field's value into its text box, leaving the field being typed into alone
    /// (`sync()`).
    fn write_field_text(&mut self, width_axis: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = format_number(self.draft.displayed(width_axis), 3);
        let Some(state) = (if width_axis {
            self.width_field.as_mut()
        } else {
            self.height_field.as_mut()
        }) else {
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

    /// Both fields.
    fn write_texts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.write_field_text(true, window, cx);
        self.write_field_text(false, window, cx);
    }

    /// The field's own one-frame bookkeeping: live typing while it has the keyboard, the text
    /// following the value while it does not.
    fn update_field(&mut self, width_axis: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.field(width_axis) else {
            return;
        };
        let input = state.input.clone();
        let last_text = state.last_text.clone();
        let text = input.read(cx).value().to_string();
        if input.read(cx).focus_handle(cx).is_focused(window) {
            // `.onChange(of: text) { if focused, let number = Double(text) { change(number) } }`.
            if text != last_text {
                if let Some(number) = parse_number(&text) {
                    self.draft.set(number, width_axis);
                    // The aspect lock may have moved the other side, whose field follows it.
                    self.write_texts(window, cx);
                }
                if let Some(state) = (if width_axis {
                    self.width_field.as_mut()
                } else {
                    self.height_field.as_mut()
                }) {
                    state.last_text = text;
                }
            }
        } else {
            // `.onChange(of: focused) { if !focused { sync() } }`.
            self.write_field_text(width_axis, window, cx);
        }
    }

    /// A scrub drag on a label (`dimension(_:)`'s set, through the scrub's own range).
    fn scrub_dimension(
        &mut self,
        value: f64,
        width_axis: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft.set(value, width_axis);
        self.write_texts(window, cx);
        cx.notify();
    }

    /// `Picker("Units", selection: $draft.unit)`.
    fn unit_row(&self, cx: &mut Context<Self>) -> Div {
        let entity = cx.entity();
        let unit = self.draft.unit;
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child("Units"))
            .child(menu_picker(
                "canvas-size-units",
                CanvasUnit::ALL.map(|unit| (unit, unit.raw_value())),
                unit,
                move |unit, _, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.draft.unit = unit;
                        cx.notify();
                    });
                },
            ))
    }

    /// One size row: the 60-point scrub label and its field.
    fn dimension_row(&self, width_axis: bool, cx: &mut Context<Self>) -> Div {
        let label = if width_axis { "Width" } else { "Height" };
        let value = self.draft.displayed(width_axis);
        let sensitivity = scrub_sensitivity(&self.draft, width_axis);
        let range = scrub_range(&self.draft, width_axis);
        let entity = cx.entity();
        let label_element = div()
            .w(px(LABEL_WIDTH))
            .child(label)
            .scrubbable(
                if width_axis {
                    "canvas-size-width"
                } else {
                    "canvas-size-height"
                },
                NumericScrub::new(value, sensitivity, range)
                    .step(1.0)
                    .on_change(move |value, window, cx| {
                        entity.update(cx, |sheet, cx| {
                            sheet.scrub_dimension(value, width_axis, window, cx)
                        });
                    }),
            );
        let mut row = h_flex().items_center().gap(px(8.0)).child(label_element);
        if let Some(input) = self.field(width_axis).map(|state| state.input.clone()) {
            row = row.child(div().flex_1().child(Input::new(&input).aria_label(label)));
        }
        row
    }

    /// `Toggle("Relative to current dimensions", isOn: $draft.relative)`.
    fn relative_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
        let entity = cx.entity();
        Checkbox::new("canvas-size-relative")
            .label("Relative to current dimensions")
            .checked(self.draft.relative)
            .on_change(move |checked, _, cx| {
                let checked = *checked;
                entity.update(cx, |sheet, cx| {
                    sheet.draft.relative = checked;
                    cx.notify();
                });
            })
    }

    /// `Toggle("Lock original aspect ratio", isOn: $draft.locked)`, whose `onChange` re-applies the
    /// width through the binding so the height follows the original ratio.
    fn lock_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
        let entity = cx.entity();
        Checkbox::new("canvas-size-lock")
            .label("Lock original aspect ratio")
            .checked(self.draft.locked)
            .on_change(move |checked, window, cx| {
                let locked = *checked;
                entity.update(cx, |sheet, cx| {
                    sheet.draft.locked = locked;
                    if locked {
                        let displayed = sheet.draft.displayed(true);
                        sheet.draft.set(displayed, true);
                    }
                    sheet.write_texts(window, cx);
                    cx.notify();
                });
            })
    }

    /// `HStack(alignment: .top, spacing: 24)`: the 3×3 anchor grid and the hint beside it.
    fn anchor_section(&self, cx: &mut Context<Self>) -> Div {
        let rows: Vec<AnyElement> = (0..3)
            .map(|row| {
                h_flex()
                    .gap(px(GRID_SPACING))
                    .children((0..3).map(|column| self.anchor_button(row * 3 + column, cx)))
                    .into_any_element()
            })
            .collect();
        h_flex()
            .items_start()
            .gap(px(24.0))
            .child(
                v_flex()
                    .gap(px(8.0))
                    .child(div().child("Anchor"))
                    .child(v_flex().gap(px(GRID_SPACING)).children(rows)),
            )
            .child(
                v_flex()
                    .gap(px(8.0))
                    .pt(px(HINT_TOP))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(ANCHOR_NAMES[self.anchor]),
                    )
                    .child(
                        div()
                            .text_color(hsla(0.0, 0.0, 1.0, 0.6))
                            .child("Keeps this point fixed. Artwork is not scaled; cropped content remains outside the canvas."),
                    ),
            )
    }

    /// One cell of the anchor grid: `circle.fill` while it is the anchor, `circle` otherwise, both
    /// `frame(width: 25, height: 25)`.
    fn anchor_button(&self, index: usize, cx: &mut Context<Self>) -> Button {
        let selected = index == self.anchor;
        let color = if selected {
            cx.theme().primary
        } else {
            hsla(0.0, 0.0, 1.0, 0.55)
        };
        let dot = div().size(px(ANCHOR_DOT)).rounded_full();
        let glyph = if selected {
            dot.bg(color)
        } else {
            dot.border_1().border_color(color)
        };
        let entity = cx.entity();
        Button::new(("canvas-size-anchor", index))
            .w(px(ANCHOR_SIZE))
            .h(px(ANCHOR_SIZE))
            .child(glyph)
            .tooltip(ANCHOR_NAMES[index])
            .accessibility_label(ANCHOR_NAMES[index])
            .on_click(move |_, _, cx| {
                entity.update(cx, |sheet, cx| {
                    sheet.anchor = index;
                    cx.notify();
                });
            })
    }

    /// `Picker("Canvas extension", selection: $extensionChoice)`.
    fn extension_row(&self, cx: &mut Context<Self>) -> Div {
        let entity = cx.entity();
        let choice = self.extension_choice;
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child("Canvas extension"))
            .child(menu_picker(
                "canvas-size-extension",
                ExtensionChoice::ALL.map(|choice| (choice, choice.label())),
                choice,
                move |choice, _, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.extension_choice = choice;
                        cx.notify();
                    });
                },
            ))
    }

    /// `if extensionChoice == "Custom"`: the swatch and its title.
    fn custom_color_row(&self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .items_center()
            .gap(px(8.0))
            .child(div().child("Extension color"))
            .child(self.swatch(cx))
    }

    /// `DialogColorSwatch(title: "Extension Color", color: $customColor, session: session)`: the
    /// brush's swatch, which opens the app's picker on the custom color.
    fn swatch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("canvas-size-extension-color")
            .relative()
            .flex_none()
            .w(px(SWATCH_WIDTH))
            .h(px(SWATCH_HEIGHT))
            .rounded(px(SWATCH_RADIUS))
            .bg(palette_rgba(self.custom_color.get()))
            .border_1()
            .border_color(hsla(0.0, 0.0, 0.0, 1.0))
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .aria_label("Extension Color")
            .tooltip(|window, cx| Tooltip::new("Color for the added canvas").build(window, cx))
            .on_click(move |_, _, cx| {
                entity.update(cx, |sheet, cx| sheet.open_picker(cx));
            })
            .child(
                // `shape.inset(by: 1).strokeBorder(.white, lineWidth: 1)`.
                div()
                    .absolute()
                    .left(px(1.0))
                    .top(px(1.0))
                    .right(px(1.0))
                    .bottom(px(1.0))
                    .rounded(px(SWATCH_RADIUS - 1.0))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 1.0)),
            )
    }

    /// The swatch's click: `session.openDialogColorPicker(title:color:)`. The chosen color comes
    /// back through the callback the sheet shares its swatch state with, as the Swift `@Binding`.
    fn open_picker(&mut self, cx: &mut Context<Self>) {
        let custom_color = self.custom_color.clone();
        self.session.update(cx, |session, _| {
            session.open_dialog_color_picker(
                "Extension Color".to_string(),
                custom_color.get(),
                Box::new(move |color| custom_color.set(color)),
            );
        });
    }
}

impl Render for CanvasSizeSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The fields' own state is made the first time the sheet is drawn, when a window is at hand.
        self.ensure_fields(window, cx);
        self.update_field(true, window, cx);
        self.update_field(false, window, cx);

        let valid = self.valid();
        let current_bytes = memory_byte_count(
            self.draft.original_width as i64 * self.draft.original_height as i64 * 4,
        );
        let new_width = self.draft.width.round() as i64;
        let new_height = self.draft.height.round() as i64;
        let new_bytes = memory_byte_count(new_width * new_height * 4);
        let secondary = hsla(0.0, 0.0, 1.0, 0.6);

        let mut sheet = v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .child(div().text_lg().child("Canvas Size"))
            .child(div().child(format!(
                "Current: {} × {} pixels",
                self.draft.original_width, self.draft.original_height
            )))
            .child(
                div()
                    .text_color(secondary)
                    .child(format!("{current_bytes} uncompressed RGBA canvas")),
            )
            .child(Separator::horizontal())
            .child(self.unit_row(cx))
            .child(self.dimension_row(true, cx))
            .child(self.dimension_row(false, cx))
            .child(self.relative_checkbox(cx))
            .child(self.lock_checkbox(cx))
            .child(if valid {
                div()
                    .text_color(secondary)
                    .child(format!(
                        "New: {new_width} × {new_height} pixels · {new_bytes} uncompressed"
                    ))
                    .into_any_element()
            } else {
                div()
                    .text_color(ORANGE)
                    .child(format!(
                        "Final dimensions must be 1–{} pixels per side.",
                        grouped(MAX_SIDE as i64)
                    ))
                    .into_any_element()
            })
            .child(self.anchor_section(cx))
            .child(self.extension_row(cx));
        if self.extension_choice == ExtensionChoice::Custom {
            sheet = sheet.child(self.custom_color_row(cx));
        }
        sheet
    }
}

/// `scrubRange(_:)`: the pixels the drag may land on, written in the field's own unit.
fn scrub_range(draft: &CanvasSizeDraft, width_axis: bool) -> (f64, f64) {
    let original = if width_axis {
        draft.original_width
    } else {
        draft.original_height
    } as f64;
    let other = if width_axis {
        draft.original_height
    } else {
        draft.original_width
    } as f64;
    let lower = if draft.locked {
        1.0f64.max(original / other)
    } else {
        1.0
    };
    let upper = if draft.locked {
        30_000.0f64.min(30_000.0 * original / other)
    } else {
        30_000.0
    };
    let displayed = |pixels: f64| -> f64 {
        let difference = pixels - if draft.relative { original } else { 0.0 };
        match draft.unit {
            CanvasUnit::Pixels => difference,
            CanvasUnit::Percent => difference / original * 100.0,
            CanvasUnit::Inches => difference / draft.resolution,
            CanvasUnit::Centimeters => difference / draft.resolution * 2.54,
        }
    };
    (displayed(lower), displayed(upper))
}

/// `scrubSensitivity(_:)`: how far one point of drag travel moves the field's own unit.
fn scrub_sensitivity(draft: &CanvasSizeDraft, width_axis: bool) -> f64 {
    let original = if width_axis {
        draft.original_width
    } else {
        draft.original_height
    } as f64;
    match draft.unit {
        CanvasUnit::Pixels => 1.0,
        CanvasUnit::Percent => 100.0 / original,
        CanvasUnit::Inches => 1.0 / draft.resolution,
        CanvasUnit::Centimeters => 2.54 / draft.resolution,
    }
}

/// `fill`: the color the added canvas is filled with, or `None` for Transparent.
fn extension_fill(
    choice: ExtensionChoice,
    foreground: PaletteColor,
    background: PaletteColor,
    custom: PaletteColor,
) -> Option<CanvasExtensionColor> {
    let color = match choice {
        ExtensionChoice::Transparent => return None,
        ExtensionChoice::Black => PaletteColor::BLACK,
        ExtensionChoice::Foreground => foreground,
        ExtensionChoice::White => PaletteColor::WHITE,
        ExtensionChoice::Background => background,
        ExtensionChoice::Custom => custom,
    };
    // The Swift converted the color to sRGB and took its components; a `PaletteColor` is already a
    // straight sRGB triple.
    Some(CanvasExtensionColor::new(color.red, color.green, color.blue))
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

/// `ByteCountFormatter.string(fromByteCount:countStyle: .memory)`: binary (1024-based) units under
/// their decimal names, with the adaptive fraction digits Foundation keeps — none for bytes and KB,
/// one for MB, two from GB up.
///
/// The chain is Foundation's `convertValue`: zero is "Zero KB", one byte is "1 byte", anything
/// under a kilobyte is "N bytes", and every larger count takes the largest unit that keeps it under
/// the next one (an exabyte is the largest unit).
fn memory_byte_count(bytes: i64) -> String {
    let count = bytes as f64;
    if count == 0.0 {
        return "Zero KB".to_string();
    }
    if count == 1.0 {
        return "1 byte".to_string();
    }
    if count == -1.0 {
        return "-1 byte".to_string();
    }
    const KILOBYTE: f64 = 1024.0;
    let (divisor, unit, decimals) = if count.abs() < KILOBYTE {
        return format!("{} bytes", grouped(bytes));
    } else if count.abs() < KILOBYTE.powi(2) {
        (KILOBYTE, "KB", 0usize)
    } else if count.abs() < KILOBYTE.powi(3) {
        (KILOBYTE.powi(2), "MB", 1)
    } else if count.abs() < KILOBYTE.powi(4) {
        (KILOBYTE.powi(3), "GB", 2)
    } else if count.abs() < KILOBYTE.powi(5) {
        (KILOBYTE.powi(4), "TB", 2)
    } else if count.abs() < KILOBYTE.powi(6) {
        (KILOBYTE.powi(5), "PB", 2)
    } else {
        (KILOBYTE.powi(6), "EB", 2)
    };
    format!("{:.*} {}", decimals, count / divisor, unit)
}

#[cfg(test)]
mod tests {
    use super::{
        extension_fill, grouped, memory_byte_count, scrub_range, scrub_sensitivity, CanvasSizeSheet,
        ExtensionChoice, ANCHOR_NAMES,
    };

    use compositor_rs_core::canvas_size::{CanvasExtensionColor, CanvasSizeDraft, CanvasUnit};
    use compositor_rs_core::color::PaletteColor;
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::limits::MAX_SIDE;
    use compositor_rs_session::EditorSession;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, TestAppContext, TestSupportExt as _, WindowBounds, WindowOptions, point,
        px, size,
    };

    #[test]
    fn the_extension_menu_keeps_the_swifts_order() {
        let labels: Vec<&str> = ExtensionChoice::ALL
            .iter()
            .map(|choice| choice.label())
            .collect();
        assert_eq!(
            labels,
            ["Transparent", "Foreground", "Background", "Black", "White", "Custom"]
        );
    }

    #[test]
    fn the_anchor_names_run_top_left_to_bottom_right() {
        assert_eq!(ANCHOR_NAMES[0], "Top left");
        assert_eq!(ANCHOR_NAMES[4], "Center");
        assert_eq!(ANCHOR_NAMES[8], "Bottom right");
    }

    #[test]
    fn the_extension_choice_maps_to_its_fill() {
        let foreground = PaletteColor::new(0.25, 0.5, 0.75);
        let background = PaletteColor::new(0.1, 0.2, 0.3);
        let custom = PaletteColor::new(1.0, 0.0, 1.0);
        let fill = |choice| extension_fill(choice, foreground, background, custom);
        assert_eq!(fill(ExtensionChoice::Transparent), None);
        assert_eq!(
            fill(ExtensionChoice::Black),
            Some(CanvasExtensionColor::new(0.0, 0.0, 0.0))
        );
        assert_eq!(
            fill(ExtensionChoice::Foreground),
            Some(CanvasExtensionColor::new(0.25, 0.5, 0.75))
        );
        assert_eq!(
            fill(ExtensionChoice::White),
            Some(CanvasExtensionColor::new(1.0, 1.0, 1.0))
        );
        assert_eq!(
            fill(ExtensionChoice::Background),
            Some(CanvasExtensionColor::new(0.1, 0.2, 0.3))
        );
        assert_eq!(
            fill(ExtensionChoice::Custom),
            Some(CanvasExtensionColor::new(1.0, 0.0, 1.0))
        );
    }

    /// `scrubRange`: the locked range keeps the original ratio, and the displayed ends are the
    /// binding's own unit.
    #[test]
    fn a_locked_scrub_keeps_the_original_ratio() {
        let mut draft = CanvasSizeDraft::new(1000, 500, 100.0);
        assert_eq!(scrub_range(&draft, true), (1.0, 30_000.0));
        draft.locked = true;
        assert_eq!(scrub_range(&draft, true), (2.0, 30_000.0));
        assert_eq!(scrub_range(&draft, false), (1.0, 15_000.0));

        // Relative measures from the original size, and each unit divides by its own scale.
        draft.relative = true;
        assert_eq!(scrub_range(&draft, true), (-998.0, 29_000.0));
        draft.relative = false;
        draft.unit = CanvasUnit::Percent;
        assert_eq!(scrub_range(&draft, true), (0.2, 3000.0));
        draft.unit = CanvasUnit::Inches;
        assert_eq!(scrub_range(&draft, true), (0.02, 300.0));
        draft.unit = CanvasUnit::Centimeters;
        // Swift evaluates `difference / draft.resolution * 2.54`; left-to-right IEEE
        // arithmetic makes the lower end 2.0 / 100.0 * 2.54, not the clean decimal literal.
        assert_eq!(
            scrub_range(&draft, true),
            (2.0 / 100.0 * 2.54, 30_000.0 / 100.0 * 2.54)
        );
    }

    #[test]
    fn scrub_sensitivity_follows_the_unit() {
        let mut draft = CanvasSizeDraft::new(1000, 500, 100.0);
        assert_eq!(scrub_sensitivity(&draft, true), 1.0);
        assert_eq!(scrub_sensitivity(&draft, false), 1.0);
        draft.unit = CanvasUnit::Percent;
        assert_eq!(scrub_sensitivity(&draft, true), 0.1);
        assert_eq!(scrub_sensitivity(&draft, false), 0.2);
        draft.unit = CanvasUnit::Inches;
        assert_eq!(scrub_sensitivity(&draft, true), 0.01);
        draft.unit = CanvasUnit::Centimeters;
        assert_eq!(scrub_sensitivity(&draft, true), 0.0254);
    }

    #[test]
    fn byte_counts_use_foundations_memory_style() {
        assert_eq!(memory_byte_count(0), "Zero KB");
        assert_eq!(memory_byte_count(1), "1 byte");
        // Foundation groups the byte count itself: Darwin's own documented example reads
        // "723 KB (722,842 bytes)".
        assert_eq!(memory_byte_count(1000), "1,000 bytes");
        assert_eq!(memory_byte_count(1024), "1 KB");
        assert_eq!(memory_byte_count(64 * 64 * 4), "16 KB");
        assert_eq!(memory_byte_count(40_000), "39 KB");
        assert_eq!(memory_byte_count(1920 * 1080 * 4), "7.9 MB");
        assert_eq!(memory_byte_count(4000 * 3000 * 4), "45.8 MB");
        assert_eq!(memory_byte_count(3_000_000_000), "2.79 GB");
    }

    #[test]
    fn the_limit_message_groups_its_digits() {
        assert_eq!(grouped(MAX_SIDE as i64), "30,000");
        assert_eq!(grouped(999), "999");
    }

    /// The document's draft and the sheet's own defaults, through a real window.
    #[gpui_kit::test]
    fn the_sheet_starts_from_the_document_and_hands_back_its_options(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let session = cx.update(|cx| {
            let session = cx.new(|_| EditorSession::new());
            session.update(cx, |session, _| {
                session.document = Some(CanvasDocument::new(1920, 1080));
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
                CanvasSizeSheet::open(session.clone(), |_, _, _| {}, window, cx)
            })
            .expect("open the sheet");

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let (original_width, original_height, resolution) = {
                let draft = sheet.read(cx).draft();
                (draft.original_width, draft.original_height, draft.resolution)
            };
            assert_eq!((original_width, original_height, resolution), (1920, 1080, 72.0));
            assert!(sheet.read(cx).valid());

            // The middle anchor and a transparent fill are the Swift's defaults.
            let options = sheet.read(cx).options();
            assert_eq!((options.width, options.height), (1920, 1080));
            assert_eq!(options.anchor, 4);
            assert_eq!(options.fill, None);
            assert_eq!(options.content_offset, None);

            sheet.update(cx, |sheet, cx| {
                sheet.anchor = 0;
                sheet.extension_choice = ExtensionChoice::Foreground;
                cx.notify();
            });
            let options = sheet.read(cx).options();
            let foreground = sheet.read(cx).foreground;
            assert_eq!(options.anchor, 0);
            assert_eq!(
                options.fill,
                Some(CanvasExtensionColor::new(
                    foreground.red,
                    foreground.green,
                    foreground.blue
                ))
            );

            // A size outside 1…30,000 turns OK off, as `.disabled(!draft.valid)` does.
            sheet.update(cx, |sheet, cx| {
                sheet.draft.set(30_001.0, true);
                cx.notify();
            });
            assert!(!sheet.read(cx).valid());
        })
        .unwrap();
    }
}
