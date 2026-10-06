//! One effect's controls, bound to the layer that opened the panel. Changes preview on the canvas
//! (port of `UI/EffectsSheet.swift`).
//!
//! The panel is built for the kind it was opened on (`EffectsSheet(session:kind:)`); while the picker
//! is open on the effect's color, every move of its working color is previewed on the layer, which is
//! the Swift's `.onChange(of: session.colorPicker?.color)`. The host draws the panel's chrome — title
//! `kind.rawValue`, frame — so this view keeps only the Swift's
//! `.padding(20).frame(width: 340).fixedSize()` content, in the port's panel type (12-point controls).
//! The Swift's `Slider`es are the port's plain-track [`CameraRawSlider`], the one the tool bars use for
//! the macOS sliders, and the buttons' `configuredNativeShortcut`s (Escape and Return) are captured by
//! the sheet's own root, since the floating panel's host does not route them.

use compositor_core::color::PaletteColor;
use compositor_core::layer_effects::{LayerEffectKind, LayerEffects, StrokeEffect};
use compositor_session::EditorSession;

use crate::canvas::overlays::palette_rgba;
use crate::tool_controls::{segmented_picker, unit_suffix, FieldSpec, Fields};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::Button;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

/// `.frame(width: 340)` — the panel's width, padding included.
const WIDTH: f32 = 340.0;
/// `.padding(20)`.
const PADDING: f32 = 20.0;
/// The `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// A slider row's `HStack(spacing: 10)`.
const ROW_SPACING: f32 = 10.0;
/// The Color row's plain `HStack`, whose spacing is SwiftUI's default 8.
const COLOR_ROW_SPACING: f32 = 8.0;
/// The bottom row's `HStack(spacing: 10)`.
const BUTTON_SPACING: f32 = 10.0;
/// `Text("…").font(.headline)` — 13 points, semibold.
const TITLE_SIZE: f32 = 13.0;
/// `Text(title).frame(width: 64, alignment: .leading)`.
const LABEL_WIDTH: f32 = 64.0;
/// `Slider(…).frame(width: 130)`.
const SLIDER_WIDTH: f32 = 130.0;
/// `TextField(…).frame(width: 48)`.
const FIELD_WIDTH: f32 = 48.0;
/// The swatch: `frame(width: 36, height: 18)` in a `RoundedRectangle(cornerRadius: 3)`, its white border
/// inset by one.
const SWATCH_WIDTH: f32 = 36.0;
const SWATCH_HEIGHT: f32 = 18.0;
const SWATCH_RADIUS: f32 = 3.0;

/// The Stroke Size slider's range (`range: 0...20`).
const STROKE_SIZE_RANGE: (f64, f64) = (0.0, 20.0);
/// The stroke Size limits (`inputRange: 0...StrokeEffect.maxSize`).
const STROKE_SIZE_LIMITS: (f64, f64) = (0.0, StrokeEffect::MAX_SIZE);
/// The glows' Size sliders (`range: 0...100`, `inputRange: 0...500`).
const GLOW_SIZE_RANGE: (f64, f64) = (0.0, 100.0);
const GLOW_SIZE_LIMITS: (f64, f64) = (0.0, 500.0);
/// Every Opacity slider (`range: 0...100`, no `inputRange`).
const OPACITY_RANGE: (f64, f64) = (0.0, 100.0);
/// The shadows' Angle sliders (`range: -180...180`).
const ANGLE_RANGE: (f64, f64) = (-180.0, 180.0);
/// The drop shadow's Distance slider (`range: 0...100`) and the inner shadow's (`range: 0...50`), both
/// limited to `inputRange: 0...5000`.
const SHADOW_DISTANCE_RANGE: (f64, f64) = (0.0, 100.0);
const INNER_SHADOW_DISTANCE_RANGE: (f64, f64) = (0.0, 50.0);
const DISTANCE_LIMITS: (f64, f64) = (0.0, 5000.0);
/// The shadows' Blur sliders (`range: 0...100`, `inputRange: 0...500`).
const BLUR_RANGE: (f64, f64) = (0.0, 100.0);
const BLUR_LIMITS: (f64, f64) = (0.0, 500.0);

/// One of the Swift's `slider(_:value:range:inputRange:unit:)` rows, one per title it is called with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Row {
    Size,
    Opacity,
    Angle,
    Distance,
    Blur,
}

impl Row {
    /// The rows a kind's panel shows, in the Swift's own order.
    fn rows(kind: LayerEffectKind) -> &'static [Row] {
        match kind {
            LayerEffectKind::Stroke => &[Row::Size, Row::Opacity],
            LayerEffectKind::Shadow | LayerEffectKind::InnerShadow => {
                &[Row::Opacity, Row::Angle, Row::Distance, Row::Blur]
            }
            LayerEffectKind::ColorOverlay => &[Row::Opacity],
            LayerEffectKind::OuterGlow | LayerEffectKind::InnerGlow => &[Row::Size, Row::Opacity],
        }
    }

    /// The row's title, which scrubs its value.
    fn title(self) -> &'static str {
        match self {
            Row::Size => "Size",
            Row::Opacity => "Opacity",
            Row::Angle => "Angle",
            Row::Distance => "Distance",
            Row::Blur => "Blur",
        }
    }

    /// `.unitSuffix(unit)`.
    fn unit(self) -> &'static str {
        match self {
            Row::Size | Row::Distance | Row::Blur => "px",
            Row::Opacity => "%",
            Row::Angle => "°",
        }
    }

    /// The field's and the label's own ids: one row's title and field are told apart from another's.
    fn field_id(self) -> &'static str {
        match self {
            Row::Size => "effects-size",
            Row::Opacity => "effects-opacity",
            Row::Angle => "effects-angle",
            Row::Distance => "effects-distance",
            Row::Blur => "effects-blur",
        }
    }

    fn label_id(self) -> &'static str {
        match self {
            Row::Size => "effects-size-label",
            Row::Opacity => "effects-opacity-label",
            Row::Angle => "effects-angle-label",
            Row::Distance => "effects-distance-label",
            Row::Blur => "effects-blur-label",
        }
    }

    fn slider_id(self) -> &'static str {
        match self {
            Row::Size => "effects-size-slider",
            Row::Opacity => "effects-opacity-slider",
            Row::Angle => "effects-angle-slider",
            Row::Distance => "effects-distance-slider",
            Row::Blur => "effects-blur-slider",
        }
    }

    /// The slider's own range (`range:`), which only pins the thumb.
    fn slider_range(self, kind: LayerEffectKind) -> (f64, f64) {
        match self {
            Row::Size => match kind {
                LayerEffectKind::Stroke => STROKE_SIZE_RANGE,
                _ => GLOW_SIZE_RANGE,
            },
            Row::Opacity => OPACITY_RANGE,
            Row::Angle => ANGLE_RANGE,
            Row::Distance => match kind {
                LayerEffectKind::InnerShadow => INNER_SHADOW_DISTANCE_RANGE,
                _ => SHADOW_DISTANCE_RANGE,
            },
            Row::Blur => BLUR_RANGE,
        }
    }

    /// The limits a typed or scrubbed amount is held inside (`inputRange ?? range`).
    fn limits(self, kind: LayerEffectKind) -> (f64, f64) {
        match self {
            Row::Size => match kind {
                LayerEffectKind::Stroke => STROKE_SIZE_LIMITS,
                _ => GLOW_SIZE_LIMITS,
            },
            Row::Opacity => OPACITY_RANGE,
            Row::Angle => ANGLE_RANGE,
            Row::Distance => DISTANCE_LIMITS,
            Row::Blur => BLUR_LIMITS,
        }
    }

    /// What the row's binding reads (`Binding(get:)` in each of the Swift's `slider(…)` calls).
    fn value(self, kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
        match self {
            Row::Size => size_of(kind, effects),
            // `CGFloat(effect.opacity * 100)`.
            Row::Opacity => opacity_of(kind, effects).map(|opacity| opacity * 100.0),
            Row::Angle => angle_of(kind, effects),
            Row::Distance => distance_of(kind, effects),
            Row::Blur => blur_of(kind, effects),
        }
    }

    /// What the row's binding writes (`session.changeEffects { $0.<effect>?.<field> = … }`).
    fn set(self, kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
        match self {
            Row::Size => set_size(kind, amount, effects),
            // `Double(value) / 100`.
            Row::Opacity => set_opacity(kind, amount / 100.0, effects),
            Row::Angle => set_angle(kind, amount, effects),
            Row::Distance => set_distance(kind, amount, effects),
            Row::Blur => set_blur(kind, amount, effects),
        }
    }
}

/// `$0.stroke?.size` (and the glows' own).
fn size_of(kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
    match kind {
        LayerEffectKind::Stroke => effects.stroke.as_ref().map(|effect| effect.size),
        LayerEffectKind::OuterGlow => effects.outer_glow.as_ref().map(|effect| effect.size),
        LayerEffectKind::InnerGlow => effects.inner_glow.as_ref().map(|effect| effect.size),
        _ => None,
    }
}

fn set_size(kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
    match kind {
        LayerEffectKind::Stroke => {
            if let Some(effect) = effects.stroke.as_mut() {
                effect.size = amount;
            }
        }
        LayerEffectKind::OuterGlow => {
            if let Some(effect) = effects.outer_glow.as_mut() {
                effect.size = amount;
            }
        }
        LayerEffectKind::InnerGlow => {
            if let Some(effect) = effects.inner_glow.as_mut() {
                effect.size = amount;
            }
        }
        _ => {}
    }
}

/// `$0.<effect>?.opacity`, the kind's own effect.
fn opacity_of(kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
    match kind {
        LayerEffectKind::Stroke => effects.stroke.as_ref().map(|effect| effect.opacity),
        LayerEffectKind::Shadow => effects.shadow.as_ref().map(|effect| effect.opacity),
        LayerEffectKind::ColorOverlay => effects.color_overlay.as_ref().map(|effect| effect.opacity),
        LayerEffectKind::InnerShadow => effects.inner_shadow.as_ref().map(|effect| effect.opacity),
        LayerEffectKind::OuterGlow => effects.outer_glow.as_ref().map(|effect| effect.opacity),
        LayerEffectKind::InnerGlow => effects.inner_glow.as_ref().map(|effect| effect.opacity),
    }
}

fn set_opacity(kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
    match kind {
        LayerEffectKind::Stroke => {
            if let Some(effect) = effects.stroke.as_mut() {
                effect.opacity = amount;
            }
        }
        LayerEffectKind::Shadow => {
            if let Some(effect) = effects.shadow.as_mut() {
                effect.opacity = amount;
            }
        }
        LayerEffectKind::ColorOverlay => {
            if let Some(effect) = effects.color_overlay.as_mut() {
                effect.opacity = amount;
            }
        }
        LayerEffectKind::InnerShadow => {
            if let Some(effect) = effects.inner_shadow.as_mut() {
                effect.opacity = amount;
            }
        }
        LayerEffectKind::OuterGlow => {
            if let Some(effect) = effects.outer_glow.as_mut() {
                effect.opacity = amount;
            }
        }
        LayerEffectKind::InnerGlow => {
            if let Some(effect) = effects.inner_glow.as_mut() {
                effect.opacity = amount;
            }
        }
    }
}

/// `$0.shadow?.angle` and `$0.innerShadow?.angle`: the shadows alone have one.
fn angle_of(kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
    match kind {
        LayerEffectKind::Shadow => effects.shadow.as_ref().map(|effect| effect.angle),
        LayerEffectKind::InnerShadow => effects.inner_shadow.as_ref().map(|effect| effect.angle),
        _ => None,
    }
}

fn set_angle(kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
    match kind {
        LayerEffectKind::Shadow => {
            if let Some(effect) = effects.shadow.as_mut() {
                effect.angle = amount;
            }
        }
        LayerEffectKind::InnerShadow => {
            if let Some(effect) = effects.inner_shadow.as_mut() {
                effect.angle = amount;
            }
        }
        _ => {}
    }
}

fn distance_of(kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
    match kind {
        LayerEffectKind::Shadow => effects.shadow.as_ref().map(|effect| effect.distance),
        LayerEffectKind::InnerShadow => effects.inner_shadow.as_ref().map(|effect| effect.distance),
        _ => None,
    }
}

fn set_distance(kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
    match kind {
        LayerEffectKind::Shadow => {
            if let Some(effect) = effects.shadow.as_mut() {
                effect.distance = amount;
            }
        }
        LayerEffectKind::InnerShadow => {
            if let Some(effect) = effects.inner_shadow.as_mut() {
                effect.distance = amount;
            }
        }
        _ => {}
    }
}

fn blur_of(kind: LayerEffectKind, effects: &LayerEffects) -> Option<f64> {
    match kind {
        LayerEffectKind::Shadow => effects.shadow.as_ref().map(|effect| effect.blur),
        LayerEffectKind::InnerShadow => effects.inner_shadow.as_ref().map(|effect| effect.blur),
        _ => None,
    }
}

fn set_blur(kind: LayerEffectKind, amount: f64, effects: &mut LayerEffects) {
    match kind {
        LayerEffectKind::Shadow => {
            if let Some(effect) = effects.shadow.as_mut() {
                effect.blur = amount;
            }
        }
        LayerEffectKind::InnerShadow => {
            if let Some(effect) = effects.inner_shadow.as_mut() {
                effect.blur = amount;
            }
        }
        _ => {}
    }
}

/// One effect's controls.
pub struct EffectsSheet {
    session: Entity<EditorSession>,
    /// Which effect the panel was opened on (`EffectsSheet(session:kind:)`).
    kind: LayerEffectKind,
    /// The number fields, made on their first frame.
    fields: Fields,
    /// The color the picker had when it last reported, so `.onChange(of: session.colorPicker?.color)`
    /// can see it move.
    picker_color: Option<PaletteColor>,
}

impl EffectsSheet {
    pub fn new(session: Entity<EditorSession>, kind: LayerEffectKind, cx: &mut Context<Self>) -> Self {
        let picker_color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
        cx.observe(&session, |this, session, cx| {
            // The `.onChange(of: session.colorPicker?.color) { _, _ in session.previewEffectColor() }`:
            // the picker previews its working color on the layer while it is open.
            let color = session.read(cx).color_picker.as_ref().map(|picker| picker.color());
            if color != this.picker_color {
                this.picker_color = color;
                session.update(cx, |session, _| session.preview_effect_color());
            }
            cx.notify();
        })
        .detach();
        Self {
            session,
            kind,
            fields: Fields::default(),
            picker_color,
        }
    }

    /// Whether one of the panel's number fields is being typed into, so Escape and Return stay with it.
    fn field_editing(&self, cx: &App) -> bool {
        [Row::Size, Row::Opacity, Row::Angle, Row::Distance, Row::Blur]
            .into_iter()
            .any(|row| self.fields.is_editing(row.field_id(), cx))
    }

    /// The rows the panel shows: the Swift's `VStack` with its switch on the kind.
    fn content(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let kind = self.kind;
        let effects = self.session.read(cx).editing_effects();
        let exists = effects.contains(kind);
        let control = if !exists {
            // `if let effect`: the header's own control only shows while the effect exists.
            None
        } else if kind == LayerEffectKind::Stroke {
            Some(self.position_picker(
                effects.stroke.as_ref().is_some_and(|effect| effect.inside),
                cx,
            ))
        } else {
            Some(self.swatch(&effects, cx))
        };
        let mut rows = vec![self.header(control)];
        if !exists {
            return rows;
        }
        if kind == LayerEffectKind::Stroke {
            rows.push(self.color_row(&effects, cx));
        }
        for row in Row::rows(kind) {
            rows.push(self.slider(*row, &effects, cx));
        }
        rows
    }

    /// The kind's header row: `Text(kind.rawValue).font(.headline)`, a `Spacer()`, then its control.
    fn header(&self, control: Option<AnyElement>) -> AnyElement {
        h_flex()
            .items_center()
            .child(
                div()
                    .text_size(px(TITLE_SIZE))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.kind.raw_value()),
            )
            .child(div().flex_1())
            .children(control)
            .into_any_element()
    }

    /// The stroke's `Picker("Position", …)`: Outside or Inside, segmented and label-less.
    fn position_picker(&self, inside: bool, cx: &mut Context<Self>) -> AnyElement {
        let session = self.session.clone();
        segmented_picker(
            "effects-position",
            [(false, "Outside"), (true, "Inside")],
            inside,
            move |inside, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_effects(move |effects| {
                        if let Some(stroke) = effects.stroke.as_mut() {
                            stroke.inside = inside;
                        }
                    });
                });
            },
        )
        .into_any_element()
    }

    /// `swatch(_:)`: the effect's color, opened in the app's own picker.
    fn swatch(&self, effects: &LayerEffects, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.kind;
        let color = effects.color(kind).unwrap_or(PaletteColor::BLACK);
        let label = format!("{} color", kind.raw_value());
        let session = self.session.clone();
        div()
            .id("effects-swatch")
            .relative()
            .flex_none()
            .w(px(SWATCH_WIDTH))
            .h(px(SWATCH_HEIGHT))
            .rounded(px(SWATCH_RADIUS))
            .bg(palette_rgba(color))
            .border_1()
            .border_color(hsla(0.0, 0.0, 0.0, 1.0))
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .aria_label(label.clone())
            .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
            .on_click(move |_, _, cx| {
                session.update(cx, |session, _| session.open_effect_color_picker(kind));
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
            .into_any_element()
    }

    /// The stroke's `Color` row: the 64-point label and the swatch.
    fn color_row(&self, effects: &LayerEffects, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap(px(COLOR_ROW_SPACING))
            .items_center()
            .child(div().w(px(LABEL_WIDTH)).child("Color"))
            .child(self.swatch(effects, cx))
            .into_any_element()
    }

    /// The Swift `slider(_:value:range:inputRange:unit:)`: the scrubbable title, the slider and the
    /// unit-suffixed field. A typed or scrubbed amount is held inside `limits`, while the slider's own
    /// range only pins the thumb — a larger typed value stays intact until the slider is dragged again.
    fn slider(&mut self, row: Row, effects: &LayerEffects, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.kind;
        let Some(value) = row.value(kind, effects) else {
            return Empty.into_any_element();
        };
        let range = row.slider_range(kind);
        let limits = row.limits(kind);
        let field = self.fields.get(row.field_id(), cx);
        let scrub = {
            let session = self.session.clone();
            NumericScrub::new(value, 1.0, limits).on_change(move |amount, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_effects(move |effects| row.set(kind, amount, effects));
                });
            })
        };
        let slider = {
            let session = self.session.clone();
            CameraRawSlider::plain(row.slider_id(), value.clamp(range.0, range.1), range, row.title())
                .on_change(move |amount, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.change_effects(move |effects| row.set(kind, amount, effects));
                    });
                })
        };
        let write = {
            let session = self.session.clone();
            move |amount: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.change_effects(move |effects| row.set(kind, amount, effects));
                });
            }
        };
        h_flex()
            .gap(px(ROW_SPACING))
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_WIDTH))
                    .child(row.title())
                    .scrubbable(row.label_id(), scrub),
            )
            .child(div().flex_none().w(px(SLIDER_WIDTH)).child(slider))
            .child(unit_suffix(
                field.element(
                    row.field_id(),
                    value,
                    // A typed value that is no number leaves the effect where it was.
                    FieldSpec::new(limits, 0).fallback(value),
                    FIELD_WIDTH,
                    write,
                    cx,
                ),
                div().child(row.unit()),
            ))
            .into_any_element()
    }
}

impl Render for EffectsSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.content(cx);
        let cancel = self.session.clone();
        let ok = self.session.clone();
        // `.configuredNativeShortcut(.escape)` / `.return` on the two buttons, which the floating
        // panel's host does not route. A number field being typed into keeps both keys for itself.
        let keys = {
            let session = self.session.clone();
            let sheet = cx.entity();
            move |event: &KeyDownEvent, _: &mut Window, cx: &mut App| {
                if sheet.read(cx).field_editing(cx) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        let session = session.clone();
                        session.update(cx, |session, _| session.finish_effects_editing(false));
                        cx.stop_propagation();
                    }
                    "enter" | "return" => {
                        let session = session.clone();
                        session.update(cx, |session, _| session.finish_effects_editing(true));
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }
        };
        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .text_size(px(crate::tool_header::CONTROL_SIZE))
            .capture_key_down(keys)
            .children(content)
            .child(
                h_flex()
                    .gap(px(BUTTON_SPACING))
                    .justify_end()
                    .child(
                        Button::new("effects-cancel")
                            .label("Cancel")
                            .on_click(move |_, _, cx| {
                                let cancel = cancel.clone();
                                cancel.update(cx, |session, _| {
                                    session.finish_effects_editing(false)
                                });
                            }),
                    )
                    .child(Button::new("effects-ok").label("OK").on_click(move |_, _, cx| {
                        let ok = ok.clone();
                        ok.update(cx, |session, _| session.finish_effects_editing(true));
                    })),
            )
    }
}
