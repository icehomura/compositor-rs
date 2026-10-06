//! Layers-panel thumbnails framed by the whole canvas (`UI/CanvasThumbnail.swift`).
//!
//! A layer's pixels (or its mask) drawn where they sit on a canvas-shaped picture, whatever the
//! layer's own bounds. `NSImage`/`CGContext` become an [`Rgba8Image`] painted through
//! [`compositor_rs_pixels::canvas::Canvas`], so the panel turns it into a gpui image exactly the way
//! the canvas does its composite.

use std::sync::Arc;

use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_mask::LayerMask;
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::Rgba8Image;
use compositor_rs_pixels::canvas::Canvas;
use compositor_rs_render::LayerRenderer;

/// `CanvasThumbnail`: the pictures the Layers panel shows for a layer, a mask or an empty canvas.
pub struct CanvasThumbnail;

impl CanvasThumbnail {
    /// Pixels per point in the pictures, so they stay sharp on Retina displays (`backingScale`).
    pub const BACKING_SCALE: f64 = 2.0;

    /// The canvas's aspect ratio fitted inside a square slot `box` points wide, in whole points
    /// (`CanvasThumbnail.fittedSize(canvas:box:)`).
    pub fn fitted_size(canvas: Size, box_: f64) -> Size {
        if !(canvas.width > 0.0
            && canvas.height > 0.0
            && canvas.width.is_finite()
            && canvas.height.is_finite())
        {
            return Size::new(box_, box_);
        }
        let scale = box_ / canvas.width.max(canvas.height);
        Size::new(
            (canvas.width * scale).round().max(1.0),
            (canvas.height * scale).round().max(1.0),
        )
    }

    /// The transparency checkerboard with the layer's pixels (`image`, usually its small preview)
    /// placed on the canvas by `transform`. Without an image it is an empty canvas
    /// (`CanvasThumbnail.layer(_:transform:canvas:box:)`).
    pub fn layer(
        image: Option<&PixelImage>,
        transform: &LayerTransform,
        canvas: Size,
        box_: f64,
    ) -> Rgba8Image {
        let (mut target, scale) = Self::render(canvas, box_);
        let width = target.width() as f64;
        let height = target.height() as f64;
        target.set_fill_gray(0.22);
        target.fill_rect(Rect::new(0.0, 0.0, width, height));
        target.set_fill_gray(0.32);
        let tile = 6.0 * Self::BACKING_SCALE;
        let rows = (height / tile).ceil() as usize;
        let columns = (width / tile).ceil() as usize;
        for row in 0..rows {
            for column in 0..columns {
                if (row + column) % 2 == 0 {
                    target.fill_rect(Rect::new(
                        column as f64 * tile,
                        row as f64 * tile,
                        tile,
                        tile,
                    ));
                }
            }
        }
        if let Some(image) = image {
            Self::place(image, transform, scale, &mut target);
        }
        target.into_rgba()
    }

    /// The mask placed by `transform`. Past its pixels a mask carries on in its background, white or
    /// black, the way the canvas treats it ([`LayerMask::background`]): a reveal-all mask reads all
    /// white, a hide-all mask all black, and a stroke touching the mask's edge doesn't turn the rest
    /// gray (`CanvasThumbnail.mask(_:transform:canvas:box:)`).
    pub fn mask(
        image: &PixelImage,
        transform: &LayerTransform,
        canvas: Size,
        box_: f64,
    ) -> Rgba8Image {
        let (mut target, scale) = Self::render(canvas, box_);
        let width = target.width() as f64;
        let height = target.height() as f64;
        target.set_fill_gray(image.as_gray().map(LayerMask::background).unwrap_or(1.0));
        target.fill_rect(Rect::new(0.0, 0.0, width, height));
        Self::place(image, transform, scale, &mut target);
        target.into_rgba()
    }

    /// A canvas-shaped target: the picture's size in points, its bitmap, and pixels per document
    /// pixel (`render(canvas:box:draw:)`).
    fn render(canvas: Size, box_: f64) -> (Canvas, f64) {
        let points = Self::fitted_size(canvas, box_);
        let width = (points.width * Self::BACKING_SCALE) as usize;
        let height = (points.height * Self::BACKING_SCALE) as usize;
        let scale = if canvas.width > 0.0 {
            width as f64 / canvas.width
        } else {
            1.0
        };
        (Canvas::new_rgba(width, height), scale)
    }

    /// Draws `image` where `transform` puts it on the canvas, the way the canvas itself places
    /// layers (`CanvasThumbnail.place(_:transform:scale:in:)`).
    fn place(image: &PixelImage, transform: &LayerTransform, scale: f64, canvas: &mut Canvas) {
        LayerRenderer::draw(
            image,
            transform,
            Point::new(transform.center().x * scale, transform.center().y * scale),
            scale,
            1.0,
            compositor_rs_core::blend::LayerBlendMode::Normal,
            None,
            canvas,
        );
    }
}

/// The picture's pixels are shared like the canvas's, so a caller can keep one.
pub type SharedThumbnail = Arc<Rgba8Image>;

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::Gray8Image;

    /// Ported from `CompositorTests.CanvasThumbnailTests.thumbnailsTakeTheCanvasShape`.
    #[test]
    fn thumbnails_take_the_canvas_shape() {
        assert_eq!(
            CanvasThumbnail::fitted_size(Size::new(400.0, 200.0), 36.0),
            Size::new(36.0, 18.0)
        );
        assert_eq!(
            CanvasThumbnail::fitted_size(Size::new(300.0, 600.0), 36.0),
            Size::new(18.0, 36.0)
        );
        assert_eq!(
            CanvasThumbnail::fitted_size(Size::ZERO, 30.0),
            Size::new(30.0, 30.0)
        );
    }

    /// Ported from `layerPixelsSitWhereTheyAreOnTheCanvas`: the layer sits in the canvas's top-left
    /// quarter, not flipped, and an empty layer is all checkerboard.
    #[test]
    fn layer_pixels_sit_where_they_are_on_the_canvas() {
        let red = Rgba8Image::opaque(100, 100, [255, 0, 0, 255]);
        let image = PixelImage::Rgba(Arc::new(red));
        let transform = LayerTransform { origin: Point::ZERO, size: Size::new(100.0, 100.0), ..LayerTransform::default() };
        let thumbnail = CanvasThumbnail::layer(
            Some(&image),
            &transform,
            Size::new(400.0, 200.0),
            36.0,
        );
        assert_eq!((thumbnail.width(), thumbnail.height()), (72, 36));
        assert_eq!(thumbnail.get(4, 4), [255, 0, 0, 255]);
        assert_eq!(thumbnail.get(12, 12)[0], 255);
        assert!(thumbnail.get(60, 30)[0] < 200, "the rest shows the checkerboard");
        assert!(thumbnail.get(4, 30)[0] < 200, "not flipped: the bottom-left is empty");
        let blank = CanvasThumbnail::layer(
            None,
            &LayerTransform { origin: Point::ZERO, size: Size::new(400.0, 200.0), ..LayerTransform::default() },
            Size::new(400.0, 200.0),
            36.0,
        );
        assert_eq!(blank.get(4, 4)[3], 255, "an empty layer is all checkerboard");
    }

    /// Ported from `masksFillTheCanvasWithTheirEdgeTone`: the edge tone carries on past the layer,
    /// and a stroke reaching the mask's edge doesn't turn the background gray.
    #[test]
    fn masks_fill_the_canvas_with_their_edge_tone() {
        let transform = LayerTransform { origin: Point::new(100.0, 50.0), size: Size::new(100.0, 100.0), ..LayerTransform::default() };
        let canvas = Size::new(400.0, 200.0);
        let hide_all = PixelImage::Gray(Arc::new(Gray8Image::uniform(96, 96, 0)));
        let hidden = CanvasThumbnail::mask(&hide_all, &transform, canvas, 30.0);
        assert!(hidden.get(1, 1)[0] < 10, "a hide-all mask reads all black");
        assert!(hidden.get(55, 25)[0] < 10);

        // White edges round a black middle: white around the layer, black where the middle sits.
        let mut framed = Gray8Image::uniform(20, 20, 255);
        for y in 5..15 {
            for x in 5..15 {
                framed.set(x, y, 0);
            }
        }
        assert_eq!(LayerMask::background(&framed), 1.0);
        let framed = PixelImage::Gray(Arc::new(framed));
        let shown = CanvasThumbnail::mask(&framed, &transform, canvas, 30.0);
        assert!(shown.get(2, 2)[0] > 245, "outside the layer the edge tone carries on");
        assert!(shown.get(22, 15)[0] < 10, "the black middle sits where the layer is");

        // A stroke reaching the mask's edge: the rest still reads white, as the canvas treats it.
        let mut stroked = Gray8Image::uniform(20, 20, 255);
        for y in 8..12 {
            for x in 0..20 {
                stroked.set(x, y, 0);
            }
        }
        let stroked = PixelImage::Gray(Arc::new(stroked));
        let beyond = CanvasThumbnail::mask(&stroked, &transform, canvas, 30.0);
        assert!(beyond.get(2, 2)[0] > 245, "the background is white or black, never a gray average");
    }
}
