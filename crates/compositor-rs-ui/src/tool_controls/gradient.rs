//! The Gradient tool's option bar (port of `UI/GradientControls.swift`): the shape, a ramp swatch,
//! the colors pair, Reverse, and the opacity, with Cancel / Apply while a gradient is being dragged.

use compositor_rs_core::image_ops::{GradientShape, GradientStyle};
use compositor_rs_session::EditorSession;

use crate::tool_controls::{segmented_picker, unit_suffix, FieldSpec, Fields};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The opacity's range (`0.01...1`), and the percent its field holds (`1...100`).
pub const OPACITY_RANGE: (f64, f64) = (0.01, 1.0);
pub const OPACITY_PERCENT_RANGE: (f64, f64) = (1.0, 100.0);
/// `.frame(width: 100)` / `.frame(width: 42)`.
pub const SLIDER_WIDTH: f32 = 100.0;
pub const FIELD_WIDTH: f32 = 42.0;
/// The ramp swatch's box (`.frame(width: 56, height: 18)`).
pub const SWATCH_WIDTH: f32 = 56.0;
pub const SWATCH_HEIGHT: f32 = 18.0;

/// The side of the rail's own icons (`Canvas` fills the frame it is given).
pub const ICON_SIZE: f32 = 16.0;
/// The dithering pattern's side: one cell per point inside the icon's frame.
const PATTERN_SIZE: usize = 16;
/// The dithered fade's frame radius (`cornerRadius: 3.5`).
const ICON_RADIUS: f32 = 3.5;
/// The frame's outline width (`lineWidth: 1.4`).
const ICON_STROKE: f32 = 1.4;

/// The dithering `GradientToolIcon.pattern` builds: a left-to-right ramp turned on and off at half,
/// with the error Floyd–Steinberg diffuses into the neighbours still to come (7/16 right, 3/16
/// below-left, 5/16 below, 1/16 below-right).
fn gradient_pattern() -> [[bool; PATTERN_SIZE]; PATTERN_SIZE] {
    let size = PATTERN_SIZE;
    let mut ramp = [[0.0_f32; PATTERN_SIZE]; PATTERN_SIZE];
    for row in ramp.iter_mut() {
        for (x, value) in row.iter_mut().enumerate() {
            *value = x as f32 / (size - 1) as f32;
        }
    }
    let mut result = [[false; PATTERN_SIZE]; PATTERN_SIZE];
    for y in 0..size {
        for x in 0..size {
            let on = ramp[y][x] >= 0.5;
            result[y][x] = on;
            let error = ramp[y][x] - if on { 1.0 } else { 0.0 };
            if x + 1 < size {
                ramp[y][x + 1] += error * 7.0 / 16.0;
            }
            if y + 1 < size {
                if x > 0 {
                    ramp[y + 1][x - 1] += error * 3.0 / 16.0;
                }
                ramp[y + 1][x] += error * 5.0 / 16.0;
                if x + 1 < size {
                    ramp[y + 1][x + 1] += error / 16.0;
                }
            }
        }
    }
    result
}

/// The Gradient tool's rail icon (`GradientToolIcon`): a Floyd–Steinberg dithered fade from empty
/// to solid, so it reads as a gradient in the same monochrome style as the SF Symbols beside it.
pub struct GradientToolIcon;

impl GradientToolIcon {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GradientToolIcon {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for GradientToolIcon {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for GradientToolIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _, window, _| {
                let unit = f32::from(bounds.size.width) / ICON_SIZE;
                // `CGRect(origin: .zero, size: size).insetBy(dx: 1, dy: 1)`, in the icon's points.
                let frame = Bounds {
                    origin: point(bounds.left() + px(unit), bounds.top() + px(unit)),
                    size: size(px(ICON_SIZE * unit - 2.0 * unit), px(ICON_SIZE * unit - 2.0 * unit)),
                };
                // `.foreground`: the rail button's own text color, as the SF Symbols beside it use.
                let color = window.text_style().color;
                let cell = f32::from(frame.size.width) / PATTERN_SIZE as f32;
                let radius = ICON_RADIUS * unit;

                // `context.clip(to: shape)`: the frame's rounded corners cut the dots off, so a dot
                // whose center falls outside the corner arcs is left out.
                let frame_left = f32::from(frame.left());
                let frame_top = f32::from(frame.top());
                let frame_right = frame_left + f32::from(frame.size.width);
                let frame_bottom = frame_top + f32::from(frame.size.height);
                let inside = |left: f32, top: f32| {
                    let center_x = left + cell / 2.0;
                    let center_y = top + cell / 2.0;
                    let nearest_x = center_x.clamp(frame_left + radius, frame_right - radius);
                    let nearest_y = center_y.clamp(frame_top + radius, frame_bottom - radius);
                    let dx = center_x - nearest_x;
                    let dy = center_y - nearest_y;
                    dx * dx + dy * dy <= radius * radius
                };

                for (row, line) in gradient_pattern().iter().enumerate() {
                    for (column, on) in line.iter().enumerate() {
                        if !*on {
                            continue;
                        }
                        let left = frame_left + column as f32 * cell;
                        let top = frame_top + row as f32 * cell;
                        if !inside(left, top) {
                            continue;
                        }
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(px(left), px(top)),
                                size: size(px(cell), px(cell)),
                            },
                            color,
                        ));
                    }
                }

                let mut outline = PathBuilder::stroke(px(ICON_STROKE * unit));
                let bend = radius * 0.5523;
                let left = f32::from(frame.left());
                let top = f32::from(frame.top());
                let right = left + f32::from(frame.size.width);
                let bottom = top + f32::from(frame.size.height);
                outline.move_to(point(px(left + radius), px(top)));
                outline.line_to(point(px(right - radius), px(top)));
                outline.cubic_bezier_to(
                    point(px(right), px(top + radius)),
                    point(px(right - radius + bend), px(top)),
                    point(px(right), px(top + radius - bend)),
                );
                outline.line_to(point(px(right), px(bottom - radius)));
                outline.cubic_bezier_to(
                    point(px(right - radius), px(bottom)),
                    point(px(right), px(bottom - radius + bend)),
                    point(px(right - radius + bend), px(bottom)),
                );
                outline.line_to(point(px(left + radius), px(bottom)));
                outline.cubic_bezier_to(
                    point(px(left), px(bottom - radius)),
                    point(px(left + radius - bend), px(bottom)),
                    point(px(left), px(bottom - radius + bend)),
                );
                outline.line_to(point(px(left), px(top + radius)));
                outline.cubic_bezier_to(
                    point(px(left + radius), px(top)),
                    point(px(left), px(top + radius - bend)),
                    point(px(left + radius - bend), px(top)),
                );
                outline.close();
                if let Ok(path) = outline.build() {
                    window.paint_path(path, color);
                }
            },
        )
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
    }
}

/// The Gradient bar.
pub struct GradientControls {
    session: Entity<EditorSession>,
    fields: Fields,
}

impl GradientControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
        }
    }
}

impl Render for GradientControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (settings, colors, is_mask, has_edit, enabled) = {
            let session = self.session.read(cx);
            (
                session.gradient_settings,
                session
                    .gradient_colors(false)
                    .into_iter()
                    .map(|[red, green, blue, alpha]| {
                        Hsla::from(Rgba {
                            r: red as f32,
                            g: green as f32,
                            b: blue as f32,
                            a: alpha as f32,
                        })
                    })
                    .collect::<Vec<_>>(),
                session.is_mask_selected,
                session.gradient_edit.is_some(),
                session.document.is_some() && !session.shows_busy,
            )
        };
        let opacity = settings.opacity;

        let shape_picker = {
            let session = self.session.clone();
            div()
                .id("gradient-shape")
                .tooltip(|window, cx| {
                    Tooltip::new("Linear runs along the line; Radial spreads out from the start point")
                        .build(window, cx)
                })
                .child(
                    segmented_picker(
                        "gradient-shape-picker",
                        GradientShape::ALL.map(|shape| (shape, shape.raw_value())),
                        settings.shape,
                        move |shape, _, cx| {
                            session.update(cx, |session, _| session.gradient_settings.shape = shape);
                        },
                    )
                    .disabled(!enabled),
                )
        };

        let swatch = gradient_swatch(colors);

        let style_picker = {
            let session = self.session.clone();
            crate::tool_controls::menu_picker(
                "gradient-colors",
                GradientStyle::ALL.map(|style| (style, style.raw_value())),
                settings.style,
                move |style, _, cx| {
                    session.update(cx, |session, _| session.gradient_settings.style = style);
                },
            )
            .disabled(!enabled)
        };

        let reverse = {
            let session = self.session.clone();
            Checkbox::new("gradient-reverse")
                .label("Reverse")
                .checked(settings.reversed)
                .disabled(!enabled)
                .on_click(move |checked, _, cx| {
                    let checked = *checked;
                    session.update(cx, |session, _| session.gradient_settings.reversed = checked);
                })
        };

        let opacity_scrub = {
            let session = self.session.clone();
            NumericScrub::new(opacity, 0.01, OPACITY_RANGE).on_change(move |value, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| session.gradient_settings.opacity = value);
            })
        };

        let opacity_slider = {
            let session = self.session.clone();
            let slider = CameraRawSlider::plain(
                "gradient-opacity-slider",
                opacity,
                OPACITY_RANGE,
                "Gradient opacity",
            )
            .step(0.01)
            .on_change(move |value, _, cx| {
                let session = session.clone();
                session.update(cx, |session, _| session.gradient_settings.opacity = value);
            });
            div()
                .w(px(SLIDER_WIDTH))
                .when(!enabled, |this| this.opacity(0.5))
                .child(slider)
        };

        let opacity_field = {
            let field = self.fields.get("gradient-opacity", cx);
            let write = {
                let session = self.session.clone();
                move |percent: f64, _: &mut Window, cx: &mut App| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.gradient_settings.opacity = percent / 100.0;
                    });
                }
            };
            unit_suffix(
                field.element(
                    "gradient-opacity-field",
                    opacity * 100.0,
                    FieldSpec::new(OPACITY_PERCENT_RANGE, 0)
                        .fallback(100.0)
                        // The arrow steps come off the percent value, as `arrowSteps(value:)` read it.
                        .step(1.0)
                        .disabled(!enabled),
                    FIELD_WIDTH,
                    write,
                    cx,
                ),
                div().child("%"),
            )
            .id("gradient-opacity-row")
            .tooltip(|window, cx| {
                Tooltip::new("Press 1–9 for 10–90%, 0 for 100%").build(window, cx)
            })
        };

        let actions = has_edit.then(|| {
            let cancel = {
                let session = self.session.clone();
                Button::new("gradient-cancel")
                    .label("Cancel")
                    .on_click(move |_, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.cancel_gradient());
                    })
            };
            let apply = {
                let session = self.session.clone();
                Button::new("gradient-apply")
                    .label("Apply")
                    .on_click(move |_, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| session.commit_gradient());
                    })
            };
            div().flex().items_center().gap(px(12.0)).child(cancel).child(apply)
        });

        tool_header_bar(12.0)
            .child(tool_header_title("Gradient"))
            .child(shape_picker)
            .child(swatch)
            .child(style_picker)
            .child(reverse)
            .child(div().child("Opacity").scrubbable("gradient-opacity-label", opacity_scrub))
            .child(opacity_slider)
            .child(opacity_field)
            .child(tool_header_spacer())
            .when(is_mask, |this| {
                this.child(
                    div()
                        .text_color(cx.theme().tokens.muted_foreground)
                        .child("Mask"),
                )
            })
            .children(actions)
    }
}

/// The current ramp over a checkerboard, so transparency reads as transparency: 4-point tiles, the
/// even ones a 45% gray on white, with the ramp drawn over them and a black 50% border.
fn gradient_swatch(colors: Vec<Hsla>) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(SWATCH_WIDTH))
        .h(px(SWATCH_HEIGHT))
        .rounded(px(3.0))
        .border_1()
        .border_color(hsla(0.0, 0.0, 0.0, 0.5))
        .overflow_hidden()
        .child(
            canvas(
                |bounds, _, _| bounds,
                move |bounds, _, window, _| {
                    const TILE: f32 = 4.0;
                    window.paint_quad(fill(bounds, hsla(0.0, 0.0, 1.0, 1.0)));
                    let columns = (f32::from(bounds.size.width) / TILE).ceil() as i32;
                    let rows = (f32::from(bounds.size.height) / TILE).ceil() as i32;
                    for row in 0..rows {
                        for column in 0..columns {
                            if (row + column) % 2 != 0 {
                                continue;
                            }
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(
                                        bounds.left() + px(column as f32 * TILE),
                                        bounds.top() + px(row as f32 * TILE),
                                    ),
                                    size: size(px(TILE), px(TILE)),
                                },
                                hsla(0.0, 0.0, 0.5, 0.45),
                            ));
                        }
                    }
                    if let [first, second, ..] = colors.as_slice() {
                        window.paint_quad(fill(
                            bounds,
                            linear_gradient(
                                90.0,
                                linear_color_stop(*first, 0.0),
                                linear_color_stop(*second, 1.0),
                            ),
                        ));
                    }
                },
            )
            .absolute()
            .size_full(),
        )
}
