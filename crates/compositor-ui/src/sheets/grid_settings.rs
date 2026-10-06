//! View > Grid Settings…: the layout grid's spacing and its look, with the canvas following every
//! change (port of `UI/GridSettingsSheet.swift`).
//!
//! The sheet is a component dialog around the Swift `VStack`; its Cancel / Restore Defaults / OK
//! row is the Swift's own button row inside the content, and Return and Escape are the dialog's
//! shortcuts (the Swift's `configuredNativeShortcut(.return)` and `.escape`). Every change shows on
//! the canvas at once through `preview` — while the draft holds together, as the Swift's
//! `onChange` guards — and Cancel puts back what was there.
//!
//! Substitutions: the Swift's `Picker` with `.labelsHidden()` is the port's menu-style picker
//! (`menu_picker`), the rounded-border number fields are the port's own fields (`Fields`), and
//! `.help(_:)` is a tooltip.

use std::cell::RefCell;
use std::rc::Rc;

use compositor_core::guides::{GridAppearance, GridAppearancePreset, GridAppearanceStyle, LayoutGrid};
use compositor_core::PaletteColor;
use compositor_session::EditorSession;

use crate::panels::color_picker::DialogColorSwatch;
use crate::tool_controls::{menu_picker, unit_suffix, FieldSpec, Fields};
use crate::toolbar::status_bar::SECONDARY;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

/// The sheet's content width, `.frame(width: 360)`.
const WIDTH: f32 = 360.0;
/// The component dialog's own padding, 16 points on each side.
const DIALOG_PADDING: f32 = 16.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The `VStack(alignment: .leading, spacing: 18)`.
const SPACING: f32 = 18.0;
/// The rows' `HStack` spacing.
const ROW_SPACING: f32 = 8.0;
/// A row label's `.frame(width: 110, alignment: .leading)`.
const LABEL_WIDTH: f32 = 110.0;
/// The opacity field's `.frame(width: 48)`.
const OPACITY_FIELD_WIDTH: f32 = 48.0;
/// The "Gridline every" field's width: what the label, the row's gaps and the "pixels" unit leave
/// of the sheet's 312-point content (the Swift field filled the row).
const GRIDLINE_FIELD_WIDTH: f32 = 150.0;
/// The "Subdivisions" field's width: the same row without a unit after the field.
const SUBDIVISION_FIELD_WIDTH: f32 = 190.0;
/// The title's size, `.title2.bold()`.
const TITLE_SIZE: f32 = 17.0;
/// The message's `.font(.callout)`.
const MESSAGE_SIZE: f32 = 13.0;
/// `Color.orange` (#FF9500), the invalid draft's `.foregroundStyle(.orange)`.
const ORANGE: Hsla = hsla(35.0 / 360.0, 1.0, 0.5, 1.0);

/// What the sheet hands back when it closes: the grid and its look, or `None` on Cancel
/// (`finish((LayoutGrid, GridAppearance)?)`).
pub type GridFinish = Box<dyn FnOnce(Option<(LayoutGrid, GridAppearance)>, &mut Window, &mut App)>;

/// `valid`: `LayoutGrid.spacingRange` contains spacing, `subdivisionRange` contains subdivisions,
/// and there is never more than one subdivision per pixel between gridlines.
fn valid(spacing: usize, subdivisions: usize) -> bool {
    LayoutGrid::SPACING_RANGE.contains(&spacing)
        && LayoutGrid::SUBDIVISION_RANGE.contains(&subdivisions)
        && subdivisions <= spacing
}

/// The line under the fields: the step when the draft holds together, and what is allowed when it
/// does not.
fn message(spacing: usize, subdivisions: usize) -> String {
    if valid(spacing, subdivisions) {
        // `Double(grid.step).formatted(.number.precision(.fractionLength(0...2)))`.
        let step = LayoutGrid::new(spacing, subdivisions).step();
        format!("A subdivision every {} pixels.", crate::tool_controls::format_number(step, 2))
    } else {
        format!(
            "Use gridlines every {}–{} pixels and {}–{} subdivisions, no more than the pixels between gridlines.",
            LayoutGrid::SPACING_RANGE.start(),
            grouped(*LayoutGrid::SPACING_RANGE.end()),
            LayoutGrid::SUBDIVISION_RANGE.start(),
            LayoutGrid::SUBDIVISION_RANGE.end(),
        )
    }
}

/// `Int.formatted()`: the number with the en-US grouping the message spells out ("4,096").
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

/// The sheet's state: the grid being drafted, its look, and the preset the color pick began from.
pub struct GridSettingsSheet {
    session: Entity<EditorSession>,
    /// `@State private var spacing`.
    spacing: usize,
    /// `@State private var subdivisions`.
    subdivisions: usize,
    /// `@State private var appearance`.
    appearance: GridAppearance,
    /// The preset in use when the picker first moved the color; Cancel in the picker puts it back
    /// (`pickedFrom`).
    picked_from: Option<GridAppearancePreset>,
    /// `preview`: every change the canvas follows.
    preview: Rc<dyn Fn(LayoutGrid, GridAppearance, &mut App)>,
    /// `finish`.
    finish: Option<GridFinish>,
    /// The color the picker last reported. The swatch's callback carries no `cx` of its own, so the
    /// color lands here and is drained on the sheet's next frame.
    picked: Rc<RefCell<Option<PaletteColor>>>,
    /// The number fields, made the first time each is drawn (the sheet is built without a window).
    fields: Fields,
    /// The opacity slider, made on the first frame.
    opacity_slider: Option<Entity<SliderState>>,
}

impl GridSettingsSheet {
    /// A draft over `grid` and `appearance`, with the Swift's `@State` initialized from them.
    pub fn new(
        session: Entity<EditorSession>,
        grid: LayoutGrid,
        appearance: GridAppearance,
        preview: Rc<dyn Fn(LayoutGrid, GridAppearance, &mut App)>,
        finish: GridFinish,
    ) -> Self {
        Self {
            session,
            spacing: grid.spacing,
            subdivisions: grid.subdivisions,
            appearance,
            picked_from: None,
            preview,
            finish: Some(finish),
            picked: Rc::new(RefCell::new(None)),
            fields: Fields::default(),
            opacity_slider: None,
        }
    }

    /// Opens the sheet as a modal dialog and returns the view it renders.
    pub fn open(
        session: Entity<EditorSession>,
        grid: LayoutGrid,
        appearance: GridAppearance,
        preview: impl Fn(LayoutGrid, GridAppearance, &mut App) + 'static,
        finish: impl FnOnce(Option<(LayoutGrid, GridAppearance)>, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let observed = session.clone();
        let view = cx.new(|_| {
            Self::new(session, grid, appearance, Rc::new(preview), Box::new(finish))
        });
        view.update(cx, |sheet, cx| {
            // `DialogColorSwatch`'s `.onChange(of: session.colorPicker?.color)`: the swatch follows
            // the picker's working color. Its callback has no `cx`, so the color it reports is
            // drained on every session change and on every frame.
            sheet.drain_picked(cx);
            cx.observe(&observed, |sheet, _, cx| {
                sheet.drain_picked(cx);
                cx.notify();
            })
            .detach();
        });
        window.open_dialog(cx, {
            let view = view.clone();
            move |dialog, _, _| {
                let ok = view.clone();
                let cancel = view.clone();
                dialog
                    .title("Grid")
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
                                sheet.confirm(window, cx);
                                confirmed = true;
                            }
                        });
                        confirmed
                    })
                    .on_cancel(move |_, window, cx| {
                        cancel.update(cx, |sheet, cx| sheet.cancel(window, cx));
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

    /// The grid the draft describes (`LayoutGrid(spacing:subdivisions:)`).
    pub fn grid(&self) -> LayoutGrid {
        LayoutGrid::new(self.spacing, self.subdivisions)
    }

    /// The appearance the draft shows.
    pub fn appearance(&self) -> GridAppearance {
        self.appearance
    }

    /// `valid`: OK is live while the draft holds together.
    pub fn valid(&self) -> bool {
        valid(self.spacing, self.subdivisions)
    }

    /// `.onChange(of: appearance)` / `.onChange(of: spacing)` / `.onChange(of: subdivisions)`:
    /// every valid change shows on the canvas at once.
    fn preview(&self, cx: &mut App) {
        if self.valid() {
            (self.preview)(self.grid(), self.appearance, cx);
        }
    }

    /// A change the canvas follows, which also redraws the sheet.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.preview(cx);
        cx.notify();
    }

    /// `finish((grid, appearance))`, with the picker put away first (`DialogColorSwatch.closePicker`).
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_picker(cx);
        let finish = self.finish.take();
        if let Some(finish) = finish {
            finish(Some((self.grid(), self.appearance)), window, cx);
        }
    }

    /// `finish(nil)`: the caller puts back what was there.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_picker(cx);
        let finish = self.finish.take();
        if let Some(finish) = finish {
            finish(None, window, cx);
        }
    }

    /// `DialogColorSwatch.closePicker(session)`.
    fn close_picker(&self, cx: &mut App) {
        DialogColorSwatch::close_picker(&self.session, cx);
    }

    /// The spacing the field or the label landed on (`$spacing`), held in its range as every field's
    /// binding is.
    fn set_spacing(&mut self, value: f64, cx: &mut Context<Self>) {
        let spacing = value.round().clamp(
            *LayoutGrid::SPACING_RANGE.start() as f64,
            *LayoutGrid::SPACING_RANGE.end() as f64,
        ) as usize;
        if spacing == self.spacing {
            return;
        }
        self.spacing = spacing;
        self.changed(cx);
    }

    /// The subdivisions the field or the label landed on (`$subdivisions`).
    fn set_subdivisions(&mut self, value: f64, cx: &mut Context<Self>) {
        let subdivisions = value.round().clamp(
            *LayoutGrid::SUBDIVISION_RANGE.start() as f64,
            *LayoutGrid::SUBDIVISION_RANGE.end() as f64,
        ) as usize;
        if subdivisions == self.subdivisions {
            return;
        }
        self.subdivisions = subdivisions;
        self.changed(cx);
    }

    /// `setOpacity(_:)`: the value held inside `GridAppearance.opacityRange`.
    fn set_opacity(&mut self, value: f64, cx: &mut Context<Self>) {
        let opacity = (value.round() as i64).clamp(
            *GridAppearance::OPACITY_RANGE.start() as i64,
            *GridAppearance::OPACITY_RANGE.end() as i64,
        ) as usize;
        if opacity == self.appearance.opacity {
            return;
        }
        self.appearance.opacity = opacity;
        self.changed(cx);
    }

    /// `Picker("Color", selection: $appearance.preset)`.
    fn set_preset(&mut self, preset: GridAppearancePreset, cx: &mut Context<Self>) {
        if preset == self.appearance.preset {
            return;
        }
        self.appearance.preset = preset;
        // `.onChange(of: appearance.preset) { _, preset in if preset != .custom { pickedFrom = nil } }`:
        // choosing a preset from the menu ends a pick that started from another.
        if preset != GridAppearancePreset::Custom {
            self.picked_from = None;
        }
        self.changed(cx);
    }

    /// `Picker("Style", selection: $appearance.style)`.
    fn set_style(&mut self, style: GridAppearanceStyle, cx: &mut Context<Self>) {
        if style == self.appearance.style {
            return;
        }
        self.appearance.style = style;
        self.changed(cx);
    }

    /// `Restore Defaults`: the default spacing and look. The Custom color is kept, so it is still
    /// there if Custom is chosen again.
    fn restore_defaults(&mut self, cx: &mut Context<Self>) {
        let grid = LayoutGrid::default();
        let appearance = GridAppearance::default();
        self.spacing = grid.spacing;
        self.subdivisions = grid.subdivisions;
        self.picked_from = None;
        self.appearance.preset = appearance.preset;
        self.appearance.style = appearance.style;
        self.appearance.opacity = appearance.opacity;
        self.changed(cx);
    }

    /// Takes the color the picker last reported, if any (`swatchColor`'s setter).
    fn drain_picked(&mut self, cx: &mut Context<Self>) {
        let picked = self.picked.borrow_mut().take();
        if let Some(picked) = picked {
            self.apply_picked(picked, cx);
        }
    }

    /// The swatch's `Binding` setter: the picker reports the color it opened on too, and that alone
    /// leaves the preset chosen; picking one makes it the Custom color.
    fn apply_picked(&mut self, picked: PaletteColor, cx: &mut Context<Self>) {
        if picked == self.appearance.color() {
            return;
        }
        if let Some(from) = self.picked_from {
            // `if let pickedFrom, picked == pickedFrom.color`: Cancel in the picker puts the preset
            // the pick began from back.
            if from.color() == Some(picked) {
                self.appearance.preset = from;
                self.picked_from = None;
                self.changed(cx);
                return;
            }
        }
        if self.appearance.preset != GridAppearancePreset::Custom {
            self.picked_from = Some(self.appearance.preset);
        }
        self.appearance.custom_color = picked;
        self.appearance.preset = GridAppearancePreset::Custom;
        self.changed(cx);
    }

    /// The opacity slider, made on the first frame: the Swift `Slider(value:in:)` whose setter
    /// rounds into the appearance and whose getter follows it.
    fn opacity_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<SliderState> {
        let slider = match self.opacity_slider.clone() {
            Some(slider) => slider,
            None => {
                let opacity = self.appearance.opacity as f32;
                let slider = window.use_keyed_state(
                    "grid-opacity-slider",
                    cx,
                    move |_, cx| {
                        SliderState::new()
                            .min(*GridAppearance::OPACITY_RANGE.start() as f32)
                            .max(*GridAppearance::OPACITY_RANGE.end() as f32)
                            .default_value(opacity)
                    },
                );
                cx.subscribe(&slider, |sheet, _, event: &SliderEvent, cx| {
                    let value = match event {
                        SliderEvent::Change(SliderValue::Single(value))
                        | SliderEvent::Release(SliderValue::Single(value)) => *value,
                        _ => return,
                    };
                    sheet.set_opacity(f64::from(value), cx);
                })
                .detach();
                self.opacity_slider = Some(slider.clone());
                slider
            }
        };
        // `get: { Double(appearance.opacity) }`: a value the field or the label moved drags the knob
        // with it.
        let wanted = self.appearance.opacity as f32;
        if slider.read(cx).value() != SliderValue::Single(wanted) {
            slider.update(cx, |state, cx| state.set_value(wanted, window, cx));
        }
        slider
    }

    /// A row's label column: `.frame(width: 110, alignment: .leading)`.
    fn label_column(label: &'static str) -> Div {
        div().flex_shrink_0().w(px(LABEL_WIDTH)).child(label)
    }

    /// The Color row: the preset menu and the swatch that opens the app's picker.
    fn color_row(&self, entity: &Entity<Self>) -> impl IntoElement {
        let appearance = self.appearance;
        let picked = self.picked.clone();
        let picker = menu_picker(
            "grid-color-preset",
            GridAppearancePreset::ALL.map(|preset| (preset, preset.raw_value())),
            appearance.preset,
            {
                let entity = entity.clone();
                move |preset, _, cx| entity.update(cx, |sheet, cx| sheet.set_preset(preset, cx))
            },
        );
        h_flex()
            .gap(px(ROW_SPACING))
            .child(Self::label_column("Color"))
            .child(picker)
            .child(
                div()
                    .id("grid-color-help")
                    .flex_shrink_0()
                    .tooltip(|window, cx| Tooltip::new("Choose a custom grid color").build(window, cx))
                    .child(DialogColorSwatch::new(
                        self.session.clone(),
                        "Grid Color",
                        appearance.color(),
                        move |color| {
                            *picked.borrow_mut() = Some(color);
                        },
                    )),
            )
    }

    /// The Style row: the major lines' pattern.
    fn style_row(&self, entity: &Entity<Self>) -> impl IntoElement {
        let appearance = self.appearance;
        let picker = menu_picker(
            "grid-style",
            GridAppearanceStyle::ALL.map(|style| (style, style.raw_value())),
            appearance.style,
            {
                let entity = entity.clone();
                move |style, _, cx| entity.update(cx, |sheet, cx| sheet.set_style(style, cx))
            },
        );
        h_flex()
            .gap(px(ROW_SPACING))
            .child(Self::label_column("Style"))
            .child(picker)
    }

    /// The Opacity row: the scrub label, the slider and the percent field.
    fn opacity_row(
        &mut self,
        entity: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let opacity = self.appearance.opacity;
        let label = {
            let entity = entity.clone();
            let scrub = NumericScrub::new(
                opacity as f64,
                0.5,
                (
                    *GridAppearance::OPACITY_RANGE.start() as f64,
                    *GridAppearance::OPACITY_RANGE.end() as f64,
                ),
            )
            .whole_numbers()
            .on_change(move |value, _, cx| {
                entity.update(cx, |sheet, cx| sheet.set_opacity(value, cx))
            });
            div()
                .flex_shrink_0()
                .w(px(LABEL_WIDTH))
                .child("Opacity")
                .scrubbable("grid-opacity-label", scrub)
        };
        let slider = self.opacity_slider(window, cx);
        let field = {
            let entity = entity.clone();
            self.fields.get("grid-opacity-field", cx).element(
                "grid-opacity-field",
                opacity as f64,
                FieldSpec::new(
                    (
                        *GridAppearance::OPACITY_RANGE.start() as f64,
                        *GridAppearance::OPACITY_RANGE.end() as f64,
                    ),
                    0,
                ),
                OPACITY_FIELD_WIDTH,
                move |value, _, cx| entity.update(cx, |sheet, cx| sheet.set_opacity(value, cx)),
                cx,
            )
        };
        h_flex()
            .gap(px(ROW_SPACING))
            .child(label)
            .child(div().flex_1().min_w(px(0.0)).child(Slider::new(&slider)))
            .child(unit_suffix(field, div().child("%")))
    }

    /// The "Gridline every" row: the scrub label, the field and its unit.
    fn spacing_row(&mut self, entity: &Entity<Self>, cx: &mut Context<Self>) -> impl IntoElement {
        let spacing = self.spacing;
        let label = {
            let entity = entity.clone();
            let scrub = NumericScrub::new(
                spacing as f64,
                1.0,
                (
                    *LayoutGrid::SPACING_RANGE.start() as f64,
                    *LayoutGrid::SPACING_RANGE.end() as f64,
                ),
            )
            .whole_numbers()
            .on_change(move |value, _, cx| {
                entity.update(cx, |sheet, cx| sheet.set_spacing(value, cx))
            });
            div()
                .flex_shrink_0()
                .w(px(LABEL_WIDTH))
                .child("Gridline every")
                .scrubbable("grid-spacing-label", scrub)
        };
        let field = {
            let entity = entity.clone();
            self.fields.get("grid-spacing-field", cx).element(
                "grid-spacing-field",
                spacing as f64,
                FieldSpec::new(
                    (
                        *LayoutGrid::SPACING_RANGE.start() as f64,
                        *LayoutGrid::SPACING_RANGE.end() as f64,
                    ),
                    0,
                ),
                GRIDLINE_FIELD_WIDTH,
                move |value, _, cx| entity.update(cx, |sheet, cx| sheet.set_spacing(value, cx)),
                cx,
            )
        };
        h_flex()
            .gap(px(ROW_SPACING))
            .child(label)
            .child(field)
            .child(div().text_color(SECONDARY).child("pixels"))
    }

    /// The Subdivisions row.
    fn subdivisions_row(&mut self, entity: &Entity<Self>, cx: &mut Context<Self>) -> impl IntoElement {
        let subdivisions = self.subdivisions;
        let label = {
            let entity = entity.clone();
            let scrub = NumericScrub::new(
                subdivisions as f64,
                0.2,
                (
                    *LayoutGrid::SUBDIVISION_RANGE.start() as f64,
                    *LayoutGrid::SUBDIVISION_RANGE.end() as f64,
                ),
            )
            .whole_numbers()
            .on_change(move |value, _, cx| {
                entity.update(cx, |sheet, cx| sheet.set_subdivisions(value, cx))
            });
            div()
                .flex_shrink_0()
                .w(px(LABEL_WIDTH))
                .child("Subdivisions")
                .scrubbable("grid-subdivisions-label", scrub)
        };
        let field = {
            let entity = entity.clone();
            self.fields.get("grid-subdivisions-field", cx).element(
                "grid-subdivisions-field",
                subdivisions as f64,
                FieldSpec::new(
                    (
                        *LayoutGrid::SUBDIVISION_RANGE.start() as f64,
                        *LayoutGrid::SUBDIVISION_RANGE.end() as f64,
                    ),
                    0,
                ),
                SUBDIVISION_FIELD_WIDTH,
                move |value, _, cx| {
                    entity.update(cx, |sheet, cx| sheet.set_subdivisions(value, cx))
                },
                cx,
            )
        };
        h_flex().gap(px(ROW_SPACING)).child(label).child(field)
    }

    /// The `HStack` of Cancel, Restore Defaults and the prominent OK.
    fn buttons(&self, entity: &Entity<Self>, valid: bool) -> impl IntoElement {
        let cancel = {
            let entity = entity.clone();
            Button::new("grid-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| {
                    entity.update(cx, |sheet, cx| {
                        sheet.cancel(window, cx);
                        window.close_dialog(cx);
                    });
                })
        };
        let restore = {
            let entity = entity.clone();
            Button::new("grid-restore-defaults")
                .label("Restore Defaults")
                .on_click(move |_, _, cx| {
                    entity.update(cx, |sheet, cx| sheet.restore_defaults(cx))
                })
        };
        let ok = {
            let entity = entity.clone();
            Button::new("grid-ok")
                .label("OK")
                .primary()
                .disabled(!valid)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |sheet, cx| {
                        if sheet.valid() {
                            sheet.confirm(window, cx);
                            window.close_dialog(cx);
                        }
                    });
                })
        };
        h_flex()
            .gap(px(ROW_SPACING))
            .child(cancel)
            .child(restore)
            // `Spacer()`.
            .child(div().flex_1())
            .child(ok)
    }
}

impl Render for GridSettingsSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drain_picked(cx);
        let valid = self.valid();
        let entity = cx.entity();
        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .child(
                div()
                    .text_size(px(TITLE_SIZE))
                    .font_weight(FontWeight::BOLD)
                    .child("Grid"),
            )
            .child(self.color_row(&entity))
            .child(self.style_row(&entity))
            .child(self.opacity_row(&entity, window, cx))
            .child(Separator::horizontal())
            .child(self.spacing_row(&entity, cx))
            .child(self.subdivisions_row(&entity, cx))
            .child(
                div()
                    .text_size(px(MESSAGE_SIZE))
                    .text_color(if valid { SECONDARY } else { ORANGE })
                    .child(message(self.spacing, self.subdivisions)),
            )
            .child(self.buttons(&entity, valid))
    }
}

#[cfg(test)]
mod tests {
    // The builtin `#[test]`: the file's `use gpui_kit::*;` glob would otherwise shadow it with the
    // toolkit's `test` attribute macro (enabled by the test-support dev-dependency).
    use ::core::prelude::v1::test;
    use super::*;

    #[test]
    fn a_draft_holds_together_inside_the_swifts_ranges() {
        assert!(valid(64, 8));
        assert!(valid(2, 1));
        assert!(valid(4096, 64));
        // `LayoutGrid.spacingRange` starts at 2 and ends at 4,096.
        assert!(!valid(1, 1));
        assert!(!valid(4097, 8));
        // `LayoutGrid.subdivisionRange` holds 1…64.
        assert!(!valid(64, 0));
        assert!(!valid(64, 65));
        // No more subdivisions than pixels between gridlines.
        assert!(!valid(4, 5));
        assert!(valid(4, 4));
    }

    #[test]
    fn the_message_counts_the_step_or_says_what_is_allowed() {
        assert_eq!(message(64, 8), "A subdivision every 8 pixels.");
        assert_eq!(message(64, 10), "A subdivision every 6.4 pixels.");
        assert_eq!(message(100, 3), "A subdivision every 33.33 pixels.");
        assert_eq!(
            message(4, 8),
            "Use gridlines every 2–4,096 pixels and 1–64 subdivisions, no more than the pixels between gridlines."
        );
    }

    #[test]
    fn the_larger_bound_is_grouped() {
        assert_eq!(grouped(4096), "4,096");
        assert_eq!(grouped(64), "64");
        assert_eq!(grouped(1000000), "1,000,000");
    }
}
