//! The sample ring shown while the eyedropper samples a color (`SampleRingOverlay`): display-only
//! comparison, the new sample above, the pre-drag color below.
//!
//! The ring is a full circle stroked in gray, with its top and bottom halves drawn over again in the
//! two colors. The Swift clips the ring with each half's rectangle; here each half is stroked as its
//! own arc, which ends on the same horizontal line with butt caps.

use compositor_core::color::PaletteColor;
use compositor_core::geom::{CGFloat, Point, Size};
use gpui_kit::*;
// gpui's own point, in window pixels, as opposed to core's `Point` (view space, CGFloat points).
use gpui_kit::Point as WindowPoint;

use super::{gray_rgba, palette_rgba};

/// The ring's frame in points; the canvas centers it on the pointer
/// (`sampleRing.frame = CGRect(x: point.x - 58, y: point.y - 58, width: 116, height: 116)`).
pub const SAMPLE_RING_SIZE: CGFloat = 116.0;
/// Half of [`SAMPLE_RING_SIZE`]: how far the frame's origin sits before the pointer.
pub const SAMPLE_RING_OFFSET: CGFloat = 58.0;
/// `bounds.insetBy(dx: 15, dy: 15)`: the ring path's inset from the frame.
const RING_INSET: CGFloat = 15.0;
/// The gray the whole ring is stroked with before the halves go over it (`NSColor(white: 0.45)`).
const RING_GRAY: f32 = 0.45;
/// `ring.lineWidth = 24`, then `16` for the colored halves.
const RING_WIDTH: f32 = 24.0;
const HALF_WIDTH: f32 = 16.0;

/// What the ring shows: the pointer it follows (canvas view points) and the two colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleRingState {
    /// The pointer, in canvas view points.
    pub point: Point,
    /// The color before the drag began.
    pub original: PaletteColor,
    /// The color being sampled now.
    pub sampled: PaletteColor,
}

/// The ring's path rectangle inside a frame of `size`: the frame inset by 15 points.
pub fn ring_frame(size: Size) -> compositor_core::geom::Rect {
    compositor_core::geom::Rect::new(
        RING_INSET,
        RING_INSET,
        (size.width - RING_INSET * 2.0).max(0.0),
        (size.height - RING_INSET * 2.0).max(0.0),
    )
}

/// The half of the frame a color is clipped to: `CGRect(x: 0, y: 0|midY, width:, height: / 2)`,
/// the new sample above (`true`), the pre-drag color below.
pub fn half_frame(size: Size, sampled: bool) -> compositor_core::geom::Rect {
    compositor_core::geom::Rect::new(
        0.0,
        if sampled { 0.0 } else { size.height / 2.0 },
        size.width,
        size.height / 2.0,
    )
}

/// The ring at `state.point`, in a [`SAMPLE_RING_SIZE`]-point square, the way the canvas placed the
/// Swift overlay's frame.
pub fn sample_ring(state: SampleRingState) -> impl IntoElement {
    let sampled = palette_rgba(state.sampled);
    let original = palette_rgba(state.original);
    let left = (state.point.x - SAMPLE_RING_OFFSET) as f32;
    let top = (state.point.y - SAMPLE_RING_OFFSET) as f32;

    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = point(
                px(f32::from(bounds.left()) + f32::from(bounds.size.width) / 2.0),
                px(f32::from(bounds.top()) + f32::from(bounds.size.height) / 2.0),
            );
            let radius = point(
                px((f32::from(bounds.size.width) / 2.0 - RING_INSET as f32).max(0.0)),
                px((f32::from(bounds.size.height) / 2.0 - RING_INSET as f32).max(0.0)),
            );
            if f32::from(radius.x) <= 0.0 || f32::from(radius.y) <= 0.0 {
                return;
            }
            stroke_ring(window, center, radius, RING_WIDTH, gray_rgba(RING_GRAY, 1.0), 0.0, std::f32::consts::TAU);
            // The halves are the clipped strokes: top in the sample, bottom in the original.
            stroke_ring(window, center, radius, HALF_WIDTH, sampled, std::f32::consts::PI, std::f32::consts::TAU);
            stroke_ring(window, center, radius, HALF_WIDTH, original, 0.0, std::f32::consts::PI);
        },
    )
    .absolute()
    .left(px(left))
    .top(px(top))
    .w(px(SAMPLE_RING_SIZE as f32))
    .h(px(SAMPLE_RING_SIZE as f32))
}

/// Strokes the ellipse `center ± radius` from `start` to `end` radians, in view space (y down).
fn stroke_ring(
    window: &mut Window,
    center: WindowPoint<Pixels>,
    radius: WindowPoint<Pixels>,
    width: f32,
    color: Rgba,
    start: f32,
    end: f32,
) {
    let mut builder = PathBuilder::stroke(px(width));
    let steps = (((end - start).abs() / std::f32::consts::TAU) * ELLIPSE_STEPS as f32).ceil().max(2.0) as usize;
    for step in 0..=steps {
        let angle = start + (end - start) * step as f32 / steps as f32;
        let at = point(
            center.x + px(f32::from(radius.x) * angle.cos()),
            center.y + px(f32::from(radius.y) * angle.sin()),
        );
        if step == 0 {
            builder.move_to(at);
        } else {
            builder.line_to(at);
        }
    }
    if end - start >= std::f32::consts::TAU - 1e-4 {
        builder.close();
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Segments a full ellipse is approximated with; the radius is at most 58 points, so the error at 128
/// segments is well under a tenth of a pixel.
const ELLIPSE_STEPS: usize = 128;

#[cfg(test)]
mod tests {
    // `use super::*` would also bring in the toolkit's `test` attribute macro (gpui-kit's test-support
    // exports one), which shadows the built-in `#[test]`; these tests are the built-in's.
    use ::core::prelude::v1::test;
    use super::*;

    #[test]
    fn the_frame_is_the_swift_overlays_116_point_square() {
        let size = Size::new(SAMPLE_RING_SIZE, SAMPLE_RING_SIZE);
        assert_eq!(ring_frame(size), compositor_core::geom::Rect::new(15.0, 15.0, 86.0, 86.0));
    }

    #[test]
    fn the_halves_split_the_frame_at_its_middle() {
        let size = Size::new(SAMPLE_RING_SIZE, SAMPLE_RING_SIZE);
        assert_eq!(half_frame(size, true), compositor_core::geom::Rect::new(0.0, 0.0, 116.0, 58.0));
        assert_eq!(half_frame(size, false), compositor_core::geom::Rect::new(0.0, 58.0, 116.0, 58.0));
    }

    #[test]
    fn the_ring_sits_around_the_pointer_at_the_swift_offset() {
        let state = SampleRingState {
            point: Point::new(200.0, 100.0),
            original: PaletteColor::BLACK,
            sampled: PaletteColor::WHITE,
        };
        // The frame's origin is 58 points before the pointer on both axes; the ring path is inset 15.
        assert_eq!(
            compositor_core::geom::Rect::new(
                state.point.x - SAMPLE_RING_OFFSET + RING_INSET,
                state.point.y - SAMPLE_RING_OFFSET + RING_INSET,
                2.0 * (SAMPLE_RING_OFFSET - RING_INSET),
                2.0 * (SAMPLE_RING_OFFSET - RING_INSET),
            ),
            compositor_core::geom::Rect::new(157.0, 57.0, 86.0, 86.0)
        );
    }
}
