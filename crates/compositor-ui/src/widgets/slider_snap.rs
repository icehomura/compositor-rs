//! The value a slider takes from a press on its track (port of `UI/SliderSnap.swift`).
//!
//! On macOS, clicking a slider's track glides the knob to the click over about a quarter of a
//! second, even though the value changes at once. Setting a slider's value in code moves the knob
//! immediately, so `SliderSnap` set the value under the pointer just before the cell started
//! tracking the press: tracking then began with the knob already under the pointer, so it neither
//! jumped nor glided, and dragging carried on from there. Pressing the knob itself still dragged it
//! from where it was.
//!
//! gpui sliders move their knob the moment a press changes the value, so the port needs no such
//! hook: the sliders in [`crate::widgets::gradient_slider`] compute the value under the press
//! directly. This module holds the arithmetic they share — the same mapping `SliderSnap` and
//! `CameraRawSliderView.value(at:)` used.

/// A linear slider's track and knob rectangles (an `NSSliderCell`'s `trackRect` and `knobRect`),
/// reduced to what the value mapping needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderGeometry {
    /// The track's left edge, in the slider's own coordinates.
    pub track_left: f32,
    /// The track's width.
    pub track_width: f32,
    /// The knob's width. Its center travels over `track_width - knob_width`, so it stays inside
    /// the track's ends.
    pub knob_width: f32,
}

impl SliderGeometry {
    /// The geometry of a track `track_width` wide, starting at `track_left`.
    pub const fn new(track_left: f32, track_width: f32, knob_width: f32) -> Self {
        Self {
            track_left,
            track_width,
            knob_width,
        }
    }

    /// How far the knob's center can travel: `track.width - knob.width`.
    pub fn travel(&self) -> f32 {
        self.track_width - self.knob_width
    }

    /// Where a press sits along the travel, `0` at the left end and `1` at the right one:
    /// `(point.x - track.minX - knob.width / 2) / travel`, clamped. `None` when the knob has no
    /// room to travel, which the Swift code treated as "leave the value alone".
    pub fn fraction(&self, point_x: f32, right_to_left: bool) -> Option<f32> {
        let travel = self.travel();
        if travel <= 0.0 {
            return None;
        }
        let fraction = ((point_x - self.track_left - self.knob_width / 2.0) / travel).clamp(0.0, 1.0);
        Some(if right_to_left { 1.0 - fraction } else { fraction })
    }

    /// The value whose knob is centered on a press: `min + fraction * (max - min)`.
    pub fn value(&self, point_x: f32, min: f64, max: f64, right_to_left: bool) -> Option<f64> {
        let fraction = self.fraction(point_x, right_to_left)?;
        Some(min + f64::from(fraction) * (max - min))
    }

    /// Where a value's knob sits, the inverse of [`Self::fraction`]:
    /// `fraction * travel + knob.width / 2`.
    pub fn knob_center_x(&self, fraction: f32) -> f32 {
        fraction * self.travel() + self.knob_width / 2.0
    }

    /// Whether a press lands on the knob. `CameraRawSliderView.isOnKnob` tested the drawn knob's
    /// frame `insetBy(dx: -2, dy: -2)`, so the reach here is likewise the knob's half-width plus
    /// two points: the slider is only as tall as the knob, so a press inside it is always within
    /// the knob's band vertically.
    pub fn is_on_knob(&self, point_x: f32, fraction: f32, reach: f32) -> bool {
        let center = self.knob_center_x(fraction) + self.track_left;
        (point_x - center).abs() <= self.knob_width / 2.0 + reach
    }
}

/// A press on a linear slider's track, mapped to the value it asks for (port of `SliderSnap`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderSnap {
    geometry: SliderGeometry,
    min: f64,
    max: f64,
    right_to_left: bool,
    /// `allowsTickMarkValuesOnly` with a positive `numberOfTickMarks`: the value rounds to the
    /// nearest mark.
    tick_marks: Option<usize>,
}

impl SliderSnap {
    /// Snapping for a slider over `min`…`max`. `right_to_left` mirrors the mapping, as a
    /// right-to-left layout direction does.
    pub fn new(geometry: SliderGeometry, min: f64, max: f64, right_to_left: bool) -> Self {
        Self {
            geometry,
            min,
            max,
            right_to_left,
            tick_marks: None,
        }
    }

    /// Makes the slider take only tick-mark values, like `allowsTickMarkValuesOnly` with
    /// `number_of_tick_marks` marks spread evenly from one end of the range to the other.
    pub fn tick_marks(mut self, number_of_tick_marks: usize) -> Self {
        self.tick_marks = (number_of_tick_marks > 0).then_some(number_of_tick_marks);
        self
    }

    /// The value the press asks for, or `None` when the slider cannot take one — a vertical or
    /// disabled slider in the Swift hook, or one whose knob has no room to travel.
    pub fn value(&self, point_x: f32) -> Option<f64> {
        let value = self
            .geometry
            .value(point_x, self.min, self.max, self.right_to_left)?;
        Some(match self.tick_marks {
            Some(count) => closest_tick_mark_value(value, self.min, self.max, count),
            None => value,
        })
    }
}

/// `NSSliderCell.closestTickMarkValue(toValue:)`: the evenly spaced tick mark nearest `value`.
/// The range's ends are marks, so `count` marks sit `(max - min) / (count - 1)` apart.
pub fn closest_tick_mark_value(value: f64, min: f64, max: f64, count: usize) -> f64 {
    if count < 2 || max <= min {
        return value.clamp(min, max);
    }
    let spacing = (max - min) / (count - 1) as f64;
    let index = ((value - min) / spacing).round().clamp(0.0, (count - 1) as f64);
    min + index * spacing
}

/// `(value * scale).rounded() / scale`: the rounding the Camera Raw panel applies to what its
/// sliders report, `scale` being `10^decimals`.
pub fn rounded_to_scale(value: f64, scale: f64) -> f64 {
    (value * scale).round() / scale
}

/// `(value / step).rounded() * step`: the grid a dragged value snaps to (see
/// [`crate::widgets::numeric_scrub::Scrub`]), whole numbers for a step of 1.
pub fn snapped_to_step(value: f64, step: f64) -> f64 {
    (value / step).round() * step
}

/// `min(upper, max(lower, value))`, the clamp every one of these controls applies.
pub fn clamped(value: f64, lower: f64, upper: f64) -> f64 {
    value.clamp(lower, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell of a 220-wide slider with the 20-point knob an `NSSliderCell` draws.
    fn slider_220() -> SliderGeometry {
        SliderGeometry::new(0.0, 220.0, 20.0)
    }

    #[test]
    fn clicking_the_track_snaps_before_native_tracking_begins() {
        let snap = SliderSnap::new(slider_220(), 0.0, 1.0, false);
        let press = 220.0 * 0.9;
        let value = snap.value(press).expect("the press maps to a value");
        assert!(
            (value - 0.94).abs() < 0.02,
            "value should snap under the click before tracking: {value}"
        );
    }

    #[test]
    fn track_click_value_matches_the_clicked_position() {
        let snap = SliderSnap::new(SliderGeometry::new(0.0, 200.0, 20.0), -100.0, 100.0, false);
        let left = snap.value(0.0).expect("left maps");
        let center = snap.value(100.0).expect("center maps");
        let right = snap.value(200.0).expect("right maps");
        assert_eq!(left, -100.0);
        assert!(center.abs() < 1.0, "the middle is the range's center: {center}");
        assert_eq!(right, 100.0);
    }

    #[test]
    fn a_press_on_the_knob_is_told_apart_from_a_press_on_the_track() {
        let geometry = SliderGeometry::new(0.0, 200.0, 20.0);
        // 40 of -100…100 puts the knob at 0.7 of the travel, its center 136 along the track.
        let fraction = ((40.0 + 100.0) / 200.0) as f32;
        assert_eq!(geometry.knob_center_x(fraction), 136.0);
        assert!(geometry.is_on_knob(136.0, fraction, 2.0));
        assert!(!geometry.is_on_knob(2.0, fraction, 2.0));
        // The inverse mapping puts the press back where it was.
        assert!((geometry.fraction(136.0, false).unwrap() - fraction).abs() < 1e-6);
    }

    #[test]
    fn a_right_to_left_layout_mirrors_the_fraction() {
        let snap = SliderSnap::new(SliderGeometry::new(0.0, 200.0, 20.0), 0.0, 100.0, true);
        assert_eq!(snap.value(0.0), Some(100.0));
        assert_eq!(snap.value(200.0), Some(0.0));
    }

    #[test]
    fn a_knob_with_no_room_to_travel_leaves_the_value_alone() {
        let snap = SliderSnap::new(SliderGeometry::new(0.0, 20.0, 20.0), 0.0, 1.0, false);
        assert_eq!(snap.value(10.0), None);
    }

    #[test]
    fn tick_mark_values_only_rounds_to_the_nearest_mark() {
        let snap = SliderSnap::new(SliderGeometry::new(0.0, 200.0, 20.0), 0.0, 100.0, false)
            .tick_marks(11);
        // The press lands at 0.472 of the travel — 47.2 — and with eleven marks every 10 the
        // tick-mark-only value rounds to the nearest mark (`SliderSnap.swift:39`).
        let value = snap.value(10.0 + 0.472 * 180.0).expect("the press maps");
        assert!((value - 50.0).abs() < 1e-9, "the press rounds to the nearest mark: {value}");
        assert_eq!(closest_tick_mark_value(47.2, 0.0, 100.0, 11), 50.0);
        assert_eq!(closest_tick_mark_value(-5.0, 0.0, 100.0, 11), 0.0);
        assert_eq!(closest_tick_mark_value(105.0, 0.0, 100.0, 11), 100.0);
    }

    #[test]
    fn the_camera_raw_rounding_and_the_drag_grid_are_both_multiples() {
        assert_eq!(rounded_to_scale(12.345, 100.0), 12.35);
        assert_eq!(rounded_to_scale(12.344, 10.0), 12.3);
        assert_eq!(snapped_to_step(12.4, 1.0), 12.0);
        assert_eq!(snapped_to_step(12.6, 1.0), 13.0);
        assert_eq!(snapped_to_step(0.4, 0.5), 0.5);
        assert_eq!(clamped(1200.0, 1.0, 2000.0), 1200.0);
        assert_eq!(clamped(2400.0, 1.0, 2000.0), 2000.0);
        assert_eq!(clamped(0.0, 1.0, 2000.0), 1.0);
    }
}
