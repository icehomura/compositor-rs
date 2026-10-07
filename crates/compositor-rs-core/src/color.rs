//! `PaletteColor`: the straight (non-premultiplied) sRGB triple the palette, pickers and tool settings use.

use crate::geom::CGFloat;
use serde::{Deserialize, Serialize};

/// Straight sRGB components, 0…1 — the Swift `PaletteColor`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaletteColor {
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
}

impl Default for PaletteColor {
    fn default() -> Self {
        PaletteColor::BLACK
    }
}

impl PaletteColor {
    pub const BLACK: PaletteColor = PaletteColor {
        red: 0.0,
        green: 0.0,
        blue: 0.0,
    };
    pub const WHITE: PaletteColor = PaletteColor {
        red: 1.0,
        green: 1.0,
        blue: 1.0,
    };

    pub const fn new(red: CGFloat, green: CGFloat, blue: CGFloat) -> Self {
        Self { red, green, blue }
    }

    /// The 8-bit sRGB triple.
    pub fn bytes(self) -> [u8; 3] {
        [to_byte(self.red), to_byte(self.green), to_byte(self.blue)]
    }

    pub fn from_bytes(bytes: [u8; 3]) -> Self {
        Self::new(
            bytes[0] as CGFloat / 255.0,
            bytes[1] as CGFloat / 255.0,
            bytes[2] as CGFloat / 255.0,
        )
    }

    /// Snaps to the 8-bit values that painting and export actually store.
    pub fn quantized(self) -> Self {
        Self::new(
            (self.red * 255.0).round() / 255.0,
            (self.green * 255.0).round() / 255.0,
            (self.blue * 255.0).round() / 255.0,
        )
    }

    pub fn hex(self) -> String {
        format!(
            "{:02X}{:02X}{:02X}",
            (self.red * 255.0).round() as i32,
            (self.green * 255.0).round() as i32,
            (self.blue * 255.0).round() as i32
        )
    }

    /// Accepts `RRGGBB` or shorthand `RGB`, with or without a leading `#`.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let mut text = hex.trim().to_string();
        if text.starts_with('#') {
            text.remove(0);
        }
        if text.chars().count() == 3 {
            text = text.chars().flat_map(|c| [c, c]).collect();
        }
        if text.len() != 6 {
            return None;
        }
        let value = u32::from_str_radix(&text, 16).ok()?;
        Some(Self::new(
            ((value >> 16) & 0xFF) as CGFloat / 255.0,
            ((value >> 8) & 0xFF) as CGFloat / 255.0,
            (value & 0xFF) as CGFloat / 255.0,
        ))
    }

    /// `NSColor(red:green:blue:alpha: 1)` clamped into sRGB the way `PaletteColor(_ color: NSColor)` did.
    pub fn clamped(self) -> Self {
        Self::new(
            self.red.clamp(0.0, 1.0),
            self.green.clamp(0.0, 1.0),
            self.blue.clamp(0.0, 1.0),
        )
    }

    /// Relative luminance (Rec. 709) of the straight color, used by tinting labels.
    pub fn luminance(self) -> CGFloat {
        0.2126 * self.red + 0.7152 * self.green + 0.0722 * self.blue
    }
}

pub fn to_byte(component: CGFloat) -> u8 {
    // Rounding a non-negative value is the same as adding a half and truncating. The sum is exact —
    // its fractions are multiples of 2⁻⁴⁵, well inside a double — so this agrees with `round` bit for
    // bit, and `as u8` truncates and saturates where `round` would give 256 for a value of 1.
    (component.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

pub fn from_byte(byte: u8) -> CGFloat {
    byte as CGFloat / 255.0
}

/// HSV, used by the color picker and by Hue/Saturation adjustments.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hsv {
    /// Degrees, 0…360.
    pub hue: CGFloat,
    /// 0…1.
    pub saturation: CGFloat,
    /// 0…1.
    pub value: CGFloat,
}

impl Hsv {
    pub const fn new(hue: CGFloat, saturation: CGFloat, value: CGFloat) -> Self {
        Self { hue, saturation, value }
    }

    pub fn from_color(color: PaletteColor) -> Self {
        let max = color.red.max(color.green).max(color.blue);
        let min = color.red.min(color.green).min(color.blue);
        let delta = max - min;
        let mut hue: CGFloat = 0.0;
        if delta > 0.0 {
            if max == color.red {
                hue = 60.0 * (((color.green - color.blue) / delta) % 6.0);
            } else if max == color.green {
                hue = 60.0 * ((color.blue - color.red) / delta + 2.0);
            } else {
                hue = 60.0 * ((color.red - color.green) / delta + 4.0);
            }
        }
        if hue < 0.0 {
            hue += 360.0;
        }
        Self {
            hue,
            saturation: if max == 0.0 { 0.0 } else { delta / max },
            value: max,
        }
    }

    pub fn to_color(self) -> PaletteColor {
        if self.saturation <= 0.0 {
            return PaletteColor::new(self.value, self.value, self.value);
        }
        let hue = ((self.hue % 360.0) + 360.0) % 360.0 / 60.0;
        let chroma = self.value * self.saturation;
        let x = chroma * (1.0 - ((hue % 2.0) - 1.0).abs());
        let (r, g, b) = match hue as i32 {
            0 => (chroma, x, 0.0),
            1 => (x, chroma, 0.0),
            2 => (0.0, chroma, x),
            3 => (0.0, x, chroma),
            4 => (x, 0.0, chroma),
            _ => (chroma, 0.0, x),
        };
        let m = self.value - chroma;
        PaletteColor::new(r + m, g + m, b + m)
    }
}

#[cfg(test)]
mod tests {
    /// `to_byte` claims to agree with `round`, and the blends only ever feed it `a + b·(1 − s)` of
    /// three byte-derived values — a finite set that can be walked in full.
    #[test]
    fn to_byte_agrees_with_rounding_at_every_blend_input() {
        let unit = |byte: u8| byte as f64 / 255.0;
        for source in 0..=255u8 {
            let source = unit(source);
            for backdrop in 0..=255u8 {
                let backdrop = unit(backdrop);
                for sa in [0.0, 0.25, unit(89), unit(128), 1.0] {
                    let value = source + backdrop * (1.0 - sa);
                    let clamped = value.clamp(0.0, 1.0);
                    assert_eq!(
                        to_byte(value),
                        (clamped * 255.0).round() as u8,
                        "value {value} from source {source} backdrop {backdrop} sa {sa}"
                    );
                }
            }
        }
    }

    use super::*;

    #[test]
    fn hex_round_trips_and_accepts_shorthand() {
        // `F80` doubles each nibble, so it is FF8800 — not FF8000.
        assert_eq!(PaletteColor::from_hex("#F80"), PaletteColor::from_hex("FF8800"));
        assert_eq!(PaletteColor::new(1.0, 0.5, 0.0).hex(), "FF8000");
        assert_eq!(PaletteColor::from_hex("#GGGGGG"), None);
        assert_eq!(PaletteColor::from_hex("ffffff"), Some(PaletteColor::WHITE));

        // Ported from CompositorTests.ColorPickerTests.hexParsesFullShorthandAndRejectsInvalid.
        assert_eq!(PaletteColor::from_hex("#FF8000"), Some(PaletteColor::new(1.0, 128.0 / 255.0, 0.0)));
        assert_eq!(PaletteColor::from_hex("0f0"), Some(PaletteColor::new(0.0, 1.0, 0.0)));
        assert_eq!(PaletteColor::from_hex(" 00ff00 "), Some(PaletteColor::new(0.0, 1.0, 0.0)));
        assert_eq!(PaletteColor::from_hex("12345"), None, "five digits are not a color");
    }

    #[test]
    fn hsv_matches_the_picker_wheel() {
        let red = PaletteColor::new(1.0, 0.0, 0.0);
        let hsv = Hsv::from_color(red);
        assert!((hsv.hue - 0.0).abs() < 1e-9 && (hsv.saturation - 1.0).abs() < 1e-9);
        let back = hsv.to_color();
        assert!((back.red - 1.0).abs() < 1e-9 && back.green.abs() < 1e-9);
    }
}
