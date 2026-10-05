//! Zoom and pan: the keyboard zoom stops, Fit, Actual Pixels, cursor-anchored zooming, the pixel
//! grid's visibility rule, and the routing that sends the View menu's zoom commands to an open
//! dialog's own preview instead of the canvas.
//!
//! Ported from `EditorSession.zoom(to:anchor:)`, `zoomKeyboard(by:)`, `fit()` in
//! `Document/EditorSession.swift`, the View menu's zoom items in `CompositorApp.swift`, the zoom
//! tool's use of the viewport in `Rendering/EditorCanvas.swift`, and `PreviewZoomCommand`.

use compositor_core::geom::{Point, Size};
use compositor_core::viewport::CanvasViewport;

use crate::session::EditorSession;

/// The zoom at which the pixel grid appears (`EditorCanvas.pixelGridZoom`), 800%.
pub const PIXEL_GRID_ZOOM: f64 = 8.0;

/// The View menu's zoom commands, which a dialog with its own zoomable preview (Export JPEG)
/// receives instead of the canvas (`EditorSession.PreviewZoomCommand`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PreviewZoomCommand {
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
}

/// Whether the pixel grid is drawn: the switch on, and the view zoomed to 800% or beyond.
pub fn pixel_grid_shows(switch: bool, zoom: f64) -> bool {
    switch && zoom >= PIXEL_GRID_ZOOM
}

/// One keyboard zoom step: the viewport moved to the next stable stop in `step`'s direction, about
/// its center (`zoomKeyboard(by:)`'s body, kept pure for the tests).
pub fn keyboard_zoomed(viewport: &mut CanvasViewport, step: i32, document_size: Size) {
    if step == 0 {
        return;
    }
    let target = viewport.keyboard_zoom_target(step);
    if target == viewport.zoom() {
        return;
    }
    viewport.set_zoom(target, viewport.center(), document_size);
}

impl EditorSession {
    /// Fits the document in the view (`session.fit()`).
    pub fn fit(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let size = document.size();
        self.viewport.fit(size);
    }

    /// Zooms to `value`, keeping the document point under `anchor` (the view's center when there is
    /// none) under it (`zoom(to:anchor:)`).
    pub fn zoom(&mut self, value: f64, anchor: Option<Point>) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let size = document.size();
        let anchor = anchor.unwrap_or_else(|| self.viewport.center());
        self.viewport.set_zoom(value, anchor, size);
    }

    /// Steps through the stable keyboard zoom levels (12.5%, ⅙, 25%, ⅓, 50%, ⅔, 100%, 125%, 150%,
    /// 200%, 300%, 400%, 500%, 600%, 800%, 1200%, 1600%) while keeping the viewport center fixed
    /// (`zoomKeyboard(by:)`).
    pub fn zoom_keyboard(&mut self, step: i32) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let size = document.size();
        keyboard_zoomed(&mut self.viewport, step, size);
    }

    /// Drags the document by `delta` points (`CanvasViewport.translate(by:)`, the Hand tool and
    /// space-drag).
    pub fn pan(&mut self, delta: Size) {
        self.viewport.translate(delta);
    }

    /// Whether the pixel grid is drawn now: the View menu switch on, and the view at 800% or more.
    pub fn pixel_grid_visible(&self) -> bool {
        pixel_grid_shows(self.shows_pixel_grid, self.viewport.zoom())
    }

    /// Routes a View menu zoom command: an open dialog's preview takes it; otherwise it zooms the
    /// canvas (`CompositorApp`'s `previewZoom?(command) ?? canvas` branches, in one place).
    pub fn route_zoom_command(&mut self, command: PreviewZoomCommand) {
        if let Some(preview) = self.preview_zoom.as_ref() {
            preview(command);
            return;
        }
        match command {
            PreviewZoomCommand::ZoomIn => self.zoom_keyboard(1),
            PreviewZoomCommand::ZoomOut => self.zoom_keyboard(-1),
            PreviewZoomCommand::Fit => self.fit(),
            PreviewZoomCommand::Actual => self.zoom(1.0, None),
        }
    }

    /// Fit Canvas (⌘0).
    pub fn fit_canvas(&mut self) {
        self.route_zoom_command(PreviewZoomCommand::Fit);
    }

    /// Actual Pixels (⌘1).
    pub fn actual_pixels(&mut self) {
        self.route_zoom_command(PreviewZoomCommand::Actual);
    }

    /// Zoom In (⌘=).
    pub fn zoom_in(&mut self) {
        self.route_zoom_command(PreviewZoomCommand::ZoomIn);
    }

    /// Zoom Out (⌘-).
    pub fn zoom_out(&mut self) {
        self.route_zoom_command(PreviewZoomCommand::ZoomOut);
    }

    /// The document point at the view's center, where an untouched drag or a paste lands.
    pub fn viewport_center_point(&self) -> Option<Point> {
        let document = self.document.as_ref()?;
        Some(self.viewport.document_point(self.viewport.center(), document.size()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> CanvasViewport {
        let mut viewport = CanvasViewport::default();
        viewport.resize(Size::new(1000.0, 800.0), 1.0, None);
        viewport
    }

    #[test]
    fn pixel_grid_appears_from_eight_times_zoom() {
        assert!(pixel_grid_shows(true, 8.0));
        assert!(pixel_grid_shows(true, 12.0));
        assert!(!pixel_grid_shows(true, 7.999));
        assert!(!pixel_grid_shows(false, 16.0));
    }

    #[test]
    fn keyboard_steps_walk_the_stable_stops_and_stop_at_the_ends() {
        let canvas = Size::new(4000.0, 3000.0);
        let mut viewport = viewport();
        viewport.set_zoom(0.5, viewport.center(), canvas);
        keyboard_zoomed(&mut viewport, 1, canvas);
        assert_eq!(viewport.zoom(), 2.0 / 3.0);
        keyboard_zoomed(&mut viewport, 1, canvas);
        assert_eq!(viewport.zoom(), 1.0);
        keyboard_zoomed(&mut viewport, -1, canvas);
        assert_eq!(viewport.zoom(), 2.0 / 3.0);
        // A step of zero changes nothing.
        keyboard_zoomed(&mut viewport, 0, canvas);
        assert_eq!(viewport.zoom(), 2.0 / 3.0);
        // Past the top stop the zoom stays there.
        viewport.set_zoom(16.0, viewport.center(), canvas);
        keyboard_zoomed(&mut viewport, 1, canvas);
        assert_eq!(viewport.zoom(), 16.0);
        // Below the bottom stop too.
        viewport.set_zoom(0.125, viewport.center(), canvas);
        keyboard_zoomed(&mut viewport, -1, canvas);
        assert_eq!(viewport.zoom(), 0.125);
    }

    #[test]
    fn keyboard_zoom_keeps_the_viewport_center() {
        let canvas = Size::new(4000.0, 3000.0);
        let mut viewport = viewport();
        viewport.set_zoom(0.5, viewport.center(), canvas);
        viewport.translate(Size::new(-120.0, 40.0));
        let before = viewport.document_point(viewport.center(), canvas);
        keyboard_zoomed(&mut viewport, 1, canvas);
        let after = viewport.document_point(viewport.center(), canvas);
        assert!((before.x - after.x).abs() < 0.000001);
        assert!((before.y - after.y).abs() < 0.000001);
    }

    #[test]
    fn zoom_commands_are_the_four_the_menu_routes() {
        // The menu builds these four; the dialog's preview receives the same values.
        let commands = [
            PreviewZoomCommand::ZoomIn,
            PreviewZoomCommand::ZoomOut,
            PreviewZoomCommand::Fit,
            PreviewZoomCommand::Actual,
        ];
        assert_eq!(commands.len(), 4);
        assert_ne!(PreviewZoomCommand::Fit, PreviewZoomCommand::Actual);
    }
}
