//! `LayerAppearance`'s blend-mode vocabulary: Photoshop's full set in its order, with the same menu
//! grouping and the same persisted raw values.

use serde::{Deserialize, Serialize};

/// `LayerBlendMode`. The raw strings are what the project manifest stores and what the UI labels show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum LayerBlendMode {
    #[default]
    #[serde(rename = "Normal")]
    Normal,
    #[serde(rename = "Darken")]
    Darken,
    #[serde(rename = "Multiply")]
    Multiply,
    #[serde(rename = "Color Burn")]
    ColorBurn,
    #[serde(rename = "Linear Burn")]
    LinearBurn,
    #[serde(rename = "Lighten")]
    Lighten,
    #[serde(rename = "Screen")]
    Screen,
    #[serde(rename = "Color Dodge")]
    ColorDodge,
    #[serde(rename = "Linear Dodge (Add)")]
    LinearDodge,
    #[serde(rename = "Overlay")]
    Overlay,
    #[serde(rename = "Soft Light")]
    SoftLight,
    #[serde(rename = "Hard Light")]
    HardLight,
    #[serde(rename = "Vivid Light")]
    VividLight,
    #[serde(rename = "Linear Light")]
    LinearLight,
    #[serde(rename = "Pin Light")]
    PinLight,
    #[serde(rename = "Hard Mix")]
    HardMix,
    #[serde(rename = "Difference")]
    Difference,
    #[serde(rename = "Exclusion")]
    Exclusion,
    #[serde(rename = "Subtract")]
    Subtract,
    #[serde(rename = "Divide")]
    Divide,
    #[serde(rename = "Hue")]
    Hue,
    #[serde(rename = "Saturation")]
    Saturation,
    #[serde(rename = "Color")]
    Color,
    #[serde(rename = "Luminosity")]
    Luminosity,
}

impl LayerBlendMode {
    /// `CaseIterable` order, which is also the picker's order.
    pub const ALL: [LayerBlendMode; 24] = [
        LayerBlendMode::Normal,
        LayerBlendMode::Darken,
        LayerBlendMode::Multiply,
        LayerBlendMode::ColorBurn,
        LayerBlendMode::LinearBurn,
        LayerBlendMode::Lighten,
        LayerBlendMode::Screen,
        LayerBlendMode::ColorDodge,
        LayerBlendMode::LinearDodge,
        LayerBlendMode::Overlay,
        LayerBlendMode::SoftLight,
        LayerBlendMode::HardLight,
        LayerBlendMode::VividLight,
        LayerBlendMode::LinearLight,
        LayerBlendMode::PinLight,
        LayerBlendMode::HardMix,
        LayerBlendMode::Difference,
        LayerBlendMode::Exclusion,
        LayerBlendMode::Subtract,
        LayerBlendMode::Divide,
        LayerBlendMode::Hue,
        LayerBlendMode::Saturation,
        LayerBlendMode::Color,
        LayerBlendMode::Luminosity,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            LayerBlendMode::Normal => "Normal",
            LayerBlendMode::Darken => "Darken",
            LayerBlendMode::Multiply => "Multiply",
            LayerBlendMode::ColorBurn => "Color Burn",
            LayerBlendMode::LinearBurn => "Linear Burn",
            LayerBlendMode::Lighten => "Lighten",
            LayerBlendMode::Screen => "Screen",
            LayerBlendMode::ColorDodge => "Color Dodge",
            LayerBlendMode::LinearDodge => "Linear Dodge (Add)",
            LayerBlendMode::Overlay => "Overlay",
            LayerBlendMode::SoftLight => "Soft Light",
            LayerBlendMode::HardLight => "Hard Light",
            LayerBlendMode::VividLight => "Vivid Light",
            LayerBlendMode::LinearLight => "Linear Light",
            LayerBlendMode::PinLight => "Pin Light",
            LayerBlendMode::HardMix => "Hard Mix",
            LayerBlendMode::Difference => "Difference",
            LayerBlendMode::Exclusion => "Exclusion",
            LayerBlendMode::Subtract => "Subtract",
            LayerBlendMode::Divide => "Divide",
            LayerBlendMode::Hue => "Hue",
            LayerBlendMode::Saturation => "Saturation",
            LayerBlendMode::Color => "Color",
            LayerBlendMode::Luminosity => "Luminosity",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }

    /// The menu label; identical to the raw value.
    pub fn label(self) -> &'static str {
        self.raw_value()
    }

    /// Photoshop's grouping: darkening modes together, then lightening, then contrast, then the
    /// comparative ones, then the component modes. The menu draws a line between each group.
    pub fn groups() -> &'static [&'static [LayerBlendMode]] {
        &[
            &[LayerBlendMode::Normal],
            &[
                LayerBlendMode::Darken,
                LayerBlendMode::Multiply,
                LayerBlendMode::ColorBurn,
                LayerBlendMode::LinearBurn,
            ],
            &[
                LayerBlendMode::Lighten,
                LayerBlendMode::Screen,
                LayerBlendMode::ColorDodge,
                LayerBlendMode::LinearDodge,
            ],
            &[
                LayerBlendMode::Overlay,
                LayerBlendMode::SoftLight,
                LayerBlendMode::HardLight,
                LayerBlendMode::VividLight,
                LayerBlendMode::LinearLight,
                LayerBlendMode::PinLight,
                LayerBlendMode::HardMix,
            ],
            &[
                LayerBlendMode::Difference,
                LayerBlendMode::Exclusion,
                LayerBlendMode::Subtract,
                LayerBlendMode::Divide,
            ],
            &[
                LayerBlendMode::Hue,
                LayerBlendMode::Saturation,
                LayerBlendMode::Color,
                LayerBlendMode::Luminosity,
            ],
        ]
    }

    /// The modes the original composited through a separate surface (Core Graphics got Color Burn and
    /// Color Dodge wrong, and had no equivalent for the rest). The Rust kernels compute every mode
    /// directly, and this stays for the renderer's planning and for the editor's own reasoning.
    pub fn needs_surface(self) -> bool {
        matches!(
            self,
            LayerBlendMode::ColorBurn
                | LayerBlendMode::ColorDodge
                | LayerBlendMode::SoftLight
                | LayerBlendMode::LinearBurn
                | LayerBlendMode::LinearDodge
                | LayerBlendMode::VividLight
                | LayerBlendMode::LinearLight
                | LayerBlendMode::PinLight
                | LayerBlendMode::HardMix
                | LayerBlendMode::Subtract
                | LayerBlendMode::Divide
        )
    }

    /// Whether the mode blends the two colors' components apart from their alpha and thus has to run in
    /// sRGB rather than a linear space — the note `SeparableBlend` carried for Color Burn, Color Dodge
    /// and Soft Light.
    pub fn is_gamma_sensitive(self) -> bool {
        matches!(self, LayerBlendMode::ColorBurn | LayerBlendMode::ColorDodge | LayerBlendMode::SoftLight)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_round_trip_and_match_the_manifest() {
        for mode in LayerBlendMode::ALL {
            assert_eq!(LayerBlendMode::from_raw(mode.raw_value()), Some(mode));
            // Ported from CompositorTests.LayerAppearanceTests.blendModesAndOpacityMatchKnownPixels:
            // every mode survives the manifest's JSON encoding.
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.raw_value()));
            assert_eq!(serde_json::from_str::<LayerBlendMode>(&json).unwrap(), mode);
        }
        assert_eq!(LayerBlendMode::LinearDodge.raw_value(), "Linear Dodge (Add)");
        assert_eq!(LayerBlendMode::from_raw("Nope"), None);
        assert_eq!(LayerBlendMode::default(), LayerBlendMode::Normal);
    }

    #[test]
    fn groups_cover_every_mode_exactly_once() {
        let mut total = 0;
        for group in LayerBlendMode::groups() {
            total += group.len();
        }
        assert_eq!(total, LayerBlendMode::ALL.len());
    }
}
