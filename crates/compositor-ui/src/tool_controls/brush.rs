//! The Brush family's header (port of `UI/BrushControls.swift`): the Brush and Eraser, Spot
//! Healing, Clone Stamp and Smear tools' options, and the Clone Stamp's own pair.

use compositor_pixels::brush::{BrushSettings, SpotHealingMode};
use compositor_pixels::warp::{BlurToolMode, BrushToolMode};
use compositor_session::EditorSession;

use compositor_core::document::NavigationTool;

use crate::canvas::overlays::palette_rgba;
use crate::tool_controls::clone_stamp::{clone_source_hint, clone_stamp_options};
use crate::tool_controls::{segmented_picker, unit_suffix, FieldSpec, Fields, CONTROL_SPACING};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The Size field's range and the value it falls back to (`range: 1...2000`, `: 40`).
pub const SIZE_RANGE: (f64, f64) = (1.0, 2000.0);
pub const SIZE_FALLBACK: f64 = 40.0;
/// The Blur radius' range (`0.5...50`), and the radius its slider covers (`0.5...20`).
pub const BLUR_RADIUS_RANGE: (f64, f64) = (0.5, 50.0);
pub const BLUR_RADIUS_SLIDER_RANGE: (f64, f64) = (0.5, 20.0);
/// The Smoothing range (`0...100`).
pub const SMOOTHING_RANGE: (f64, f64) = (0.0, 100.0);
/// `.frame(width: 100)` — a slider's width.
pub const SLIDER_WIDTH: f32 = 100.0;
/// `.frame(width: 48)` / `.frame(width: 42)` — the fields'.
pub const SIZE_FIELD_WIDTH: f32 = 48.0;
pub const FIELD_WIDTH: f32 = 42.0;
/// `.frame(width: 180)` — the mask Paint picker's width.
pub const MASK_PAINT_WIDTH: f32 = 180.0;

/// The Brush bar's own state, read in one pass so the render can build elements afterwards.
struct State {
    tool: NavigationTool,
    brush_mode: BrushToolMode,
    blur_mode: BlurToolMode,
    healing_mode: SpotHealingMode,
    settings: BrushSettings,
    is_mask_selected: bool,
    mask_paint_white: bool,
    clone_aligned: bool,
    clone_sample_all_layers: bool,
    clone_source_set: bool,
    can_edit_palette: bool,
    foreground: compositor_core::PaletteColor,
    enabled: bool,
}

/// The bar the Brush, Spot Healing, Clone Stamp and Blur tools share.
pub struct BrushControls {
    session: Entity<EditorSession>,
    fields: Fields,
}

impl BrushControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
        }
    }

    /// The bar's title: the tool's name, the Eraser when the Brush is erasing, or "Smear" for the
    /// Blur tool — `Text(session.tool == .spotHealing ? "Spot Healing" : …)`.
    fn title(state: &State) -> &'static str {
        match state.tool {
            NavigationTool::SpotHealing => "Spot Healing",
            NavigationTool::CloneStamp => "Clone Stamp",
            NavigationTool::Blur => "Smear",
            _ if state.brush_mode == BrushToolMode::Erase => "Eraser",
            _ => "Brush",
        }
    }
}

impl Render for BrushControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = {
            let session = self.session.read(cx);
            State {
                tool: session.tool,
                brush_mode: session.brush_mode,
                blur_mode: session.blur_mode,
                healing_mode: session.spot_healing_mode,
                settings: session.brush_settings.clone(),
                is_mask_selected: session.is_mask_selected,
                mask_paint_white: session.mask_paint_white,
                clone_aligned: session.clone_settings.aligned,
                clone_sample_all_layers: session.clone_settings.sample_all_layers,
                clone_source_set: session.clone_source.is_some(),
                can_edit_palette: session.can_edit_palette(),
                foreground: session.foreground_color(),
                enabled: !session.shows_busy,
            }
        };
        let tool = state.tool;
        let settings = &state.settings;

        let title = tool_header_title(Self::title(&state));

        // `if session.tool == .brush`: Paint or Erase.
        let brush_mode = (tool == NavigationTool::Brush).then(|| {
            let session = self.session.clone();
            div()
                .id("brush-mode")
                .tooltip(|window, cx| {
                    Tooltip::new("Paint with the foreground color (B), or erase pixels away (E)")
                        .build(window, cx)
                })
                .child(
                    segmented_picker(
                        "brush-mode-picker",
                        BrushToolMode::ALL.map(|mode| (mode, mode.raw_value())),
                        state.brush_mode,
                        move |mode, _, cx| {
                            session.update(cx, |session, _| session.brush_mode = mode);
                        },
                    )
                    .disabled(!state.enabled),
                )
        });

        // `if session.tool == .blur`: Liquify, Blur or Smudge.
        let blur_mode = (tool == NavigationTool::Blur).then(|| {
            let session = self.session.clone();
            div()
                .id("blur-mode")
                .tooltip(|window, cx| {
                    Tooltip::new("Liquify pushes pixels · Blur softens · Smudge drags color along")
                        .build(window, cx)
                })
                .child(
                    segmented_picker(
                        "blur-mode-picker",
                        BlurToolMode::ALL.map(|mode| (mode, mode.raw_value())),
                        state.blur_mode,
                        move |mode, _, cx| {
                            session.update(cx, |session, _| session.blur_mode = mode);
                        },
                    )
                    .disabled(!state.enabled),
                )
        });

        // `if session.tool == .spotHealing`: Content-Aware, Create Texture or Proximity Match.
        let healing_mode = (tool == NavigationTool::SpotHealing).then(|| {
            let session = self.session.clone();
            div()
                .id("spot-healing-type")
                .aria_label("spotHealingType")
                .child(
                    segmented_picker(
                        "spot-healing-type-picker",
                        SpotHealingMode::ALL.map(|mode| (mode, mode.raw_value())),
                        state.healing_mode,
                        move |mode, _, cx| {
                            session.update(cx, |session, _| session.spot_healing_mode = mode);
                        },
                    )
                    .disabled(!state.enabled),
                )
        });

        // `if session.tool == .cloneStamp`: Aligned and Sample.
        let clone_options = (tool == NavigationTool::CloneStamp)
            .then(|| clone_stamp_options(self.session.clone(), state.clone_aligned, state.clone_sample_all_layers));

        let size_field = self.fields.get("brush-size", cx);
        let size_scrub = {
            let session = self.session.clone();
            NumericScrub::new(settings.diameter, 1.0, SIZE_RANGE).on_change(move |value, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.diameter = value);
            })
        };
        let size_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.diameter = value);
            }
        };

        let hardness_field = self.fields.get("brush-hardness", cx);
        let hardness_scrub = {
            let session = self.session.clone();
            NumericScrub::new(settings.hardness, 0.01, (0.0, 1.0)).on_change(move |value, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.hardness = value);
            })
        };
        let hardness_slider = {
            let session = self.session.clone();
            let slider =
                CameraRawSlider::plain("brush-hardness-slider", settings.hardness, (0.0, 1.0), "Hardness")
                    .step(0.01)
                    .on_change(move |value, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.brush_settings.hardness = value);
                    });
            div()
                .w(px(SLIDER_WIDTH))
                .when(!state.enabled, |this| this.opacity(0.5))
                .child(slider)
        };
        let hardness_write = {
            let session = self.session.clone();
            move |percent: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.hardness = percent / 100.0);
            }
        };

        let opacity_field = self.fields.get("brush-opacity", cx);
        let opacity_scrub = {
            let session = self.session.clone();
            NumericScrub::new(settings.opacity, 0.01, (0.01, 1.0)).on_change(move |value, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.opacity = value);
            })
        };
        let opacity_slider = {
            let session = self.session.clone();
            let slider =
                CameraRawSlider::plain("brush-opacity-slider", settings.opacity, (0.01, 1.0), "Opacity")
                    .step(0.01)
                    .on_change(move |value, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.brush_settings.opacity = value);
                    });
            div()
                .w(px(SLIDER_WIDTH))
                .when(!state.enabled, |this| this.opacity(0.5))
                .child(slider)
        };
        let opacity_write = {
            let session = self.session.clone();
            move |percent: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| session.brush_settings.opacity = percent / 100.0);
            }
        };

        // Blur softens by a radius of its own, apart from how strongly it lays the softening down.
        let radius_row = (tool == NavigationTool::Blur && state.blur_mode == BlurToolMode::Blur).then(|| {
            let field = self.fields.get("brush-blur-radius", cx);
            let scrub = {
                let session = self.session.clone();
                NumericScrub::new(settings.blur_radius, 0.1, BLUR_RADIUS_RANGE).on_change(
                    move |value, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.brush_settings.blur_radius = value);
                    },
                )
            };
            let slider = {
                let session = self.session.clone();
                let slider = CameraRawSlider::plain(
                    "brush-blur-radius-slider",
                    // The slider covers everyday radii; typing or scrubbing reaches up to 50.
                    settings.blur_radius.min(BLUR_RADIUS_SLIDER_RANGE.1),
                    BLUR_RADIUS_SLIDER_RANGE,
                    "Blur radius",
                )
                .step(0.1)
                .on_change(move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.brush_settings.blur_radius = value);
                });
                div()
                    .w(px(SLIDER_WIDTH))
                    .when(!state.enabled, |this| this.opacity(0.5))
                    .child(slider)
            };
            let write = {
                let session = self.session.clone();
                move |value: f64, _: &mut Window, cx: &mut App| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.brush_settings.blur_radius = value);
                }
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().child("Radius").scrubbable("brush-blur-radius-label", scrub))
                .child(slider)
                .child(
                    unit_suffix(
                        field.element(
                            "brush-blur-radius-field",
                            settings.blur_radius,
                            FieldSpec::new(BLUR_RADIUS_RANGE, 1)
                                .fallback(5.0)
                                .disabled(!state.enabled),
                            FIELD_WIDTH,
                            write,
                            cx,
                        ),
                        div().child("px"),
                    )
                    .id("brush-blur-radius-row")
                    .tooltip(|window, cx| {
                        Tooltip::new("How far the blur softens, in pixels").build(window, cx)
                    }),
                )
        });

        // Paint and Erase only: healing, cloning and smearing have their own feel.
        let smoothing_row = (tool == NavigationTool::Brush).then(|| {
            let field = self.fields.get("brush-smoothing", cx);
            let scrub = {
                let session = self.session.clone();
                NumericScrub::new(settings.smoothing, 1.0, SMOOTHING_RANGE).on_change(
                    move |value, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.brush_settings.smoothing = value);
                    },
                )
            };
            let slider = {
                let session = self.session.clone();
                let slider =
                    CameraRawSlider::plain("brush-smoothing-slider", settings.smoothing, SMOOTHING_RANGE, "Smoothing")
                        .on_change(move |value, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| session.brush_settings.smoothing = value);
                        });
                div()
                    .w(px(SLIDER_WIDTH))
                    .when(!state.enabled, |this| this.opacity(0.5))
                    .child(slider)
            };
            let write = {
                let session = self.session.clone();
                move |value: f64, _: &mut Window, cx: &mut App| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.brush_settings.smoothing = value);
                }
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().child("Smoothing").scrubbable("brush-smoothing-label", scrub))
                .child(slider)
                .child(
                    field
                        .element(
                            "brush-smoothing-field",
                            settings.smoothing,
                            FieldSpec::new(SMOOTHING_RANGE, 0)
                                .fallback(0.0)
                                .disabled(!state.enabled),
                            FIELD_WIDTH,
                            write,
                            cx,
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new("The brush trails the pointer on a string this long, so a shaky hand still draws a smooth line")
                                .build(window, cx)
                        }),
                )
        });

        // The mask's Paint choice, or the foreground swatch.
        let color_control: Option<AnyElement> = if state.is_mask_selected {
            let session = self.session.clone();
            Some(
                div()
                    .w(px(MASK_PAINT_WIDTH))
                    .child(
                        segmented_picker(
                            "mask-paint-picker",
                            [(false, "Black · Hide"), (true, "White · Reveal")],
                            state.mask_paint_white,
                            move |value, _, cx| {
                                let session = session.clone();
                                session.update(cx, |session, _| session.set_mask_paint_white(value));
                            },
                        )
                        .disabled(!state.enabled),
                    )
                    .into_any_element(),
            )
        } else if tool != NavigationTool::CloneStamp && tool != NavigationTool::Blur {
            // Same foreground color and Color Picker as the tool-rail swatch.
            let session = self.session.clone();
            Some(
                div()
                    .id("brush-color")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child("Color")
                    .child(
                        Button::new("brush-foreground")
                            .tooltip("Foreground color")
                            .accessibility_label("Foreground color")
                            .disabled(!state.can_edit_palette || !state.enabled)
                            .child(
                                div()
                                    .w(px(34.0))
                                    .h(px(18.0))
                                    .rounded(px(4.0))
                                    .border_2()
                                    .border_color(hsla(0.0, 0.0, 1.0, 1.0))
                                    .bg(Hsla::from(palette_rgba(state.foreground)))
                                    .child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .rounded(px(4.0))
                                            .border_1()
                                            .border_color(hsla(0.0, 0.0, 0.0, 1.0)),
                                    ),
                            )
                            .on_click(move |_, _, cx| {
                                let session = session.clone();
                                session.update(cx, |session, _| session.open_color_picker(false));
                            }),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        let hint = (tool == NavigationTool::CloneStamp && !state.clone_source_set)
            .then(clone_source_hint);
        let mask_note = state.is_mask_selected.then(|| {
            div()
                .text_color(cx.theme().tokens.muted_foreground)
                .child("Mask")
        });

        tool_header_bar(CONTROL_SPACING)
            .child(title)
            .children(brush_mode)
            .children(blur_mode)
            .children(healing_mode)
            .children(clone_options)
            .child(div().child("Size").scrubbable("brush-size-label", size_scrub))
            .child(unit_suffix(
                size_field.element(
                    "brush-size-field",
                    settings.diameter,
                    FieldSpec::new(SIZE_RANGE, 0)
                        .fallback(SIZE_FALLBACK)
                        .disabled(!state.enabled),
                    SIZE_FIELD_WIDTH,
                    size_write,
                    cx,
                ),
                div().child("px"),
            ))
            .child(div().child("Hardness").scrubbable("brush-hardness-label", hardness_scrub))
            .child(hardness_slider)
            .child(unit_suffix(
                hardness_field.element(
                    "brush-hardness-field",
                    settings.hardness * 100.0,
                    FieldSpec::new((0.0, 100.0), 0)
                        .fallback(100.0)
                        .disabled(!state.enabled),
                    FIELD_WIDTH,
                    hardness_write,
                    cx,
                ),
                div().child("%"),
            ))
            .child(
                div()
                    .child(if tool == NavigationTool::Blur { "Strength" } else { "Opacity" })
                    .scrubbable("brush-opacity-label", opacity_scrub),
            )
            .child(opacity_slider)
            .child(
                unit_suffix(
                    opacity_field.element(
                        "brush-opacity-field",
                        settings.opacity * 100.0,
                        FieldSpec::new((1.0, 100.0), 0)
                            .fallback(100.0)
                            .disabled(!state.enabled),
                        FIELD_WIDTH,
                        opacity_write,
                        cx,
                    ),
                    div().child("%"),
                )
                .id("brush-opacity-row")
                .tooltip(|window, cx| {
                    Tooltip::new("Press 1–9 for 10–90%, 0 for 100%").build(window, cx)
                }),
            )
            .children(radius_row)
            .children(smoothing_row)
            .children(color_control)
            .child(tool_header_spacer())
            .children(hint)
            .children(mask_note)
    }
}
