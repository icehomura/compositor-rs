//! The Gradient tool: `Document/Gradient.swift` — the pending gradient's lifecycle (drawn, resolved,
//! cancelled) and the two-color ramp it fills with.
//!
//! **Async → sync (docs/PORTING.md §4).** Swift's `commitGradient()` awaited `commitRasterEdit`, and
//! `resolveGradient()` started it from a detached `Task` so that switching tools, layers or targets
//! applied the pending gradient without blocking. The port runs the commit inline: the raster edit is
//! synchronous, so `resolve_gradient` is just `commit_gradient`, and the same undo entries and error
//! strings result.

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::error::CoreError;
use compositor_rs_core::geom::Point;
use compositor_rs_core::image_ops::{GradientSettings, GradientShape, GradientStyle};
use compositor_rs_core::color::PaletteColor;
use compositor_rs_pixels::brush::{BrushSettings, BrushStroke, GradientShape as PixelGradientShape};

use crate::brush::brush_failure;
use crate::session::EditorSession;

/// The pixel engine's shape enum for a gradient: `GradientShape` is the session/model vocabulary
/// (`compositor_rs_core::image_ops`), and the brush kernel takes its own copy of the same two cases.
fn pixel_shape(shape: GradientShape) -> PixelGradientShape {
    match shape {
        GradientShape::Linear => PixelGradientShape::Linear,
        GradientShape::Radial => PixelGradientShape::Radial,
    }
}

/// What the gradient is filled with, as it's dragged and set (`GradientEdit.Fill`).
///
/// `colors` are straight (non-premultiplied) sRGB components plus alpha, the form
/// `BrushStroke::fill_gradient` and `Canvas`'s gradient paint take.
#[derive(Clone, Debug, PartialEq)]
pub struct GradientFill {
    pub shape: GradientShape,
    pub start: Point,
    pub end: Point,
    pub colors: Vec<[f64; 4]>,
    pub opacity: f64,
}

/// An uncommitted gradient on one layer or mask. Endpoints are document pixels; the raster preview
/// lives in `raster` and never touches the document until commit.
pub struct GradientEdit {
    pub raster: BrushStroke,
    pub start: Point,
    pub end: Point,
    /// What the gradient is filled with, as it's dragged and set.
    pub fill: Option<GradientFill>,
    /// The fill the raster's tiles hold. They're filled only when something reads them — the canvas,
    /// or the commit — as the GPU canvas draws the gradient itself.
    filled: Option<GradientFill>,
    /// Where the fill's second color comes from for a mask: the mask paints in gray, so only the
    /// foreground's red channel and the alpha survive.
    mask: bool,
}

impl GradientEdit {
    /// `GradientEdit(raster:start:)`: the end starts on the start until the drag moves it.
    pub fn new(raster: BrushStroke, start: Point) -> Self {
        let mask = raster.is_mask;
        Self {
            raster,
            start,
            end: start,
            fill: None,
            filled: None,
            mask,
        }
    }

    pub fn has_line(&self) -> bool {
        (self.end.x - self.start.x).hypot(self.end.y - self.start.y) >= 0.5
    }

    /// Whether this edit draws into the layer's mask rather than its pixels.
    pub fn is_mask(&self) -> bool {
        self.mask
    }

    /// Fills the raster's tiles with the pending fill, if one changed since they were last filled.
    pub fn apply_fill(&mut self) -> Result<(), CoreError> {
        let Some(fill) = self.fill.clone() else { return Ok(()) };
        if self.filled.as_ref() == Some(&fill) {
            return Ok(());
        }
        self.raster
            .fill_gradient(pixel_shape(fill.shape), fill.start, fill.end, &fill.colors, fill.opacity)?;
        self.filled = Some(fill);
        Ok(())
    }
}

impl EditorSession {
    pub fn begin_gradient(&mut self, point: Point) {
        if self.tool != NavigationTool::Gradient || !(self.can_paint() || self.gradient_edit.is_some()) {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else { return };
        // Dragging a new line replaces the pending one on the same target.
        let same_target = self
            .gradient_edit
            .as_ref()
            .map(|edit| edit.raster.layer.id == layer.id && edit.raster.is_mask == self.is_mask_selected)
            .unwrap_or(false);
        if same_target {
            let edit = self.gradient_edit.as_mut().expect("checked above");
            edit.start = point;
            edit.end = point;
            self.refresh_gradient();
            return;
        }
        if !self.can_paint() {
            self.brush_error = self.paint_refusal();
            return;
        }
        self.finish_opacity_edit();
        match self.make_raster_edit(&layer, BrushSettings::default(), true) {
            Ok(raster) => {
                self.gradient_edit = Some(GradientEdit::new(raster, point));
                self.brush_revision += 1;
            }
            Err(error) => self.brush_error = Some(brush_failure(&error)),
        }
    }

    pub fn move_gradient(&mut self, start: Option<Point>, end: Option<Point>) {
        let Some(edit) = self.gradient_edit.as_mut() else { return };
        if let Some(start) = start {
            edit.start = start;
        }
        if let Some(end) = end {
            edit.end = end;
        }
        self.refresh_gradient();
    }

    /// Re-renders the pending gradient from the current endpoints, settings, and palette.
    pub fn refresh_gradient(&mut self) {
        let Some(edit) = self.gradient_edit.as_ref() else { return };
        let has_line = edit.has_line();
        let mask = edit.is_mask();
        let (start, end) = (edit.start, edit.end);
        if has_line {
            let fill = GradientFill {
                shape: self.gradient_settings.shape,
                start,
                end,
                colors: self.gradient_colors(mask),
                opacity: self.gradient_settings.opacity,
            };
            self.gradient_edit.as_mut().expect("checked above").fill = Some(fill);
        }
        self.brush_revision += 1;
    }

    /// The ramp's two colors, straight sRGB + alpha. A mask's ramp is gray: the foreground's red
    /// channel carries the value, as the device-gray color space did.
    pub fn gradient_colors(&self, mask: bool) -> Vec<[f64; 4]> {
        fn color(value: PaletteColor, alpha: f64, mask: bool) -> [f64; 4] {
            if mask {
                [value.red, value.red, value.red, alpha]
            } else {
                [value.red, value.green, value.blue, alpha]
            }
        }
        let foreground = self.palette_color(false);
        let colors = match self.gradient_settings.style {
            GradientStyle::ForegroundToBackground => vec![
                color(foreground, 1.0, mask),
                color(self.palette_color(true), 1.0, mask),
            ],
            GradientStyle::ForegroundToTransparent => vec![color(foreground, 1.0, mask), color(foreground, 0.0, mask)],
        };
        if self.gradient_settings.reversed {
            colors.into_iter().rev().collect()
        } else {
            colors
        }
    }

    /// Ends a drag; a click without a line leaves nothing pending.
    pub fn end_gradient_drag(&mut self) {
        if self.gradient_edit.as_ref().map(|edit| !edit.has_line()).unwrap_or(false) {
            self.cancel_gradient();
        }
    }

    pub fn cancel_gradient(&mut self) {
        if self.gradient_edit.is_none() {
            return;
        }
        self.gradient_edit = None;
        self.brush_revision += 1;
    }

    /// `undo()`'s first rule: like Photoshop, the first Undo discards a pending gradient. True when
    /// it did — and the undo was consumed for it.
    pub fn discard_pending_gradient(&mut self) -> bool {
        if self.gradient_edit.is_none() {
            return false;
        }
        self.cancel_gradient();
        true
    }

    /// Swift's `async func commitGradient()`; synchronous here (docs/PORTING.md §4).
    pub fn commit_gradient(&mut self) {
        if self.is_project_busy {
            return;
        }
        let Some(mut edit) = self.gradient_edit.take() else { return };
        if !edit.has_line() {
            self.gradient_edit = Some(edit);
            self.cancel_gradient();
            return;
        }
        let name = if edit.is_mask() { "Gradient Mask" } else { "Gradient" }.to_string();
        let outcome = edit
            .apply_fill()
            .and_then(|()| self.commit_raster_edit(&edit.raster, &name, None));
        if let Err(error) = outcome {
            self.brush_error = Some(brush_failure(&error));
        }
        // Swift's `if gradientEdit === edit { cancelGradient() }`: nothing else can have started a
        // gradient while the commit ran, so the taken edit is the one to close out.
        if self.gradient_edit.is_none() {
            self.gradient_edit = Some(edit);
            self.cancel_gradient();
        }
    }

    /// Switching tools, layers, or targets applies the pending gradient, as in Photoshop.
    /// Swift ran this from a detached `Task`; the commit is synchronous here.
    pub fn resolve_gradient(&mut self) {
        if self.gradient_edit.is_none() {
            return;
        }
        self.commit_gradient();
    }

    /// The `didSet` on `gradientSettings`.
    pub fn set_gradient_settings(&mut self, settings: GradientSettings) {
        self.gradient_settings = settings;
        self.refresh_gradient();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::document::{CanvasDocument, ImageLayer};
    use compositor_rs_core::geom::Size;

    /// A session with one 64-pixel layer selected and a pending gradient on it.
    fn session_with_gradient() -> EditorSession {
        let mut session = EditorSession::default();
        let canvas = Size::new(64.0, 64.0);
        let layer = ImageLayer::blank("Layer 1", canvas);
        let id = layer.id;
        let mut document = CanvasDocument::new(64, 64);
        document.layers = vec![layer.clone()];
        session.document = Some(document);
        session.active_layer_id = Some(id);
        session.selected_layer_ids = [id].into_iter().collect();
        let raster = BrushStroke::new(&layer, false, BrushSettings::default(), canvas, true)
            .expect("a default stroke fits a 64-pixel canvas");
        session.gradient_edit = Some(GradientEdit::new(raster, Point::new(4.0, 4.0)));
        session
    }

    #[test]
    fn gradient_defaults_match_the_options_bar() {
        // The vocabulary itself (`GradientShape`, `GradientStyle`, their raw values) is
        // `compositor_rs_core::image_ops`, covered by its own tests.
        let settings = GradientSettings::default();
        assert_eq!(settings.shape, GradientShape::Linear);
        assert_eq!(settings.style, GradientStyle::ForegroundToTransparent);
        assert!(!settings.reversed);
        assert_eq!(settings.opacity, 1.0);
    }

    /// A line under half a document pixel long is not a line: a click leaves nothing pending.
    #[test]
    fn a_click_without_a_line_cancels_the_gradient() {
        let mut session = session_with_gradient();
        assert!(!session.gradient_edit.as_ref().expect("pending").has_line());
        let revision = session.brush_revision;
        session.end_gradient_drag();
        assert!(session.gradient_edit.is_none());
        assert_eq!(session.brush_revision, revision + 1);
    }

    /// `undo()`'s first rule: the first Undo discards a pending gradient instead of popping history.
    #[test]
    fn undo_discards_a_pending_gradient_first() {
        let mut session = session_with_gradient();
        let revision = session.brush_revision;
        assert!(session.discard_pending_gradient());
        assert!(session.gradient_edit.is_none());
        assert_eq!(session.brush_revision, revision + 1);
        // The second Undo is history's to consume.
        assert!(!session.discard_pending_gradient());
    }

    /// The ramp runs foreground to background, or foreground to transparent, reversed on request;
    /// a mask's ramp is gray.
    #[test]
    fn gradient_colors_follow_the_style_and_reversal() {
        let mut session = EditorSession::default();
        session.brush_settings.red = 1.0;
        session.brush_settings.green = 0.0;
        session.brush_settings.blue = 0.0;
        session.background_color = PaletteColor::new(0.0, 0.0, 1.0);

        let colors = session.gradient_colors(false);
        assert_eq!(colors, vec![[1.0, 0.0, 0.0, 1.0], [1.0, 0.0, 0.0, 0.0]]);

        session.gradient_settings.style = GradientStyle::ForegroundToBackground;
        assert_eq!(
            session.gradient_colors(false),
            vec![[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]]
        );

        session.gradient_settings.reversed = true;
        assert_eq!(
            session.gradient_colors(false),
            vec![[0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0]]
        );

        // On a mask the value lives in the red channel and the ramp stays gray.
        assert_eq!(session.gradient_colors(true), vec![[0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]]);
    }

    /// Moving the line refills the pending gradient and bumps the revision the canvas watches.
    #[test]
    fn moving_a_gradient_updates_its_fill() {
        let mut session = session_with_gradient();
        let revision = session.brush_revision;
        session.move_gradient(Some(Point::new(4.0, 4.0)), Some(Point::new(40.0, 4.0)));
        let edit = session.gradient_edit.as_ref().expect("pending");
        assert!(edit.has_line());
        let fill = edit.fill.as_ref().expect("a line makes a fill");
        assert_eq!(fill.start, Point::new(4.0, 4.0));
        assert_eq!(fill.end, Point::new(40.0, 4.0));
        assert_eq!(fill.opacity, 1.0);
        assert!(session.brush_revision > revision);
    }
}
