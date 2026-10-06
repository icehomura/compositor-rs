//! The Blend and Opacity row above the Layers list.
//!
//! Ported from `UI/LayerAppearanceControls.swift`. The percentage field's Up/Down stepping and its
//! focus rules are the port's field behaviour: the value is applied when the field is left (Return,
//! Escape or a click elsewhere), and the canvas takes the keyboard back afterwards.

use compositor_core::Id;
use compositor_session::EditorSession;

use crate::panels::blend_mode_picker::BlendModePicker;
use crate::widgets::numeric_scrub::{arrow_stepped, NumericScrub, Scrubbable as _};

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

/// The Blend and Opacity controls.
pub struct LayerAppearanceControls {
    session: Entity<EditorSession>,
    /// The layer this row was built for (`layerID`): an edit that arrives after the selection moved
    /// on is ignored.
    layer_id: Option<Id>,
    blend_mode_picker: Entity<BlendModePicker>,
    /// The opacity slider and its own state.
    slider: Entity<SliderState>,
    /// The percentage field.
    percentage: Entity<InputState>,
    /// Whether the percentage field had the keyboard on the previous frame (`focused`), so leaving
    /// it applies the value.
    was_focused: bool,
}

impl LayerAppearanceControls {
    pub fn new(
        session: Entity<EditorSession>,
        layer_id: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let blend_mode_picker = cx.new(|cx| BlendModePicker::new(session.clone(), cx));
        let opacity = session
            .read(cx)
            .active_layer()
            .map(|layer| layer.opacity)
            .unwrap_or(1.0);
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.01)
                .default_value(opacity as f32)
        });
        // `Slider(value:in:onEditingChanged:)`: the drag is one undo step, opened on the first change
        // and closed when the knob is released.
        cx.subscribe(&slider, |this, _, event: &SliderEvent, cx| {
            let value = match event {
                SliderEvent::Change(SliderValue::Single(value))
                | SliderEvent::Release(SliderValue::Single(value)) => *value,
                _ => return,
            };
            let value = f64::from(value);
            this.session.update(cx, |session, _| match event {
                SliderEvent::Change(_) => {
                    session.begin_opacity_edit();
                    session.set_layer_opacity(value);
                }
                SliderEvent::Release(_) => session.finish_opacity_edit(),
            });
        })
        .detach();
        let percentage = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Opacity percent")
                .default_value(Self::percentage_text(opacity))
        });
        Self {
            session,
            layer_id,
            blend_mode_picker,
            slider,
            percentage,
            was_focused: false,
        }
    }

    /// `String(Int(((activeLayer?.opacity ?? 1) * 100).rounded()))`.
    fn percentage_text(opacity: f64) -> String {
        format!("{}", (opacity * 100.0).round() as i64)
    }

    fn opacity(&self, cx: &App) -> f64 {
        self.session
            .read(cx)
            .active_layer()
            .map(|layer| layer.opacity)
            .unwrap_or(1.0)
    }

    /// `sync()`: the field shows the layer's percentage again.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = Self::percentage_text(self.opacity(cx));
        if self.percentage.read(cx).value().as_ref() != text {
            self.percentage
                .update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    /// `step(_:)`: the label's scrub and the arrow keys.
    fn step(&mut self, percent: f64, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.read(cx).active_layer_id != self.layer_id {
            return;
        }
        let value = percent.clamp(0.0, 100.0) / 100.0;
        self.session
            .update(cx, |session, _| session.set_layer_opacity(value));
        self.sync(window, cx);
    }

    /// `applyPercentage()`: what was typed is applied when the field is left.
    fn apply_percentage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.read(cx).active_layer_id == self.layer_id {
            let text = self.percentage.read(cx).value();
            if let Ok(value) = text.trim().parse::<f64>() {
                if value.is_finite() {
                    self.session
                        .update(cx, |session, _| session.set_layer_opacity(value / 100.0));
                }
            }
        }
        self.sync(window, cx);
    }

    /// `releaseFocus()`: the canvas takes the keyboard back, so a tool's key works straight away.
    fn release_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, _| session.canvas_focus_request += 1);
    }

    /// The percentage field: `.frame(width: 44)`, with Up/Down stepping while it is focused.
    fn percentage_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .w(px(44.0))
            // `arrowSteps(editing:stepper:value:change:)`: Up and Down nudge by one percent, ten
            // with Shift, a tenth with Control or Alt.
            .capture_key_down(move |event: &KeyDownEvent, window, cx| {
                let base = entity.read(cx).opacity(cx) * 100.0;
                match event.keystroke.key.as_str() {
                    "up" | "down" => {
                        let value = arrow_stepped(
                            base,
                            1.0,
                            event.keystroke.key == "up",
                            event.keystroke.modifiers,
                        );
                        entity.update(cx, |controls, cx| controls.step(value, window, cx));
                        cx.stop_propagation();
                    }
                    // `.onSubmit { releaseFocus() }` and `.onExitCommand { releaseFocus() }`: the
                    // field is left, which applies the value on the next frame.
                    "enter" | "escape" => {
                        window.blur(cx);
                        entity.update(cx, |controls, cx| controls.release_focus(window, cx));
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            })
            .child(Input::new(&self.percentage).aria_label("Opacity percent"))
    }
}

impl Render for LayerAppearanceControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (can_edit_appearance, can_edit_opacity) = {
            let session = self.session.read(cx);
            (session.can_edit_appearance(), session.can_edit_opacity())
        };
        // `.onChange(of: focused) { if !isFocused { applyPercentage() } }`.
        let focused = self.percentage.read(cx).focus_handle(cx).is_focused(window);
        if self.was_focused && !focused {
            self.apply_percentage(window, cx);
        }
        self.was_focused = focused;
        // `.onChange(of: session.activeLayer?.opacity) { if !focused { sync() } }` and
        // `.onAppear { sync() }`.
        if !focused {
            self.sync(window, cx);
        }
        let opacity = self.opacity(cx);
        // The slider follows the layer, whichever control moved it.
        if let SliderValue::Single(shown) = self.slider.read(cx).value() {
            if (f64::from(shown) - opacity).abs() > 1e-6 {
                self.slider
                    .update(cx, |state, cx| state.set_value(opacity as f32, window, cx));
            }
        }

        let scrub_entity = cx.entity();
        let scrub_end_entity = cx.entity();
        let label = div().text_size(px(12.0)).child("Opacity").scrubbable(
            "layer-appearance-opacity",
            NumericScrub::new(opacity * 100.0, 1.0, (0.0, 100.0))
                .on_change(move |value, window, cx| {
                    scrub_entity.update(cx, |controls, cx| controls.step(value, window, cx));
                })
                .on_start({
                    let entity = cx.entity();
                    move |_, cx| {
                        entity.update(cx, |controls, cx| {
                            controls
                                .session
                                .update(cx, |session, _| session.begin_opacity_edit());
                        });
                    }
                })
                .on_end(move |_, cx| {
                    scrub_end_entity.update(cx, |controls, cx| {
                        controls
                            .session
                            .update(cx, |session, _| session.finish_opacity_edit());
                    });
                }),
        );

        v_flex()
            .gap(px(8.0))
            .p(px(12.0))
            .when(!can_edit_opacity, |this| this.opacity(0.5))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(!can_edit_appearance, |this| this.opacity(0.5))
                    .child(div().text_size(px(12.0)).child("Blend"))
                    .child(self.blend_mode_picker.clone()),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(label)
                    .child(
                        Slider::new(&self.slider)
                            .horizontal()
                            .flex_1()
                            .disabled(!can_edit_opacity),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(self.percentage_field(cx))
                            .child(div().text_size(px(12.0)).child("%")),
                    ),
            )
    }
}
