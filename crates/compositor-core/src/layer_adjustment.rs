//! The adjustment-layer model: what correction a layer applies and the settings behind it.
//!
//! Ported from `Document/LayerAdjustment.swift` ([`AdjustmentKind`], [`LayerAdjustment`]),
//! `Document/Levels.swift` ([`LevelsChannel`], [`LevelRange`], [`LevelsSettings`]),
//! `Document/Curves.swift` ([`CurvePoint`], [`CurvesSettings`]),
//! `Document/HueSaturation.swift`'s settings types ([`ColorRange`], [`HueBand`],
//! [`RangeAdjustment`], [`HueSaturationSettings`]) and `Document/ImageAdjustments.swift`
//! ([`AdjustmentColor`], [`ExposureSettings`], [`GradientMapSettings`], [`BlackWhiteSettings`],
//! [`ColorBalanceSettings`], [`GrainSettings`]).
//!
//! The per-pixel work is not here: `compositor_core` cannot depend on a rasterizer.
//! `LayerAdjustment.apply(_:region:scale:)`'s dispatch is
//! `compositor_render::AdjustmentApply` and the settings' `apply`s are `compositor_pixels`.
//! This module is data only — the ranges, the validity rules and the scalar math the panels and
//! the rasterizers share.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::color::PaletteColor;
use crate::image_ops::{finite_clamp, FilterKind};

/// The correction an adjustment layer applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AdjustmentKind {
    #[serde(rename = "Hue/Saturation")]
    Hsv,
    #[serde(rename = "Levels")]
    Levels,
    #[serde(rename = "Curves")]
    Curves,
    #[serde(rename = "Exposure")]
    Exposure,
    #[serde(rename = "Gradient Map")]
    GradientMap,
    #[serde(rename = "Grain")]
    Grain,
    #[serde(rename = "Add Noise")]
    AddNoise,
    #[serde(rename = "Gaussian Blur")]
    GaussianBlur,
    #[serde(rename = "Motion Blur")]
    MotionBlur,
    #[serde(rename = "Invert")]
    Invert,
    #[serde(rename = "Black & White")]
    BlackWhite,
    #[serde(rename = "Color Balance")]
    ColorBalance,
}

impl AdjustmentKind {
    /// `CaseIterable` order, which is also the New Adjustment Layer menu's.
    pub const ALL: [AdjustmentKind; 12] = [
        AdjustmentKind::Hsv,
        AdjustmentKind::Levels,
        AdjustmentKind::Curves,
        AdjustmentKind::Exposure,
        AdjustmentKind::GradientMap,
        AdjustmentKind::Grain,
        AdjustmentKind::AddNoise,
        AdjustmentKind::GaussianBlur,
        AdjustmentKind::MotionBlur,
        AdjustmentKind::Invert,
        AdjustmentKind::BlackWhite,
        AdjustmentKind::ColorBalance,
    ];

    /// The raw string the Swift case holds (the manifest's `kind`, menu labels and undo names).
    pub fn raw_value(self) -> &'static str {
        match self {
            AdjustmentKind::Hsv => "Hue/Saturation",
            AdjustmentKind::Levels => "Levels",
            AdjustmentKind::Curves => "Curves",
            AdjustmentKind::Exposure => "Exposure",
            AdjustmentKind::GradientMap => "Gradient Map",
            AdjustmentKind::Grain => "Grain",
            AdjustmentKind::AddNoise => "Add Noise",
            AdjustmentKind::GaussianBlur => "Gaussian Blur",
            AdjustmentKind::MotionBlur => "Motion Blur",
            AdjustmentKind::Invert => "Invert",
            AdjustmentKind::BlackWhite => "Black & White",
            AdjustmentKind::ColorBalance => "Color Balance",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.raw_value() == value)
    }

    /// The Layers panel's icon.
    pub fn symbol(self) -> &'static str {
        match self {
            AdjustmentKind::Curves => "point.topleft.down.to.point.bottomright.curvepath",
            AdjustmentKind::Levels => "slider.horizontal.3",
            AdjustmentKind::Hsv => "circle.lefthalf.filled",
            AdjustmentKind::Exposure => "plusminus.circle",
            AdjustmentKind::GradientMap => "paintpalette",
            AdjustmentKind::Grain => "circle.grid.3x3",
            AdjustmentKind::GaussianBlur => "drop.fill",
            AdjustmentKind::MotionBlur => "wind",
            AdjustmentKind::AddNoise => "circle.dotted",
            AdjustmentKind::Invert => "circle.righthalf.filled",
            AdjustmentKind::BlackWhite => "circle.filled.pattern.diagonalline.rectangle",
            AdjustmentKind::ColorBalance => "scale.3d",
        }
    }

    /// The filter panel that edits this kind; Levels and Hue/Saturation have panels of their own.
    /// Every kind but Invert opens an editor when its layer is double-clicked.
    pub fn is_editable(self) -> bool {
        self != AdjustmentKind::Invert
    }

    pub fn filter_kind(self) -> Option<FilterKind> {
        match self {
            AdjustmentKind::Curves => Some(FilterKind::Curves),
            AdjustmentKind::BlackWhite => Some(FilterKind::BlackWhite),
            AdjustmentKind::ColorBalance => Some(FilterKind::ColorBalance),
            AdjustmentKind::Exposure => Some(FilterKind::Exposure),
            AdjustmentKind::GradientMap => Some(FilterKind::GradientMap),
            AdjustmentKind::Grain => Some(FilterKind::Grain),
            AdjustmentKind::GaussianBlur => Some(FilterKind::GaussianBlur),
            AdjustmentKind::MotionBlur => Some(FilterKind::MotionBlur),
            AdjustmentKind::AddNoise => Some(FilterKind::AddNoise),
            // Hue/Saturation and Levels have panels of their own; Invert has nothing to set.
            AdjustmentKind::Hsv | AdjustmentKind::Levels | AdjustmentKind::Invert => None,
        }
    }
}

/// A Levels channel; the composite RGB first, as the panel lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LevelsChannel {
    #[serde(rename = "RGB")]
    Rgb,
    #[serde(rename = "Red")]
    Red,
    #[serde(rename = "Green")]
    Green,
    #[serde(rename = "Blue")]
    Blue,
}

impl LevelsChannel {
    /// `CaseIterable` order, which is also the picker's.
    pub const ALL: [LevelsChannel; 4] = [
        LevelsChannel::Rgb,
        LevelsChannel::Red,
        LevelsChannel::Green,
        LevelsChannel::Blue,
    ];

    /// The channel's index into `LevelsSettings.ranges` and the histogram.
    pub fn index(self) -> usize {
        match self {
            LevelsChannel::Rgb => 0,
            LevelsChannel::Red => 1,
            LevelsChannel::Green => 2,
            LevelsChannel::Blue => 3,
        }
    }

    /// The raw string the Swift case holds.
    pub fn raw_value(self) -> &'static str {
        match self {
            LevelsChannel::Rgb => "RGB",
            LevelsChannel::Red => "Red",
            LevelsChannel::Green => "Green",
            LevelsChannel::Blue => "Blue",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|channel| channel.raw_value() == value)
    }
}

/// One channel's input and output points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LevelRange {
    pub black: f64,
    pub gamma: f64,
    pub white: f64,
    pub output_black: f64,
    pub output_white: f64,
}

impl Default for LevelRange {
    fn default() -> Self {
        LevelRange {
            black: 0.0,
            gamma: 1.0,
            white: 255.0,
            output_black: 0.0,
            output_white: 255.0,
        }
    }
}

impl LevelRange {
    /// The values the slider ranges allow: black below white, gamma past zero, outputs in 0…255.
    pub fn normalized(&self) -> Self {
        let mut result = *self;
        result.black = finite_clamp(self.black, (0.0, 254.0), 0.0);
        result.white = finite_clamp(self.white, (result.black + 1.0, 255.0), 255.0);
        result.gamma = finite_clamp(self.gamma, (0.1, 9.99), 1.0);
        result.output_black = finite_clamp(self.output_black, (0.0, 255.0), 0.0);
        result.output_white = finite_clamp(self.output_white, (0.0, 255.0), 255.0);
        result
    }

    /// The value's normalized position between the black and white points, bent by the gamma,
    /// placed between the output points.
    pub fn apply(&self, value: f64) -> f64 {
        let settings = self.normalized();
        let input = ((value * 255.0 - settings.black) / (settings.white - settings.black))
            .min(1.0)
            .max(0.0);
        (settings.output_black
            + input.powf(1.0 / settings.gamma) * (settings.output_white - settings.output_black))
            / 255.0
    }
}

/// One Levels adjustment's settings: the channel the panel edits and a range for each channel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LevelsSettings {
    pub channel: LevelsChannel,
    /// RGB, then red, green and blue.
    pub ranges: [LevelRange; 4],
}

impl Default for LevelsSettings {
    fn default() -> Self {
        LevelsSettings {
            channel: LevelsChannel::Rgb,
            ranges: [LevelRange::default(); 4],
        }
    }
}

impl LevelsSettings {
    /// The range the sliders point at.
    pub fn current(&self) -> LevelRange {
        self.ranges[self.channel.index()]
    }

    /// Writes the edited range back, normalized as the sliders' ranges allow.
    pub fn set_current(&mut self, value: LevelRange) {
        self.ranges[self.channel.index()] = value.normalized();
    }

    pub fn is_identity(&self) -> bool {
        self.ranges.iter().all(|range| range.normalized() == LevelRange::default())
    }

    /// Individual channels, followed by the composite RGB adjustment.
    pub fn apply(&self, value: f64, channel: LevelsChannel) -> f64 {
        self.ranges[0].apply(self.ranges[channel.index()].apply(value))
    }
}

/// One handle of a curves channel, in 0…255 input and output.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurvePoint {
    pub x: f64,
    pub y: f64,
}

/// One Curves adjustment's settings: the channel the panel edits and a point list for each channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CurvesSettings {
    pub channel: LevelsChannel,
    /// RGB, then red, green and blue.
    pub channels: [Vec<CurvePoint>; 4],
}

impl Default for CurvesSettings {
    fn default() -> Self {
        CurvesSettings {
            channel: LevelsChannel::Rgb,
            channels: std::array::from_fn(|_| {
                vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 255.0, y: 255.0 }]
            }),
        }
    }
}

impl CurvesSettings {
    /// Two to thirty-two strictly increasing handles, from x = 0 to x = 255, every coordinate in
    /// 0…255.
    pub fn is_valid(&self) -> bool {
        self.channels.iter().all(|points| {
            (2..=32).contains(&points.len())
                && points.first().map(|point| point.x) == Some(0.0)
                && points.last().map(|point| point.x) == Some(255.0)
                && points.iter().all(|point| {
                    point.x.is_finite()
                        && point.y.is_finite()
                        && (0.0..=255.0).contains(&point.x)
                        && (0.0..=255.0).contains(&point.y)
                })
                && points.windows(2).all(|pair| pair[0].x < pair[1].x)
        })
    }

    /// Shape-preserving cubic Hermite interpolation avoids overshoot between handles.
    pub fn value(&self, x: f64, channel: usize) -> f64 {
        let points = &self.channels[channel];
        if points.len() < 2 {
            return 0.0;
        }
        let deltas: Vec<f64> = points
            .windows(2)
            .map(|pair| (pair[1].y - pair[0].y) / (pair[1].x - pair[0].x))
            .collect();

        /// The tangent at a handle: the neighbours' secant at the ends, the harmonic mean of the
        /// two when the curve keeps going the same way, and flat at a turning point.
        fn slope(deltas: &[f64], count: usize, index: usize) -> f64 {
            if index == 0 {
                return deltas[0];
            }
            if index == count - 1 {
                return deltas[deltas.len() - 1];
            }
            if deltas[index - 1] * deltas[index] <= 0.0 {
                return 0.0;
            }
            2.0 / (1.0 / deltas[index - 1] + 1.0 / deltas[index])
        }

        let last = points.iter().rposition(|point| point.x <= x).unwrap_or(0);
        let i = (points.len() - 2).min(last);
        let h = points[i + 1].x - points[i].x;
        let t = ((x - points[i].x) / h).min(1.0).max(0.0);
        let y = (2.0 * t * t * t - 3.0 * t * t + 1.0) * points[i].y
            + (t * t * t - 2.0 * t * t + t) * h * slope(&deltas, points.len(), i)
            + (-2.0 * t * t * t + 3.0 * t * t) * points[i + 1].y
            + (t * t * t - t * t) * h * slope(&deltas, points.len(), i + 1);
        y.min(255.0).max(0.0)
    }
}

/// The six color ranges plus Master, as in Photoshop's Cmd+U.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorRange {
    #[serde(rename = "Master")]
    Master,
    #[serde(rename = "Reds")]
    Reds,
    #[serde(rename = "Yellows")]
    Yellows,
    #[serde(rename = "Greens")]
    Greens,
    #[serde(rename = "Cyans")]
    Cyans,
    #[serde(rename = "Blues")]
    Blues,
    #[serde(rename = "Magentas")]
    Magentas,
}

impl ColorRange {
    /// `CaseIterable` order, Master first.
    pub const ALL: [ColorRange; 7] = [
        ColorRange::Master,
        ColorRange::Reds,
        ColorRange::Yellows,
        ColorRange::Greens,
        ColorRange::Cyans,
        ColorRange::Blues,
        ColorRange::Magentas,
    ];

    /// The six ranges that have a band; Master applies everywhere.
    pub const COLOR_RANGES: [ColorRange; 6] = [
        ColorRange::Reds,
        ColorRange::Yellows,
        ColorRange::Greens,
        ColorRange::Cyans,
        ColorRange::Blues,
        ColorRange::Magentas,
    ];

    /// The raw string the Swift case holds.
    pub fn raw_value(self) -> &'static str {
        match self {
            ColorRange::Master => "Master",
            ColorRange::Reds => "Reds",
            ColorRange::Yellows => "Yellows",
            ColorRange::Greens => "Greens",
            ColorRange::Cyans => "Cyans",
            ColorRange::Blues => "Blues",
            ColorRange::Magentas => "Magentas",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|range| range.raw_value() == value)
    }

    /// Photoshop's starting hue band: falloff start, range start, range end, falloff end.
    pub fn default_band(self) -> HueBand {
        match self {
            ColorRange::Master => HueBand {
                falloff_start: 0.0,
                range_start: 0.0,
                range_end: 360.0,
                falloff_end: 360.0,
            },
            ColorRange::Reds => HueBand {
                falloff_start: 315.0,
                range_start: 345.0,
                range_end: 15.0,
                falloff_end: 45.0,
            },
            ColorRange::Yellows => HueBand {
                falloff_start: 15.0,
                range_start: 45.0,
                range_end: 75.0,
                falloff_end: 105.0,
            },
            ColorRange::Greens => HueBand {
                falloff_start: 75.0,
                range_start: 105.0,
                range_end: 135.0,
                falloff_end: 165.0,
            },
            ColorRange::Cyans => HueBand {
                falloff_start: 135.0,
                range_start: 165.0,
                range_end: 195.0,
                falloff_end: 225.0,
            },
            ColorRange::Blues => HueBand {
                falloff_start: 195.0,
                range_start: 225.0,
                range_end: 255.0,
                falloff_end: 285.0,
            },
            ColorRange::Magentas => HueBand {
                falloff_start: 255.0,
                range_start: 285.0,
                range_end: 315.0,
                falloff_end: 345.0,
            },
        }
    }
}

/// A hue band in degrees, wrapping at 360: full strength between `range_start` and `range_end`,
/// fading to nothing at `falloff_start` and `falloff_end`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HueBand {
    pub falloff_start: f64,
    pub range_start: f64,
    pub range_end: f64,
    pub falloff_end: f64,
}

impl HueBand {
    /// Degrees from `from` forward to `to`, always 0…360.
    pub fn forward(from: f64, to: f64) -> f64 {
        let delta = (to - from) % 360.0;
        if delta < 0.0 {
            delta + 360.0
        } else {
            delta
        }
    }

    /// How strongly this band claims a hue: 1 inside the range, ramping linearly through each
    /// falloff shoulder, 0 outside. Wraparound is handled by measuring forward.
    pub fn weight(&self, hue: f64) -> f64 {
        let span = Self::forward(self.falloff_start, self.falloff_end);
        if !(span > 0.0) {
            // Master covers everything.
            return 1.0;
        }
        let position = Self::forward(self.falloff_start, hue);
        if position > span {
            return 0.0;
        }
        let ramp_in = Self::forward(self.falloff_start, self.range_start);
        let plateau_end = Self::forward(self.falloff_start, self.range_end);
        if position < ramp_in {
            return if ramp_in > 0.0 { position / ramp_in } else { 1.0 };
        }
        if position <= plateau_end {
            return 1.0;
        }
        let ramp_out = span - plateau_end;
        if ramp_out > 0.0 {
            (span - position) / ramp_out
        } else {
            1.0
        }
    }

    pub fn handles(&self) -> [f64; 4] {
        [self.falloff_start, self.range_start, self.range_end, self.falloff_end]
    }

    /// A band centered on one hue, keeping this band's core and shoulder widths.
    pub fn centered(&self, hue: f64) -> HueBand {
        let core = Self::forward(self.range_start, self.range_end);
        let leading = Self::forward(self.falloff_start, self.range_start);
        let trailing = Self::forward(self.range_end, self.falloff_end);
        fn wrap(value: f64) -> f64 {
            let remainder = value % 360.0;
            if remainder < 0.0 {
                remainder + 360.0
            } else {
                remainder
            }
        }
        let start = wrap(hue - core / 2.0);
        HueBand {
            falloff_start: wrap(start - leading),
            range_start: start,
            range_end: wrap(start + core),
            falloff_end: wrap(start + core + trailing),
        }
    }

    /// Widens the band so this hue is fully inside it, moving whichever edge is nearer.
    pub fn include(&mut self, hue: f64) {
        if !(self.weight(hue) < 1.0) {
            return;
        }
        let shoulder_in = Self::forward(self.falloff_start, self.range_start);
        let shoulder_out = Self::forward(self.range_end, self.falloff_end);
        let before_start = Self::forward(hue, self.range_start);
        let after_end = Self::forward(self.range_end, hue);
        if before_start <= after_end {
            self.range_start = hue;
            self.falloff_start = hue - shoulder_in;
        } else {
            self.range_end = hue;
            self.falloff_end = hue + shoulder_out;
        }
        self.normalize();
    }

    /// Narrows the band so this hue falls outside it entirely, shoulder included.
    pub fn exclude(&mut self, hue: f64) {
        if !(self.weight(hue) > 0.0) {
            return;
        }
        let shoulder_in = Self::forward(self.falloff_start, self.range_start);
        let shoulder_out = Self::forward(self.range_end, self.falloff_end);
        let from_start = Self::forward(self.falloff_start, hue);
        let to_end = Self::forward(hue, self.falloff_end);
        if from_start <= to_end {
            self.falloff_start = hue + 1.0;
            self.range_start = hue + 1.0 + shoulder_in;
        } else {
            self.falloff_end = hue - 1.0;
            self.range_end = hue - 1.0 - shoulder_out;
        }
        self.normalize();
    }

    /// Keeps all four handles in 0…360 and the band under a full circle.
    fn normalize(&mut self) {
        fn wrap(value: f64) -> f64 {
            let remainder = value % 360.0;
            if remainder < 0.0 {
                remainder + 360.0
            } else {
                remainder
            }
        }
        self.falloff_start = wrap(self.falloff_start);
        self.range_start = wrap(self.range_start);
        self.range_end = wrap(self.range_end);
        self.falloff_end = wrap(self.falloff_end);
        if Self::forward(self.falloff_start, self.falloff_end) > 350.0 {
            self.falloff_end = wrap(self.falloff_start + 350.0);
        }
    }

    /// Moves one handle, keeping the four in order and the band under a full circle.
    pub fn set_handle(&mut self, index: usize, degrees: f64) {
        let mut updated = *self;
        let value = ((degrees % 360.0) + 360.0) % 360.0;
        match index {
            0 => updated.falloff_start = value,
            1 => updated.range_start = value,
            2 => updated.range_end = value,
            _ => updated.falloff_end = value,
        }
        let span = Self::forward(updated.falloff_start, updated.falloff_end);
        let to_start = Self::forward(updated.falloff_start, updated.range_start);
        let to_end = Self::forward(updated.falloff_start, updated.range_end);
        if !(span > 1.0 && span <= 350.0 && to_start <= to_end && to_end <= span) {
            return;
        }
        *self = updated;
    }
}

/// One color range's shift. Hue is −180…180 (0…360 when colorizing), Saturation −100…100 (0…100
/// colorizing) and Lightness −100…100.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RangeAdjustment {
    pub hue: f64,
    pub saturation: f64,
    pub lightness: f64,
}

/// One Hue/Saturation adjustment's settings: each color range keeps its own values; Master applies
/// everywhere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HueSaturationSettings {
    /// Which range the sliders and spectrum edit.
    pub range: ColorRange,
    pub colorize: bool,
    /// Applies the selected range to everything *outside* its band instead.
    pub invert_range: bool,
    #[serde(with = "pair_map")]
    pub adjustments: HashMap<ColorRange, RangeAdjustment>,
    #[serde(with = "pair_map")]
    pub bands: HashMap<ColorRange, HueBand>,
}

impl Default for HueSaturationSettings {
    fn default() -> Self {
        HueSaturationSettings::new(0.0, 0.0, 0.0, false, ColorRange::Master)
    }
}

impl HueSaturationSettings {
    /// Photoshop's starting point when Colorize is switched on (`colorizeStart`).
    pub fn colorize_start() -> Self {
        HueSaturationSettings::new(0.0, 25.0, 0.0, true, ColorRange::Master)
    }

    /// `HueSaturationSettings(hue:saturation:lightness:colorize:range:)`: the selected range's own
    /// values, and every range's default band.
    pub fn new(
        hue: f64,
        saturation: f64,
        lightness: f64,
        colorize: bool,
        range: ColorRange,
    ) -> Self {
        let mut adjustments = HashMap::new();
        adjustments.insert(range, RangeAdjustment { hue, saturation, lightness });
        HueSaturationSettings {
            range,
            colorize,
            invert_range: false,
            adjustments,
            bands: ColorRange::ALL
                .into_iter()
                .map(|range| (range, range.default_band()))
                .collect(),
        }
    }

    /// The sliders read and write the selected range.
    pub fn hue(&self) -> f64 {
        self.adjustments.get(&self.range).map_or(0.0, |adjustment| adjustment.hue)
    }

    pub fn set_hue(&mut self, value: f64) {
        self.adjustments.entry(self.range).or_default().hue = value;
    }

    pub fn saturation(&self) -> f64 {
        self.adjustments
            .get(&self.range)
            .map_or(0.0, |adjustment| adjustment.saturation)
    }

    pub fn set_saturation(&mut self, value: f64) {
        self.adjustments.entry(self.range).or_default().saturation = value;
    }

    pub fn lightness(&self) -> f64 {
        self.adjustments
            .get(&self.range)
            .map_or(0.0, |adjustment| adjustment.lightness)
    }

    pub fn set_lightness(&mut self, value: f64) {
        self.adjustments.entry(self.range).or_default().lightness = value;
    }

    pub fn band(&self) -> HueBand {
        self.bands
            .get(&self.range)
            .copied()
            .unwrap_or_else(|| self.range.default_band())
    }

    pub fn set_band(&mut self, band: HueBand) {
        self.bands.insert(self.range, band);
    }

    pub fn is_identity(&self) -> bool {
        !self.colorize
            && self
                .adjustments
                .values()
                .all(|adjustment| *adjustment == RangeAdjustment::default())
    }

    /// How much a range applies to one hue: Master everywhere, others through their band.
    pub fn weight(&self, color_range: ColorRange, hue: f64) -> f64 {
        if color_range == ColorRange::Master {
            return 1.0;
        }
        let weight = self
            .bands
            .get(&color_range)
            .copied()
            .unwrap_or_else(|| color_range.default_band())
            .weight(hue);
        if self.invert_range && color_range == self.range {
            1.0 - weight
        } else {
            weight
        }
    }
}

/// Swift's `Dictionary` writes a map whose key is not a `String`, an `Int` or
/// `CodingKeyRepresentable` as an unkeyed container of alternating keys and values (SE-0320), which
/// is how upstream saves `[ColorRange: RangeAdjustment]` and `[ColorRange: HueBand]`: the keys go
/// out in `ColorRange` order so a save is stable, and a repeated key keeps the last value read.
mod pair_map {
    use super::ColorRange;
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;
    use std::fmt;
    use std::marker::PhantomData;

    pub fn serialize<V, S>(map: &HashMap<ColorRange, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        V: Serialize,
        S: Serializer,
    {
        let present: Vec<ColorRange> = ColorRange::ALL
            .into_iter()
            .filter(|range| map.contains_key(range))
            .collect();
        let mut sequence = serializer.serialize_seq(Some(present.len() * 2))?;
        for range in present {
            sequence.serialize_element(&range)?;
            sequence.serialize_element(map.get(&range).expect("the key was in the map"))?;
        }
        sequence.end()
    }

    pub fn deserialize<'de, V, D>(deserializer: D) -> Result<HashMap<ColorRange, V>, D::Error>
    where
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(PairMapVisitor(PhantomData))
    }

    struct PairMapVisitor<V>(PhantomData<V>);

    impl<'de, V> Visitor<'de> for PairMapVisitor<V>
    where
        V: Deserialize<'de>,
    {
        type Value = HashMap<ColorRange, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an array of alternating color ranges and values")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut map = HashMap::new();
            while let Some(range) = sequence.next_element::<ColorRange>()? {
                let Some(value) = sequence.next_element::<V>()? else {
                    return Err(A::Error::custom("a color range without its value"));
                };
                map.insert(range, value);
            }
            Ok(map)
        }
    }
}

/// A straight sRGB color stored with an adjustment, 0–1 per channel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustmentColor {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
}

impl AdjustmentColor {
    pub const fn new(red: f64, green: f64, blue: f64) -> Self {
        AdjustmentColor { red, green, blue }
    }

    pub fn is_valid(&self) -> bool {
        [self.red, self.green, self.blue]
            .iter()
            .all(|channel| channel.is_finite() && (0.0..=1.0).contains(channel))
    }

    pub fn clamped(&self) -> Self {
        AdjustmentColor::new(
            finite_clamp(self.red, (0.0, 1.0), 0.0),
            finite_clamp(self.green, (0.0, 1.0), 0.0),
            finite_clamp(self.blue, (0.0, 1.0), 0.0),
        )
    }
}

impl From<PaletteColor> for AdjustmentColor {
    fn from(color: PaletteColor) -> Self {
        AdjustmentColor::new(color.red, color.green, color.blue)
    }
}

/// Photoshop's Exposure: `exposure` (stops) scales linear light and `offset` shifts it, then gamma
/// correction bends the result. The same curve runs on every channel; alpha is kept.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExposureSettings {
    /// Stops of light, −20…20.
    pub exposure: f64,
    /// Added in linear light, −0.5…0.5: negative deepens the shadows, positive lifts them.
    pub offset: f64,
    /// Gamma correction, 0.01…9.99; above 1 brightens the midtones.
    pub gamma: f64,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        ExposureSettings {
            exposure: 0.0,
            offset: 0.0,
            gamma: 1.0,
        }
    }
}

impl ExposureSettings {
    pub const EXPOSURE_RANGE: (f64, f64) = (-20.0, 20.0);
    pub const OFFSET_RANGE: (f64, f64) = (-0.5, 0.5);
    pub const GAMMA_RANGE: (f64, f64) = (0.01, 9.99);

    pub fn is_valid(&self) -> bool {
        (-20.0..=20.0).contains(&self.exposure)
            && (-0.5..=0.5).contains(&self.offset)
            && (0.01..=9.99).contains(&self.gamma)
    }

    pub fn normalized(&self) -> Self {
        ExposureSettings {
            exposure: finite_clamp(self.exposure, Self::EXPOSURE_RANGE, 0.0),
            offset: finite_clamp(self.offset, Self::OFFSET_RANGE, 0.0),
            gamma: finite_clamp(self.gamma, Self::GAMMA_RANGE, 1.0),
        }
    }
}

/// Gradient Map: each pixel's brightness picks a color between `shadows` and `highlights` (the
/// other way round when reversed); alpha is kept.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GradientMapSettings {
    pub shadows: AdjustmentColor,
    pub highlights: AdjustmentColor,
    pub reversed: bool,
}

impl Default for GradientMapSettings {
    fn default() -> Self {
        GradientMapSettings {
            shadows: AdjustmentColor::new(0.0, 0.0, 0.0),
            highlights: AdjustmentColor::new(1.0, 1.0, 1.0),
            reversed: false,
        }
    }
}

impl GradientMapSettings {
    pub fn is_valid(&self) -> bool {
        self.shadows.is_valid() && self.highlights.is_valid()
    }

    pub fn normalized(&self) -> Self {
        GradientMapSettings {
            shadows: self.shadows.clamped(),
            highlights: self.highlights.clamped(),
            reversed: self.reversed,
        }
    }

    /// The colors for the darkest and lightest tones, in the order they apply.
    pub fn ends(&self) -> (AdjustmentColor, AdjustmentColor) {
        if self.reversed {
            (self.highlights, self.shadows)
        } else {
            (self.shadows, self.highlights)
        }
    }
}

/// Black & White, as Photoshop's is: not a desaturation, but a choice of how bright each family of
/// colors becomes in gray. Reds at 40% and yellows at 60% is why a default conversion keeps skin
/// and foliage apart where a plain luminance flattens them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlackWhiteSettings {
    /// Photoshop's defaults.
    pub reds: f64,
    pub yellows: f64,
    pub greens: f64,
    pub cyans: f64,
    pub blues: f64,
    pub magentas: f64,
    /// Color the result while keeping its tones, for a sepia or a cyanotype.
    pub tint: bool,
    pub tint_hue: f64,
    pub tint_saturation: f64,
}

impl Default for BlackWhiteSettings {
    fn default() -> Self {
        BlackWhiteSettings {
            reds: 40.0,
            yellows: 60.0,
            greens: 40.0,
            cyans: 60.0,
            blues: 20.0,
            magentas: 80.0,
            tint: false,
            tint_hue: 40.0,
            tint_saturation: 20.0,
        }
    }
}

impl BlackWhiteSettings {
    pub const RANGE: (f64, f64) = (-200.0, 300.0);

    pub fn is_valid(&self) -> bool {
        [self.reds, self.yellows, self.greens, self.cyans, self.blues, self.magentas]
            .iter()
            .all(|weight| weight.is_finite() && (-200.0..=300.0).contains(weight))
            && self.tint_hue.is_finite()
            && (0.0..=360.0).contains(&self.tint_hue)
            && self.tint_saturation.is_finite()
            && (0.0..=100.0).contains(&self.tint_saturation)
    }
}

/// Color Balance: shifts color towards one end of each opposing pair, separately for shadows,
/// midtones and highlights. Preserve Luminosity puts each pixel's brightness back afterwards, so a
/// warm cast doesn't also lighten the picture.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ColorBalanceSettings {
    pub shadow_cyan_red: f64,
    pub shadow_magenta_green: f64,
    pub shadow_yellow_blue: f64,
    pub mid_cyan_red: f64,
    pub mid_magenta_green: f64,
    pub mid_yellow_blue: f64,
    pub highlight_cyan_red: f64,
    pub highlight_magenta_green: f64,
    pub highlight_yellow_blue: f64,
    pub preserve_luminosity: bool,
}

impl Default for ColorBalanceSettings {
    fn default() -> Self {
        ColorBalanceSettings {
            shadow_cyan_red: 0.0,
            shadow_magenta_green: 0.0,
            shadow_yellow_blue: 0.0,
            mid_cyan_red: 0.0,
            mid_magenta_green: 0.0,
            mid_yellow_blue: 0.0,
            highlight_cyan_red: 0.0,
            highlight_magenta_green: 0.0,
            highlight_yellow_blue: 0.0,
            preserve_luminosity: true,
        }
    }
}

impl ColorBalanceSettings {
    pub const RANGE: (f64, f64) = (-100.0, 100.0);

    /// The nine amounts, in the order the settings declare them.
    fn all(&self) -> [f64; 9] {
        [
            self.shadow_cyan_red,
            self.shadow_magenta_green,
            self.shadow_yellow_blue,
            self.mid_cyan_red,
            self.mid_magenta_green,
            self.mid_yellow_blue,
            self.highlight_cyan_red,
            self.highlight_magenta_green,
            self.highlight_yellow_blue,
        ]
    }

    pub fn is_valid(&self) -> bool {
        self.all()
            .iter()
            .all(|amount| amount.is_finite() && (-100.0..=100.0).contains(amount))
    }

    pub fn is_identity(&self) -> bool {
        self.all().iter().all(|amount| *amount == 0.0)
    }
}

/// Film grain: brightness noise, strongest in the midtones. Its pattern is fixed in document space
/// by `seed`, so it stays put as the canvas pans or redraws part of the image.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GrainSettings {
    /// Strength, 0–100.
    pub amount: f64,
    /// Grain scale in document pixels, 0.5–20.
    pub size: f64,
    /// 0–100: how much smaller, irregular detail roughens the main grain particles.
    pub roughness: f64,
    pub seed: u32,
}

impl Default for GrainSettings {
    fn default() -> Self {
        GrainSettings {
            amount: 25.0,
            size: 1.5,
            roughness: 50.0,
            seed: 0,
        }
    }
}

impl GrainSettings {
    pub const AMOUNT_RANGE: (f64, f64) = (0.0, 100.0);
    pub const SIZE_RANGE: (f64, f64) = (0.5, 20.0);
    pub const ROUGHNESS_RANGE: (f64, f64) = (0.0, 100.0);

    pub fn is_valid(&self) -> bool {
        self.amount.is_finite()
            && (0.0..=100.0).contains(&self.amount)
            && self.size.is_finite()
            && (0.5..=20.0).contains(&self.size)
            && self.roughness.is_finite()
            && (0.0..=100.0).contains(&self.roughness)
    }

    pub fn normalized(&self) -> Self {
        GrainSettings {
            amount: finite_clamp(self.amount, Self::AMOUNT_RANGE, 25.0),
            size: finite_clamp(self.size, Self::SIZE_RANGE, 1.5),
            roughness: finite_clamp(self.roughness, Self::ROUGHNESS_RANGE, 50.0),
            seed: self.seed,
        }
    }
}

/// One adjustment layer's record: the kind and the settings of every kind, exactly as the manifest
/// stores them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerAdjustment {
    pub kind: AdjustmentKind,
    #[serde(default)]
    pub hue: f64,
    #[serde(default)]
    pub saturation: f64,
    #[serde(default)]
    pub lightness: f64,
    #[serde(default)]
    pub colorize: bool,
    /// Optional so projects saved before range-aware HSV adjustments still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hsv_settings: Option<HueSaturationSettings>,
    #[serde(default)]
    pub levels: LevelsSettings,
    #[serde(default)]
    pub curves: CurvesSettings,
    /// Optional so projects saved before these adjustments existed decode, and save, exactly as
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_settings: Option<ExposureSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradient_map_settings: Option<GradientMapSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain_settings: Option<GrainSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub black_white_settings: Option<BlackWhiteSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_balance_settings: Option<ColorBalanceSettings>,
    /// Optional so projects created before blur adjustments continue to decode unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blur_radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_angle: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_distance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_amount: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_gaussian: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_monochromatic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_seed: Option<u32>,
}

impl Default for LayerAdjustment {
    /// The Swift type has no default initializer; this stands in for the `..Default::default()`
    /// constructions and fixtures the port makes, with the first kind as the placeholder.
    fn default() -> Self {
        LayerAdjustment::new(AdjustmentKind::Hsv)
    }
}

impl LayerAdjustment {
    /// `LayerAdjustment(kind:)`: a record of this kind with every setting at its default.
    pub fn new(kind: AdjustmentKind) -> Self {
        LayerAdjustment {
            kind,
            hue: 0.0,
            saturation: 0.0,
            lightness: 0.0,
            colorize: false,
            hsv_settings: None,
            levels: LevelsSettings::default(),
            curves: CurvesSettings::default(),
            exposure_settings: None,
            gradient_map_settings: None,
            grain_settings: None,
            black_white_settings: None,
            color_balance_settings: None,
            blur_radius: None,
            motion_angle: None,
            motion_distance: None,
            noise_amount: None,
            noise_gaussian: None,
            noise_monochromatic: None,
            noise_seed: None,
        }
    }

    /// The range-aware settings, or the legacy color values when the record predates them.
    pub fn resolved_hsv(&self) -> HueSaturationSettings {
        self.hsv_settings.clone().unwrap_or_else(|| {
            HueSaturationSettings::new(
                self.hue,
                self.saturation,
                self.lightness,
                self.colorize,
                ColorRange::Master,
            )
        })
    }

    pub fn exposure(&self) -> ExposureSettings {
        self.exposure_settings.unwrap_or_default()
    }

    pub fn gradient_map(&self) -> GradientMapSettings {
        self.gradient_map_settings.unwrap_or_default()
    }

    pub fn grain(&self) -> GrainSettings {
        self.grain_settings.unwrap_or_default()
    }

    pub fn black_white(&self) -> BlackWhiteSettings {
        self.black_white_settings.unwrap_or_default()
    }

    pub fn color_balance(&self) -> ColorBalanceSettings {
        self.color_balance_settings.unwrap_or_default()
    }

    /// The Gaussian blur's radius, 10 when the record predates it.
    pub fn gaussian_radius(&self) -> f64 {
        self.blur_radius.unwrap_or(10.0)
    }

    /// The stored angle, 0 when the record predates it.
    pub fn resolved_motion_angle(&self) -> f64 {
        self.motion_angle.unwrap_or(0.0)
    }

    /// The stored distance, 10 when the record predates it.
    pub fn resolved_motion_distance(&self) -> f64 {
        self.motion_distance.unwrap_or(10.0)
    }

    /// The stored amount, 10 when the record predates it.
    pub fn resolved_noise_amount(&self) -> f64 {
        self.noise_amount.unwrap_or(10.0)
    }

    /// The stored distribution, Uniform when the record predates it.
    pub fn resolved_noise_gaussian(&self) -> bool {
        self.noise_gaussian.unwrap_or(false)
    }

    /// The stored monochromatic flag, off when the record predates it.
    pub fn resolved_noise_monochromatic(&self) -> bool {
        self.noise_monochromatic.unwrap_or(false)
    }

    /// The stored seed, 0 when the record predates it.
    pub fn resolved_noise_seed(&self) -> u32 {
        self.noise_seed.unwrap_or(0)
    }

    /// Document-pixel halo needed so a partial canvas redraw can sample beyond its dirty rectangle.
    pub fn sampling_margin(&self) -> f64 {
        match self.kind {
            AdjustmentKind::GaussianBlur => self.gaussian_radius() * 3.0 + 2.0,
            AdjustmentKind::MotionBlur => self.resolved_motion_distance() / 2.0 + 2.0,
            _ => 0.0,
        }
    }

    /// Every stored value inside its slider's range, as `ProjectStore.validate` requires.
    pub fn is_valid(&self) -> bool {
        let hsv = self.resolved_hsv();
        self.hue.is_finite()
            && self.saturation.is_finite()
            && self.lightness.is_finite()
            && self.hue.abs() <= 360.0
            && self.saturation.abs() <= 100.0
            && self.lightness.abs() <= 100.0
            && hsv.adjustments.values().all(|adjustment| {
                adjustment.hue.is_finite()
                    && adjustment.hue.abs() <= 360.0
                    && adjustment.saturation.is_finite()
                    && adjustment.saturation.abs() <= 100.0
                    && adjustment.lightness.is_finite()
                    && adjustment.lightness.abs() <= 100.0
            })
            && hsv
                .bands
                .values()
                .all(|band| band.handles().iter().all(|handle| handle.is_finite()))
            && self.levels.ranges.len() == 4
            && self.levels.ranges.iter().all(|range| *range == range.normalized())
            && self.curves.is_valid()
            && self.exposure().is_valid()
            && self.gradient_map().is_valid()
            && self.grain().is_valid()
            && self.black_white().is_valid()
            && self.color_balance().is_valid()
            && self.gaussian_radius().is_finite()
            && (0.1..=250.0).contains(&self.gaussian_radius())
            && self.resolved_motion_angle().is_finite()
            && (-90.0..=90.0).contains(&self.resolved_motion_angle())
            && self.resolved_motion_distance().is_finite()
            && (1.0..=2000.0).contains(&self.resolved_motion_distance())
            && self.resolved_noise_amount().is_finite()
            && (0.1..=400.0).contains(&self.resolved_noise_amount())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjustment_kinds_keep_their_raw_values_and_order() {
        let values: Vec<&str> = AdjustmentKind::ALL.iter().map(|kind| kind.raw_value()).collect();
        assert_eq!(
            values,
            vec![
                "Hue/Saturation",
                "Levels",
                "Curves",
                "Exposure",
                "Gradient Map",
                "Grain",
                "Add Noise",
                "Gaussian Blur",
                "Motion Blur",
                "Invert",
                "Black & White",
                "Color Balance",
            ]
        );
        for kind in AdjustmentKind::ALL {
            assert_eq!(AdjustmentKind::from_raw(kind.raw_value()), Some(kind));
        }
        assert_eq!(AdjustmentKind::from_raw("Nope"), None);
        assert_eq!(serde_json::to_string(&AdjustmentKind::GradientMap).unwrap(), "\"Gradient Map\"");
    }

    #[test]
    fn invert_is_the_only_kind_without_an_editor_or_a_filter_kind() {
        assert!(!AdjustmentKind::Invert.is_editable());
        for kind in AdjustmentKind::ALL {
            if kind != AdjustmentKind::Invert {
                assert!(kind.is_editable(), "{} is editable", kind.raw_value());
            }
        }
        assert_eq!(AdjustmentKind::Hsv.filter_kind(), None);
        assert_eq!(AdjustmentKind::Levels.filter_kind(), None);
        assert_eq!(AdjustmentKind::Invert.filter_kind(), None);
        assert_eq!(AdjustmentKind::GradientMap.filter_kind(), Some(FilterKind::GradientMap));
    }

    /// An existing kind saves exactly as before: the newer settings stay absent, and the record
    /// round-trips.
    #[test]
    fn settings_save_and_older_adjustments_still_open() {
        let levels = LayerAdjustment::new(AdjustmentKind::Levels);
        let json = serde_json::to_string(&levels).unwrap();
        assert!(!json.contains("exposureSettings"), "an existing kind saves exactly as before");
        assert!(!json.contains("gradientMapSettings"));
        assert!(!json.contains("grainSettings"));
        assert!(!json.contains("blackWhiteSettings"));
        assert!(!json.contains("colorBalanceSettings"));
        assert!(!json.contains("hsvSettings"));
        assert!(!json.contains("blurRadius"));
        let decoded: LayerAdjustment = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, levels);

        let mut grain = LayerAdjustment::new(AdjustmentKind::Grain);
        grain.grain_settings = Some(GrainSettings {
            amount: 40.0,
            size: 3.0,
            roughness: 10.0,
            seed: 9,
        });
        let decoded: LayerAdjustment =
            serde_json::from_str(&serde_json::to_string(&grain).unwrap()).unwrap();
        assert_eq!(decoded, grain);
        assert!(decoded.is_valid());

        let mut broken = LayerAdjustment::new(AdjustmentKind::Exposure);
        broken.exposure_settings = Some(ExposureSettings { gamma: 0.0, ..ExposureSettings::default() });
        assert!(!broken.is_valid());
    }

    /// A legacy Hue/Saturation record has color values of its own and no range-aware settings.
    #[test]
    fn legacy_hsv_still_decodes_and_renders_identity_settings() {
        let mut legacy = LayerAdjustment::new(AdjustmentKind::Hsv);
        legacy.hue = 120.0;
        let decoded: LayerAdjustment =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(decoded.hsv_settings, None);
        assert_eq!(decoded.resolved_hsv().hue(), 120.0);
    }

    /// Swift's `Dictionary` writes an alternating key/value array for a `ColorRange` key.
    #[test]
    fn hue_saturation_maps_encode_as_alternating_arrays() {
        let settings = HueSaturationSettings::new(30.0, 40.0, 0.0, false, ColorRange::Master);
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            value["adjustments"],
            serde_json::json!(["Master", { "hue": 30.0, "saturation": 40.0, "lightness": 0.0 }])
        );
        assert_eq!(value["bands"].as_array().unwrap().len(), ColorRange::ALL.len() * 2);
        let decoded: HueSaturationSettings = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, settings);
    }

    /// The Colorize switch starts Photoshop's 25% saturation, as the sheet's Reset does.
    #[test]
    fn colorize_starts_at_photoshop_values_and_is_not_identity() {
        let settings = HueSaturationSettings::colorize_start();
        assert!(settings.colorize);
        assert_eq!(settings.hue(), 0.0);
        assert_eq!(settings.saturation(), 25.0);
        assert_eq!(settings.lightness(), 0.0);
        assert!(!settings.is_identity());
        assert!(HueSaturationSettings::default().is_identity());
    }

    /// Ported from CompositorTests.HueSaturationTests.bandWeightsRampThroughFalloffAndWrapAround.
    #[test]
    fn band_weights_ramp_through_falloff_and_wrap_around() {
        let reds = ColorRange::Reds.default_band(); // 315 / 345 / 15 / 45, wrapping past 0.
        assert!(reds.weight(0.0) == 1.0 && reds.weight(345.0) == 1.0 && reds.weight(15.0) == 1.0);
        assert!((reds.weight(330.0) - 0.5).abs() < 0.001, "halfway up the shoulder: {}", reds.weight(330.0));
        assert!((reds.weight(30.0) - 0.5).abs() < 0.001, "halfway down the far shoulder: {}", reds.weight(30.0));
        assert!(reds.weight(315.0) == 0.0 && reds.weight(45.0) == 0.0 && reds.weight(180.0) == 0.0);
        assert_eq!(ColorRange::Master.default_band().weight(123.0), 1.0);

        // Handles keep their order: crossing moves are refused.
        let mut band = ColorRange::Greens.default_band();
        band.set_handle(1, 200.0); // range_start past range_end.
        assert_eq!(band, ColorRange::Greens.default_band());
        band.set_handle(1, 110.0);
        assert_eq!(band.range_start, 110.0);
    }

    /// The core half of CompositorTests.HueSaturationTests.slidersEditTheSelectedRangeAndTheAfterBarFollowsHueShifts;
    /// the "after" bar's shifted hue is a pixel kernel, covered in `compositor-pixels`.
    #[test]
    fn sliders_edit_the_selected_range() {
        let mut settings = HueSaturationSettings::default();
        settings.set_hue(30.0); // Master.
        settings.range = ColorRange::Greens;
        assert_eq!(settings.hue(), 0.0, "Greens start untouched");
        settings.set_hue(-40.0);
        assert_eq!(settings.adjustments[&ColorRange::Master].hue, 30.0);
        assert_eq!(settings.adjustments[&ColorRange::Greens].hue, -40.0);
        settings.range = ColorRange::Master;
        assert_eq!(settings.hue(), 30.0);
        assert!(!settings.is_identity());
    }
}
