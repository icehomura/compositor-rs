//! Camera Raw's develop controls: the Filter › Camera Raw panel's whole pipeline.
//!
//! Port of `Document/CameraRaw.swift` (`CameraRawSettings`, `CameraRawScope`), `Document/CameraRawColor.swift`
//! (parametric/point curves, the Color Mixer, Color Grading), `Document/CameraRawDetailOptics.swift`
//! (sharpening, manual noise reduction, lens optics) and `Document/CameraRawGeometryCalibration.swift`
//! (upright/projection geometry and the calibration sliders).
//!
//! The kernels themselves live in [`crate::adjust_pixels`], ported from the C; this module holds the settings
//! types, their ranges and normalization, and the application order the Swift chose: geometry and optics
//! corrections, calibration, the Light/Color grade, curve/mixer/grading, effects, then detail.
//!
//! One substitution: the geometry group's perspective warp ran on `CIPerspectiveTransform`, a Core Image
//! filter. [`perspective_warp`] reproduces it — the same four destination corners and inverse projective
//! mapping, bilinear in premultiplied space, transparent outside the source. `PixelAdjust.render` is
//! already the canonical buffer here, so no Core Image render step remains.

use compositor_core::buffer::Rgba8Image;
use compositor_core::error::{CoreError, Result};
use compositor_core::geom::{Point, Rect};
use compositor_core::image_ops::FilterJob;

use crate::adjust_pixels::{
    adjust_camera_raw, adjust_camera_raw_calibration, adjust_camera_raw_clip_overlay,
    adjust_camera_raw_curve_color, adjust_camera_raw_detail, adjust_camera_raw_effects,
    adjust_camera_raw_optics, adjust_camera_raw_sharpen_mask_overlay, adjust_grain,
};
use crate::brush_pixels::brush_alpha_bounds;
use crate::filters::{CameraRawInputs, PixelFilter, LENS_STRENGTH};
use crate::levels_pixels::levels_histogram;

/// `ProjectError.invalid`'s message, thrown when the sliders hold something the kernel cannot use.
fn invalid_settings() -> CoreError {
    CoreError::Message(
        "This is not a valid Compositor project, or its metadata is damaged.".to_string(),
    )
}

// MARK: - Develop sheet settings

/// Kelvin the develop sliders start at, and the value the camera's own balance stands for.
pub const DEFAULT_TEMPERATURE: f32 = 5000.0;

/// `RawDevelopSettings` (`IO/RawImporter.swift`): the develop sheet's controls, before the grade
/// turns them into [`CameraRawSettings`].
///
/// The original drove `CIRAWFilter`, whose controls are Kelvin and a "boost" amount. The grade here
/// works on already-decoded pixels and its temperature/tint are *relative offsets*, not Kelvin (see
/// [`CameraRawWhiteBalance`]); the mapping between the two lives in
/// `compositor_io::raw_importer::camera_settings`. The type lives here because the session, which
/// sits below `compositor-io`, holds it while the sheet is up.
///
/// What a camera recorded, before anyone decided how it should look. The file holds one value per
/// photosite at 12–14 bits; every choice a JPEG has already baked in — exposure, white balance,
/// contrast — is still open. Compositor's layers are 8-bit, so that latitude has to be spent at
/// import: these are the controls for spending it deliberately rather than accepting a default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawDevelopSettings {
    /// Stops of exposure, either side of what the camera recorded.
    pub exposure: f32,
    /// White balance in Kelvin, starting from the camera's own reading.
    pub temperature: f32,
    /// Green–magenta balance, starting from the camera's own reading.
    pub tint: f32,
    /// Apple's tone curve: 1 is its full interpretation, 0 leaves the image flat and neutral.
    pub boost: f32,
    /// What the camera itself chose, so Reset has somewhere to go back to.
    pub as_shot_temperature: f32,
    pub as_shot_tint: f32,
}

impl Default for RawDevelopSettings {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            temperature: DEFAULT_TEMPERATURE,
            tint: 0.0,
            boost: 1.0,
            as_shot_temperature: DEFAULT_TEMPERATURE,
            as_shot_tint: 0.0,
        }
    }
}

impl RawDevelopSettings {
    pub fn is_as_shot(&self) -> bool {
        self.exposure == 0.0
            && self.boost == 1.0
            && self.temperature == self.as_shot_temperature
            && self.tint == self.as_shot_tint
    }

    pub fn reset(&mut self) {
        self.exposure = 0.0;
        self.boost = 1.0;
        self.temperature = self.as_shot_temperature;
        self.tint = self.as_shot_tint;
    }

    /// `RawDevelopSettings(temperature:tint:asShotTemperature:asShotTint:)` — the camera's own
    /// reading, from which the sliders move.
    pub fn as_shot(temperature: f32, tint: f32) -> Self {
        Self {
            exposure: 0.0,
            temperature,
            tint,
            boost: 1.0,
            as_shot_temperature: temperature,
            as_shot_tint: tint,
        }
    }
}

// MARK: - Enums

/// White balance on an already-rendered layer. Raw lighting presets are absent: temperature and tint
/// are relative offsets, not kelvin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawWhiteBalance {
    #[default]
    Custom,
    Auto,
}

impl CameraRawWhiteBalance {
    pub const ALL: [CameraRawWhiteBalance; 2] =
        [CameraRawWhiteBalance::Custom, CameraRawWhiteBalance::Auto];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawWhiteBalance::Custom => "Custom",
            CameraRawWhiteBalance::Auto => "Auto",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// Glow's three looks. Warmth tints Diffusion and Bloom from cool to warm; Halation's fringe stays red.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawGlowStyle {
    #[default]
    Diffusion,
    Bloom,
    Halation,
}

impl CameraRawGlowStyle {
    pub const ALL: [CameraRawGlowStyle; 3] = [
        CameraRawGlowStyle::Diffusion,
        CameraRawGlowStyle::Bloom,
        CameraRawGlowStyle::Halation,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawGlowStyle::Diffusion => "Diffusion",
            CameraRawGlowStyle::Bloom => "Bloom",
            CameraRawGlowStyle::Halation => "Halation",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.raw_value() == value)
    }

    pub fn kernel_value(self) -> i32 {
        match self {
            CameraRawGlowStyle::Diffusion => 0,
            CameraRawGlowStyle::Bloom => 1,
            CameraRawGlowStyle::Halation => 2,
        }
    }
}

/// Post-crop vignette. Highlight Priority is the style whose Highlights slider protects bright pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawVignetteStyle {
    #[default]
    HighlightPriority,
    ColorPriority,
    PaintOverlay,
}

impl CameraRawVignetteStyle {
    pub const ALL: [CameraRawVignetteStyle; 3] = [
        CameraRawVignetteStyle::HighlightPriority,
        CameraRawVignetteStyle::ColorPriority,
        CameraRawVignetteStyle::PaintOverlay,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawVignetteStyle::HighlightPriority => "Highlight Priority",
            CameraRawVignetteStyle::ColorPriority => "Color Priority",
            CameraRawVignetteStyle::PaintOverlay => "Paint Overlay",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.raw_value() == value)
    }

    pub fn kernel_value(self) -> i32 {
        match self {
            CameraRawVignetteStyle::HighlightPriority => 0,
            CameraRawVignetteStyle::ColorPriority => 1,
            CameraRawVignetteStyle::PaintOverlay => 2,
        }
    }
}

/// Temporary clipping view while Option is held on a Light slider. Never written into the layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CameraRawClipping {
    /// Clipped channels lit on black. Exposure, Highlights, and Whites.
    Highlights = 1,
    /// Clipped channels dark on white. Shadows and Blacks.
    Shadows = 2,
}

impl CameraRawClipping {
    /// The value `FilterJob::camera_raw_clipping` carries; 0 stands for no clipping view.
    pub fn raw_value(self) -> i32 {
        self as i32
    }

    pub fn from_raw_value(value: i32) -> Option<Self> {
        match value {
            1 => Some(CameraRawClipping::Highlights),
            2 => Some(CameraRawClipping::Shadows),
            _ => None,
        }
    }
}

/// Histogram or the vectorscope shown in its place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawScopeMode {
    #[default]
    Histogram,
    Vectorscope,
}

impl CameraRawScopeMode {
    pub const ALL: [CameraRawScopeMode; 2] =
        [CameraRawScopeMode::Histogram, CameraRawScopeMode::Vectorscope];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawScopeMode::Histogram => "Histogram",
            CameraRawScopeMode::Vectorscope => "Vectorscope",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// The two halves of the Curve panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawCurvePage {
    #[default]
    Parametric,
    Point,
}

impl CameraRawCurvePage {
    pub const ALL: [CameraRawCurvePage; 2] =
        [CameraRawCurvePage::Parametric, CameraRawCurvePage::Point];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawCurvePage::Parametric => "Parametric",
            CameraRawCurvePage::Point => "Point",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|page| page.raw_value() == value)
    }
}

/// The channel a point curve belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawPointChannel {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
}

impl CameraRawPointChannel {
    pub const ALL: [CameraRawPointChannel; 4] = [
        CameraRawPointChannel::Rgb,
        CameraRawPointChannel::Red,
        CameraRawPointChannel::Green,
        CameraRawPointChannel::Blue,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawPointChannel::Rgb => "RGB",
            CameraRawPointChannel::Red => "Red",
            CameraRawPointChannel::Green => "Green",
            CameraRawPointChannel::Blue => "Blue",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|channel| channel.raw_value() == value)
    }
}

/// The three pages of the Color Mixer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawMixerPage {
    #[default]
    Hsl,
    Color,
    Point,
}

impl CameraRawMixerPage {
    pub const ALL: [CameraRawMixerPage; 3] = [
        CameraRawMixerPage::Hsl,
        CameraRawMixerPage::Color,
        CameraRawMixerPage::Point,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawMixerPage::Hsl => "HSL",
            CameraRawMixerPage::Color => "Color",
            CameraRawMixerPage::Point => "Point Color",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|page| page.raw_value() == value)
    }
}

/// Which of the mixer's three shifts a drag writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawMixerTab {
    #[default]
    Hue,
    Saturation,
    Luminance,
}

impl CameraRawMixerTab {
    pub const ALL: [CameraRawMixerTab; 3] = [
        CameraRawMixerTab::Hue,
        CameraRawMixerTab::Saturation,
        CameraRawMixerTab::Luminance,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawMixerTab::Hue => "Hue",
            CameraRawMixerTab::Saturation => "Saturation",
            CameraRawMixerTab::Luminance => "Luminance",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tab| tab.raw_value() == value)
    }
}

/// The Color Grading panel's pages: the three-way view opens the tonal wheels, the rest one at a time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawGradePage {
    #[default]
    ThreeWay,
    Shadows,
    Midtones,
    Highlights,
    Global,
}

impl CameraRawGradePage {
    pub const ALL: [CameraRawGradePage; 5] = [
        CameraRawGradePage::ThreeWay,
        CameraRawGradePage::Shadows,
        CameraRawGradePage::Midtones,
        CameraRawGradePage::Highlights,
        CameraRawGradePage::Global,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawGradePage::ThreeWay => "Three-Way",
            CameraRawGradePage::Shadows => "Shadows",
            CameraRawGradePage::Midtones => "Midtones",
            CameraRawGradePage::Highlights => "Highlights",
            CameraRawGradePage::Global => "Global",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|page| page.raw_value() == value)
    }
}

// MARK: - Curves

/// A curve handle, 0…1 on both axes — the shape Camera Raw's own curves and the Color Mixer's
/// point colors use. (`CurvePoint` in the Swift; declared here because `compositor-core` cannot name
/// pixel-side curve storage.)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CurvePoint {
    pub x: f64,
    pub y: f64,
}

impl CurvePoint {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// `CurvesSettings.value(_:channel:)` on an arbitrary point list: shape-preserving cubic Hermite
/// interpolation, which avoids overshoot between handles. `points` must hold at least two entries
/// with increasing x and span 0…255; the result is clamped to 0…255.
fn hermite_value(points: &[CurvePoint], x: f64) -> f64 {
    let count = points.len();
    if count < 2 {
        return x;
    }
    let mut index = 0usize;
    for (position, point) in points.iter().enumerate() {
        if point.x <= x {
            index = position;
        }
    }
    let index = index.min(count - 2);
    let mut slopes = Vec::with_capacity(count - 1);
    for pair in points.windows(2) {
        slopes.push((pair[1].y - pair[0].y) / (pair[1].x - pair[0].x));
    }
    let slope = |j: usize| -> f64 {
        if j == 0 {
            return slopes[0];
        }
        if j == count - 1 {
            return slopes[count - 2];
        }
        if slopes[j - 1] * slopes[j] <= 0.0 {
            return 0.0;
        }
        2.0 / (1.0 / slopes[j - 1] + 1.0 / slopes[j])
    };
    let h = points[index + 1].x - points[index].x;
    let t = ((x - points[index].x) / h).clamp(0.0, 1.0);
    let y = (2.0 * t * t * t - 3.0 * t * t + 1.0) * points[index].y
        + (t * t * t - 2.0 * t * t + t) * h * slope(index)
        + (-2.0 * t * t * t + 3.0 * t * t) * points[index + 1].y
        + (t * t * t - t * t) * h * slope(index + 1);
    y.clamp(0.0, 255.0)
}

/// Which parametric region a tone belongs to, for the targeted adjustment tool. The Swift version
/// answered with a key path; the same choice comes back as an enum here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CameraRawCurveRegion {
    Shadows,
    Darks,
    Lights,
    Highlights,
}

/// Parametric regions and point curves. Amounts are −100…100. Curve points use 0…1 on both axes.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawCurveSettings {
    pub shadows: f64,
    pub darks: f64,
    pub lights: f64,
    pub highlights: f64,
    /// Dividers, 0…100, kept in order. They set where each parametric slider hands off to the next.
    pub shadow_split: f64,
    pub dark_split: f64,
    pub light_split: f64,
    pub rgb: Vec<CurvePoint>,
    pub red: Vec<CurvePoint>,
    pub green: Vec<CurvePoint>,
    pub blue: Vec<CurvePoint>,
    /// How much the composite curve also changes saturation. 0 keeps it to brightness.
    pub refine_saturation: f64,
}

impl Default for CameraRawCurveSettings {
    fn default() -> Self {
        Self {
            shadows: 0.0,
            darks: 0.0,
            lights: 0.0,
            highlights: 0.0,
            shadow_split: 25.0,
            dark_split: 50.0,
            light_split: 75.0,
            rgb: Self::linear(),
            red: Self::linear(),
            green: Self::linear(),
            blue: Self::linear(),
            refine_saturation: 0.0,
        }
    }
}

impl CameraRawCurveSettings {
    /// The straight curve, black to white.
    pub fn linear() -> Vec<CurvePoint> {
        vec![CurvePoint::new(0.0, 0.0), CurvePoint::new(1.0, 1.0)]
    }

    pub fn medium_contrast() -> Vec<CurvePoint> {
        vec![
            CurvePoint::new(0.0, 0.0),
            CurvePoint::new(0.25, 0.18),
            CurvePoint::new(0.75, 0.82),
            CurvePoint::new(1.0, 1.0),
        ]
    }

    pub fn strong_contrast() -> Vec<CurvePoint> {
        vec![
            CurvePoint::new(0.0, 0.0),
            CurvePoint::new(0.25, 0.10),
            CurvePoint::new(0.75, 0.90),
            CurvePoint::new(1.0, 1.0),
        ]
    }

    pub fn adjusts(&self) -> bool {
        self.shadows != 0.0
            || self.darks != 0.0
            || self.lights != 0.0
            || self.highlights != 0.0
            || self.refine_saturation != 0.0
            || !Self::is_linear(&self.rgb)
            || !Self::is_linear(&self.red)
            || !Self::is_linear(&self.green)
            || !Self::is_linear(&self.blue)
    }

    pub fn is_linear(points: &[CurvePoint]) -> bool {
        points.len() == 2
            && points[0].x == 0.0
            && points[0].y == 0.0
            && points[1].x == 1.0
            && points[1].y == 1.0
    }

    /// Camera Raw's parametric curve, matched to Photoshop's: Darks bends the whole range below the middle divider and
    /// Lights the whole range above it, Shadows and Highlights just the ranges past the outer dividers. Each bend is a
    /// gamma curve across its range, which keeps the curve rising however far the sliders go, and the result is run
    /// through the same smooth curve as Image › Curves so the halves meet without a corner. Dividers are percentages.
    pub fn parametric(&self, tone: f64) -> f64 {
        if self.shadows == 0.0 && self.darks == 0.0 && self.lights == 0.0 && self.highlights == 0.0 {
            return tone;
        }
        let anchors: Vec<CurvePoint> = (0..=32)
            .map(|index| {
                let x = index as f64 / 32.0;
                CurvePoint::new(
                    x,
                    Self::bend(
                        Self::bend(
                            x,
                            self.shadow_split / 100.0,
                            self.shadows,
                            self.light_split / 100.0,
                            self.highlights,
                        ),
                        self.dark_split / 100.0,
                        self.darks,
                        self.dark_split / 100.0,
                        self.lights,
                    ),
                )
            })
            .collect();
        self.point(tone, &anchors)
    }

    /// Bends the tones below `lower` by `low` and above `upper` by `high` (−100…100), leaving black, white and the
    /// dividers in place. Fitted to Photoshop: Darks −51 dips the curve about 0.1 at a quarter of the way up.
    fn bend(tone: f64, lower: f64, low: f64, upper: f64, high: f64) -> f64 {
        let strength = 1.66;
        if tone < lower && lower > 0.0 {
            return lower * (tone / lower).powf(2.0f64.powf(-low / 100.0 * strength));
        }
        if tone > upper && upper < 1.0 {
            let rest = 1.0 - upper;
            return 1.0 - rest * ((1.0 - tone) / rest).powf(2.0f64.powf(high / 100.0 * strength));
        }
        tone
    }

    pub fn tone_table(&self) -> Vec<f32> {
        (0..=255)
            .map(|value| self.point(self.parametric(value as f64 / 255.0), &self.rgb) as f32)
            .collect()
    }

    pub fn channel_table(&self, points: &[CurvePoint]) -> Vec<f32> {
        (0..=255)
            .map(|value| self.point(value as f64 / 255.0, points) as f32)
            .collect()
    }

    /// The Swift nudged `settings.cameraRaw.curve[keyPath: key]`; the same amount by region.
    pub fn amount(&self, region: CameraRawCurveRegion) -> f64 {
        match region {
            CameraRawCurveRegion::Shadows => self.shadows,
            CameraRawCurveRegion::Darks => self.darks,
            CameraRawCurveRegion::Lights => self.lights,
            CameraRawCurveRegion::Highlights => self.highlights,
        }
    }

    pub fn set_amount(&mut self, region: CameraRawCurveRegion, value: f64) {
        let value = value.clamp(-100.0, 100.0);
        match region {
            CameraRawCurveRegion::Shadows => self.shadows = value,
            CameraRawCurveRegion::Darks => self.darks = value,
            CameraRawCurveRegion::Lights => self.lights = value,
            CameraRawCurveRegion::Highlights => self.highlights = value,
        }
    }

    /// The region name a tone belongs to, for the targeted adjustment tool.
    pub fn region(&self, tone: f64) -> CameraRawCurveRegion {
        if tone < self.shadow_split / 100.0 {
            return CameraRawCurveRegion::Shadows;
        }
        if tone < self.dark_split / 100.0 {
            return CameraRawCurveRegion::Darks;
        }
        if tone < self.light_split / 100.0 {
            return CameraRawCurveRegion::Lights;
        }
        CameraRawCurveRegion::Highlights
    }

    pub fn nudged(&self, channel: CameraRawPointChannel, tone: f64, delta: f64) -> Self {
        let mut result = self.clone();
        let mut points = match channel {
            CameraRawPointChannel::Rgb => self.rgb.clone(),
            CameraRawPointChannel::Red => self.red.clone(),
            CameraRawPointChannel::Green => self.green.clone(),
            CameraRawPointChannel::Blue => self.blue.clone(),
        };
        // `min(by:)` keeps the earliest of equal distances; a plain `min_by` would keep the latest.
        let mut best: Option<usize> = None;
        for (index, point) in points.iter().enumerate() {
            let distance = (point.x - tone).abs();
            match best {
                Some(current) if distance >= (points[current].x - tone).abs() => {}
                _ => best = Some(index),
            }
        }
        if let Some(index) = best {
            points[index].y = (points[index].y + delta).clamp(0.0, 1.0);
        }
        match channel {
            CameraRawPointChannel::Rgb => result.rgb = points,
            CameraRawPointChannel::Red => result.red = points,
            CameraRawPointChannel::Green => result.green = points,
            CameraRawPointChannel::Blue => result.blue = points,
        }
        result
    }

    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.shadows = camera_clamp(self.shadows, -100.0, 100.0, 0.0);
        result.darks = camera_clamp(self.darks, -100.0, 100.0, 0.0);
        result.lights = camera_clamp(self.lights, -100.0, 100.0, 0.0);
        result.highlights = camera_clamp(self.highlights, -100.0, 100.0, 0.0);
        result.refine_saturation = camera_clamp(self.refine_saturation, -100.0, 100.0, 0.0);
        result.shadow_split = camera_clamp(self.shadow_split, 5.0, 90.0, 25.0);
        result.dark_split = camera_clamp(self.dark_split, result.shadow_split + 2.0, 95.0, 50.0);
        result.light_split = camera_clamp(self.light_split, result.dark_split + 2.0, 98.0, 75.0);
        result.rgb = Self::repair(&self.rgb);
        result.red = Self::repair(&self.red);
        result.green = Self::repair(&self.green);
        result.blue = Self::repair(&self.blue);
        result
    }

    /// `CurvesSettings.value` on a 0…1 point list: the curve's own 0…255 space and back.
    fn point(&self, x: f64, points: &[CurvePoint]) -> f64 {
        let scaled: Vec<CurvePoint> = points
            .iter()
            .map(|point| CurvePoint::new(point.x * 255.0, point.y * 255.0))
            .collect();
        if scaled.len() < 2 {
            return x;
        }
        hermite_value(&scaled, x * 255.0) / 255.0
    }

    fn repair(points: &[CurvePoint]) -> Vec<CurvePoint> {
        let mut sorted: Vec<CurvePoint> = points
            .iter()
            .filter(|point| point.x.is_finite() && point.y.is_finite())
            .cloned()
            .collect();
        sorted.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        if sorted.len() < 2 {
            return Self::linear();
        }
        let last = sorted.len() - 1;
        sorted[0] = CurvePoint::new(0.0, sorted[0].y.clamp(0.0, 1.0));
        sorted[last] = CurvePoint::new(1.0, sorted[last].y.clamp(0.0, 1.0));
        let mut kept: Vec<CurvePoint> = vec![sorted[0]];
        for point in &sorted[1..last] {
            let x = point.x.clamp(0.01, 0.99);
            if x <= kept[kept.len() - 1].x + 0.01 {
                continue;
            }
            kept.push(CurvePoint::new(x, point.y.clamp(0.0, 1.0)));
        }
        kept.push(sorted[last]);
        kept
    }
}

/// `ImageAdjustmentPixels.clamp`: a finite value inside the range, otherwise the fallback.
fn camera_clamp(value: f64, low: f64, high: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(low, high)
    } else {
        fallback
    }
}

// MARK: - Color Mixer

/// One picked color and how far its adjustment reaches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawPointColor {
    pub hue: f64,
    pub saturation: f64,
    pub luminance: f64,
    pub hue_shift: f64,
    pub saturation_shift: f64,
    pub luminance_shift: f64,
    pub hue_range: f64,
    pub saturation_range: f64,
    pub luminance_range: f64,
    pub visualize: bool,
}

impl Default for CameraRawPointColor {
    fn default() -> Self {
        Self {
            hue: 0.0,
            saturation: 0.0,
            luminance: 0.0,
            hue_shift: 0.0,
            saturation_shift: 0.0,
            luminance_shift: 0.0,
            hue_range: 30.0,
            saturation_range: 0.4,
            luminance_range: 0.4,
            visualize: false,
        }
    }
}

impl CameraRawPointColor {
    pub fn normalized(&self) -> Self {
        Self {
            hue: camera_clamp(self.hue, 0.0, 360.0, 0.0),
            saturation: camera_clamp(self.saturation, 0.0, 1.0, 0.0),
            luminance: camera_clamp(self.luminance, 0.0, 1.0, 0.0),
            hue_shift: camera_clamp(self.hue_shift, -100.0, 100.0, 0.0),
            saturation_shift: camera_clamp(self.saturation_shift, -100.0, 100.0, 0.0),
            luminance_shift: camera_clamp(self.luminance_shift, -100.0, 100.0, 0.0),
            hue_range: camera_clamp(self.hue_range, 5.0, 180.0, 30.0),
            saturation_range: camera_clamp(self.saturation_range, 0.05, 1.0, 0.4),
            luminance_range: camera_clamp(self.luminance_range, 0.05, 1.0, 0.4),
            visualize: self.visualize,
        }
    }
}

/// Eight color families, each with hue, saturation, and luminance shifts of −100…100.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawMixerSettings {
    pub hue: [f64; 8],
    pub saturation: [f64; 8],
    pub luminance: [f64; 8],
    pub points: Vec<CameraRawPointColor>,
}

impl Default for CameraRawMixerSettings {
    fn default() -> Self {
        Self {
            hue: [0.0; 8],
            saturation: [0.0; 8],
            luminance: [0.0; 8],
            points: Vec::new(),
        }
    }
}

impl CameraRawMixerSettings {
    pub fn names() -> [&'static str; 8] {
        [
            "Reds", "Oranges", "Yellows", "Greens", "Aquas", "Blues", "Purples", "Magentas",
        ]
    }

    pub fn centers() -> [f64; 8] {
        [0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0]
    }

    pub fn adjusts(&self) -> bool {
        self.hue.iter().any(|value| *value != 0.0)
            || self.saturation.iter().any(|value| *value != 0.0)
            || self.luminance.iter().any(|value| *value != 0.0)
            || self
                .points
                .iter()
                .any(|point| point.hue_shift != 0.0 || point.saturation_shift != 0.0 || point.luminance_shift != 0.0)
    }

    /// How much each family shares a hue, in degrees. Neighbors overlap.
    pub fn weights(for_hue: f64) -> [f64; 8] {
        Self::centers().map(|center| {
            let mut distance = (for_hue - center).abs();
            if distance > 180.0 {
                distance = 360.0 - distance;
            }
            (1.0 - distance / 40.0).max(0.0)
        })
    }

    pub fn mixer_floats(&self) -> Vec<f32> {
        self.hue
            .iter()
            .chain(self.saturation.iter())
            .chain(self.luminance.iter())
            .map(|value| (value / 100.0) as f32)
            .collect()
    }

    pub fn point_floats(&self) -> Vec<f32> {
        self.points
            .iter()
            .flat_map(|point| {
                [
                    point.hue / 360.0,
                    point.saturation,
                    point.luminance,
                    point.hue_shift / 100.0,
                    point.saturation_shift / 100.0,
                    point.luminance_shift / 100.0,
                    point.hue_range / 360.0,
                    point.saturation_range,
                    point.luminance_range,
                ]
                .map(|value| value as f32)
            })
            .collect()
    }

    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.hue = self.hue.map(|value| camera_clamp(value, -100.0, 100.0, 0.0));
        result.saturation = self.saturation.map(|value| camera_clamp(value, -100.0, 100.0, 0.0));
        result.luminance = self.luminance.map(|value| camera_clamp(value, -100.0, 100.0, 0.0));
        result.points = self
            .points
            .iter()
            .take(8)
            .map(|point| point.normalized())
            .collect();
        result
    }
}

// MARK: - Color Grading

/// One color wheel: hue turns, saturation 0…100, luminance −100…100.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraRawGradeWheel {
    pub hue: f64,
    pub saturation: f64,
    pub luminance: f64,
}

impl CameraRawGradeWheel {
    pub fn normalized(&self) -> Self {
        Self {
            hue: camera_clamp(self.hue, 0.0, 360.0, 0.0),
            saturation: camera_clamp(self.saturation, 0.0, 100.0, 0.0),
            luminance: camera_clamp(self.luminance, -100.0, 100.0, 0.0),
        }
    }
}

/// Four color wheels plus how the three tonal wheels overlap and which end they favor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawGradingSettings {
    pub shadows: CameraRawGradeWheel,
    pub midtones: CameraRawGradeWheel,
    pub highlights: CameraRawGradeWheel,
    pub global: CameraRawGradeWheel,
    /// 0…100. Higher values let the three tonal wheels overlap more.
    pub blending: f64,
    /// −100…100. Negative favors shadows, positive favors highlights.
    pub balance: f64,
}

impl Default for CameraRawGradingSettings {
    fn default() -> Self {
        Self {
            shadows: CameraRawGradeWheel::default(),
            midtones: CameraRawGradeWheel::default(),
            highlights: CameraRawGradeWheel::default(),
            global: CameraRawGradeWheel::default(),
            blending: 50.0,
            balance: 0.0,
        }
    }
}

impl CameraRawGradingSettings {
    pub fn wheels(&self) -> [CameraRawGradeWheel; 4] {
        [self.shadows, self.midtones, self.highlights, self.global]
    }

    pub fn adjusts(&self) -> bool {
        self.wheels()
            .iter()
            .any(|wheel| wheel.saturation != 0.0 || wheel.luminance != 0.0)
    }

    pub fn grade_floats(&self) -> Vec<f32> {
        self.wheels()
            .iter()
            .flat_map(|wheel| {
                [
                    (wheel.hue / 360.0) as f32,
                    (wheel.saturation / 100.0) as f32,
                    (wheel.luminance / 100.0) as f32,
                ]
            })
            .collect()
    }

    pub fn normalized(&self) -> Self {
        let mut result = *self;
        result.shadows = self.shadows.normalized();
        result.midtones = self.midtones.normalized();
        result.highlights = self.highlights.normalized();
        result.global = self.global.normalized();
        result.blending = camera_clamp(self.blending, 0.0, 100.0, 50.0);
        result.balance = camera_clamp(self.balance, -100.0, 100.0, 0.0);
        result
    }
}

// MARK: - Detail

/// Sharpening and manual noise reduction. Amount is 0…150; the rest use Camera Raw's usual 0…100 ranges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawDetailSettings {
    pub sharpen_amount: f64,
    pub sharpen_radius: f64,
    pub sharpen_detail: f64,
    pub sharpen_masking: f64,
    pub noise_luminance: f64,
    pub noise_luminance_detail: f64,
    pub noise_luminance_contrast: f64,
    pub noise_color: f64,
    pub noise_color_detail: f64,
    pub noise_color_smoothness: f64,
}

impl Default for CameraRawDetailSettings {
    fn default() -> Self {
        Self {
            sharpen_amount: 0.0,
            sharpen_radius: 10.0,
            sharpen_detail: 25.0,
            sharpen_masking: 0.0,
            noise_luminance: 0.0,
            noise_luminance_detail: 50.0,
            noise_luminance_contrast: 0.0,
            noise_color: 0.0,
            noise_color_detail: 50.0,
            noise_color_smoothness: 50.0,
        }
    }
}

impl CameraRawDetailSettings {
    pub const SHARPEN_AMOUNT_RANGE: (f64, f64) = (0.0, 150.0);
    pub const UNIT_RANGE: (f64, f64) = (0.0, 100.0);

    pub fn adjusts_sharpening(&self) -> bool {
        self.sharpen_amount != 0.0
    }

    pub fn adjusts_noise(&self) -> bool {
        self.noise_luminance != 0.0 || self.noise_color != 0.0
    }

    pub fn adjusts(&self) -> bool {
        self.adjusts_sharpening() || self.adjusts_noise()
    }

    pub fn normalized(&self) -> Self {
        let mut result = *self;
        result.sharpen_amount =
            camera_clamp(self.sharpen_amount, Self::SHARPEN_AMOUNT_RANGE.0, Self::SHARPEN_AMOUNT_RANGE.1, 0.0);
        result.sharpen_radius = camera_clamp(self.sharpen_radius, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 10.0);
        result.sharpen_detail = camera_clamp(self.sharpen_detail, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 25.0);
        result.sharpen_masking = camera_clamp(self.sharpen_masking, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 0.0);
        result.noise_luminance = camera_clamp(self.noise_luminance, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 0.0);
        result.noise_luminance_detail =
            camera_clamp(self.noise_luminance_detail, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 50.0);
        result.noise_luminance_contrast =
            camera_clamp(self.noise_luminance_contrast, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 0.0);
        result.noise_color = camera_clamp(self.noise_color, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 0.0);
        result.noise_color_detail =
            camera_clamp(self.noise_color_detail, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 50.0);
        result.noise_color_smoothness =
            camera_clamp(self.noise_color_smoothness, Self::UNIT_RANGE.0, Self::UNIT_RANGE.1, 50.0);
        result
    }

    /// The panel's eye turned off: the whole group returns to its defaults.
    pub fn applying(&self, shows: bool) -> Self {
        if shows {
            *self
        } else {
            Self::default()
        }
    }
}

// MARK: - Settings

/// Camera Raw Filter settings. Defaults leave the image unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawSettings {
    pub white_balance: CameraRawWhiteBalance,
    /// Relative cool-to-warm, −100…100. Positive is warmer.
    pub temperature: f64,
    /// Green-to-magenta, −100…100. Positive is magenta.
    pub tint: f64,
    /// Stops of linear light, −5…5.
    pub exposure: f64,
    pub contrast: f64,
    pub highlights: f64,
    pub shadows: f64,
    pub whites: f64,
    pub blacks: f64,
    pub vibrance: f64,
    pub saturation: f64,
    /// Local contrast, −100…100. Texture is the finer band; Clarity is the broader one.
    pub texture: f64,
    pub clarity: f64,
    /// −100…100. Positive deepens contrast and saturation; negative lifts shadows and fades color.
    pub dehaze: f64,
    /// 0…100. Range, spread, and warmth are idle while this stays at zero.
    pub glow: f64,
    pub glow_style: CameraRawGlowStyle,
    pub glow_range: f64,
    pub glow_spread: f64,
    pub glow_warmth: f64,
    /// −100…100. Negative darkens the edges, positive lightens them. The center is left alone.
    pub vignette_amount: f64,
    pub vignette_style: CameraRawVignetteStyle,
    pub vignette_midpoint: f64,
    pub vignette_roundness: f64,
    pub vignette_feather: f64,
    /// Used only while `vignette_amount` darkens, and only for Highlight Priority.
    pub vignette_highlights: f64,
    /// 0…100. Zero adds no grain. Size is mapped into the shared grain kernel's pixel scale.
    pub grain_amount: f64,
    pub grain_size: f64,
    pub grain_roughness: f64,
    pub curve: CameraRawCurveSettings,
    pub mixer: CameraRawMixerSettings,
    pub grading: CameraRawGradingSettings,
    pub detail: CameraRawDetailSettings,
    pub optics: CameraRawOpticsSettings,
    pub geometry: CameraRawGeometrySettings,
    pub calibration: CameraRawCalibrationSettings,
}

impl Default for CameraRawSettings {
    fn default() -> Self {
        Self {
            white_balance: CameraRawWhiteBalance::Custom,
            temperature: 0.0,
            tint: 0.0,
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            texture: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            glow: 0.0,
            glow_style: CameraRawGlowStyle::Diffusion,
            glow_range: 0.0,
            glow_spread: 0.0,
            glow_warmth: 0.0,
            vignette_amount: 0.0,
            vignette_style: CameraRawVignetteStyle::HighlightPriority,
            vignette_midpoint: 50.0,
            vignette_roundness: 0.0,
            vignette_feather: 50.0,
            vignette_highlights: 0.0,
            grain_amount: 0.0,
            grain_size: 25.0,
            grain_roughness: 50.0,
            curve: CameraRawCurveSettings::default(),
            mixer: CameraRawMixerSettings::default(),
            grading: CameraRawGradingSettings::default(),
            detail: CameraRawDetailSettings::default(),
            optics: CameraRawOpticsSettings::default(),
            geometry: CameraRawGeometrySettings::default(),
            calibration: CameraRawCalibrationSettings::default(),
        }
    }
}

impl CameraRawSettings {
    pub const EXPOSURE_RANGE: (f64, f64) = (-5.0, 5.0);
    pub const TONE_RANGE: (f64, f64) = (-100.0, 100.0);
    pub const UNIT_RANGE: (f64, f64) = (0.0, 100.0);
    /// Share of a full warm/cool swing applied to red and blue. Kept here so the eyedropper inverts the same gains the kernel multiplies.
    pub const TEMPERATURE_GAIN: f64 = 0.35;
    /// Magenta/green swing shared by red and blue.
    pub const TINT_RED_BLUE: f64 = 0.15;
    /// Magenta/green swing on green, opposite the other two channels.
    pub const TINT_GREEN: f64 = 0.30;

    pub fn adjusts_light(&self) -> bool {
        self.exposure != 0.0
            || self.contrast != 0.0
            || self.highlights != 0.0
            || self.shadows != 0.0
            || self.whites != 0.0
            || self.blacks != 0.0
    }

    pub fn adjusts_color(&self) -> bool {
        self.temperature != 0.0 || self.tint != 0.0 || self.vibrance != 0.0 || self.saturation != 0.0
    }

    pub fn adjusts_effects(&self) -> bool {
        self.texture != 0.0
            || self.clarity != 0.0
            || self.dehaze != 0.0
            || self.glow != 0.0
            || self.vignette_amount != 0.0
            || self.grain_amount != 0.0
    }

    pub fn adjusts_curve(&self) -> bool {
        self.curve.adjusts()
    }

    pub fn adjusts_mixer(&self) -> bool {
        self.mixer.adjusts()
    }

    pub fn adjusts_grading(&self) -> bool {
        self.grading.adjusts()
    }

    pub fn adjusts_detail(&self) -> bool {
        self.detail.adjusts()
    }

    pub fn adjusts_optics(&self) -> bool {
        self.optics.adjusts()
    }

    pub fn adjusts_geometry(&self) -> bool {
        self.geometry.adjusts()
    }

    pub fn adjusts_calibration(&self) -> bool {
        self.calibration.adjusts()
    }

    pub fn is_identity(&self) -> bool {
        !self.adjusts_light()
            && !self.adjusts_color()
            && !self.adjusts_effects()
            && !self.adjusts_curve()
            && !self.adjusts_mixer()
            && !self.adjusts_grading()
            && !self.adjusts_detail()
            && !self.adjusts_optics()
            && !self.adjusts_geometry()
            && !self.adjusts_calibration()
    }

    pub fn is_valid(&self) -> bool {
        within(self.exposure, Self::EXPOSURE_RANGE)
            && [
                self.contrast,
                self.highlights,
                self.shadows,
                self.whites,
                self.blacks,
                self.temperature,
                self.tint,
                self.vibrance,
                self.saturation,
                self.texture,
                self.clarity,
                self.dehaze,
                self.glow_range,
                self.glow_spread,
                self.glow_warmth,
                self.vignette_amount,
                self.vignette_roundness,
            ]
            .iter()
            .all(|value| within(*value, Self::TONE_RANGE))
            && [
                self.glow,
                self.vignette_midpoint,
                self.vignette_feather,
                self.vignette_highlights,
                self.grain_amount,
                self.grain_size,
                self.grain_roughness,
            ]
            .iter()
            .all(|value| within(*value, Self::UNIT_RANGE))
    }

    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.exposure = camera_clamp(self.exposure, Self::EXPOSURE_RANGE.0, Self::EXPOSURE_RANGE.1, 0.0);
        result.contrast = camera_clamp(self.contrast, -100.0, 100.0, 0.0);
        result.highlights = camera_clamp(self.highlights, -100.0, 100.0, 0.0);
        result.shadows = camera_clamp(self.shadows, -100.0, 100.0, 0.0);
        result.whites = camera_clamp(self.whites, -100.0, 100.0, 0.0);
        result.blacks = camera_clamp(self.blacks, -100.0, 100.0, 0.0);
        result.temperature = camera_clamp(self.temperature, -100.0, 100.0, 0.0);
        result.tint = camera_clamp(self.tint, -100.0, 100.0, 0.0);
        result.vibrance = camera_clamp(self.vibrance, -100.0, 100.0, 0.0);
        result.saturation = camera_clamp(self.saturation, -100.0, 100.0, 0.0);
        result.texture = camera_clamp(self.texture, -100.0, 100.0, 0.0);
        result.clarity = camera_clamp(self.clarity, -100.0, 100.0, 0.0);
        result.dehaze = camera_clamp(self.dehaze, -100.0, 100.0, 0.0);
        result.glow = camera_clamp(self.glow, 0.0, 100.0, 0.0);
        result.glow_range = camera_clamp(self.glow_range, -100.0, 100.0, 0.0);
        result.glow_spread = camera_clamp(self.glow_spread, -100.0, 100.0, 0.0);
        result.glow_warmth = camera_clamp(self.glow_warmth, -100.0, 100.0, 0.0);
        result.vignette_amount = camera_clamp(self.vignette_amount, -100.0, 100.0, 0.0);
        result.vignette_midpoint = camera_clamp(self.vignette_midpoint, 0.0, 100.0, 50.0);
        result.vignette_roundness = camera_clamp(self.vignette_roundness, -100.0, 100.0, 0.0);
        result.vignette_feather = camera_clamp(self.vignette_feather, 0.0, 100.0, 50.0);
        result.vignette_highlights = camera_clamp(self.vignette_highlights, 0.0, 100.0, 0.0);
        result.grain_amount = camera_clamp(self.grain_amount, 0.0, 100.0, 0.0);
        result.grain_size = camera_clamp(self.grain_size, 0.0, 100.0, 25.0);
        result.grain_roughness = camera_clamp(self.grain_roughness, 0.0, 100.0, 50.0);
        result.curve = self.curve.normalized();
        result.mixer = self.mixer.normalized();
        result.grading = self.grading.normalized();
        result.detail = self.detail.normalized();
        result.optics = self.optics.normalized();
        result.geometry = self.geometry.normalized();
        result.calibration = self.calibration.normalized();
        result
    }

    /// The grade with a panel's eye turned off: that group's amounts become zero and the rest stay.
    pub fn applying(
        &self,
        shows_light: bool,
        shows_color: bool,
        shows_effects: bool,
        shows_curve: bool,
        shows_mixer: bool,
        shows_grading: bool,
        shows_detail: bool,
        shows_optics: bool,
        shows_geometry: bool,
        shows_calibration: bool,
    ) -> Self {
        let mut result = self.clone();
        if !shows_light {
            result.exposure = 0.0;
            result.contrast = 0.0;
            result.highlights = 0.0;
            result.shadows = 0.0;
            result.whites = 0.0;
            result.blacks = 0.0;
        }
        if !shows_color {
            result.temperature = 0.0;
            result.tint = 0.0;
            result.vibrance = 0.0;
            result.saturation = 0.0;
        }
        if !shows_effects {
            result.texture = 0.0;
            result.clarity = 0.0;
            result.dehaze = 0.0;
            result.glow = 0.0;
            result.vignette_amount = 0.0;
            result.grain_amount = 0.0;
        }
        if !shows_curve {
            result.curve = CameraRawCurveSettings::default();
        }
        if !shows_mixer {
            result.mixer = CameraRawMixerSettings::default();
        }
        if !shows_grading {
            result.grading = CameraRawGradingSettings::default();
        }
        if !shows_detail {
            result.detail = CameraRawDetailSettings::default();
        }
        if !shows_optics {
            result.optics = CameraRawOpticsSettings::default();
        }
        if !shows_geometry {
            result.geometry = CameraRawGeometrySettings::default();
        }
        if !shows_calibration {
            result.calibration = CameraRawCalibrationSettings::default();
        }
        result
    }

    /// Camera Raw's 0…100 size, in the pixel scale `adjust_grain` already uses.
    pub fn grain_kernel_size(&self) -> f64 {
        0.5 + (self.grain_size / 100.0) * 19.5
    }

    /// Channel multipliers for `temperature` and `tint`. Neutral is 1, 1, 1.
    pub fn gains(&self) -> (f64, f64, f64) {
        let warm = self.temperature / 100.0;
        let magenta = self.tint / 100.0;
        (
            1.0 + Self::TEMPERATURE_GAIN * warm + Self::TINT_RED_BLUE * magenta,
            1.0 - Self::TINT_GREEN * magenta,
            1.0 - Self::TEMPERATURE_GAIN * warm + Self::TINT_RED_BLUE * magenta,
        )
    }

    /// `clipping` draws the Option-drag overlay instead of the grade. None renders the image.
    /// `scale` is preview pixels per layer pixel. Grain uses `seed` so the pattern stays put while the panel is open.
    pub fn apply(
        &self,
        image: &Rgba8Image,
        clipping: Option<CameraRawClipping>,
        scale: f64,
        seed: u32,
        visualize_point_color: i32,
        sharpen_mask: bool,
    ) -> Result<Rgba8Image> {
        let settings = self.normalized();
        if settings.is_identity() && clipping.is_none() && visualize_point_color < 0 && !sharpen_mask {
            return Ok(image.clone());
        }
        if !settings.is_valid() {
            return Err(invalid_settings());
        }
        let gains = settings.gains();
        let mode = clipping.map(CameraRawClipping::raw_value).unwrap_or(0);
        let pixel_scale = if scale > 0.0 { scale } else { 1.0 };
        let paint_color = clipping.is_none()
            && !sharpen_mask
            && (settings.adjusts_curve()
                || settings.adjusts_mixer()
                || settings.adjusts_grading()
                || visualize_point_color >= 0);
        let paint_effects = clipping.is_none() && !sharpen_mask && settings.adjusts_effects();
        let paint_detail_optics =
            clipping.is_none() && (settings.adjusts_detail() || settings.adjusts_optics() || sharpen_mask);
        let mut result = if clipping.is_none()
            && !sharpen_mask
            && visualize_point_color < 0
            && settings.adjusts_geometry()
        {
            settings.geometry.apply(image)?
        } else {
            image.clone()
        };
        // `ImageAdjustmentPixels.run`: the kernels work in place, on premultiplied rows top-down.
        let (width, height) = (result.width(), result.height());
        let stride = result.stride();
        {
            let pixels = result.data_mut();
            if clipping.is_none() && !sharpen_mask && settings.adjusts_calibration() {
                settings.apply_calibration(pixels, width, height, stride);
            }
            if settings.adjusts_light() || settings.adjusts_color() || clipping.is_some() {
                adjust_camera_raw(
                    pixels,
                    width,
                    height,
                    stride,
                    gains.0,
                    gains.1,
                    gains.2,
                    settings.exposure,
                    settings.contrast,
                    settings.highlights,
                    settings.shadows,
                    settings.whites,
                    settings.blacks,
                    settings.vibrance,
                    settings.saturation,
                    mode,
                );
            }
            if paint_color {
                settings.apply_curve_color(pixels, width, height, stride, visualize_point_color);
            }
            if paint_effects {
                if settings.texture != 0.0
                    || settings.clarity != 0.0
                    || settings.dehaze != 0.0
                    || settings.glow != 0.0
                    || settings.vignette_amount != 0.0
                {
                    adjust_camera_raw_effects(
                        pixels,
                        width,
                        height,
                        stride,
                        settings.texture,
                        settings.clarity,
                        settings.dehaze,
                        settings.glow,
                        settings.glow_style.kernel_value(),
                        settings.glow_range,
                        settings.glow_spread,
                        settings.glow_warmth,
                        settings.vignette_amount,
                        settings.vignette_midpoint,
                        settings.vignette_roundness,
                        settings.vignette_feather,
                        settings.vignette_highlights,
                        settings.vignette_style.kernel_value(),
                        pixel_scale,
                    );
                }
                if settings.grain_amount > 0.0 {
                    adjust_grain(
                        pixels,
                        width,
                        height,
                        stride,
                        settings.grain_amount,
                        settings.grain_kernel_size(),
                        settings.grain_roughness,
                        seed,
                        0.0,
                        0.0,
                        1.0 / pixel_scale,
                    );
                }
            }
            if paint_detail_optics {
                settings.apply_detail_optics(
                    pixels,
                    width,
                    height,
                    stride,
                    pixel_scale,
                    LENS_STRENGTH,
                    sharpen_mask,
                );
            }
        }
        Ok(result)
    }

    /// Temperature and tint that bring one linear-light pixel to neutral, using the same gains `apply` multiplies.
    /// None when a channel is missing or the cast cannot be expressed as those two axes.
    pub fn neutralize_linear(red: f64, green: f64, blue: f64) -> Option<(f64, f64)> {
        if !(red > 1e-4 && green > 1e-4 && blue > 1e-4) {
            return None;
        }
        let a1 = Self::TEMPERATURE_GAIN * red;
        let b1 = Self::TINT_RED_BLUE * red + Self::TINT_GREEN * green;
        let c1 = green - red;
        let a2 = -Self::TEMPERATURE_GAIN * blue;
        let b2 = Self::TINT_RED_BLUE * blue + Self::TINT_GREEN * green;
        let c2 = green - blue;
        let determinant = a1 * b2 - a2 * b1;
        if !(determinant.abs() > 1e-8) {
            return None;
        }
        let warm = (c1 * b2 - c2 * b1) / determinant;
        let magenta = (a1 * c2 - a2 * c1) / determinant;
        if !warm.is_finite() || !magenta.is_finite() {
            return None;
        }
        Some((warm * 100.0, magenta * 100.0))
    }

    pub fn neutralize_straight(red: f64, green: f64, blue: f64) -> Option<(f64, f64)> {
        Self::neutralize_linear(Self::decode(red), Self::decode(green), Self::decode(blue))
    }

    /// Gray-world balance of the opaque pixels. None when the image has no coverage or no solution.
    pub fn auto_balance(image: &Rgba8Image) -> Option<(f64, f64)> {
        let average = Self::average_linear(image)?;
        Self::neutralize_linear(average.0, average.1, average.2)
    }

    /// A hue on the color wheel, 0…360 degrees, from straight 0…1 channels. The defringe eyedropper
    /// centers its purple or green range on this.
    pub fn hue_degrees(red: f64, green: f64, blue: f64) -> f64 {
        let max_channel = red.max(green).max(blue);
        let min_channel = red.min(green).min(blue);
        let chroma = max_channel - min_channel;
        if !(chroma > 1e-6) {
            return 0.0;
        }
        let hue = if max_channel == red {
            (green - blue) / chroma
        } else if max_channel == green {
            2.0 + (blue - red) / chroma
        } else {
            4.0 + (red - green) / chroma
        };
        let mut degrees = hue * 60.0;
        if degrees < 0.0 {
            degrees += 360.0;
        }
        degrees
    }

    fn decode(encoded: f64) -> f64 {
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    }

    fn average_linear(image: &Rgba8Image) -> Option<(f64, f64, f64)> {
        if image.is_empty() {
            return None;
        }
        let mut red = 0.0;
        let mut green = 0.0;
        let mut blue = 0.0;
        let mut count = 0.0;
        for pixel in image.data().chunks_exact(4) {
            let alpha = pixel[3] as f64;
            if alpha == 0.0 {
                continue;
            }
            red += Self::decode((pixel[0] as f64 / alpha).min(1.0));
            green += Self::decode((pixel[1] as f64 / alpha).min(1.0));
            blue += Self::decode((pixel[2] as f64 / alpha).min(1.0));
            count += 1.0;
        }
        if !(count > 0.0) {
            return None;
        }
        Some((red / count, green / count, blue / count))
    }
}

/// `ClosedRange.contains`: a finite value inside the range.
fn within(value: f64, range: (f64, f64)) -> bool {
    value.is_finite() && value >= range.0 && value <= range.1
}

// MARK: - Optics

/// Lens profile toggles, manual distortion, defringe, and lens-vignetting correction. Profile metadata is not
/// available on a rendered layer, so the profile sliders only scale generic correction strength.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawOpticsSettings {
    pub remove_chromatic_aberration: bool,
    pub enable_lens_profile: bool,
    pub profile_distortion: f64,
    pub profile_vignetting: f64,
    /// −100…100, same sign convention as the Lens Correction filter.
    pub distortion: f64,
    pub purple_amount: f64,
    /// Degrees on the color wheel, 0…360. The low handle must stay below the high handle.
    pub purple_hue_low: f64,
    pub purple_hue_high: f64,
    pub green_amount: f64,
    pub green_hue_low: f64,
    pub green_hue_high: f64,
    /// Brightens the corners to counter lens falloff. Midpoint is 0…100.
    pub vignette_amount: f64,
    pub vignette_midpoint: f64,
}

impl Default for CameraRawOpticsSettings {
    fn default() -> Self {
        Self {
            remove_chromatic_aberration: false,
            enable_lens_profile: false,
            profile_distortion: 100.0,
            profile_vignetting: 100.0,
            distortion: 0.0,
            purple_amount: 0.0,
            purple_hue_low: 270.0,
            purple_hue_high: 310.0,
            green_amount: 0.0,
            green_hue_low: 60.0,
            green_hue_high: 120.0,
            vignette_amount: 0.0,
            vignette_midpoint: 50.0,
        }
    }
}

impl CameraRawOpticsSettings {
    pub const TONE_RANGE: (f64, f64) = (-100.0, 100.0);
    pub const UNIT_RANGE: (f64, f64) = (0.0, 100.0);
    pub const HUE_RANGE: (f64, f64) = (0.0, 360.0);

    pub fn adjusts(&self) -> bool {
        self.remove_chromatic_aberration
            || self.enable_lens_profile
            || self.distortion != 0.0
            || self.purple_amount != 0.0
            || self.green_amount != 0.0
            || self.vignette_amount != 0.0
    }

    pub fn normalized(&self) -> Self {
        let mut result = *self;
        result.profile_distortion = camera_clamp(self.profile_distortion, 0.0, 100.0, 100.0);
        result.profile_vignetting = camera_clamp(self.profile_vignetting, 0.0, 100.0, 100.0);
        result.distortion = camera_clamp(self.distortion, -100.0, 100.0, 0.0);
        result.purple_amount = camera_clamp(self.purple_amount, 0.0, 100.0, 0.0);
        result.green_amount = camera_clamp(self.green_amount, 0.0, 100.0, 0.0);
        result.vignette_amount = camera_clamp(self.vignette_amount, -100.0, 100.0, 0.0);
        result.vignette_midpoint = camera_clamp(self.vignette_midpoint, 0.0, 100.0, 50.0);
        result.purple_hue_low = camera_clamp(self.purple_hue_low, 0.0, 360.0, 270.0);
        result.purple_hue_high = camera_clamp(self.purple_hue_high, 0.0, 360.0, 310.0);
        result.green_hue_low = camera_clamp(self.green_hue_low, 0.0, 360.0, 60.0);
        result.green_hue_high = camera_clamp(self.green_hue_high, 0.0, 360.0, 120.0);
        if result.purple_hue_low > result.purple_hue_high {
            std::mem::swap(&mut result.purple_hue_low, &mut result.purple_hue_high);
        }
        if result.green_hue_low > result.green_hue_high {
            std::mem::swap(&mut result.green_hue_low, &mut result.green_hue_high);
        }
        result
    }

    /// The panel's eye turned off: the whole group returns to its defaults.
    pub fn applying(&self, shows: bool) -> Self {
        if shows {
            *self
        } else {
            Self::default()
        }
    }

    /// Combined radial distortion passed to `lens_distort`, matching the Lens Correction filter scale.
    pub fn distortion_k(&self, profile_strength: f64) -> f64 {
        let manual = self.distortion / 100.0 * profile_strength;
        let profile = if self.enable_lens_profile {
            self.profile_distortion / 100.0 * profile_strength
        } else {
            0.0
        };
        manual + profile
    }
}

// MARK: - Geometry

/// Whether the guided lines straighten the picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawUprightMode {
    #[default]
    Off,
    Guided,
}

impl CameraRawUprightMode {
    pub const ALL: [CameraRawUprightMode; 2] =
        [CameraRawUprightMode::Off, CameraRawUprightMode::Guided];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawUprightMode::Off => "Off",
            CameraRawUprightMode::Guided => "Guided",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// The projection the geometry warp assumes; Rectilinear corrects more gently than Perspective.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawProjection {
    #[default]
    Perspective,
    Rectilinear,
}

impl CameraRawProjection {
    pub const ALL: [CameraRawProjection; 2] =
        [CameraRawProjection::Perspective, CameraRawProjection::Rectilinear];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawProjection::Perspective => "Perspective",
            CameraRawProjection::Rectilinear => "Rectilinear",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|projection| projection.raw_value() == value)
    }
}

/// A guide line in normalized image coordinates, 0…1 from the lower-left of the pixel grid.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraRawGeometryGuide {
    pub start_x: f64,
    pub start_y: f64,
    pub end_x: f64,
    pub end_y: f64,
}

impl CameraRawGeometryGuide {
    pub fn new(start_x: f64, start_y: f64, end_x: f64, end_y: f64) -> Self {
        Self {
            start_x,
            start_y,
            end_x,
            end_y,
        }
    }

    pub fn start(&self) -> Point {
        Point::new(self.start_x, self.start_y)
    }

    pub fn end(&self) -> Point {
        Point::new(self.end_x, self.end_y)
    }
}

/// The Geometry panel: upright/projection, the four affine corrections, and the guides.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawGeometrySettings {
    pub upright: CameraRawUprightMode,
    pub projection: CameraRawProjection,
    pub vertical: f64,
    pub horizontal: f64,
    pub rotate: f64,
    pub aspect: f64,
    pub scale: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub constrain_crop: bool,
    pub guides: Vec<CameraRawGeometryGuide>,
}

impl Default for CameraRawGeometrySettings {
    fn default() -> Self {
        Self {
            upright: CameraRawUprightMode::Off,
            projection: CameraRawProjection::Perspective,
            vertical: 0.0,
            horizontal: 0.0,
            rotate: 0.0,
            aspect: 0.0,
            scale: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
            constrain_crop: false,
            guides: Vec::new(),
        }
    }
}

impl CameraRawGeometrySettings {
    pub const TONE_RANGE: (f64, f64) = (-100.0, 100.0);
    pub const ROTATE_RANGE: (f64, f64) = (-45.0, 45.0);

    pub fn adjusts(&self) -> bool {
        self.uses_guides()
            || self.vertical != 0.0
            || self.horizontal != 0.0
            || self.rotate != 0.0
            || self.aspect != 0.0
            || self.scale != 0.0
            || self.offset_x != 0.0
            || self.offset_y != 0.0
    }

    /// Guided only counts once a line is long enough to read. An empty Guided choice must not warp the picture.
    fn uses_guides(&self) -> bool {
        self.upright == CameraRawUprightMode::Guided
            && self.guides.iter().any(|guide| {
                (guide.end_x - guide.start_x).hypot(guide.end_y - guide.start_y) > 0.01
            })
    }

    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.vertical = camera_clamp(self.vertical, -100.0, 100.0, 0.0);
        result.horizontal = camera_clamp(self.horizontal, -100.0, 100.0, 0.0);
        result.rotate = camera_clamp(self.rotate, -45.0, 45.0, 0.0);
        result.aspect = camera_clamp(self.aspect, -100.0, 100.0, 0.0);
        result.scale = camera_clamp(self.scale, -100.0, 100.0, 0.0);
        result.offset_x = camera_clamp(self.offset_x, -100.0, 100.0, 0.0);
        result.offset_y = camera_clamp(self.offset_y, -100.0, 100.0, 0.0);
        result.guides = self
            .guides
            .iter()
            .filter(|guide| {
                (guide.end_x - guide.start_x).hypot(guide.end_y - guide.start_y) > 0.01
            })
            .cloned()
            .collect();
        result
    }

    /// The panel's eye turned off: the whole group returns to its defaults.
    pub fn applying(&self, shows: bool) -> Self {
        if shows {
            self.clone()
        } else {
            Self::default()
        }
    }

    /// Perspective and affine geometry on the pixel grid. Output matches the input size unless Constrain Crop trims empty edges.
    pub fn apply(&self, image: &Rgba8Image) -> Result<Rgba8Image> {
        let settings = self.normalized();
        if !settings.adjusts() {
            return Ok(image.clone());
        }
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 {
            return Ok(image.clone());
        }
        let (vertical, horizontal, rotate) = settings.effective_corrections();
        let corners =
            settings.output_corners(width, height, vertical, horizontal, rotate);
        // Core Image measures y upward from the bottom; the buffer runs top-down.
        let quad = corners.map(|(x, y)| (x, height as f64 - y));
        let result = perspective_warp(image, quad);
        if !settings.constrain_crop {
            return Ok(result);
        }
        let edges = brush_alpha_bounds(result.data(), width, height, result.stride());
        let crop = Rect::new(
            edges[0] as f64,
            edges[1] as f64,
            (edges[2] - edges[0]) as f64,
            (edges[3] - edges[1]) as f64,
        );
        if !(crop.width() >= 1.0 && crop.height() >= 1.0)
            || !(crop.width() < width as f64 || crop.height() < height as f64)
        {
            return Ok(result);
        }
        let Some(cropped) = result.cropped(crop) else {
            return Ok(result);
        };
        let scale = (width as f64 / crop.width()).min(height as f64 / crop.height());
        let draw = Rect::new(
            (width as f64 - crop.width() * scale) / 2.0,
            (height as f64 - crop.height() * scale) / 2.0,
            crop.width() * scale,
            crop.height() * scale,
        );
        let mut fitted = Rgba8Image::new(width, height);
        let scaled = scale_nearest(
            &cropped,
            (draw.width().round() as usize).max(1),
            (draw.height().round() as usize).max(1),
        );
        fitted.draw_over(
            &scaled,
            [
                draw.min_x().round() as i32,
                draw.min_y().round() as i32,
            ],
        );
        Ok(fitted)
    }

    fn effective_corrections(&self) -> (f64, f64, f64) {
        match self.upright {
            CameraRawUprightMode::Off => (self.vertical, self.horizontal, self.rotate),
            CameraRawUprightMode::Guided => {
                let guided = Self::guided_corrections(&self.guides);
                (
                    self.vertical + guided.0,
                    self.horizontal + guided.1,
                    self.rotate + guided.2,
                )
            }
        }
    }

    fn guided_corrections(guides: &[CameraRawGeometryGuide]) -> (f64, f64, f64) {
        let Some(first) = guides.first() else {
            return (0.0, 0.0, 0.0);
        };
        let (dx, dy) = (first.end_x - first.start_x, first.end_y - first.start_y);
        let length = dx.hypot(dy);
        if length <= 1e-4 {
            return (0.0, 0.0, 0.0);
        }
        let angle = dy.atan2(dx) * 180.0 / std::f64::consts::PI;
        let mut rotate = -angle;
        if rotate > 45.0 {
            rotate -= 90.0;
        } else if rotate < -45.0 {
            rotate += 90.0;
        }
        let mut vertical = 0.0;
        let mut horizontal = 0.0;
        if guides.len() > 1 {
            let second = guides[1];
            let (sx, sy) = (
                second.end_x - second.start_x,
                second.end_y - second.start_y,
            );
            let sl = sx.hypot(sy);
            if sl > 1e-4 {
                let a2 = sy.atan2(sx) * 180.0 / std::f64::consts::PI;
                vertical = if a2.abs() > 45.0 {
                    if a2 > 0.0 {
                        25.0
                    } else {
                        -25.0
                    }
                } else {
                    0.0
                };
                horizontal = if a2.abs() <= 45.0 {
                    if a2 > 0.0 {
                        25.0
                    } else {
                        -25.0
                    }
                } else {
                    0.0
                };
            }
        }
        (vertical, horizontal, rotate)
    }

    /// Core Image corner positions with y measured upward from the bottom.
    fn output_corners(
        &self,
        width: usize,
        height: usize,
        vertical: f64,
        horizontal: f64,
        rotation: f64,
    ) -> [(f64, f64); 4] {
        let w = width as f64;
        let h = height as f64;
        let strength = if self.projection == CameraRawProjection::Perspective {
            1.0
        } else {
            0.55
        };
        let v = vertical / 100.0 * w * 0.18 * strength;
        let hz = horizontal / 100.0 * h * 0.18 * strength;
        let aspect_scale = 1.0 + self.aspect / 200.0;
        let zoom = 1.0 + self.scale / 100.0;
        let shift_x = self.offset_x / 100.0 * w * 0.15;
        let shift_y = self.offset_y / 100.0 * h * 0.15;
        let mut top_left = (-v + shift_x, h + shift_y);
        let mut top_right = (w + v + shift_x, h + shift_y);
        let mut bottom_right = (w + hz + shift_x, -shift_y);
        let mut bottom_left = (-hz + shift_x, -shift_y);
        let center = (w / 2.0 + shift_x, h / 2.0 + shift_y);
        let radians = rotation * std::f64::consts::PI / 180.0;
        let rotate = |point: (f64, f64)| -> (f64, f64) {
            let (dx, dy) = (point.0 - center.0, point.1 - center.1);
            let (cosine, sine) = (radians.cos(), radians.sin());
            (
                center.0 + dx * cosine - dy * sine,
                center.1 + dx * sine + dy * cosine,
            )
        };
        top_left = rotate(top_left);
        top_right = rotate(top_right);
        bottom_right = rotate(bottom_right);
        bottom_left = rotate(bottom_left);
        if aspect_scale != 1.0 {
            let scaled = |point: (f64, f64)| -> (f64, f64) {
                (
                    center.0 + (point.0 - center.0) * aspect_scale,
                    center.1 + (point.1 - center.1) / aspect_scale,
                )
            };
            top_left = scaled(top_left);
            top_right = scaled(top_right);
            bottom_right = scaled(bottom_right);
            bottom_left = scaled(bottom_left);
        }
        if zoom != 1.0 {
            let zoomed = |point: (f64, f64)| -> (f64, f64) {
                (
                    center.0 + (point.0 - center.0) * zoom,
                    center.1 + (point.1 - center.1) * zoom,
                )
            };
            top_left = zoomed(top_left);
            top_right = zoomed(top_right);
            bottom_right = zoomed(bottom_right);
            bottom_left = zoomed(bottom_left);
        }
        [top_left, top_right, bottom_right, bottom_left]
    }
}

/// `CIPerspectiveTransform`: the source image's corners — top-left, top-right, bottom-right,
/// bottom-left, rows top-down — land on `quad` in the output, which keeps the source's size.
/// Destination pixels outside the mapped quad stay transparent, as Core Image leaves them; sampling
/// is bilinear in premultiplied space.
fn perspective_warp(image: &Rgba8Image, quad: [(f64, f64); 4]) -> Rgba8Image {
    let (width, height) = (image.width(), image.height());
    let mut result = Rgba8Image::new(width, height);
    let Some(inverse) = square_to_quad(quad).map(invert3) else {
        return result;
    };
    let (sw, sh) = (width as f64, height as f64);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (x as f64 + 0.5, y as f64 + 0.5);
            let denominator = inverse[6] * dx + inverse[7] * dy + inverse[8];
            if !denominator.is_finite() || denominator == 0.0 {
                continue;
            }
            let u = (inverse[0] * dx + inverse[1] * dy + inverse[2]) / denominator;
            let v = (inverse[3] * dx + inverse[4] * dy + inverse[5]) / denominator;
            if !u.is_finite() || !v.is_finite() {
                continue;
            }
            // Sampling outside the source is nothing, the way Core Image's default sampler reads it.
            if u < -0.5 / sw || u > 1.0 + 0.5 / sw || v < -0.5 / sh || v > 1.0 + 0.5 / sh {
                continue;
            }
            let pixel = sample_bilinear_premultiplied(image, u * sw - 0.5, v * sh - 0.5);
            result.set(x, y, pixel);
        }
    }
    result
}

/// The projective transform taking the unit square's corners — (0,0), (1,0), (1,1), (0,1) — to
/// `quad`, row-major `[a b c d e f g h 1]`: `x = (a·u + b·v + c) / (g·u + h·v + 1)` and likewise for y.
fn square_to_quad(quad: [(f64, f64); 4]) -> Option<[f64; 9]> {
    let [(x0, y0), (x1, y1), (x2, y2), (x3, y3)] = quad;
    let dx1 = x1 - x2;
    let dx2 = x3 - x2;
    let dx3 = x0 - x1 + x2 - x3;
    let dy1 = y1 - y2;
    let dy2 = y3 - y2;
    let dy3 = y0 - y1 + y2 - y3;
    let (g, h) = if dx3 == 0.0 && dy3 == 0.0 {
        (0.0, 0.0)
    } else {
        let denominator = dx1 * dy2 - dx2 * dy1;
        if !denominator.is_finite() || denominator.abs() < 1e-12 {
            return None;
        }
        (
            (dx3 * dy2 - dx2 * dy3) / denominator,
            (dx1 * dy3 - dx3 * dy1) / denominator,
        )
    };
    Some([
        x1 - x0 + g * x1,
        x3 - x0 + h * x3,
        x0,
        y1 - y0 + g * y1,
        y3 - y0 + h * y3,
        y0,
        g,
        h,
        1.0,
    ])
}

/// The inverse of a row-major 3×3; all-zero when it has no inverse.
fn invert3(matrix: [f64; 9]) -> [f64; 9] {
    let [a, b, c, d, e, f, g, h, i] = matrix;
    let determinant = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return [0.0; 9];
    }
    [
        (e * i - f * h) / determinant,
        (c * h - b * i) / determinant,
        (b * f - c * e) / determinant,
        (f * g - d * i) / determinant,
        (a * i - c * g) / determinant,
        (c * d - a * f) / determinant,
        (d * h - e * g) / determinant,
        (b * g - a * h) / determinant,
        (a * e - b * d) / determinant,
    ]
}

/// The premultiplied pixel at `(x, y)` in pixel-center coordinates; nothing outside the image.
fn sample_bilinear_premultiplied(image: &Rgba8Image, x: f64, y: f64) -> [u8; 4] {
    let (width, height) = (image.width(), image.height());
    let base_x = x.floor();
    let base_y = y.floor();
    let fx = x - base_x;
    let fy = y - base_y;
    let mut out = [0.0f64; 4];
    for (ox, wx) in [(0i64, 1.0 - fx), (1, fx)] {
        for (oy, wy) in [(0i64, 1.0 - fy), (1, fy)] {
            let weight = wx * wy;
            if weight == 0.0 {
                continue;
            }
            let sx = base_x as i64 + ox;
            let sy = base_y as i64 + oy;
            if sx < 0 || sy < 0 || sx >= width as i64 || sy >= height as i64 {
                continue;
            }
            let pixel = image.get(sx as usize, sy as usize);
            for channel in 0..4 {
                out[channel] += pixel[channel] as f64 * weight;
            }
        }
    }
    [
        out[0].round().clamp(0.0, 255.0) as u8,
        out[1].round().clamp(0.0, 255.0) as u8,
        out[2].round().clamp(0.0, 255.0) as u8,
        out[3].round().clamp(0.0, 255.0) as u8,
    ]
}

/// The nearest source pixel, Core Graphics' `.none` interpolation quality, which is what
/// `BrushRaster.draw` asks for while Constrain Crop fits the trimmed picture back into the frame.
fn scale_nearest(source: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut result = Rgba8Image::new(width, height);
    if source.is_empty() || width == 0 || height == 0 {
        return result;
    }
    let (sw, sh) = (source.width() as f64, source.height() as f64);
    for y in 0..height {
        let sy = (((y as f64 + 0.5) * sh / height as f64).floor() as i64)
            .clamp(0, source.height() as i64 - 1) as usize;
        for x in 0..width {
            let sx = (((x as f64 + 0.5) * sw / width as f64).floor() as i64)
                .clamp(0, source.width() as i64 - 1) as usize;
            result.set(x, y, source.get(sx, sy));
        }
    }
    result
}

// MARK: - Calibration

/// Camera Raw's Process Version, which scales how far the calibration sliders move.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraRawProcessVersion {
    Version1,
    Version2,
    Version3,
    Version4,
    Version5,
    #[default]
    Version6,
}

impl CameraRawProcessVersion {
    pub const ALL: [CameraRawProcessVersion; 6] = [
        CameraRawProcessVersion::Version1,
        CameraRawProcessVersion::Version2,
        CameraRawProcessVersion::Version3,
        CameraRawProcessVersion::Version4,
        CameraRawProcessVersion::Version5,
        CameraRawProcessVersion::Version6,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawProcessVersion::Version1 => "Version 1",
            CameraRawProcessVersion::Version2 => "Version 2",
            CameraRawProcessVersion::Version3 => "Version 3",
            CameraRawProcessVersion::Version4 => "Version 4",
            CameraRawProcessVersion::Version5 => "Version 5",
            CameraRawProcessVersion::Version6 => "Version 6",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|version| version.raw_value() == value)
    }

    pub fn kernel_value(self) -> i32 {
        match self {
            CameraRawProcessVersion::Version1 => 1,
            CameraRawProcessVersion::Version2 => 2,
            CameraRawProcessVersion::Version3 => 3,
            CameraRawProcessVersion::Version4 => 4,
            CameraRawProcessVersion::Version5 => 5,
            CameraRawProcessVersion::Version6 => 6,
        }
    }

    /// What this process does to the calibration sliders on an already-rendered layer.
    pub fn summary(self) -> &'static str {
        match self {
            CameraRawProcessVersion::Version1 => {
                "Earliest response. Hue, saturation, and shadow tint move about half as far as Version 6."
            }
            CameraRawProcessVersion::Version2 => {
                "A little stronger than Version 1. The sliders below still fall well short of the current look."
            }
            CameraRawProcessVersion::Version3 => {
                "Firmer color than Version 2. Primary shifts stay gentler than the current process."
            }
            CameraRawProcessVersion::Version4 => {
                "The 2012 response. Calibration reaches most of the strength used by Version 6."
            }
            CameraRawProcessVersion::Version5 => {
                "Close to the current process, with slightly softer primary and shadow shifts."
            }
            CameraRawProcessVersion::Version6 => {
                "Current default. The calibration sliders below apply at full strength."
            }
        }
    }
}

/// Camera Calibration: shadow tint and the three primaries' hue and saturation shifts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawCalibrationSettings {
    pub process: CameraRawProcessVersion,
    pub shadow_tint: f64,
    pub red_hue: f64,
    pub red_saturation: f64,
    pub green_hue: f64,
    pub green_saturation: f64,
    pub blue_hue: f64,
    pub blue_saturation: f64,
}

impl Default for CameraRawCalibrationSettings {
    fn default() -> Self {
        Self {
            process: CameraRawProcessVersion::Version6,
            shadow_tint: 0.0,
            red_hue: 0.0,
            red_saturation: 0.0,
            green_hue: 0.0,
            green_saturation: 0.0,
            blue_hue: 0.0,
            blue_saturation: 0.0,
        }
    }
}

impl CameraRawCalibrationSettings {
    pub const TONE_RANGE: (f64, f64) = (-100.0, 100.0);

    pub fn adjusts(&self) -> bool {
        self.shadow_tint != 0.0
            || self.red_hue != 0.0
            || self.red_saturation != 0.0
            || self.green_hue != 0.0
            || self.green_saturation != 0.0
            || self.blue_hue != 0.0
            || self.blue_saturation != 0.0
    }

    pub fn normalized(&self) -> Self {
        let mut result = *self;
        result.shadow_tint = camera_clamp(self.shadow_tint, -100.0, 100.0, 0.0);
        result.red_hue = camera_clamp(self.red_hue, -100.0, 100.0, 0.0);
        result.red_saturation = camera_clamp(self.red_saturation, -100.0, 100.0, 0.0);
        result.green_hue = camera_clamp(self.green_hue, -100.0, 100.0, 0.0);
        result.green_saturation = camera_clamp(self.green_saturation, -100.0, 100.0, 0.0);
        result.blue_hue = camera_clamp(self.blue_hue, -100.0, 100.0, 0.0);
        result.blue_saturation = camera_clamp(self.blue_saturation, -100.0, 100.0, 0.0);
        result
    }

    /// The panel's eye turned off: the whole group returns to its defaults.
    pub fn applying(&self, shows: bool) -> Self {
        if shows {
            *self
        } else {
            Self::default()
        }
    }
}

// MARK: - Application

impl CameraRawSettings {
    /// Runs Curve, then Color Mixer, then Color Grading. `visualize` dims pixels outside that point color.
    pub fn apply_curve_color(
        &self,
        pixels: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        visualize: i32,
    ) {
        let curve = self.curve.normalized();
        let mixer = self.mixer.normalized();
        let grading = self.grading.normalized();
        let tone = curve.tone_table();
        let red = curve.channel_table(&curve.red);
        let green = curve.channel_table(&curve.green);
        let blue = curve.channel_table(&curve.blue);
        let mixer_floats = mixer.mixer_floats();
        let point_floats = mixer.point_floats();
        let grade = grading.grade_floats();
        adjust_camera_raw_curve_color(
            pixels,
            width,
            height,
            stride,
            &tone,
            &red,
            &green,
            &blue,
            curve.refine_saturation / 100.0,
            &mixer_floats,
            mixer.points.len() as i32,
            &point_floats,
            &grade,
            grading.blending / 100.0,
            grading.balance / 100.0,
            visualize,
        );
    }

    /// Optics, then Detail. While Option is held (`sharpen_mask`) the sharpen-mask overlay replaces both.
    pub fn apply_detail_optics(
        &self,
        pixels: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        scale: f64,
        profile_strength: f64,
        sharpen_mask: bool,
    ) {
        let detail = self.detail.normalized();
        let optics = self.optics.normalized();
        if sharpen_mask {
            adjust_camera_raw_sharpen_mask_overlay(
                pixels,
                width,
                height,
                stride,
                detail.sharpen_radius,
                detail.sharpen_detail,
                detail.sharpen_masking,
                scale,
            );
            return;
        }
        if optics.adjusts() {
            adjust_camera_raw_optics(
                pixels,
                width,
                height,
                stride,
                if optics.remove_chromatic_aberration { 1 } else { 0 },
                if optics.enable_lens_profile { 1 } else { 0 },
                optics.profile_distortion,
                optics.profile_vignetting,
                optics.distortion_k(profile_strength),
                optics.purple_amount,
                optics.purple_hue_low,
                optics.purple_hue_high,
                optics.green_amount,
                optics.green_hue_low,
                optics.green_hue_high,
                optics.vignette_amount,
                optics.vignette_midpoint,
                scale,
            );
        }
        if detail.adjusts() {
            adjust_camera_raw_detail(
                pixels,
                width,
                height,
                stride,
                detail.sharpen_amount,
                detail.sharpen_radius,
                detail.sharpen_detail,
                detail.sharpen_masking,
                detail.noise_luminance,
                detail.noise_luminance_detail,
                detail.noise_luminance_contrast,
                detail.noise_color,
                detail.noise_color_detail,
                detail.noise_color_smoothness,
                scale,
            );
        }
    }

    /// Camera Calibration, before the Light and Color grade.
    pub fn apply_calibration(
        &self,
        pixels: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
    ) {
        let calibration = self.calibration.normalized();
        if !calibration.adjusts() {
            return;
        }
        adjust_camera_raw_calibration(
            pixels,
            width,
            height,
            stride,
            calibration.shadow_tint,
            calibration.red_hue,
            calibration.red_saturation,
            calibration.green_hue,
            calibration.green_saturation,
            calibration.blue_hue,
            calibration.blue_saturation,
            calibration.process.kernel_value(),
        );
    }
}

// MARK: - Scope

/// One RGB histogram and a hue/saturation vectorscope of the same graded pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawScope {
    pub red: Vec<f64>,
    pub green: Vec<f64>,
    pub blue: Vec<f64>,
    /// Density from the center outward. Index `y * scope_side + x`.
    pub vectorscope: Vec<f64>,
}

impl CameraRawScope {
    pub const BIN_COUNT: usize = 256;
    pub const SCOPE_SIDE: usize = 64;

    /// An all-zero scope, what the Swift falls back to when the image cannot be counted.
    pub fn empty() -> Self {
        Self {
            red: vec![0.0; Self::BIN_COUNT],
            green: vec![0.0; Self::BIN_COUNT],
            blue: vec![0.0; Self::BIN_COUNT],
            vectorscope: vec![0.0; Self::SCOPE_SIDE * Self::SCOPE_SIDE],
        }
    }

    /// Shared vertical scale so the three ribbons stay comparable.
    pub fn peak(&self) -> f64 {
        histogram_scale(&self.red)
            .max(histogram_scale(&self.green))
            .max(histogram_scale(&self.blue))
    }

    /// Counts the graded image. Fully transparent pixels are skipped.
    pub fn make(image: &Rgba8Image) -> Option<Self> {
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 {
            return None;
        }
        let mut bins = vec![0.0f64; Self::BIN_COUNT * 4];
        levels_histogram(image.data(), None, width * height, &mut bins);
        let mut scope = vec![0.0f64; Self::SCOPE_SIDE * Self::SCOPE_SIDE];
        let stride = image.stride();
        let bytes = image.data();
        for y in 0..height {
            let row = y * stride;
            for x in 0..width {
                let pixel = row + x * 4;
                let alpha = bytes[pixel + 3] as f64;
                if alpha == 0.0 {
                    continue;
                }
                let red = (bytes[pixel] as f64 / alpha).min(1.0);
                let green = (bytes[pixel + 1] as f64 / alpha).min(1.0);
                let blue = (bytes[pixel + 2] as f64 / alpha).min(1.0);
                let max_channel = red.max(green).max(blue);
                let min_channel = red.min(green).min(blue);
                let chroma = max_channel - min_channel;
                if !(chroma > 1e-4) || !(max_channel > 1e-4) {
                    continue;
                }
                let mut hue = if max_channel == red {
                    (green - blue) / chroma
                } else if max_channel == green {
                    2.0 + (blue - red) / chroma
                } else {
                    4.0 + (red - green) / chroma
                };
                hue /= 6.0;
                if hue < 0.0 {
                    hue += 1.0;
                }
                let angle = hue * 2.0 * std::f64::consts::PI;
                let saturation = chroma / max_channel;
                let plot_x = 0.5 + angle.cos() * saturation * 0.48;
                let plot_y = 0.5 + angle.sin() * saturation * 0.48;
                let column = ((plot_x * Self::SCOPE_SIDE as f64) as i64)
                    .clamp(0, Self::SCOPE_SIDE as i64 - 1) as usize;
                let row_index = ((plot_y * Self::SCOPE_SIDE as f64) as i64)
                    .clamp(0, Self::SCOPE_SIDE as i64 - 1) as usize;
                scope[row_index * Self::SCOPE_SIDE + column] += alpha / 255.0;
            }
        }
        Some(Self {
            red: bins[256..512].to_vec(),
            green: bins[512..768].to_vec(),
            blue: bins[768..1024].to_vec(),
            vectorscope: scope,
        })
    }

    /// Paints clipped shadows blue and clipped highlights red. The histogram is counted before this.
    pub fn overlay(image: &Rgba8Image, shadows: bool, highlights: bool) -> Result<Rgba8Image> {
        if !shadows && !highlights {
            return Ok(image.clone());
        }
        let mut result = image.clone();
        let (width, height, stride) = (result.width(), result.height(), result.stride());
        adjust_camera_raw_clip_overlay(
            result.data_mut(),
            width,
            height,
            stride,
            if shadows { 1 } else { 0 },
            if highlights { 1 } else { 0 },
        );
        Ok(result)
    }

    /// Preview image plus the scope of the grade itself, without Option-drag or indicator paint.
    ///
    /// `settings` is the panel's Camera Raw state, which a Rust `FilterJob` cannot carry; `clipping` is
    /// `job.camera_raw_clipping` in either form.
    pub fn preview(
        job: &FilterJob,
        settings: &CameraRawSettings,
        clipping: Option<CameraRawClipping>,
    ) -> Result<(Rgba8Image, CameraRawScope)> {
        let mut grade = job.clone();
        grade.camera_raw_clipping = 0;
        grade.shows_shadow_clipping = false;
        grade.shows_highlight_clipping = false;
        grade.visualizes_point_color = -1;
        grade.shows_sharpen_mask = false;
        let graded = PixelFilter::run(
            &grade,
            Some(CameraRawInputs {
                settings,
                clipping: None,
            }),
        )?;
        let scope = Self::make(&graded).unwrap_or_else(Self::empty);
        if job.camera_raw_clipping != 0 || job.shows_sharpen_mask {
            let image = PixelFilter::run(job, Some(CameraRawInputs { settings, clipping }))?;
            return Ok((image, scope));
        }
        if job.shows_shadow_clipping || job.shows_highlight_clipping {
            let image = Self::overlay(&graded, job.shows_shadow_clipping, job.shows_highlight_clipping)?;
            return Ok((image, scope));
        }
        Ok((graded, scope))
    }
}

/// `LevelsHistogramDisplay.scale(for:)`: display-only vertical scaling. Keep linear bin ratios, but
/// cap isolated spikes so large solid backgrounds cannot flatten the useful tonal distribution.
fn histogram_scale(bins: &[f64]) -> f64 {
    let peak = bins
        .iter()
        .filter(|value| value.is_finite() && **value > 0.0)
        .fold(0.0_f64, |best, value| best.max(*value));
    if !(peak > 0.0) {
        return 0.0;
    }
    let mut interior: Vec<f64> = bins
        .iter()
        .skip(1)
        .take(bins.len().saturating_sub(2))
        .filter(|value| value.is_finite() && **value > 0.0)
        .cloned()
        .collect();
    if interior.is_empty() {
        return peak;
    }
    interior.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let typical = interior[((interior.len() - 1) as f64 * 0.95) as usize];
    peak.min(typical * 4.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(value: u8) -> Rgba8Image {
        Rgba8Image::from_data(2, 1, vec![value, value, value, 255, 0, 0, 0, 0])
    }

    /// The Swift tests' `image(width:height:red:green:blue:alpha:)`: a solid premultiplied fill.
    fn solid(width: usize, height: usize, red: f64, green: f64, blue: f64, alpha: f64) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        let pixel = [
            (red * alpha * 255.0).round().clamp(0.0, 255.0) as u8,
            (green * alpha * 255.0).round().clamp(0.0, 255.0) as u8,
            (blue * alpha * 255.0).round().clamp(0.0, 255.0) as u8,
            (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
        ];
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, pixel);
            }
        }
        image
    }

    /// The Swift tests' `gray()`: an opaque mid-gray fill.
    fn mid_gray(width: usize, height: usize) -> Rgba8Image {
        solid(width, height, 128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0)
    }

    /// The Swift tests' `checker()`: 2×2 blocks of 0.9 and 0.1.
    fn checker(width: usize, height: usize) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let value: f64 = if ((x / 2) + (y / 2)) % 2 == 0 { 0.9 } else { 0.1 };
                let byte = (value * 255.0).round() as u8;
                image.set(x, y, [byte, byte, byte, 255]);
            }
        }
        image
    }

    /// The Swift tests' `step()`: 24×4, 40 on the left half and 200 on the right.
    fn step() -> Rgba8Image {
        let mut image = Rgba8Image::new(24, 4);
        for y in 0..4 {
            for x in 0..24 {
                let byte = if x < 12 { 40 } else { 200 };
                image.set(x, y, [byte, byte, byte, 255]);
            }
        }
        image
    }

    /// The Swift tests' `pixels()` on one pixel: straight RGBA, alpha last.
    fn straight(image: &Rgba8Image, x: usize, y: usize) -> [i32; 4] {
        let pixel = image.get(x, y);
        let alpha = pixel[3] as i32;
        let channel = |value: u8| -> i32 {
            if alpha == 0 {
                0
            } else {
                ((value as i32 * 255 + alpha / 2) / alpha).min(255)
            }
        };
        [channel(pixel[0]), channel(pixel[1]), channel(pixel[2]), alpha]
    }

    /// The Swift tests' `chroma()`: max channel minus min channel.
    fn chroma(pixel: [i32; 4]) -> i32 {
        pixel[0].max(pixel[1]).max(pixel[2]) - pixel[0].min(pixel[1]).min(pixel[2])
    }

    /// The Swift tests' `byte(_:x:)`.
    fn byte(image: &Rgba8Image, x: usize, y: usize) -> i32 {
        straight(image, x, y)[0]
    }

    /// `CameraRawSettings::apply` with the panel's defaults: no clipping view, one preview pixel per
    /// layer pixel, no point-color visualization, no sharpen mask.
    fn grade(settings: &CameraRawSettings, image: &Rgba8Image) -> Rgba8Image {
        settings.apply(image, None, 1.0, 0, -1, false).expect("apply")
    }

    fn grade_seed(settings: &CameraRawSettings, image: &Rgba8Image, seed: u32) -> Rgba8Image {
        settings.apply(image, None, 1.0, seed, -1, false).expect("apply")
    }

    /// The Swift tests' `peakIndex(_:)`.
    fn peak_index(bins: &[f64]) -> usize {
        let mut best = 0;
        for (index, value) in bins.iter().enumerate() {
            if *value > bins[best] {
                best = index;
            }
        }
        best
    }

    #[test]
    fn camera_raw_defaults_are_identity() {
        let image = gray(128);
        let settings = CameraRawSettings::default();
        assert!(settings.is_identity());
        assert!(settings.is_valid());
        assert_eq!(settings.normalized(), settings);
        assert_eq!(
            settings
                .apply(&image, None, 1.0, 0, -1, false)
                .expect("apply"),
            image
        );
    }

    #[test]
    fn camera_raw_normalized_clamps_every_group() {
        let mut settings = CameraRawSettings {
            exposure: 9.0,
            contrast: -400.0,
            temperature: f64::NAN,
            grain_size: f64::INFINITY,
            vignette_midpoint: f64::INFINITY,
            ..CameraRawSettings::default()
        };
        settings.curve.shadow_split = 95.0;
        settings.curve.dark_split = 20.0;
        settings.optics.purple_hue_low = 320.0;
        settings.optics.purple_hue_high = 300.0;
        settings.detail.sharpen_amount = 400.0;
        settings.geometry.rotate = 90.0;
        settings.calibration.shadow_tint = -400.0;
        settings.mixer.hue[0] = 400.0;
        settings.grading.blending = -10.0;
        settings.mixer.points = vec![CameraRawPointColor {
            hue_range: 400.0,
            ..CameraRawPointColor::default()
        }];
        let normalized = settings.normalized();
        assert_eq!(normalized.exposure, 5.0);
        assert_eq!(normalized.contrast, -100.0);
        assert_eq!(normalized.temperature, 0.0, "a non-finite value falls back");
        assert_eq!(normalized.grain_size, 25.0);
        assert_eq!(normalized.vignette_midpoint, 50.0);
        assert_eq!(normalized.curve.shadow_split, 90.0);
        assert_eq!(normalized.curve.dark_split, 92.0, "the divider stays above the one before it");
        assert_eq!(normalized.curve.light_split, 94.0);
        assert_eq!(normalized.optics.purple_hue_low, 300.0, "the hue handles swap into order");
        assert_eq!(normalized.optics.purple_hue_high, 320.0);
        assert_eq!(normalized.detail.sharpen_amount, 150.0);
        assert_eq!(normalized.geometry.rotate, 45.0);
        assert_eq!(normalized.calibration.shadow_tint, -100.0);
        assert_eq!(normalized.mixer.hue[0], 100.0);
        assert_eq!(normalized.grading.blending, 0.0);
        assert_eq!(normalized.mixer.points[0].hue_range, 180.0);
    }

    #[test]
    fn camera_raw_temperature_warms_and_tint_magentaes() {
        let warm = CameraRawSettings {
            temperature: 100.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(warm.gains(), (1.35, 1.0, 0.65));
        let magenta = CameraRawSettings {
            tint: 100.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(magenta.gains(), (1.15, 0.7, 1.15));
        let graded = warm
            .apply(&gray(128), None, 1.0, 0, -1, false)
            .expect("apply");
        let pixel = graded.get(0, 0);
        assert!(pixel[0] > 128 && pixel[1] == 128 && pixel[2] < 128, "warm moves red against blue");
        assert_eq!(graded.get(1, 0), [0, 0, 0, 0], "transparent pixels are left alone");
    }

    #[test]
    fn camera_raw_exposure_of_one_stop_doubles_linear_light() {
        let settings = CameraRawSettings {
            exposure: 1.0,
            ..CameraRawSettings::default()
        };
        let graded = settings
            .apply(&gray(128), None, 1.0, 0, -1, false)
            .expect("apply");
        let value = graded.get(0, 0)[0];
        // 0.501961 sRGB is 0.215854 linear; doubled and encoded again it is 0.688485, or 175.56.
        assert!(
            (value as i32 - 176).abs() <= 1,
            "one stop of linear light on mid gray: {value}"
        );
        let darker = CameraRawSettings {
            exposure: -1.0,
            ..CameraRawSettings::default()
        }
        .apply(&gray(128), None, 1.0, 0, -1, false)
        .expect("apply");
        assert!(darker.get(0, 0)[0] < 128);
    }

    #[test]
    fn camera_raw_parametric_curve_bends_the_matching_tones() {
        let light = CameraRawCurveSettings {
            shadows: 100.0,
            ..CameraRawCurveSettings::default()
        };
        assert!(light.parametric(0.1) > 0.1, "Shadows +100 lifts the dark end");
        assert_eq!(light.parametric(0.5), 0.5, "the middle divider is left in place");
        let dark = CameraRawCurveSettings {
            highlights: -100.0,
            ..CameraRawCurveSettings::default()
        };
        assert!(dark.parametric(0.9) < 0.9, "Highlights -100 lowers the bright end");
        // The fit the Swift documents: Darks -51 dips the curve about 0.1 at a quarter of the way up.
        let fitted = CameraRawCurveSettings {
            darks: -51.0,
            ..CameraRawCurveSettings::default()
        };
        assert!(
            (fitted.parametric(0.25) - 0.15).abs() < 0.01,
            "Darks -51 at 0.25: {}",
            fitted.parametric(0.25)
        );
        let flat = CameraRawCurveSettings::default();
        assert_eq!(flat.parametric(0.37), 0.37);
    }

    #[test]
    fn camera_raw_curve_points_and_tables_round_trip() {
        let curve = CameraRawCurveSettings::default();
        assert_eq!(curve.tone_table()[128], 128.0 / 255.0);
        assert_eq!(curve.channel_table(&curve.red)[200], 200.0 / 255.0);
        let mut contrast = CameraRawCurveSettings {
            rgb: CameraRawCurveSettings::medium_contrast(),
            ..CameraRawCurveSettings::default()
        };
        // The handle nearest the tone takes the drag: on the medium-contrast curve tone 0.25 is the
        // interior handle's own, and on the linear curve it is nearer black than white.
        assert_eq!(
            contrast.nudged(CameraRawPointChannel::Rgb, 0.25, 0.1).rgb[1].y,
            0.28,
            "the nearest handle takes the drag"
        );
        let raised = curve.nudged(CameraRawPointChannel::Rgb, 0.25, 0.1);
        assert_eq!(raised.rgb[0].y, 0.1, "the linear curve's nearest handle is black");
        assert_eq!(raised.rgb[1].y, 1.0, "and the far one stays put");
        assert!(contrast.adjusts());
        assert!(contrast.normalized().tone_table()[64] < 64.0 / 255.0);
        // Repair drops handles that would collapse onto the one before them.
        contrast.rgb = vec![
            CurvePoint::new(0.0, 0.0),
            CurvePoint::new(0.5, 0.5),
            CurvePoint::new(0.505, 0.9),
            CurvePoint::new(1.0, 1.0),
        ];
        assert_eq!(contrast.normalized().rgb.len(), 3);
    }

    #[test]
    fn camera_raw_mixer_weights_overlap_neighbours() {
        assert_eq!(
            CameraRawMixerSettings::weights(0.0),
            [1.0, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(
            CameraRawMixerSettings::weights(300.0),
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.25, 1.0]
        );
    }

    #[test]
    fn camera_raw_neutralize_undoes_a_cast() {
        let solved = CameraRawSettings::neutralize_linear(1.0, 1.0, 0.8).expect("a solution");
        assert!((solved.0 - -32.967_033).abs() < 1e-5, "{:?}", solved);
        assert!((solved.1 - 25.641_026).abs() < 1e-5, "{:?}", solved);
        let settings = CameraRawSettings {
            temperature: solved.0,
            tint: solved.1,
            ..CameraRawSettings::default()
        };
        let gains = settings.gains();
        let (red, green, blue) = (1.0 * gains.0, 1.0 * gains.1, 0.8 * gains.2);
        assert!((red - green).abs() < 1e-12);
        assert!((blue - green).abs() < 1e-12);
        assert!(CameraRawSettings::neutralize_linear(0.0, 1.0, 1.0).is_none());
    }

    #[test]
    fn camera_raw_geometry_corners_follow_the_sliders() {
        let geometry = CameraRawGeometrySettings {
            vertical: 100.0,
            ..CameraRawGeometrySettings::default()
        };
        assert!(geometry.adjusts());
        assert_eq!(
            geometry.output_corners(100, 50, 100.0, 0.0, 0.0),
            [(-18.0, 50.0), (118.0, 50.0), (100.0, 0.0), (0.0, 0.0)]
        );
        let zoomed = CameraRawGeometrySettings {
            scale: 100.0,
            ..CameraRawGeometrySettings::default()
        };
        assert_eq!(
            zoomed.output_corners(100, 50, 0.0, 0.0, 0.0),
            [(-50.0, 75.0), (150.0, 75.0), (150.0, -25.0), (-50.0, -25.0)]
        );
        let rectilinear = CameraRawGeometrySettings {
            projection: CameraRawProjection::Rectilinear,
            ..CameraRawGeometrySettings::default()
        };
        assert!(
            (rectilinear.output_corners(100, 50, 100.0, 0.0, 0.0)[0].0 + 9.9).abs() < 1e-12,
            "Rectilinear corrects at 0.55 of Perspective's strength"
        );
        assert!(!CameraRawGeometrySettings::default().adjusts());
        // An empty Guided choice must not warp the picture.
        let guided = CameraRawGeometrySettings {
            upright: CameraRawUprightMode::Guided,
            ..CameraRawGeometrySettings::default()
        };
        assert!(!guided.adjusts());
        let guided = CameraRawGeometrySettings {
            upright: CameraRawUprightMode::Guided,
            guides: vec![CameraRawGeometryGuide::new(0.4, 0.9, 0.6, 0.9)],
            ..CameraRawGeometrySettings::default()
        };
        assert!(guided.adjusts());
        assert_eq!(guided.effective_corrections(), (0.0, 0.0, 0.0), "a level line corrects nothing");
        let tilted = CameraRawGeometrySettings {
            upright: CameraRawUprightMode::Guided,
            guides: vec![CameraRawGeometryGuide::new(0.4, 0.4, 0.6, 0.6)],
            ..CameraRawGeometrySettings::default()
        };
        assert_eq!(
            tilted.effective_corrections(),
            (0.0, 0.0, -45.0),
            "a 45 degree guide turns the picture back to level"
        );
        let pair = CameraRawGeometrySettings {
            upright: CameraRawUprightMode::Guided,
            guides: vec![
                CameraRawGeometryGuide::new(0.4, 0.4, 0.6, 0.6),
                CameraRawGeometryGuide::new(0.5, 0.5, 0.9, 0.5),
            ],
            ..CameraRawGeometrySettings::default()
        };
        assert_eq!(pair.effective_corrections(), (0.0, -25.0, -45.0));
    }

    #[test]
    fn camera_raw_perspective_warp_identity_quad_is_pixel_exact() {
        let image = Rgba8Image::from_data(
            2,
            2,
            vec![10, 20, 30, 255, 40, 50, 60, 128, 70, 80, 90, 255, 0, 0, 0, 0],
        );
        let quad = [(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)];
        assert_eq!(perspective_warp(&image, quad), image);
    }

    #[test]
    fn camera_raw_scope_counts_channels_and_vectorscope() {
        let image = Rgba8Image::from_data(2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]);
        let scope = CameraRawScope::make(&image).expect("a scope");
        assert_eq!(scope.red[255], 1.0, "the opaque red pixel lands in the last red bin");
        // The transparent pixel would land in bin 0 of every channel; the opaque red pixel only in
        // the last red bin, so an empty red bin 0 is what shows it was skipped.
        assert_eq!(scope.red[0], 0.0, "the transparent pixel is skipped");
        assert_eq!(scope.peak(), 1.0);
        let lit = scope.vectorscope.iter().filter(|value| **value > 0.0).count();
        assert_eq!(lit, 1, "one plot per opaque pixel");
        assert_eq!(scope.vectorscope[32 * CameraRawScope::SCOPE_SIDE + 62], 1.0);
    }

    #[test]
    fn camera_raw_histogram_scale_caps_isolated_spikes() {
        assert_eq!(histogram_scale(&[0.0; 256]), 0.0);
        assert_eq!(histogram_scale(&[100.0; 256]), 100.0);
        let mut spiked = vec![10.0; 256];
        spiked[0] = 10_000.0;
        assert_eq!(histogram_scale(&spiked), 40.0);
    }

    #[test]
    fn camera_raw_clipping_view_runs_on_identity_settings() {
        // A white pixel (every channel clipped), a mid gray and a black one (none), and a
        // transparent pixel — which no view touches.
        let image = Rgba8Image::from_data(
            4,
            1,
            vec![255, 255, 255, 255, 128, 128, 128, 255, 0, 0, 0, 255, 0, 0, 0, 0],
        );
        let settings = CameraRawSettings::default();
        let clipped = settings
            .apply(&image, Some(CameraRawClipping::Highlights), 1.0, 0, -1, false)
            .expect("apply");
        assert_eq!(clipped.get(0, 0), [255, 255, 255, 255], "clipped channels lit on black");
        assert_eq!(clipped.get(1, 0), [0, 0, 0, 255], "an unclipped pixel comes back black");
        assert_eq!(clipped.get(2, 0), [0, 0, 0, 255]);
        assert_eq!(clipped.get(3, 0), [0, 0, 0, 0], "the transparent pixel is left alone");
        let shadow = settings
            .apply(&image, Some(CameraRawClipping::Shadows), 1.0, 0, -1, false)
            .expect("apply");
        assert_eq!(shadow.get(0, 0), [255, 255, 255, 255], "nothing is dark, so nothing is marked");
        assert_eq!(shadow.get(1, 0), [255, 255, 255, 255]);
        assert_eq!(shadow.get(2, 0), [0, 0, 0, 255], "a clipped channel goes dark");
        assert_eq!(shadow.get(3, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn camera_raw_sharpen_mask_replaces_the_detail_group() {
        // An opaque mid gray: the mask overlay scales its gray by the pixel's alpha and leaves the
        // alpha alone, as the C does, so a translucent source would keep its own alpha.
        let image = Rgba8Image::from_data(4, 4, (0..4 * 4).flat_map(|_| [128u8, 128, 128, 255]).collect());
        let mut settings = CameraRawSettings::default();
        settings.detail.sharpen_radius = 0.0;
        settings.detail.sharpen_detail = 0.0;
        let masked = settings
            .apply(&image, None, 1.0, 0, -1, true)
            .expect("apply");
        for pixel in masked.pixels() {
            assert_eq!(&pixel[0..3], &[0, 0, 0], "a flat image has nothing to sharpen");
            assert_eq!(pixel[3], 255);
        }
        assert_eq!(
            settings.apply(&image, None, 1.0, 0, -1, false).expect("apply"),
            image,
            "the plain grade is still identity"
        );
    }

    #[test]
    fn camera_raw_group_eyes_reset_their_group() {
        let settings = CameraRawSettings {
            exposure: 2.0,
            saturation: 50.0,
            texture: 20.0,
            ..CameraRawSettings::default()
        };
        let color_only = settings.applying(
            false, true, true, true, true, true, true, true, true, true,
        );
        assert_eq!(color_only.exposure, 0.0);
        assert_eq!(color_only.saturation, 50.0);
        let mut noisy = settings.clone();
        noisy.detail.noise_luminance = 40.0;
        noisy.optics.green_amount = 30.0;
        noisy.curve.shadows = 10.0;
        let quiet = noisy.applying(true, true, true, false, true, true, false, false, true, true);
        assert_eq!(quiet.detail, CameraRawDetailSettings::default());
        assert_eq!(quiet.optics, CameraRawOpticsSettings::default());
        assert_eq!(quiet.curve, CameraRawCurveSettings::default());
        assert_eq!(quiet.grading, noisy.grading, "untouched groups stay");
    }

    #[test]
    fn camera_raw_grain_kernel_size_matches_the_slider() {
        let mut settings = CameraRawSettings::default();
        settings.grain_size = 0.0;
        assert_eq!(settings.grain_kernel_size(), 0.5);
        settings.grain_size = 25.0;
        assert_eq!(settings.grain_kernel_size(), 5.375);
        settings.grain_size = 100.0;
        assert_eq!(settings.grain_kernel_size(), 20.0);
    }

    #[test]
    fn camera_raw_optic_distortion_k_combines_manual_and_profile() {
        let strength = LENS_STRENGTH;
        let mut optics = CameraRawOpticsSettings::default();
        assert_eq!(optics.distortion_k(strength), 0.0);
        optics.distortion = 50.0;
        assert!((optics.distortion_k(strength) - 0.175).abs() < 1e-12);
        optics.enable_lens_profile = true;
        assert!((optics.distortion_k(strength) - (0.175 + strength)).abs() < 1e-12);
        optics.enable_lens_profile = false;
        optics.profile_distortion = 50.0;
        optics.enable_lens_profile = true;
        assert!((optics.distortion_k(strength) - 0.35).abs() < 1e-12);
    }

    /// `defaultsLeavePixelsAndAlphaAlone`. The opaque-plus-transparent pair is
    /// `camera_raw_defaults_are_identity`; this is the half-transparent round trip and the invalid
    /// slider fallback. (`!FilterKind.cameraRaw.isImageAdjustment` is compositor-core's.)
    #[test]
    fn defaults_leave_pixels_and_alpha_alone() {
        let input = solid(2, 1, 128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 0.5);
        let settings = CameraRawSettings::default();
        assert_eq!(grade(&settings, &input), input);
        assert_eq!(straight(&grade(&settings, &input), 0, 0), straight(&input, 0, 0));

        let broken = CameraRawSettings {
            exposure: f64::NAN,
            temperature: 400.0,
            ..CameraRawSettings::default()
        };
        assert!(!broken.is_valid());
        let normalized = broken.normalized();
        assert_eq!(normalized.exposure, 0.0);
        assert_eq!(normalized.temperature, 100.0);
    }

    /// `exposureAddsOneStopAndContrastPivotsAroundMidGray`. The one-stop half is
    /// `camera_raw_exposure_of_one_stop_doubles_linear_light`; the contrast pivot around mid gray,
    /// and the alpha a translucent pixel keeps, are here.
    #[test]
    fn exposure_adds_one_stop_and_contrast_pivots_around_mid_gray() {
        let settings = CameraRawSettings {
            exposure: 1.0,
            ..CameraRawSettings::default()
        };
        let translucent = solid(2, 1, 128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 0.5);
        assert_eq!(
            straight(&grade(&settings, &translucent), 0, 0)[3],
            straight(&translucent, 0, 0)[3]
        );

        let pair = Rgba8Image::from_data(2, 1, vec![64, 64, 64, 255, 192, 192, 192, 255]);
        let pushed = grade(
            &CameraRawSettings {
                contrast: 100.0,
                ..CameraRawSettings::default()
            },
            &pair,
        );
        assert!(
            byte(&pushed, 0, 0) < 10 && byte(&pushed, 1, 0) > 250,
            "contrast +100 drives the pair apart: {:?}",
            straight(&pushed, 0, 0)
        );
        let flat = grade(
            &CameraRawSettings {
                contrast: -100.0,
                ..CameraRawSettings::default()
            },
            &pair,
        );
        assert!(
            (byte(&flat, 0, 0) - 128).abs() <= 2 && (byte(&flat, 1, 0) - 128).abs() <= 2,
            "contrast −100 meets at mid gray: {flat:?}"
        );
    }

    /// `tonalSlidersMoveTheEndTheyName`: each end slider moves its own tone and leaves mid gray,
    /// and a clipping view returns white/black rather than the grade.
    #[test]
    fn tonal_sliders_move_the_end_they_name() {
        let bright_and_mid = Rgba8Image::from_data(2, 1, vec![230, 230, 230, 255, 128, 128, 128, 255]);
        let recovered = grade(
            &CameraRawSettings {
                highlights: -100.0,
                ..CameraRawSettings::default()
            },
            &bright_and_mid,
        );
        assert!(
            byte(&recovered, 0, 0) < 200,
            "highlights −100 darkens the bright tone: {:?}",
            straight(&recovered, 0, 0)
        );
        assert!((byte(&recovered, 1, 0) - 128).abs() <= 2, "and leaves mid gray");

        let settings = CameraRawSettings {
            whites: 100.0,
            ..CameraRawSettings::default()
        };
        let clipped = grade(&settings, &bright_and_mid);
        assert_eq!(byte(&clipped, 0, 0), 255, "whites +100 clips the bright tone");
        assert!((byte(&clipped, 1, 0) - 128).abs() <= 2, "not the midtone");
        let viz = settings
            .apply(
                &bright_and_mid,
                Some(CameraRawClipping::Highlights),
                1.0,
                0,
                -1,
                false,
            )
            .expect("apply");
        assert_eq!(
            straight(&viz, 0, 0),
            [255, 255, 255, 255],
            "highlight clipping is not the grade"
        );
        assert_eq!(straight(&viz, 1, 0), [0, 0, 0, 255]);

        let dark_and_mid = Rgba8Image::from_data(2, 1, vec![20, 20, 20, 255, 128, 128, 128, 255]);
        let opened = grade(
            &CameraRawSettings {
                shadows: 100.0,
                ..CameraRawSettings::default()
            },
            &dark_and_mid,
        );
        assert!(
            byte(&opened, 0, 0) > 50,
            "shadows +100 opens the dark tone: {:?}",
            straight(&opened, 0, 0)
        );
        assert!((byte(&opened, 1, 0) - 128).abs() <= 2, "and leaves mid gray");

        let settings = CameraRawSettings {
            blacks: -100.0,
            ..CameraRawSettings::default()
        };
        let crushed = grade(&settings, &dark_and_mid);
        assert!(
            byte(&crushed, 0, 0) < 20,
            "blacks −100 crushes the dark tone: {:?}",
            straight(&crushed, 0, 0)
        );
        assert!((byte(&crushed, 1, 0) - 128).abs() <= 2);
        let shadow_viz = settings
            .apply(
                &dark_and_mid,
                Some(CameraRawClipping::Shadows),
                1.0,
                0,
                -1,
                false,
            )
            .expect("apply");
        assert_eq!(straight(&shadow_viz, 0, 0), [0, 0, 0, 255]);
        assert_eq!(
            straight(&shadow_viz, 1, 0),
            [255, 255, 255, 255],
            "shadow clipping is not the grade"
        );
    }

    /// `temperatureWarmsAndTintMovesTowardMagenta`. The warm half is
    /// `camera_raw_temperature_warms_and_tint_magentaes`; this is the applied magenta pixel.
    #[test]
    fn temperature_warms_and_tint_moves_toward_magenta() {
        let magenta = grade(
            &CameraRawSettings {
                tint: 100.0,
                ..CameraRawSettings::default()
            },
            &mid_gray(2, 2),
        );
        let pixel = straight(&magenta, 0, 0);
        assert!(
            pixel[1] < 128 && pixel[1] < pixel[0] && pixel[1] < pixel[2],
            "magenta lowers green: {pixel:?}"
        );
    }

    /// `vibranceFavorsDullColorsAndProtectsSkinWhileSaturationDoesNot`.
    #[test]
    fn vibrance_favors_dull_colors_and_protects_skin_while_saturation_does_not() {
        let dull_green = solid(4, 4, 77.0 / 255.0, 153.0 / 255.0, 77.0 / 255.0, 1.0);
        let saturated_green = solid(4, 4, 20.0 / 255.0, 200.0 / 255.0, 20.0 / 255.0, 1.0);
        let skin = solid(4, 4, 153.0 / 255.0, 115.0 / 255.0, 77.0 / 255.0, 1.0);
        let settings = CameraRawSettings {
            vibrance: 100.0,
            ..CameraRawSettings::default()
        };
        let dull_before = chroma(straight(&dull_green, 0, 0));
        let saturated_before = chroma(straight(&saturated_green, 0, 0));
        let skin_before = chroma(straight(&skin, 0, 0));
        let dull_delta = chroma(straight(&grade(&settings, &dull_green), 0, 0)) - dull_before;
        let saturated_delta =
            chroma(straight(&grade(&settings, &saturated_green), 0, 0)) - saturated_before;
        let skin_delta = chroma(straight(&grade(&settings, &skin), 0, 0)) - skin_before;
        assert!(
            dull_delta > saturated_delta + 10,
            "dull {dull_delta} vs saturated {saturated_delta}"
        );
        assert_eq!(dull_before, skin_before);
        assert!(
            dull_delta > skin_delta + 10,
            "skin is protected at the same saturation: {skin_delta} vs {dull_delta}"
        );

        let red = solid(4, 4, 160.0 / 255.0, 120.0 / 255.0, 120.0 / 255.0, 1.0);
        let blue = solid(4, 4, 100.0 / 255.0, 100.0 / 255.0, 140.0 / 255.0, 1.0);
        let settings = CameraRawSettings {
            saturation: 100.0,
            ..CameraRawSettings::default()
        };
        let red_before = chroma(straight(&red, 0, 0));
        let blue_before = chroma(straight(&blue, 0, 0));
        let red_ratio = f64::from(chroma(straight(&grade(&settings, &red), 0, 0)))
            / f64::from(red_before);
        let blue_ratio = f64::from(chroma(straight(&grade(&settings, &blue), 0, 0)))
            / f64::from(blue_before);
        assert!(
            (red_ratio - 2.0).abs() < 0.15 && (blue_ratio - 2.0).abs() < 0.15,
            "saturation doubles both: {red_ratio}, {blue_ratio}"
        );
    }

    /// `eyedropperAndAutoNeutralizeAWarmPixel`: the eyedropper's solved balance and Auto's both pull
    /// the cast out. The session half (`sampleCameraRawWhiteBalance`, the transparent-pixel skip) is
    /// compositor-session's.
    #[test]
    fn eyedropper_and_auto_neutralize_a_warm_pixel() {
        let (red, green, blue) = (160.0 / 255.0, 140.0 / 255.0, 120.0 / 255.0);
        let warm = solid(8, 8, red, green, blue, 1.0);
        let before = straight(&warm, 0, 0);
        let solved = CameraRawSettings::neutralize_straight(red, green, blue).expect("a solution");
        let settings = CameraRawSettings {
            temperature: solved.0,
            tint: solved.1,
            ..CameraRawSettings::default()
        };
        let neutral = straight(&grade(&settings, &warm), 0, 0);
        assert!(
            chroma(neutral) < chroma(before) / 2,
            "eyedropper pulls the cast in: {before:?} → {neutral:?}"
        );

        let auto = CameraRawSettings::auto_balance(&warm).expect("a solution");
        let settings = CameraRawSettings {
            temperature: auto.0,
            tint: auto.1,
            ..CameraRawSettings::default()
        };
        let averaged = straight(&grade(&settings, &warm), 0, 0);
        assert!(
            chroma(averaged) < chroma(before) / 2,
            "auto matches the solid color: {averaged:?}"
        );
    }

    /// `textureAndClaritySharpenAnEdgeAndLeaveAFlatField`.
    #[test]
    fn texture_and_clarity_sharpen_an_edge_and_leave_a_flat_field() {
        let flat = mid_gray(4, 4);
        let settings = CameraRawSettings {
            texture: 100.0,
            clarity: 100.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(grade(&settings, &flat), flat, "a flat field has no local contrast");

        let edge = step();
        let original_far = byte(&edge, 0, 0);
        let original_near = byte(&edge, 8, 0);
        let original_edge = byte(&edge, 11, 0);
        let textured = grade(
            &CameraRawSettings {
                texture: 100.0,
                ..CameraRawSettings::default()
            },
            &edge,
        );
        assert_eq!(byte(&textured, 0, 0), original_far);
        assert_eq!(
            byte(&textured, 8, 0),
            original_near,
            "texture's fine radius does not reach this far"
        );
        assert_ne!(byte(&textured, 11, 0), original_edge);

        let clarified = grade(
            &CameraRawSettings {
                clarity: 100.0,
                ..CameraRawSettings::default()
            },
            &edge,
        );
        assert_eq!(byte(&clarified, 0, 0), original_far);
        assert_ne!(
            byte(&clarified, 8, 0),
            original_near,
            "clarity's wider radius reaches further in"
        );
        let softened = grade(
            &CameraRawSettings {
                clarity: -100.0,
                ..CameraRawSettings::default()
            },
            &edge,
        );
        let hard_gap = (byte(&edge, 12, 0) - byte(&edge, 11, 0)).abs();
        let soft_gap = (byte(&softened, 12, 0) - byte(&softened, 11, 0)).abs();
        assert!(
            soft_gap < hard_gap,
            "negative clarity pulls the step together: {soft_gap} vs {hard_gap}"
        );
    }

    /// `dehazeDeepensOrLiftsAndKeepsAlpha`.
    #[test]
    fn dehaze_deepens_or_lifts_and_keeps_alpha() {
        let dark = solid(4, 4, 30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0);
        let pale = solid(4, 4, 180.0 / 255.0, 150.0 / 255.0, 150.0 / 255.0, 1.0);
        let settings = CameraRawSettings {
            dehaze: 100.0,
            ..CameraRawSettings::default()
        };
        let deepened = straight(&grade(&settings, &dark), 0, 0);
        assert!(deepened[0] < 30, "positive dehaze darkens a shadow: {deepened:?}");
        let pale_before = straight(&pale, 0, 0);
        let pale_after = straight(&grade(&settings, &pale), 0, 0);
        assert!(
            chroma(pale_after) > chroma(pale_before),
            "and raises saturation: {pale_before:?} → {pale_after:?}"
        );
        let settings = CameraRawSettings {
            dehaze: -100.0,
            ..CameraRawSettings::default()
        };
        let lifted = straight(&grade(&settings, &dark), 0, 0);
        assert!(lifted[0] > 30, "negative dehaze lifts a shadow: {lifted:?}");
        let faded = straight(&grade(&settings, &pale), 0, 0);
        assert!(chroma(faded) < chroma(pale_before), "and lowers saturation: {faded:?}");
        let translucent = solid(4, 4, 30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 0.5);
        let settings = CameraRawSettings {
            dehaze: 100.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(
            straight(&grade(&settings, &translucent), 0, 0)[3],
            straight(&translucent, 0, 0)[3]
        );
    }

    /// `glowIsIdleAtZeroAndHalationFringeIsRedderThanDiffusion`.
    #[test]
    fn glow_is_idle_at_zero_and_halation_fringe_is_redder_than_diffusion() {
        // The Swift fills the frame opaque black, then a white 5×5 square at (8, 8).
        let mut spot = Rgba8Image::opaque(21, 21, [0, 0, 0, 255]);
        for y in 8..13 {
            for x in 8..13 {
                spot.set(x, y, [255, 255, 255, 255]);
            }
        }
        let idle = CameraRawSettings {
            glow_warmth: 100.0,
            glow_range: 100.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(
            grade(&idle, &spot),
            spot,
            "range and warmth do nothing until Glow is raised"
        );

        let diffusion_settings = CameraRawSettings {
            glow: 100.0,
            glow_warmth: 100.0,
            glow_style: CameraRawGlowStyle::Diffusion,
            ..CameraRawSettings::default()
        };
        let halation_settings = CameraRawSettings {
            glow_style: CameraRawGlowStyle::Halation,
            ..diffusion_settings.clone()
        };
        let diffusion = grade(&diffusion_settings, &spot);
        let halation = grade(&halation_settings, &spot);
        let fringe = straight(&diffusion, 15, 10);
        let far = straight(&diffusion, 0, 0);
        assert!(
            fringe[0] > far[0],
            "glow brightens the neighborhood: {fringe:?}"
        );
        assert_eq!(straight(&halation, 0, 0), [0, 0, 0, 255]);
        let halation_fringe = straight(&halation, 15, 10);
        assert!(
            halation_fringe[0] - halation_fringe[1] > fringe[0] - fringe[1],
            "halation is redder than diffusion at the same warmth: {halation_fringe:?} vs {fringe:?}"
        );
    }

    /// `vignetteDarkensCornersAndHighlightsOnlyWhileDarkening`.
    #[test]
    fn vignette_darkens_corners_and_highlights_only_while_darkening() {
        let gray_field = mid_gray(9, 9);
        let darkened = grade(
            &CameraRawSettings {
                vignette_amount: -100.0,
                ..CameraRawSettings::default()
            },
            &gray_field,
        );
        let center = byte(&darkened, 4, 4);
        let corner = byte(&darkened, 0, 0);
        assert!((center - 128).abs() <= 2, "the center stays: {center}");
        assert!(corner < center - 40, "the corner darkens: {corner}");

        let white = solid(9, 9, 1.0, 1.0, 1.0, 1.0);
        let mut settings = CameraRawSettings {
            vignette_amount: -100.0,
            vignette_highlights: 100.0,
            vignette_style: CameraRawVignetteStyle::HighlightPriority,
            ..CameraRawSettings::default()
        };
        let protected = byte(&grade(&settings, &white), 0, 0);
        settings.vignette_highlights = 0.0;
        let exposed = byte(&grade(&settings, &white), 0, 0);
        assert!(
            protected > exposed + 40,
            "highlights holds a bright corner: {protected} vs {exposed}"
        );
        settings.vignette_highlights = 100.0;
        settings.vignette_style = CameraRawVignetteStyle::PaintOverlay;
        let painted = byte(&grade(&settings, &white), 0, 0);
        assert!(
            painted < protected,
            "paint overlay does not use Highlights: {painted}"
        );

        let plain = grade(
            &CameraRawSettings {
                vignette_amount: 100.0,
                vignette_highlights: 0.0,
                ..CameraRawSettings::default()
            },
            &gray_field,
        );
        let with_highlights = grade(
            &CameraRawSettings {
                vignette_amount: 100.0,
                vignette_highlights: 100.0,
                ..CameraRawSettings::default()
            },
            &gray_field,
        );
        assert_eq!(plain, with_highlights, "highlights is idle while the vignette lightens");
    }

    /// `grainIsStableAndTheEffectsEyeDropsTheWholeGroup`.
    #[test]
    fn grain_is_stable_and_the_effects_eye_drops_the_whole_group() {
        let field = mid_gray(16, 16);
        let mut settings = CameraRawSettings {
            grain_size: 40.0,
            grain_roughness: 80.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(
            grade_seed(&settings, &field, 4),
            field,
            "size and roughness do nothing at amount zero"
        );
        settings.grain_amount = 70.0;
        let first = grade_seed(&settings, &field, 4);
        let second = grade_seed(&settings, &field, 4);
        assert_eq!(first, second, "the same seed keeps the same grain");
        assert_ne!(first, field);
        let pixel = straight(&first, 0, 0);
        assert!(
            pixel[0] == pixel[1] && pixel[1] == pixel[2],
            "grain moves brightness only: {pixel:?}"
        );
        let clear = solid(4, 4, 0.5, 0.5, 0.5, 0.0);
        assert_eq!(straight(&grade_seed(&settings, &clear, 4), 0, 0)[3], 0);

        let edge = step();
        let settings = CameraRawSettings {
            texture: 100.0,
            grain_amount: 50.0,
            ..CameraRawSettings::default()
        };
        let shown = grade_seed(&settings, &edge, 2);
        let hidden = settings.applying(true, true, false, true, true, true, true, true, true, true);
        assert!(hidden.is_identity());
        assert_eq!(grade_seed(&hidden, &edge, 2), edge);
        assert_ne!(shown, edge);
    }

    /// `histogramFollowsTheGradeAndClippingPaintStaysOffTheResult`. The red end of the vectorscope is
    /// `camera_raw_scope_counts_channels_and_vectorscope`.
    #[test]
    fn histogram_follows_the_grade_and_clipping_paint_stays_off_the_result() {
        let black = solid(1, 1, 0.0, 0.0, 0.0, 1.0);
        let white = solid(1, 1, 1.0, 1.0, 1.0, 1.0);
        assert_eq!(peak_index(&CameraRawScope::make(&black).expect("a scope").red), 0);
        assert_eq!(
            peak_index(&CameraRawScope::make(&white).expect("a scope").red),
            255
        );
        let settings = CameraRawSettings {
            exposure: 1.0,
            ..CameraRawSettings::default()
        };
        let shifted = CameraRawScope::make(&grade(&settings, &mid_gray(4, 4))).expect("a scope");
        assert!(
            peak_index(&shifted.red) > 128,
            "exposure moves the midtones toward white"
        );

        let shadowed = straight(
            &CameraRawScope::overlay(&black, true, false).expect("overlay"),
            0,
            0,
        );
        assert!(
            shadowed[2] > shadowed[0],
            "clipped shadows are painted blue: {shadowed:?}"
        );
        let highlighted = straight(
            &CameraRawScope::overlay(&white, false, true).expect("overlay"),
            0,
            0,
        );
        assert!(
            highlighted[0] > highlighted[2],
            "clipped highlights are painted red: {highlighted:?}"
        );
        assert_eq!(
            CameraRawScope::overlay(&black, false, false).expect("overlay"),
            black
        );
    }

    /// `curveMixerAndGradingChangeOnlyTheirOwnTones`.
    #[test]
    fn curve_mixer_and_grading_change_only_their_own_tones() {
        let dark = solid(4, 4, 0.12, 0.12, 0.12, 1.0);
        let light = solid(4, 4, 0.62, 0.62, 0.62, 1.0);
        let settings = CameraRawSettings {
            curve: CameraRawCurveSettings {
                shadows: 100.0,
                ..CameraRawCurveSettings::default()
            },
            ..CameraRawSettings::default()
        };
        let dark_gain = byte(&grade(&settings, &dark), 0, 0) - (0.12_f64 * 255.0).round() as i32;
        let light_gain = byte(&grade(&settings, &light), 0, 0) - (0.62_f64 * 255.0).round() as i32;
        assert!(
            dark_gain > light_gain + 8,
            "parametric shadows lift the dark tone more: {dark_gain} vs {light_gain}"
        );

        let settings = CameraRawSettings {
            curve: CameraRawCurveSettings {
                rgb: CameraRawCurveSettings::strong_contrast(),
                ..CameraRawCurveSettings::default()
            },
            ..CameraRawSettings::default()
        };
        let contrasted = byte(&grade(&settings, &solid(4, 4, 0.25, 0.25, 0.25, 1.0)), 0, 0);
        assert!(
            contrasted < 55,
            "strong contrast pulls a dark midtone down: {contrasted}"
        );

        let mut mixer = CameraRawMixerSettings::default();
        mixer.hue[0] = 100.0;
        let settings = CameraRawSettings {
            mixer,
            ..CameraRawSettings::default()
        };
        let shifted = straight(&grade(&settings, &solid(4, 4, 1.0, 0.0, 0.0, 1.0)), 0, 0);
        assert!(
            shifted[1] > shifted[2],
            "reds hue moves red toward orange: {shifted:?}"
        );

        let mut grading = CameraRawGradingSettings::default();
        grading.shadows.saturation = 100.0;
        let settings = CameraRawSettings {
            grading,
            ..CameraRawSettings::default()
        };
        let graded_dark = straight(&grade(&settings, &dark), 0, 0);
        let graded_light = straight(&grade(&settings, &solid(4, 4, 1.0, 1.0, 1.0, 1.0)), 0, 0);
        assert!(
            graded_dark[0] > graded_dark[1] + 5,
            "shadow grading tints a dark pixel: {graded_dark:?}"
        );
        assert!(
            (graded_light[0] - graded_light[1]).abs() <= 2,
            "and leaves white alone: {graded_light:?}"
        );
        let balanced_settings = CameraRawSettings {
            grading: CameraRawGradingSettings {
                balance: 100.0,
                ..settings.grading
            },
            ..settings.clone()
        };
        let balanced = straight(&grade(&balanced_settings, &dark), 0, 0);
        assert!(
            balanced[0] - balanced[1] < graded_dark[0] - graded_dark[1],
            "balance toward highlights weakens the shadow tint"
        );

        let hidden = CameraRawSettings {
            curve: CameraRawCurveSettings {
                shadows: 100.0,
                ..CameraRawCurveSettings::default()
            },
            ..CameraRawSettings::default()
        }
        .applying(true, true, true, false, true, true, true, true, true, true);
        assert_eq!(grade(&hidden, &dark), dark);
    }

    /// `parametricCurveIsSmooth`: one smooth, rising curve with fixed ends and no kinks at the dividers.
    #[test]
    fn parametric_curve_is_smooth() {
        let mut curve = CameraRawCurveSettings::default();
        assert_eq!(curve.parametric(0.3), 0.3);
        for amounts in [
            (100.0, 0.0, 0.0, 0.0),
            (0.0, 100.0, -100.0, 0.0),
            (-100.0, 50.0, 100.0, -60.0),
        ] {
            curve.shadows = amounts.0;
            curve.darks = amounts.1;
            curve.lights = amounts.2;
            curve.highlights = amounts.3;
            let samples: Vec<f64> = (0..=200)
                .map(|index| curve.parametric(index as f64 / 200.0))
                .collect();
            assert_eq!(
                samples.first().copied(),
                Some(0.0),
                "{amounts:?}: the ends stay black and white"
            );
            assert_eq!(
                samples.last().copied(),
                Some(1.0),
                "{amounts:?}: the ends stay black and white"
            );
            assert!(
                samples.windows(2).all(|pair| pair[1] >= pair[0] - 1e-9),
                "{amounts:?}: the curve keeps rising"
            );
            // Sampled twice as finely, the largest step-to-step change of slope about halves. At a
            // corner, where one region stopped dead and the next began, it stays the same.
            fn jump(curve: &CameraRawCurveSettings, steps: usize) -> f64 {
                let values: Vec<f64> = (0..=steps)
                    .map(|index| curve.parametric(index as f64 / steps as f64))
                    .collect();
                let slopes: Vec<f64> = values
                    .windows(2)
                    .map(|pair| (pair[1] - pair[0]) * steps as f64)
                    .collect();
                slopes
                    .windows(2)
                    .map(|pair| (pair[1] - pair[0]).abs())
                    .fold(0.0, f64::max)
            }
            assert!(
                jump(&curve, 400) < jump(&curve, 200) * 0.7,
                "{amounts:?}: a corner in the curve"
            );
        }
        let mut curve = CameraRawCurveSettings::default();
        curve.darks = 100.0;
        let before = curve.parametric(0.4);
        curve.dark_split = 70.0;
        assert!(
            curve.parametric(0.4) > before,
            "widening the darks region spreads its lift"
        );
    }

    /// `parametricCurveMatchesPhotoshop`: the trace of Camera Raw 18.6 at the default dividers.
    #[test]
    fn parametric_curve_matches_photoshop() {
        let mut curve = CameraRawCurveSettings::default();
        curve.darks = -51.0;
        curve.lights = 59.0;
        let photoshop: [(f64, f64); 10] = [
            (0.093, 0.011),
            (0.192, 0.089),
            (0.267, 0.174),
            (0.367, 0.310),
            (0.491, 0.498),
            (0.616, 0.698),
            (0.690, 0.804),
            (0.765, 0.886),
            (0.840, 0.947),
            (0.915, 0.982),
        ];
        for (tone, expected) in photoshop {
            assert!(
                (curve.parametric(tone) - expected).abs() < 0.035,
                "at {tone}: {}, Photoshop {expected}",
                curve.parametric(tone)
            );
        }
    }

    /// `curveDeepensColorLikePhotoshop`: the curve works on red, green and blue alike, and Refine
    /// Saturation −100 keeps it to brightness.
    #[test]
    fn curve_deepens_color_like_photoshop() {
        let orange = solid(4, 4, 0.85, 0.35, 0.1, 1.0);
        let mut curve = CameraRawCurveSettings::default();
        curve.darks = -51.0;
        curve.lights = 59.0;
        let settings = CameraRawSettings {
            curve,
            ..CameraRawSettings::default()
        };
        let curved = straight(&grade(&settings, &orange), 0, 0);
        assert!(
            curved[0] > 230 && curved[1] < 80,
            "red up and green down, as Photoshop's: {curved:?}"
        );
        let mut curve = CameraRawCurveSettings::default();
        curve.darks = -51.0;
        curve.lights = 59.0;
        curve.refine_saturation = -100.0;
        let settings = CameraRawSettings {
            curve,
            ..CameraRawSettings::default()
        };
        let brightness = straight(&grade(&settings, &orange), 0, 0);
        assert!(
            f64::from(brightness[1]) / f64::from(brightness[0]) > 0.35,
            "brightness only keeps orange orange: {brightness:?}"
        );
    }

    /// `detailSharpeningNoiseAndMaskingPreview`. The mask overlay on a flat field is
    /// `camera_raw_sharpen_mask_replaces_the_detail_group`.
    #[test]
    fn detail_sharpening_noise_and_masking_preview() {
        let edge = step();
        let mut detail = CameraRawDetailSettings {
            sharpen_amount: 150.0,
            sharpen_radius: 50.0,
            ..CameraRawDetailSettings::default()
        };
        let settings = CameraRawSettings {
            detail,
            ..CameraRawSettings::default()
        };
        assert_ne!(grade(&settings, &edge), edge, "sharpening changes the step image");

        detail = CameraRawDetailSettings {
            noise_luminance: 80.0,
            ..CameraRawDetailSettings::default()
        };
        let settings = CameraRawSettings {
            detail,
            ..CameraRawSettings::default()
        };
        let flat = mid_gray(8, 8);
        assert_eq!(grade(&settings, &flat), flat, "luminance NR leaves a flat field alone");

        detail.sharpen_masking = 50.0;
        let settings = CameraRawSettings {
            detail,
            ..CameraRawSettings::default()
        };
        let mask = settings
            .apply(&edge, None, 1.0, 0, -1, true)
            .expect("apply");
        for pixel in mask.pixels() {
            assert!(
                pixel[0] == pixel[1] && pixel[1] == pixel[2],
                "mask preview is grayscale: {pixel:?}"
            );
        }
        assert!(settings.detail.adjusts());
    }

    /// `opticsDistortionDefringeAndDetailEye`. The distortion strength itself is
    /// `camera_raw_optic_distortion_k_combines_manual_and_profile`.
    #[test]
    fn optics_distortion_defringe_and_detail_eye() {
        let stepped = checker(24, 24);
        let settings = CameraRawSettings {
            optics: CameraRawOpticsSettings {
                distortion: 100.0,
                ..CameraRawOpticsSettings::default()
            },
            ..CameraRawSettings::default()
        };
        assert_ne!(
            grade(&settings, &stepped),
            stepped,
            "distortion resamples pixels"
        );

        let purple = solid(4, 4, 0.8, 0.2, 0.9, 1.0);
        let purple_before = straight(&purple, 0, 0);
        let settings = CameraRawSettings {
            optics: CameraRawOpticsSettings {
                purple_amount: 100.0,
                purple_hue_low: 250.0,
                purple_hue_high: 320.0,
                ..CameraRawOpticsSettings::default()
            },
            ..CameraRawSettings::default()
        };
        let defringed = straight(&grade(&settings, &purple), 0, 0);
        assert!(
            chroma(defringed) < chroma(purple_before),
            "purple defringe lowers chroma: {purple_before:?} → {defringed:?}"
        );

        let optics = CameraRawOpticsSettings {
            remove_chromatic_aberration: true,
            ..CameraRawOpticsSettings::default()
        };
        assert!(optics.adjusts());

        let detail = CameraRawDetailSettings {
            sharpen_amount: 40.0,
            ..CameraRawDetailSettings::default()
        };
        let settings = CameraRawSettings {
            detail,
            ..CameraRawSettings::default()
        };
        let hidden = settings.applying(true, true, true, true, true, true, false, true, true, true);
        let edge_image = step();
        assert_eq!(grade(&hidden, &edge_image), edge_image);
    }

    /// `geometryWarpAndCalibrationPrimaries`. The correction math is
    /// `camera_raw_geometry_corners_follow_the_sliders`; this is the warp and the applied calibration.
    #[test]
    fn geometry_warp_and_calibration_primaries() {
        let grid = checker(12, 12);
        let settings = CameraRawSettings {
            geometry: CameraRawGeometrySettings {
                vertical: 40.0,
                ..CameraRawGeometrySettings::default()
            },
            ..CameraRawSettings::default()
        };
        assert_ne!(grade(&settings, &grid), grid);

        let red = solid(4, 4, 1.0, 0.0, 0.0, 1.0);
        let before = straight(&red, 0, 0);
        let settings = CameraRawSettings {
            calibration: CameraRawCalibrationSettings {
                red_hue: 80.0,
                ..CameraRawCalibrationSettings::default()
            },
            ..CameraRawSettings::default()
        };
        let after = straight(&grade(&settings, &red), 0, 0);
        assert_ne!(after, before, "calibration shifts a pure red: {before:?} → {after:?}");

        let hidden = settings.applying(true, true, true, true, true, true, true, true, true, false);
        assert_eq!(straight(&grade(&hidden, &red), 0, 0), before);
    }

    /// `guidedUprightFollowsADrawnLineAndLeavesAnUnguidedPicture`.
    #[test]
    fn guided_upright_follows_a_drawn_line_and_leaves_an_unguided_picture() {
        assert_eq!(
            CameraRawUprightMode::ALL,
            [CameraRawUprightMode::Off, CameraRawUprightMode::Guided]
        );
        let cool = solid(16, 16, 0.2, 0.45, 0.8, 1.0);
        let warm = solid(16, 16, 0.85, 0.25, 0.15, 1.0);
        let mut settings = CameraRawSettings {
            geometry: CameraRawGeometrySettings {
                upright: CameraRawUprightMode::Guided,
                ..CameraRawGeometrySettings::default()
            },
            ..CameraRawSettings::default()
        };
        assert_eq!(grade(&settings, &cool), cool);
        assert_eq!(grade(&settings, &warm), warm);

        settings.geometry.guides = vec![CameraRawGeometryGuide::new(0.1, 0.15, 0.9, 0.8)];
        let cool_guided = grade(&settings, &cool);
        let warm_guided = grade(&settings, &warm);
        assert_ne!(cool_guided, cool);
        assert_ne!(warm_guided, warm);
        assert_ne!(cool_guided, warm_guided);
    }
}
