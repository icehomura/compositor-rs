//! Guides, the layout grid, the snapping switches and the ruler arithmetic.
//!
//! Ported from the `extension EditorSession` in `Document/Guides.swift` (the `CanvasGuide`,
//! `LayoutGrid`, `GridAppearance` and `GuideDrag` value types themselves live in
//! `compositor_core::guides`), plus `CanvasRuler`, the ruler's tick arithmetic.

use std::collections::HashSet;

use compositor_core::color::PaletteColor;
use compositor_core::geom::{CGFloat, Point, Size};
use compositor_core::guides::{
    CanvasGuide, CanvasGuideAxis, GridAppearance, GridAppearancePreset, GridAppearanceStyle,
    GuideDrag, LayoutGrid,
};
use compositor_core::layer_transform::TransformSnap;
use compositor_core::settings;
use compositor_core::Id;

use crate::session::EditorSession;
use compositor_pixels::warp::DistortWarp;

/// Cyan, as Photoshop's default guide color (`EditorSession.guideColor`).
pub fn guide_color() -> PaletteColor {
    PaletteColor::new(0.0, 1.0, 1.0)
}

/// The default guide color's alpha (`CGColor(srgbRed:green:blue:alpha: 0.9)`).
pub const GUIDE_ALPHA: CGFloat = 0.9;

/// How close, in view points, the pointer must come to a guide to grab it.
pub const GUIDE_HIT_DISTANCE: CGFloat = 5.0;

/// One of the View menu's sticky switches, with the `ToolDefaults` key and default upstream stores
/// it under. The switches belong to the person rather than to a document, so they survive tabs and
/// launches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GuideSwitch {
    /// View > Show > Guides.
    Guides,
    /// View > Rulers.
    Rulers,
    /// View > Show > Grid.
    Grid,
    /// View > Pixel Grid (800% and above).
    PixelGrid,
    /// View > Snap (master switch).
    Snap,
    /// View > Snap To > Guides.
    SnapToGuides,
    /// View > Snap To > Grid.
    SnapToGrid,
    /// View > Snap To > Layers.
    SnapToLayers,
    /// View > Snap To > Document Bounds.
    SnapToDocumentBounds,
    /// View > Lock Guides.
    LockGuides,
}

impl GuideSwitch {
    /// `CaseIterable` order, as the menu lists them.
    pub const ALL: [GuideSwitch; 10] = [
        GuideSwitch::Guides,
        GuideSwitch::Rulers,
        GuideSwitch::Grid,
        GuideSwitch::PixelGrid,
        GuideSwitch::Snap,
        GuideSwitch::SnapToGuides,
        GuideSwitch::SnapToGrid,
        GuideSwitch::SnapToLayers,
        GuideSwitch::SnapToDocumentBounds,
        GuideSwitch::LockGuides,
    ];

    /// The `ToolDefaults` key, verbatim from upstream.
    pub fn key(self) -> &'static str {
        match self {
            GuideSwitch::Guides => "guides",
            GuideSwitch::Rulers => "rulers",
            GuideSwitch::Grid => "grid",
            GuideSwitch::PixelGrid => "pixelGrid",
            GuideSwitch::Snap => "snap",
            GuideSwitch::SnapToGuides => "snapGuides",
            GuideSwitch::SnapToGrid => "snapGrid",
            GuideSwitch::SnapToLayers => "snapLayers",
            GuideSwitch::SnapToDocumentBounds => "snapBounds",
            GuideSwitch::LockGuides => "lockGuides",
        }
    }

    /// The compiled default, for the first launch.
    pub fn fallback(self) -> bool {
        match self {
            GuideSwitch::Guides
            | GuideSwitch::PixelGrid
            | GuideSwitch::Snap
            | GuideSwitch::SnapToGuides
            | GuideSwitch::SnapToLayers
            | GuideSwitch::SnapToDocumentBounds => true,
            GuideSwitch::Rulers
            | GuideSwitch::Grid
            | GuideSwitch::SnapToGrid
            | GuideSwitch::LockGuides => false,
        }
    }

    pub fn load(self) -> bool {
        settings::bool_value(self.key(), self.fallback())
    }

    pub fn store(self, value: bool) {
        settings::set_bool(value, self.key());
    }
}

/// Every View-menu guide/grid/snap preference, as one loadable record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuideSettings {
    pub shows_guides: bool,
    pub shows_rulers: bool,
    pub shows_grid: bool,
    pub shows_pixel_grid: bool,
    pub snap_enabled: bool,
    pub snap_to_guides: bool,
    pub snap_to_grid: bool,
    pub snap_to_layers: bool,
    pub snap_to_document_bounds: bool,
    pub locks_guides: bool,
    pub layout_grid: LayoutGrid,
    pub grid_appearance: GridAppearance,
}

impl Default for GuideSettings {
    fn default() -> Self {
        Self {
            shows_guides: GuideSwitch::Guides.fallback(),
            shows_rulers: GuideSwitch::Rulers.fallback(),
            shows_grid: GuideSwitch::Grid.fallback(),
            shows_pixel_grid: GuideSwitch::PixelGrid.fallback(),
            snap_enabled: GuideSwitch::Snap.fallback(),
            snap_to_guides: GuideSwitch::SnapToGuides.fallback(),
            snap_to_grid: GuideSwitch::SnapToGrid.fallback(),
            snap_to_layers: GuideSwitch::SnapToLayers.fallback(),
            snap_to_document_bounds: GuideSwitch::SnapToDocumentBounds.fallback(),
            locks_guides: GuideSwitch::LockGuides.fallback(),
            layout_grid: LayoutGrid::default(),
            grid_appearance: GridAppearance::default(),
        }
    }
}

impl GuideSettings {
    /// The switches and grid look as last stored, or the compiled defaults.
    pub fn load() -> Self {
        Self {
            shows_guides: GuideSwitch::Guides.load(),
            shows_rulers: GuideSwitch::Rulers.load(),
            shows_grid: GuideSwitch::Grid.load(),
            shows_pixel_grid: GuideSwitch::PixelGrid.load(),
            snap_enabled: GuideSwitch::Snap.load(),
            snap_to_guides: GuideSwitch::SnapToGuides.load(),
            snap_to_grid: GuideSwitch::SnapToGrid.load(),
            snap_to_layers: GuideSwitch::SnapToLayers.load(),
            snap_to_document_bounds: GuideSwitch::SnapToDocumentBounds.load(),
            locks_guides: GuideSwitch::LockGuides.load(),
            layout_grid: GuideSettings::stored_layout_grid(),
            grid_appearance: GuideSettings::stored_grid_appearance(),
        }
    }

    pub fn store(&self) {
        GuideSwitch::Guides.store(self.shows_guides);
        GuideSwitch::Rulers.store(self.shows_rulers);
        GuideSwitch::Grid.store(self.shows_grid);
        GuideSwitch::PixelGrid.store(self.shows_pixel_grid);
        GuideSwitch::Snap.store(self.snap_enabled);
        GuideSwitch::SnapToGuides.store(self.snap_to_guides);
        GuideSwitch::SnapToGrid.store(self.snap_to_grid);
        GuideSwitch::SnapToLayers.store(self.snap_to_layers);
        GuideSwitch::SnapToDocumentBounds.store(self.snap_to_document_bounds);
        GuideSwitch::LockGuides.store(self.locks_guides);
        GuideSettings::store_layout_grid(self.layout_grid);
        GuideSettings::store_grid_appearance(self.grid_appearance);
    }

    /// The layout grid's spacing and subdivisions (View > Grid Settings…). The person's, not the
    /// project's.
    pub fn stored_layout_grid() -> LayoutGrid {
        LayoutGrid::new(
            settings::int_value("gridSpacing", 64).max(0) as usize,
            settings::int_value("gridSubdivisions", 8).max(0) as usize,
        )
    }

    pub fn store_layout_grid(grid: LayoutGrid) {
        settings::set_int(grid.spacing() as i64, "gridSpacing");
        settings::set_int(grid.subdivisions() as i64, "gridSubdivisions");
    }

    /// The layout grid's color, line style and opacity (View > Grid Settings…), also the person's.
    pub fn stored_grid_appearance() -> GridAppearance {
        let default = GridAppearance::default();
        GridAppearance {
            preset: GridAppearancePreset::from_raw(&settings::string_value("gridColor", ""))
                .unwrap_or(default.preset),
            custom_color: PaletteColor::from_hex(&settings::string_value("gridCustomColor", ""))
                .unwrap_or(default.custom_color),
            style: GridAppearanceStyle::from_raw(&settings::string_value("gridStyle", ""))
                .unwrap_or(default.style),
            opacity: settings::int_value("gridOpacity", default.opacity as i64).max(0) as usize,
        }
    }

    pub fn store_grid_appearance(appearance: GridAppearance) {
        settings::set_string(appearance.preset.raw_value(), "gridColor");
        settings::set_string(&appearance.custom_color.hex(), "gridCustomColor");
        settings::set_string(appearance.style.raw_value(), "gridStyle");
        settings::set_int(appearance.opacity as i64, "gridOpacity");
    }
}

/// The rulers' arithmetic (`CanvasRuler`): tick spacing and labels. The ruler's drawing is the UI's.
pub struct CanvasRuler;

impl CanvasRuler {
    /// The ruler strip's thickness in points.
    pub const THICKNESS: CGFloat = 18.0;

    /// Numbered ticks about 70 points apart, using 1-2-5 steps in document pixels.
    pub fn major_step(points_per_pixel: CGFloat) -> CGFloat {
        let target = 70.0 / points_per_pixel.max(0.0001);
        const NICE: [CGFloat; 18] = [
            1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0, 1_000.0, 2_000.0,
            2_500.0, 5_000.0, 10_000.0, 20_000.0, 25_000.0,
        ];
        NICE.into_iter()
            .find(|step| *step >= target)
            .unwrap_or(50_000.0)
    }

    /// A tick's label: the value to whole pixels, "0" at the origin.
    pub fn label(value: CGFloat) -> String {
        let rounded = value.round();
        if rounded == 0.0 {
            "0".to_string()
        } else {
            format!("{}", rounded as i64)
        }
    }
}

impl EditorSession {
    pub fn can_clear_guides(&self) -> bool {
        self.document
            .as_ref()
            .map(|document| !document.guides.is_empty())
            .unwrap_or(false)
    }

    /// Upstream reads `showsBusy` first so the UI re-evaluates when a long operation starts or ends;
    /// here the callers already re-check it, so the flag is simply part of the test.
    pub fn can_edit_guides(&self) -> bool {
        self.document.is_some()
            && !self.locks_guides
            && !self.is_project_busy
            && !self.is_importing
            && !self.shows_new_document
            && self.levels.is_none()
            && self.hue_saturation.is_none()
            && self.filter_edit.is_none()
            && self.renaming_layer_id.is_none()
    }

    /// Guides as currently shown, including a drag in progress.
    pub fn displayed_guides(&self) -> Vec<CanvasGuide> {
        let mut guides = self
            .document
            .as_ref()
            .map(|document| document.guides.clone())
            .unwrap_or_default();
        let Some(drag) = self.guide_drag else {
            return guides;
        };
        let current = CanvasGuide::new(drag.id, drag.axis, drag.position);
        if let Some(index) = guides.iter().position(|guide| guide.id == drag.id) {
            guides[index] = current;
        } else if drag.is_new {
            guides.push(current);
        }
        guides
    }

    pub fn hit_guide(&self, at: Point, tolerance: CGFloat) -> Option<CanvasGuide> {
        if !self.shows_guides || self.locks_guides {
            return None;
        }
        let document = self.document.as_ref()?;
        let mut best: Option<(CanvasGuide, CGFloat)> = None;
        for guide in self.displayed_guides() {
            let distance = if guide.axis == CanvasGuideAxis::Vertical {
                let x = self
                    .viewport
                    .view_point(Point::new(guide.position, 0.0), document.size())
                    .x;
                (at.x - x).abs()
            } else {
                let y = self
                    .viewport
                    .view_point(Point::new(0.0, guide.position), document.size())
                    .y;
                (at.y - y).abs()
            };
            if distance <= tolerance
                && best
                    .map(|(_, best_distance)| distance < best_distance)
                    .unwrap_or(true)
            {
                best = Some((guide, distance));
            }
        }
        best.map(|(guide, _)| guide)
    }

    pub fn begin_guide_creation(&mut self, axis: CanvasGuideAxis, position: f64) {
        if !self.can_edit_guides() {
            return;
        }
        self.shows_guides = true;
        self.guide_drag = Some(GuideDrag {
            id: compositor_core::new_id(),
            axis,
            position: self.snapped_guide_position(position, axis, None),
            is_new: true,
            original: None,
        });
        self.refresh_canvas_preview();
    }

    pub fn begin_guide_move(&mut self, guide: CanvasGuide) {
        if !self.can_edit_guides() {
            return;
        }
        self.guide_drag = Some(GuideDrag {
            id: guide.id,
            axis: guide.axis,
            position: guide.position,
            is_new: false,
            original: Some(guide.position),
        });
        self.refresh_canvas_preview();
    }

    pub fn move_guide_drag(&mut self, position: f64) {
        let Some(mut drag) = self.guide_drag else {
            return;
        };
        drag.position = self.snapped_guide_position(position, drag.axis, Some(drag.id));
        self.guide_drag = Some(drag);
        self.refresh_canvas_preview();
    }

    /// `delete` is true when the pointer was released on a ruler (cancel a new guide, remove an
    /// existing one).
    pub fn finish_guide_drag(&mut self, delete: bool) {
        let Some(drag) = self.guide_drag.take() else {
            return;
        };
        if delete {
            if drag.is_new {
                self.refresh_canvas_preview();
                return;
            }
            self.begin_edit("Delete Guide");
            if let Some(document) = self.document.as_mut() {
                document.guides.retain(|guide| guide.id != drag.id);
            }
            self.end_edit();
            self.refresh_canvas_preview();
            return;
        }
        if drag.is_new {
            self.begin_edit("New Guide");
            if let Some(document) = self.document.as_mut() {
                document
                    .guides
                    .push(CanvasGuide::new(drag.id, drag.axis, drag.position));
            }
            self.end_edit();
        } else if drag.original != Some(drag.position) {
            self.begin_edit("Move Guide");
            if let Some(document) = self.document.as_mut() {
                if let Some(index) = document.guides.iter().position(|guide| guide.id == drag.id) {
                    document.guides[index].position = drag.position;
                }
            }
            self.end_edit();
        }
        self.refresh_canvas_preview();
    }

    pub fn cancel_guide_drag(&mut self) {
        self.guide_drag = None;
        self.refresh_canvas_preview();
    }

    pub fn clear_guides(&mut self) {
        if !self.can_clear_guides() {
            return;
        }
        self.begin_edit("Clear Guides");
        if let Some(document) = self.document.as_mut() {
            document.guides = Vec::new();
        }
        self.end_edit();
        self.refresh_canvas_preview();
    }

    pub fn add_guide(&mut self, guide: CanvasGuide) {
        if !self.can_edit_guides() {
            return;
        }
        self.shows_guides = true;
        self.begin_edit("New Guide");
        if let Some(document) = self.document.as_mut() {
            document.guides.push(guide);
        }
        self.end_edit();
    }

    /// Alignment lines a move or crop may snap to, according to View > Snap and Snap To.
    pub fn alignment_snap_targets(
        &self,
        excluding: &HashSet<Id>,
        include_centers: bool,
    ) -> (Vec<f64>, Vec<f64>) {
        if !self.snap_enabled {
            return (Vec::new(), Vec::new());
        }
        let Some(document) = self.document.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        let size = document.size();
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        if self.snap_to_document_bounds {
            xs.extend([0.0, size.width]);
            ys.extend([0.0, size.height]);
            if include_centers {
                xs.push(size.width / 2.0);
                ys.push(size.height / 2.0);
            }
        }
        if self.snap_to_layers {
            for layer in document.render_layers() {
                if layer.asset.is_none() || excluding.contains(&layer.id) {
                    continue;
                }
                let corners = DistortWarp::corners(&self.displayed_transform(layer));
                let mut min_x = f64::INFINITY;
                let mut max_x = f64::NEG_INFINITY;
                let mut min_y = f64::INFINITY;
                let mut max_y = f64::NEG_INFINITY;
                for corner in corners {
                    min_x = min_x.min(corner.x);
                    max_x = max_x.max(corner.x);
                    min_y = min_y.min(corner.y);
                    max_y = max_y.max(corner.y);
                }
                if !(min_x.is_finite()
                    && max_x.is_finite()
                    && min_y.is_finite()
                    && max_y.is_finite())
                {
                    continue;
                }
                if include_centers {
                    xs.extend([
                        min_x.round(),
                        ((min_x + max_x) / 2.0).round(),
                        max_x.round(),
                    ]);
                    ys.extend([
                        min_y.round(),
                        ((min_y + max_y) / 2.0).round(),
                        max_y.round(),
                    ]);
                } else {
                    xs.extend([min_x.round(), max_x.round()]);
                    ys.extend([min_y.round(), max_y.round()]);
                }
            }
        }
        // Hidden extras do not snap, matching Photoshop.
        if self.snap_to_grid && self.shows_grid {
            xs.extend(self.layout_grid.lines(size.width));
            ys.extend(self.layout_grid.lines(size.height));
        }
        if self.snap_to_guides && self.shows_guides {
            for guide in self.displayed_guides() {
                if guide.axis == CanvasGuideAxis::Vertical {
                    xs.push(guide.position);
                } else {
                    ys.push(guide.position);
                }
            }
        }
        (xs, ys)
    }

    /// A guide position pulled onto a nearby grid line, guide, canvas edge/center or layer edge,
    /// within the same screen-point tolerance the layer snaps use.
    pub fn snapped_guide_position(
        &self,
        position: f64,
        axis: CanvasGuideAxis,
        excluding: Option<Id>,
    ) -> f64 {
        if !self.snap_enabled {
            return position;
        }
        let Some(document) = self.document.as_ref() else {
            return position;
        };
        let tolerance = TransformSnap::DISTANCE / self.viewport.points_per_pixel().max(0.0001);
        let mut targets: Vec<f64> = Vec::new();
        let size = document.size();
        let length = if axis == CanvasGuideAxis::Vertical {
            size.width
        } else {
            size.height
        };
        if self.snap_to_grid && self.shows_grid {
            targets.extend(self.layout_grid.lines(length));
        }
        if self.snap_to_guides && self.shows_guides {
            targets.extend(
                self.displayed_guides()
                    .into_iter()
                    .filter(|guide| guide.axis == axis && Some(guide.id) != excluding)
                    .map(|guide| guide.position),
            );
        }
        if self.snap_to_document_bounds {
            targets.extend([0.0, length / 2.0, length]);
        }
        if self.snap_to_layers {
            for layer in document.render_layers() {
                if layer.asset.is_none() {
                    continue;
                }
                let corners = DistortWarp::corners(&self.displayed_transform(layer));
                let mut min = f64::INFINITY;
                let mut max = f64::NEG_INFINITY;
                for corner in corners {
                    let value = if axis == CanvasGuideAxis::Vertical {
                        corner.x
                    } else {
                        corner.y
                    };
                    min = min.min(value);
                    max = max.max(value);
                }
                if !(min.is_finite() && max.is_finite()) {
                    continue;
                }
                targets.extend([min.round(), ((min + max) / 2.0).round(), max.round()]);
            }
        }
        let mut best: Option<f64> = None;
        for target in targets {
            if (target - position).abs() <= tolerance {
                if let Some(current) = best {
                    if (current - position).abs() <= (target - position).abs() {
                        continue;
                    }
                }
                best = Some(target);
            }
        }
        best.unwrap_or(position)
    }

    /// A guide's document position for a point in the canvas' view coordinates
    /// (`EditorCanvas.documentPosition(axis:at:)`).
    pub fn guide_position_for(&self, axis: CanvasGuideAxis, view_point: Point) -> Option<f64> {
        let document = self.document.as_ref()?;
        let pixel = self.viewport.document_point(view_point, document.size());
        Some(if axis == CanvasGuideAxis::Vertical {
            pixel.x
        } else {
            pixel.y
        })
    }

    /// Released on the top or left ruler strip, which sits just outside the canvas
    /// (`EditorCanvas.isOverRuler`): a new guide is cancelled, an existing one deleted.
    pub fn is_over_ruler(&self, view_point: Point) -> bool {
        self.shows_rulers && (view_point.x < 0.0 || view_point.y < 0.0)
    }

    /// The layout grid as the overlay draws it: `(spacing, subdivisions, step)`.
    pub fn layout_grid_description(&self) -> (usize, usize, CGFloat) {
        (
            self.layout_grid.spacing(),
            self.layout_grid.subdivisions(),
            self.layout_grid.step(),
        )
    }

    /// The grid's lines along a document edge, majors and subdivisions, in whole pixels.
    pub fn layout_grid_lines(&self, length: CGFloat) -> Vec<CGFloat> {
        self.layout_grid.lines(length)
    }

    /// The grid appearance the overlay draws with.
    pub fn grid_color(&self) -> PaletteColor {
        self.grid_appearance.color()
    }

    pub fn grid_major_alpha(&self) -> CGFloat {
        self.grid_appearance.major_alpha()
    }

    pub fn grid_subdivision_alpha(&self) -> CGFloat {
        self.grid_appearance.subdivision_alpha()
    }

    /// The major lines' dash pattern in view points; empty for solid lines.
    pub fn grid_dashes(&self) -> &'static [CGFloat] {
        self.grid_appearance.style.dashes()
    }

    /// The rulers' strip thickness, in points.
    pub fn ruler_thickness(&self) -> CGFloat {
        CanvasRuler::THICKNESS
    }

    /// The size a new document is suggested from a clipboard image (`NewCanvasSheet`): the decoded
    /// pixel size, with a rotated EXIF orientation (5…8) swapped, and only when both sides are
    /// valid document dimensions. `None` when it isn't a usable image.
    pub fn clipboard_canvas_size(
        width: i64,
        height: i64,
        orientation: Option<i64>,
    ) -> Option<(usize, usize)> {
        let (mut width, mut height) = (width, height);
        if let Some(orientation) = orientation {
            if (5..=8).contains(&orientation) {
                std::mem::swap(&mut width, &mut height);
            }
        }
        let width = Self::valid_dimension(width)?;
        let height = Self::valid_dimension(height)?;
        Some((width, height))
    }

    /// The shared size check for clipboard dimensions (`CanvasDocument.validDimension`).
    fn valid_dimension(value: i64) -> Option<usize> {
        if (1..=compositor_core::limits::MAX_SIDE as i64).contains(&value) {
            Some(value as usize)
        } else {
            None
        }
    }

    /// The document rectangle the guides are laid out over, for the overlays.
    pub fn document_bounds(&self) -> Option<Size> {
        self.document.as_ref().map(|document| document.size())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_carry_the_upstream_keys_and_defaults() {
        for switch in GuideSwitch::ALL {
            assert!(!switch.key().is_empty());
        }
        assert_eq!(GuideSwitch::Guides.key(), "guides");
        assert_eq!(GuideSwitch::SnapToDocumentBounds.key(), "snapBounds");
        assert_eq!(GuideSwitch::LockGuides.key(), "lockGuides");
        assert!(GuideSwitch::Guides.fallback());
        assert!(GuideSwitch::PixelGrid.fallback());
        assert!(GuideSwitch::Snap.fallback());
        assert!(GuideSwitch::SnapToGuides.fallback());
        assert!(GuideSwitch::SnapToLayers.fallback());
        assert!(GuideSwitch::SnapToDocumentBounds.fallback());
        assert!(!GuideSwitch::Rulers.fallback());
        assert!(!GuideSwitch::Grid.fallback());
        assert!(!GuideSwitch::SnapToGrid.fallback());
        assert!(!GuideSwitch::LockGuides.fallback());
    }

    #[test]
    fn guide_settings_default_matches_the_compiled_defaults() {
        let settings = GuideSettings::default();
        assert_eq!(settings.layout_grid, LayoutGrid::new(64, 8));
        assert_eq!(settings.grid_appearance, GridAppearance::default());
        assert!(settings.shows_guides && settings.snap_enabled && settings.snap_to_guides);
        assert!(!settings.shows_grid && !settings.locks_guides);
    }

    #[test]
    fn ruler_steps_use_one_two_five_and_label_whole_pixels() {
        assert_eq!(CanvasRuler::major_step(1.0), 100.0);
        assert_eq!(CanvasRuler::major_step(0.5), 200.0);
        assert_eq!(CanvasRuler::major_step(2.0), 50.0);
        assert_eq!(CanvasRuler::major_step(0.0), 50_000.0);
        assert_eq!(CanvasRuler::label(0.0), "0");
        assert_eq!(CanvasRuler::label(24.4), "24");
        assert_eq!(CanvasRuler::label(-12.6), "-13");
    }

    #[test]
    fn clipboard_size_swaps_rotated_orientations_and_validates() {
        assert_eq!(
            EditorSession::clipboard_canvas_size(1920, 1080, None),
            Some((1920, 1080))
        );
        assert_eq!(
            EditorSession::clipboard_canvas_size(1920, 1080, Some(6)),
            Some((1080, 1920))
        );
        assert_eq!(
            EditorSession::clipboard_canvas_size(1920, 1080, Some(1)),
            Some((1920, 1080))
        );
        assert_eq!(EditorSession::clipboard_canvas_size(0, 1080, None), None);
        assert_eq!(EditorSession::clipboard_canvas_size(40_000, 10, None), None);
    }

    fn session_with_document(width: usize, height: usize) -> EditorSession {
        let mut session = EditorSession::default();
        session.document = Some(compositor_core::document::CanvasDocument::new(
            width, height,
        ));
        session
    }

    #[test]
    fn a_new_guide_is_one_undo_step_named_new_guide() {
        let mut session = session_with_document(100, 80);
        session.snap_enabled = false;
        session.begin_guide_creation(CanvasGuideAxis::Vertical, 25.0);
        assert!(session.shows_guides, "creating a guide shows them");
        let drag = session.guide_drag.expect("drag");
        assert!(drag.is_new);
        assert_eq!(drag.position, 25.0);
        // A drag in progress shows through `displayedGuides` before it is committed.
        assert_eq!(session.displayed_guides().len(), 1);
        session.move_guide_drag(60.0);
        session.finish_guide_drag(false);
        assert_eq!(session.guide_drag, None);
        let guides = &session.document.as_ref().unwrap().guides;
        assert_eq!(guides.len(), 1);
        assert_eq!(guides[0].position, 60.0);
        assert_eq!(guides[0].axis, CanvasGuideAxis::Vertical);
        assert_eq!(session.history.undo_name(), "New Guide");
    }

    #[test]
    fn releasing_a_new_guide_on_the_ruler_leaves_nothing_behind() {
        let mut session = session_with_document(100, 80);
        session.begin_guide_creation(CanvasGuideAxis::Horizontal, 20.0);
        session.finish_guide_drag(true);
        assert!(session.document.as_ref().unwrap().guides.is_empty());
        assert!(!session.history.can_undo());
        assert_eq!(session.guide_drag, None);
    }

    #[test]
    fn deleting_an_existing_guide_is_one_undo_step_named_delete_guide() {
        let mut session = session_with_document(100, 80);
        session.snap_enabled = false;
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 10.0));
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 30.0));
        let doomed = session.document.as_ref().unwrap().guides[0];
        session.begin_guide_move(doomed);
        session.move_guide_drag(40.0);
        session.finish_guide_drag(true);
        let guides = &session.document.as_ref().unwrap().guides;
        assert_eq!(guides.len(), 1);
        assert_eq!(guides[0].position, 30.0);
        assert_eq!(session.history.undo_name(), "Delete Guide");
    }

    #[test]
    fn moving_a_guide_commits_only_when_it_moved() {
        let mut session = session_with_document(100, 80);
        session.snap_enabled = false;
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 10.0));
        let guide = session.document.as_ref().unwrap().guides[0];
        let history_before = session.history.undo_count();
        session.begin_guide_move(guide);
        session.finish_guide_drag(false);
        assert_eq!(
            session.history.undo_count(),
            history_before,
            "no move, no edit"
        );
        session.begin_guide_move(guide);
        session.move_guide_drag(55.0);
        session.finish_guide_drag(false);
        assert_eq!(session.document.as_ref().unwrap().guides[0].position, 55.0);
        assert_eq!(session.history.undo_name(), "Move Guide");
        // Escape puts a drag away without touching the document.
        session.begin_guide_move(guide);
        session.move_guide_drag(70.0);
        session.cancel_guide_drag();
        assert_eq!(session.document.as_ref().unwrap().guides[0].position, 55.0);
        assert_eq!(session.guide_drag, None);
    }

    #[test]
    fn locked_guides_refuse_edits() {
        let mut session = session_with_document(100, 80);
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 10.0));
        session.locks_guides = true;
        session.clear_guides();
        // Clear Guides still works while locked (`GuideTests.swift: lockPreventsCreatingAndMoving`,
        // line 47-48: `canClearGuides` checks only for guides, not the lock).
        assert!(session.document.as_ref().unwrap().guides.is_empty());
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 20.0));
        assert!(session.document.as_ref().unwrap().guides.is_empty());
        session.begin_guide_creation(CanvasGuideAxis::Vertical, 30.0);
        assert_eq!(session.guide_drag, None);
        session.locks_guides = false;
        session.clear_guides();
        assert!(session.document.as_ref().unwrap().guides.is_empty());
        assert_eq!(session.history.undo_name(), "Clear Guides");
    }

    #[test]
    fn hit_testing_uses_the_view_space_distance() {
        let mut session = session_with_document(1000, 800);
        session.viewport.resize(
            Size::new(1000.0, 800.0),
            1.0,
            Some(Size::new(1000.0, 800.0)),
        );
        session
            .viewport
            .set_zoom(1.0, Point::ZERO, Size::new(1000.0, 800.0));
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 300.0));
        let x = session
            .viewport
            .view_point(Point::new(300.0, 0.0), Size::new(1000.0, 800.0))
            .x;
        assert!(session
            .hit_guide(Point::new(x + 4.0, 100.0), GUIDE_HIT_DISTANCE)
            .is_some());
        assert!(session
            .hit_guide(Point::new(x + 9.0, 100.0), GUIDE_HIT_DISTANCE)
            .is_none());
        session.shows_guides = false;
        assert!(session
            .hit_guide(Point::new(x, 100.0), GUIDE_HIT_DISTANCE)
            .is_none());
    }

    #[test]
    fn alignment_targets_follow_the_snap_to_switches() {
        let mut session = session_with_document(100, 80);
        session.snap_enabled = false;
        assert_eq!(
            session.alignment_snap_targets(&HashSet::new(), true),
            (Vec::new(), Vec::new())
        );

        session.snap_enabled = true;
        session.snap_to_document_bounds = true;
        session.snap_to_layers = false;
        session.snap_to_grid = false;
        session.snap_to_guides = false;
        let (xs, ys) = session.alignment_snap_targets(&HashSet::new(), true);
        assert_eq!(xs, vec![0.0, 100.0, 50.0]);
        assert_eq!(ys, vec![0.0, 80.0, 40.0]);
        let (xs, ys) = session.alignment_snap_targets(&HashSet::new(), false);
        assert_eq!(xs, vec![0.0, 100.0]);
        assert_eq!(ys, vec![0.0, 80.0]);

        // Hidden guides do not snap; shown ones do. `addGuide` shows the guides itself, as in
        // upstream (`Guides.swift: addGuide`), so the switch flips off after the add
        // (`GuideTests.swift: snapTargetsFollowViewMenu`, lines 123-126).
        session.snap_to_document_bounds = false;
        session.snap_to_guides = true;
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Vertical, 12.0));
        session.shows_guides = false;
        assert_eq!(
            session.alignment_snap_targets(&HashSet::new(), false).0,
            Vec::<f64>::new()
        );
        session.shows_guides = true;
        assert_eq!(
            session.alignment_snap_targets(&HashSet::new(), false).0,
            vec![12.0]
        );

        // The grid's lines appear only while it is shown.
        session.snap_to_guides = false;
        session.snap_to_grid = true;
        session.shows_grid = false;
        assert!(session
            .alignment_snap_targets(&HashSet::new(), false)
            .0
            .is_empty());
        session.shows_grid = true;
        session.layout_grid = LayoutGrid::new(50, 1);
        assert_eq!(
            session.alignment_snap_targets(&HashSet::new(), false).0,
            vec![0.0, 50.0, 100.0]
        );
    }

    #[test]
    fn guide_positions_snap_within_the_screen_tolerance() {
        let mut session = session_with_document(100, 80);
        session.viewport.resize(Size::new(100.0, 80.0), 1.0, None);
        session.snap_enabled = true;
        session.snap_to_document_bounds = true;
        session.snap_to_guides = false;
        session.snap_to_grid = false;
        session.snap_to_layers = false;
        // 10 screen points at 1 point per pixel: 40 is pulled to the 40 center target only when close.
        assert_eq!(
            session.snapped_guide_position(38.0, CanvasGuideAxis::Vertical, None),
            38.0
        );
        assert_eq!(
            session.snapped_guide_position(42.0, CanvasGuideAxis::Vertical, None),
            50.0
        );
        assert_eq!(
            session.snapped_guide_position(2.0, CanvasGuideAxis::Horizontal, None),
            0.0
        );
        // The guide being dragged is not a target for itself.
        let guide = CanvasGuide::at(CanvasGuideAxis::Vertical, 40.0);
        session.add_guide(guide);
        session.snap_to_guides = true;
        session.shows_guides = true;
        assert_eq!(
            session.snapped_guide_position(42.0, CanvasGuideAxis::Vertical, Some(guide.id)),
            50.0
        );
        assert_eq!(
            session.snapped_guide_position(42.0, CanvasGuideAxis::Vertical, None),
            40.0
        );
    }

    #[test]
    fn clearing_guides_needs_one_to_clear() {
        let mut session = session_with_document(100, 80);
        assert!(!session.can_clear_guides());
        session.add_guide(CanvasGuide::at(CanvasGuideAxis::Horizontal, 5.0));
        assert!(session.can_clear_guides());
        session.clear_guides();
        assert!(!session.can_clear_guides());
    }
}
