//! Camera Raw's sliders and their gradient tracks (port of `UI/CameraRawSlider.swift`).
//!
//! A `CameraRawSlider` is a slider whose track can show a color ramp — temperature's cool blue to
//! warm yellow, a color family's hues around its center, the whole hue circle — instead of the
//! plain bar. Where a plain track uses the standard look, the ramp replaces it, so the whole track
//! shows the color and not only the side before the knob (`GradientSliderCell.drawBar`).
//!
//! The behaviour is `CameraRawSliderView`'s:
//!
//! * a press on the knob drags the value from where the knob is, continuously;
//! * a press on the track publishes the value it asks for at once and glides the knob there over
//!   0.18 seconds with an ease-out curve, without tracking the pointer (`animateTrackClick(to:)`);
//! * a double-click on the knob restores the default (`isOnKnob` and the coordinator's `reset`).
//!
//! gpui-component's `Slider` cannot carry a gradient track, a glide, or a knob-only reset, so the
//! track is painted here; the bar and knob take their colors from the same theme tokens the
//! component's slider uses.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::tooltip::Tooltip;
use std::time::Instant;
use gpui_kit::*;


use super::numeric_scrub::arrow_step_amount;
use super::slider_snap::{SliderGeometry, clamped};

/// How long a click on the track takes to glide the knob to it (`animateTrackClick`: 0.18).
pub const GLIDE_DURATION: Duration = Duration::from_millis(180);

/// The height of a track bar (`GradientSliderCell.drawBar`).
pub const TRACK_HEIGHT: f32 = 4.0;

/// The knob's size, the same 16 points gpui-component's sliders draw.
pub const KNOB_SIZE: f32 = 16.0;

/// How far past the knob's edge a press still counts as being on it (`isOnKnob`'s −2 inset).
pub const KNOB_REACH: f32 = 2.0;

/// The height of the slider (`CameraRawSlider.sizeThatFits`).
pub const SLIDER_HEIGHT: f32 = 22.0;

/// The colored tracks used by Camera Raw's color sliders. Plain sliders keep the system track.
#[derive(Clone, Debug, PartialEq)]
pub enum CameraRawSliderTrack {
    /// The system track, as a plain slider draws it.
    Plain,
    /// Cool blue to warm yellow.
    Temperature,
    /// Green to mauve.
    Tint,
    /// Gray to red.
    Chroma,
    /// Neighboring hues around a color-family center, in degrees.
    Hue(f64),
    /// Gray to that family's own color.
    Saturation(f64),
    /// Dark to light in that family's hue.
    Luminance(f64),
    /// One color to its opposite, as Color Balance's Cyan / Red.
    Opposing(Rgba, Rgba),
    /// The whole hue circle, with that hue in the middle.
    Spectrum(f64),
}

impl CameraRawSliderTrack {
    /// Left-to-right track colors. `None` keeps the system track.
    pub fn colors(&self) -> Option<Vec<Rgba>> {
        Some(match self {
            Self::Plain => return None,
            Self::Temperature => vec![
                srgb(0.22, 0.46, 0.95),
                srgb(0.98, 0.82, 0.18),
            ],
            Self::Tint => vec![srgb(0.28, 0.70, 0.34), srgb(0.70, 0.40, 0.64)],
            Self::Chroma => vec![srgb(0.62, 0.62, 0.64), srgb(0.86, 0.18, 0.20)],
            Self::Hue(degrees) => vec![
                Self::color(*degrees - 50.0, 0.85, 0.9),
                Self::color(*degrees + 50.0, 0.85, 0.9),
            ],
            Self::Saturation(degrees) => vec![
                srgb(0.55, 0.55, 0.56),
                Self::color(*degrees, 0.9, 0.9),
            ],
            Self::Luminance(degrees) => vec![
                Self::color(*degrees, 0.55, 0.18),
                Self::color(*degrees, 0.35, 0.95),
            ],
            Self::Opposing(from, to) => vec![*from, *to],
            // `stride(from: -180, through: 180, by: 30)`.
            Self::Spectrum(degrees) => (-180..=180)
                .step_by(30)
                .map(|offset| Self::color(*degrees + offset as f64, 0.85, 0.9))
                .collect(),
        })
    }

    /// A color at `degrees` around the hue circle, in `NSColor(hue:saturation:brightness:)`'s
    /// terms.
    fn color(degrees: f64, saturation: f32, brightness: f32) -> Rgba {
        hsb((degrees / 360.0) as f32, saturation, brightness)
    }
}

/// A straight sRGB color, the space the Swift `NSColor(srgbRed:green:blue:alpha:)` colors live in.
pub fn srgb(red: f32, green: f32, blue: f32) -> Rgba {
    Rgba {
        r: red,
        g: green,
        b: blue,
        a: 1.0,
    }
}

/// `NSColor(hue:saturation:brightness:alpha:)` in sRGB: `turns` is the hue as a fraction of the
/// circle, wrapping as the system color did.
pub fn hsb(turns: f32, saturation: f32, brightness: f32) -> Rgba {
    let turns = turns - turns.floor();
    let sector = turns * 6.0;
    let index = sector.floor();
    let fraction = sector - index;
    let p = brightness * (1.0 - saturation);
    let q = brightness * (1.0 - fraction * saturation);
    let t = brightness * (1.0 - (1.0 - fraction) * saturation);
    let (r, g, b) = match (index as i32).rem_euclid(6) {
        0 => (brightness, t, p),
        1 => (q, brightness, p),
        2 => (p, brightness, t),
        3 => (p, q, brightness),
        4 => (t, p, brightness),
        _ => (brightness, p, q),
    };
    Rgba { r, g, b, a: 1.0 }
}

/// `CAMediaTimingFunction(name: .easeOut)`, which is cubic-bézier(0, 0, 0.58, 1).
fn ease_out(delta: f32) -> f32 {
    cubic_bezier(0.0, 0.0, 0.58, 1.0, delta)
}

/// The y of the cubic bézier from (0,0) to (1,1) with the given control points, at `x`.
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    // Bisection only converges toward the ends, but the curve's endpoints are exact: at x = 0
    // the run has not started, at x = 1 it is done, as CAMediaTimingFunction reports them.
    if x == 0.0 {
        return 0.0;
    }
    if x == 1.0 {
        return 1.0;
    }
    let coordinate = |a: f32, b: f32, t: f32| {
        let rest = 1.0 - t;
        3.0 * rest * rest * t * a + 3.0 * rest * t * t * b + t * t * t
    };
    // x(t) climbs from 0 to 1, so bisection lands on the t the run has reached.
    let (mut low, mut high) = (0.0_f32, 1.0_f32);
    let mut t = x;
    for _ in 0..24 {
        t = (low + high) / 2.0;
        if coordinate(x1, x2, t) < x {
            low = t;
        } else {
            high = t;
        }
    }
    coordinate(y1, y2, t)
}

/// One click on the track's glide (`animateTrackClick(to:)`): where the knob was, where it is
/// going, and when it set off, so its position can be sampled at any moment.
#[derive(Clone, Copy)]
struct Glide {
    from: f32,
    to: f32,
    started: Instant,
}

impl Glide {
    /// The fraction the knob is drawn at, eased out over the glide's life.
    fn fraction(&self, now: Instant) -> f32 {
        let delta = self.progress(now);
        self.from + (self.to - self.from) * ease_out(delta)
    }

    /// How much of the glide has run, from 0 to 1.
    fn progress(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        (elapsed / GLIDE_DURATION.as_secs_f32()).clamp(0.0, 1.0)
    }

    fn finished(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

/// What one slider remembers between frames (`CameraRawSliderView`'s own fields).
struct SliderInteraction {
    /// The bar's bounds, recorded at prepaint, so a press can be turned into a value.
    bar: Option<Bounds<Pixels>>,
    /// The fraction under the pointer while the knob is dragged (`isTrackingValue`).
    tracking: Option<f32>,
    /// The glide a click on the track started.
    glide: Option<Glide>,
    /// The fraction the knob is drawn at, so a press can tell the knob from the track
    /// (`isOnKnob` read the drawn knob's frame).
    displayed: f32,
    /// The keyboard focus handle.
    focus: FocusHandle,
}

impl SliderInteraction {
    fn new(cx: &mut App) -> Self {
        Self {
            bar: None,
            tracking: None,
            glide: None,
            displayed: 0.0,
            focus: cx.focus_handle(),
        }
    }

    /// The fraction the knob is drawn at: the pointer's while it is dragged, the glide's while a
    /// track click is running, otherwise the value's.
    fn shown_fraction(&self, value_fraction: f32, now: Instant) -> f32 {
        if let Some(tracking) = self.tracking {
            tracking
        } else if let Some(glide) = self.glide.filter(|glide| !glide.finished(now)) {
            glide.fraction(now)
        } else {
            value_fraction
        }
    }

    /// The bar's geometry, which maps presses to values.
    fn geometry(&self) -> Option<SliderGeometry> {
        let bar = self.bar?;
        Some(SliderGeometry::new(
            f32::from(bar.left()),
            f32::from(bar.size.width),
            KNOB_SIZE,
        ))
    }
}

/// Where a value sits along the bar, `0`…`1`.
fn fraction_of(value: f64, range: (f64, f64)) -> f32 {
    if range.1 <= range.0 {
        return 0.0;
    }
    ((value - range.0) / (range.1 - range.0)).clamp(0.0, 1.0) as f32
}

/// What a value at a fraction is.
fn value_of(fraction: f32, range: (f64, f64)) -> f64 {
    range.0 + f64::from(fraction) * (range.1 - range.0)
}

/// The empty view a slider drag carries: dragging the knob moves a value, so the drag shows no
/// ghost under the pointer.
struct SliderDrag;

impl Render for SliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A Camera Raw slider: a gradient track, a knob that glides to a click on the track, and a
/// double-click on the knob that restores the default.
pub struct CameraRawSlider {
    id: ElementId,
    value: f64,
    range: (f64, f64),
    track: CameraRawSliderTrack,
    help: SharedString,
    step: f64,
    on_change: Option<Rc<dyn Fn(f64, &mut Window, &mut App)>>,
    on_reset: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
}

impl CameraRawSlider {
    /// A slider over `range` whose track shows `track`'s colors, with `help` as its tooltip and
    /// accessibility label. `id` names the slider; give the element a width — the Camera Raw rows
    /// give it `flex_1()`.
    pub fn new(
        id: impl Into<ElementId>,
        value: f64,
        range: (f64, f64),
        track: CameraRawSliderTrack,
        help: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            value,
            range,
            track,
            help: help.into(),
            step: 1.0,
            on_change: None,
            on_reset: None,
        }
    }

    /// A slider with the system's plain track.
    pub fn plain(
        id: impl Into<ElementId>,
        value: f64,
        range: (f64, f64),
        help: impl Into<SharedString>,
    ) -> Self {
        Self::new(id, value, range, CameraRawSliderTrack::Plain, help)
    }

    /// How far an arrow key moves the value: one `step`, ten with Shift, a tenth with Control or
    /// Alt (`step` is 1 by default).
    pub fn step(mut self, step: f64) -> Self {
        self.step = step;
        self
    }

    /// Called with the value as it changes: continuously while the knob is dragged, and once when
    /// the track is clicked. The value is the slider's own — the Camera Raw panel rounds it, as
    /// `(rawValue * step).rounded() / step`.
    pub fn on_change(mut self, listener: impl Fn(f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(listener));
        self
    }

    /// Called when the knob is double-clicked.
    pub fn on_reset(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_reset = Some(Rc::new(listener));
        self
    }
}

impl IntoElement for CameraRawSlider {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for CameraRawSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            value,
            range,
            track,
            help,
            step,
            on_change,
            on_reset,
        } = self;

        let interaction = window.use_keyed_state((id.clone(), "camera-raw-slider"), cx, |_, cx| {
            SliderInteraction::new(cx)
        });

        let now = cx.background_executor().now();
        let value_fraction = fraction_of(value, range);
        let gliding = interaction
            .read(cx)
            .glide
            .is_some_and(|glide| !glide.finished(now));
        let shown = interaction.read(cx).shown_fraction(value_fraction, now);
        interaction.update(cx, |interaction, _| {
            if !gliding {
                interaction.glide = None;
            }
            interaction.displayed = shown;
        });
        if gliding {
            window.request_animation_frame();
        }

        let colors: Option<Vec<Hsla>> = track
            .colors()
            .map(|colors| colors.into_iter().map(Hsla::from).collect());
        let bar_color = cx.theme().tokens.slider_bar;
        let thumb_color = cx.theme().tokens.slider_thumb;

        let bar = canvas(
            {
                let interaction = interaction.clone();
                move |bounds, _, cx| {
                    let bar = bar_bounds(bounds);
                    interaction.update(cx, |interaction, _| interaction.bar = Some(bar));
                    bar
                }
            },
            {
                let colors = colors.clone();
                move |bar, _, window, _| match &colors {
                    Some(colors) => paint_gradient(window, bar, colors),
                    None => paint_plain_track(window, bar, shown, bar_color.into()),
                }
            },
        )
        .absolute()
        .size_full();

        let press = {
            let interaction = interaction.clone();
            let on_change = on_change.clone();
            let on_reset = on_reset.clone();
            move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                let Some(geometry) = interaction.read(cx).geometry() else {
                    return;
                };
                let point_x = f32::from(event.position.x);
                let displayed = interaction.read(cx).displayed;
                let on_knob = geometry.is_on_knob(point_x, displayed, KNOB_REACH);
                // A double-click on the knob restores the default; on the track it is just
                // another click, which glides the knob there (the Swift view did the same).
                if event.click_count >= 2 && on_knob {
                    if let Some(on_reset) = &on_reset {
                        on_reset(window, cx);
                    }
                    return;
                }
                if on_knob {
                    // Dragging carries on from where the knob is.
                    interaction.update(cx, |interaction, _| {
                        interaction.glide = None;
                        interaction.tracking = Some(displayed);
                    });
                    return;
                }
                let Some(target) = geometry.fraction(point_x, false) else {
                    return;
                };
                if let Some(on_change) = &on_change {
                    on_change(geometry.value(point_x, range.0, range.1, false).unwrap_or(value), window, cx);
                }
                let started = cx.background_executor().now();
                let glide = (!cx.reduce_motion()).then_some(Glide {
                    from: displayed,
                    to: target,
                    started,
                });
                interaction.update(cx, |interaction, _| {
                    interaction.tracking = None;
                    interaction.glide = glide;
                });
                // Event dispatch runs with no draw phase, so `request_animation_frame`
                // (which needs the current view) would assert here. Notifying the keyed
                // state fires its observation, redraws the view, and the render then
                // requests the animation frames while the glide runs.
                cx.notify(interaction.entity_id());
            }
        };

        let drag = {
            let interaction = interaction.clone();
            let on_change = on_change.clone();
            move |event: &DragMoveEvent<SliderDrag>, window: &mut Window, cx: &mut App| {
                if interaction.read(cx).tracking.is_none() {
                    return;
                }
                let Some(geometry) = interaction.read(cx).geometry() else {
                    return;
                };
                let Some(fraction) = geometry.fraction(f32::from(event.event.position.x), false)
                else {
                    return;
                };
                interaction.update(cx, |interaction, _| interaction.tracking = Some(fraction));
                if let Some(on_change) = &on_change {
                    on_change(value_of(fraction, range), window, cx);
                }
            }
        };

        let release = |interaction: Entity<SliderInteraction>| {
            move |_: &MouseUpEvent, _window: &mut Window, cx: &mut App| {
                interaction.update(cx, |interaction, _| interaction.tracking = None);
            }
        };

        let keys = {
            let interaction = interaction.clone();
            let on_change = on_change.clone();
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                let up = match event.keystroke.key.as_str() {
                    "up" | "right" => true,
                    "down" | "left" => false,
                    _ => return,
                };
                let amount = arrow_step_amount(step, event.keystroke.modifiers);
                let next = clamped(
                    value + if up { amount } else { -amount },
                    range.0,
                    range.1,
                );
                interaction.update(cx, |interaction, _| interaction.glide = None);
                if let Some(on_change) = &on_change {
                    on_change(next, window, cx);
                }
            }
        };

        let focus = interaction.read(cx).focus.clone();
        let knob = div()
            .absolute()
            .top(px((SLIDER_HEIGHT - KNOB_SIZE) / 2.0))
            // The knob's center travels over the bar less its own width, as in NSSlider:
            // `left` places it by the fraction, and the margin pulls it back so the center lands
            // there.
            .left(relative(shown))
            .ml(px(-KNOB_SIZE * shown))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(KNOB_SIZE))
            .p(px(1.0))
            .rounded_full()
            .bg(bar_color.opacity(0.5))
            .child(
                div()
                    .flex_none()
                    .size_full()
                    .rounded_full()
                    .bg(thumb_color),
            );

        div()
            .id(id)
            .test_support()
            .relative()
            .h(px(SLIDER_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .track_focus(&focus)
            .tab_index(0)
            .role(Role::Slider)
            .aria_label(help.clone())
            .aria_numeric_value(value)
            .aria_min_numeric_value(range.0)
            .aria_max_numeric_value(range.1)
            .aria_numeric_value_step(step)
            .tooltip({
                let help = help.clone();
                move |window, cx| Tooltip::new(help.clone()).build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, press)
            .on_drag(SliderDrag, |_, _, _, cx| cx.new(|_| SliderDrag))
            .on_drag_move::<SliderDrag>(drag)
            .on_mouse_up(MouseButton::Left, release(interaction.clone()))
            .on_mouse_up_out(MouseButton::Left, release(interaction))
            .on_key_down(keys)
            .child(bar)
            .child(knob)
    }
}

/// The bar's rectangle inside the slider's bounds: a `TRACK_HEIGHT`-tall bar, vertically centered.
fn bar_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let height = px(TRACK_HEIGHT);
    let inset = (bounds.size.height - height) / 2.0;
    Bounds {
        origin: point(bounds.origin.x, bounds.origin.y + inset),
        size: size(bounds.size.width, height),
    }
}

/// Paints a track's left-to-right colors, one segment per neighboring pair: gpui's gradient
/// backgrounds carry two stops, and Camera Raw's spectrum track has thirteen.
fn paint_gradient(window: &mut Window, bar: Bounds<Pixels>, colors: &[Hsla]) {
    let segments = colors.len().saturating_sub(1);
    if segments == 0 {
        if let Some(color) = colors.first() {
            let mut quad = fill(bar, *color);
            quad.corner_radii = pill(px(TRACK_HEIGHT / 2.0));
            window.paint_quad(quad);
        }
        return;
    }
    let radius = px(TRACK_HEIGHT / 2.0);
    for index in 0..segments {
        let start = bar.size.width * (index as f32 / segments as f32);
        let end = bar.size.width * ((index + 1) as f32 / segments as f32);
        let segment = Bounds {
            origin: point(bar.left() + start, bar.top()),
            size: size(end - start, bar.size.height),
        };
        let mut quad = fill(
            segment,
            linear_gradient(
                90.0,
                linear_color_stop(colors[index], 0.0),
                linear_color_stop(colors[index + 1], 1.0),
            ),
        );
        // The rounded ends belong to the bar, not to the segments: only the outer edges round.
        quad.corner_radii = Corners {
            top_left: if index == 0 { radius } else { px(0.0) },
            bottom_left: if index == 0 { radius } else { px(0.0) },
            top_right: if index == segments - 1 {
                radius
            } else {
                px(0.0)
            },
            bottom_right: if index == segments - 1 {
                radius
            } else {
                px(0.0)
            },
        };
        window.paint_quad(quad);
    }
}

/// Paints the system track a plain slider draws: the bar, with the stretch before the knob filled,
/// as `NSSliderCell` and gpui-component's slider both show it.
fn paint_plain_track(window: &mut Window, bar: Bounds<Pixels>, fraction: f32, slider_bar: Hsla) {
    let radius = px(TRACK_HEIGHT / 2.0);
    let mut track = fill(bar, slider_bar.opacity(0.2));
    track.corner_radii = pill(radius);
    window.paint_quad(track);

    let filled_width = bar.size.width * fraction;
    if filled_width > px(0.0) {
        let filled = Bounds {
            origin: bar.origin,
            size: size(filled_width, bar.size.height),
        };
        let mut quad = fill(filled, slider_bar);
        quad.corner_radii = pill(radius);
        window.paint_quad(quad);
    }
}

/// The radii that round a short bar into a pill.
fn pill(radius: Pixels) -> Corners<Pixels> {
    Corners {
        top_left: radius,
        top_right: radius,
        bottom_right: radius,
        bottom_left: radius,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::{
        CameraRawSlider, CameraRawSliderTrack, GLIDE_DURATION, KNOB_REACH, KNOB_SIZE, ease_out, hsb,
        srgb,
    };
    use crate::widgets::slider_snap::SliderGeometry;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
        Render, Styled as _, TestAppContext, TestSupportExt as _, Window, WindowBounds,
        WindowOptions, div, point, px, size,
    };

    #[test]
    fn color_tracks_run_from_the_cool_or_muted_end_to_the_warm_or_strong_end() {
        let temperature = CameraRawSliderTrack::Temperature.colors().unwrap();
        let cool = *temperature.first().unwrap();
        let warm = *temperature.last().unwrap();
        assert!(cool.b > warm.b);

        let tint = CameraRawSliderTrack::Tint.colors().unwrap();
        let green = *tint.first().unwrap();
        let mauve = *tint.last().unwrap();
        assert!(green.g > mauve.g);

        let chroma = CameraRawSliderTrack::Chroma.colors().unwrap();
        let gray = *chroma.first().unwrap();
        let red = *chroma.last().unwrap();
        assert!((gray.r - gray.g).abs() < 0.05);
        assert!(red.r > red.g + 0.4);

        assert_eq!(CameraRawSliderTrack::Plain.colors(), None);
    }

    /// The Swift test drew the bar and read its pixels; here the ends of the ramp the bar is
    /// painted from carry the same two assertions, and the knob's hit test is checked beside them.
    #[test]
    fn temperature_bar_is_blue_on_the_left_and_a_double_click_hits_only_the_knob() {
        let temperature = CameraRawSliderTrack::Temperature.colors().unwrap();
        let left = *temperature.first().unwrap();
        let right = *temperature.last().unwrap();
        assert!(left.b > left.r, "left is blue: {left:?}");
        assert!(right.r + right.g > right.b + 0.4, "right is yellow: {right:?}");

        // 40 of -100…100 on a 200-wide bar with the 20-point knob the Swift test used: the knob
        // sits at 0.7 of the travel, its center 136 along the track.
        let geometry = SliderGeometry::new(0.0, 200.0, 20.0);
        assert!(geometry.is_on_knob(136.0, 0.7, KNOB_REACH));
        assert!(!geometry.is_on_knob(2.0, 0.7, KNOB_REACH));
    }

    #[test]
    fn color_balance_tracks_run_from_each_color_to_its_opposite() {
        let cyan_red =
            CameraRawSliderTrack::Opposing(srgb(0.10, 0.72, 0.80), srgb(0.86, 0.18, 0.20))
                .colors()
                .unwrap();
        assert!(cyan_red[0].b > cyan_red[0].r && cyan_red[1].r > cyan_red[1].b);
        let magenta_green =
            CameraRawSliderTrack::Opposing(srgb(0.80, 0.22, 0.70), srgb(0.24, 0.70, 0.30))
                .colors()
                .unwrap();
        assert!(
            magenta_green[0].r > magenta_green[0].g && magenta_green[1].g > magenta_green[1].r
        );
        let yellow_blue =
            CameraRawSliderTrack::Opposing(srgb(0.95, 0.82, 0.18), srgb(0.22, 0.40, 0.92))
                .colors()
                .unwrap();
        assert!(yellow_blue[0].g > yellow_blue[0].b && yellow_blue[1].b > yellow_blue[1].g);

        let greens = CameraRawSliderTrack::Luminance(120.0).colors().unwrap();
        let greens = *greens.last().unwrap();
        assert!(greens.g > greens.r && greens.g > greens.b);
    }

    #[test]
    fn the_spectrum_track_is_the_whole_hue_circle() {
        // `stride(from: -180, through: 180, by: 30)`: thirteen colors. The ends are hue ±180,
        // which wrap to the same 0.5 cyan — opposite the middle's red — so the circle closes
        // (`CameraRawSlider.swift:46-47,52-54`).
        let colors = CameraRawSliderTrack::Spectrum(0.0).colors().unwrap();
        assert_eq!(colors.len(), 13);
        assert_eq!(colors[6], hsb(0.0, 0.85, 0.9), "the hue in the middle is red");
        assert_eq!(colors[0], colors[12], "the ends of the circle meet");
        assert!(colors[0].b > colors[0].r, "the ends are the hue opposite red");
        // Walking backwards from the red middle: hue 0.9167 sits between red and magenta.
        assert!(
            colors[5].r > colors[5].g && colors[5].b > colors[5].g,
            "and runs backwards through magenta"
        );
    }

    #[test]
    fn the_track_glide_eases_out_like_the_animation_curve() {
        assert_eq!(ease_out(0.0), 0.0);
        assert!((ease_out(1.0) - 1.0).abs() < 1e-5);
        assert!(ease_out(0.5) > 0.5, "ease-out is ahead of linear");
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = ease_out(step as f32 / 100.0);
            assert!(value >= previous, "the curve never goes back: {value} < {previous}");
            previous = value;
        }
        assert!(GLIDE_DURATION > std::time::Duration::ZERO);
    }

    #[test]
    fn hsb_wraps_the_hue_circle_like_the_system_color() {
        let red = hsb(0.0, 1.0, 1.0);
        assert_eq!((red.r, red.g, red.b), (1.0, 0.0, 0.0));
        assert_eq!(hsb(1.0, 1.0, 1.0), red);
        assert_eq!(hsb(-1.0, 1.0, 1.0), red);
        let green = hsb(1.0 / 3.0, 1.0, 1.0);
        assert!(green.g > 0.99 && green.r < 0.01 && green.b < 0.01);
        assert_eq!(hsb(-1.0 / 6.0, 1.0, 1.0), hsb(5.0 / 6.0, 1.0, 1.0));
    }

    struct Harness {
        value: f64,
        track: CameraRawSliderTrack,
        changes: Rc<RefCell<Vec<f64>>>,
        resets: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let changes = self.changes.clone();
            let resets = self.resets.clone();
            let harness = cx.entity().downgrade();
            div().w(px(200.)).h(px(22.)).child(
                CameraRawSlider::new(
                    "exposure",
                    self.value,
                    (-100.0, 100.0),
                    self.track.clone(),
                    "Exposure. Double-click to reset.",
                )
                .step(1.0)
                .on_change(move |value, _, cx| {
                    changes.borrow_mut().push(value);
                    if let Some(harness) = harness.upgrade() {
                        harness.update(cx, |harness, cx| {
                            harness.value = value;
                            cx.notify();
                        });
                    }
                })
                .on_reset(move |_, _| resets.set(resets.get() + 1)),
            )
        }
    }

    /// The value the knob's center asks for, the mapping `CameraRawSliderView.value(at:)` used.
    fn value_at(point_x: f32) -> f64 {
        -100.0 + f64::from((point_x - KNOB_SIZE / 2.0) / (200.0 - KNOB_SIZE)) * 200.0
    }

    #[gpui_kit::test]
    fn a_press_on_the_track_publishes_where_it_points_once_and_glides_there(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let resets = Rc::new(Cell::new(0));
        let (window, _harness) = open_window(cx, {
            let changes = changes.clone();
            let resets = resets.clone();
            |_, cx| {
                cx.new(|_| Harness {
                    value: 0.0,
                    track: CameraRawSliderTrack::Temperature,
                    changes,
                    resets,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click_at("exposure", point(px(150.), px(11.)), cx);

            let published = changes.borrow().clone();
            assert_eq!(
                published.len(),
                1,
                "the destination is published once: {published:?}"
            );
            assert!(
                (published[0] - value_at(150.0)).abs() < 1.0,
                "under the click: {} against {}",
                published[0],
                value_at(150.0)
            );

            // The knob glides there over the next frames; the value is not published again.
            for _ in 0..12 {
                window.simulate_next_frame(cx);
            }
            assert_eq!(changes.borrow().len(), 1);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_double_click_on_the_knob_restores_the_default(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let resets = Rc::new(Cell::new(0));
        let (window, _harness) = open_window(cx, {
            let changes = changes.clone();
            let resets = resets.clone();
            |_, cx| {
                cx.new(|_| Harness {
                    // Zero of -100…100 puts the knob in the middle, where a double-click lands.
                    value: 0.0,
                    track: CameraRawSliderTrack::Tint,
                    changes,
                    resets,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.double_click("exposure", cx);
            assert_eq!(resets.get(), 1, "the knob's double-click resets");
            assert!(changes.borrow().is_empty());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_double_click_on_the_track_does_not_reset(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let resets = Rc::new(Cell::new(0));
        let (window, _harness) = open_window(cx, {
            let changes = changes.clone();
            let resets = resets.clone();
            |_, cx| {
                cx.new(|_| Harness {
                    // The value's knob sits at the far left, so the element's center is track.
                    value: -100.0,
                    track: CameraRawSliderTrack::Chroma,
                    changes,
                    resets,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.double_click("exposure", cx);
            assert_eq!(resets.get(), 0, "only the knob resets");
            // A double click is two mouse-downs; the Swift view publishes on each track press
            // (`CameraRawSlider.swift:139-146`), so both land and neither resets.
            assert_eq!(changes.borrow().len(), 2, "and both presses still land");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn dragging_the_knob_reports_the_value_under_the_pointer(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let resets = Rc::new(Cell::new(0));
        let (window, _harness) = open_window(cx, {
            let changes = changes.clone();
            let resets = resets.clone();
            |_, cx| {
                cx.new(|_| Harness {
                    value: 0.0,
                    track: CameraRawSliderTrack::Spectrum(0.0),
                    changes,
                    resets,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // The knob's center is the element's middle at zero; drag it to 140.
            window.drag(point(px(100.), px(11.)), point(px(140.), px(11.)), cx);
            let changes = changes.borrow();
            assert!(changes.len() > 1, "a drag is continuous: {changes:?}");
            let last = *changes.last().unwrap();
            assert!(
                (last - value_at(140.0)).abs() < 2.0,
                "the pointer's value: {last} against {}",
                value_at(140.0)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn arrow_keys_step_the_value_and_shift_steps_ten_times_as_far(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let resets = Rc::new(Cell::new(0));
        let (window, _harness) = open_window(cx, {
            let changes = changes.clone();
            let resets = resets.clone();
            |_, cx| {
                cx.new(|_| Harness {
                    value: 0.0,
                    track: CameraRawSliderTrack::Plain,
                    changes,
                    resets,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // A press on the knob takes focus without changing the value.
            window.click_at("exposure", point(px(100.), px(11.)), cx);
            assert!(changes.borrow().is_empty());

            window.press("right", cx);
            assert_eq!(changes.borrow().last(), Some(&1.0));
            window.press("shift-left", cx);
            assert_eq!(changes.borrow().last(), Some(&-9.0));
        })
        .unwrap();
    }

    /// Opens a window the way gpui-kit's own tests do: the production entry point with test bounds.
    fn open_window<V: Render>(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
    ) -> (AnyWindowHandle, Entity<V>) {
        cx.update(|cx| {
            let bounds = gpui_kit::Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(200.), px(24.)),
            };
            let (window, content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                build,
            )
            .expect("open the test window");
            (window, content)
        })
    }
}
