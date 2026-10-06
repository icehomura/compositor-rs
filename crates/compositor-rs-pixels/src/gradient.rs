//! The gradient tool's paint: the ramp its two ends make, and the `CGContext` draws that put it on
//! a layer or a mask (`Document/Gradient.swift`'s `gradientColors(mask:)` and
//! `BrushStroke.fillGradient`), plus the tables a gradient builds for a table-driven path (the
//! gradient map's 256 × 3 sRGB lookup, and the mask ramp's encoding for `Canvas`'s gray targets).
//!
//! The drag's endpoints, the shape and the opacity come from `compositor_rs_core::image_ops::
//! GradientSettings`; the colors come from the palette, as the Swift's `EditorSession.
//! gradientColors(mask:)` reads them.

use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::geom::{Point, Rect};
use compositor_rs_core::image_ops::{GradientSettings, GradientShape, GradientStyle};
use compositor_rs_core::layer_adjustment::{AdjustmentColor, GradientMapSettings};

use crate::canvas::{Canvas, GradientExtend, GradientKind, GradientPaint};

/// `GradientEdit.hasLine`: a drag shorter than this is a click and leaves nothing pending.
pub const MINIMUM_LINE: f64 = 0.5;

/// Whether the pending gradient has a line to paint.
pub fn has_line(start: Point, end: Point) -> bool {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    (dx * dx + dy * dy).sqrt() >= MINIMUM_LINE
}

/// `EditorSession.gradientColors(mask:)`: the ramp's two ends, in the order they apply.
///
/// `foregroundToBackground` runs from the foreground to the background color;
/// `foregroundToTransparent` runs from the foreground to the same color at zero alpha. `reversed`
/// swaps the two.
///
/// On a mask the colors are device gray built from the palette color's red channel, exactly as the
/// Swift's `CGColor(colorSpace: CGColorSpaceCreateDeviceGray(), components: [value.red, alpha])`
/// does.
pub fn gradient_colors(
    settings: &GradientSettings,
    foreground: PaletteColor,
    background: PaletteColor,
    mask: bool,
) -> [[f64; 4]; 2] {
    let (start, end) = match settings.style {
        GradientStyle::ForegroundToBackground => (foreground, background),
        GradientStyle::ForegroundToTransparent => (foreground, foreground),
    };
    let alphas = match settings.style {
        GradientStyle::ForegroundToBackground => [1.0, 1.0],
        GradientStyle::ForegroundToTransparent => [1.0, 0.0],
    };
    let color = |color: PaletteColor, alpha: f64| -> [f64; 4] {
        if mask {
            [color.red, color.red, color.red, alpha]
        } else {
            [color.red, color.green, color.blue, alpha]
        }
    };
    let colors = [color(start, alphas[0]), color(end, alphas[1])];
    if settings.reversed {
        [colors[1], colors[0]]
    } else {
        colors
    }
}

/// The two stops [`Canvas::fill_gradient`] paints from `gradient_colors`.
///
/// A gray (mask) target takes the paint's **alpha** as the coverage it writes (see
/// `Canvas::blend_into_target`: "Coverage onto a mask is a plain source-over of the alpha
/// channel"), while Core Graphics composited the gradient's own gray into a device-gray context.
/// The two agree for this tool's two styles — `foregroundToBackground` holds alpha at 1 the whole
/// way, and `foregroundToTransparent` holds the gray fixed and ramps only the alpha — so the mask
/// ramp is carried in the alpha channel as `gray × alpha`, which is the same linear ramp in both
/// cases and reproduces the Swift's result exactly.
pub fn gradient_stops(
    settings: &GradientSettings,
    foreground: PaletteColor,
    background: PaletteColor,
    mask: bool,
) -> Vec<(f64, [f64; 4])> {
    let colors = gradient_colors(settings, foreground, background, mask);
    let stops: Vec<[f64; 4]> = colors
        .into_iter()
        .map(|color| {
            if mask {
                let value = color[0] * color[3];
                [value, value, value, value]
            } else {
                color
            }
        })
        .collect();
    vec![(0.0, stops[0]), (1.0, stops[1])]
}

/// The gradient paint a drag makes: linear from start to end, or radial about the start with the
/// end on its rim, both padded past their ends as `CGGradient` drawing options
/// `[.drawsBeforeStartLocation, .drawsAfterEndLocation]` ask.
pub fn gradient_paint(
    settings: &GradientSettings,
    start: Point,
    end: Point,
    foreground: PaletteColor,
    background: PaletteColor,
    mask: bool,
) -> GradientPaint {
    let radius = {
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        (dx * dx + dy * dy).sqrt()
    };
    let kind = match settings.shape {
        GradientShape::Linear => GradientKind::Linear { from: start, to: end },
        GradientShape::Radial => GradientKind::Radial { center: start, radius },
    };
    GradientPaint {
        kind,
        stops: gradient_stops(settings, foreground, background, mask),
        extend: GradientExtend::Pad,
    }
}

/// `BrushStroke.fillGradient`: replaces the pending gradient over `rect` (the document canvas, or
/// the selection when the context is clipped to one), composited onto what is already there.
///
/// `start` and `end` are in the canvas's current user space — the document pixels the drag was
/// made in — and `rect` is what the gradient covers; the caller sets up the transform from
/// document space and the clip before calling.
pub fn fill_gradient(
    canvas: &mut Canvas,
    rect: Rect,
    settings: &GradientSettings,
    start: Point,
    end: Point,
    foreground: PaletteColor,
    background: PaletteColor,
) {
    let mask = canvas.is_mask();
    let paint = gradient_paint(settings, start, end, foreground, background, mask);
    canvas.set_alpha(settings.opacity.clamp(0.0, 1.0));
    canvas.fill_gradient(rect, &paint);
}

/// `GradientMapSettings.ends`: the colors for the darkest and lightest tones, in the order they
/// apply.
pub fn gradient_map_ends(settings: &GradientMapSettings) -> (AdjustmentColor, AdjustmentColor) {
    if settings.reversed {
        (settings.highlights, settings.shadows)
    } else {
        (settings.shadows, settings.highlights)
    }
}

/// `GradientMapSettings.apply`'s lookup: the output color for each input brightness, 256 × 3
/// straight sRGB bytes, darkest first — the table `adjust_gradient_map` reads.
pub fn gradient_map_table(settings: &GradientMapSettings) -> [u8; 256 * 3] {
    let (dark, light) = gradient_map_ends(settings);
    fn channel(from: f64, to: f64, t: f64) -> u8 {
        let value = from + (to - from) * t;
        let scaled = (value * 255.0).round();
        scaled.clamp(0.0, 255.0) as u8
    }
    let mut table = [0u8; 256 * 3];
    for index in 0..256 {
        let t = index as f64 / 255.0;
        table[index * 3] = channel(dark.red, light.red, t);
        table[index * 3 + 1] = channel(dark.green, light.green, t);
        table[index * 3 + 2] = channel(dark.blue, light.blue, t);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::geom::Size;

    fn settings() -> GradientSettings {
        GradientSettings {
            style: GradientStyle::ForegroundToBackground,
            ..GradientSettings::default()
        }
    }

    #[test]
    fn a_click_shorter_than_half_a_pixel_has_no_line() {
        let start = Point::new(10.0, 10.0);
        assert!(!has_line(start, Point::new(10.3, 10.0)));
        assert!(!has_line(start, Point::new(10.0, 10.0)));
        assert!(has_line(start, Point::new(10.5, 10.0)));
        assert!(has_line(start, Point::new(10.0, 11.0)));
    }

    #[test]
    fn foreground_to_background_runs_between_the_two_palette_colors() {
        let colors = gradient_colors(
            &settings(),
            PaletteColor::BLACK,
            PaletteColor::WHITE,
            false,
        );
        assert_eq!(colors[0], [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(colors[1], [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn foreground_to_transparent_keeps_the_color_and_ramps_the_alpha() {
        let settings = GradientSettings {
            style: GradientStyle::ForegroundToTransparent,
            ..GradientSettings::default()
        };
        let colors = gradient_colors(&settings, PaletteColor::new(1.0, 0.0, 0.0), PaletteColor::WHITE, false);
        assert_eq!(colors[0], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(colors[1], [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn reversed_swaps_the_two_ends() {
        let colors = gradient_colors(
            &GradientSettings { reversed: true, ..settings() },
            PaletteColor::BLACK,
            PaletteColor::WHITE,
            false,
        );
        assert_eq!(colors[0], [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(colors[1], [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_mask_ramp_is_gray_and_is_carried_in_the_alpha_channel() {
        let stops = gradient_stops(&settings(), PaletteColor::BLACK, PaletteColor::WHITE, true);
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0], (0.0, [0.0, 0.0, 0.0, 0.0]));
        assert_eq!(stops[1], (1.0, [1.0, 1.0, 1.0, 1.0]));
        // A palette color with a gray in its red channel.
        let gray = PaletteColor::new(0.5, 0.5, 0.5);
        let stops = gradient_stops(
            &GradientSettings { style: GradientStyle::ForegroundToTransparent, ..GradientSettings::default() },
            gray,
            gray,
            true,
        );
        assert_eq!(stops[0].1, [0.5, 0.5, 0.5, 0.5]);
        assert_eq!(stops[1].1, [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_linear_ramp_reads_black_to_white_across_the_line() {
        let mut canvas = Canvas::new_rgba(101, 4);
        let rect = Rect::new(0.0, 0.0, 101.0, 4.0);
        let start = Point::new(0.5, 2.0);
        let end = Point::new(100.5, 2.0);
        fill_gradient(
            &mut canvas,
            rect,
            &settings(),
            start,
            end,
            PaletteColor::BLACK,
            PaletteColor::WHITE,
        );
        let image = canvas.into_rgba();
        let pixel = |x: usize| image.get(x, 0);
        // Padded before the start and after the end, so the whole rect is painted.
        assert_eq!(pixel(0), [0, 0, 0, 255]);
        assert_eq!(pixel(100)[0], 255);
        let middle = pixel(50)[0];
        assert!((middle as i32 - 128).abs() <= 2, "middle {middle}");
        assert_eq!(pixel(50)[1], middle);
        assert_eq!(pixel(50)[2], middle);
        assert_eq!(pixel(50)[3], 255);
    }

    #[test]
    fn a_radial_ramp_is_the_same_distance_in_every_direction() {
        let mut canvas = Canvas::new_rgba(101, 101);
        let rect = Rect::new(0.0, 0.0, 101.0, 101.0);
        let start = Point::new(50.5, 50.5);
        let end = Point::new(90.5, 50.5);
        let settings = GradientSettings { shape: GradientShape::Radial, ..settings() };
        fill_gradient(
            &mut canvas,
            rect,
            &settings,
            start,
            end,
            PaletteColor::BLACK,
            PaletteColor::WHITE,
        );
        let image = canvas.into_rgba();
        let center = image.get(50, 50)[0];
        assert!(center <= 2, "the center is the start color: {center}");
        let ring: Vec<u8> = [(70, 50), (30, 50), (50, 70), (50, 30)]
            .into_iter()
            .map(|(x, y)| image.get(x, y)[0])
            .collect();
        assert!(ring.iter().all(|value| (*value as i32 - 128).abs() <= 5), "{ring:?}");
        // The rim and beyond: the end color.
        assert_eq!(image.get(90, 50)[0], 255);
        assert_eq!(image.get(0, 0)[0], 255);
    }

    #[test]
    fn the_opacity_multiplies_the_ramp_and_reverse_runs_it_backwards() {
        let mut canvas = Canvas::new_rgba(101, 4);
        let rect = Rect::new(0.0, 0.0, 101.0, 4.0);
        let settings = GradientSettings {
            style: GradientStyle::ForegroundToBackground,
            reversed: true,
            opacity: 0.5,
            ..GradientSettings::default()
        };
        fill_gradient(
            &mut canvas,
            rect,
            &settings,
            Point::new(0.5, 2.0),
            Point::new(100.5, 2.0),
            PaletteColor::BLACK,
            PaletteColor::WHITE,
        );
        let image = canvas.into_rgba();
        // White at the start at half alpha on the blank canvas.
        assert_eq!(image.get(0, 2)[0], 128);
        assert_eq!(image.get(0, 2)[3], 128);
        // Black at the end, still half opaque.
        assert_eq!(image.get(100, 2)[0], 0);
        assert_eq!(image.get(100, 2)[3], 128);
    }

    #[test]
    fn a_foreground_to_transparent_ramp_over_pixels_keeps_them_where_it_fades() {
        let mut canvas = Canvas::from_rgba(compositor_rs_core::Rgba8Image::opaque(10, 1, [255, 0, 0, 255]));
        let rect = Rect::new(0.0, 0.0, 10.0, 1.0);
        let settings = GradientSettings {
            style: GradientStyle::ForegroundToTransparent,
            ..GradientSettings::default()
        };
        fill_gradient(
            &mut canvas,
            rect,
            &settings,
            Point::new(-0.5, 0.5),
            Point::new(10.5, 0.5),
            PaletteColor::BLACK,
            PaletteColor::BLACK,
        );
        let image = canvas.into_rgba();
        // Black at the start, mostly red by the far end, with the layer showing through as the ramp
        // fades: the pixels are at x + 0.5, and the ramp runs from -0.5 to 10.5.
        let start = image.get(0, 0);
        assert!(start[0] <= 25 && start[3] == 255, "{start:?}");
        let end = image.get(9, 0);
        assert!(end[0] >= 230 && end[3] == 255, "{end:?}");
        assert!(end[0] > start[0]);
    }

    #[test]
    fn the_gradient_map_table_interpolates_each_channel_between_the_ends() {
        let settings = GradientMapSettings::default();
        let table = gradient_map_table(&settings);
        assert_eq!(&table[0..3], &[0, 0, 0]);
        assert_eq!(&table[255 * 3..255 * 3 + 3], &[255, 255, 255]);
        // Halfway is a rounded half.
        assert_eq!(&table[128 * 3..128 * 3 + 3], &[128, 128, 128]);

        let colored = GradientMapSettings {
            shadows: AdjustmentColor { red: 0.0, green: 0.0, blue: 1.0 },
            highlights: AdjustmentColor { red: 1.0, green: 0.0, blue: 0.0 },
            reversed: false,
        };
        let table = gradient_map_table(&colored);
        assert_eq!(&table[0..3], &[0, 0, 255]);
        assert_eq!(&table[255 * 3..255 * 3 + 3], &[255, 0, 0]);
        // Reversed applies the ends the other way round.
        let reversed = gradient_map_table(&GradientMapSettings { reversed: true, ..colored });
        assert_eq!(&reversed[0..3], &[255, 0, 0]);
        assert_eq!(&reversed[255 * 3..255 * 3 + 3], &[0, 0, 255]);
    }

    #[test]
    fn a_paint_is_linear_or_radial_from_the_same_drag() {
        let linear = gradient_paint(
            &settings(),
            Point::new(1.0, 2.0),
            Point::new(4.0, 6.0),
            PaletteColor::BLACK,
            PaletteColor::WHITE,
            false,
        );
        assert!(matches!(linear.kind, GradientKind::Linear { .. }));
        assert_eq!(linear.extend, GradientExtend::Pad);
        let radial = gradient_paint(
            &GradientSettings { shape: GradientShape::Radial, ..settings() },
            Point::new(1.0, 2.0),
            Point::new(4.0, 6.0),
            PaletteColor::BLACK,
            PaletteColor::WHITE,
            false,
        );
        match radial.kind {
            GradientKind::Radial { center, radius } => {
                assert_eq!(center, Point::new(1.0, 2.0));
                assert_eq!(radius, 5.0);
            }
            _ => panic!("expected a radial paint"),
        }
    }

    #[test]
    fn a_canvas_that_is_not_square_keeps_the_rect_it_is_given() {
        let mut canvas = Canvas::new_rgba(4, 4);
        let rect = Rect::from_origin_size(Point::new(2.0, 0.0), Size::new(2.0, 4.0));
        fill_gradient(
            &mut canvas,
            rect,
            &settings(),
            Point::new(0.0, 0.0),
            Point::new(4.0, 0.0),
            PaletteColor::BLACK,
            PaletteColor::WHITE,
        );
        let image = canvas.into_rgba();
        // The untouched half stays transparent; the filled half holds the ramp.
        assert_eq!(image.get(0, 0), [0, 0, 0, 0]);
        assert!(image.get(2, 0)[3] > 0 && image.get(3, 0)[3] > 0);
    }
}
