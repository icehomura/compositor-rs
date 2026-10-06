//! The editor's image-operation vocabulary: the Filter menu's kinds and their settings.
//!
//! Ported from `Document/Filters.swift` (`FilterKind`, `BackgroundQuality`, `FilterSettings`,
//! `FilterJob`) and `Document/Gradient.swift` (`GradientStyle`, `GradientShape`,
//! `GradientSettings`), plus `Document/Dither.swift`'s `DitherStyle`, `DitherPixelShape`,
//! `DitherColors` and `DitherSettings`. The pixel work itself lives in `compositor-pixels`;
//! this module is data only, so the manifest, the session and the panels can name it without
//! pulling in a rasterizer.
//!
//! Camera Raw's settings are the one exception: `CameraRawSettings`/`CameraRawClipping` are
//! declared by `compositor_pixels::camera_raw` (core cannot depend on pixels), so
//! `FilterSettings` carries no camera-raw field. The session holds that state and hands it to the
//! rasterizer alongside the job; `FilterJob::camera_raw_clipping` keeps the clipping view's raw
//! value (`CameraRawClipping::raw_value()`, 0 for none) so a preview still describes itself.

use serde::{Deserialize, Serialize};

use crate::geom::{AffineTransform, CGFloat, Point, Rect};
use crate::selection::SelectionClip;
use crate::layer_adjustment::{
    AdjustmentColor, BlackWhiteSettings, ColorBalanceSettings, CurvesSettings, ExposureSettings,
    GradientMapSettings, GrainSettings,
};
use crate::buffer::Rgba8Image;

/// Filters from the Filter menu. Each runs on the active image layer, inside the selection if
/// there is one, with a live preview and one undo step on OK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterKind {
    #[serde(rename = "Gaussian Blur")]
    GaussianBlur,
    #[serde(rename = "Motion Blur")]
    MotionBlur,
    #[serde(rename = "Add Noise")]
    AddNoise,
    #[serde(rename = "Vignette")]
    Vignette,
    #[serde(rename = "Bloom / Glow")]
    BloomGlow,
    #[serde(rename = "Dither")]
    Dither,
    #[serde(rename = "Tonal Contrast")]
    TonalContrast,
    #[serde(rename = "Lens Correction")]
    LensCorrection,
    #[serde(rename = "Camera Raw Filter")]
    CameraRaw,
    #[serde(rename = "Remove Background")]
    RemoveBackground,
    #[serde(rename = "Content-Aware Fill")]
    ContentAwareFill,
    #[serde(rename = "Curves")]
    Curves,
    #[serde(rename = "Exposure")]
    Exposure,
    #[serde(rename = "Gradient Map")]
    GradientMap,
    #[serde(rename = "Grain")]
    Grain,
    #[serde(rename = "Black & White")]
    BlackWhite,
    #[serde(rename = "Color Balance")]
    ColorBalance,
}

impl FilterKind {
    /// `CaseIterable` order, which is also the menu's.
    pub const ALL: [FilterKind; 17] = [
        FilterKind::GaussianBlur,
        FilterKind::MotionBlur,
        FilterKind::AddNoise,
        FilterKind::Vignette,
        FilterKind::BloomGlow,
        FilterKind::Dither,
        FilterKind::TonalContrast,
        FilterKind::LensCorrection,
        FilterKind::CameraRaw,
        FilterKind::RemoveBackground,
        FilterKind::ContentAwareFill,
        FilterKind::Curves,
        FilterKind::Exposure,
        FilterKind::GradientMap,
        FilterKind::Grain,
        FilterKind::BlackWhite,
        FilterKind::ColorBalance,
    ];

    /// The string the Swift case's raw value holds (menu labels and undo names).
    pub fn raw_value(self) -> &'static str {
        match self {
            FilterKind::GaussianBlur => "Gaussian Blur",
            FilterKind::MotionBlur => "Motion Blur",
            FilterKind::AddNoise => "Add Noise",
            FilterKind::Vignette => "Vignette",
            FilterKind::BloomGlow => "Bloom / Glow",
            FilterKind::Dither => "Dither",
            FilterKind::TonalContrast => "Tonal Contrast",
            FilterKind::LensCorrection => "Lens Correction",
            FilterKind::CameraRaw => "Camera Raw Filter",
            FilterKind::RemoveBackground => "Remove Background",
            FilterKind::ContentAwareFill => "Content-Aware Fill",
            FilterKind::Curves => "Curves",
            FilterKind::Exposure => "Exposure",
            FilterKind::GradientMap => "Gradient Map",
            FilterKind::Grain => "Grain",
            FilterKind::BlackWhite => "Black & White",
            FilterKind::ColorBalance => "Color Balance",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.raw_value() == value)
    }

    pub fn is_automatic(self) -> bool {
        self == FilterKind::ContentAwareFill || self == FilterKind::RemoveBackground
    }

    /// Color adjustments: in the Image menu (and editable as adjustment layers), not under Filter.
    pub fn is_image_adjustment(self) -> bool {
        matches!(
            self,
            FilterKind::Curves
                | FilterKind::Exposure
                | FilterKind::GradientMap
                | FilterKind::Grain
                | FilterKind::BlackWhite
                | FilterKind::ColorBalance
        )
    }
}

/// Remove Background's two ways of working: Apple's own subject mask on its own, or that mask
/// refined against the layer's detail, which recovers hair and fur but takes longer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackgroundQuality {
    #[default]
    #[serde(rename = "Basic")]
    Basic,
    #[serde(rename = "Advanced")]
    Advanced,
}

impl BackgroundQuality {
    pub const ALL: [BackgroundQuality; 2] = [BackgroundQuality::Basic, BackgroundQuality::Advanced];

    pub fn raw_value(self) -> &'static str {
        match self {
            BackgroundQuality::Basic => "Basic",
            BackgroundQuality::Advanced => "Advanced",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|quality| quality.raw_value() == value)
    }
}

/// `ImageAdjustmentPixels.clamp`: a finite value inside `range`, otherwise the fallback.
pub(crate) fn finite_clamp(value: f64, range: (f64, f64), fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(range.0, range.1)
    } else {
        fallback
    }
}

/// Filter › Dither's looks, grouped as the panel's menu lists them. The order matches
/// `DitherPixels.h`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DitherStyle {
    #[default]
    #[serde(rename = "Atkinson (Classic Mac)")]
    Atkinson,
    #[serde(rename = "Floyd–Steinberg")]
    FloydSteinberg,
    #[serde(rename = "Bayer 2 × 2")]
    Bayer2,
    #[serde(rename = "Bayer 4 × 4")]
    Bayer4,
    #[serde(rename = "Bayer 8 × 8")]
    Bayer8,
    #[serde(rename = "Halftone Dots")]
    Dots,
    #[serde(rename = "Halftone Lines")]
    Lines,
    #[serde(rename = "Halftone Diamonds")]
    Diamonds,
    #[serde(rename = "Mac Patterns")]
    Patterns,
    #[serde(rename = "ASCII")]
    Ascii,
    #[serde(rename = "Scanlines (CRT)")]
    Scanlines,
}

impl DitherStyle {
    /// `CaseIterable` order.
    pub const ALL: [DitherStyle; 11] = [
        DitherStyle::Atkinson,
        DitherStyle::FloydSteinberg,
        DitherStyle::Bayer2,
        DitherStyle::Bayer4,
        DitherStyle::Bayer8,
        DitherStyle::Dots,
        DitherStyle::Lines,
        DitherStyle::Diamonds,
        DitherStyle::Patterns,
        DitherStyle::Ascii,
        DitherStyle::Scanlines,
    ];

    /// The panel's groups, in menu order (`Dither.swift`'s `groups`): diffusion, ordered, halftones,
    /// then the marks. Between groups the Style menu draws a line.
    pub const GROUPS: [&[DitherStyle]; 4] = [
        &[DitherStyle::Atkinson, DitherStyle::FloydSteinberg],
        &[DitherStyle::Bayer2, DitherStyle::Bayer4, DitherStyle::Bayer8],
        &[DitherStyle::Dots, DitherStyle::Lines, DitherStyle::Diamonds],
        &[DitherStyle::Patterns, DitherStyle::Ascii, DitherStyle::Scanlines],
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            DitherStyle::Atkinson => "Atkinson (Classic Mac)",
            DitherStyle::FloydSteinberg => "Floyd–Steinberg",
            DitherStyle::Bayer2 => "Bayer 2 × 2",
            DitherStyle::Bayer4 => "Bayer 4 × 4",
            DitherStyle::Bayer8 => "Bayer 8 × 8",
            DitherStyle::Dots => "Halftone Dots",
            DitherStyle::Lines => "Halftone Lines",
            DitherStyle::Diamonds => "Halftone Diamonds",
            DitherStyle::Patterns => "Mac Patterns",
            DitherStyle::Ascii => "ASCII",
            DitherStyle::Scanlines => "Scanlines (CRT)",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.raw_value() == value)
    }

    /// `DitherPixels.h`'s style code: the case's index.
    pub fn code(self) -> i32 {
        Self::ALL
            .into_iter()
            .position(|style| style == self)
            .unwrap_or(0) as i32
    }

    /// Error diffusion: each pixel's rounding error is passed to its neighbors.
    pub fn diffuses(self) -> bool {
        matches!(self, DitherStyle::Atkinson | DitherStyle::FloydSteinberg)
    }

    /// Diffusion and ordered styles quantize to a number of tones; the rest draw marks in two.
    pub fn has_tones(self) -> bool {
        matches!(
            self,
            DitherStyle::Atkinson
                | DitherStyle::FloydSteinberg
                | DitherStyle::Bayer2
                | DitherStyle::Bayer4
                | DitherStyle::Bayer8
        )
    }

    pub fn is_halftone(self) -> bool {
        matches!(self, DitherStyle::Dots | DitherStyle::Lines | DitherStyle::Diamonds)
    }

    /// Halftone shapes, patterns and characters mark one tone on the other, so which one is the
    /// mark matters.
    pub fn draws_marks(self) -> bool {
        !self.has_tones() && self != DitherStyle::Scanlines
    }

    /// ASCII's characters and a CRT's lines are drawn at full resolution, not in chunky pixels.
    pub fn uses_pixel_size(self) -> bool {
        self != DitherStyle::Ascii && self != DitherStyle::Scanlines
    }
}

/// How a chunky pixel is drawn: a solid square, or a round dot with the dark color showing
/// around it, like the lit pixels of a dot-matrix or LED screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DitherPixelShape {
    #[default]
    #[serde(rename = "Square")]
    Square,
    #[serde(rename = "Dot")]
    Dot,
}

impl DitherPixelShape {
    pub const ALL: [DitherPixelShape; 2] = [DitherPixelShape::Square, DitherPixelShape::Dot];

    pub fn raw_value(self) -> &'static str {
        match self {
            DitherPixelShape::Square => "Square",
            DitherPixelShape::Dot => "Dot",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.raw_value() == value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DitherColors {
    #[default]
    #[serde(rename = "Black & White")]
    BlackWhite,
    #[serde(rename = "Two Colors")]
    TwoColors,
    #[serde(rename = "Original")]
    Original,
}

impl DitherColors {
    pub const ALL: [DitherColors; 3] = [
        DitherColors::BlackWhite,
        DitherColors::TwoColors,
        DitherColors::Original,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            DitherColors::BlackWhite => "Black & White",
            DitherColors::TwoColors => "Two Colors",
            DitherColors::Original => "Original",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|colors| colors.raw_value() == value)
    }
}

/// Filter › Dither's settings, down to the ASCII character set's layout rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DitherSettings {
    pub style: DitherStyle,
    /// Each dithered pixel covers this many layer pixels on a side, for chunky old-screen pixels.
    pub pixel_size: f64,
    pub pixel_shape: DitherPixelShape,
    /// Halftone screen and character cells, in dithered pixels.
    pub cell_size: f64,
    /// ASCII's line height in layer pixels; the characters are about six tenths as wide.
    pub text_size: f64,
    /// Scanlines: how far apart the lines are, in layer pixels.
    pub line_spacing: f64,
    /// Scanlines, 0–100%: light blooming around the lines, how far the lines break into round
    /// dots, and (in pixels) how far they waver sideways.
    pub glow: f64,
    pub dots: f64,
    pub wobble: f64,
    /// Halftone screen angle in degrees.
    pub angle: f64,
    /// Tones per channel for diffusion and ordered styles; 2 is 1-bit.
    pub levels: f64,
    /// How much of the error diffusion passes on, 0–100%. Less gives flatter, posterized areas.
    pub diffusion: f64,
    /// −100…100: more ink (darker) or less, and flatter or punchier, before dithering.
    pub density: f64,
    pub contrast: f64,
    pub colors: DitherColors,
    pub dark: AdjustmentColor,
    pub light: AdjustmentColor,
    /// Marks stand for the light tones, drawn in the light color on the dark: glowing dots on a
    /// black screen. On by default; it only affects halftone, patterns and ASCII.
    pub light_on_dark: bool,
    /// ASCII's characters, any order: they're sorted by how much ink each one has.
    pub characters: String,
}

impl Default for DitherSettings {
    fn default() -> Self {
        DitherSettings {
            style: DitherStyle::Atkinson,
            pixel_size: 2.0,
            pixel_shape: DitherPixelShape::Square,
            cell_size: 8.0,
            text_size: 14.0,
            line_spacing: 4.0,
            glow: 35.0,
            dots: 0.0,
            wobble: 0.0,
            angle: 45.0,
            levels: 2.0,
            diffusion: 100.0,
            density: 0.0,
            contrast: 0.0,
            colors: DitherColors::BlackWhite,
            dark: AdjustmentColor { red: 0.0, green: 0.0, blue: 0.0 },
            light: AdjustmentColor { red: 1.0, green: 1.0, blue: 1.0 },
            light_on_dark: true,
            characters: DitherSettings::DEFAULT_CHARACTERS.to_string(),
        }
    }
}

impl DitherSettings {
    pub const PIXEL_SIZE_RANGE: (f64, f64) = (1.0, 32.0);
    pub const CELL_SIZE_RANGE: (f64, f64) = (4.0, 64.0);
    pub const TEXT_SIZE_RANGE: (f64, f64) = (6.0, 64.0);
    pub const LEVELS_RANGE: (f64, f64) = (2.0, 8.0);
    pub const LINE_SPACING_RANGE: (f64, f64) = (2.0, 32.0);
    pub const WOBBLE_RANGE: (f64, f64) = (0.0, 64.0);
    pub const DEFAULT_CHARACTERS: &'static str = " .:-=+*#%@";

    /// The sliders' values as the rasterizer uses them: finite, in range, whole numbers rounded.
    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.pixel_size = finite_clamp(self.pixel_size, Self::PIXEL_SIZE_RANGE, 2.0).round();
        result.cell_size = finite_clamp(self.cell_size, Self::CELL_SIZE_RANGE, 8.0).round();
        result.text_size = finite_clamp(self.text_size, Self::TEXT_SIZE_RANGE, 14.0).round();
        result.line_spacing =
            finite_clamp(self.line_spacing, Self::LINE_SPACING_RANGE, 4.0).round();
        result.glow = finite_clamp(self.glow, (0.0, 100.0), 35.0);
        result.dots = finite_clamp(self.dots, (0.0, 100.0), 0.0);
        result.wobble = finite_clamp(self.wobble, Self::WOBBLE_RANGE, 0.0);
        result.angle = finite_clamp(self.angle, (-90.0, 90.0), 45.0);
        result.levels = finite_clamp(self.levels, Self::LEVELS_RANGE, 2.0).round();
        result.diffusion = finite_clamp(self.diffusion, (0.0, 100.0), 100.0);
        result.density = finite_clamp(self.density, (-100.0, 100.0), 0.0);
        result.contrast = finite_clamp(self.contrast, (-100.0, 100.0), 0.0);
        result.dark = self.dark.clamped();
        result.light = self.light.clamped();
        result.characters = self
            .characters
            .chars()
            .filter(|character| !is_newline(*character))
            .take(64)
            .collect();
        result
    }
}

/// `Character.isNewline`, which the dither panel uses to drop line breaks from the character set.
fn is_newline(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// The gradient tool's colors: foreground-to-background, or foreground to transparent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GradientStyle {
    #[default]
    #[serde(rename = "Foreground to Background")]
    ForegroundToBackground,
    #[serde(rename = "Foreground to Transparent")]
    ForegroundToTransparent,
}

impl GradientStyle {
    pub const ALL: [GradientStyle; 2] = [
        GradientStyle::ForegroundToBackground,
        GradientStyle::ForegroundToTransparent,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            GradientStyle::ForegroundToBackground => "Foreground to Background",
            GradientStyle::ForegroundToTransparent => "Foreground to Transparent",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.raw_value() == value)
    }
}

/// Linear runs from start to end; radial is centered on the start with the end on its rim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GradientShape {
    #[default]
    #[serde(rename = "Linear")]
    Linear,
    #[serde(rename = "Radial")]
    Radial,
}

impl GradientShape {
    pub const ALL: [GradientShape; 2] = [GradientShape::Linear, GradientShape::Radial];

    pub fn raw_value(self) -> &'static str {
        match self {
            GradientShape::Linear => "Linear",
            GradientShape::Radial => "Radial",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.raw_value() == value)
    }
}

/// The gradient tool's options bar: shape, colors, direction and opacity.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GradientSettings {
    pub shape: GradientShape,
    pub style: GradientStyle,
    pub reversed: bool,
    pub opacity: CGFloat,
}

impl Default for GradientSettings {
    fn default() -> Self {
        GradientSettings {
            shape: GradientShape::Linear,
            style: GradientStyle::ForegroundToTransparent,
            reversed: false,
            opacity: 1.0,
        }
    }
}

/// Every filter's settings; each filter reads only its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterSettings {
    /// Gaussian Blur radius in layer pixels (the blur's standard deviation), 0.1–250.
    pub radius: f64,
    /// Motion Blur direction in degrees, counterclockwise from horizontal as in Photoshop, −90–90.
    pub angle: f64,
    /// Motion Blur streak length in layer pixels, 1–2000.
    pub distance: f64,
    /// Add Noise strength as Photoshop's percentage, 0.1–400.
    pub amount: f64,
    /// Add Noise distribution: Gaussian (more speckled) instead of Uniform.
    pub gaussian: bool,
    /// Add Noise changes brightness only, the same amount on every channel.
    pub monochromatic: bool,
    /// Standalone vignette: edge color, strength, and shape of its falloff.
    pub vignette_amount: f64,
    pub vignette_color: AdjustmentColor,
    pub vignette_midpoint: f64,
    pub vignette_roundness: f64,
    pub vignette_feather: f64,
    pub vignette_highlights: f64,
    /// Bloom / Glow: strength and blur radius in layer pixels.
    pub bloom_amount: f64,
    pub bloom_radius: f64,
    /// Tonal Contrast: one local-detail radius and separate tonal strengths.
    pub tonal_amount: f64,
    pub tonal_radius: f64,
    pub tonal_shadows: f64,
    pub tonal_midtones: f64,
    pub tonal_highlights: f64,
    /// Lens Correction's Remove Distortion, −100–100: positive straightens barrel distortion
    /// (lines bowing outward), negative straightens pincushion (lines bowing inward).
    pub distortion: f64,
    pub curves: CurvesSettings,
    pub exposure: ExposureSettings,
    pub gradient_map: GradientMapSettings,
    pub grain: GrainSettings,
    pub black_white: BlackWhiteSettings,
    pub color_balance: ColorBalanceSettings,
    pub dither: DitherSettings,
    /// Remove Background: Basic is the quick subject mask; Advanced refines it (see the three
    /// settings below).
    pub background_quality: BackgroundQuality,
    /// Remove Background: how far the mask is pulled onto the image's own edges (0 off, in layer
    /// pixels).
    pub refine_edges: f64,
    /// Remove Background: pushes the mask's grays toward black and white, 0–100, clearing haze in
    /// thin areas.
    pub matte_contrast: f64,
    /// Remove Background: contracts (negative) or expands (positive) the mask edge, in layer
    /// pixels.
    pub shift_edge: f64,
}

impl Default for FilterSettings {
    fn default() -> Self {
        FilterSettings {
            radius: 1.0,
            angle: 0.0,
            distance: 10.0,
            amount: 10.0,
            gaussian: false,
            monochromatic: false,
            vignette_amount: 35.0,
            vignette_color: AdjustmentColor { red: 0.0, green: 0.0, blue: 0.0 },
            vignette_midpoint: 50.0,
            vignette_roundness: 100.0,
            vignette_feather: 60.0,
            vignette_highlights: 25.0,
            bloom_amount: 40.0,
            bloom_radius: 24.0,
            tonal_amount: 50.0,
            tonal_radius: 16.0,
            tonal_shadows: 40.0,
            tonal_midtones: 60.0,
            tonal_highlights: 30.0,
            distortion: 0.0,
            curves: CurvesSettings::default(),
            exposure: ExposureSettings::default(),
            gradient_map: GradientMapSettings::default(),
            grain: GrainSettings::default(),
            black_white: BlackWhiteSettings::default(),
            color_balance: ColorBalanceSettings::default(),
            dither: DitherSettings::default(),
            background_quality: BackgroundQuality::Basic,
            refine_edges: 12.0,
            matte_contrast: 25.0,
            shift_edge: 0.0,
        }
    }
}

impl FilterSettings {
    /// Every value inside its slider's range, with the Swift's fallbacks for non-finite input.
    pub fn normalized(&self) -> Self {
        let mut result = self.clone();
        result.radius = finite_clamp(self.radius, (0.1, 250.0), 1.0);
        result.angle = finite_clamp(self.angle, (-90.0, 90.0), 0.0);
        result.distance = finite_clamp(self.distance, (1.0, 2000.0), 10.0);
        result.amount = finite_clamp(self.amount, (0.1, 400.0), 10.0);
        result.vignette_amount = finite_clamp(self.vignette_amount, (0.0, 100.0), 35.0);
        result.vignette_color = self.vignette_color.clamped();
        result.vignette_midpoint = finite_clamp(self.vignette_midpoint, (0.0, 100.0), 50.0);
        result.vignette_roundness = finite_clamp(self.vignette_roundness, (-100.0, 100.0), 100.0);
        result.vignette_feather = finite_clamp(self.vignette_feather, (0.0, 100.0), 60.0);
        result.vignette_highlights = finite_clamp(self.vignette_highlights, (0.0, 100.0), 25.0);
        result.bloom_amount = finite_clamp(self.bloom_amount, (0.0, 100.0), 40.0);
        result.bloom_radius = finite_clamp(self.bloom_radius, (1.0, 150.0), 24.0);
        result.tonal_amount = finite_clamp(self.tonal_amount, (0.0, 100.0), 50.0);
        result.tonal_radius = finite_clamp(self.tonal_radius, (1.0, 100.0), 16.0);
        result.tonal_shadows = finite_clamp(self.tonal_shadows, (-100.0, 100.0), 40.0);
        result.tonal_midtones = finite_clamp(self.tonal_midtones, (-100.0, 100.0), 60.0);
        result.tonal_highlights = finite_clamp(self.tonal_highlights, (-100.0, 100.0), 30.0);
        result.distortion = finite_clamp(self.distortion, (-100.0, 100.0), 0.0);
        result.refine_edges = finite_clamp(self.refine_edges, (0.0, 40.0), 12.0);
        result.matte_contrast = finite_clamp(self.matte_contrast, (0.0, 100.0), 25.0);
        result.shift_edge = finite_clamp(self.shift_edge, (-10.0, 10.0), 0.0);
        result.exposure = self.exposure.normalized();
        result.gradient_map = self.gradient_map.normalized();
        result.grain = self.grain.normalized();
        result.dither = self.dither.normalized();
        result
    }
}

/// One filter run. The pixels and the settings it works from, the selection it is confined to and
/// the transform that puts its pixels in the document.
#[derive(Clone, Debug)]
pub struct FilterJob {
    pub kind: FilterKind,
    pub image: Rgba8Image,
    pub settings: FilterSettings,
    /// Pixels in `image` per original layer pixel, so a downscaled preview blurs proportionally
    /// less.
    pub scale: CGFloat,
    pub selection: Option<SelectionClip>,
    pub mapping: AffineTransform,
    /// Add Noise's random pattern: the same seed gives the same grain.
    pub seed: u32,
    /// Vignette on an empty layer: the canvas, in the document, which it frames and fills.
    /// Otherwise the vignette frames the layer's own pixels and recolors only those.
    pub canvas: Option<Rect>,
    /// Canvas-space origin used by live adjustment layers so partial redraws keep one noise field.
    pub noise_origin: Point,
    /// Camera Raw's Option-drag clipping view. Preview only; committing leaves this nil (0).
    /// The value is `CameraRawClipping`'s raw value: 1 highlights, 2 shadows.
    pub camera_raw_clipping: i32,
    /// Persistent histogram clipping indicators. Preview only; committing leaves these off.
    pub shows_shadow_clipping: bool,
    pub shows_highlight_clipping: bool,
    /// Point-color range preview. −1 leaves the grade alone.
    pub visualizes_point_color: i32,
    /// Option-drag on Sharpening Masking. Preview only.
    pub shows_sharpen_mask: bool,
}

impl FilterJob {
    /// A job with the Swift's defaults for everything the caller doesn't pass.
    pub fn new(
        kind: FilterKind,
        image: Rgba8Image,
        settings: FilterSettings,
        scale: CGFloat,
        selection: Option<SelectionClip>,
        mapping: AffineTransform,
    ) -> Self {
        FilterJob {
            kind,
            image,
            settings,
            scale,
            selection,
            mapping,
            seed: 0,
            canvas: None,
            noise_origin: Point::ZERO,
            camera_raw_clipping: 0,
            shows_shadow_clipping: false,
            shows_highlight_clipping: false,
            visualizes_point_color: -1,
            shows_sharpen_mask: false,
        }
    }
}

/// `FilterEdit.blurMargin`: the room a blur needs around the layer — about three standard
/// deviations, or half a streak.
pub fn blur_margin(kind: FilterKind, settings: &FilterSettings) -> CGFloat {
    match kind {
        FilterKind::GaussianBlur => settings.radius * 3.0 + 2.0,
        FilterKind::MotionBlur => settings.distance / 2.0 + 2.0,
        FilterKind::BloomGlow => settings.bloom_radius * 3.0 + 2.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_kinds_keep_their_raw_values_and_order() {
        let values: Vec<&str> = FilterKind::ALL.iter().map(|kind| kind.raw_value()).collect();
        assert_eq!(
            values,
            vec![
                "Gaussian Blur",
                "Motion Blur",
                "Add Noise",
                "Vignette",
                "Bloom / Glow",
                "Dither",
                "Tonal Contrast",
                "Lens Correction",
                "Camera Raw Filter",
                "Remove Background",
                "Content-Aware Fill",
                "Curves",
                "Exposure",
                "Gradient Map",
                "Grain",
                "Black & White",
                "Color Balance",
            ]
        );
        for kind in FilterKind::ALL {
            assert_eq!(FilterKind::from_raw(kind.raw_value()), Some(kind));
        }
        assert_eq!(FilterKind::from_raw("Nope"), None);
        assert!(FilterKind::ContentAwareFill.is_automatic());
        assert!(FilterKind::RemoveBackground.is_automatic());
        assert!(!FilterKind::Vignette.is_automatic());
        assert!(FilterKind::GradientMap.is_image_adjustment());
        assert!(FilterKind::ColorBalance.is_image_adjustment());
        assert!(!FilterKind::GaussianBlur.is_image_adjustment());
        assert!(!FilterKind::Dither.is_image_adjustment());
    }

    #[test]
    fn filter_settings_normalize_clamps_and_falls_back() {
        let normalized = FilterSettings {
            radius: 0.0,
            angle: -400.0,
            distance: 0.5,
            amount: 1e9,
            vignette_roundness: -400.0,
            tonal_shadows: -400.0,
            shift_edge: -20.0,
            refine_edges: 400.0,
            distortion: 500.0,
            ..FilterSettings::default()
        }
        .normalized();
        assert_eq!(normalized.radius, 0.1);
        assert_eq!(normalized.angle, -90.0);
        assert_eq!(normalized.distance, 1.0);
        assert_eq!(normalized.amount, 400.0);
        assert_eq!(normalized.vignette_roundness, -100.0);
        assert_eq!(normalized.tonal_shadows, -100.0);
        assert_eq!(normalized.shift_edge, -10.0);
        assert_eq!(normalized.refine_edges, 40.0);
        assert_eq!(normalized.distortion, 100.0);

        let nan = FilterSettings {
            radius: f64::NAN,
            angle: f64::INFINITY,
            amount: f64::NEG_INFINITY,
            bloom_radius: f64::NAN,
            ..FilterSettings::default()
        }
        .normalized();
        assert_eq!(nan.radius, 1.0);
        assert_eq!(nan.angle, 0.0);
        assert_eq!(nan.amount, 10.0);
        assert_eq!(nan.bloom_radius, 24.0);

        let defaults = FilterSettings::default().normalized();
        assert_eq!(defaults.radius, 1.0);
        assert_eq!(defaults.angle, 0.0);
        assert_eq!(defaults.distance, 10.0);
        assert_eq!(defaults.vignette_color.red, 0.0);
        assert_eq!(defaults.background_quality, BackgroundQuality::Basic);
    }

    #[test]
    fn blur_margin_matches_the_swift_formulas() {
        let settings = FilterSettings {
            radius: 3.0,
            distance: 16.0,
            bloom_radius: 24.0,
            ..FilterSettings::default()
        };
        assert_eq!(blur_margin(FilterKind::GaussianBlur, &settings), 11.0);
        assert_eq!(blur_margin(FilterKind::MotionBlur, &settings), 10.0);
        assert_eq!(blur_margin(FilterKind::BloomGlow, &settings), 74.0);
        assert_eq!(blur_margin(FilterKind::Dither, &settings), 0.0);
    }

    #[test]
    fn dither_style_codes_and_groups_follow_the_c_header() {
        assert_eq!(DitherStyle::Atkinson.code(), 0);
        assert_eq!(DitherStyle::FloydSteinberg.code(), 1);
        assert_eq!(DitherStyle::Bayer2.code(), 2);
        assert_eq!(DitherStyle::Bayer8.code(), 4);
        assert_eq!(DitherStyle::Dots.code(), 5);
        assert_eq!(DitherStyle::Ascii.code(), 9);
        assert_eq!(DitherStyle::Scanlines.code(), 10);
        assert!(DitherStyle::Atkinson.diffuses() && DitherStyle::FloydSteinberg.diffuses());
        assert!(!DitherStyle::Bayer4.diffuses());
        assert!(DitherStyle::Bayer8.has_tones() && !DitherStyle::Dots.has_tones());
        assert!(DitherStyle::Diamonds.is_halftone() && !DitherStyle::Patterns.is_halftone());
        assert!(DitherStyle::Patterns.draws_marks() && !DitherStyle::Scanlines.draws_marks());
        assert!(!DitherStyle::Ascii.uses_pixel_size() && DitherStyle::Dots.uses_pixel_size());
        assert_eq!(
            DitherStyle::GROUPS,
            [
                &[DitherStyle::Atkinson, DitherStyle::FloydSteinberg][..],
                &[DitherStyle::Bayer2, DitherStyle::Bayer4, DitherStyle::Bayer8][..],
                &[DitherStyle::Dots, DitherStyle::Lines, DitherStyle::Diamonds][..],
                &[DitherStyle::Patterns, DitherStyle::Ascii, DitherStyle::Scanlines][..],
            ]
        );
        // The menu offers every style exactly once, in `CaseIterable` order.
        let listed: Vec<DitherStyle> =
            DitherStyle::GROUPS.iter().flat_map(|group| group.iter().copied()).collect();
        assert_eq!(listed, DitherStyle::ALL.to_vec());
    }

    #[test]
    fn dither_settings_normalize_rounds_sizes_and_truncates_characters() {
        let settings = DitherSettings {
            pixel_size: 2.4,
            cell_size: 8.5,
            text_size: 14.4,
            line_spacing: 4.6,
            wobble: 100.0,
            angle: 200.0,
            levels: 2.6,
            characters: "ab\ncd\ref\u{2028}gh".to_string(),
            ..DitherSettings::default()
        }
        .normalized();
        assert_eq!(settings.pixel_size, 2.0);
        assert_eq!(settings.cell_size, 9.0);
        assert_eq!(settings.text_size, 14.0);
        assert_eq!(settings.line_spacing, 5.0);
        assert_eq!(settings.wobble, 64.0);
        assert_eq!(settings.angle, 90.0);
        assert_eq!(settings.levels, 3.0);
        assert_eq!(settings.characters, "abcdefgh");

        let long = DitherSettings {
            characters: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-".to_string(),
            ..DitherSettings::default()
        }
        .normalized();
        assert_eq!(long.characters.chars().count(), 64);
        assert_eq!(long.characters.chars().next(), Some('a'));
        assert_eq!(long.characters.chars().last(), Some('-'));

        let base = long.characters.clone();
        let last_kept = base.chars().last();
        let truncated = DitherSettings {
            characters: format!("{base}z"),
            ..DitherSettings::default()
        }
        .normalized();
        assert_eq!(truncated.characters.chars().count(), 64);
        assert_eq!(truncated.characters.chars().last(), last_kept);
    }

    #[test]
    fn gradient_settings_defaults_match_the_swift() {
        let settings = GradientSettings::default();
        assert_eq!(settings.shape, GradientShape::Linear);
        assert_eq!(settings.style, GradientStyle::ForegroundToTransparent);
        assert!(!settings.reversed);
        assert_eq!(settings.opacity, 1.0);
        assert_eq!(GradientShape::Radial.raw_value(), "Radial");
        assert_eq!(GradientStyle::from_raw("Foreground to Background"), Some(GradientStyle::ForegroundToBackground));
        assert_eq!(GradientStyle::from_raw("Foreground to Transparent"), Some(GradientStyle::ForegroundToTransparent));
    }

    #[test]
    fn filter_job_defaults_leave_the_preview_only_options_off() {
        let job = FilterJob::new(
            FilterKind::Vignette,
            Rgba8Image::new(1, 1),
            FilterSettings::default(),
            1.0,
            None,
            AffineTransform::IDENTITY,
        );
        assert_eq!(job.seed, 0);
        assert_eq!(job.camera_raw_clipping, 0);
        assert_eq!(job.visualizes_point_color, -1);
        assert!(!job.shows_shadow_clipping && !job.shows_highlight_clipping && !job.shows_sharpen_mask);
        assert!(job.canvas.is_none());
    }

    #[test]
    fn filter_settings_round_trip_through_json_with_camel_case_keys() {
        let settings = FilterSettings {
            vignette_color: AdjustmentColor { red: 0.25, green: 0.5, blue: 0.75 },
            background_quality: BackgroundQuality::Advanced,
            ..FilterSettings::default()
        };
        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(json["vignetteColor"]["red"], 0.25);
        assert_eq!(json["backgroundQuality"], "Advanced");
        assert!(json["vignetteAmount"].is_number());
        assert!(json["blackWhite"].is_object());
        let back: FilterSettings = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, settings);
    }
}
