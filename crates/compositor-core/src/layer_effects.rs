//! The styles a layer draws around itself: stroke, drop shadow, color overlay, inner shadow, outer
//! glow and inner glow. Ported from `Document/LayerEffects.swift`; the rasterization lives in
//! `compositor-pixels` and the session commands in `compositor-session`.

use serde::{Deserialize, Serialize};

use crate::color::PaletteColor;
use crate::geom::{CGFloat, Size};
use crate::Id;

/// A component inside `0…1` that is finite, the way every effect validates its color and opacity.
fn unit_scalar(value: CGFloat) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

/// A line drawn around what the layer shows, outside its edge or inside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrokeEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub size: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
    pub inside: bool,
}

impl StrokeEffect {
    /// Supported document-pixel width; preview work is bounded independently of this value.
    pub const MAX_SIZE: CGFloat = 500.0;
}

impl Default for StrokeEffect {
    fn default() -> Self {
        StrokeEffect {
            enabled: None,
            size: 4.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            opacity: 1.0,
            inside: false,
        }
    }
}

impl StrokeEffect {
    /// A missing `enabled` means visible.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    pub fn is_valid(&self) -> bool {
        self.size.is_finite()
            && (0.0..=StrokeEffect::MAX_SIZE).contains(&self.size)
            && unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// The layer's shape repeated behind it, offset and softened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Where the light comes from, in degrees counterclockwise from the right, as Photoshop's dial is:
    /// 90 is from straight above, which drops the shadow straight down.
    pub angle: CGFloat,
    pub distance: CGFloat,
    pub blur: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
}

impl Default for ShadowEffect {
    fn default() -> Self {
        ShadowEffect {
            enabled: None,
            angle: 90.0,
            distance: 20.0,
            blur: 20.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            opacity: 0.5,
        }
    }
}

impl ShadowEffect {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    /// Where the shadow sits, in layer pixels (y grows downward, as the layer's own pixels do).
    pub fn offset(&self) -> Size {
        let radians = self.angle * std::f64::consts::PI / 180.0;
        // The shadow falls away from the light, and a layer's pixels count y downward.
        Size::new(-radians.cos() * self.distance, radians.sin() * self.distance)
    }

    pub fn is_valid(&self) -> bool {
        self.angle.is_finite()
            && self.distance.is_finite()
            && self.blur.is_finite()
            && (-360.0..=360.0).contains(&self.angle)
            && (0.0..=5000.0).contains(&self.distance)
            && (0.0..=500.0).contains(&self.blur)
            && unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// A flat color over everything the layer shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorOverlayEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
}

impl Default for ColorOverlayEffect {
    fn default() -> Self {
        ColorOverlayEffect {
            enabled: None,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            opacity: 1.0,
        }
    }
}

impl ColorOverlayEffect {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    pub fn is_valid(&self) -> bool {
        unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// A shadow cast inside the layer's own edges, as though it were cut out of what is behind it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InnerShadowEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub angle: CGFloat,
    pub distance: CGFloat,
    pub blur: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
}

impl Default for InnerShadowEffect {
    fn default() -> Self {
        InnerShadowEffect {
            enabled: None,
            angle: 90.0,
            distance: 10.0,
            blur: 10.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            opacity: 0.5,
        }
    }
}

impl InnerShadowEffect {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    /// Where the shadow falls, in layer pixels (y grows downward).
    pub fn offset(&self) -> Size {
        let radians = self.angle * std::f64::consts::PI / 180.0;
        Size::new(-radians.cos() * self.distance, radians.sin() * self.distance)
    }

    pub fn is_valid(&self) -> bool {
        self.angle.is_finite()
            && self.distance.is_finite()
            && self.blur.is_finite()
            && (-360.0..=360.0).contains(&self.angle)
            && (0.0..=5000.0).contains(&self.distance)
            && (0.0..=500.0).contains(&self.blur)
            && unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// A soft glow drawn omnidirectionally around the outside of what the layer shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OuterGlowEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub size: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
}

impl Default for OuterGlowEffect {
    fn default() -> Self {
        OuterGlowEffect {
            enabled: None,
            size: 20.0,
            red: 1.0,
            green: 1.0,
            blue: 1.0,
            opacity: 0.75,
        }
    }
}

impl OuterGlowEffect {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    pub fn is_valid(&self) -> bool {
        self.size.is_finite()
            && (0.0..=500.0).contains(&self.size)
            && unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// A glow cast inside the layer's own edges, emanating inward from its boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InnerGlowEffect {
    /// Missing in older projects means visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub size: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub opacity: f64,
}

impl Default for InnerGlowEffect {
    fn default() -> Self {
        InnerGlowEffect {
            enabled: None,
            size: 10.0,
            red: 1.0,
            green: 1.0,
            blue: 1.0,
            opacity: 0.75,
        }
    }
}

impl InnerGlowEffect {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    pub fn is_valid(&self) -> bool {
        self.size.is_finite()
            && (0.0..=500.0).contains(&self.size)
            && unit_scalar(self.opacity)
            && unit_scalar(self.red)
            && unit_scalar(self.green)
            && unit_scalar(self.blue)
    }
}

/// What a layer draws around itself. Kept with the layer, so it follows every edit and can be changed
/// or removed at any time; the pixels themselves are never touched.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerEffects {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<StrokeEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_overlay: Option<ColorOverlayEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_shadow: Option<InnerShadowEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outer_glow: Option<OuterGlowEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_glow: Option<InnerGlowEffect>,
}

impl LayerEffects {
    pub fn is_empty(&self) -> bool {
        self.stroke.is_none()
            && self.shadow.is_none()
            && self.color_overlay.is_none()
            && self.inner_shadow.is_none()
            && self.outer_glow.is_none()
            && self.inner_glow.is_none()
    }

    pub fn is_valid(&self) -> bool {
        self.stroke.as_ref().map_or(true, StrokeEffect::is_valid)
            && self.shadow.as_ref().map_or(true, ShadowEffect::is_valid)
            && self
                .color_overlay
                .as_ref()
                .map_or(true, ColorOverlayEffect::is_valid)
            && self
                .inner_shadow
                .as_ref()
                .map_or(true, InnerShadowEffect::is_valid)
            && self
                .outer_glow
                .as_ref()
                .map_or(true, OuterGlowEffect::is_valid)
            && self
                .inner_glow
                .as_ref()
                .map_or(true, InnerGlowEffect::is_valid)
    }

    /// The effects this record carries, in `LayerEffectKind::ALL` order.
    pub fn kinds(&self) -> Vec<LayerEffectKind> {
        LayerEffectKind::ALL
            .into_iter()
            .filter(|kind| self.contains(*kind))
            .collect()
    }

    pub fn contains(&self, kind: LayerEffectKind) -> bool {
        match kind {
            LayerEffectKind::Stroke => self.stroke.is_some(),
            LayerEffectKind::Shadow => self.shadow.is_some(),
            LayerEffectKind::ColorOverlay => self.color_overlay.is_some(),
            LayerEffectKind::InnerShadow => self.inner_shadow.is_some(),
            LayerEffectKind::OuterGlow => self.outer_glow.is_some(),
            LayerEffectKind::InnerGlow => self.inner_glow.is_some(),
        }
    }

    pub fn is_enabled(&self, kind: LayerEffectKind) -> bool {
        match kind {
            LayerEffectKind::Stroke => self.stroke.as_ref().is_some_and(StrokeEffect::is_enabled),
            LayerEffectKind::Shadow => self.shadow.as_ref().is_some_and(ShadowEffect::is_enabled),
            LayerEffectKind::ColorOverlay => self
                .color_overlay
                .as_ref()
                .is_some_and(ColorOverlayEffect::is_enabled),
            LayerEffectKind::InnerShadow => self
                .inner_shadow
                .as_ref()
                .is_some_and(InnerShadowEffect::is_enabled),
            LayerEffectKind::OuterGlow => self
                .outer_glow
                .as_ref()
                .is_some_and(OuterGlowEffect::is_enabled),
            LayerEffectKind::InnerGlow => self
                .inner_glow
                .as_ref()
                .is_some_and(InnerGlowEffect::is_enabled),
        }
    }

    /// The effect's own color, and a way to put a new one back.
    pub fn color(&self, kind: LayerEffectKind) -> Option<PaletteColor> {
        match kind {
            LayerEffectKind::Stroke => self.stroke.as_ref().map(StrokeEffect::color),
            LayerEffectKind::Shadow => self.shadow.as_ref().map(ShadowEffect::color),
            LayerEffectKind::ColorOverlay => {
                self.color_overlay.as_ref().map(ColorOverlayEffect::color)
            }
            LayerEffectKind::InnerShadow => {
                self.inner_shadow.as_ref().map(InnerShadowEffect::color)
            }
            LayerEffectKind::OuterGlow => self.outer_glow.as_ref().map(OuterGlowEffect::color),
            LayerEffectKind::InnerGlow => self.inner_glow.as_ref().map(InnerGlowEffect::color),
        }
    }

    pub fn set_color(&mut self, color: PaletteColor, kind: LayerEffectKind) {
        let target = match kind {
            LayerEffectKind::Stroke => self.stroke.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
            LayerEffectKind::Shadow => self.shadow.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
            LayerEffectKind::ColorOverlay => self.color_overlay.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
            LayerEffectKind::InnerShadow => self.inner_shadow.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
            LayerEffectKind::OuterGlow => self.outer_glow.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
            LayerEffectKind::InnerGlow => self.inner_glow.as_mut().map(|effect| {
                (&mut effect.red, &mut effect.green, &mut effect.blue)
            }),
        };
        if let Some((red, green, blue)) = target {
            *red = color.red;
            *green = color.green;
            *blue = color.blue;
        }
    }

    pub fn remove(&mut self, kind: LayerEffectKind) {
        match kind {
            LayerEffectKind::Stroke => self.stroke = None,
            LayerEffectKind::Shadow => self.shadow = None,
            LayerEffectKind::ColorOverlay => self.color_overlay = None,
            LayerEffectKind::InnerShadow => self.inner_shadow = None,
            LayerEffectKind::OuterGlow => self.outer_glow = None,
            LayerEffectKind::InnerGlow => self.inner_glow = None,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool, kind: LayerEffectKind) {
        let target = match kind {
            LayerEffectKind::Stroke => self.stroke.as_mut().map(|effect| &mut effect.enabled),
            LayerEffectKind::Shadow => self.shadow.as_mut().map(|effect| &mut effect.enabled),
            LayerEffectKind::ColorOverlay => {
                self.color_overlay.as_mut().map(|effect| &mut effect.enabled)
            }
            LayerEffectKind::InnerShadow => {
                self.inner_shadow.as_mut().map(|effect| &mut effect.enabled)
            }
            LayerEffectKind::OuterGlow => self.outer_glow.as_mut().map(|effect| &mut effect.enabled),
            LayerEffectKind::InnerGlow => self.inner_glow.as_mut().map(|effect| &mut effect.enabled),
        };
        if let Some(target) = target {
            *target = Some(enabled);
        }
    }

    /// The effects that are actually shown: hidden ones keep all their parameters but drop out here.
    pub fn visible(&self) -> LayerEffects {
        LayerEffects {
            stroke: self.stroke.clone().filter(StrokeEffect::is_enabled),
            shadow: self.shadow.clone().filter(ShadowEffect::is_enabled),
            color_overlay: self.color_overlay.clone().filter(ColorOverlayEffect::is_enabled),
            inner_shadow: self.inner_shadow.clone().filter(InnerShadowEffect::is_enabled),
            outer_glow: self.outer_glow.clone().filter(OuterGlowEffect::is_enabled),
            inner_glow: self.inner_glow.clone().filter(InnerGlowEffect::is_enabled),
        }
    }
}

/// The layer effects, with the display names Photoshop shows (and the ones the UI uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LayerEffectKind {
    Stroke,
    Shadow,
    ColorOverlay,
    InnerShadow,
    OuterGlow,
    InnerGlow,
}

impl LayerEffectKind {
    /// `CaseIterable.allCases`, in declaration order.
    pub const ALL: [LayerEffectKind; 6] = [
        LayerEffectKind::Stroke,
        LayerEffectKind::Shadow,
        LayerEffectKind::ColorOverlay,
        LayerEffectKind::InnerShadow,
        LayerEffectKind::OuterGlow,
        LayerEffectKind::InnerGlow,
    ];

    /// The raw value the Swift enum carries (`String` raw value).
    pub fn raw_value(self) -> &'static str {
        match self {
            LayerEffectKind::Stroke => "Stroke",
            LayerEffectKind::Shadow => "Drop Shadow",
            LayerEffectKind::ColorOverlay => "Color Overlay",
            LayerEffectKind::InnerShadow => "Inner Shadow",
            LayerEffectKind::OuterGlow => "Outer Glow",
            LayerEffectKind::InnerGlow => "Inner Glow",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.raw_value() == value)
    }
}

/// Which layer's effect the panel or the layer list is pointing at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerEffectSelection {
    pub layer_id: Id,
    pub kind: LayerEffectKind,
}

impl LayerEffectSelection {
    pub fn new(layer_id: Id, kind: LayerEffectKind) -> Self {
        Self { layer_id, kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// Angles in degrees summed with the counterclockwise convention.
    fn offset_of(angle: CGFloat, distance: CGFloat) -> Size {
        ShadowEffect {
            angle,
            distance,
            ..Default::default()
        }
        .offset()
    }

    #[test]
    fn defaults_match_the_swift_declarations() {
        let stroke = StrokeEffect::default();
        assert_eq!(stroke.size, 4.0);
        assert_eq!(stroke.opacity, 1.0);
        assert_eq!(stroke.color(), PaletteColor::BLACK);
        assert!(!stroke.inside);
        assert_eq!(stroke.enabled, None);
        assert!(stroke.is_enabled());
        assert!(stroke.is_valid());

        let shadow = ShadowEffect::default();
        assert_eq!(shadow.angle, 90.0);
        assert_eq!(shadow.distance, 20.0);
        assert_eq!(shadow.blur, 20.0);
        assert_eq!(shadow.opacity, 0.5);
        assert_eq!(shadow.enabled, None);
        assert!(shadow.is_enabled());
        assert!(shadow.is_valid());

        let overlay = ColorOverlayEffect::default();
        assert_eq!(overlay.opacity, 1.0);
        assert!(overlay.is_enabled());
        assert!(overlay.is_valid());

        let inner = InnerShadowEffect::default();
        assert_eq!(inner.angle, 90.0);
        assert_eq!(inner.distance, 10.0);
        assert_eq!(inner.blur, 10.0);
        assert_eq!(inner.opacity, 0.5);
        assert!(inner.is_valid());

        let outer_glow = OuterGlowEffect::default();
        assert_eq!(outer_glow.size, 20.0);
        assert_eq!(outer_glow.opacity, 0.75);
        assert_eq!(outer_glow.color(), PaletteColor::WHITE);
        assert!(outer_glow.is_valid());

        let inner_glow = InnerGlowEffect::default();
        assert_eq!(inner_glow.size, 10.0);
        assert_eq!(inner_glow.opacity, 0.75);
        assert_eq!(inner_glow.color(), PaletteColor::WHITE);
        assert!(inner_glow.is_valid());

        assert!(LayerEffects::default().is_empty());
        assert!(LayerEffects::default().is_valid());
    }

    #[test]
    fn validation_keeps_the_swift_limits() {
        // Stroke: 0…500 layer pixels.
        let mut stroke = StrokeEffect::default();
        stroke.size = 500.0;
        assert!(stroke.is_valid());
        stroke.size = 500.1;
        assert!(!stroke.is_valid());
        stroke.size = -1.0;
        assert!(!stroke.is_valid());

        // Glows: 0…500.
        let mut glow = OuterGlowEffect::default();
        glow.size = 500.0;
        assert!(glow.is_valid());
        glow.size = -1.0;
        assert!(!glow.is_valid());
        let mut inner_glow = InnerGlowEffect::default();
        inner_glow.size = -1.0;
        assert!(!inner_glow.is_valid());

        // Shadows: angle -360…360, distance 0…5000, blur 0…500.
        let mut shadow = ShadowEffect::default();
        shadow.angle = -360.0;
        assert!(shadow.is_valid());
        shadow.angle = 360.1;
        assert!(!shadow.is_valid());
        shadow.angle = 90.0;
        shadow.distance = 5000.0;
        shadow.blur = 500.0;
        assert!(shadow.is_valid());
        shadow.distance = 5000.1;
        assert!(!shadow.is_valid());
        shadow.distance = 20.0;
        shadow.blur = 500.1;
        assert!(!shadow.is_valid());

        // Opacity and color components stay in 0…1, and must be finite.
        let mut overlay = ColorOverlayEffect::default();
        overlay.opacity = 1.5;
        assert!(!overlay.is_valid());
        overlay.opacity = 1.0;
        overlay.red = 2.0;
        assert!(!overlay.is_valid());
        overlay.red = f64::NAN;
        assert!(!overlay.is_valid());
        overlay.red = 0.0;
        overlay.opacity = f64::INFINITY;
        assert!(!overlay.is_valid());

        // A LayerEffects record is valid when every effect it carries is.
        let mut effects = LayerEffects::default();
        effects.stroke = Some(StrokeEffect {
            size: 1.0,
            ..Default::default()
        });
        assert!(effects.is_valid());
        effects.inner_shadow = Some(InnerShadowEffect {
            angle: 400.0,
            ..Default::default()
        });
        assert!(!effects.is_valid());
    }

    #[test]
    fn shadow_offset_follows_the_photoshop_dial() {
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // From straight above: the shadow drops straight down.
        let down = offset_of(90.0, 20.0);
        assert!(near(down.width, 0.0) && near(down.height, 20.0));
        // From the right: the shadow falls to the left.
        let left = offset_of(0.0, 20.0);
        assert!(near(left.width, -20.0) && near(left.height, 0.0));
        // From the left: to the right.
        let right = offset_of(180.0, 20.0);
        assert!(near(right.width, 20.0) && near(right.height, 0.0));
        // From below: upward.
        let up = offset_of(270.0, 20.0);
        assert!(near(up.width, 0.0) && near(up.height, -20.0));
    }

    #[test]
    fn serde_keys_match_the_swift_codable_output() {
        let effects = LayerEffects {
            stroke: Some(StrokeEffect {
                enabled: Some(false),
                size: 8.0,
                red: 0.1,
                green: 0.8,
                blue: 0.2,
                opacity: 0.9,
                inside: false,
            }),
            color_overlay: Some(ColorOverlayEffect {
                red: 0.9,
                ..Default::default()
            }),
            inner_shadow: Some(InnerShadowEffect {
                angle: 135.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let value: Value = serde_json::to_value(&effects).unwrap();

        // Camel-case keys for the multi-word effects.
        assert!(value.get("colorOverlay").is_some());
        assert!(value.get("innerShadow").is_some());
        assert!(value.get("color_overlay").is_none());
        // Absent effects (and the implicit `nil` enabled) are omitted entirely.
        assert!(value.get("shadow").is_none());
        assert!(value.get("outerGlow").is_none());
        assert!(value.get("innerGlow").is_none());
        assert_eq!(value["stroke"]["enabled"], json!(false));
        assert_eq!(value["stroke"]["size"], json!(8.0));
        assert_eq!(value["stroke"]["inside"], json!(false));
        // Computed colors are not stored: the manifest keeps the flat components.
        assert!(value["stroke"].get("color").is_none());
        assert!(value["colorOverlay"].get("enabled").is_none());
        assert_eq!(value["colorOverlay"]["red"], json!(0.9));
        assert_eq!(value["innerShadow"]["angle"], json!(135.0));

        let decoded: LayerEffects = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, effects);
    }

    #[test]
    fn older_projects_without_enabled_or_newer_effects_still_decode() {
        let older = r#"{
            "stroke": { "size": 3, "red": 0, "green": 0, "blue": 0, "opacity": 1, "inside": false }
        }"#;
        let effects: LayerEffects = serde_json::from_str(older).unwrap();
        assert_eq!(effects.stroke.as_ref().unwrap().size, 3.0);
        assert!(effects.stroke.as_ref().unwrap().is_enabled());
        assert!(effects.outer_glow.is_none());
        assert!(effects.inner_glow.is_none());
        assert!(effects.is_valid());

        // An explicit `enabled` round-trips, including a hidden effect.
        let hidden: LayerEffects = serde_json::from_str(
            r#"{
                "shadow": {
                    "enabled": false, "angle": 45, "distance": 15, "blur": 10,
                    "red": 0.2, "green": 0.2, "blue": 0.3, "opacity": 0.75
                }
            }"#,
        )
        .unwrap();
        assert_eq!(hidden.shadow.as_ref().unwrap().distance, 15.0);
        assert!(!hidden.is_enabled(LayerEffectKind::Shadow));
        assert!(hidden.visible().shadow.is_none());

        let encoded = serde_json::to_value(&hidden).unwrap();
        assert_eq!(encoded["shadow"]["enabled"], json!(false));
        let round_trip: LayerEffects = serde_json::from_value(encoded).unwrap();
        assert_eq!(round_trip, hidden);
    }

    #[test]
    fn accessors_read_and_write_one_effect() {
        let mut effects = LayerEffects::default();
        assert!(effects.is_empty());
        assert!(!effects.contains(LayerEffectKind::InnerGlow));
        assert!(effects.kinds().is_empty());

        effects.inner_glow = Some(InnerGlowEffect {
            size: 15.0,
            ..Default::default()
        });
        assert!(!effects.is_empty());
        assert!(effects.contains(LayerEffectKind::InnerGlow));
        assert!(effects.is_enabled(LayerEffectKind::InnerGlow));
        assert_eq!(effects.kinds(), vec![LayerEffectKind::InnerGlow]);

        // Disabling keeps the effect but hides it from `visible`.
        effects.set_enabled(false, LayerEffectKind::InnerGlow);
        assert!(!effects.is_enabled(LayerEffectKind::InnerGlow));
        assert!(effects.visible().inner_glow.is_none());
        assert!(effects.contains(LayerEffectKind::InnerGlow));

        effects.set_color(PaletteColor::new(1.0, 0.8, 0.0), LayerEffectKind::InnerGlow);
        assert_eq!(
            effects.color(LayerEffectKind::InnerGlow),
            Some(PaletteColor::new(1.0, 0.8, 0.0))
        );
        assert_eq!(
            effects.color(LayerEffectKind::InnerGlow),
            effects.inner_glow.as_ref().map(InnerGlowEffect::color)
        );

        effects.remove(LayerEffectKind::InnerGlow);
        assert!(!effects.contains(LayerEffectKind::InnerGlow));
        assert!(effects.is_empty());

        // Writing through a kind with no effect is a no-op, as the Swift optional chain is.
        let mut empty = LayerEffects::default();
        empty.set_color(PaletteColor::WHITE, LayerEffectKind::Stroke);
        empty.set_enabled(false, LayerEffectKind::Stroke);
        assert!(empty.is_empty());
        assert_eq!(empty.color(LayerEffectKind::Stroke), None);
    }

    #[test]
    fn kinds_use_photoshops_display_names() {
        assert_eq!(LayerEffectKind::ALL.len(), 6);
        assert_eq!(LayerEffectKind::Stroke.raw_value(), "Stroke");
        assert_eq!(LayerEffectKind::Shadow.raw_value(), "Drop Shadow");
        assert_eq!(LayerEffectKind::ColorOverlay.raw_value(), "Color Overlay");
        assert_eq!(LayerEffectKind::InnerShadow.raw_value(), "Inner Shadow");
        assert_eq!(LayerEffectKind::OuterGlow.raw_value(), "Outer Glow");
        assert_eq!(LayerEffectKind::InnerGlow.raw_value(), "Inner Glow");
        for kind in LayerEffectKind::ALL {
            assert_eq!(LayerEffectKind::from_raw(kind.raw_value()), Some(kind));
        }
        assert_eq!(LayerEffectKind::from_raw("Nope"), None);
    }
}
