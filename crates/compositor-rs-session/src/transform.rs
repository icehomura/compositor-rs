//! `EditorSession`'s transform commands — the Move tool's box, its drags, snapping and distortion.
//!
//! Ports the session half of `Document/LayerTransform.swift` (its value types are
//! [`compositor_rs_core::layer_transform`]), the transform commands of `EditorSession.swift`
//! (`beginTransform`, `previewTransform`, `beginDuplicateTransform`, `commitTransform`,
//! `cancelTransform`, `nudgeLayer`, and the state they read: `canTransform`, `transformsAsGroup`,
//! `groupTransformMembers`, `groupTransformBox`, `transformPixelSize`, `displayedTransform`,
//! `transformTargetsMask`, `editedTransform`, `pendingTransform`), the session extension of
//! `Document/Distort.swift` (`beginDistort`, `previewCorners`, `distortShape`, `distortedEffects`,
//! `distortPreview`, `commitDistort` and the pixel resample they commit with) and
//! `Document/LayerMask.swift`'s `commitMaskTransform` and
//! `displayedMaskPlacement`. `Rendering/TransformOverlay.swift`'s geometry, its hit-testing and the
//! direction maths behind the handle cursors are here too — the drawing itself belongs to the UI.
//!
//! The Move tool's drag loop lives in the canvas view: it builds a `TransformDrag`, asks
//! `snappedResizePoint`/`snappedMove` (see `crop.rs`) where the pointer lands, then calls
//! [`EditorSession::preview_transform`] or [`EditorSession::preview_corners`] with the result, and
//! [`EditorSession::commit_transform`] when the button comes up. Swift's `Task`/`async` coordination
//! has no equivalent: the commands here are synchronous, and the view reads the state it used to
//! await (`is_project_busy`, `shows_busy`, `can_undo`) as `docs/PORTING.md` §4 lays out.
//!
//! The two previews `Distort.swift` leaves to the canvas — `distortedEffects` and `distortPreview`
//! — keep their warps in the session's [`DistortPreviewCache`] and [`DistortEffectsCache`], so
//! moving one corner does not redo a warp the last one already made; `commitDistort` seeds the
//! effects preview it warped with, so a layer's stroke and shadow do not blink off for a frame on
//! Apply while the worker renders the new pixels' effects. (`maskDistortPreview`, the mask's own
//! counterpart, is `mask_ops.rs`'s.)

use std::collections::{HashMap, HashSet};
use std::f64::consts::{FRAC_PI_4, PI};
use std::sync::Arc;

use compositor_rs_core::document::{ImageLayer, NavigationTool};
use compositor_rs_core::geom::{Point, Size};
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_mask::LayerMask;
use compositor_rs_core::layer_transform::{
    LayerTransform, TransformDragMode, TransformEdit, TransformGroup,
};
use compositor_rs_core::viewport::CanvasViewport;
use compositor_rs_core::CoreError;
use compositor_rs_core::Id;
use compositor_rs_pixels::adjustments::PixelAdjust;
use compositor_rs_pixels::effects::LayerEffectsRenderer;
use compositor_rs_pixels::warp::DistortWarp;
use compositor_rs_render::live_mask_renderer::MaskClip;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::session::EditorSession;

/// The copies an Option-drag made, and what was selected before it, so Escape can take them away again.
/// Swift held this as the loose tuple `(copies: [UUID], source: Set<UUID>, primary: UUID?)`.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformDuplicate {
    pub copies: Vec<Id>,
    pub source: HashSet<Id>,
    pub primary: Option<Id>,
}

/// The canvas's last warped preview, reused while the distortion and layer are unchanged.
#[derive(Clone, Debug)]
pub struct DistortPreviewCache {
    pub corners: Vec<Point>,
    pub draft: LayerTransform,
    pub image: PixelImage,
    pub mask: Option<PixelImage>,
    pub result: Option<(PixelImage, Option<PixelImage>, LayerTransform)>,
}

/// The last effects image warped for a distortion, so the corners can keep moving without redoing it.
#[derive(Clone, Debug)]
pub struct DistortEffectsCache {
    pub corners: Vec<Point>,
    pub image: PixelImage,
    pub result: Option<(PixelImage, LayerTransform)>,
}

/// Where a transform's handles sit in view points, and which drag a press starts
/// (`TransformOverlayGeometry`). The drawing belongs to the UI; the rotation handle's placement, the
/// hit-testing and the cursor direction maths are here.
#[derive(Clone, Debug, PartialEq)]
pub struct TransformOverlayGeometry {
    pub handles: [Point; 8],
    pub rotation_handle: Point,
    /// A distortion has no single rotation, so its rotation handle is hidden.
    pub shows_rotation: bool,
}

impl TransformOverlayGeometry {
    /// How far, in screen points, a press may land from a handle or an edge and still catch it.
    const HIT_DISTANCE: f64 = 10.0;
    /// How far the rotation handle stands above the top edge.
    const ROTATION_REACH: f64 = 28.0;

    /// The eight handles around `transform`'s box, in [`LayerTransform::HANDLES`] order, and the
    /// rotation handle 28 points up from the top edge's midpoint.
    pub fn new(transform: &LayerTransform, viewport: &CanvasViewport, document_size: Size) -> Self {
        let handles = std::array::from_fn(|index| {
            viewport.view_point(transform.point(LayerTransform::HANDLES[index]), document_size)
        });
        let (sin, cos) = transform.radians().sin_cos();
        let rotation_handle = Point::new(
            handles[1].x + sin * Self::ROTATION_REACH,
            handles[1].y - cos * Self::ROTATION_REACH,
        );
        Self {
            handles,
            rotation_handle,
            shows_rotation: true,
        }
    }

    /// Handles for a distortion: its four corners (document pixels) and the midpoints of its edges.
    pub fn from_corners(corners: &[Point], viewport: &CanvasViewport, document_size: Size) -> Self {
        assert!(corners.len() == 4, "a distortion has four corners");
        let view: Vec<Point> = corners
            .iter()
            .map(|corner| viewport.view_point(*corner, document_size))
            .collect();
        fn middle(a: Point, b: Point) -> Point {
            Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
        }
        let handles = [
            view[0],
            middle(view[0], view[1]),
            view[1],
            middle(view[1], view[2]),
            view[2],
            middle(view[2], view[3]),
            view[3],
            middle(view[3], view[0]),
        ];
        Self {
            handles,
            rotation_handle: handles[1],
            shows_rotation: false,
        }
    }

    /// Which drag a press at `point` (view points) starts: the rotation handle, a handle, or an edge
    /// — nil for the body, which moves. The rotation handle is tested first, as in the overlay.
    pub fn hit(&self, point: Point) -> Option<TransformDragMode> {
        let near = |other: Point| point.distance(other) <= Self::HIT_DISTANCE;
        if self.shows_rotation && near(self.rotation_handle) {
            return Some(TransformDragMode::Rotate);
        }
        if let Some(index) = self.handles.iter().position(|handle| near(*handle)) {
            return Some(TransformDragMode::Resize(index));
        }
        for (start, end, handle) in [(0usize, 2usize, 1usize), (2, 4, 3), (4, 6, 5), (6, 0, 7)] {
            let a = self.handles[start];
            let b = self.handles[end];
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let length_squared = dx * dx + dy * dy;
            if length_squared <= 0.0 {
                continue;
            }
            let t = ((point.x - a.x) * dx + (point.y - a.y) * dy) / length_squared;
            if (0.0..=1.0).contains(&t) && near(Point::new(a.x + t * dx, a.y + t * dy)) {
                return Some(TransformDragMode::Resize(handle));
            }
        }
        None
    }

    /// The frame-resize direction the handle's cursor points in: the box's own angle, the handle's
    /// quarter-turn offset, rounded to one of four positions (`resizeCursor(for:)`).
    pub fn resize_direction(&self, index: usize) -> ResizeDirection {
        let angle = (self.handles[2].y - self.handles[0].y).atan2(self.handles[2].x - self.handles[0].x);
        const OFFSETS: [f64; 8] = [
            FRAC_PI_4,
            PI / 2.0,
            3.0 * FRAC_PI_4,
            0.0,
            FRAC_PI_4,
            PI / 2.0,
            3.0 * FRAC_PI_4,
            0.0,
        ];
        let direction = ((angle + OFFSETS[index]) / FRAC_PI_4).round() as i64;
        ResizeDirection::ALL[direction.rem_euclid(4) as usize]
    }
}

/// The four directions `NSCursor.frameResize` picked from, in the order the overlay's `positions`
/// array listed them — the UI maps them onto its own cursors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResizeDirection {
    Right,
    BottomRight,
    Bottom,
    TopRight,
}

impl ResizeDirection {
    /// `NSCursor.FrameResizePosition`'s array order.
    pub const ALL: [ResizeDirection; 4] = [Self::Right, Self::BottomRight, Self::Bottom, Self::TopRight];
}

/// The arrow keys the canvas and the Layers panel step a layer with: AppKit's `NSEvent.keyCode`
/// 123…126.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArrowKey {
    Left,
    Right,
    Up,
    Down,
}

impl ArrowKey {
    /// The AppKit key codes the two arrow-key handlers matched.
    pub fn from_key_code(code: u16) -> Option<Self> {
        match code {
            123 => Some(Self::Left),
            124 => Some(Self::Right),
            125 => Some(Self::Down),
            126 => Some(Self::Up),
            _ => None,
        }
    }

    /// The step this key takes with the Move tool: 1 px, 10 px with Shift.
    pub fn step(self, shift: bool) -> (f64, f64) {
        let step = if shift { 10.0 } else { 1.0 };
        match self {
            Self::Left => (-step, 0.0),
            Self::Right => (step, 0.0),
            Self::Up => (0.0, -step),
            Self::Down => (0.0, step),
        }
    }
}

/// What [`EditorSession::distort_at`] does to a layer's mask, decided before the document is lent
/// mutably.
enum MaskChange {
    Replace(LayerMask),
    SetPlacement(Option<LayerTransform>),
}

/// Identity of the shared rasters the Swift compared with `===` (`warped.image === mask.asset.image`).
fn same_image(left: &PixelImage, right: &PixelImage) -> bool {
    match (left, right) {
        (PixelImage::Rgba(left), PixelImage::Rgba(right)) => Arc::ptr_eq(left, right),
        (PixelImage::Gray(left), PixelImage::Gray(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

/// The same identity for the optional images the Swift compared with `===` (`cache.mask === mask`):
/// both absent, or one shared raster on each side.
fn same_optional_image(left: &Option<PixelImage>, right: &Option<PixelImage>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => same_image(left, right),
        (None, None) => true,
        _ => false,
    }
}

/// `LayerMask.background(of:)` over the pixel type a mask's thumbnail carries; white when it is not
/// the gray raster a mask is meant to hold.
pub(crate) fn mask_background(thumbnail: &PixelImage) -> f64 {
    thumbnail.as_gray().map_or(1.0, LayerMask::background)
}

impl EditorSession {
    // MARK: - What a transform targets

    /// Whether transforming moves several layers, or a folder's contents, in one box
    /// (`transformsAsGroup`).
    pub fn transforms_as_group(&self) -> bool {
        let count = self.selected_layer_ids.len();
        count > 1 || (count == 1 && self.active_layer().is_some_and(|layer| layer.is_group))
    }

    /// Whether a transform can start at all (`canTransform`).
    pub fn can_transform(&self) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        // Several selected layers, or a folder's contents, transform together.
        if self.transforms_as_group() {
            return !self.group_transform_members().is_empty();
        }
        let Some(layer) = self.active_layer() else { return false };
        if layer.asset.is_none() || layer.is_group {
            return false;
        }
        let Some(id) = self.active_layer_id else { return false };
        self.document
            .as_ref()
            .is_some_and(|document| document.effective_visible_ids().contains(&id))
    }

    /// What a group transform moves: the visible pixel layers selected and inside selected folders.
    pub fn group_transform_members(&self) -> Vec<Id> {
        if !self.transforms_as_group() {
            return Vec::new();
        }
        let Some(document) = self.document.as_ref() else { return Vec::new() };
        let parents: FxHashMap<Id, Option<Id>> =
            document.layers.iter().map(|layer| (layer.id, layer.parent_id)).collect();
        let visible = document.effective_visible_ids();
        document
            .layers
            .iter()
            .filter(|layer| {
                if layer.asset.is_none() || layer.is_group || !visible.contains(&layer.id) {
                    return false;
                }
                let mut current = Some(layer.id);
                for _ in 0..64 {
                    let Some(id) = current else { return false };
                    if self.selected_layer_ids.contains(&id) {
                        return true;
                    }
                    current = parents.get(&id).copied().flatten();
                }
                false
            })
            .map(|layer| layer.id)
            .collect()
    }

    /// The upright box around `group_transform_members`, at `max(1, …)` pixels a side.
    pub fn group_transform_box(&self) -> Option<LayerTransform> {
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        let mut found = false;
        for id in self.group_transform_members() {
            let Some(layer) = self
                .document
                .as_ref()
                .and_then(|document| document.layers.iter().find(|layer| layer.id == id))
            else {
                continue;
            };
            for corner in DistortWarp::corners(&layer.transform) {
                min_x = min_x.min(corner.x);
                min_y = min_y.min(corner.y);
                max_x = max_x.max(corner.x);
                max_y = max_y.max(corner.y);
                found = true;
            }
        }
        if !found {
            return None;
        }
        Some(LayerTransform {
            origin: Point::new(min_x, min_y),
            size: Size::new((max_x - min_x).max(1.0), (max_y - min_y).max(1.0)),
            ..Default::default()
        })
    }

    /// Pixels the transform places — what 100% scale draws 1:1. Nil for a layer without pixels.
    pub fn transform_pixel_size(&self) -> Option<Size> {
        if let Some(group) = self.transform_edit.as_ref().and_then(|edit| edit.group.as_ref()) {
            return Some(group.r#box.size);
        }
        if self.transform_edit.is_none() && self.transforms_as_group() {
            return self.group_transform_box().map(|box_| box_.size);
        }
        if self.transform_targets_mask() {
            return None;
        }
        if let Some(floating) = self.transform_edit.as_ref().and_then(|edit| edit.floating.as_ref()) {
            return Some(floating.pixel_size);
        }
        let image = &self.active_layer()?.asset.as_ref()?.image;
        Some(Size::new(image.width() as f64, image.height() as f64))
    }

    /// Whether transforming places only the active layer's mask (an unlinked mask selected in the
    /// Layers panel).
    pub fn transform_targets_mask(&self) -> bool {
        match self.transform_edit.as_ref() {
            Some(edit) => edit.mask,
            None => self.is_mask_selected && self.active_layer().is_some_and(|layer| layer.mask.as_ref().is_some_and(|mask| !mask.is_linked)),
        }
    }

    /// The transform a layer is shown with (`displayedTransform`).
    pub fn displayed_transform(&self, layer: &ImageLayer) -> LayerTransform {
        if let Some(pending) = self.pending_transform(layer) {
            return pending;
        }
        // Content-Aware Fill past the layer's edge previews on the grown layer.
        if let Some(edit) = self.filter_edit.as_ref() {
            if let Some(grown) = edit.prepared_transform {
                if edit.preview_image(layer.id).is_some() {
                    return grown;
                }
            }
        }
        layer.transform
    }

    /// Where `layer`'s transform handles sit: the pending edit's draft — the layer's or its mask's —
    /// else the layer.
    pub fn edited_transform(&self, layer: &ImageLayer) -> LayerTransform {
        if let Some(edit) = self.transform_edit.as_ref() {
            if edit.layer_id == layer.id {
                return edit.draft;
            }
        }
        if self.transform_edit.is_none() && Some(layer.id) == self.active_layer_id && self.transforms_as_group() {
            if let Some(box_) = self.group_transform_box() {
                return box_;
            }
        }
        if Some(layer.id) == self.active_layer_id && self.transform_targets_mask() {
            return layer.mask_transform();
        }
        layer.transform
    }

    /// A layer's transform under the pending edit: the draft for the edited layer, carried along with
    /// the box for each layer of a group; nil when the edit doesn't move it.
    pub fn pending_transform(&self, layer: &ImageLayer) -> Option<LayerTransform> {
        let edit = self.transform_edit.as_ref()?;
        if edit.mask {
            return None;
        }
        if let Some(group) = edit.group.as_ref() {
            return group
                .originals
                .get(&layer.id)
                .map(|original| original.following(group.r#box, edit.draft));
        }
        if edit.layer_id == layer.id {
            Some(edit.draft)
        } else {
            None
        }
    }

    // MARK: - The transform lifecycle

    /// Begins a transform of the active layer — or of everything selected, when several layers or a
    /// folder are. `persistent` edits wait for Apply; a drag's (`false`) applies when it is let go.
    pub fn begin_transform(&mut self, persistent: bool) {
        self.cancel_crop();
        if self.transform_edit.is_some() || !self.can_transform() {
            return;
        }
        let Some((layer_id, layer_transform, mask_transform, mask_linked)) = self.active_layer().map(|layer| {
            (
                layer.id,
                layer.transform,
                layer.mask_transform(),
                layer.mask.as_ref().is_some_and(|mask| mask.is_linked),
            )
        }) else {
            return;
        };
        self.tool = NavigationTool::Move;
        if self.transforms_as_group() {
            let Some(box_) = self.group_transform_box() else { return };
            let mut originals = HashMap::new();
            for id in self.group_transform_members() {
                if let Some(member) = self
                    .document
                    .as_ref()
                    .and_then(|document| document.layers.iter().find(|layer| layer.id == id))
                {
                    originals.insert(id, member.transform);
                }
            }
            let mut edit = TransformEdit::new(layer_id, box_, persistent);
            edit.group = Some(TransformGroup { r#box: box_, originals });
            self.transform_edit = Some(edit);
            return;
        }
        // An unlinked mask, when selected, transforms on its own; linked, layer and mask move together.
        let mask_alone = self.is_mask_selected && !mask_linked;
        let mut edit = TransformEdit::new(layer_id, if mask_alone { mask_transform } else { layer_transform }, persistent);
        edit.mask = mask_alone;
        self.transform_edit = Some(edit);
    }

    /// Shows `value` as the edit's draft; an invalid transform is ignored.
    pub fn preview_transform(&mut self, value: LayerTransform) {
        if !value.is_valid() {
            return;
        }
        if let Some(edit) = self.transform_edit.as_mut() {
            edit.draft = value;
        }
    }

    /// Option-drag duplicates selected roots with their descendants and drags the copies.
    pub fn begin_duplicate_transform(&mut self) {
        if self.transform_duplicate.is_some() {
            return;
        }
        let Some(primary) = self.active_layer_id else { return };
        self.commit_transform();
        if !self.can_transform() {
            return;
        }
        let selection = self.selected_layer_ids.clone();
        // Bottom to top, so the copies keep the order they had.
        let mut carried: FxHashSet<Id> = FxHashSet::default();
        for id in &selection {
            carried.extend(self.descendant_ids(*id));
        }
        let targets: Vec<Id> = self
            .document
            .as_ref()
            .map(|document| {
                document
                    .layers
                    .iter()
                    .filter(|layer| selection.contains(&layer.id) && !carried.contains(&layer.id))
                    .map(|layer| layer.id)
                    .collect()
            })
            .unwrap_or_default();
        if targets.is_empty() {
            return;
        }
        self.begin_edit(if targets.len() > 1 { "Duplicate Layers" } else { "Duplicate Layer" });
        // Stacked as Duplicate Layer stacks them: several together above the topmost original.
        self.duplicate_layers(&targets, if targets.len() > 1 { "Duplicate Layers" } else { "Duplicate Layer" });
        let copies: FxHashSet<Id> = self
            .selected_layer_ids
            .difference(&selection)
            .copied()
            .collect();
        if copies.is_empty() {
            self.end_edit();
            self.select_layers(selection.iter().copied(), Some(primary));
            return;
        }
        self.transform_duplicate = Some(TransformDuplicate {
            copies: copies.iter().copied().collect(),
            source: selection,
            primary: Some(primary),
        });
        let active = self.active_layer_id;
        self.select_layers(copies.iter().copied(), active);
        self.begin_transform(false);
    }

    /// Applies the pending transform, as one undo step (`Transform Layer`, a group's
    /// `Transform Layers`, a distortion's `Distort`/`Distort Layers`, a mask's
    /// `Transform Layer Mask`/`Distort Layer Mask`).
    pub fn commit_transform(&mut self) {
        self.snap_guides = (Vec::new(), Vec::new());
        self.blend_preview = None;
        self.finish_opacity_edit();
        let Some(edit) = self.transform_edit.take() else { return };
        self.apply_transform(edit);
        // Swift's `defer`: an Option-drag's outer duplicate edit closes with this one, on every path.
        if self.transform_duplicate.is_some() {
            self.transform_duplicate = None;
            self.end_edit();
        }
    }

    /// The body of [`EditorSession::commit_transform`], run once the edit has been taken off the
    /// session (Swift cleared `transformEdit` before dispatching in the same way).
    fn apply_transform(&mut self, edit: TransformEdit) {
        if let Some(floating) = edit.floating.clone() {
            // Unchanged: restore exactly, so soft selection edges never pick up a seam.
            if edit.draft == floating.original && edit.corners.is_none() {
                self.cancel_floating_transform(&floating);
            } else {
                self.merge_floating_transform(&edit, &floating);
            }
            return;
        }
        if edit.mask {
            self.commit_mask_transform(&edit);
            return;
        }
        if let Some(corners) = edit.corners.clone() {
            self.commit_distort(&edit, &corners);
            return;
        }
        if let Some(group) = edit.group.clone() {
            if !edit.draft.is_valid() {
                return;
            }
            self.begin_edit("Transform Layers");
            for (id, original) in &group.originals {
                let Some(index) = self
                    .document
                    .as_ref()
                    .and_then(|document| document.layers.iter().position(|layer| layer.id == *id))
                else {
                    continue;
                };
                let moved = original.following(group.r#box, edit.draft);
                if !moved.is_valid() {
                    continue;
                }
                let placement = self.document.as_ref().and_then(|document| {
                    document.layers[index]
                        .mask
                        .as_ref()
                        .map(|mask| mask.placement_moving_layer(original, &moved))
                });
                if let Some(document) = self.document.as_mut() {
                    if let Some(placement) = placement {
                        if let Some(mask) = document.layers[index].mask.as_mut() {
                            mask.placement = placement;
                        }
                    }
                    document.layers[index].transform = moved;
                }
                self.redraw_shape(index);
            }
            self.end_edit();
            return;
        }
        if !edit.draft.is_valid() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        self.begin_edit("Transform Layer");
        let placement = self.document.as_ref().and_then(|document| {
            let layer = &document.layers[index];
            layer.mask.as_ref().map(|mask| mask.placement_moving_layer(&layer.transform, &edit.draft))
        });
        if let Some(document) = self.document.as_mut() {
            if let Some(placement) = placement {
                if let Some(mask) = document.layers[index].mask.as_mut() {
                    mask.placement = placement;
                }
            }
            document.layers[index].transform = edit.draft;
        }
        self.redraw_shape(index);
        self.end_edit();
    }

    /// Cancels the pending transform; an Option-drag's copies are taken away again.
    pub fn cancel_transform(&mut self) {
        self.snap_guides = (Vec::new(), Vec::new());
        let Some(edit) = self.transform_edit.take() else { return };
        if let Some(duplicate) = self.transform_duplicate.take() {
            let mut removed: FxHashSet<Id> = duplicate.copies.iter().copied().collect();
            for copy in &duplicate.copies {
                removed.extend(self.descendant_ids(*copy));
            }
            if let Some(document) = self.document.as_mut() {
                document.layers.retain(|layer| !removed.contains(&layer.id));
            }
            self.collapsed_group_ids.retain(|id| !removed.contains(id));
            self.select_layers(duplicate.source.iter().copied(), duplicate.primary);
            self.end_edit();
        }
        if let Some(floating) = edit.floating {
            self.cancel_floating_transform(&floating);
        }
    }

    /// Moves the pending transform (or the active layer, opening and applying a drag's edit around
    /// it) by `dx`/`dy` document pixels.
    pub fn nudge_layer(&mut self, dx: f64, dy: f64) {
        let already_editing = self.transform_edit.is_some();
        if !already_editing {
            self.begin_transform(false);
        }
        let Some(mut value) = self.transform_edit.as_ref().map(|edit| edit.draft) else { return };
        value.origin.x += dx;
        value.origin.y += dy;
        self.preview_transform(value);
        if let Some(corners) = self.transform_edit.as_ref().and_then(|edit| edit.corners.clone()) {
            let moved: Vec<Point> = corners
                .iter()
                .map(|corner| Point::new(corner.x + dx, corner.y + dy))
                .collect();
            self.preview_corners(&moved);
        }
        if !already_editing {
            self.commit_transform();
        }
    }

    /// The canvas's and the Layers panel's arrow-key branch: step the layer 1 px, or 10 px with
    /// Shift.
    pub fn nudge_layer_by_key(&mut self, key: ArrowKey, shift: bool) {
        let (dx, dy) = key.step(shift);
        self.nudge_layer(dx, dy);
    }

    // MARK: - Distortion

    /// Cmd-drag on a transform handle: the corners start moving freely. Each distortion resamples
    /// the pixels, so the edit then waits for Apply rather than applying on mouse-up.
    pub fn begin_distort(&mut self) {
        let Some(edit) = self.transform_edit.as_ref() else { return };
        if edit.corners.is_some() || !edit.draft.is_valid() {
            return;
        }
        let mut next = TransformEdit::new(edit.layer_id, edit.draft, true);
        next.floating = edit.floating.clone();
        next.corners = Some(DistortWarp::corners(&edit.draft).to_vec());
        next.mask = edit.mask;
        next.group = edit.group.clone();
        self.transform_edit = Some(next);
    }

    /// Moves the distortion's corners; a twisted or collapsed shape is ignored.
    pub fn preview_corners(&mut self, corners: &[Point]) {
        let Some(edit) = self.transform_edit.as_mut() else { return };
        if edit.corners.is_none() || !DistortWarp::is_usable(corners) {
            return;
        }
        edit.corners = Some(corners.to_vec());
    }

    /// Where a distortion takes `layer`: its transform under the edit and the corners that transform
    /// moves to — for a group, each layer by the same perspective as the box.
    pub fn distort_shape(&self, layer: &ImageLayer) -> Option<(LayerTransform, Vec<Point>)> {
        let edit = self.transform_edit.as_ref()?;
        if edit.mask {
            return None;
        }
        let shape = edit.corners.as_ref()?;
        self.distort_target(layer.id, edit, shape)
    }

    /// `distortTarget(for:edit:shape:)`: the box and corners a distortion in progress is taking the
    /// layer to, when it's taking it anywhere.
    fn distort_target(
        &self,
        layer_id: Id,
        edit: &TransformEdit,
        shape: &[Point],
    ) -> Option<(LayerTransform, Vec<Point>)> {
        let Some(group) = edit.group.as_ref() else {
            return if edit.layer_id == layer_id {
                Some((edit.draft, shape.to_vec()))
            } else {
                None
            };
        };
        let original = group.originals.get(&layer_id)?;
        let transform = original.following(group.r#box, edit.draft);
        let corners = DistortWarp::carried(&transform, &edit.draft, shape).to_vec();
        if DistortWarp::is_usable(&corners) {
            Some((transform, corners))
        } else {
            None
        }
    }

    /// The layer warped into the pending distortion, at preview size, for the canvas to draw.
    /// A layer's effects, warped into the shape a distortion in progress is making — so its stroke
    /// and shadow stay on while the corners are dragged, rather than disappearing until the
    /// distortion is applied. `image` is the layer with its effects around it (see
    /// [`LayerEffectsRenderer`]), which already includes its mask.
    /// (`distortedEffects(for:effects:inset:)`.)
    pub fn distorted_effects(
        &mut self,
        layer: &ImageLayer,
        image: &PixelImage,
        inset: f64,
    ) -> Option<(PixelImage, LayerTransform)> {
        let target = {
            let edit = self.transform_edit.as_ref()?;
            if edit.mask {
                return None;
            }
            let shape = edit.corners.as_ref()?;
            self.distort_target(layer.id, edit, shape)?
        };
        self.distorted_effects_target(layer.id, image, inset, (target.0, &target.1))
    }

    /// The same, for a distortion whose target is already known — the commit, which runs once the
    /// edit is over. (`distortedEffects(for:effects:inset:target:)`.)
    fn distorted_effects_target(
        &mut self,
        layer_id: Id,
        image: &PixelImage,
        inset: f64,
        target: (LayerTransform, &[Point]),
    ) -> Option<(PixelImage, LayerTransform)> {
        let (transform, corners) = target;
        // The effects image is the layer's box grown by its margin; its corners take the same
        // perspective. An effects raster is RGBA — the kind `LayerEffectsRenderer` renders.
        let grown = LayerEffectsRenderer::placed(&transform, image.as_rgba()?, inset);
        let carried = DistortWarp::carried(&grown, &transform, corners).to_vec();
        if let Some(cache) = self.distort_effects_cache.get(&layer_id) {
            if cache.corners == carried && same_image(&cache.image, image) {
                return cache.result.clone();
            }
        }
        let result = DistortWarp::warp(image, &grown, &carried, false, Some(2048.0)).ok();
        self.distort_effects_cache.insert(
            layer_id,
            DistortEffectsCache {
                corners: carried,
                image: image.clone(),
                result: result.clone(),
            },
        );
        result
    }

    /// The layer warped into the pending distortion, at preview size, for the canvas to draw —
    /// its mask warped with it. (`distortPreview(for:)`.)
    pub fn distort_preview(
        &mut self,
        layer: &ImageLayer,
    ) -> Option<(PixelImage, Option<PixelImage>, LayerTransform)> {
        let (image, transform, corners) = {
            let edit = self.transform_edit.as_ref()?;
            if edit.mask {
                return None;
            }
            let shape = edit.corners.as_ref()?;
            let image = layer.asset.as_ref()?.image.clone();
            let (transform, corners) = self.distort_target(layer.id, edit, shape)?;
            (image, transform, corners)
        };
        let mask = layer.mask.as_ref().and_then(LayerMask::enabled_image).cloned();
        if let Some(cache) = self.distort_preview_cache.get(&layer.id) {
            if cache.corners == corners
                && cache.draft == transform
                && same_image(&cache.image, &image)
                && same_optional_image(&cache.mask, &mask)
            {
                return cache.result.clone();
            }
        }
        let mut result = None;
        if let Ok((warped, warped_transform)) =
            DistortWarp::warp(&image, &transform, &corners, false, Some(2048.0))
        {
            let owned = layer.mask.as_ref();
            let width = warped.width();
            let height = warped.height();
            let covers_layer = owned.map_or(true, |mask| mask.placement.is_none() && mask.is_linked);
            let warped_mask = if covers_layer {
                // A mask covering the layer warps like its pixels; no mask at all leaves none.
                mask.as_ref().and_then(|mask| {
                    DistortWarp::warp(mask, &transform, &corners, true, Some(2048.0))
                        .ok()
                        .map(|(image, _)| image)
                })
            } else {
                // A linked mask placed apart takes the same perspective over its own bounds; its
                // warp failing (or the shape not being one a perspective can take) leaves the
                // mask where it is, as the Swift's `else if` falls through.
                let placed_apart = owned.and_then(|owned| {
                    if !owned.is_linked {
                        return None;
                    }
                    let placed = owned.placement?;
                    let placement = placed.following(layer.transform, transform);
                    let carried = DistortWarp::carried(&placement, &transform, &corners);
                    if !DistortWarp::is_convex(&carried) {
                        return None;
                    }
                    let (moved_image, moved_transform) = DistortWarp::warp_mask(
                        &owned.asset.image,
                        &placement,
                        &carried,
                        mask_background(&owned.asset.thumbnail),
                        Some(2048.0),
                    )
                    .ok()?;
                    let moved = LayerMask {
                        asset: ImportedImage::new(
                            moved_image,
                            owned.asset.thumbnail.clone(),
                            owned.asset.name.clone(),
                        ),
                        is_enabled: owned.is_enabled,
                        placement: None,
                        is_linked: true,
                    };
                    Some(moved.clip_image(
                        Some(&moved_transform),
                        &warped_transform,
                        width,
                        height,
                        Some(2048.0),
                    ))
                });
                if let Some(clip) = placed_apart {
                    clip
                } else {
                    // An unlinked mask stays where it is on the document.
                    owned.and_then(|owned| {
                        let placement = owned.placement.unwrap_or(layer.transform);
                        owned.clip_image(
                            Some(&placement),
                            &warped_transform,
                            width,
                            height,
                            Some(2048.0),
                        )
                    })
                }
            };
            result = Some((warped, warped_mask, warped_transform));
        }
        self.distort_preview_cache.insert(
            layer.id,
            DistortPreviewCache {
                corners,
                draft: transform,
                image,
                mask,
                result: result.clone(),
            },
        );
        result
    }

    /// Apply for a distortion: each distorted layer's pixels and mask are resampled into its shape,
    /// as one undo step.
    pub fn commit_distort(&mut self, edit: &TransformEdit, shape: &[Point]) {
        self.distort_preview_cache.clear();
        let ids: Vec<Id> = match edit.group.as_ref() {
            Some(group) => group.originals.keys().copied().collect(),
            None => vec![edit.layer_id],
        };
        self.begin_edit(if edit.group.is_none() { "Distort" } else { "Distort Layers" });
        for id in ids {
            let Some(index) = self.document.as_ref().and_then(|document| {
                document.layers.iter().position(|layer| layer.id == id)
            }) else {
                continue;
            };
            let Some((transform, corners)) = self.distort_target(id, edit, shape) else {
                continue;
            };
            // The effects warped for this distortion are already in hand: keep showing them until
            // the worker has rendered the effects for the layer's new pixels, or they blink off
            // for a frame on Apply.
            let rendered = self.effects_previews.rendered(id);
            let warped_effects = rendered.and_then(|(image, inset, _)| {
                self.distorted_effects_target(id, &PixelImage::Rgba(image), inset, (transform, &corners))
            });
            if let Err(error) = self.distort_at(index, transform, &corners) {
                self.brush_error = Some(error.to_string());
            }
            // Placed where it was warped to: applying a distortion also crops the layer, so the
            // margins around it are no longer even and an inset could not put it back in the
            // right place.
            if let Some((image, placement)) = warped_effects {
                // The effects warp renders RGBA (`is_mask: false`), the only kind the seed takes.
                if let PixelImage::Rgba(image) = image {
                    self.effects_previews.seed(id, image, placement);
                }
            }
        }
        self.end_edit();
        self.distort_effects_cache.clear();
    }

    /// The layer at `index`, shown by `transform`, resampled so its corners land on `corners`.
    /// (`distort(at:transform:corners:)`.)
    fn distort_at(&mut self, index: usize, transform: LayerTransform, corners: &[Point]) -> Result<(), CoreError> {
        let Some((image, name, layer_transform)) = self.document.as_ref().and_then(|document| {
            let layer = &document.layers[index];
            layer
                .asset
                .as_ref()
                .map(|asset| (asset.image.clone(), layer.name.clone(), layer.transform))
        }) else {
            return Ok(());
        };
        let (warped_image, warped_transform, crop) = DistortWarp::warp_trimmed(&image, &transform, corners)?;
        // A layer's pixels are RGBA, so the thumbnail is a byte copy for anything of ordinary size.
        let thumbnail = match warped_image.as_rgba() {
            Some(rgba) => PixelImage::Rgba(Arc::new(PixelAdjust::thumbnail(rgba))),
            None => warped_image.clone(),
        };
        let asset = ImportedImage::new(warped_image, thumbnail, name);
        let mut mask_change: Option<MaskChange> = None;
        if let Some(original) = self.document.as_ref().and_then(|document| document.layers[index].mask.as_ref()) {
            if original.placement.is_none() && original.is_linked {
                let (mask_image, _transform) = DistortWarp::warp(&original.asset.image, &transform, corners, true, None)?;
                let mask_asset = if same_image(&mask_image, &original.asset.image) {
                    original.asset.clone()
                } else {
                    let Some(cropped) = mask_image.cropped(crop) else {
                        return Err(CoreError::Message(
                            "The canvas could not be rendered. Try a smaller canvas.".into(),
                        ));
                    };
                    LayerMask::asset(cropped).map_err(|error| CoreError::Message(error.to_string()))?
                };
                mask_change = Some(MaskChange::Replace(LayerMask {
                    asset: mask_asset,
                    is_enabled: original.is_enabled,
                    placement: original.placement,
                    is_linked: original.is_linked,
                }));
            } else if original.is_linked {
                if let Some(placed) = original.placement {
                    let placement = placed.following(layer_transform, warped_transform);
                    let carried = DistortWarp::carried(&placement, &warped_transform, corners);
                    if DistortWarp::is_convex(&carried) {
                        // A linked mask placed apart takes the same perspective over its own bounds.
                        let background = mask_background(&original.asset.thumbnail);
                        let (moved_image, moved_transform) =
                            DistortWarp::warp_mask(&original.asset.image, &placement, &carried, background, None)?;
                        let mask_asset = if same_image(&moved_image, &original.asset.image) {
                            original.asset.clone()
                        } else {
                            LayerMask::asset(moved_image)?
                        };
                        mask_change = Some(MaskChange::Replace(LayerMask {
                            asset: mask_asset,
                            is_enabled: original.is_enabled,
                            placement: Some(moved_transform),
                            is_linked: true,
                        }));
                    } else {
                        // The shape is not one a perspective warp can take: the mask keeps its place.
                        mask_change = Some(MaskChange::SetPlacement(Some(placed)));
                    }
                } else {
                    mask_change = Some(MaskChange::SetPlacement(Some(layer_transform)));
                }
            } else {
                // An unlinked mask keeps its place on the document.
                mask_change = Some(MaskChange::SetPlacement(Some(original.placement.unwrap_or(layer_transform))));
            }
        }
        if let Some(document) = self.document.as_mut() {
            let layer = &mut document.layers[index];
            layer.asset = Some(asset);
            layer.transform = warped_transform;
            match mask_change {
                Some(MaskChange::Replace(mask)) => layer.mask = Some(mask),
                Some(MaskChange::SetPlacement(placement)) => {
                    if let Some(mask) = layer.mask.as_mut() {
                        mask.placement = placement;
                    }
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Apply for an unlinked mask transformed on its own: it takes the new placement (its pixels
    /// untouched); a distorted one is resampled into the shape.
    pub fn commit_mask_transform(&mut self, edit: &TransformEdit) {
        self.mask_distort_preview_cache = None;
        if !edit.draft.is_valid() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == edit.layer_id))
        else {
            return;
        };
        let Some((layer_transform, mask_image, mask_thumbnail, mask_placement)) =
            self.document.as_ref().and_then(|document| {
                let layer = &document.layers[index];
                layer.mask.as_ref().map(|mask| {
                    (
                        layer.transform,
                        mask.asset.image.clone(),
                        mask.asset.thumbnail.clone(),
                        mask.placement,
                    )
                })
            })
        else {
            return;
        };
        if let Some(corners) = edit.corners.clone() {
            // A distorted mask is resampled into the shape, over the shape's bounds, its background
            // outside it.
            let background = mask_background(&mask_thumbnail);
            match DistortWarp::warp_mask(&mask_image, &edit.draft, &corners, background, None) {
                Ok((moved_image, moved_transform)) => {
                    let asset = if same_image(&moved_image, &mask_image) {
                        self.document
                            .as_ref()
                            .and_then(|document| document.layers[index].mask.as_ref())
                            .map(|mask| mask.asset.clone())
                    } else {
                        match LayerMask::asset(moved_image) {
                            Ok(asset) => Some(asset),
                            Err(error) => {
                                self.brush_error = Some(error.to_string());
                                return;
                            }
                        }
                    };
                    let Some(asset) = asset else { return };
                    self.finish_opacity_edit();
                    self.begin_edit("Distort Layer Mask");
                    let placement = if moved_transform.same_placement(layer_transform) {
                        None
                    } else {
                        Some(moved_transform)
                    };
                    if let Some(document) = self.document.as_mut() {
                        if let Some(mask) = document.layers[index].mask.as_mut() {
                            let is_enabled = mask.is_enabled;
                            let is_linked = mask.is_linked;
                            *mask = LayerMask { asset, is_enabled, placement, is_linked };
                        }
                    }
                    self.end_edit();
                }
                Err(error) => self.brush_error = Some(error.to_string()),
            }
            return;
        }
        let placement = if edit.draft.same_placement(layer_transform) {
            None
        } else {
            Some(edit.draft)
        };
        if placement == mask_placement {
            return;
        }
        self.finish_opacity_edit();
        self.begin_edit("Transform Layer Mask");
        if let Some(document) = self.document.as_mut() {
            if let Some(mask) = document.layers[index].mask.as_mut() {
                mask.placement = placement;
            }
        }
        self.end_edit();
    }

    /// Where `layer`'s mask shows right now — nil while it covers the layer's (displayed) pixel grid:
    /// a pending mask transform's draft, or where a pending layer transform leaves it (carried along
    /// when linked).
    pub fn displayed_mask_placement(&self, layer: &ImageLayer) -> Option<LayerTransform> {
        let mask = layer.mask.as_ref()?;
        // Content-Aware Fill previewing on a grown layer: the mask keeps covering the layer's old bounds.
        if let Some(edit) = self.filter_edit.as_ref() {
            if edit.prepared_transform.is_some() && edit.preview_image(layer.id).is_some() {
                return mask.placement.or(Some(layer.transform));
            }
        }
        if let Some(edit) = self.transform_edit.as_ref() {
            if let Some(group) = edit.group.as_ref() {
                let Some(original) = group.originals.get(&layer.id) else {
                    return mask.placement;
                };
                if edit.corners.is_some() {
                    return if mask.is_linked && mask.placement.is_none() {
                        None
                    } else {
                        mask.placement.or(Some(layer.transform))
                    };
                }
                return mask.placement_moving_layer(&layer.transform, &original.following(group.r#box, edit.draft));
            }
            if edit.layer_id == layer.id && edit.floating.is_none() {
                if edit.mask {
                    return if edit.draft.same_placement(layer.transform) {
                        None
                    } else {
                        Some(edit.draft)
                    };
                }
                // A distortion carries a mask covering a linked layer with it; any other mask stays put.
                if edit.corners.is_some() {
                    return if mask.is_linked && mask.placement.is_none() {
                        None
                    } else {
                        mask.placement.or(Some(layer.transform))
                    };
                }
                return mask.placement_moving_layer(&layer.transform, &edit.draft);
            }
        }
        mask.placement
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::buffer::{Gray8Image, Rgba8Image, SharedImage};
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::geom::Rect;
    use compositor_rs_core::layer_transform::TransformSnap;

    fn box_(x: f64, y: f64, width: f64, height: f64) -> LayerTransform {
        LayerTransform {
            origin: Point::new(x, y),
            size: Size::new(width, height),
            ..Default::default()
        }
    }

    // MARK: Distortion corners

    #[test]
    fn a_distorted_corner_moves_alone() {
        let corners = DistortWarp::corners(&box_(0.0, 0.0, 100.0, 50.0));
        let drag = compositor_rs_core::layer_transform::TransformDrag {
            original: box_(0.0, 0.0, 100.0, 50.0),
            start: Point::new(0.0, 0.0),
            mode: TransformDragMode::Distort(0),
            original_corners: Some(corners.to_vec()),
        };
        let moved = drag.corners(Point::new(10.0, 20.0), false).expect("a distorting drag");
        assert_eq!(moved[0], Point::new(10.0, 20.0));
        assert_eq!(moved[1], corners[1]);
        assert_eq!(moved[2], corners[2]);
        assert_eq!(moved[3], corners[3]);
    }

    #[test]
    fn a_distorted_edge_moves_both_of_its_corners() {
        let corners = DistortWarp::corners(&box_(0.0, 0.0, 100.0, 50.0));
        let drag = compositor_rs_core::layer_transform::TransformDrag {
            original: box_(0.0, 0.0, 100.0, 50.0),
            start: Point::new(50.0, 0.0),
            mode: TransformDragMode::Distort(1),
            original_corners: Some(corners.to_vec()),
        };
        let moved = drag.corners(Point::new(55.0, 5.0), false).expect("a distorting drag");
        assert_eq!(moved[0], Point::new(5.0, 5.0));
        assert_eq!(moved[1], Point::new(105.0, 5.0));
        assert_eq!(moved[2], corners[2]);
        assert_eq!(moved[3], corners[3]);
    }

    #[test]
    fn shift_keeps_a_distorted_corner_on_one_axis() {
        let corners = DistortWarp::corners(&box_(0.0, 0.0, 100.0, 50.0));
        let drag = compositor_rs_core::layer_transform::TransformDrag {
            original: box_(0.0, 0.0, 100.0, 50.0),
            start: Point::new(0.0, 0.0),
            mode: TransformDragMode::Distort(0),
            original_corners: Some(corners.to_vec()),
        };
        // The larger delta wins: 30 across, 10 down — the corner slides sideways.
        assert_eq!(drag.corners(Point::new(30.0, 10.0), true).unwrap()[0], Point::new(30.0, 0.0));
        // The other way round it stays on the vertical.
        assert_eq!(drag.corners(Point::new(10.0, 30.0), true).unwrap()[0], Point::new(0.0, 30.0));
        // A body drag is locked the same way, and moves every corner.
        let moved = compositor_rs_core::layer_transform::TransformDrag {
            mode: TransformDragMode::Move,
            ..drag
        }
        .corners(Point::new(8.0, 3.0), true)
        .unwrap();
        for (index, corner) in moved.iter().enumerate() {
            assert_eq!(corner.x, corners[index].x + 8.0);
            assert_eq!(corner.y, corners[index].y);
        }
    }

    // MARK: Distortion previews

    /// The corners a distortion is dragged to: a usable, convex quad.
    fn dragged_corners() -> [Point; 4] {
        [
            Point::new(10.0, 10.0),
            Point::new(60.0, 10.0),
            Point::new(30.0, 30.0),
            Point::new(10.0, 30.0),
        ]
    }

    /// The same distortion somewhere else — another shape, so every warp keyed on it misses.
    fn moved_corners() -> [Point; 4] {
        [
            Point::new(8.0, 6.0),
            Point::new(70.0, 12.0),
            Point::new(52.0, 36.0),
            Point::new(10.0, 30.0),
        ]
    }

    /// A session with one pixel layer mid-distortion, and that layer as it was before the edit.
    fn distort_session(image: PixelImage) -> (EditorSession, ImageLayer) {
        let layer = ImageLayer::from_asset(
            ImportedImage::new(image.clone(), image, "Layer 1"),
            Point::new(10.0, 10.0),
        );
        let mut document = CanvasDocument::new(100, 60);
        document.layers = vec![layer.clone()];
        let mut session = EditorSession::default();
        session.document = Some(document);
        session.set_active_layer(Some(layer.id));
        session.begin_transform(false);
        session.begin_distort();
        session.preview_corners(&dragged_corners());
        (session, layer)
    }

    #[test]
    fn distorted_effects_reuses_a_warp_while_its_corners_and_image_stand() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let (mut session, layer) = distort_session(image.clone());
        let first = session
            .distorted_effects(&layer, &image, 2.0)
            .expect("a warped effects preview");
        let again = session
            .distorted_effects(&layer, &image, 2.0)
            .expect("a warped effects preview");
        assert!(same_image(&first.0, &again.0), "unchanged corners reuse the warp");

        // The corners move: a fresh warp.
        session.preview_corners(&moved_corners());
        let moved = session
            .distorted_effects(&layer, &image, 2.0)
            .expect("a warped effects preview");
        assert!(!same_image(&first.0, &moved.0), "changed corners warp again");

        // The same pixels under another raster are another image, as the Swift's `===` saw them.
        let other = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let restamped = session
            .distorted_effects(&layer, &other, 2.0)
            .expect("a warped effects preview");
        assert!(
            !same_image(&moved.0, &restamped.0),
            "a different raster warps again"
        );
    }

    #[test]
    fn distort_preview_reuses_a_warp_until_one_of_its_keys_changes() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let (mut session, layer) = distort_session(image.clone());
        let first = session.distort_preview(&layer).expect("a warped preview");
        assert!(first.1.is_none(), "an unmasked layer previews without a mask");
        let again = session.distort_preview(&layer).expect("a warped preview");
        assert!(same_image(&first.0, &again.0), "unchanged corners reuse the warp");

        // The corners are part of the key.
        session.preview_corners(&moved_corners());
        let moved = session.distort_preview(&layer).expect("a warped preview");
        assert!(!same_image(&first.0, &moved.0), "changed corners warp again");

        // So are the layer's pixels: equal content under another raster is another image.
        let mut other_layer = layer.clone();
        other_layer.asset = Some(ImportedImage::new(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8))),
            "Layer 1",
        ));
        let restamped = session
            .distort_preview(&other_layer)
            .expect("a warped preview");
        assert!(!same_image(&moved.0, &restamped.0), "a different raster warps again");

        // And so is the mask: the same layer and corners with a mask to warp along is a miss.
        let mut masked_layer = other_layer.clone();
        masked_layer.mask = Some(LayerMask::new(ImportedImage::new(
            PixelImage::Gray(Arc::new(Gray8Image::uniform(8, 8, 255))),
            PixelImage::Gray(Arc::new(Gray8Image::uniform(8, 8, 255))),
            "Mask",
        )));
        let masked = session
            .distort_preview(&masked_layer)
            .expect("a warped preview");
        assert!(masked.1.is_some(), "a mask covering the layer warps with it");
        assert!(!same_image(&restamped.0, &masked.0), "a new mask warps again");
    }

    #[test]
    fn a_mask_that_covers_or_leaves_the_layer_still_comes_back() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let (mut session, layer) = distort_session(image);

        // Unlinked: the mask stays where it is on the document and is clipped into the warp's grid.
        let mut unlinked = layer.clone();
        let mut unlinked_mask = LayerMask::new(ImportedImage::new(
            PixelImage::Gray(Arc::new(Gray8Image::uniform(8, 8, 255))),
            PixelImage::Gray(Arc::new(Gray8Image::uniform(8, 8, 255))),
            "Mask",
        ));
        unlinked_mask.is_linked = false;
        unlinked.mask = Some(unlinked_mask);
        let preview = session.distort_preview(&unlinked).expect("a warped preview");
        assert!(preview.1.is_some(), "an unlinked mask is still drawn");

        // Linked and placed apart: it takes the perspective over its own bounds, or falls back to
        // the same clip an unlinked mask gets when that shape cannot be warped.
        session.distort_preview_cache.clear();
        let mut placed = layer.clone();
        let mut placed_mask = LayerMask::new(ImportedImage::new(
            PixelImage::Gray(Arc::new(Gray8Image::uniform(6, 6, 128))),
            PixelImage::Gray(Arc::new(Gray8Image::uniform(6, 6, 128))),
            "Mask",
        ));
        placed_mask.placement = Some(box_(12.0, 12.0, 4.0, 4.0));
        placed.mask = Some(placed_mask);
        let preview = session.distort_preview(&placed).expect("a warped preview");
        assert!(preview.1.is_some(), "a linked mask placed apart is drawn too");
    }

    #[test]
    fn the_distort_previews_refuse_what_they_do_not_draw() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let (mut session, layer) = distort_session(image.clone());

        // A layer without pixels has nothing to preview; the effects preview never reads them.
        let mut bare = layer.clone();
        bare.asset = None;
        assert!(session.distort_preview(&bare).is_none());

        // Without a distortion in progress there is nothing to warp.
        session.transform_edit = None;
        assert!(session.distorted_effects(&layer, &image, 0.0).is_none());
        assert!(session.distort_preview(&layer).is_none());

        // A mask edit distorts the mask itself (`maskDistortPreview` in `mask_ops.rs`), not the layer.
        let mut mask_edit = TransformEdit::new(layer.id, layer.transform, true);
        mask_edit.mask = true;
        mask_edit.corners = Some(dragged_corners().to_vec());
        session.transform_edit = Some(mask_edit);
        assert!(session.distorted_effects(&layer, &image, 0.0).is_none());
        assert!(session.distort_preview(&layer).is_none());
    }

    #[test]
    fn commit_distort_seeds_the_effects_preview_it_warped_and_clears_the_caches() {
        let image = PixelImage::Rgba(Arc::new(Rgba8Image::new(8, 8)));
        let (mut session, layer) = distort_session(image.clone());
        let effects: SharedImage = Arc::new(Rgba8Image::new(12, 12));
        session
            .effects_previews
            .seed(layer.id, effects.clone(), layer.transform);
        // Drawing once fills the preview cache the commit then drops.
        session.distort_preview(&layer).expect("a warped preview");
        let edit = session.transform_edit.clone().expect("a distortion edit");
        let corners = edit.corners.clone().expect("corners");
        session.commit_distort(&edit, &corners);

        let (seeded, inset, placement) = session
            .effects_previews
            .rendered(layer.id)
            .expect("the effects preview stays");
        assert!(
            !Arc::ptr_eq(&seeded, &effects),
            "the warped effects are seeded, not the originals"
        );
        assert_eq!(inset, 0.0, "a seed needs no inset of its own");
        assert!(placement.is_some(), "placed where they were warped to");
        assert!(
            session.distort_preview_cache.is_empty(),
            "the commit drops the preview cache"
        );
        assert!(
            session.distort_effects_cache.is_empty(),
            "and the effects cache with it"
        );
    }

    // MARK: Group transforms

    #[test]
    fn a_group_member_follows_a_plain_box_move_exactly() {
        let base = box_(10.0, 20.0, 100.0, 50.0);
        let draft = box_(40.0, 15.0, 100.0, 50.0);
        let member = box_(20.0, 30.0, 40.0, 20.0);
        let moved = member.following(base, draft);
        assert_eq!(moved.origin, Point::new(50.0, 25.0));
        assert_eq!(moved.size, member.size);
        assert_eq!(moved.rotation, member.rotation);
        assert_eq!(moved.flip_x, member.flip_x);
        assert_eq!(moved.flip_y, member.flip_y);
    }

    #[test]
    fn a_group_member_follows_a_box_resize_about_the_box() {
        let base = box_(0.0, 0.0, 100.0, 100.0);
        // Twice the size about the same center (50, 50).
        let draft = box_(-50.0, -50.0, 200.0, 200.0);
        let member = box_(50.0, 50.0, 20.0, 20.0);
        let moved = member.following(base, draft);
        // The member's center sits 60% into the box, which a doubled box puts at 70%.
        let center = moved.center();
        assert!((center.x - 70.0).abs() < 1e-9, "center x was {center:?}");
        assert!((center.y - 70.0).abs() < 1e-9, "center y was {center:?}");
        assert!((moved.size.width - 40.0).abs() < 1e-9, "width was {:?}", moved.size.width);
        assert!((moved.size.height - 40.0).abs() < 1e-9, "height was {:?}", moved.size.height);
    }

    #[test]
    fn an_untouched_box_leaves_every_member_where_it_is() {
        let base = box_(10.0, 20.0, 100.0, 50.0);
        let member = box_(20.0, 30.0, 40.0, 20.0);
        assert_eq!(member.following(base, base), member);
    }

    // MARK: Snapping

    #[test]
    fn snapping_takes_the_nearest_target_on_each_axis() {
        let rect = Rect::new(103.0, 200.0, 50.0, 20.0);
        // minX 103 is 3 from 100, midX 128 is 72 from 200 — the edge wins. minY 200 is exactly ten away
        // from 190, which is still within the tolerance.
        let snap = TransformSnap::offset(rect, &[0.0, 100.0, 200.0], &[190.0], 10.0);
        assert_eq!(snap.offset.width, -3.0);
        assert_eq!(snap.x, Some(100.0));
        assert_eq!(snap.offset.height, -10.0);
        assert_eq!(snap.y, Some(190.0));
        // Two axes are decided on their own: a target on x never drags y with it.
        let only_x = TransformSnap::offset(rect, &[100.0], &[], 10.0);
        assert_eq!(only_x.offset, Size::new(-3.0, 0.0));
        assert_eq!(only_x.x, Some(100.0));
        assert_eq!(only_x.y, None);
    }

    #[test]
    fn snapping_ignores_targets_beyond_the_tolerance() {
        let rect = Rect::new(110.001, 0.0, 50.0, 20.0);
        let snap = TransformSnap::offset(rect, &[100.0], &[], 10.0);
        assert_eq!(snap.offset.width, 0.0);
        assert_eq!(snap.x, None);
        // A move exactly on the tolerance still snaps.
        let at_limit = Rect::new(110.0, 0.0, 50.0, 20.0);
        assert_eq!(TransformSnap::offset(at_limit, &[100.0], &[], 10.0).offset.width, -10.0);
    }

    #[test]
    fn snapping_keeps_the_first_target_when_two_are_equally_close() {
        // minX 100 is 10 from 90 and maxX 120 is 10 from 130: the earlier pair is kept.
        let rect = Rect::new(100.0, 0.0, 20.0, 20.0);
        let snap = TransformSnap::offset(rect, &[90.0, 130.0], &[], 10.0);
        assert_eq!(snap.offset.width, -10.0);
        assert_eq!(snap.x, Some(90.0));
    }

    // MARK: Overlay geometry

    fn viewport() -> CanvasViewport {
        // Zoom 1, no pan: the document's top-left lands at (-width / 2, -height / 2) in view points.
        CanvasViewport::default()
    }

    #[test]
    fn a_handle_press_resizes_and_the_rotation_handle_rotates() {
        let transform = box_(0.0, 0.0, 100.0, 100.0);
        let geometry = TransformOverlayGeometry::new(&transform, &viewport(), Size::new(100.0, 100.0));
        assert!(geometry.shows_rotation);
        assert_eq!(geometry.hit(geometry.handles[0]), Some(TransformDragMode::Resize(0)));
        assert_eq!(geometry.hit(geometry.handles[5]), Some(TransformDragMode::Resize(5)));
        assert_eq!(geometry.hit(geometry.rotation_handle), Some(TransformDragMode::Rotate));
        // An edge between two handles resizes by the edge's own handle index.
        let middle_of_top_edge = geometry.handles[1];
        assert_eq!(geometry.hit(middle_of_top_edge), Some(TransformDragMode::Resize(1)));
        // On an edge but out of reach of both its handles: the edge itself catches it.
        assert_eq!(geometry.hit(Point::new(-25.0, -50.0)), Some(TransformDragMode::Resize(1)));
        assert_eq!(geometry.hit(Point::new(-50.0, 25.0)), Some(TransformDragMode::Resize(7)));
        // Inside the box, away from every edge, the press moves it.
        assert_eq!(geometry.hit(Point::new(0.0, 20.0)), None);
        // Nine points away from a handle is still a hit; eleven is not.
        let near = Point::new(geometry.handles[0].x + 9.0, geometry.handles[0].y);
        assert!(geometry.hit(near).is_some());
        let far = Point::new(geometry.handles[0].x + 11.0, geometry.handles[0].y + 11.0);
        assert_eq!(geometry.hit(far), None);
    }

    #[test]
    fn a_distortion_geometry_has_no_rotation_and_midpoint_handles() {
        let corners = [
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(100.0, 50.0),
            Point::new(0.0, 50.0),
        ];
        let geometry = TransformOverlayGeometry::from_corners(&corners, &viewport(), Size::new(100.0, 50.0));
        assert!(!geometry.shows_rotation);
        assert_eq!(geometry.rotation_handle, geometry.handles[1]);
        // View points: the document's top-left is at (-50, -25) for a 100×50 document at zoom 1.
        assert_eq!(geometry.handles[0], Point::new(-50.0, -25.0));
        assert_eq!(geometry.handles[1], Point::new(0.0, -25.0));
        assert_eq!(geometry.handles[2], Point::new(50.0, -25.0));
        assert_eq!(geometry.handles[3], Point::new(50.0, 0.0));
        assert_eq!(geometry.handles[4], Point::new(50.0, 25.0));
        assert_eq!(geometry.handles[5], Point::new(0.0, 25.0));
        assert_eq!(geometry.handles[6], Point::new(-50.0, 25.0));
        assert_eq!(geometry.handles[7], Point::new(-50.0, 0.0));
        // Pressing the rotation handle now falls through to nothing: it is not shown.
        assert_eq!(geometry.hit(geometry.rotation_handle), Some(TransformDragMode::Resize(1)));
    }

    #[test]
    fn the_handle_cursor_direction_follows_the_box_angle() {
        // A square box runs at 45°: the top-left corner rounds to the third position, the top edge's
        // to the fourth, the right edge's to the second, the top-right corner's to the first.
        let mut rotated = box_(0.0, 0.0, 100.0, 100.0);
        rotated.rotation = 45.0;
        let geometry = TransformOverlayGeometry::new(&rotated, &viewport(), Size::new(100.0, 100.0));
        assert_eq!(geometry.resize_direction(0), ResizeDirection::Bottom);
        assert_eq!(geometry.resize_direction(1), ResizeDirection::TopRight);
        assert_eq!(geometry.resize_direction(3), ResizeDirection::BottomRight);
        assert_eq!(geometry.resize_direction(2), ResizeDirection::Right);
    }

    // MARK: Arrow-key stepping

    #[test]
    fn the_arrow_keys_step_one_pixel_or_ten_with_shift() {
        assert_eq!(ArrowKey::from_key_code(123), Some(ArrowKey::Left));
        assert_eq!(ArrowKey::from_key_code(124), Some(ArrowKey::Right));
        assert_eq!(ArrowKey::from_key_code(125), Some(ArrowKey::Down));
        assert_eq!(ArrowKey::from_key_code(126), Some(ArrowKey::Up));
        assert_eq!(ArrowKey::from_key_code(49), None);
        assert_eq!(ArrowKey::Left.step(false), (-1.0, 0.0));
        assert_eq!(ArrowKey::Right.step(false), (1.0, 0.0));
        assert_eq!(ArrowKey::Up.step(false), (0.0, -1.0));
        assert_eq!(ArrowKey::Down.step(false), (0.0, 1.0));
        assert_eq!(ArrowKey::Left.step(true), (-10.0, 0.0));
        assert_eq!(ArrowKey::Right.step(true), (10.0, 0.0));
        assert_eq!(ArrowKey::Up.step(true), (0.0, -10.0));
        assert_eq!(ArrowKey::Down.step(true), (0.0, 10.0));
    }

    // MARK: Resampling identity

    #[test]
    fn an_image_is_only_itself() {
        let image = PixelImage::Gray(Arc::new(compositor_rs_core::buffer::Gray8Image::new(2, 2)));
        let other = PixelImage::Gray(Arc::new(compositor_rs_core::buffer::Gray8Image::new(2, 2)));
        assert!(same_image(&image, &image));
        assert!(!same_image(&image, &other));
    }
}
