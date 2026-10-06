//! The canvas overlays: the transform box and handles, rulers' guides, brush cursor and sample ring, and
//! the canvas lines a drag uses.
//!
//! Every overlay is a gpui element drawing into the canvas's paint pass; the geometry comes from the
//! session and these modules only turn it into shapes (see `docs/PORTING.md` § 6).

pub mod brush_cursor;
pub mod canvas_lines;
pub mod sample_ring;
pub mod transform;

use compositor_rs_core::color::PaletteColor;
use gpui_kit::*;

/// A palette color as a gpui color (`PaletteColor.nsColor`), fully opaque: the overlays' white, black and
/// accent strokes are opaque unless the Swift gave them an alpha.
pub(crate) fn palette_rgba(color: PaletteColor) -> Rgba {
    Rgba {
        r: color.red as f32,
        g: color.green as f32,
        b: color.blue as f32,
        a: 1.0,
    }
}

/// A palette color with an explicit alpha.
pub(crate) fn palette_rgba_alpha(color: PaletteColor, alpha: f32) -> Rgba {
    Rgba {
        a: alpha,
        ..palette_rgba(color)
    }
}

/// `NSColor(white:alpha:)` as a gpui color: the same gray in sRGB. AppKit's device gray and sRGB gray
/// coincide for these neutral values, which is the substitution `docs/PORTING.md` § 3 keeps for colors.
pub(crate) fn gray_rgba(white: f32, alpha: f32) -> Rgba {
    Rgba {
        r: white,
        g: white,
        b: white,
        a: alpha,
    }
}
