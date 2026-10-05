//! `LayerAppearance`'s blend vocabulary beyond the enum itself: the Core Graphics equivalents the
//! original handed straight to `CGContext`, and the Core Image filters it fell back to for the modes
//! Core Graphics could not draw — or drew wrongly.
//!
//! `LayerBlendMode` itself lives in [`crate::blend`], together with the pieces the port's own renderer
//! reads: [`LayerBlendMode::needs_surface`] is exactly the set [`core_image_filter`] names, and
//! [`LayerBlendMode::is_gamma_sensitive`] is the note the Core Image path carried. The Rust kernels
//! compute every mode directly, so nothing here is on the hot path; the tables are kept so the
//! original's mapping survives the port.

use crate::blend::LayerBlendMode;

/// The subset of `CGBlendMode` the original handed to Core Graphics. Every other mode went through a
/// Core Image filter or a hand-written surface pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CgBlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    SoftLight,
    HardLight,
    Darken,
    Lighten,
    Difference,
    Exclusion,
    ColorDodge,
    ColorBurn,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

/// What Core Graphics can draw directly. The rest are composited through Core Image or by hand,
/// so this is only meaningful for the modes [`LayerBlendMode::needs_surface`] leaves alone.
pub fn cg_mode(mode: LayerBlendMode) -> CgBlendMode {
    match mode {
        LayerBlendMode::Normal => CgBlendMode::Normal,
        LayerBlendMode::Multiply => CgBlendMode::Multiply,
        LayerBlendMode::Screen => CgBlendMode::Screen,
        LayerBlendMode::Overlay => CgBlendMode::Overlay,
        LayerBlendMode::SoftLight => CgBlendMode::SoftLight,
        LayerBlendMode::HardLight => CgBlendMode::HardLight,
        LayerBlendMode::Darken => CgBlendMode::Darken,
        LayerBlendMode::Lighten => CgBlendMode::Lighten,
        LayerBlendMode::Difference => CgBlendMode::Difference,
        LayerBlendMode::Exclusion => CgBlendMode::Exclusion,
        LayerBlendMode::ColorDodge => CgBlendMode::ColorDodge,
        LayerBlendMode::ColorBurn => CgBlendMode::ColorBurn,
        LayerBlendMode::Hue => CgBlendMode::Hue,
        LayerBlendMode::Saturation => CgBlendMode::Saturation,
        LayerBlendMode::Color => CgBlendMode::Color,
        LayerBlendMode::Luminosity => CgBlendMode::Luminosity,
        // Drawn through Core Image or by hand; never reaches Core Graphics.
        LayerBlendMode::LinearBurn
        | LayerBlendMode::LinearDodge
        | LayerBlendMode::VividLight
        | LayerBlendMode::LinearLight
        | LayerBlendMode::PinLight
        | LayerBlendMode::HardMix
        | LayerBlendMode::Subtract
        | LayerBlendMode::Divide => CgBlendMode::Normal,
    }
}

/// The Core Image filter that computes this mode, for the ones Core Graphics has no equivalent
/// for — or computes wrongly, as it does for Color Burn and Color Dodge, and for Soft Light, whose formula is up to
/// 25 levels off Photoshop's with a light blend color (Core Image's is within 5).
pub fn core_image_filter(mode: LayerBlendMode) -> Option<&'static str> {
    match mode {
        LayerBlendMode::ColorBurn => Some("CIColorBurnBlendMode"),
        LayerBlendMode::ColorDodge => Some("CIColorDodgeBlendMode"),
        LayerBlendMode::SoftLight => Some("CISoftLightBlendMode"),
        LayerBlendMode::LinearBurn => Some("CILinearBurnBlendMode"),
        LayerBlendMode::LinearDodge => Some("CILinearDodgeBlendMode"),
        LayerBlendMode::VividLight => Some("CIVividLightBlendMode"),
        LayerBlendMode::LinearLight => Some("CILinearLightBlendMode"),
        LayerBlendMode::PinLight => Some("CIPinLightBlendMode"),
        LayerBlendMode::HardMix => Some("CIHardMixBlendMode"),
        LayerBlendMode::Subtract => Some("CISubtractBlendMode"),
        LayerBlendMode::Divide => Some("CIDivideBlendMode"),
        _ => None,
    }
}

// Photoshop's Darker Color and Lighter Color are left out: they compare a pixel's whole
// brightness rather than working a channel at a time, and neither framework implements them.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_set_is_exactly_the_surface_set() {
        for mode in LayerBlendMode::ALL {
            assert_eq!(
                core_image_filter(mode).is_some(),
                mode.needs_surface(),
                "{mode:?} disagrees between the Core Image filters and the surface pass",
            );
        }
    }

    #[test]
    fn filter_names_are_the_core_image_ones() {
        assert_eq!(core_image_filter(LayerBlendMode::ColorBurn), Some("CIColorBurnBlendMode"));
        assert_eq!(core_image_filter(LayerBlendMode::SoftLight), Some("CISoftLightBlendMode"));
        assert_eq!(core_image_filter(LayerBlendMode::LinearDodge), Some("CILinearDodgeBlendMode"));
        assert_eq!(core_image_filter(LayerBlendMode::Normal), None);
        assert_eq!(core_image_filter(LayerBlendMode::Multiply), None);
    }

    #[test]
    fn cg_mode_maps_the_direct_modes_and_falls_back_to_normal() {
        assert_eq!(cg_mode(LayerBlendMode::Multiply), CgBlendMode::Multiply);
        assert_eq!(cg_mode(LayerBlendMode::Luminosity), CgBlendMode::Luminosity);
        for mode in [LayerBlendMode::LinearBurn, LayerBlendMode::HardMix, LayerBlendMode::Divide] {
            assert_eq!(cg_mode(mode), CgBlendMode::Normal);
        }
    }
}
