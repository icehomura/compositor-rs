//! The color picker's model: what the open picker edits, its working color, and the hue-preserving
//! HSB source of truth behind it.
//!
//! `PaletteColor` itself lives in [`crate::color`]; `ColorPickerTarget`, `ColorPickerState` and
//! `PickerHSB` are the Swift `ColorPalette.swift` types.

use crate::color::PaletteColor;
use crate::geom::CGFloat;
use crate::layer_effects::LayerEffectKind;
use crate::layer_text::LayerTextStyle;
use crate::Id;

/// What the open color picker edits: a palette swatch, or one end of the Gradient Map being edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorPickerTarget {
    Palette { background: bool },
    /// A layer effect's own color.
    Effect { kind: LayerEffectKind },
    GradientMap { highlights: bool },
    Vignette,
    /// Dither's Two Colors: the dark one or the light one.
    Dither { light: bool },
    Text { draft_id: Option<Id> },
    /// A dialog's own color, such as Export JPEG's background for transparency. The dialog is told as it changes.
    Dialog { title: String },
}

impl ColorPickerTarget {
    /// Whether this is the background palette swatch.
    pub fn background(&self) -> bool {
        matches!(self, ColorPickerTarget::Palette { background: true })
    }

    pub fn title(&self) -> String {
        match self {
            ColorPickerTarget::Text { .. } => "Color Picker (Text Color)".to_string(),
            ColorPickerTarget::Effect { kind } => format!("Color Picker ({} Color)", kind.raw_value()),
            ColorPickerTarget::Palette { background } => {
                if *background {
                    "Color Picker (Background Color)".to_string()
                } else {
                    "Color Picker (Foreground Color)".to_string()
                }
            }
            ColorPickerTarget::GradientMap { highlights } => {
                if *highlights {
                    "Color Picker (Gradient Map Highlights)".to_string()
                } else {
                    "Color Picker (Gradient Map Shadows)".to_string()
                }
            }
            ColorPickerTarget::Vignette => "Color Picker (Vignette Color)".to_string(),
            ColorPickerTarget::Dither { light } => {
                if *light {
                    "Color Picker (Dither Light Color)".to_string()
                } else {
                    "Color Picker (Dither Dark Color)".to_string()
                }
            }
            ColorPickerTarget::Dialog { title } => format!("Color Picker ({title})"),
        }
    }
}

/// The open color picker's working color. Nothing is written to the palette until OK.
#[derive(Clone, Debug)]
pub struct ColorPickerState {
    pub target: ColorPickerTarget,
    pub original: PaletteColor,
    /// The foreground picker opened while text was being edited: that text and the color it had.
    pub edited_text: Option<(Id, LayerTextStyle)>,
    pub hsb: PickerHSB,
}

impl ColorPickerState {
    pub fn new(target: ColorPickerTarget, original: PaletteColor) -> Self {
        Self {
            target,
            original,
            edited_text: None,
            hsb: PickerHSB::from_color(original),
        }
    }

    /// The palette picker for one of the two swatches.
    pub fn palette(background: bool, original: PaletteColor) -> Self {
        Self::new(ColorPickerTarget::Palette { background }, original)
    }

    pub fn background(&self) -> bool {
        self.target.background()
    }

    pub fn color(&self) -> PaletteColor {
        self.hsb.rgb().quantized()
    }
}

/// Hue in degrees, saturation and brightness 0...1. Kept as the picker's source of
/// truth so hue survives dragging through grays and black.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PickerHSB {
    pub hue: CGFloat,
    pub saturation: CGFloat,
    pub brightness: CGFloat,
}

impl PickerHSB {
    pub const fn new(hue: CGFloat, saturation: CGFloat, brightness: CGFloat) -> Self {
        Self {
            hue,
            saturation,
            brightness,
        }
    }

    pub fn from_color(color: PaletteColor) -> Self {
        let mut hsb = Self::new(0.0, 0.0, 0.0);
        hsb.set_rgb(color);
        hsb
    }

    pub fn rgb(&self) -> PaletteColor {
        let h = (((self.hue % 360.0) + 360.0) % 360.0) / 60.0;
        let c = self.brightness * self.saturation;
        let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
        let m = self.brightness - c;
        let (r, g, b) = match h as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        PaletteColor::new(r + m, g + m, b + m)
    }

    /// Updates from RGB while keeping the previous hue for grays and the previous
    /// saturation for black, matching how Photoshop's field behaves.
    pub fn set_rgb(&mut self, color: PaletteColor) {
        let high = color.red.max(color.green).max(color.blue);
        let low = color.red.min(color.green).min(color.blue);
        let delta = high - low;
        self.brightness = high;
        if high > 0.0 {
            self.saturation = delta / high;
        }
        if delta <= 0.0 {
            return;
        }
        let mut h: CGFloat;
        if high == color.red {
            h = (color.green - color.blue) / delta;
        } else if high == color.green {
            h = (color.blue - color.red) / delta + 2.0;
        } else {
            h = (color.red - color.green) / delta + 4.0;
        }
        h *= 60.0;
        self.hue = if h < 0.0 { h + 360.0 } else { h };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_hsb_round_trips_primaries() {
        for color in [
            PaletteColor::new(1.0, 0.0, 0.0),
            PaletteColor::new(0.0, 1.0, 0.0),
            PaletteColor::new(0.0, 0.0, 1.0),
            PaletteColor::new(0.2, 0.6, 0.9),
        ] {
            let hsb = PickerHSB::from_color(color);
            let back = hsb.rgb();
            assert!((back.red - color.red).abs() < 1e-9, "{back:?} != {color:?}");
            assert!((back.green - color.green).abs() < 1e-9, "{back:?} != {color:?}");
            assert!((back.blue - color.blue).abs() < 1e-9, "{back:?} != {color:?}");
        }
    }

    #[test]
    fn hue_survives_dragging_through_grays_and_black() {
        let mut hsb = PickerHSB::from_color(PaletteColor::new(1.0, 0.0, 0.0));
        let hue = hsb.hue;
        assert_eq!(hsb.hue, 0.0);
        assert_eq!(hsb.saturation, 1.0);
        hsb.set_rgb(PaletteColor::WHITE);
        assert_eq!(hsb.hue, hue, "a gray keeps the hue");
        assert_eq!(hsb.saturation, 0.0);
        hsb.set_rgb(PaletteColor::new(1.0, 0.0, 0.0));
        hsb.set_rgb(PaletteColor::BLACK);
        assert_eq!(hsb.hue, hue, "black keeps the hue");
        assert_eq!(hsb.saturation, 1.0, "black keeps the previous saturation");
    }

    #[test]
    fn target_titles_match_the_swift_wording() {
        assert_eq!(
            ColorPickerTarget::Palette { background: false }.title(),
            "Color Picker (Foreground Color)"
        );
        assert_eq!(
            ColorPickerTarget::Palette { background: true }.title(),
            "Color Picker (Background Color)"
        );
        assert_eq!(
            ColorPickerTarget::Effect { kind: LayerEffectKind::Shadow }.title(),
            "Color Picker (Drop Shadow Color)"
        );
        assert_eq!(
            ColorPickerTarget::GradientMap { highlights: true }.title(),
            "Color Picker (Gradient Map Highlights)"
        );
        assert_eq!(ColorPickerTarget::Vignette.title(), "Color Picker (Vignette Color)");
        assert_eq!(
            ColorPickerTarget::Dither { light: false }.title(),
            "Color Picker (Dither Dark Color)"
        );
        assert_eq!(ColorPickerTarget::Text { draft_id: None }.title(), "Color Picker (Text Color)");
        assert_eq!(
            ColorPickerTarget::Dialog { title: "Export JPEG".to_string() }.title(),
            "Color Picker (Export JPEG)"
        );
    }

    #[test]
    fn state_tracks_the_target_and_quantizes_its_color() {
        let mut state = ColorPickerState::palette(true, PaletteColor::WHITE);
        assert!(state.background());
        assert_eq!(state.original, PaletteColor::WHITE);
        assert_eq!(state.color(), PaletteColor::WHITE);

        state.hsb.set_rgb(PaletteColor::new(0.5, 0.25, 0.125));
        assert_eq!(state.color(), PaletteColor::new(0.5, 0.25, 0.125).quantized());
        assert!(!ColorPickerState::palette(false, PaletteColor::BLACK).background());
    }
}
