//! `CanvasViewport`: where the document sits in the view and how big it is drawn.
//!
//! Document: pixels with top-left origin. View: points. Zoom 1 means actual display pixels.

use crate::geom::{CGFloat, Point, Rect, Size};

/// The zoom limits (`CanvasViewport.zoomRange`).
pub const MIN_ZOOM: CGFloat = 0.001;
pub const MAX_ZOOM: CGFloat = 32.0;

/// `CanvasViewport.keyboardZoomLevels`: the stable stops the keyboard steps through.
pub const KEYBOARD_ZOOM_LEVELS: [CGFloat; 17] = [
    0.125,
    1.0 / 6.0,
    0.25,
    1.0 / 3.0,
    0.5,
    2.0 / 3.0,
    1.0,
    1.25,
    1.5,
    2.0,
    3.0,
    4.0,
    5.0,
    6.0,
    8.0,
    12.0,
    16.0,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasViewport {
    /// The view's size in points.
    pub view_size: Size,
    /// The display's backing scale (2 on a Retina display).
    pub backing_scale: CGFloat,
    zoom: CGFloat,
    /// How far the document has been dragged from the view's center, in points.
    pub pan: Size,
    follows_fit: bool,
}

impl Default for CanvasViewport {
    fn default() -> Self {
        Self {
            view_size: Size::ZERO,
            backing_scale: 1.0,
            zoom: 1.0,
            pan: Size::ZERO,
            follows_fit: true,
        }
    }
}

impl CanvasViewport {
    pub fn zoom(&self) -> CGFloat {
        self.zoom
    }

    pub fn follows_fit(&self) -> bool {
        self.follows_fit
    }

    /// Points drawn per document pixel.
    pub fn points_per_pixel(&self) -> CGFloat {
        self.zoom / self.backing_scale
    }

    pub fn center(&self) -> Point {
        Point::new(self.view_size.width / 2.0, self.view_size.height / 2.0)
    }

    /// Where the document sits in the view, for a document of `size` pixels.
    pub fn document_rect(&self, size: Size) -> Rect {
        let scaled = Size::new(size.width * self.points_per_pixel(), size.height * self.points_per_pixel());
        Rect::new(
            self.center().x - scaled.width / 2.0 + self.pan.width,
            self.center().y - scaled.height / 2.0 + self.pan.height,
            scaled.width,
            scaled.height,
        )
    }

    pub fn document_point(&self, point: Point, document_size: Size) -> Point {
        let origin = self.document_rect(document_size).origin;
        Point::new(
            (point.x - origin.x) / self.points_per_pixel(),
            (point.y - origin.y) / self.points_per_pixel(),
        )
    }

    pub fn view_point(&self, point: Point, document_size: Size) -> Point {
        let origin = self.document_rect(document_size).origin;
        Point::new(
            origin.x + point.x * self.points_per_pixel(),
            origin.y + point.y * self.points_per_pixel(),
        )
    }

    /// Fits the document in the view with a 48-point margin all round, and follows it from now on.
    pub fn fit(&mut self, document_size: Size) {
        if !(self.view_size.width > 0.0 && self.view_size.height > 0.0) {
            self.follows_fit = true;
            return;
        }
        self.zoom = self.clamp(
            ((self.view_size.width - 96.0).max(1.0) / document_size.width)
                .min((self.view_size.height - 96.0).max(1.0) / document_size.height)
                * self.backing_scale,
        );
        self.pan = Size::ZERO;
        self.follows_fit = true;
    }

    pub fn resize(&mut self, size: Size, backing_scale: CGFloat, document_size: Option<Size>) {
        // Preserve the center document point when moving between displays.
        let old_scale = self.points_per_pixel();
        self.view_size = size;
        self.backing_scale = backing_scale.max(1.0);
        match (self.follows_fit, document_size) {
            (true, Some(document_size)) => self.fit(document_size),
            _ => {
                let ratio = self.points_per_pixel() / old_scale;
                self.pan = Size::new(self.pan.width * ratio, self.pan.height * ratio);
            }
        }
    }

    /// Zooms to `value`, keeping the document point under `anchor` under it.
    pub fn set_zoom(&mut self, value: CGFloat, anchored_at: Point, document_size: Size) {
        if !value.is_finite() {
            return;
        }
        let anchor = anchored_at;
        let pixel = self.document_point(anchor, document_size);
        self.zoom = self.clamp(value);
        let moved = self.view_point(pixel, document_size);
        self.pan.width += anchor.x - moved.x;
        self.pan.height += anchor.y - moved.y;
        self.follows_fit = false;
    }

    /// The next stable zoom stop in the given direction, or the current zoom at the ends.
    pub fn keyboard_zoom_target(&self, step: i32) -> CGFloat {
        if step == 0 {
            return self.zoom;
        }
        let tolerance = 0.000000001_f64.max(self.zoom.abs() * 0.000000001);
        if step > 0 {
            return KEYBOARD_ZOOM_LEVELS
                .iter()
                .copied()
                .find(|level| *level > self.zoom + tolerance)
                .unwrap_or(self.zoom);
        }
        KEYBOARD_ZOOM_LEVELS
            .iter()
            .copied()
            .rev()
            .find(|level| *level < self.zoom - tolerance)
            .unwrap_or(self.zoom)
    }

    /// Drags the document by `delta` points.
    pub fn translate(&mut self, delta: Size) {
        self.pan.width += delta.width;
        self.pan.height += delta.height;
        self.follows_fit = false;
    }

    fn clamp(&self, value: CGFloat) -> CGFloat {
        MAX_ZOOM.min(MIN_ZOOM.max(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Size {
        Size::new(1920.0, 1080.0)
    }

    fn assert_close(a: CGFloat, b: CGFloat) {
        assert!((a - b).abs() < 0.000001, "{a} != {b}");
    }

    #[test]
    fn actual_pixels_and_round_trip() {
        for backing in [1.0, 2.0] {
            let mut viewport = CanvasViewport::default();
            viewport.resize(Size::new(800.0, 600.0), backing, None);
            for zoom in [0.25, 1.0, 3.75] {
                viewport.set_zoom(zoom, viewport.center(), document());
                viewport.translate(Size::new(73.5, -44.25));
                let pixel = Point::new(183.25, 837.5);
                let view_point = viewport.view_point(pixel, document());
                let result = viewport.document_point(view_point, document());
                assert_close(result.x, pixel.x);
                assert_close(result.y, pixel.y);
                assert_close(viewport.document_rect(document()).width() * backing, document().width * zoom);
            }
        }
    }

    #[test]
    fn zoom_keeps_cursor_pixel_fixed() {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(1000.0, 700.0), 2.0, Some(document()));
        let anchor = Point::new(157.0, 221.0);
        let before = viewport.document_point(anchor, document());
        viewport.set_zoom(4.0, anchor, document());
        let after = viewport.document_point(anchor, document());
        assert_close(before.x, after.x);
        assert_close(before.y, after.y);
    }

    #[test]
    fn fit_and_resize_modes() {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(800.0, 600.0), 2.0, Some(document()));
        let rect = viewport.document_rect(document());
        assert!(rect.width() <= 704.000001);
        assert!(rect.height() <= 504.000001);
        assert_eq!(rect.mid_x(), 400.0);
        assert_eq!(rect.mid_y(), 300.0);
        viewport.translate(Size::new(60.0, -35.0));
        let before = viewport.document_point(viewport.center(), document());
        let zoom = viewport.zoom();
        viewport.resize(Size::new(1200.0, 800.0), 1.0, Some(document()));
        let after = viewport.document_point(viewport.center(), document());
        assert_eq!(viewport.zoom(), zoom);
        assert_close(before.x, after.x);
        assert_close(before.y, after.y);
        viewport.fit(document());
        assert_eq!(viewport.pan, Size::ZERO);
        assert!(viewport.follows_fit());
    }

    #[test]
    fn keyboard_zoom_uses_stable_stops_and_clamps_at_the_ends() {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(1000.0, 800.0), 1.0, None);
        viewport.set_zoom(0.5, Point::ZERO, document());

        viewport.set_zoom(viewport.keyboard_zoom_target(1), viewport.center(), document());
        assert_close(viewport.zoom(), 2.0 / 3.0);
        viewport.set_zoom(viewport.keyboard_zoom_target(1), viewport.center(), document());
        assert_eq!(viewport.zoom(), 1.0);
        viewport.set_zoom(viewport.keyboard_zoom_target(1), viewport.center(), document());
        assert_eq!(viewport.zoom(), 1.25);

        viewport.set_zoom(KEYBOARD_ZOOM_LEVELS[0], viewport.center(), document());
        assert_eq!(viewport.keyboard_zoom_target(-1), KEYBOARD_ZOOM_LEVELS[0]);
        viewport.set_zoom(*KEYBOARD_ZOOM_LEVELS.last().unwrap(), viewport.center(), document());
        assert_eq!(viewport.keyboard_zoom_target(1), *KEYBOARD_ZOOM_LEVELS.last().unwrap());
    }

    #[test]
    fn keyboard_zoom_round_trip_does_not_drift_after_ten_steps() {
        let mut viewport = CanvasViewport::default();
        let canvas = Size::new(4000.0, 3000.0);
        viewport.resize(Size::new(1200.0, 900.0), 2.0, None);
        viewport.set_zoom(0.5, viewport.center(), canvas);
        viewport.translate(Size::new(-430.0, 275.0));

        let center = viewport.center();
        let before = viewport.document_point(center, canvas);
        for _ in 0..10 {
            let target = viewport.keyboard_zoom_target(1);
            viewport.set_zoom(target, center, canvas);
        }
        for _ in 0..10 {
            let target = viewport.keyboard_zoom_target(-1);
            viewport.set_zoom(target, center, canvas);
        }
        let after = viewport.document_point(center, canvas);

        assert_eq!(viewport.zoom(), 0.5);
        assert_close(before.x, after.x);
        assert_close(before.y, after.y);
    }

    #[test]
    fn zoom_clamps_to_the_range() {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(800.0, 600.0), 2.0, None);
        viewport.set_zoom(1000.0, Point::ZERO, document());
        assert_eq!(viewport.zoom(), MAX_ZOOM);
        viewport.set_zoom(0.0, Point::ZERO, document());
        assert_eq!(viewport.zoom(), MIN_ZOOM);
        viewport.set_zoom(CGFloat::NAN, Point::ZERO, document());
        assert_eq!(viewport.zoom(), MIN_ZOOM);
    }
}
