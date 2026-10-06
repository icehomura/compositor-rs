//! Camera Raw Filter's adjustment column (port of `UI/CameraRawControls.swift`,
//! `UI/CameraRawColorControls.swift`, `UI/CameraRawDetailOpticsControls.swift` and
//! `UI/CameraRawGeometryCalibrationControls.swift`).
//!
//! The column is the histogram — three ribbons over the shared peak or the vectorscope's cells,
//! its clipping indicators, context menu, mode help and the pointer readout — then the ten
//! disclosure sections (Light, Color, Color Grading, Effects, Curve, Color Mixer, Detail, Optics,
//! Geometry, Calibration), each with an eye that drops its amounts from the preview.
//!
//! The Swift font styles use the panel vocabulary's sizes: `.headline` is 13 points semibold,
//! `.subheadline` and `.caption` are 12 points, `.caption2` is 11. `NSEvent`'s Option key is the
//! window's Alt (`Window::modifiers().alt`), and `installOptionMonitor`'s listener is registered
//! while the histogram paints (`Window::on_modifiers_changed`), which gives it the same
//! while-the-view-is-on-screen life the Swift monitor had. `CameraRawSlider` is the finished port
//! in [`crate::widgets::gradient_slider`], and a `WritableKeyPath<…, Double>` becomes a closure
//! that writes one field, with the same ranges, rounding and reset defaults at the same call
//! sites.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use compositor_pixels::camera_raw::{
    CameraRawCalibrationSettings, CameraRawClipping, CameraRawCurvePage, CameraRawCurveRegion,
    CameraRawCurveSettings, CameraRawDetailSettings, CameraRawGeometrySettings, CameraRawGlowStyle,
    CameraRawGradePage, CameraRawGradeWheel, CameraRawGradingSettings, CameraRawMixerPage,
    CameraRawMixerSettings, CameraRawMixerTab, CameraRawOpticsSettings, CameraRawPointChannel,
    CameraRawProcessVersion, CameraRawProjection, CameraRawScope, CameraRawScopeMode,
    CameraRawSettings, CameraRawUprightMode, CameraRawVignetteStyle, CameraRawWhiteBalance, CurvePoint,
};
use compositor_session::camera_raw::{CameraRawPanel, CameraRawSection};
use compositor_session::EditorSession;

use crate::tool_controls::{menu_picker, segmented_picker, FieldSpec, Fields};
use crate::widgets::gradient_slider::{hsb, CameraRawSlider, CameraRawSliderTrack};
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};
use crate::widgets::slider_snap::rounded_to_scale;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `CameraRawControls.labelWidth`.
pub const LABEL_WIDTH: f32 = 96.0;
/// The column's `VStack(spacing: 10)`.
const COLUMN_SPACING: f32 = 10.0;
/// The sections' `VStack(spacing: 12)`.
const SECTION_SPACING: f32 = 12.0;
/// A disclosure's `VStack(spacing: 8)`.
const DISCLOSURE_SPACING: f32 = 8.0;
/// The open body's `.padding(.leading, 18)`.
const SECTION_INDENT: f32 = 18.0;
/// The Effects rows' `.padding(.leading, 16)`.
const ROW_INDENT: f32 = 16.0;
/// A slider row's `HStack(spacing: 10)`.
const CONTROL_SPACING: f32 = 10.0;
/// `TextField(…).frame(width: 56)`.
const FIELD_WIDTH: f32 = 56.0;
/// `.headline`.
const HEADLINE_SIZE: f32 = 13.0;
/// `.subheadline`.
const SUBHEADLINE_SIZE: f32 = 12.0;
/// `.caption`.
const CAPTION_SIZE: f32 = 12.0;
/// `.caption2`.
const CAPTION2_SIZE: f32 = 11.0;
/// `graph(…).frame(height: 110)`.
const HISTOGRAM_HEIGHT: f32 = 110.0;
/// `RoundedRectangle(cornerRadius: 4, style: .continuous)`.
const HISTOGRAM_RADIUS: f32 = 4.0;
/// The clipping buttons' `.padding(4)`.
const HISTOGRAM_INSET: f32 = 4.0;
/// `Color.black.opacity(0.35)`.
const HISTOGRAM_BACKGROUND: f32 = 0.35;
/// The ribbons' `color.opacity(0.55)`.
const RIBBON_OPACITY: f32 = 0.55;
/// The vectorscope's `0.15 + 0.85 * amount`.
const VECTORSCOPE_MIN: f32 = 0.15;
const VECTORSCOPE_RANGE: f32 = 0.85;
/// `width: cell + 0.2`.
const VECTORSCOPE_BLEED: f32 = 0.2;
/// `curveGraph.frame(height: 150)`.
const CURVE_GRAPH_HEIGHT: f32 = 150.0;
/// `GradeWheel(…).frame(width: 86, height: 86)`.
const WHEEL_SIZE: f32 = 86.0;
/// `side / 2 - 6`.
const WHEEL_INSET: f32 = 6.0;
/// How many sectors the hue ring is painted from (`AngularGradient` spread out by hand).
const WHEEL_SEGMENTS: usize = 120;
/// The eyedropper's chrome, as the Color Range sheet's `Image(systemName:)` button.
const EYEDROPPER_WIDTH: f32 = 24.0;
const EYEDROPPER_HEIGHT: f32 = 20.0;
const EYEDROPPER_RADIUS: f32 = 4.0;
const ACCENT_OPACITY: f32 = 0.25;
/// The Color Mixer's swatches: `.frame(width: 18, height: 18)`.
const MIXER_SWATCH: f32 = 18.0;
/// The Point Color swatches: `.frame(width: 16, height: 16)`.
const POINT_SWATCH: f32 = 16.0;
/// The `.opacity(0.45)` a disabled group of Detail rows shows.
const DISABLED_OPACITY: f32 = 0.45;

/// Camera Raw Filter's adjustment column.
pub struct CameraRawControls {
    session: Entity<EditorSession>,
    /// `@State private var expanded: Set<Section> = [.light, .color, .colorGrading]`.
    expanded: HashSet<CameraRawSection>,
    /// `CameraRawCurveControls.drag`: what the graph drag is moving.
    curve_drag: Option<CurveGesture>,
    /// `CameraRawCurveControls.selected`.
    curve_selected: Option<usize>,
    /// The number fields the sliders' rows share.
    fields: Fields,
}

impl CameraRawControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            expanded: HashSet::from([
                CameraRawSection::Light,
                CameraRawSection::Color,
                CameraRawSection::ColorGrading,
            ]),
            curve_drag: None,
            curve_selected: None,
            fields: Fields::default(),
        }
    }

    fn settings(&self, cx: &App) -> CameraRawSettings {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.camera_raw.clone())
            .unwrap_or_default()
    }

    fn panel(&self, cx: &App) -> CameraRawPanel {
        self.session
            .read(cx)
            .filter_edit
            .as_ref()
            .map(|edit| edit.panel.clone())
            .unwrap_or_default()
    }

    fn column(&mut self, cx: &mut Context<Self>) -> Div {
        let raw = self.settings(cx);
        let panel = self.panel(cx);
        let histogram = self.histogram(&panel, cx);
        let mut sections = Vec::with_capacity(CameraRawSection::ALL.len());
        for section in CameraRawSection::ALL {
            sections.push(self.disclosure(section, &raw, &panel, cx));
        }

        let session = self.session.clone();
        v_flex()
            .items_start()
            .gap(px(COLUMN_SPACING))
            .child(histogram)
            .child(
                div()
                    .id("camera-raw-sections")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .items_start()
                            .gap(px(SECTION_SPACING))
                            .children(sections),
                    ),
            )
            .on_modifiers_changed(move |event, _, cx| option_released(&session, event, cx))
    }

    fn histogram(&self, panel: &CameraRawPanel, cx: &mut Context<Self>) -> Div {
        let scope = panel.scope.clone();
        let mode = panel.scope_mode;
        let readout = panel.readout;
        let shadow_shows = panel.shows_shadow_clipping;
        let highlight_shows = panel.shows_highlight_clipping;
        let session = self.session.clone();
        let graph = div()
            .id("camera-raw-histogram")
            .relative()
            .w_full()
            .h(px(HISTOGRAM_HEIGHT))
            .rounded(px(HISTOGRAM_RADIUS))
            .bg(hsla(0.0, 0.0, 0.0, HISTOGRAM_BACKGROUND))
            .overflow_hidden()
            .tooltip(move |window, cx| {
                let help = if mode == CameraRawScopeMode::Histogram {
                    "Tones from black on the left to white on the right: blacks, shadows, midtones, highlights, whites. Control-click to show the vectorscope."
                } else {
                    "Hue around the wheel, saturation outward from the center. Control-click to show the histogram."
                };
                Tooltip::new(help).build(window, cx)
            })
            .child(canvas(
                |_, _, _| (),
                move |bounds, (), window, _| paint_scope(window, bounds, scope.as_ref(), mode),
            ))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .p(px(HISTOGRAM_INSET))
                    .flex()
                    .items_center()
                    .child(Self::clip_button(
                        "camera-raw-clip-shadow",
                        true,
                        shadow_shows,
                        &session,
                    ))
                    .child(div().flex_1())
                    .child(Self::clip_button(
                        "camera-raw-clip-highlight",
                        false,
                        highlight_shows,
                        &session,
                    )),
            );
        let menu_session = self.session.clone();
        let graph = graph.context_menu(move |menu, _, cx| {
            let histogram_session = menu_session.clone();
            let vectorscope_session = menu_session.clone();
            menu.item(
                PopupMenuItem::new("Histogram").on_click(move |_, _, cx| {
                    edit_panel(&histogram_session, cx, |panel| {
                        panel.scope_mode = CameraRawScopeMode::Histogram
                    });
                }),
            )
            .item(PopupMenuItem::new("Vectorscope").on_click(move |_, _, cx| {
                edit_panel(&vectorscope_session, cx, |panel| {
                    panel.scope_mode = CameraRawScopeMode::Vectorscope
                });
            }))
        });

        let text = match readout {
            Some(value) => format!("R {}   G {}   B {}", value.red, value.green, value.blue),
            None => "R —   G —   B —".to_string(),
        };
        v_flex()
            .items_start()
            .gap(px(4.0))
            .child(graph)
            .child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().tokens.muted_foreground)
                    .child(text),
            )
    }

    fn clip_button(
        id: &'static str,
        shadows: bool,
        shown: bool,
        session: &Entity<EditorSession>,
    ) -> Stateful<Div> {
        let color = if shown {
            if shadows {
                hsla(0.6, 1.0, 0.6, 1.0)
            } else {
                hsla(0.0, 1.0, 0.55, 1.0)
            }
        } else {
            hsla(0.0, 0.0, 1.0, 0.55)
        };
        let session = session.clone();
        div()
            .id(id)
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .aria_label(if shadows {
                "Shadow Clipping Indicator"
            } else {
                "Highlight Clipping Indicator"
            })
            .tooltip(move |window, cx| {
                let help = if shadows {
                    "Show clipped shadows in blue on the preview."
                } else {
                    "Show clipped highlights in red on the preview."
                };
                Tooltip::new(help).build(window, cx)
            })
            .on_click(move |_, _, cx| {
                let session = session.clone();
                session.update(cx, |session, cx| {
                    let Some(edit) = session.filter_edit.as_mut() else {
                        return;
                    };
                    if shadows {
                        edit.panel.shows_shadow_clipping = !edit.panel.shows_shadow_clipping;
                    } else {
                        edit.panel.shows_highlight_clipping = !edit.panel.shows_highlight_clipping;
                    }
                    let settings = edit.settings.clone();
                    let preview = edit.preview;
                    session.update_filter(settings, preview);
                    cx.notify();
                });
            })
            .child(
                div()
                    .text_size(px(CAPTION2_SIZE))
                    .text_color(color)
                    .child("▲"),
            )
    }

    fn disclosure(
        &mut self,
        section: CameraRawSection,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let expanded = self.expanded.contains(&section);
        let entity = cx.entity();
        let title_id = SharedString::from(format!("camera-raw-section-{}", section.raw_value()));
        let title = div()
            .id(title_id)
            .cursor(CursorStyle::PointingHand)
            .role(Role::Button)
            .aria_label(section.raw_value())
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    if !this.expanded.remove(&section) {
                        this.expanded.insert(section);
                    }
                    cx.notify();
                });
            })
            .child(
                h_flex()
                    .gap(px(6.0))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(10.0)),
                    )
                    .child(
                        div()
                            .text_size(px(HEADLINE_SIZE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(section.raw_value()),
                    ),
            );

        let mut header = h_flex()
            .items_center()
            .gap(px(6.0))
            .child(title)
            .child(div().flex_1());
        if Self::section_adjusts(raw, section) {
            header = header.child(self.eye(section, panel, cx));
        }

        let mut body = v_flex().items_start().gap(px(8.0));
        if expanded {
            body = body.pl(px(SECTION_INDENT)).child(self.section_body(section, raw, panel, cx));
        }
        v_flex().items_start().gap(px(DISCLOSURE_SPACING)).child(header).child(body)
    }

    fn section_adjusts(raw: &CameraRawSettings, section: CameraRawSection) -> bool {
        match section {
            CameraRawSection::Light => raw.adjusts_light(),
            CameraRawSection::Color => raw.adjusts_color(),
            CameraRawSection::ColorGrading => raw.adjusts_grading(),
            CameraRawSection::Effects => raw.adjusts_effects(),
            CameraRawSection::Curve => raw.adjusts_curve(),
            CameraRawSection::ColorMixer => raw.adjusts_mixer(),
            CameraRawSection::Detail => raw.adjusts_detail(),
            CameraRawSection::Optics => raw.adjusts_optics(),
            CameraRawSection::Geometry => raw.adjusts_geometry(),
            CameraRawSection::Calibration => raw.adjusts_calibration(),
        }
    }

    fn eye(
        &self,
        section: CameraRawSection,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Button {
        let shown = panel.shows(section);
        let name = section.raw_value();
        let session = self.session.clone();
        Button::new(SharedString::from(format!("camera-raw-eye-{}", name)))
            .icon(Icon::new(if shown { IconName::Eye } else { IconName::EyeOff }).size(px(12.0)))
            .ghost()
            .tooltip(if shown {
                format!("Hide {name} in the preview")
            } else {
                format!("Show {name} in the preview")
            })
            .accessibility_label(if shown {
                format!("Hide {name}")
            } else {
                format!("Show {name}")
            })
            .on_click(move |_, _, cx| {
                let session = session.clone();
                session.update(cx, |session, cx| {
                    let Some(edit) = session.filter_edit.as_mut() else {
                        return;
                    };
                    edit.panel.toggle_shows(section);
                    let settings = edit.settings.clone();
                    let preview = edit.preview;
                    session.update_filter(settings, preview);
                    cx.notify();
                });
            })
    }

    fn section_body(
        &mut self,
        section: CameraRawSection,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        match section {
            CameraRawSection::Light => self.light_controls(raw, cx),
            CameraRawSection::Color => self.color_controls(raw, panel, cx),
            CameraRawSection::ColorGrading => self.grading_controls(raw, panel, cx),
            CameraRawSection::Effects => self.effects_controls(raw, cx),
            CameraRawSection::Curve => self.curve_controls(raw, panel, cx),
            CameraRawSection::ColorMixer => self.mixer_controls(raw, panel, cx),
            CameraRawSection::Detail => self.detail_controls(raw, cx),
            CameraRawSection::Optics => self.optics_controls(raw, panel, cx),
            CameraRawSection::Geometry => self.geometry_controls(raw, panel, cx),
            CameraRawSection::Calibration => self.calibration_controls(raw, cx),
        }
    }

    fn amount_row(&mut self, row: AmountRow, cx: &mut Context<Self>) -> Div {
        let AmountRow {
            id,
            title,
            label_width,
            field_width,
            value,
            range,
            decimals,
            reset_value,
            help,
            track,
            scrub,
            label_reset,
            rounding,
            kind,
            clipping,
            masking,
            enabled,
            set,
        } = row;
        let scale = 10f64.powi(decimals as i32);
        let step = match rounding {
            Rounding::Step => 1.0 / scale,
            Rounding::Whole => 1.0,
            Rounding::Exact => 1.0 / scale,
        };
        let write_value = {
            let session = self.session.clone();
            let set = set.clone();
            move |amount: f64, window: &mut Window, cx: &mut App| {
                let amount = match rounding {
                    Rounding::Step => rounded_to_scale(amount, scale),
                    Rounding::Whole => amount.round(),
                    Rounding::Exact => amount,
                };
                write_amount(&session, kind, clipping, masking, amount, {
                    let set = set.clone();
                    move |value, raw| set(value, raw)
                }, window, cx);
            }
        };
        let reset_value_session = self.session.clone();
        let reset_set = set.clone();
        let reset = move |window: &mut Window, cx: &mut App| {
            write_amount(
                &reset_value_session,
                kind,
                None,
                false,
                reset_value,
                {
                    let set = reset_set.clone();
                    move |value, raw| set(value, raw)
                },
                window,
                cx,
            );
        };
        let field_session = self.session.clone();
        let field_set = set.clone();
        let field_write = move |amount: f64, window: &mut Window, cx: &mut App| {
            write_amount(&field_session, kind, clipping, masking, amount, {
                let set = field_set.clone();
                move |value, raw| set(value, raw)
            }, window, cx);
        };

        let mut label = div()
            .id(SharedString::from(format!("camera-raw-label-{id}")))
            .flex_none()
            .w(px(label_width))
            .text_size(px(crate::tool_header::CONTROL_SIZE))
            .child(title)
            .tooltip({
                let help = help.clone();
                move |window, cx| Tooltip::new(help.clone()).build(window, cx)
            });
        if label_reset {
            label = label.on_click({
                let reset = reset.clone();
                move |event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        reset(window, cx);
                    }
                }
            });
        }
        let label = if scrub && enabled {
            let scrub_session = self.session.clone();
            let scrub_set = set.clone();
            let sensitivity = step;
            label
                .scrubbable(
                    SharedString::from(format!("camera-raw-scrub-{id}")),
                    NumericScrub::new(value, sensitivity, range).on_change(
                        move |amount, window, cx| {
                            write_amount(&scrub_session, kind, clipping, masking, amount, {
                                let set = scrub_set.clone();
                                move |value, raw| set(value, raw)
                            }, window, cx);
                        },
                    ),
                )
                .into_any_element()
        } else {
            label.into_any_element()
        };

        let slider = CameraRawSlider::new(id, value, range, track, help.clone())
            .on_change({
                let write_value = write_value.clone();
                move |amount, window, cx| write_value(amount, window, cx)
            })
            .on_reset(reset.clone());
        let slider = div()
            .flex_1()
            .min_w(px(0.0))
            .when(!enabled, |this| this.opacity(0.5))
            .child(slider);

        let field = self.fields.get(id, cx);
        let field_width = field_width.unwrap_or(FIELD_WIDTH);
        let field = field.element(
            id,
            value,
            FieldSpec::new(range, decimals)
                .fallback(value)
                .disabled(!enabled),
            field_width,
            field_write,
            cx,
        );

        h_flex()
            .items_center()
            .gap(px(CONTROL_SPACING))
            .child(label)
            .child(slider)
            .child(field)
            .when(!enabled, |this| this.opacity(0.5))
    }

    fn light_controls(&mut self, raw: &CameraRawSettings, cx: &mut Context<Self>) -> Div {
        let rows = [
            AmountRow {
                id: "camera-raw-exposure",
                title: "Exposure",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.exposure,
                range: CameraRawSettings::EXPOSURE_RANGE,
                decimals: 2,
                reset_value: 0.0,
                help: "Brightens or darkens the whole picture, in stops of light. Hold Option to see clipped highlights.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Step,
                kind: AssignKind::Column,
                clipping: Some(CameraRawClipping::Highlights),
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.exposure = value),
            },
            AmountRow {
                id: "camera-raw-contrast",
                title: "Contrast",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.contrast,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Makes light and dark tones more or less different, mostly around the middle.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.contrast = value),
            },
            AmountRow {
                id: "camera-raw-highlights",
                title: "Highlights",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.highlights,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adjusts the bright parts of the picture. Hold Option to see clipped highlights.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: Some(CameraRawClipping::Highlights),
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.highlights = value),
            },
            AmountRow {
                id: "camera-raw-shadows",
                title: "Shadows",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.shadows,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adjusts the dark parts of the picture. Hold Option to see clipped shadows.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: Some(CameraRawClipping::Shadows),
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.shadows = value),
            },
            AmountRow {
                id: "camera-raw-whites",
                title: "Whites",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.whites,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Sets the brightest point. Hold Option to see clipped highlights.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: Some(CameraRawClipping::Highlights),
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.whites = value),
            },
            AmountRow {
                id: "camera-raw-blacks",
                title: "Blacks",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.blacks,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Sets the darkest point. Hold Option to see clipped shadows.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: Some(CameraRawClipping::Shadows),
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.blacks = value),
            },
        ];
        let mut column = v_flex().items_start().gap(px(8.0));
        for row in rows {
            column = column.child(self.amount_row(row, cx));
        }
        column
    }

    fn color_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let white_balance = raw.white_balance;
        let session = self.session.clone();
        let picker = menu_picker(
            "camera-raw-white-balance",
            CameraRawWhiteBalance::ALL.map(|mode| (mode, mode.raw_value())),
            white_balance,
            move |mode, _, cx| {
                let session = session.clone();
                session.update(cx, |session, cx| {
                    if mode == CameraRawWhiteBalance::Auto {
                        session.apply_camera_raw_auto_white_balance();
                    } else if let Some(edit) = session.filter_edit.as_mut() {
                        edit.camera_raw.white_balance = mode;
                        let settings = edit.settings.clone();
                        let preview = edit.preview;
                        session.update_filter(settings, preview);
                    }
                    cx.notify();
                });
            },
        );
        let wb_session = self.session.clone();
        let eyedropper = Button::new("camera-raw-white-balance-eyedropper")
            .icon(Icon::new(IconName::Pipette).size(px(14.0)))
            .selected(panel.samples_white_balance)
            .tooltip("Click a pixel that should be neutral.")
            .accessibility_label("White Balance Selector")
            .on_click(move |_, _, cx| {
                arm_eyedropper(&wb_session, cx, |panel| {
                    panel.samples_white_balance = !panel.samples_white_balance;
                });
            });

        let mut column = v_flex().items_start().gap(px(8.0));
        column = column.child(
            h_flex()
                .items_center()
                .gap(px(CONTROL_SPACING))
                .child(
                    div()
                        .id("camera-raw-white-balance-label")
                        .flex_none()
                        .w(px(LABEL_WIDTH))
                        .text_size(px(crate::tool_header::CONTROL_SIZE))
                        .child("White Balance")
                        .tooltip(|window, cx| {
                            Tooltip::new("Auto balances the average color. Custom follows Temperature and Tint.")
                                .build(window, cx)
                        }),
                )
                .child(picker)
                .child(eyedropper),
        );
        if panel.samples_white_balance {
            column = column.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(cx.theme().tokens.muted_foreground)
                    .child("Click the original layer. Click the eyedropper again to stop."),
            );
        }
        for row in [
            AmountRow {
                id: "camera-raw-temperature",
                title: "Temperature",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.temperature,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Shifts the picture from blue to yellow.".into(),
                track: CameraRawSliderTrack::Temperature,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| {
                    raw.temperature = value;
                    raw.white_balance = CameraRawWhiteBalance::Custom;
                }),
            },
            AmountRow {
                id: "camera-raw-tint",
                title: "Tint",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.tint,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Shifts the picture from green to mauve.".into(),
                track: CameraRawSliderTrack::Tint,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| {
                    raw.tint = value;
                    raw.white_balance = CameraRawWhiteBalance::Custom;
                }),
            },
            AmountRow {
                id: "camera-raw-vibrance",
                title: "Vibrance",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vibrance,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Strengthens quiet colors more than colors that are already strong, and protects skin tones.".into(),
                track: CameraRawSliderTrack::Chroma,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vibrance = value),
            },
            AmountRow {
                id: "camera-raw-saturation",
                title: "Saturation",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.saturation,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Strengthens or weakens every color by the same amount.".into(),
                track: CameraRawSliderTrack::Chroma,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.saturation = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }
        column
    }

    fn effects_controls(&mut self, raw: &CameraRawSettings, cx: &mut Context<Self>) -> Div {
        let mut column = v_flex().items_start().gap(px(8.0));
        for row in [
            AmountRow {
                id: "camera-raw-texture",
                title: "Texture",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.texture,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adds or softens small detail.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.texture = value),
            },
            AmountRow {
                id: "camera-raw-clarity",
                title: "Clarity",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.clarity,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adds or softens contrast along broader shapes.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.clarity = value),
            },
            AmountRow {
                id: "camera-raw-dehaze",
                title: "Dehaze",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.dehaze,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Clears haze when raised, and adds haze when lowered.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.dehaze = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Glow"),
        );
        let glow_style = raw.glow_style;
        let glow_session = self.session.clone();
        column = column.child(menu_picker(
            "camera-raw-glow-style",
            CameraRawGlowStyle::ALL.map(|style| (style, style.raw_value())),
            glow_style,
            move |style, _, cx| {
                update_camera_raw(&glow_session, cx, |raw| raw.glow_style = style);
            },
        ));
        for row in [
            AmountRow {
                id: "camera-raw-glow",
                title: "Glow",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.glow,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Spreads a glow from the bright areas.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.glow = value),
            },
            AmountRow {
                id: "camera-raw-glow-range",
                title: "Range",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.glow_range,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Chooses how bright an area must be to glow. Has no effect until Glow is raised.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.glow_range = value),
            },
            AmountRow {
                id: "camera-raw-glow-spread",
                title: "Spread",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.glow_spread,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Sets how far the glow reaches. Has no effect until Glow is raised.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.glow_spread = value),
            },
            AmountRow {
                id: "camera-raw-glow-warmth",
                title: "Warmth",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.glow_warmth,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Shifts the glow from cool to warm. Halation stays red. Has no effect until Glow is raised.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.glow_warmth = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Vignette"),
        );
        let vignette_style = raw.vignette_style;
        let vignette_session = self.session.clone();
        column = column.child(menu_picker(
            "camera-raw-vignette-style",
            CameraRawVignetteStyle::ALL.map(|style| (style, style.raw_value())),
            vignette_style,
            move |style, _, cx| {
                update_camera_raw(&vignette_session, cx, |raw| raw.vignette_style = style);
            },
        ));
        for row in [
            AmountRow {
                id: "camera-raw-vignette-amount",
                title: "Amount",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vignette_amount,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Darkens or lightens the edges. The center does not change.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vignette_amount = value),
            },
            AmountRow {
                id: "camera-raw-vignette-midpoint",
                title: "Midpoint",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vignette_midpoint,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Sets where the vignette begins, from the center outward.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vignette_midpoint = value),
            },
            AmountRow {
                id: "camera-raw-vignette-roundness",
                title: "Roundness",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vignette_roundness,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Makes the vignette rounder or more square.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vignette_roundness = value),
            },
            AmountRow {
                id: "camera-raw-vignette-feather",
                title: "Feather",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vignette_feather,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Softens the edge of the vignette.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vignette_feather = value),
            },
            AmountRow {
                id: "camera-raw-vignette-highlights",
                title: "Highlights",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.vignette_highlights,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Protects bright pixels while a dark vignette is applied. Used by Highlight Priority.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.vignette_highlights = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Grain"),
        );
        for row in [
            AmountRow {
                id: "camera-raw-grain-amount",
                title: "Amount",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.grain_amount,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adds film grain, strongest in the middle tones.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.grain_amount = value),
            },
            AmountRow {
                id: "camera-raw-grain-size",
                title: "Size",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.grain_size,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 25.0,
                help: "Makes the grain coarser or finer.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.grain_size = value),
            },
            AmountRow {
                id: "camera-raw-grain-roughness",
                title: "Roughness",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: raw.grain_roughness,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Makes the grain smoother or more uneven.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Column,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.grain_roughness = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }
        column
    }

    fn curve_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let page = panel.curve_page;
        let session = self.session.clone();
        let page_picker = segmented_picker(
            "camera-raw-curve-page",
            CameraRawCurvePage::ALL.map(|page| (page, page.raw_value())),
            page,
            move |page, _, cx| edit_panel(&session, cx, |panel| panel.curve_page = page),
        );
        let mut column = v_flex().items_start().gap(px(8.0)).child(page_picker);

        if page == CameraRawCurvePage::Point {
            let channel = panel.point_channel;
            let session = self.session.clone();
            column = column.child(segmented_picker(
                "camera-raw-curve-channel",
                CameraRawPointChannel::ALL.map(|channel| (channel, channel.raw_value())),
                channel,
                move |channel, _, cx| edit_panel(&session, cx, |panel| panel.point_channel = channel),
            ));
        }

        let points = match panel.point_channel {
            CameraRawPointChannel::Rgb => raw.curve.rgb.clone(),
            CameraRawPointChannel::Red => raw.curve.red.clone(),
            CameraRawPointChannel::Green => raw.curve.green.clone(),
            CameraRawPointChannel::Blue => raw.curve.blue.clone(),
        };
        let selected = self.curve_selected.filter(|index| *index < points.len());
        let accent = cx.theme().primary;
        let point_channel = panel.point_channel;
        let curve = raw.curve.clone();
        let graph_bounds = Rc::new(Cell::new(Bounds::default()));
        let press_bounds = graph_bounds.clone();
        let drag_bounds = graph_bounds.clone();
        let entity = cx.entity();
        let graph_session = self.session.clone();
        let tooltip_page = page;
        let graph = div()
            .id("camera-raw-curve-graph")
            .relative()
            .w_full()
            .h(px(CURVE_GRAPH_HEIGHT))
            .rounded(px(HISTOGRAM_RADIUS))
            .bg(hsla(0.0, 0.0, 0.0, HISTOGRAM_BACKGROUND))
            .overflow_hidden()
            .tooltip(move |window, cx| {
                let help = if tooltip_page == CameraRawCurvePage::Parametric {
                    "Drag up or down to lift or lower those tones. Drag a divider along the bottom to change which tones each region covers."
                } else {
                    "Drag a point. Click to add one. Double-click a point to remove it."
                };
                Tooltip::new(help).build(window, cx)
            })
            .child(canvas(
                {
                    let bounds_writer = graph_bounds.clone();
                    move |bounds, _, _| bounds_writer.set(bounds)
                },
                {
                    let points = points.clone();
                    let curve = curve.clone();
                    move |bounds, (), window, _| {
                        paint_curve_graph(window, bounds, &curve, page, &points, selected, accent);
                    }
                },
            ))
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                let session = graph_session.clone();
                let points = points.clone();
                let curve = curve.clone();
                let point_channel = point_channel;
                move |event: &MouseDownEvent, _, cx| {
                    let bounds = press_bounds.get();
                    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
                        return;
                    }
                    let x = f64::from((event.position.x - bounds.origin.x) / bounds.size.width);
                    let y = 1.0 - f64::from((event.position.y - bounds.origin.y) / bounds.size.height);
                    if event.click_count >= 2 && page == CameraRawCurvePage::Point {
                        let mut next = points.clone();
                        if next.len() > 2 {
                            let index = next
                                .iter()
                                .enumerate()
                                .min_by(|(_, a), (_, b)| {
                                    (a.x - x).abs().total_cmp(&(b.x - x).abs())
                                })
                                .map(|(index, _)| index)
                                .unwrap_or(0);
                            if (next[index].x - x).abs() < 0.04 {
                                next.remove(index);
                                entity.update(cx, |this, cx| {
                                    this.curve_selected = None;
                                    this.curve_drag = None;
                                    CameraRawControls::store_points(&session, point_channel, next, cx);
                                    cx.notify();
                                });
                            }
                        }
                        return;
                    }
                    entity.update(cx, |this, cx| {
                        if page == CameraRawCurvePage::Parametric {
                            let tone = (x * 100.0).clamp(0.0, 100.0);
                            let splits = [
                                curve.shadow_split,
                                curve.dark_split,
                                curve.light_split,
                            ];
                            let index = if y < 18.0 / f64::from(bounds.size.height) {
                                splits
                                    .iter()
                                    .enumerate()
                                    .min_by(|(_, a), (_, b)| {
                                        (*a - tone).abs().total_cmp(&(*b - tone).abs())
                                    })
                                    .map(|(index, _)| index)
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            let drag = if y < 18.0 / f64::from(bounds.size.height) {
                                CurveDrag::Divider(index)
                            } else {
                                let region = curve.region(tone);
                                CurveDrag::Region(region, curve.amount(region))
                            };
                            this.curve_drag = Some(CurveGesture {
                                drag,
                                start: event.position,
                            });
                        } else {
                            let near = points
                                .iter()
                                .enumerate()
                                .min_by(|(_, a), (_, b)| {
                                    ((a.x - x).powi(2) + (a.y - y).powi(2))
                                        .total_cmp(&((b.x - x).powi(2) + (b.y - y).powi(2)))
                                })
                                .map(|(index, _)| index);
                            if let Some(index) = near {
                                let distance = ((points[index].x - x).powi(2)
                                    + (points[index].y - y).powi(2))
                                .sqrt();
                                if distance < 0.055 {
                                    this.curve_selected = Some(index);
                                    this.curve_drag = Some(CurveGesture {
                                        drag: CurveDrag::Point(index),
                                        start: event.position,
                                    });
                                    return;
                                }
                            }
                            if points.len() < 16
                                && x > 0.01
                                && x < 0.99
                                && points.iter().all(|point| (point.x - x).abs() > 0.01)
                            {
                                let mut next = points.clone();
                                next.push(CurvePoint::new(x, y.clamp(0.0, 1.0)));
                                next.sort_by(|a, b| a.x.total_cmp(&b.x));
                                let index = next
                                    .iter()
                                    .position(|point| point.x == x && point.y == y.clamp(0.0, 1.0))
                                    .unwrap_or(next.len() - 1);
                                this.curve_selected = Some(index);
                                this.curve_drag = Some(CurveGesture {
                                    drag: CurveDrag::Point(index),
                                    start: event.position,
                                });
                                CameraRawControls::store_points(&session, point_channel, next, cx);
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .on_drag(CurveGraphDrag, |_, _, _, cx| cx.new(|_| CurveGraphDrag))
            .on_drag_move::<CurveGraphDrag>({
                let entity = entity.clone();
                let session = graph_session.clone();
                let points = points.clone();
                let point_channel = point_channel;
                move |event: &DragMoveEvent<CurveGraphDrag>, _, cx| {
                    let bounds = drag_bounds.get();
                    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
                        return;
                    }
                    let x = f64::from((event.event.position.x - bounds.origin.x) / bounds.size.width);
                    let y = 1.0
                        - f64::from((event.event.position.y - bounds.origin.y) / bounds.size.height);
                    entity.update(cx, |this, cx| {
                        let Some(gesture) = this.curve_drag else {
                            return;
                        };
                        match gesture.drag {
                            CurveDrag::Point(index) => {
                                let mut next = points.clone();
                                if index < next.len() {
                                    next[index].y = y.clamp(0.0, 1.0);
                                    if index > 0 && index + 1 < next.len() {
                                        next[index].x = (next[index + 1].x - 0.01)
                                            .max(next[index - 1].x + 0.01)
                                            .min(x);
                                    }
                                    CameraRawControls::store_points(
                                        &session,
                                        point_channel,
                                        next,
                                        cx,
                                    );
                                }
                            }
                            CurveDrag::Divider(index) => {
                                let value = (x * 100.0).clamp(2.0, 98.0);
                                update_camera_raw(&session, cx, |raw| match index {
                                    0 => {
                                        raw.curve.shadow_split =
                                            value.min(raw.curve.dark_split - 2.0)
                                    }
                                    1 => {
                                        raw.curve.dark_split = value
                                            .clamp(raw.curve.shadow_split + 2.0, raw.curve.light_split - 2.0)
                                    }
                                    _ => {
                                        raw.curve.light_split =
                                            value.max(raw.curve.dark_split + 2.0)
                                    }
                                });
                            }
                            CurveDrag::Region(region, start) => {
                                let start_y = 1.0
                                    - f64::from(
                                        (gesture.start.y - bounds.origin.y) / bounds.size.height,
                                    );
                                let delta = (y - start_y).clamp(-1.0, 1.0);
                                let value = (start - delta * 200.0).clamp(-100.0, 100.0).round();
                                update_camera_raw(&session, cx, |raw| {
                                    raw.curve.set_amount(region, value)
                                });
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.curve_drag = None;
                        cx.notify();
                    });
                }
            });
        column = column.child(graph);

        if page == CameraRawCurvePage::Parametric {
            for row in [
                ("Highlights", raw.curve.highlights, "camera-raw-curve-highlights", CameraRawCurveRegion::Highlights, "Lifts or lowers the brightest tones."),
                ("Lights", raw.curve.lights, "camera-raw-curve-lights", CameraRawCurveRegion::Lights, "Lifts or lowers the light tones."),
                ("Darks", raw.curve.darks, "camera-raw-curve-darks", CameraRawCurveRegion::Darks, "Lifts or lowers the dark tones."),
                ("Shadows", raw.curve.shadows, "camera-raw-curve-shadows", CameraRawCurveRegion::Shadows, "Lifts or lowers the darkest tones."),
            ] {
                let (title, value, id, region, help) = row;
                let session = self.session.clone();
                column = column.child(self.amount_row(
                    AmountRow {
                        id,
                        title,
                        label_width: 88.0,
                        field_width: Some(48.0),
                        value,
                        range: CameraRawSettings::TONE_RANGE,
                        decimals: 0,
                        reset_value: 0.0,
                        help: help.into(),
                        track: CameraRawSliderTrack::Plain,
                        scrub: true,
                        label_reset: true,
                        rounding: Rounding::Whole,
                        kind: AssignKind::Column,
                        clipping: None,
                        masking: false,
                        enabled: true,
                        set: Rc::new(move |value, raw| raw.curve.set_amount(region, value)),
                    },
                    cx,
                ));
                let _ = session;
            }
        } else {
            if let Some(point) = selected.and_then(|index| points.get(index).copied()) {
                column = column.child(
                    div()
                        .text_size(px(CAPTION_SIZE))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().tokens.muted_foreground)
                        .child(format!(
                            "In {}   Out {}",
                            (point.x * 255.0).round() as i64,
                            (point.y * 255.0).round() as i64
                        )),
                );
            }
            let preset = CurvePreset::matching(&points);
            let session = self.session.clone();
            column = column.child(menu_picker(
                "camera-raw-curve-preset",
                CurvePreset::ALL.map(|preset| (preset, preset.raw_value())),
                preset,
                move |preset, _, cx| {
                    if let Some(points) = preset.points() {
                        let session = session.clone();
                        session.update(cx, |session, cx| {
                            let Some(edit) = session.filter_edit.as_mut() else {
                                return;
                            };
                            match edit.panel.point_channel {
                                CameraRawPointChannel::Rgb => edit.camera_raw.curve.rgb = points,
                                CameraRawPointChannel::Red => edit.camera_raw.curve.red = points,
                                CameraRawPointChannel::Green => edit.camera_raw.curve.green = points,
                                CameraRawPointChannel::Blue => edit.camera_raw.curve.blue = points,
                            }
                            let settings = edit.settings.clone();
                            let preview = edit.preview;
                            session.update_filter(settings, preview);
                            cx.notify();
                        });
                    }
                },
            ));
            if panel.point_channel == CameraRawPointChannel::Rgb {
                let refine = raw.curve.refine_saturation;
                let session = self.session.clone();
                column = column.child(self.amount_row(
                    AmountRow {
                        id: "camera-raw-refine-saturation",
                        title: "Refine Saturation",
                        label_width: LABEL_WIDTH,
                        field_width: Some(FIELD_WIDTH),
                        value: refine,
                        range: CameraRawSettings::TONE_RANGE,
                        decimals: 0,
                        reset_value: 0.0,
                        help: "How much the curve also changes color strength. Zero matches Photoshop; lower keeps it to brightness, higher adds more color.".into(),
                        track: CameraRawSliderTrack::Plain,
                        scrub: true,
                        label_reset: true,
                        rounding: Rounding::Whole,
                        kind: AssignKind::Column,
                        clipping: None,
                        masking: false,
                        enabled: true,
                        set: Rc::new(|value, raw| raw.curve.refine_saturation = value),
                    },
                    cx,
                ));
                let _ = session;
            }
        }

        let targets = panel.targets_curve;
        let session = self.session.clone();
        column = column.child(
            Button::new("camera-raw-curve-target")
                .label("Targeted Adjustment")
                .selected(targets)
                .tooltip("Drag on the picture to move the curve for the tone under the pointer.")
                .on_click(move |_, _, cx| {
                    edit_panel(&session, cx, |panel| {
                        panel.targets_mixer = false;
                        panel.targets_curve = !panel.targets_curve;
                    });
                }),
        );
        column
    }

    fn grading_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let page = panel.grade_page;
        let session = self.session.clone();
        let picker = menu_picker(
            "camera-raw-grade-page",
            CameraRawGradePage::ALL.map(|page| (page, page.raw_value())),
            page,
            move |page, _, cx| edit_panel(&session, cx, |panel| panel.grade_page = page),
        );
        let mut column = v_flex().items_start().gap(px(8.0)).child(picker);

        let wheels = match page {
            CameraRawGradePage::ThreeWay => vec![
                ("Shadows", GradeWheelKey::Shadows, raw.grading.shadows),
                ("Midtones", GradeWheelKey::Midtones, raw.grading.midtones),
                ("Highlights", GradeWheelKey::Highlights, raw.grading.highlights),
            ],
            _ => vec![(
                page.raw_value(),
                GradeWheelKey::of_page(page),
                GradeWheelKey::of_page(page).wheel(&raw.grading),
            )],
        };
        let wheel_row = if wheels.len() == 3 {
            h_flex()
                .items_start()
                .gap(px(30.0))
                .children(
                    wheels
                        .into_iter()
                        .map(|(title, key, wheel)| self.grade_wheel(title, key, wheel, cx)),
                )
                .into_any_element()
        } else {
            let (title, key, wheel) = wheels[0];
            self.grade_wheel(title, key, wheel, cx).into_any_element()
        };
        column = column.child(wheel_row);

        let blending = raw.grading.blending;
        column = column.child(self.amount_row(
            AmountRow {
                id: "camera-raw-grade-blending",
                title: "Blending",
                label_width: 78.0,
                field_width: None,
                value: blending,
                range: CameraRawSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Controls how much the three tonal wheels overlap.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.grading.blending = value),
            },
            cx,
        ));
        let balance = raw.grading.balance;
        column = column.child(self.amount_row(
            AmountRow {
                id: "camera-raw-grade-balance",
                title: "Balance",
                label_width: 78.0,
                field_width: None,
                value: balance,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Shifts the wheels toward shadows or highlights.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.grading.balance = value),
            },
            cx,
        ));
        column
    }

    fn grade_wheel(
        &mut self,
        title: &'static str,
        key: GradeWheelKey,
        wheel: CameraRawGradeWheel,
        cx: &mut Context<Self>,
    ) -> Div {
        let accent = cx.theme().primary;
        let bounds_cell = Rc::new(Cell::new(Bounds::default()));
        let press_bounds = bounds_cell.clone();
        let drag_bounds = bounds_cell.clone();
        let entity = cx.entity();
        let session = self.session.clone();
        let graph = div()
            .id(SharedString::from(format!(
                "camera-raw-grade-wheel-{}",
                title.to_lowercase().replace(' ', "-")
            )))
            .relative()
            .w(px(WHEEL_SIZE))
            .h(px(WHEEL_SIZE))
            .tooltip(|window, cx| {
                Tooltip::new("Drag to set hue and saturation. Double-click to reset this wheel.")
                    .build(window, cx)
            })
            .child(canvas(
                {
                    let writer = bounds_cell.clone();
                    move |bounds, _, _| writer.set(bounds)
                },
                move |bounds, _, window, _| paint_hue_wheel(window, bounds, wheel.hue, wheel.saturation),
            ))
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                let session = session.clone();
                move |event, _, cx| {
                    let bounds = press_bounds.get();
                    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
                        return;
                    }
                    let (hue, saturation) = wheel_point(event.position, bounds);
                    entity.update(cx, |_, cx| {
                        update_camera_raw(&session, cx, |raw| {
                            key.set_hue_saturation(&mut raw.grading, hue, saturation);
                        });
                        cx.notify();
                    });
                }
            })
            .on_drag(GradeWheelDrag, |_, _, _, cx| cx.new(|_| GradeWheelDrag))
            .on_drag_move::<GradeWheelDrag>({
                let entity = entity.clone();
                let session = session.clone();
                move |event, _, cx| {
                    let bounds = drag_bounds.get();
                    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
                        return;
                    }
                    let (hue, saturation) = wheel_point(event.event.position, bounds);
                    entity.update(cx, |_, cx| {
                        update_camera_raw(&session, cx, |raw| {
                            key.set_hue_saturation(&mut raw.grading, hue, saturation);
                        });
                        cx.notify();
                    });
                }
            })
            .on_click(move |event, _, cx| {
                if event.click_count() >= 2 {
                    let session = session.clone();
                    entity.update(cx, |_, cx| {
                        update_camera_raw(&session, cx, |raw| {
                            key.set_hue_saturation(&mut raw.grading, 0.0, 0.0);
                        });
                        cx.notify();
                    });
                }
            });

        let readout = div()
            .text_size(px(CAPTION2_SIZE))
            .font_family(cx.theme().mono_font_family.clone())
            .text_color(cx.theme().tokens.muted_foreground)
            .child(format!(
                "{}°  {}°",
                wheel.hue.round() as i64,
                wheel.saturation.round() as i64
            ));
        let luminance = self.amount_row(
            AmountRow {
                id: match key {
                    GradeWheelKey::Shadows => "camera-raw-grade-shadows-luminance",
                    GradeWheelKey::Midtones => "camera-raw-grade-midtones-luminance",
                    GradeWheelKey::Highlights => "camera-raw-grade-highlights-luminance",
                    GradeWheelKey::Global => "camera-raw-grade-global-luminance",
                },
                title,
                label_width: 0.0,
                field_width: Some(48.0),
                value: wheel.luminance,
                range: CameraRawSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Brightness added by this wheel.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: false,
                label_reset: false,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(move |value, raw| key.set_luminance(&mut raw.grading, value)),
            },
            cx,
        );

        v_flex()
            .items_start()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(cx.theme().tokens.muted_foreground)
                    .child(title),
            )
            .child(graph)
            .child(readout)
            .child(luminance)
    }

    fn mixer_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let page = panel.mixer_page;
        let session = self.session.clone();
        let page_picker = segmented_picker(
            "camera-raw-mixer-page",
            CameraRawMixerPage::ALL.map(|page| (page, page.raw_value())),
            page,
            move |page, _, cx| edit_panel(&session, cx, |panel| panel.mixer_page = page),
        );
        let mut column = v_flex().items_start().gap(px(8.0)).child(page_picker);

        match page {
            CameraRawMixerPage::Hsl => {
                let tab = panel.mixer_tab;
                let session = self.session.clone();
                column = column.child(segmented_picker(
                    "camera-raw-mixer-tab",
                    CameraRawMixerTab::ALL.map(|tab| (tab, tab.raw_value())),
                    tab,
                    move |tab, _, cx| edit_panel(&session, cx, |panel| panel.mixer_tab = tab),
                ));
                const IDS: [&str; 8] = [
                    "camera-raw-mixer-reds",
                    "camera-raw-mixer-oranges",
                    "camera-raw-mixer-yellows",
                    "camera-raw-mixer-greens",
                    "camera-raw-mixer-aquas",
                    "camera-raw-mixer-blues",
                    "camera-raw-mixer-purples",
                    "camera-raw-mixer-magentas",
                ];
                let names = CameraRawMixerSettings::names();
                for index in 0..8 {
                    let value = match tab {
                        CameraRawMixerTab::Hue => raw.mixer.hue[index],
                        CameraRawMixerTab::Saturation => raw.mixer.saturation[index],
                        CameraRawMixerTab::Luminance => raw.mixer.luminance[index],
                    };
                    let help = SharedString::from(format!(
                        "{} of {}.",
                        tab.raw_value(),
                        names[index]
                    ));
                    column = column.child(self.amount_row(
                        AmountRow {
                            id: IDS[index],
                            title: names[index],
                            label_width: 78.0,
                            field_width: Some(48.0),
                            value,
                            range: CameraRawSettings::TONE_RANGE,
                            decimals: 0,
                            reset_value: 0.0,
                            help,
                            track: family_track(index, tab),
                            scrub: true,
                            label_reset: true,
                            rounding: Rounding::Whole,
                            kind: AssignKind::Plain,
                            clipping: None,
                            masking: false,
                            enabled: true,
                            set: Rc::new(move |value, raw| match tab {
                                CameraRawMixerTab::Hue => raw.mixer.hue[index] = value,
                                CameraRawMixerTab::Saturation => raw.mixer.saturation[index] = value,
                                CameraRawMixerTab::Luminance => raw.mixer.luminance[index] = value,
                            }),
                        },
                        cx,
                    ));
                }
            }
            CameraRawMixerPage::Color => {
                let index = panel.mixer_swatch.min(7);
                let centers = CameraRawMixerSettings::centers();
                let names = CameraRawMixerSettings::names();
                let mut swatches = h_flex().gap(px(6.0));
                for swatch in 0..8 {
                    let selected = swatch == index;
                    let session = self.session.clone();
                    swatches = swatches.child(
                        div()
                            .id(SharedString::from(format!("camera-raw-mixer-swatch-{swatch}")))
                            .w(px(MIXER_SWATCH))
                            .h(px(MIXER_SWATCH))
                            .rounded(px(MIXER_SWATCH / 2.0))
                            .bg(Hsla::from(hsb(
                                (centers[swatch] / 360.0) as f32,
                                0.8,
                                0.9,
                            )))
                            .border_2()
                            .border_color(if selected {
                                hsla(0.0, 0.0, 1.0, 1.0)
                            } else {
                                hsla(0.0, 0.0, 0.0, 0.0)
                            })
                            .cursor(CursorStyle::PointingHand)
                            .tooltip(move |window, cx| {
                                Tooltip::new(format!("Edit {}.", names[swatch])).build(window, cx)
                            })
                            .on_click(move |_, _, cx| {
                                edit_panel(&session, cx, |panel| panel.mixer_swatch = swatch);
                            }),
                    );
                }
                column = column.child(swatches);
                for (title, id, values) in [
                    ("Hue", "camera-raw-mixer-color-hue", &raw.mixer.hue[..]),
                    (
                        "Saturation",
                        "camera-raw-mixer-color-saturation",
                        &raw.mixer.saturation[..],
                    ),
                    (
                        "Luminance",
                        "camera-raw-mixer-color-luminance",
                        &raw.mixer.luminance[..],
                    ),
                ] {
                    let value = values[index];
                    let help = SharedString::from(match title {
                        "Hue" => "Shifts the selected color family around the wheel.",
                        "Saturation" => "Makes the selected color family stronger or quieter.",
                        _ => "Makes the selected color family lighter or darker.",
                    });
                    column = column.child(self.amount_row(
                        AmountRow {
                            id,
                            title,
                            label_width: 78.0,
                            field_width: None,
                            value,
                            range: CameraRawSettings::TONE_RANGE,
                            decimals: 0,
                            reset_value: 0.0,
                            help,
                            track: family_track(index, CameraRawMixerTab::Hue),
                            scrub: false,
                            label_reset: false,
                            rounding: Rounding::Whole,
                            kind: AssignKind::Plain,
                            clipping: None,
                            masking: false,
                            enabled: true,
                            set: Rc::new(move |value, raw| match title {
                                "Hue" => raw.mixer.hue[index] = value,
                                "Saturation" => raw.mixer.saturation[index] = value,
                                _ => raw.mixer.luminance[index] = value,
                            }),
                        },
                        cx,
                    ));
                }
            }
            CameraRawMixerPage::Point => {
                let sample_session = self.session.clone();
                let sample_armed = panel.samples_point_color;
                column = column.child(
                    Button::new("camera-raw-point-eyedropper")
                        .icon(Icon::new(IconName::Pipette).size(px(14.0)))
                        .selected(sample_armed)
                        .tooltip("Click the picture to save a color. Up to eight colors.")
                        .accessibility_label("Point Color Selector")
                        .on_click(move |_, _, cx| {
                            arm_eyedropper(&sample_session, cx, |panel| {
                                panel.samples_point_color = !panel.samples_point_color;
                            });
                        }),
                );
                let mut points = h_flex().gap(px(6.0));
                for (index, point) in raw.mixer.points.iter().enumerate() {
                    let selected = index == panel.point_index;
                    let session = self.session.clone();
                    points = points.child(
                        div()
                            .id(SharedString::from(format!("camera-raw-point-{index}")))
                            .w(px(POINT_SWATCH))
                            .h(px(POINT_SWATCH))
                            .rounded(px(POINT_SWATCH / 2.0))
                            .bg(Hsla::from(hsb(
                                (point.hue / 360.0) as f32,
                                point.saturation as f32,
                                point.luminance as f32,
                            )))
                            .border_2()
                            .border_color(if selected {
                                hsla(0.0, 0.0, 1.0, 1.0)
                            } else {
                                hsla(0.0, 0.0, 0.0, 0.0)
                            })
                            .cursor(CursorStyle::PointingHand)
                            .tooltip(|window, cx| {
                                Tooltip::new("Select this picked color.").build(window, cx)
                            })
                            .on_click(move |_, _, cx| {
                                edit_panel(&session, cx, |panel| panel.point_index = index);
                            }),
                    );
                }
                column = column.child(points);
                if let Some(point) = raw.mixer.points.get(panel.point_index).copied() {
                    for (title, id, value, range, reset, track) in [
                        (
                            "Hue Shift",
                            "camera-raw-point-hue-shift",
                            point.hue_shift,
                            CameraRawSettings::TONE_RANGE,
                            0.0,
                            CameraRawSliderTrack::Hue(point.hue),
                        ),
                        (
                            "Saturation Shift",
                            "camera-raw-point-saturation-shift",
                            point.saturation_shift,
                            CameraRawSettings::TONE_RANGE,
                            0.0,
                            CameraRawSliderTrack::Saturation(point.hue),
                        ),
                        (
                            "Luminance Shift",
                            "camera-raw-point-luminance-shift",
                            point.luminance_shift,
                            CameraRawSettings::TONE_RANGE,
                            0.0,
                            CameraRawSliderTrack::Luminance(point.hue),
                        ),
                        (
                            "Hue Range",
                            "camera-raw-point-hue-range",
                            point.hue_range,
                            (5.0, 180.0),
                            30.0,
                            CameraRawSliderTrack::Plain,
                        ),
                        (
                            "Saturation Range",
                            "camera-raw-point-saturation-range",
                            point.saturation_range,
                            (0.05, 1.0),
                            0.4,
                            CameraRawSliderTrack::Plain,
                        ),
                        (
                            "Luminance Range",
                            "camera-raw-point-luminance-range",
                            point.luminance_range,
                            (0.05, 1.0),
                            0.4,
                            CameraRawSliderTrack::Plain,
                        ),
                    ] {
                        let index = panel.point_index;
                        column = column.child(self.amount_row(
                            AmountRow {
                                id,
                                title,
                                label_width: 110.0,
                                field_width: None,
                                value,
                                range,
                                decimals: if range.1 <= 1.0 { 2 } else { 0 },
                                reset_value: reset,
                                help: SharedString::from(title),
                                track,
                                scrub: false,
                                label_reset: false,
                                rounding: Rounding::Step,
                                kind: AssignKind::Plain,
                                clipping: None,
                                masking: false,
                                enabled: true,
                                set: Rc::new(move |value, raw| {
                                    if let Some(point) = raw.mixer.points.get_mut(index) {
                                        match title {
                                            "Hue Shift" => point.hue_shift = value,
                                            "Saturation Shift" => point.saturation_shift = value,
                                            "Luminance Shift" => point.luminance_shift = value,
                                            "Hue Range" => point.hue_range = value,
                                            "Saturation Range" => point.saturation_range = value,
                                            _ => point.luminance_range = value,
                                        }
                                    }
                                }),
                            },
                            cx,
                        ));
                    }
                    let index = panel.point_index;
                    let checked = point.visualize;
                    let session = self.session.clone();
                    column = column.child(
                        Switch::new("camera-raw-point-visualize")
                            .label("Visualize Range")
                            .accessibility_label("Visualize Range")
                            .checked(checked)
                            .tooltip("Dims the picture outside this color's range. It is not kept when you press OK.")
                            .on_click(move |value, _, cx| {
                                let value = *value;
                                update_camera_raw(&session, cx, move |raw| {
                                    if let Some(point) = raw.mixer.points.get_mut(index) {
                                        point.visualize = value;
                                    }
                                });
                            }),
                    );
                }
            }
        }

        let armed = panel.targets_mixer;
        let session = self.session.clone();
        column = column.child(
            Button::new("camera-raw-mixer-target")
                .label("Targeted Adjustment")
                .selected(armed)
                .tooltip("Drag a color in the picture. Nearby color families move together.")
                .on_click(move |_, _, cx| {
                    edit_panel(&session, cx, |panel| {
                        panel.targets_curve = false;
                        panel.targets_mixer = !panel.targets_mixer;
                    });
                }),
        );
        column
    }

    fn detail_controls(&mut self, raw: &CameraRawSettings, cx: &mut Context<Self>) -> Div {
        let detail = raw.detail;
        let mut column = v_flex().items_start().gap(px(8.0));
        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Sharpening"),
        );
        for row in [
            AmountRow {
                id: "camera-raw-sharpen-amount",
                title: "Amount",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.sharpen_amount,
                range: CameraRawDetailSettings::SHARPEN_AMOUNT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Controls how strong the sharpening is.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.sharpen_amount = value),
            },
            AmountRow {
                id: "camera-raw-sharpen-radius",
                title: "Radius",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.sharpen_radius,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 10.0,
                help: "How far from each edge the sharpening reaches, in pixels.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.sharpen_radius = value),
            },
            AmountRow {
                id: "camera-raw-sharpen-detail",
                title: "Detail",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.sharpen_detail,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 25.0,
                help: "Emphasizes fine texture over broader edges.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.sharpen_detail = value),
            },
            AmountRow {
                id: "camera-raw-sharpen-masking",
                title: "Masking",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.sharpen_masking,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Limits sharpening to stronger edges. Hold Option to see the mask.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: true,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.sharpen_masking = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Noise Reduction"),
        );
        let luminance_enabled = detail.noise_luminance > 0.0;
        for row in [
            AmountRow {
                id: "camera-raw-noise-luminance",
                title: "Luminance",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_luminance,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Smooths grain and noise in brightness.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.noise_luminance = value),
            },
            AmountRow {
                id: "camera-raw-noise-luminance-detail",
                title: "Luminance Detail",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_luminance_detail,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Preserves fine texture while luminance noise is reduced.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: luminance_enabled,
                set: Rc::new(|value, raw| raw.detail.noise_luminance_detail = value),
            },
            AmountRow {
                id: "camera-raw-noise-luminance-contrast",
                title: "Luminance Contrast",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_luminance_contrast,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Keeps local contrast after luminance smoothing.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: luminance_enabled,
                set: Rc::new(|value, raw| raw.detail.noise_luminance_contrast = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        let color_enabled = detail.noise_color > 0.0;
        for row in [
            AmountRow {
                id: "camera-raw-noise-color",
                title: "Color",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_color,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Smooths colored speckles.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.detail.noise_color = value),
            },
            AmountRow {
                id: "camera-raw-noise-color-detail",
                title: "Color Detail",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_color_detail,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Preserves colored edges while color noise is reduced.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: color_enabled,
                set: Rc::new(|value, raw| raw.detail.noise_color_detail = value),
            },
            AmountRow {
                id: "camera-raw-noise-color-smoothness",
                title: "Color Smoothness",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: detail.noise_color_smoothness,
                range: CameraRawDetailSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Makes the color smoothing softer or tighter.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Detail,
                clipping: None,
                masking: false,
                enabled: color_enabled,
                set: Rc::new(|value, raw| raw.detail.noise_color_smoothness = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }
        column
    }

    fn optics_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let optics = raw.optics;
        let mut column = v_flex().items_start().gap(px(8.0));
        let optics_switches: [(&str, &str, bool, &str, fn(bool, &mut CameraRawSettings)); 2] = [
            (
                "camera-raw-remove-chromatic",
                "Remove Chromatic Aberration",
                optics.remove_chromatic_aberration,
                "Pulls red and blue fringes apart toward the center to reduce color edging.",
                |value: bool, raw: &mut CameraRawSettings| {
                    raw.optics.remove_chromatic_aberration = value
                },
            ),
            (
                "camera-raw-lens-profile",
                "Enable Lens Profile Corrections",
                optics.enable_lens_profile,
                "Applies generic profile strength when camera metadata is not available.",
                |value: bool, raw: &mut CameraRawSettings| raw.optics.enable_lens_profile = value,
            ),
        ];
        for (id, title, checked, help, set) in optics_switches {
            let session = self.session.clone();
            column = column.child(
                Switch::new(id)
                    .label(title)
                    .accessibility_label(title)
                    .checked(checked)
                    .tooltip(help)
                    .on_click(move |value, _, cx| {
                        let value = *value;
                        update_camera_raw(&session, cx, move |raw| set(value, raw));
                    }),
            );
        }

        if optics.enable_lens_profile {
            column = column.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(cx.theme().tokens.muted_foreground)
                    .child("No lens metadata on this layer. Profile sliders set generic correction strength."),
            );
            for row in [
                AmountRow {
                    id: "camera-raw-profile-distortion",
                    title: "Distortion",
                    label_width: LABEL_WIDTH,
                    field_width: Some(FIELD_WIDTH),
                    value: optics.profile_distortion,
                    range: CameraRawOpticsSettings::UNIT_RANGE,
                    decimals: 0,
                    reset_value: 100.0,
                    help: "How much of the profile distortion correction is applied.".into(),
                    track: CameraRawSliderTrack::Plain,
                    scrub: true,
                    label_reset: true,
                    rounding: Rounding::Whole,
                    kind: AssignKind::Plain,
                    clipping: None,
                    masking: false,
                    enabled: true,
                    set: Rc::new(|value, raw| raw.optics.profile_distortion = value),
                },
                AmountRow {
                    id: "camera-raw-profile-vignetting",
                    title: "Vignetting",
                    label_width: LABEL_WIDTH,
                    field_width: Some(FIELD_WIDTH),
                    value: optics.profile_vignetting,
                    range: CameraRawOpticsSettings::UNIT_RANGE,
                    decimals: 0,
                    reset_value: 100.0,
                    help: "How much of the profile vignetting correction is applied.".into(),
                    track: CameraRawSliderTrack::Plain,
                    scrub: true,
                    label_reset: true,
                    rounding: Rounding::Whole,
                    kind: AssignKind::Plain,
                    clipping: None,
                    masking: false,
                    enabled: true,
                    set: Rc::new(|value, raw| raw.optics.profile_vignetting = value),
                },
            ] {
                column = column.child(self.amount_row(row, cx));
            }
        }

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Manual"),
        );
        for row in [
            AmountRow {
                id: "camera-raw-optics-distortion",
                title: "Distortion",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: optics.distortion,
                range: CameraRawOpticsSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Straightens barrel or pincushion bending.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.optics.distortion = value),
            },
            AmountRow {
                id: "camera-raw-purple-amount",
                title: "Purple Amount",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: optics.purple_amount,
                range: CameraRawOpticsSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Weakens purple fringes inside the purple hue range.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.optics.purple_amount = value),
            },
            AmountRow {
                id: "camera-raw-green-amount",
                title: "Green Amount",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: optics.green_amount,
                range: CameraRawOpticsSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Weakens green fringes inside the green hue range.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.optics.green_amount = value),
            },
            AmountRow {
                id: "camera-raw-optics-vignette",
                title: "Vignetting",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: optics.vignette_amount,
                range: CameraRawOpticsSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Brightens or darkens the corners to counter lens falloff.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.optics.vignette_amount = value),
            },
            AmountRow {
                id: "camera-raw-optics-vignette-midpoint",
                title: "Midpoint",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: optics.vignette_midpoint,
                range: CameraRawOpticsSettings::UNIT_RANGE,
                decimals: 0,
                reset_value: 50.0,
                help: "Moves the vignette correction inward or outward.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.optics.vignette_midpoint = value),
            },
        ] {
            column = column.child(self.amount_row(row, cx));
        }

        let sample_session = self.session.clone();
        let armed = panel.samples_defringe;
        column = column.child(
            h_flex()
                .items_center()
                .gap(px(CONTROL_SPACING))
                .child(
                    div()
                        .id("camera-raw-defringe-label")
                        .flex_none()
                        .w(px(LABEL_WIDTH))
                        .child("Defringe")
                        .tooltip(|window, cx| {
                            Tooltip::new("Click a purple or green fringe to set its hue range.")
                                .build(window, cx)
                        }),
                )
                .child(
                    Button::new("camera-raw-defringe-eyedropper")
                        .icon(Icon::new(IconName::Pipette).size(px(14.0)))
                        .selected(armed)
                        .tooltip("Click a purple or green fringe to set its hue range.")
                        .accessibility_label("Defringe Selector")
                        .on_click(move |_, _, cx| {
                            arm_eyedropper(&sample_session, cx, |panel| {
                                panel.samples_defringe = !panel.samples_defringe;
                            });
                        }),
                ),
        );
        if armed {
            column = column.child(
                div()
                    .text_size(px(CAPTION_SIZE))
                    .text_color(cx.theme().tokens.muted_foreground)
                    .child("Click the fringe on the layer. Click the eyedropper again to stop."),
            );
        }
        column
    }

    fn geometry_controls(
        &mut self,
        raw: &CameraRawSettings,
        panel: &CameraRawPanel,
        cx: &mut Context<Self>,
    ) -> Div {
        let geometry = raw.geometry.clone();
        let upright = geometry.upright;
        let session = self.session.clone();
        let upright_picker = segmented_picker(
            "camera-raw-upright",
            CameraRawUprightMode::ALL.map(|mode| (mode, mode.raw_value())),
            upright,
            move |mode, _, cx| {
                let session = session.clone();
                session.update(cx, |session, cx| {
                    if let Some(edit) = session.filter_edit.as_mut() {
                        edit.camera_raw.geometry.upright = mode;
                        if mode != CameraRawUprightMode::Guided {
                            edit.panel.drawing_geometry_guide = false;
                        }
                        let settings = edit.settings.clone();
                        let preview = edit.preview;
                        session.update_filter(settings, preview);
                    }
                    cx.notify();
                });
            },
        );
        let mut column = v_flex().items_start().gap(px(8.0));
        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Upright"),
        );
        column = column.child(upright_picker);

        if upright == CameraRawUprightMode::Guided {
            let drawing = panel.drawing_geometry_guide;
            let session = self.session.clone();
            column = column.child(
                Button::new("camera-raw-draw-guides")
                    .label("Draw Guides")
                    .selected(drawing)
                    .tooltip("Draw two or more lines on the preview that should be level or vertical.")
                    .on_click(move |_, _, cx| {
                        arm_eyedropper(&session, cx, |panel| {
                            panel.drawing_geometry_guide = !panel.drawing_geometry_guide;
                        });
                    }),
            );
            if drawing {
                column = column.child(
                    div()
                        .text_size(px(CAPTION_SIZE))
                        .text_color(cx.theme().tokens.muted_foreground)
                        .child("Drag on the layer to place a guide. Draw at least two lines."),
                );
            }
            if !geometry.guides.is_empty() {
                let session = self.session.clone();
                column = column.child(
                    Button::new("camera-raw-clear-guides")
                        .label("Clear Guides")
                        .tooltip("Remove every guide line.")
                        .on_click(move |_, _, cx| {
                            update_camera_raw(&session, cx, |raw| raw.geometry.guides.clear());
                        }),
                );
            }
        }

        let projection = geometry.projection;
        let session = self.session.clone();
        column = column.child(segmented_picker(
            "camera-raw-projection",
            CameraRawProjection::ALL.map(|mode| (mode, mode.raw_value())),
            projection,
            move |mode, _, cx| {
                update_camera_raw(&session, cx, |raw| raw.geometry.projection = mode);
            },
        ));

        let geometry_rows: [(&str, &str, f64, (f64, f64), &str, (f64, f64), f64, fn(f64, &mut CameraRawSettings)); 7] = [
            (
                "Vertical",
                "camera-raw-geometry-vertical",
                geometry.vertical,
                CameraRawGeometrySettings::TONE_RANGE,
                "Straightens vertical lines toward the center.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.vertical = value,
            ),
            (
                "Horizontal",
                "camera-raw-geometry-horizontal",
                geometry.horizontal,
                CameraRawGeometrySettings::TONE_RANGE,
                "Straightens horizontal lines toward the center.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.horizontal = value,
            ),
            (
                "Rotate",
                "camera-raw-geometry-rotate",
                geometry.rotate,
                CameraRawGeometrySettings::ROTATE_RANGE,
                "Rotates the picture around its center.",
                CameraRawGeometrySettings::ROTATE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.rotate = value,
            ),
            (
                "Aspect",
                "camera-raw-geometry-aspect",
                geometry.aspect,
                CameraRawGeometrySettings::TONE_RANGE,
                "Stretches width relative to height.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.aspect = value,
            ),
            (
                "Scale",
                "camera-raw-geometry-scale",
                geometry.scale,
                CameraRawGeometrySettings::TONE_RANGE,
                "Zooms the transformed picture within the frame.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.scale = value,
            ),
            (
                "Offset X",
                "camera-raw-geometry-offset-x",
                geometry.offset_x,
                CameraRawGeometrySettings::TONE_RANGE,
                "Moves the picture left or right.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.offset_x = value,
            ),
            (
                "Offset Y",
                "camera-raw-geometry-offset-y",
                geometry.offset_y,
                CameraRawGeometrySettings::TONE_RANGE,
                "Moves the picture up or down.",
                CameraRawGeometrySettings::TONE_RANGE,
                0.0,
                |value: f64, raw: &mut CameraRawSettings| raw.geometry.offset_y = value,
            ),
        ];
        for row in geometry_rows {
            let (title, id, value, range, help, field_range, reset, set) = row;
            column = column.child(self.amount_row(
                AmountRow {
                    id,
                    title,
                    label_width: LABEL_WIDTH,
                    field_width: Some(FIELD_WIDTH),
                    value,
                    range,
                    decimals: 0,
                    reset_value: reset,
                    help: help.into(),
                    track: CameraRawSliderTrack::Plain,
                    scrub: true,
                    label_reset: true,
                    rounding: Rounding::Whole,
                    kind: AssignKind::Plain,
                    clipping: None,
                    masking: false,
                    enabled: true,
                    set: Rc::new(set),
                },
                cx,
            ));
            let _ = field_range;
        }

        let constrain = geometry.constrain_crop;
        let session = self.session.clone();
        column = column.child(
            Switch::new("camera-raw-constrain-crop")
                .label("Constrain Crop")
                .accessibility_label("Constrain Crop")
                .checked(constrain)
                .tooltip("Crops empty edges after the transform and fits the result back into the frame.")
                .on_click(move |value, _, cx| {
                    let value = *value;
                    update_camera_raw(&session, cx, move |raw| raw.geometry.constrain_crop = value);
                }),
        );
        column
    }

    fn calibration_controls(&mut self, raw: &CameraRawSettings, cx: &mut Context<Self>) -> Div {
        let calibration = raw.calibration;
        let process = calibration.process;
        let session = self.session.clone();
        let process_picker = menu_picker(
            "camera-raw-process",
            CameraRawProcessVersion::ALL.map(|version| (version, version.raw_value())),
            process,
            move |version, _, cx| {
                update_camera_raw(&session, cx, |raw| raw.calibration.process = version);
            },
        );
        let mut column = v_flex().items_start().gap(px(8.0)).child(process_picker);
        column = column.child(
            div()
                .text_size(px(CAPTION_SIZE))
                .text_color(cx.theme().tokens.muted_foreground)
                .child(process.summary()),
        );

        column = column.child(
            div()
                .text_size(px(SUBHEADLINE_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Shadows"),
        );
        column = column.child(self.amount_row(
            AmountRow {
                id: "camera-raw-calibration-shadow-tint",
                title: "Tint",
                label_width: LABEL_WIDTH,
                field_width: Some(FIELD_WIDTH),
                value: calibration.shadow_tint,
                range: CameraRawCalibrationSettings::TONE_RANGE,
                decimals: 0,
                reset_value: 0.0,
                help: "Adds green or magenta to the darkest tones.".into(),
                track: CameraRawSliderTrack::Plain,
                scrub: true,
                label_reset: true,
                rounding: Rounding::Whole,
                kind: AssignKind::Plain,
                clipping: None,
                masking: false,
                enabled: true,
                set: Rc::new(|value, raw| raw.calibration.shadow_tint = value),
            },
            cx,
        ));

        let primary_rows: [(
            &str,
            &str,
            f64,
            &str,
            &str,
            f64,
            &str,
            fn(f64, &mut CameraRawSettings),
            fn(f64, &mut CameraRawSettings),
        ); 3] = [
            (
                "Red Primary",
                "camera-raw-calibration-red-hue",
                calibration.red_hue,
                "Shifts how red is interpreted.",
                "camera-raw-calibration-red-saturation",
                calibration.red_saturation,
                "Strengthens or weakens the red primary.",
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.red_hue = value,
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.red_saturation = value,
            ),
            (
                "Green Primary",
                "camera-raw-calibration-green-hue",
                calibration.green_hue,
                "Shifts how green is interpreted.",
                "camera-raw-calibration-green-saturation",
                calibration.green_saturation,
                "Strengthens or weakens the green primary.",
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.green_hue = value,
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.green_saturation = value,
            ),
            (
                "Blue Primary",
                "camera-raw-calibration-blue-hue",
                calibration.blue_hue,
                "Shifts how blue is interpreted.",
                "camera-raw-calibration-blue-saturation",
                calibration.blue_saturation,
                "Strengthens or weakens the blue primary.",
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.blue_hue = value,
                |value: f64, raw: &mut CameraRawSettings| raw.calibration.blue_saturation = value,
            ),
        ];
        for (group, hue_id, hue, hue_help, sat_id, saturation, sat_help, hue_set, sat_set) in
            primary_rows
        {
            column = column.child(
                div()
                    .text_size(px(SUBHEADLINE_SIZE))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(group),
            );
            column = column.child(self.amount_row(
                AmountRow {
                    id: hue_id,
                    title: "Hue",
                    label_width: LABEL_WIDTH,
                    field_width: Some(FIELD_WIDTH),
                    value: hue,
                    range: CameraRawCalibrationSettings::TONE_RANGE,
                    decimals: 0,
                    reset_value: 0.0,
                    help: hue_help.into(),
                    track: CameraRawSliderTrack::Plain,
                    scrub: true,
                    label_reset: true,
                    rounding: Rounding::Whole,
                    kind: AssignKind::Plain,
                    clipping: None,
                    masking: false,
                    enabled: true,
                    set: Rc::new(hue_set),
                },
                cx,
            ));
            column = column.child(self.amount_row(
                AmountRow {
                    id: sat_id,
                    title: "Saturation",
                    label_width: LABEL_WIDTH,
                    field_width: Some(FIELD_WIDTH),
                    value: saturation,
                    range: CameraRawCalibrationSettings::TONE_RANGE,
                    decimals: 0,
                    reset_value: 0.0,
                    help: sat_help.into(),
                    track: CameraRawSliderTrack::Plain,
                    scrub: true,
                    label_reset: true,
                    rounding: Rounding::Whole,
                    kind: AssignKind::Plain,
                    clipping: None,
                    masking: false,
                    enabled: true,
                    set: Rc::new(sat_set),
                },
                cx,
            ));
        }
        column
    }

    fn store_points(
        session: &Entity<EditorSession>,
        channel: CameraRawPointChannel,
        points: Vec<CurvePoint>,
        cx: &mut App,
    ) {
        update_camera_raw(session, cx, |raw| match channel {
            CameraRawPointChannel::Rgb => raw.curve.rgb = points,
            CameraRawPointChannel::Red => raw.curve.red = points,
            CameraRawPointChannel::Green => raw.curve.green = points,
            CameraRawPointChannel::Blue => raw.curve.blue = points,
        });
    }
}

impl Render for CameraRawControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.column(cx)
    }
}

/// The panel flags the column reads this frame (`session.filterEdit?.showsCameraRaw…` and the
/// editors' pages).
#[derive(Clone, Copy)]
struct PanelView {
    shows: [bool; 10],
    samples_white_balance: bool,
    samples_point_color: bool,
    samples_defringe: bool,
    drawing_geometry_guide: bool,
    targets_curve: bool,
    targets_mixer: bool,
    shows_shadow_clipping: bool,
    shows_highlight_clipping: bool,
    curve_page: CameraRawCurvePage,
    point_channel: CameraRawPointChannel,
    mixer_page: CameraRawMixerPage,
    mixer_tab: CameraRawMixerTab,
    mixer_swatch: usize,
    point_index: usize,
    grade_page: CameraRawGradePage,
}

impl Default for PanelView {
    fn default() -> Self {
        Self {
            shows: [true; 10],
            samples_white_balance: false,
            samples_point_color: false,
            samples_defringe: false,
            drawing_geometry_guide: false,
            targets_curve: false,
            targets_mixer: false,
            shows_shadow_clipping: false,
            shows_highlight_clipping: false,
            curve_page: CameraRawCurvePage::default(),
            point_channel: CameraRawPointChannel::default(),
            mixer_page: CameraRawMixerPage::default(),
            mixer_tab: CameraRawMixerTab::default(),
            mixer_swatch: 0,
            point_index: 0,
            grade_page: CameraRawGradePage::default(),
        }
    }
}

impl PanelView {
    fn of(panel: &CameraRawPanel) -> Self {
        Self {
            shows: [
                panel.shows_light,
                panel.shows_color,
                panel.shows_grading,
                panel.shows_effects,
                panel.shows_curve,
                panel.shows_mixer,
                panel.shows_detail,
                panel.shows_optics,
                panel.shows_geometry,
                panel.shows_calibration,
            ],
            samples_white_balance: panel.samples_white_balance,
            samples_point_color: panel.samples_point_color,
            samples_defringe: panel.samples_defringe,
            drawing_geometry_guide: panel.drawing_geometry_guide,
            targets_curve: panel.targets_curve,
            targets_mixer: panel.targets_mixer,
            shows_shadow_clipping: panel.shows_shadow_clipping,
            shows_highlight_clipping: panel.shows_highlight_clipping,
            curve_page: panel.curve_page,
            point_channel: panel.point_channel,
            mixer_page: panel.mixer_page,
            mixer_tab: panel.mixer_tab,
            mixer_swatch: panel.mixer_swatch,
            point_index: panel.point_index,
            grade_page: panel.grade_page,
        }
    }

    /// `session.filterEdit?.showsCameraRaw…`.
    fn shows(&self, section: CameraRawSection) -> bool {
        self.shows[section_index(section)]
    }
}

/// The section's place in `CameraRawSection::ALL`, which is `Section.allCases`' order.
fn section_index(section: CameraRawSection) -> usize {
    match section {
        CameraRawSection::Light => 0,
        CameraRawSection::Color => 1,
        CameraRawSection::ColorGrading => 2,
        CameraRawSection::Effects => 3,
        CameraRawSection::Curve => 4,
        CameraRawSection::ColorMixer => 5,
        CameraRawSection::Detail => 6,
        CameraRawSection::Optics => 7,
        CameraRawSection::Geometry => 8,
        CameraRawSection::Calibration => 9,
    }
}

/// How an amount is rounded before it is assigned: `(value * step).rounded() / step`, plain
/// `value.rounded()`, or as it comes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    Exact,
    Step,
    Whole,
}

/// What a write also tells besides the settings: `CameraRawControls.assign`'s Option-held clipping
/// view, `CameraRawDetailControls.assignDetail`'s sharpen mask, or neither (the other editors'
/// `update {}`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AssignKind {
    Column,
    Detail,
    Plain,
}

/// One slider row, the values the Swift helpers passed in their parameter lists.
struct AmountRow {
    id: &'static str,
    title: &'static str,
    label_width: f32,
    field_width: Option<f32>,
    value: f64,
    range: (f64, f64),
    decimals: usize,
    reset_value: f64,
    help: SharedString,
    track: CameraRawSliderTrack,
    scrub: bool,
    label_reset: bool,
    rounding: Rounding,
    kind: AssignKind,
    clipping: Option<CameraRawClipping>,
    masking: bool,
    enabled: bool,
    set: Rc<dyn Fn(f64, &mut CameraRawSettings)>,
}

/// `CameraRawCurveControls.Drag`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CurveDrag {
    Point(usize),
    Divider(usize),
    Region(CameraRawCurveRegion, f64),
}

/// The graph's live drag and where the press began (`DragGesture.value.startLocation`).
#[derive(Clone, Copy)]
struct CurveGesture {
    drag: CurveDrag,
    start: Point<Pixels>,
}

/// `CurvePreset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CurvePreset {
    Custom,
    Linear,
    Medium,
    Strong,
}

impl CurvePreset {
    const ALL: [CurvePreset; 4] = [
        CurvePreset::Custom,
        CurvePreset::Linear,
        CurvePreset::Medium,
        CurvePreset::Strong,
    ];

    fn raw_value(self) -> &'static str {
        match self {
            CurvePreset::Custom => "Custom",
            CurvePreset::Linear => "Linear",
            CurvePreset::Medium => "Medium Contrast",
            CurvePreset::Strong => "Strong Contrast",
        }
    }

    /// `points`: Custom has no set of its own.
    fn points(self) -> Option<Vec<CurvePoint>> {
        match self {
            CurvePreset::Custom => None,
            CurvePreset::Linear => Some(CameraRawCurveSettings::linear()),
            CurvePreset::Medium => Some(CameraRawCurveSettings::medium_contrast()),
            CurvePreset::Strong => Some(CameraRawCurveSettings::strong_contrast()),
        }
    }

    /// `matching(_:)`.
    fn matching(points: &[CurvePoint]) -> Self {
        if points == CameraRawCurveSettings::linear().as_slice() {
            return CurvePreset::Linear;
        }
        if points == CameraRawCurveSettings::medium_contrast().as_slice() {
            return CurvePreset::Medium;
        }
        if points == CameraRawCurveSettings::strong_contrast().as_slice() {
            return CurvePreset::Strong;
        }
        CurvePreset::Custom
    }
}

/// Which Color Grading wheel a row edits (`CameraRawGradingControls.wheel(_:_:)`'s key path).
#[derive(Clone, Copy, PartialEq, Eq)]
enum GradeWheelKey {
    Shadows,
    Midtones,
    Highlights,
    Global,
}

impl GradeWheelKey {
    /// `pageKey`.
    fn of_page(page: CameraRawGradePage) -> Self {
        match page {
            CameraRawGradePage::ThreeWay | CameraRawGradePage::Shadows => GradeWheelKey::Shadows,
            CameraRawGradePage::Midtones => GradeWheelKey::Midtones,
            CameraRawGradePage::Highlights => GradeWheelKey::Highlights,
            CameraRawGradePage::Global => GradeWheelKey::Global,
        }
    }

    fn wheel(self, grading: &CameraRawGradingSettings) -> CameraRawGradeWheel {
        match self {
            GradeWheelKey::Shadows => grading.shadows,
            GradeWheelKey::Midtones => grading.midtones,
            GradeWheelKey::Highlights => grading.highlights,
            GradeWheelKey::Global => grading.global,
        }
    }

    fn set_hue_saturation(self, grading: &mut CameraRawGradingSettings, hue: f64, saturation: f64) {
        let wheel = match self {
            GradeWheelKey::Shadows => &mut grading.shadows,
            GradeWheelKey::Midtones => &mut grading.midtones,
            GradeWheelKey::Highlights => &mut grading.highlights,
            GradeWheelKey::Global => &mut grading.global,
        };
        wheel.hue = hue;
        wheel.saturation = saturation;
    }

    fn set_luminance(self, grading: &mut CameraRawGradingSettings, value: f64) {
        let wheel = match self {
            GradeWheelKey::Shadows => &mut grading.shadows,
            GradeWheelKey::Midtones => &mut grading.midtones,
            GradeWheelKey::Highlights => &mut grading.highlights,
            GradeWheelKey::Global => &mut grading.global,
        };
        wheel.luminance = value;
    }
}

/// The empty view a curve-graph drag carries: the drag moves a point, not a payload.
#[derive(Clone)]
struct CurveGraphDrag;

impl Render for CurveGraphDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The empty view a grading-wheel drag carries.
#[derive(Clone)]
struct GradeWheelDrag;

impl Render for GradeWheelDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

// MARK: - The panel's writes

/// `assign(_:_:clipping:)` / `assignDetail(_:_:maskingPreview:)` / `update {}`: `set` writes the
/// amount, the panel's Option-held preview views follow the flags, and the panel round-trips
/// through `updateFilter(_:preview:)`.
fn write_amount<F>(
    session: &Entity<EditorSession>,
    kind: AssignKind,
    clipping: Option<CameraRawClipping>,
    masking: bool,
    value: f64,
    set: F,
    window: &mut Window,
    cx: &mut App,
) where
    F: FnOnce(f64, &mut CameraRawSettings),
{
    // `NSEvent.modifierFlags.contains(.option)`: the window's Alt.
    let option = window.modifiers().alt;
    let clipping = if clipping.is_some() && option { clipping } else { None };
    let masking = masking && option;
    session.update(cx, |session, cx| {
        let Some(edit) = session.filter_edit.as_mut() else {
            return;
        };
        match kind {
            AssignKind::Column => edit.panel.clipping = clipping,
            AssignKind::Detail => edit.panel.sharpen_mask = masking,
            AssignKind::Plain => {}
        }
        set(value, &mut edit.camera_raw);
        let (settings, preview) = (edit.settings.clone(), edit.preview);
        session.update_filter(settings, preview);
        cx.notify();
    });
}

/// `session.filterEdit?.cameraRaw… = …` on the panel alone: no `updateFilter`, exactly as the
/// Swift panel-state writes did.
fn edit_panel<F>(session: &Entity<EditorSession>, cx: &mut App, change: F)
where
    F: FnOnce(&mut CameraRawPanel),
{
    session.update(cx, |session, cx| {
        if let Some(edit) = session.filter_edit.as_mut() {
            change(&mut edit.panel);
        }
        cx.notify();
    });
}

/// An armed eyedropper: the panel flag and `session.brushRevision += 1`, which the canvas reads.
fn arm_eyedropper<F>(session: &Entity<EditorSession>, cx: &mut App, change: F)
where
    F: FnOnce(&mut CameraRawPanel),
{
    session.update(cx, |session, cx| {
        if let Some(edit) = session.filter_edit.as_mut() {
            change(&mut edit.panel);
        }
        session.brush_revision += 1;
        cx.notify();
    });
}

/// `update { … }`: writes a Camera Raw amount and round-trips through `updateFilter(_:preview:)`.
fn update_camera_raw<F>(session: &Entity<EditorSession>, cx: &mut App, change: F)
where
    F: FnOnce(&mut CameraRawSettings),
{
    session.update(cx, |session, cx| {
        let Some(edit) = session.filter_edit.as_mut() else {
            return;
        };
        change(&mut edit.camera_raw);
        let (settings, preview) = (edit.settings.clone(), edit.preview);
        session.update_filter(settings, preview);
        cx.notify();
    });
}

/// `installOptionMonitor`'s listener: with Option up, the clipping view and the sharpen mask go
/// away and the panel re-renders.
fn option_released(session: &Entity<EditorSession>, event: &ModifiersChangedEvent, cx: &mut App) {
    if event.alt {
        return;
    }
    session.update(cx, |session, cx| {
        let clear = session
            .filter_edit
            .as_ref()
            .is_some_and(|edit| edit.panel.clipping.is_some() || edit.panel.sharpen_mask);
        if !clear {
            return;
        }
        let Some(edit) = session.filter_edit.as_mut() else {
            return;
        };
        edit.panel.clipping = None;
        edit.panel.sharpen_mask = false;
        let (settings, preview) = (edit.settings.clone(), edit.preview);
        session.update_filter(settings, preview);
        cx.notify();
    });
}

// MARK: - The scope and the curve graph's arithmetic

/// `graph(_:mode:)`: the three ribbons over the shared peak, or the vectorscope's cells.
fn paint_scope(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    scope: Option<&CameraRawScope>,
    mode: CameraRawScopeMode,
) {
    let Some(scope) = scope else {
        return;
    };
    match mode {
        CameraRawScopeMode::Histogram => {
            let peak = scope.peak();
            if peak <= 0.0 {
                return;
            }
            paint_ribbon(window, bounds, &scope.red, red(), peak);
            paint_ribbon(window, bounds, &scope.green, green(), peak);
            paint_ribbon(window, bounds, &scope.blue, blue(), peak);
        }
        CameraRawScopeMode::Vectorscope => {
            let peak = scope.vectorscope.iter().copied().fold(0.0, f64::max);
            if peak <= 0.0 {
                return;
            }
            let cell = f32::from(bounds.size.width) / CameraRawScope::SCOPE_SIDE as f32;
            let height = f32::from(bounds.size.height);
            for (index, value) in scope.vectorscope.iter().enumerate() {
                if *value <= 0.0 {
                    continue;
                }
                let (x, y, width, cell_height) = vectorscope_cell(index, cell, height);
                window.paint_quad(fill(
                    Bounds {
                        origin: point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
                        size: size(px(width), px(cell_height)),
                    },
                    white().opacity(vectorscope_amount(*value, peak)),
                ));
            }
        }
    }
}

/// `ribbon(_:color:peak:in:size:)`: the filled bins under one channel's curve.
fn paint_ribbon(window: &mut Window, bounds: Bounds<Pixels>, bins: &[f64], color: Hsla, peak: f64) {
    let points = ribbon_points(
        bins,
        peak,
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    );
    let mut builder = PathBuilder::fill();
    for (index, (x, y)) in points.iter().enumerate() {
        let at = point(bounds.origin.x + px(*x), bounds.origin.y + px(*y));
        if index == 0 {
            builder.move_to(at);
        } else {
            builder.line_to(at);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color.opacity(RIBBON_OPACITY));
    }
}

/// `ribbon`'s points in the graph's own coordinates: bottom-left, one point per bin, bottom-right.
fn ribbon_points(bins: &[f64], peak: f64, width: f32, height: f32) -> Vec<(f32, f32)> {
    let mut points = vec![(0.0, height)];
    for (index, bin) in bins.iter().enumerate() {
        let x = index as f32 * width / bins.len() as f32;
        let reached = height * (bin / peak).clamp(0.0, 1.0) as f32;
        points.push((x, height - reached));
    }
    points.push((width, height));
    points
}

/// The vectorscope cell at `index`: `(x, y, width, height)` with the `+ 0.2` bleed, the rows
/// counted up from the bottom as the Swift counted them.
fn vectorscope_cell(index: usize, cell: f32, height: f32) -> (f32, f32, f32, f32) {
    let column = index % CameraRawScope::SCOPE_SIDE;
    let row = index / CameraRawScope::SCOPE_SIDE;
    (
        column as f32 * cell,
        height - (row + 1) as f32 * cell,
        cell + VECTORSCOPE_BLEED,
        cell + VECTORSCOPE_BLEED,
    )
}

/// `0.15 + 0.85 * amount`.
fn vectorscope_amount(value: f64, peak: f64) -> f32 {
    VECTORSCOPE_MIN + VECTORSCOPE_RANGE * (value / peak).min(1.0) as f32
}

/// `.monospacedDigit()`: the system face with tabular figures.
fn monospaced_digit_font() -> Font {
    Font {
        features: FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])),
        ..font(".SystemUIFont")
    }
}

/// `CameraRawCurveControls.pointDrag`'s nearest test: the closest point and its distance.
fn nearest_curve_point(points: &[CurvePoint], x: f64, y: f64) -> Option<(usize, f64)> {
    let mut nearest: Option<(usize, f64)> = None;
    for (index, point) in points.iter().enumerate() {
        let distance = ((point.x - x).powi(2) + (point.y - y).powi(2)).sqrt();
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((index, distance));
        }
    }
    nearest
}

/// The pointer's `(x, 1 − y)` in the graph's own 0…1 grid (`start.x / max(size.width, 1)`).
fn normalized_graph_point(at: Point<Pixels>, bounds: Bounds<Pixels>) -> (f64, f64) {
    let x = f64::from(f32::from(at.x - bounds.origin.x)) / f64::from(f32::from(bounds.size.width).max(1.0));
    let y = 1.0 - f64::from(f32::from(at.y - bounds.origin.y)) / f64::from(f32::from(bounds.size.height).max(1.0));
    (x, y)
}

/// `parametricDrag(at:in:)`.
fn parametric_drag(curve: &CameraRawCurveSettings, at: Point<Pixels>, bounds: Bounds<Pixels>) -> CurveDrag {
    let (x, _) = normalized_graph_point(at, bounds);
    let tone = x * 100.0;
    let splits = [curve.shadow_split, curve.dark_split, curve.light_split];
    let from_bottom = f32::from(bounds.origin.y + bounds.size.height - at.y);
    if from_bottom < 18.0 {
        let index = splits
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (*a - tone).abs().total_cmp(&(*b - tone).abs()))
            .map(|(index, _)| index)
            .unwrap_or(0);
        return CurveDrag::Divider(index);
    }
    let region = if tone < splits[0] {
        CameraRawCurveRegion::Shadows
    } else if tone < splits[1] {
        CameraRawCurveRegion::Darks
    } else if tone < splits[2] {
        CameraRawCurveRegion::Lights
    } else {
        CameraRawCurveRegion::Highlights
    };
    CurveDrag::Region(region, curve.amount(region))
}

/// `CameraRawCurveControls.currentPoints`.
fn channel_points(curve: &CameraRawCurveSettings, channel: CameraRawPointChannel) -> Vec<CurvePoint> {
    match channel {
        CameraRawPointChannel::Rgb => curve.rgb.clone(),
        CameraRawPointChannel::Red => curve.red.clone(),
        CameraRawPointChannel::Green => curve.green.clone(),
        CameraRawPointChannel::Blue => curve.blue.clone(),
    }
}

/// `stroke(samples:in:size:)`.
fn paint_samples(window: &mut Window, bounds: Bounds<Pixels>, samples: &[f64], width: f32) {
    let graph_width = f32::from(bounds.size.width);
    let graph_height = f32::from(bounds.size.height);
    let denominator = samples.len().saturating_sub(1).max(1) as f32;
    let mut builder = PathBuilder::stroke(px(width));
    for (index, sample) in samples.iter().enumerate() {
        let x = index as f32 / denominator * graph_width;
        let y = (1.0 - *sample as f32) * graph_height;
        let at = point(bounds.origin.x + px(x), bounds.origin.y + px(y));
        if index == 0 {
            builder.move_to(at);
        } else {
            builder.line_to(at);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, white());
    }
}

/// The corner radii that round a small square into a circle.
fn rounded(radius: Pixels) -> Corners<Pixels> {
    Corners {
        top_left: radius,
        top_right: radius,
        bottom_right: radius,
        bottom_left: radius,
    }
}

/// `curveGraph`'s drawing: the axis, then the selected page.
fn paint_curve_graph(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    curve: &CameraRawCurveSettings,
    page: CameraRawCurvePage,
    points: &[CurvePoint],
    selected: Option<usize>,
    accent: Hsla,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let mut axis = PathBuilder::stroke(px(1.0));
    axis.move_to(point(bounds.origin.x, bounds.origin.y + px(height)));
    axis.line_to(point(bounds.origin.x + px(width), bounds.origin.y));
    if let Ok(path) = axis.build() {
        window.paint_path(path, white().opacity(0.25));
    }
    if page == CameraRawCurvePage::Parametric {
        let samples: Vec<f64> = (0..64).map(|index| curve.parametric(index as f64 / 63.0)).collect();
        paint_samples(window, bounds, &samples, 1.5);
        for split in [curve.shadow_split, curve.dark_split, curve.light_split] {
            let x = split as f32 / 100.0 * width;
            let mut line = PathBuilder::stroke(px(3.0));
            line.move_to(point(bounds.origin.x + px(x), bounds.origin.y + px(height - 8.0)));
            line.line_to(point(bounds.origin.x + px(x), bounds.origin.y + px(height)));
            if let Ok(path) = line.build() {
                window.paint_path(path, white());
            }
        }
    } else {
        let samples: Vec<f64> = curve.channel_table(points).into_iter().map(f64::from).collect();
        paint_samples(window, bounds, &samples, 1.5);
        for (index, point) in points.iter().enumerate() {
            let rect = Bounds {
                origin: gpui_kit::point(
                    bounds.origin.x + px(point.x as f32 * width - 4.0),
                    bounds.origin.y + px((1.0 - point.y) as f32 * height - 4.0),
                ),
                size: size(px(8.0), px(8.0)),
            };
            let color = if selected == Some(index) { accent } else { white() };
            let mut quad = fill(rect, color);
            quad.corner_radii = rounded(px(4.0));
            window.paint_quad(quad);
        }
    }
}

/// `GradeWheel`'s drawing: the hue ring, its rim, and the white dot at hue and saturation. The
/// SwiftUI angular gradient runs clockwise with its stops reversed; painted directly, the hue is
/// the angle counterclockwise from red at the right, which is what the drag measures.
fn paint_hue_wheel(window: &mut Window, bounds: Bounds<Pixels>, hue: f64, saturation: f64) {
    let side = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    let center = gpui_kit::point(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    let radius = (side / 2.0 - WHEEL_INSET).max(0.0);
    if radius <= 0.0 {
        return;
    }
    for index in 0..WHEEL_SEGMENTS {
        let start = std::f32::consts::TAU * index as f32 / WHEEL_SEGMENTS as f32;
        let end = std::f32::consts::TAU * (index + 1) as f32 / WHEEL_SEGMENTS as f32;
        let mid = (start + end) / 2.0;
        let mut builder = PathBuilder::fill();
        builder.move_to(center);
        for angle in [start, end] {
            builder.line_to(gpui_kit::point(
                center.x + px(radius * angle.cos()),
                center.y - px(radius * angle.sin()),
            ));
        }
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(path, Hsla::from(hsb(mid / std::f32::consts::TAU, 1.0, 1.0)).opacity(0.85));
        }
    }
    let mut rim = PathBuilder::stroke(px(1.0));
    let steps = 64;
    for step in 0..=steps {
        let angle = std::f32::consts::TAU * step as f32 / steps as f32;
        let at = gpui_kit::point(
            center.x + px(radius * angle.cos()),
            center.y - px(radius * angle.sin()),
        );
        if step == 0 {
            rim.move_to(at);
        } else {
            rim.line_to(at);
        }
    }
    rim.close();
    if let Ok(path) = rim.build() {
        window.paint_path(path, white().opacity(0.8));
    }
    let angle = hue as f32 * std::f32::consts::PI / 180.0;
    let distance = (saturation as f32 / 100.0) * radius;
    let dot = gpui_kit::point(center.x + px(angle.cos() * distance), center.y - px(angle.sin() * distance));
    let mut quad = fill(
        Bounds {
            origin: gpui_kit::point(dot.x - px(5.0), dot.y - px(5.0)),
            size: size(px(10.0), px(10.0)),
        },
        white(),
    );
    quad.corner_radii = rounded(px(5.0));
    window.paint_quad(quad);
}

/// The hue and saturation a press at `at` asks for (`GradeWheel`'s `onChanged`).
fn wheel_point(at: Point<Pixels>, bounds: Bounds<Pixels>) -> (f64, f64) {
    let side = f64::from(f32::from(bounds.size.width).min(f32::from(bounds.size.height)));
    let radius = side / 2.0 - f64::from(WHEEL_INSET);
    let center = (
        f64::from(f32::from(bounds.origin.x + bounds.size.width / 2.0)),
        f64::from(f32::from(bounds.origin.y + bounds.size.height / 2.0)),
    );
    let dx = f64::from(f32::from(at.x)) - center.0;
    let dy = center.1 - f64::from(f32::from(at.y));
    let mut degrees = dy.atan2(dx) * 180.0 / std::f64::consts::PI;
    if degrees < 0.0 {
        degrees += 360.0;
    }
    let saturation = if radius > 0.0 {
        (dx.hypot(dy) / radius * 100.0).min(100.0)
    } else {
        0.0
    };
    (degrees, saturation)
}

/// `CameraRawMixerControls.familyTrack(_:_:)`.
fn family_track(index: usize, tab: CameraRawMixerTab) -> CameraRawSliderTrack {
    let hue = CameraRawMixerSettings::centers()[index];
    match tab {
        CameraRawMixerTab::Hue => CameraRawSliderTrack::Hue(hue),
        CameraRawMixerTab::Saturation => CameraRawSliderTrack::Saturation(hue),
        CameraRawMixerTab::Luminance => CameraRawSliderTrack::Luminance(hue),
    }
}

/// One family's amount (`raw.mixer[keyPath: key][index]`).
fn mixer_amount(raw: &CameraRawSettings, tab: CameraRawMixerTab, index: usize) -> f64 {
    match tab {
        CameraRawMixerTab::Hue => raw.mixer.hue[index],
        CameraRawMixerTab::Saturation => raw.mixer.saturation[index],
        CameraRawMixerTab::Luminance => raw.mixer.luminance[index],
    }
}

fn set_mixer_amount(raw: &mut CameraRawSettings, tab: CameraRawMixerTab, index: usize, value: f64) {
    match tab {
        CameraRawMixerTab::Hue => raw.mixer.hue[index] = value,
        CameraRawMixerTab::Saturation => raw.mixer.saturation[index] = value,
        CameraRawMixerTab::Luminance => raw.mixer.luminance[index] = value,
    }
}
