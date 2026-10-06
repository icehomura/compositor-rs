//! Layer and folder commands: grouping and ungrouping, nesting and placing, merging, flipping,
//! duplicating, whole-layer copy/paste and the appearance edits — opacity and blend mode — that
//! preview live on the canvas.
//!
//! Ported from the `extension EditorSession` parts of `Document/LayerGroups.swift`,
//! `Document/LayerMerge.swift`, `Document/LayerFlip.swift`, `Document/LayerAppearance.swift` and
//! the layer half of `Document/SelectionClipboard.swift`.
//!
//! The heavy lifting the Swift kept inside the session — the panel's row list, folder opacity and
//! the cached drawing order — is `compositor_core::groups`. What is left, and lives here, is the
//! list arithmetic: it is written as free functions over `&[ImageLayer]` so the ordering rules can
//! be pinned without a session. The live-mask side of deleting and clipping lives in
//! [`crate::live_mask`] (`delete_with_live_mask_choice`, `finish_deleting_layer(s)`,
//! `link_mask`/`remove_live_mask`, `adopt_clipping`/`release_detached_clipping`), which this module
//! calls while rearranging layers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use compositor_core::blend::LayerBlendMode;
use compositor_core::document::{CanvasDocument, ImageLayer, ProjectLayerRecord};
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::groups::{LayerHierarchy, LayerHierarchyEntry, LayerOrder};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_transform::LayerTransform;
use compositor_core::limits::{document_budget_megapixels, document_pixel_budget};
use compositor_core::{new_id, Id};
use compositor_pixels::canvas::Canvas;
use compositor_pixels::filters::PixelFilter;
use compositor_render::composite::Composite;

use crate::clipboard::rgba_thumbnail;
use crate::projects::SessionHost;
use crate::session::EditorSession;

/// Every layer inside `id`, folders included — `descendantIDs(of:)`.
pub fn descendants(layers: &[ImageLayer], id: Id) -> HashSet<Id> {
    let mut children: HashMap<Option<Id>, Vec<Id>> = HashMap::new();
    for layer in layers {
        children.entry(layer.parent_id).or_default().push(layer.id);
    }
    let mut result: HashSet<Id> = HashSet::new();
    let mut pending = vec![id];
    while let Some(parent) = pending.pop() {
        if let Some(ids) = children.get(&Some(parent)) {
            for child in ids {
                if result.insert(*child) {
                    pending.push(*child);
                }
            }
        }
    }
    result
}

/// `IndexSet.move(fromOffsets:toOffset:)`: the rows at `offsets` come out immediately before the
/// row that was at `destination` in the list as it was before the move (`destination == rows.len()`
/// puts them at the end). `offsets` must be ascending and unique, as `IndexSet` keeps them, and all
/// within the list; `None` otherwise.
fn moved_rows<T: Clone>(rows: &[T], offsets: &[usize], destination: usize) -> Option<Vec<T>> {
    if destination > rows.len() || offsets.len() > rows.len() {
        return None;
    }
    if offsets.iter().any(|offset| *offset >= rows.len()) || !offsets.windows(2).all(|pair| pair[0] < pair[1]) {
        return None;
    }
    let insertion = destination - offsets.iter().filter(|offset| **offset < destination).count();
    let moved: Vec<T> = offsets.iter().map(|offset| rows[*offset].clone()).collect();
    let remaining: Vec<T> = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| !offsets.contains(index))
        .map(|(_, row)| row.clone())
        .collect();
    let mut result = Vec::with_capacity(rows.len());
    result.extend_from_slice(&remaining[..insertion]);
    result.extend(moved);
    result.extend_from_slice(&remaining[insertion..]);
    Some(result)
}

/// `reorderLayers(fromOffsets:toOffset:)`'s conversion: the panel's rows read top-to-bottom and the
/// compositor's `layers` bottom-to-top, so the move runs on the reversed list and the result is
/// reversed back.
pub fn reordered_layers(layers: &[ImageLayer], offsets: &[usize], destination: usize) -> Option<Vec<ImageLayer>> {
    let mut top_first: Vec<ImageLayer> = layers.iter().rev().cloned().collect();
    top_first = moved_rows(&top_first, offsets, destination)?;
    Some(top_first.into_iter().rev().collect())
}

/// `placeLayer(_:in:above:atBottom:)`'s list arithmetic: `id` moves under `parent` (nil for the
/// root), just above `above` within that parent, or to the very bottom; everything else keeps the
/// order it had. Clipping is adopted and released exactly as `placeLayer` does it, and the result
/// must still pass [`LayerHierarchy::validate`] — a move that would nest a folder past 64 levels,
/// or under itself, is refused with `None`.
///
/// A folder moved somewhere keeps its contents: only the folder's own row moves.
pub fn placed_layers(
    layers: &[ImageLayer],
    id: Id,
    parent: Option<Id>,
    above: Option<Id>,
    at_bottom: bool,
) -> Option<Vec<ImageLayer>> {
    if above == Some(id) {
        return None;
    }
    let index = layers.iter().position(|layer| layer.id == id)?;
    let mut next = layers.to_vec();
    let mut layer = next.remove(index);
    layer.parent_id = parent;
    let mut insertion = if at_bottom { 0 } else { next.len() };
    if let Some(target) = above {
        let target_index = next
            .iter()
            .position(|layer| layer.id == target && layer.parent_id == parent)?;
        insertion = target_index + 1;
    }
    next.insert(insertion, layer);
    EditorSession::adopt_clipping(id, &mut next);
    EditorSession::release_detached_clipping(&mut next);
    let records: Vec<ProjectLayerRecord> = next.iter().map(ImageLayer::hierarchy_record).collect();
    LayerHierarchy::validate(&records).ok()?;
    Some(next)
}

/// `canPlaceLayer(_:in:)`: the layer must exist, and the folder must be a folder that is neither the
/// layer nor inside it.
pub fn can_place_in(layers: &[ImageLayer], id: Id, parent: Option<Id>) -> bool {
    if !layers.iter().any(|layer| layer.id == id) {
        return false;
    }
    let Some(parent) = parent else { return true };
    if parent == id || descendants(layers, id).contains(&parent) {
        return false;
    }
    layers.iter().any(|layer| layer.id == parent && layer.is_group)
}

/// Where a Layers-panel drop lands: the folder the layers go into, the row they go just above, or the
/// very bottom of the list (the port of the conversion in `NativeLayerList`'s drop handler).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerDropPlacement {
    pub parent: Option<Id>,
    pub above: Option<Id>,
    pub at_bottom: bool,
}

/// The Layers panel's drop conversion: `row` is the row the pointer is on in the top-first row list
/// (`LayerHierarchy::entries(…, top_first: true, …)`), and `into_folder` says the drop was an `.on`
/// drop aimed at that row's folder rather than an `.above` one. A row past the end is the very
/// bottom; `None` for a row the list does not have.
pub fn drop_placement(rows: &[LayerHierarchyEntry], row: usize, into_folder: bool) -> Option<LayerDropPlacement> {
    if into_folder {
        let layer = rows.get(row)?;
        return Some(LayerDropPlacement {
            parent: Some(layer.layer.id),
            above: None,
            at_bottom: false,
        });
    }
    if row >= rows.len() {
        return (row == rows.len()).then_some(LayerDropPlacement {
            parent: None,
            above: None,
            at_bottom: true,
        });
    }
    let target = &rows[row].layer;
    Some(LayerDropPlacement {
        parent: target.parent_id,
        above: Some(target.id),
        at_bottom: false,
    })
}

/// What ⌘E merges, in stacking order, and where the result goes (`mergePlan()`).
#[derive(Clone, Debug, PartialEq)]
pub struct MergePlan {
    pub ids: Vec<Id>,
    pub removed: HashSet<Id>,
    pub name: String,
    pub parent: Option<Id>,
    pub anchor: Id,
    pub action: &'static str,
}

/// `ancestors(_:)`: `id`'s folder chain, immediate parent first and `None` (the root) last.
fn ancestors(by_id: &HashMap<Id, &ImageLayer>, id: Id) -> Vec<Option<Id>> {
    let mut result = Vec::new();
    let mut parent = by_id.get(&id).and_then(|layer| layer.parent_id);
    while let Some(current) = parent {
        result.push(Some(current));
        parent = by_id.get(&current).and_then(|layer| layer.parent_id);
    }
    result.push(None);
    result
}

/// A new folder with the layers `Group Selected Layers` puts inside it.
#[derive(Clone, Debug, PartialEq)]
pub struct Grouping {
    pub layers: Vec<ImageLayer>,
    pub group: Id,
    /// The folder the new group itself went into; the panel expands it (it leaves
    /// `collapsedGroupIDs`).
    pub parent: Option<Id>,
}

/// `groupSelectedLayers()`'s list arithmetic: a new folder, named after the first free `Folder N`,
/// takes the selected layers (a selected folder carries its subtree, and selected descendants are
/// not pulled out of it) and lands in the common parent at the topmost selected branch's place.
/// `None` when the result would be an invalid hierarchy.
pub fn grouped_layers(layers: &[ImageLayer], selected: &HashSet<Id>, size: Size) -> Option<Grouping> {
    let by_id: HashMap<Id, &ImageLayer> = layers.iter().map(|layer| (layer.id, layer)).collect();
    let selected: HashSet<Id> = selected.iter().copied().filter(|id| by_id.contains_key(id)).collect();
    // A selected folder carries its subtree; selected descendants must not be pulled out of it.
    let root_ids: HashSet<Id> = selected
        .iter()
        .copied()
        .filter(|id| {
            !ancestors(&by_id, *id)
                .iter()
                .any(|candidate| candidate.is_some_and(|parent| selected.contains(&parent)))
        })
        .collect();
    let ordered: Vec<Id> = LayerOrder::resolve(layers)
        .order
        .into_iter()
        .filter(|id| root_ids.contains(id))
        .collect();
    let parent: Option<Id> = ordered.first().and_then(|first| {
        ancestors(&by_id, *first)
            .into_iter()
            .find(|candidate| ordered.iter().all(|id| ancestors(&by_id, *id).contains(candidate)))
            .flatten()
    });
    let names: HashSet<&str> = layers.iter().map(|layer| layer.name.as_str()).collect();
    let mut number = 1;
    while names.contains(format!("Folder {number}").as_str()) {
        number += 1;
    }
    let mut group = ImageLayer::blank(format!("Folder {number}"), size);
    group.is_group = true;
    group.parent_id = parent;
    let group_id = group.id;
    // Put the wrapper at the topmost selected branch in the common parent.
    let branches: Vec<Id> = ordered
        .iter()
        .map(|id| {
            let mut branch = *id;
            while let Some(next) = by_id.get(&branch).and_then(|layer| layer.parent_id) {
                if Some(next) == parent {
                    break;
                }
                branch = next;
            }
            branch
        })
        .collect();
    let highest = layers.iter().rposition(|layer| branches.contains(&layer.id));
    let insertion = highest
        .map(|index| {
            layers[..=index]
                .iter()
                .filter(|layer| !root_ids.contains(&layer.id))
                .count()
        })
        .unwrap_or(layers.len());
    let mut result: Vec<ImageLayer> = layers
        .iter()
        .filter(|layer| !root_ids.contains(&layer.id))
        .cloned()
        .collect();
    result.insert(insertion.min(result.len()), group);
    for id in &ordered {
        if let Some(child) = by_id.get(id) {
            let mut child = (*child).clone();
            child.parent_id = Some(group_id);
            result.push(child);
        }
    }
    let records: Vec<ProjectLayerRecord> = result.iter().map(ImageLayer::hierarchy_record).collect();
    LayerHierarchy::validate(&records).ok()?;
    Some(Grouping {
        layers: result,
        group: group_id,
        parent,
    })
}

/// `ungroupLayers()`'s list arithmetic: the folder's direct children take its place among its own
/// siblings, in the order they had inside it, and the folder goes. Returns the layers and the
/// children's ids, in the order they are now listed.
pub fn ungrouped_layers(layers: &[ImageLayer], group_id: Id) -> Option<(Vec<ImageLayer>, Vec<Id>)> {
    let group = layers.iter().find(|layer| layer.id == group_id)?;
    let child_ids: HashSet<Id> = layers
        .iter()
        .filter(|layer| layer.parent_id == Some(group_id))
        .map(|layer| layer.id)
        .collect();
    let mut children: Vec<ImageLayer> = layers
        .iter()
        .filter(|layer| child_ids.contains(&layer.id))
        .cloned()
        .collect();
    for child in children.iter_mut() {
        child.parent_id = group.parent_id;
    }
    let mut result = Vec::with_capacity(layers.len());
    for layer in layers {
        if layer.id == group_id {
            result.extend(children.iter().cloned());
        } else if !child_ids.contains(&layer.id) {
            result.push(layer.clone());
        }
    }
    EditorSession::release_detached_clipping(&mut result);
    let records: Vec<ProjectLayerRecord> = result.iter().map(ImageLayer::hierarchy_record).collect();
    LayerHierarchy::validate(&records).ok()?;
    Some((result, children.iter().map(|child| child.id).collect()))
}

/// `addGroup()`'s list arithmetic: a new folder named after the first free `Folder N`, placed just
/// above the active layer — inside it when the active layer is a folder. `None` when the result
/// would be an invalid hierarchy.
pub fn added_group(layers: &[ImageLayer], active_id: Option<Id>, size: Size) -> Option<(Vec<ImageLayer>, Id)> {
    let names: HashSet<&str> = layers.iter().map(|layer| layer.name.as_str()).collect();
    let mut number = 1;
    while names.contains(format!("Folder {number}").as_str()) {
        number += 1;
    }
    let active = active_id.and_then(|id| layers.iter().find(|layer| layer.id == id));
    let mut group = ImageLayer::blank(format!("Folder {number}"), size);
    group.is_group = true;
    group.parent_id = if active.is_some_and(|layer| layer.is_group) {
        active_id
    } else {
        active.and_then(|layer| layer.parent_id)
    };
    let group_id = group.id;
    let mut result = layers.to_vec();
    let insertion = result
        .iter()
        .position(|layer| Some(layer.id) == active_id)
        .map(|index| index + 1)
        .unwrap_or(result.len());
    result.insert(insertion, group);
    let records: Vec<ProjectLayerRecord> = result.iter().map(ImageLayer::hierarchy_record).collect();
    LayerHierarchy::validate(&records).ok()?;
    Some((result, group_id))
}

/// `mergePlan()`: what merges, in stacking order, and where the result goes; nil when there is
/// nothing to merge. One layer merges with the layer beneath it in the same folder; several
/// selected layers merge together (with anything their folders hold); a folder merges its contents,
/// and the folder goes.
pub fn merge_plan_for(
    layers: &[ImageLayer],
    selected_layer_ids: &HashSet<Id>,
    active: &ImageLayer,
) -> Option<MergePlan> {
    if selected_layer_ids.len() > 1 {
        let mut picked = selected_layer_ids.clone();
        for id in selected_layer_ids {
            picked.extend(descendants(layers, *id));
        }
        let ordered: Vec<&ImageLayer> = layers.iter().filter(|layer| picked.contains(&layer.id)).collect();
        if !ordered.iter().any(|layer| !layer.is_group) {
            return None;
        }
        let top = ordered.iter().rev().find(|layer| selected_layer_ids.contains(&layer.id))?;
        return Some(MergePlan {
            ids: ordered.iter().map(|layer| layer.id).collect(),
            removed: picked,
            name: top.name.clone(),
            parent: top.parent_id,
            anchor: top.id,
            action: "Merge Layers",
        });
    }
    if active.is_group {
        let inside = descendants(layers, active.id);
        if !layers.iter().any(|layer| inside.contains(&layer.id) && !layer.is_group) {
            return None;
        }
        let ids: Vec<Id> = layers
            .iter()
            .filter(|layer| inside.contains(&layer.id) || layer.id == active.id)
            .map(|layer| layer.id)
            .collect();
        return Some(MergePlan {
            removed: ids.iter().copied().collect(),
            ids,
            name: active.name.clone(),
            parent: active.parent_id,
            anchor: active.id,
            action: "Merge Group",
        });
    }
    let index = layers.iter().position(|layer| layer.id == active.id)?;
    let below = layers[..index].iter().rev().find(|layer| layer.parent_id == active.parent_id)?;
    if below.is_group {
        return None;
    }
    Some(MergePlan {
        ids: vec![below.id, active.id],
        removed: [below.id, active.id].into_iter().collect(),
        name: below.name.clone(),
        parent: active.parent_id,
        anchor: active.id,
        action: "Merge Down",
    })
}

/// `copyLayers(_:into:at:)`'s anchor: what the copies are placed by — the one copied layer's own
/// middle, or the middle of the box around everything pictured, folders left out.
fn copy_anchor(copied: &[ImageLayer], ids: &[Id], source_size: Size) -> Point {
    let pictured: Vec<Rect> = copied
        .iter()
        .filter(|layer| !layer.is_group)
        .map(|layer| Rect::from_origin_size(layer.transform.origin, layer.transform.size))
        .collect();
    if ids.len() == 1 || pictured.is_empty() {
        let first = ids.first().copied();
        return copied
            .iter()
            .find(|layer| Some(layer.id) == first)
            .map(|layer| layer.transform.center())
            .unwrap_or_else(|| Point::new(source_size.width / 2.0, source_size.height / 2.0));
    }
    let union = pictured[1..]
        .iter()
        .fold(pictured[0], |box_, rect| box_.union(*rect));
    Point::new(union.mid_x(), union.mid_y())
}

impl EditorSession {
    /// Cmd-Shift-click on the canvas: adds a layer to the selection, or takes it out again when it
    /// is already in it.
    pub fn extend_selection(&mut self, id: Id) {
        let known = self
            .document
            .as_ref()
            .is_some_and(|document| document.layers.iter().any(|layer| layer.id == id));
        if !(self.can_edit_layers() || self.transform_edit.is_some()) || !known {
            return;
        }
        let mut ids = self.selected_layer_ids.clone();
        if ids.contains(&id) && ids.len() > 1 {
            ids.remove(&id);
            let primary = if self.active_layer_id == Some(id) {
                ids.iter().next().copied()
            } else {
                self.active_layer_id
            };
            self.select_layers(ids, primary);
        } else {
            ids.insert(id);
            self.select_layers(ids, Some(id));
        }
    }

    /// Group Selected Layers: wraps the selected layers in a new folder, at the topmost selected
    /// branch's place in their common parent, as one undo step.
    pub fn group_selected_layers(&mut self) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else { return };
        if document.layers.len() >= 10_000 {
            return;
        }
        let layers = document.layers.clone();
        let size = document.size();
        let Some(grouping) = grouped_layers(&layers, &self.selected_layer_ids, size) else {
            return;
        };
        self.begin_edit("Group Layers");
        if let Some(document) = self.document.as_mut() {
            document.layers = grouping.layers;
        }
        self.set_active_layer(Some(grouping.group));
        if let Some(parent) = grouping.parent {
            self.collapsed_group_ids.remove(&parent);
        }
        self.end_edit();
    }

    /// The active layer must be a folder, so there is something to unwrap.
    pub fn can_ungroup_layers(&self) -> bool {
        self.can_edit_layers() && self.active_layer().is_some_and(|layer| layer.is_group)
    }

    /// Reverses Group from Layers: the folder's direct children take its place among its own
    /// siblings, in the order they had inside it, and the folder goes. Its own opacity, blend mode,
    /// mask and effects are discarded along with it, as Photoshop's Ungroup does.
    pub fn ungroup_layers(&mut self) {
        if !self.can_ungroup_layers() {
            return;
        }
        let Some(group) = self.active_layer().cloned() else { return };
        let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) else {
            return;
        };
        let Some((next, children)) = ungrouped_layers(&layers, group.id) else {
            return;
        };
        self.finish_opacity_edit();
        self.begin_edit("Ungroup Layers");
        if let Some(document) = self.document.as_mut() {
            document.layers = next;
        }
        self.select_layers(children.clone(), children.first().copied());
        self.collapsed_group_ids.remove(&group.id);
        self.end_edit();
    }

    /// The Layers panel's rows, top first, with collapsed folders kept closed.
    pub fn layer_rows(&self) -> Vec<LayerHierarchyEntry> {
        let records: Vec<ProjectLayerRecord> = self
            .document
            .as_ref()
            .map(|document| document.layers.iter().map(ImageLayer::hierarchy_record).collect())
            .unwrap_or_default();
        LayerHierarchy::entries(&records, true, &self.collapsed_group_ids)
    }

    /// New Folder: a folder just above the active layer — inside it when it is a folder itself.
    pub fn add_group(&mut self) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else { return };
        if document.layers.len() >= 10_000 {
            return;
        }
        let layers = document.layers.clone();
        let size = document.size();
        let Some((next, group_id)) = added_group(&layers, self.active_layer_id, size) else {
            return;
        };
        self.begin_edit("New Folder");
        if let Some(document) = self.document.as_mut() {
            document.layers = next;
        }
        self.set_active_layer(Some(group_id));
        if let Some(parent) = self.active_layer().and_then(|layer| layer.parent_id) {
            self.collapsed_group_ids.remove(&parent);
        }
        self.end_edit();
    }

    /// The panel's disclosure triangle: opening or closing a folder. Closing it moves the
    /// selection out to the folder itself when it sat inside.
    pub fn toggle_group_expansion(&mut self, id: Id) {
        if self.is_project_busy {
            return;
        }
        let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) else {
            return;
        };
        if !layers.iter().any(|layer| layer.id == id && layer.is_group) {
            return;
        }
        if self.collapsed_group_ids.remove(&id) {
            return;
        }
        if self
            .active_layer_id
            .is_some_and(|active| descendants(&layers, id).contains(&active))
        {
            self.select_layer(Some(id));
        }
        self.collapsed_group_ids.insert(id);
    }

    /// Whether a layer may be placed inside `parent` (nil for the root).
    pub fn can_place_layer(&self, id: Id, parent: Option<Id>) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else {
            return false;
        };
        can_place_in(layers, id, parent)
    }

    /// Moves a layer under `parent`, just above `above`, or to the very bottom of the list, as one
    /// undo step (`Move Layer`); false when the move is refused.
    #[must_use]
    pub fn place_layer(&mut self, id: Id, parent: Option<Id>, above: Option<Id>, at_bottom: bool) -> bool {
        if !self.can_place_layer(id, parent) {
            return false;
        }
        let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) else {
            return false;
        };
        let Some(next) = placed_layers(&layers, id, parent, above, at_bottom) else {
            return false;
        };
        self.begin_edit("Move Layer");
        if let Some(document) = self.document.as_mut() {
            document.layers = next;
        }
        self.set_active_layer(Some(id));
        if let Some(parent) = parent {
            self.collapsed_group_ids.remove(&parent);
        }
        self.end_edit();
        true
    }

    /// A Layers-panel drop of `ids` at `row` (the port of the panel's `place(_:at:intoFolder:copying:)`).
    /// Dropped above a layer (or at the very bottom) the last one placed ends up nearest it, so they
    /// go in from the top down; dropped into a folder each lands on top, so they go in from the
    /// bottom up. With `copying` each is duplicated rather than moved. True when anything landed.
    #[must_use]
    pub fn place_layers(&mut self, ids: &[Id], row: usize, into_folder: bool, copying: bool) -> bool {
        let rows = self.layer_rows();
        let Some(placement) = drop_placement(&rows, row, into_folder) else {
            return false;
        };
        let order: Vec<Id> = if into_folder {
            ids.iter().rev().copied().collect()
        } else {
            ids.to_vec()
        };
        self.begin_edit(match (copying, ids.len()) {
            (true, count) if count > 1 => "Duplicate Layers",
            (true, _) => "Duplicate Layer",
            (false, count) if count > 1 => "Move Layers",
            (false, _) => "Move Layer",
        });
        let mut placed = false;
        for id in order {
            let done = if copying {
                self.duplicate_layer(id, placement.parent, placement.above, placement.at_bottom)
            } else {
                self.place_layer(id, placement.parent, placement.above, placement.at_bottom)
            };
            placed = done || placed;
        }
        // The layers that moved stay selected, so they can be dragged on as a group.
        if placed && !copying {
            self.select_layers(ids.iter().copied(), ids.first().copied());
        }
        self.end_edit();
        placed
    }

    /// Move Out of Folder: takes the active layer out of its folder, right above it.
    pub fn move_active_layer_out_of_group(&mut self) {
        let Some(layer) = self.active_layer().cloned() else { return };
        let Some(parent) = layer.parent_id else { return };
        let Some(grandparent) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|group| group.id == parent))
            .map(|group| group.parent_id)
        else {
            return;
        };
        // Swift's `placeLayer` is `@discardableResult` here: Move Out of Folder has no result to act on.
        let _ = self.place_layer(layer.id, grandparent, Some(parent), false);
    }
}

impl EditorSession {
    /// What ⌘E merges, in stacking order, and where the result goes; nil when there is nothing to merge.
    pub fn merge_plan(&self) -> Option<MergePlan> {
        if !self.can_edit_layers() {
            return None;
        }
        let active = self.active_layer()?.clone();
        let layers = self.document.as_ref()?.layers.clone();
        merge_plan_for(&layers, &self.selected_layer_ids, &active)
    }

    /// Whether ⌘E has anything to merge.
    pub fn can_merge_layers(&self) -> bool {
        self.merge_plan().is_some()
    }

    /// The merge item's title: `Merge Down`, `Merge Layers` or `Merge Group`.
    pub fn merge_title(&self) -> &'static str {
        self.merge_plan().map(|plan| plan.action).unwrap_or("Merge Down")
    }

    /// ⌘E: the layers composited as the canvas shows them — blend modes, opacity, masks, clipping
    /// and adjustments baked in — into one pixel layer, trimmed to what is there, in their place, as
    /// one undo step.
    pub fn merge_layers(&mut self) {
        self.commit_transform();
        let Some(plan) = self.merge_plan() else { return };
        let Some(document) = self.document.as_ref() else { return };
        let width = document.width;
        let height = document.height;
        let resolution = document.resolution;
        let document_id = document.id;
        let layers = document.layers.clone();
        let kept = plan.ids.iter().copied().collect::<HashSet<Id>>();
        // Only the merged layers, cut loose from anything outside the merge.
        let subset: Vec<ImageLayer> = layers
            .iter()
            .filter(|layer| kept.contains(&layer.id))
            .map(|layer| {
                let mut copy = layer.clone();
                if let Some(parent) = copy.parent_id {
                    if !kept.contains(&parent) {
                        copy.parent_id = None;
                    }
                }
                if let Some(source) = copy.mask_source_id {
                    if !kept.contains(&source) {
                        copy.mask_source_id = None;
                    }
                }
                copy
            })
            .collect();
        let flat = CanvasDocument::with_id(document_id, width, height, subset, resolution, Vec::new());
        let mut canvas = Canvas::new_rgba(width, height);
        Composite::draw(&flat, 1.0, &|point| point, &mut canvas);
        let full = canvas.into_rgba();
        let placed = LayerTransform {
            origin: Point::ZERO,
            size: Size::new(width as f64, height as f64),
            ..Default::default()
        };
        let Ok((image, transform)) = PixelFilter::trimmed(&full, &placed) else {
            return;
        };
        let thumbnail = rgba_thumbnail(&image);
        let asset = ImportedImage::new(PixelImage::Rgba(Arc::new(image)), thumbnail, plan.name.clone());
        let mut merged = ImageLayer::from_asset(asset, transform.origin);
        merged.transform = transform;
        merged.name = plan.name.clone();
        merged.parent_id = plan.parent;
        let mut next: Vec<ImageLayer> = layers
            .iter()
            .filter(|layer| !plan.removed.contains(&layer.id))
            .cloned()
            .collect();
        // Layers clipped to anything that was merged now clip to the result.
        for layer in next.iter_mut() {
            if layer.mask_source_id.is_some_and(|source| plan.removed.contains(&source)) {
                layer.mask_source_id = Some(merged.id);
            }
        }
        let slot = layers
            .iter()
            .position(|layer| layer.id == plan.anchor)
            .unwrap_or(layers.len());
        let insertion = slot - layers[..slot].iter().filter(|layer| plan.removed.contains(&layer.id)).count();
        next.insert(insertion.min(next.len()), merged.clone());
        let records: Vec<ProjectLayerRecord> = next.iter().map(ImageLayer::hierarchy_record).collect();
        if LayerHierarchy::validate(&records).is_err() {
            return;
        }
        self.finish_opacity_edit();
        self.begin_edit(plan.action);
        if let Some(document) = self.document.as_mut() {
            document.layers = next;
        }
        self.set_active_layer(Some(merged.id));
        self.end_edit();
    }

    /// Flips the selected layer about its own middle — or several selected layers, or a folder's
    /// contents, about the middle of the box around them — as one undo step. Masks follow the link:
    /// a linked mask flips with its layer, an unlinked one stays where it is.
    pub fn flip_layers(&mut self, horizontally: bool) {
        self.commit_transform();
        if !self.can_transform() {
            return;
        }
        let (members, axis) = if self.transforms_as_group() {
            let Some(box_) = self.group_transform_box() else { return };
            (
                self.group_transform_members(),
                if horizontally { box_.center().x } else { box_.center().y },
            )
        } else {
            let Some(layer) = self.active_layer() else { return };
            (
                vec![layer.id],
                if horizontally {
                    layer.transform.center().x
                } else {
                    layer.transform.center().y
                },
            )
        };
        let ids: HashSet<Id> = members.into_iter().collect();
        if ids.is_empty() {
            return;
        }
        let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) else {
            return;
        };
        self.finish_opacity_edit();
        self.begin_edit(if horizontally { "Flip Horizontal" } else { "Flip Vertical" });
        for (index, layer) in layers.iter().enumerate() {
            if !ids.contains(&layer.id) {
                continue;
            }
            let flipped = layer.transform.mirrored(horizontally, axis);
            let placement = layer
                .mask
                .as_ref()
                .map(|mask| mask.placement_moving_layer(&layer.transform, &flipped));
            if let Some(document) = self.document.as_mut() {
                if let (Some(mask), Some(placement)) = (document.layers[index].mask.as_mut(), placement) {
                    mask.placement = placement;
                }
                document.layers[index].transform = flipped;
            }
        }
        self.end_edit();
    }
}

impl EditorSession {
    /// The blend mode a layer is drawn with right now: the live preview's, while the panel is
    /// previewing one on the active layer, else its own.
    pub fn displayed_blend_mode(&self, layer: &ImageLayer) -> LayerBlendMode {
        if let Some((layer_id, mode)) = self.blend_preview {
            if layer_id == layer.id && self.active_layer_id == Some(layer.id) {
                return mode;
            }
        }
        layer.blend_mode
    }

    /// The blend menu's live preview: the mode the canvas shows while the pointer is over the menu,
    /// without changing the layer until the choice is made.
    pub fn preview_blend_mode(&mut self, mode: Option<LayerBlendMode>, for_id: Option<Id>) {
        if let (Some(mode), Some(id)) = (mode, for_id) {
            if Some(id) == self.active_layer_id && self.can_edit_appearance() {
                self.blend_preview = Some((id, mode));
            } else {
                self.blend_preview = None;
            }
        } else {
            self.blend_preview = None;
        }
        self.refresh_canvas_preview();
    }

    /// Whether the appearance controls apply: one layer selected, and not a folder.
    pub fn can_edit_appearance(&self) -> bool {
        self.can_edit_layers()
            && self.selected_layer_ids.len() == 1
            && self.active_layer().is_some_and(|layer| !layer.is_group)
    }

    /// A folder takes an opacity of its own, which dims everything inside it (see
    /// `LayerOpacity`); blending still belongs to each layer, so the rest of the appearance
    /// controls stay off for folders.
    pub fn can_edit_opacity(&self) -> bool {
        self.can_edit_layers() && self.selected_layer_ids.len() == 1 && self.active_layer().is_some()
    }

    /// Opens the undo transaction an opacity drag runs inside.
    pub fn begin_opacity_edit(&mut self) {
        if !self.can_edit_opacity() || self.opacity_edit_layer_id.is_some() {
            return;
        }
        let Some(id) = self.active_layer_id else { return };
        self.begin_edit("Layer Opacity");
        self.opacity_edit_layer_id = Some(id);
    }

    /// Closes the opacity transaction, wherever the drag ended.
    pub fn finish_opacity_edit(&mut self) {
        if self.opacity_edit_layer_id.is_none() {
            return;
        }
        self.opacity_edit_layer_id = None;
        self.end_edit();
    }

    /// One step of an opacity drag: the layer's opacity, clamped to 0–1, in the open transaction —
    /// or in a transaction of its own when none is open.
    pub fn set_layer_opacity(&mut self, opacity: f64) {
        if !opacity.is_finite() || !self.can_edit_opacity() {
            return;
        }
        let Some(id) = self.opacity_edit_layer_id.or(self.active_layer_id) else {
            return;
        };
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
        else {
            return;
        };
        let standalone = self.opacity_edit_layer_id.is_none();
        if standalone {
            self.begin_edit("Layer Opacity");
        }
        if let Some(document) = self.document.as_mut() {
            document.layers[index].opacity = opacity.clamp(0.0, 1.0);
        }
        if standalone {
            self.end_edit();
        }
    }

    /// Sets every selected layer's opacity as one undo step. A selected folder takes the value too,
    /// dimming its contents on top of their own opacity.
    pub fn set_selected_layers_opacity(&mut self, opacity: f64) {
        if !opacity.is_finite() || !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else { return };
        let value = opacity.clamp(0.0, 1.0);
        let indices: Vec<usize> = document
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| self.selected_layer_ids.contains(&layer.id) && layer.opacity != value)
            .map(|(index, _)| index)
            .collect();
        if indices.is_empty() {
            return;
        }
        self.finish_opacity_edit();
        self.begin_edit("Layer Opacity");
        for index in indices {
            if let Some(document) = self.document.as_mut() {
                document.layers[index].opacity = value;
            }
        }
        self.end_edit();
    }

    /// Shift-+ / Shift-−: the active layer's blend mode steps to the next or previous one in the
    /// blend menu's order, wrapping around, as one undo step.
    pub fn cycle_blend_mode(&mut self, forward: bool) {
        if !self.can_edit_appearance() {
            return;
        }
        let Some(layer) = self.active_layer() else { return };
        let modes = LayerBlendMode::ALL;
        let index = modes.iter().position(|mode| *mode == layer.blend_mode).unwrap_or(0);
        self.set_layer_blend_mode(modes[(index + if forward { 1 } else { modes.len() - 1 }) % modes.len()]);
    }

    /// The blend menu's choice for the active layer, as one undo step.
    pub fn set_layer_blend_mode(&mut self, mode: LayerBlendMode) {
        self.blend_preview = None;
        if !self.can_edit_appearance() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| Some(layer.id) == self.active_layer_id))
        else {
            return;
        };
        self.finish_opacity_edit();
        self.begin_edit("Layer Blend Mode");
        if let Some(document) = self.document.as_mut() {
            document.layers[index].blend_mode = mode;
        }
        self.end_edit();
    }
}

impl EditorSession {
    /// The selected layers Copy and Duplicate take whole, in document order, leaving out any inside
    /// a selected folder.
    pub fn copied_layer_ids(&self) -> Vec<Id> {
        let mut selected = self.selected_layer_ids.clone();
        if let Some(active) = self.active_layer_id {
            selected.insert(active);
        }
        let mut nested: HashSet<Id> = HashSet::new();
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else {
            return Vec::new();
        };
        for id in &selected {
            nested.extend(descendants(layers, *id));
        }
        layers
            .iter()
            .map(|layer| layer.id)
            .filter(|id| selected.contains(id) && !nested.contains(id))
            .collect()
    }

    /// ⌘J and Duplicate Layer: every selected layer, as Photoshop does.
    pub fn duplicate_active_layer(&mut self) {
        let ids = self.copied_layer_ids();
        self.duplicate_layers(&ids, "Duplicate Layer");
    }

    /// A copy of each layer (a folder with all it holds), as one undo step: Duplicate Layer, and
    /// Paste of layers Copy took whole. One copy sits just above its original; several stack
    /// together, in their order, above the topmost original, as Photoshop's do. The copies end up
    /// selected.
    pub fn duplicate_layers(&mut self, ids: &[Id], edit_name: &str) {
        if !self.can_edit_layers() || ids.is_empty() {
            return;
        }
        let active = self.active_layer_id;
        self.begin_edit(edit_name);
        let mut copies_of: HashMap<Id, Id> = HashMap::new();
        for id in ids {
            if let Some(copy) = self.insert_copy(*id) {
                copies_of.insert(*id, copy);
            }
        }
        if copies_of.is_empty() {
            self.end_edit();
            return;
        }
        if copies_of.len() > 1 {
            if let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) {
                // Panel order, top first, so layers in different folders compare as they're seen.
                let records: Vec<ProjectLayerRecord> =
                    layers.iter().map(ImageLayer::hierarchy_record).collect();
                let panel: Vec<Id> = LayerHierarchy::entries(&records, true, &HashSet::new())
                    .into_iter()
                    .map(|entry| entry.layer.id)
                    .collect();
                let originals: Vec<Id> = panel.into_iter().filter(|id| copies_of.contains_key(id)).collect();
                if let Some(top) = originals.first().copied() {
                    if let Some(top_layer) = layers.iter().find(|layer| layer.id == top) {
                        let parent = top_layer.parent_id;
                        let mut below = top;
                        for original in originals.iter().rev() {
                            let Some(copy) = copies_of.get(original).copied() else { continue };
                            if !self.place_layer(copy, parent, Some(below), false) {
                                continue;
                            }
                            below = copy;
                        }
                    }
                }
            }
        }
        let next = active
            .and_then(|id| copies_of.get(&id).copied())
            .or_else(|| copies_of.get(&ids[0]).copied())
            .or_else(|| copies_of.values().next().copied());
        if let Some(next) = next {
            self.set_active_layer(Some(next));
        }
        self.selected_layer_ids = copies_of.values().copied().collect();
        self.end_edit();
    }

    /// Inserts a copy of the layer and anything it holds just above it; returns the copy's id.
    fn insert_copy(&mut self, id: Id) -> Option<Id> {
        let layers = self.document.as_ref().map(|document| document.layers.clone())?;
        let layer = layers.iter().find(|layer| layer.id == id)?.clone();
        let index = layers.iter().position(|candidate| candidate.id == layer.id)?;
        let mut included = descendants(&layers, layer.id);
        included.insert(layer.id);
        let originals: Vec<ImageLayer> = layers
            .iter()
            .filter(|layer| included.contains(&layer.id))
            .cloned()
            .collect();
        if layers.len() + originals.len() > 10_000 {
            return None;
        }
        let mapping: HashMap<Id, Id> = originals.iter().map(|layer| (layer.id, new_id())).collect();
        let copies: Vec<ImageLayer> = originals
            .iter()
            .map(|original| {
                ImageLayer::with_id(
                    mapping[&original.id],
                    original.asset.clone(),
                    if original.id == layer.id {
                        format!("{} copy", original.name)
                    } else {
                        original.name.clone()
                    },
                    original.is_visible,
                    original.transform,
                    original.parent_id.map(|parent| mapping.get(&parent).copied().unwrap_or(parent)),
                    original.is_group,
                    original.opacity,
                    original.blend_mode,
                    original.mask.clone(),
                    original
                        .mask_source_id
                        .map(|source| mapping.get(&source).copied().unwrap_or(source)),
                    original.adjustment.clone(),
                    original.shape.clone(),
                    original.effects.clone(),
                    original.text.clone(),
                )
            })
            .collect();
        let copy_id = mapping[&layer.id];
        if let Some(document) = self.document.as_mut() {
            document.layers.splice(index + 1..index + 1, copies);
        }
        for original in &originals {
            if self.collapsed_group_ids.contains(&original.id) {
                self.collapsed_group_ids.insert(mapping[&original.id]);
            }
        }
        Some(copy_id)
    }

    /// Option-drag in the Layers panel: a copy of the layer placed where it was dropped (inside
    /// `parent`, above `above`, or at the very bottom), as one undo step. Folders carry all
    /// descendants.
    #[must_use]
    pub fn duplicate_layer(&mut self, id: Id, parent: Option<Id>, above: Option<Id>, at_bottom: bool) -> bool {
        if !self.can_edit_layers() || !self.can_place_layer(id, parent) {
            return false;
        }
        self.begin_edit("Duplicate Layer");
        self.select_layer(Some(id));
        self.duplicate_active_layer();
        let placed = self
            .active_layer_id
            .filter(|copy| *copy != id)
            .is_some_and(|copy| self.place_layer(copy, parent, above, at_bottom));
        self.end_edit();
        placed
    }

    /// `ProjectWorkspace.copyLayers(_:into:at:)`'s source half: the layers `ids` name, each with
    /// everything inside it, where a layer clipped to a source that is not coming along has that
    /// dependency baked into its own pixels and the clip released — so the copy keeps the masked
    /// appearance wherever it lands. `None` when there is nothing to copy, or when this session
    /// cannot edit layers; a baker failure leaves `brush_error` set and returns `None`.
    pub fn layers_for_copy(&mut self, ids: &[Id], host: &dyn SessionHost) -> Option<Vec<ImageLayer>> {
        if !self.can_edit_layers() {
            return None;
        }
        let Some(first) = ids.first().copied() else { return None };
        let Some(document) = self.document.as_ref() else { return None };
        if !document.layers.iter().any(|layer| layer.id == first) {
            return None;
        }
        let included: HashSet<Id> = ids
            .iter()
            .flat_map(|id| descendants(&document.layers, *id))
            .chain(ids.iter().copied())
            .collect();
        let mut copied: Vec<ImageLayer> = document
            .layers
            .iter()
            .filter(|layer| included.contains(&layer.id))
            .cloned()
            .collect();
        let Some(snapshot) = self.project_snapshot() else { return None };
        self.is_project_busy = true;
        let mut failure = None;
        for layer in copied.iter_mut() {
            if !layer.mask_source_id.is_some_and(|source| !included.contains(&source)) {
                continue;
            }
            if layer.adjustment.is_some() {
                layer.mask_source_id = None;
                continue;
            }
            match host.bake_live_mask(&snapshot, layer.id) {
                Ok(baked) => {
                    layer.asset = baked;
                    layer.mask_source_id = None;
                }
                Err(message) => {
                    failure = Some(message);
                    break;
                }
            }
        }
        self.is_project_busy = false;
        match failure {
            Some(message) => {
                self.brush_error = Some(message);
                None
            }
            None => Some(copied),
        }
    }

    /// `ProjectWorkspace.copyLayers(_:into:at:)`'s destination half: installs copies of `copied` (a
    /// source session's [`Self::layers_for_copy`] output) in this project, as one undo step named
    /// "Copy Layers from Project" — new ids, the same places relative to each other, centered on
    /// `point` or on the canvas, with a new canvas made at the source's size when this project has
    /// none. False when nothing was installed (an empty copy, or a session that cannot take it); a
    /// copy over the document pixel budget leaves `import_error` set.
    #[must_use]
    pub fn copy_layers_into(&mut self, copied: &[ImageLayer], ids: &[Id], source_size: Size, point: Option<Point>) -> bool {
        if copied.is_empty() || !ids.iter().any(|id| copied.iter().any(|layer| layer.id == *id)) {
            return false;
        }
        if self.document.is_some() && !self.can_edit_layers() {
            return false;
        }
        let Some(first) = ids.first().copied() else { return false };
        let pixels = |layers: &[ImageLayer]| -> usize {
            layers
                .iter()
                .filter_map(|layer| layer.asset.as_ref())
                .map(|asset| asset.image.width().saturating_mul(asset.image.height()))
                .sum()
        };
        let used = self
            .document
            .as_ref()
            .map(|document| pixels(&document.layers))
            .unwrap_or(0);
        if used.saturating_add(pixels(copied)) > document_pixel_budget() {
            self.import_error = Some(format!(
                "The copied layers exceed this project’s {}-megapixel limit.",
                document_budget_megapixels()
            ));
            return false;
        }
        let size = self.document.as_ref().map(|document| document.size()).unwrap_or(source_size);
        let anchor = copy_anchor(copied, ids, source_size);
        let center = point.unwrap_or_else(|| Point::new(size.width / 2.0, size.height / 2.0));
        let mapping: HashMap<Id, Id> = copied.iter().map(|layer| (layer.id, new_id())).collect();
        let layers: Vec<ImageLayer> = copied
            .iter()
            .map(|layer| {
                let mut transform = layer.transform;
                transform.origin.x += center.x - anchor.x;
                transform.origin.y += center.y - anchor.y;
                let mut mask = layer.mask.clone();
                if let Some(placement) = mask.as_mut().and_then(|mask| mask.placement.as_mut()) {
                    placement.origin.x += center.x - anchor.x;
                    placement.origin.y += center.y - anchor.y;
                }
                ImageLayer::with_id(
                    mapping[&layer.id],
                    layer.asset.clone(),
                    layer.name.clone(),
                    layer.is_visible,
                    transform,
                    layer.parent_id.and_then(|parent| mapping.get(&parent).copied()),
                    layer.is_group,
                    layer.opacity,
                    layer.blend_mode,
                    mask,
                    layer.mask_source_id.and_then(|source| mapping.get(&source).copied()),
                    layer.adjustment.clone(),
                    layer.shape.clone(),
                    layer.effects.clone(),
                    layer.text.clone(),
                )
            })
            .collect();
        self.begin_edit("Copy Layers from Project");
        if self.document.is_none() {
            self.create_document(size.width as usize, size.height as usize, false);
        }
        if let Some(document) = self.document.as_mut() {
            document.layers.extend(layers);
        }
        self.set_active_layer(mapping.get(&first).copied());
        self.selected_layer_ids = ids.iter().filter_map(|id| mapping.get(id).copied()).collect();
        self.end_edit();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(name: &str, parent: Option<Id>) -> ImageLayer {
        let mut layer = ImageLayer::blank(name, Size::new(8.0, 8.0));
        layer.parent_id = parent;
        layer
    }

    fn folder(name: &str, parent: Option<Id>) -> ImageLayer {
        let mut folder = layer(name, parent);
        folder.is_group = true;
        folder
    }

    fn ids(layers: &[ImageLayer]) -> Vec<Id> {
        layers.iter().map(|layer| layer.id).collect()
    }

    /// `reorderLayers(fromOffsets:toOffset:)` converts the panel's top-to-bottom rows to the
    /// compositor's bottom-to-top layer list and back; the moved rows land immediately before the
    /// row the destination named before the move.
    #[test]
    fn reorder_converts_between_panel_rows_and_layer_order() {
        let bottom_to_top: Vec<ImageLayer> = (1..=4)
            .map(|number| layer(&format!("Layer {number}"), None))
            .collect();
        let [one, two, three, four] = [
            bottom_to_top[0].id,
            bottom_to_top[1].id,
            bottom_to_top[2].id,
            bottom_to_top[3].id,
        ];
        // The panel lists four, three, two, one; moving the top row down to row 3.
        assert_eq!(
            ids(&reordered_layers(&bottom_to_top, &[0], 3).expect("in range")),
            vec![one, four, two, three]
        );
        // Moving the bottom row (panel row 3) to the top.
        assert_eq!(
            ids(&reordered_layers(&bottom_to_top, &[3], 0).expect("in range")),
            vec![two, three, four, one]
        );
        // A destination past the last row puts the moved rows at the very bottom of the panel.
        assert_eq!(
            ids(&reordered_layers(&bottom_to_top, &[0], 4).expect("in range")),
            vec![four, one, two, three]
        );
        // Two rows move together, keeping their order.
        assert_eq!(
            ids(&reordered_layers(&bottom_to_top, &[0, 1], 4).expect("in range")),
            vec![three, four, one, two]
        );
        // Out of range, or offsets IndexSet could not hold (descending), are refused.
        assert_eq!(reordered_layers(&bottom_to_top, &[0], 5), None);
        assert_eq!(reordered_layers(&bottom_to_top, &[4], 0), None);
        assert_eq!(reordered_layers(&bottom_to_top, &[2, 1], 0), None);
    }

    /// A folder moved among its siblings keeps its contents: only the folder's own row moves, and
    /// what is inside stays inside.
    #[test]
    fn placing_a_folder_keeps_its_contents() {
        let group = folder("Folder 1", None);
        let group_id = group.id;
        // The child lives inside the folder, so the hierarchy stays valid across the move.
        let child = layer("Child", Some(group_id));
        let other = layer("Other", None);
        let layers = vec![child, group, other];
        let next = placed_layers(&layers, group_id, None, None, true).expect("the move is valid");
        assert_eq!(ids(&next), vec![group_id, layers[0].id, layers[2].id]);
        assert_eq!(next[0].parent_id, None);
        assert_eq!(next[1].parent_id, Some(group_id));
    }

    /// The nesting ceiling is 64 levels of ancestors: a layer under 64 folders is valid, under 65 the
    /// hierarchy is refused and the move with it.
    #[test]
    fn placing_refuses_a_sixty_fifth_folder_level() {
        let mut layers = Vec::new();
        let mut parent: Option<Id> = None;
        for number in 1..=64 {
            let group = folder(&format!("Folder {number}"), parent);
            parent = Some(group.id);
            layers.push(group);
        }
        let deepest = parent.expect("64 folders were built");
        let moving = layer("Moving", None);
        layers.push(moving.clone());
        let under_64 = placed_layers(&layers, moving.id, Some(deepest), None, false).expect("64 levels are allowed");
        assert_eq!(
            under_64.iter().find(|layer| layer.id == moving.id).and_then(|layer| layer.parent_id),
            Some(deepest)
        );
        let fifth = folder("Folder 65", Some(deepest));
        let fifth_id = fifth.id;
        layers.push(fifth);
        // Folder 65 itself is one level past the ceiling, so the document it would live in is invalid.
        assert!(placed_layers(&layers, moving.id, Some(fifth_id), None, false).is_none());
        let records: Vec<ProjectLayerRecord> = layers.iter().map(ImageLayer::hierarchy_record).collect();
        assert!(LayerHierarchy::validate(&records).is_err());
    }

    /// The panel's drop conversion: above a row means that row's folder, inside a folder means the
    /// folder itself, and a row past the end means the very bottom.
    #[test]
    fn drop_rows_convert_to_folder_and_neighbor() {
        let group = folder("Folder 1", None);
        let group_id = group.id;
        let child = layer("Child", Some(group_id));
        let layers = vec![group.clone(), child.clone()];
        let records: Vec<ProjectLayerRecord> = layers.iter().map(ImageLayer::hierarchy_record).collect();
        let rows = LayerHierarchy::entries(&records, true, &HashSet::new());
        // Top first: the folder, then its child.
        assert_eq!(rows[0].layer.id, group_id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].layer.id, child.id);
        assert_eq!(
            drop_placement(&rows, 0, false).expect("row 0"),
            LayerDropPlacement {
                parent: None,
                above: Some(group_id),
                at_bottom: false
            }
        );
        assert_eq!(
            drop_placement(&rows, 0, true).expect("row 0 into the folder"),
            LayerDropPlacement {
                parent: Some(group_id),
                above: None,
                at_bottom: false
            }
        );
        assert_eq!(
            drop_placement(&rows, rows.len(), false).expect("past the end"),
            LayerDropPlacement {
                parent: None,
                above: None,
                at_bottom: true
            }
        );
        assert_eq!(drop_placement(&rows, rows.len() + 1, false), None);
        // The child's own row carries the folder as its parent.
        assert_eq!(drop_placement(&rows, 1, false).expect("row 1").parent, Some(group_id));
    }

    /// `mergePlan()`'s rules: one layer merges down with the sibling beneath it, several merge in
    /// document order above the topmost selected, and a folder merges its contents and goes.
    #[test]
    fn merge_plan_follows_the_stacking_order() {
        let parent = new_id();
        let below = layer("Below", Some(parent));
        let above = layer("Above", Some(parent));
        let layers = vec![below.clone(), above.clone()];
        let plan = merge_plan_for(&layers, &HashSet::new(), &above).expect("a layer above another merges down");
        assert_eq!(plan.ids, vec![below.id, above.id]);
        assert_eq!(plan.name, "Below");
        assert_eq!(plan.parent, Some(parent));
        assert_eq!(plan.anchor, above.id);
        assert_eq!(plan.action, "Merge Down");
        // The bottom layer of a folder has nothing beneath it to merge with.
        assert_eq!(merge_plan_for(&layers, &HashSet::new(), &below), None);
        // Neither has a layer whose neighbour below is a folder.
        let group = folder("Folder 1", None);
        let pixel = layer("Pixel", None);
        assert_eq!(merge_plan_for(&[group.clone(), pixel.clone()], &HashSet::new(), &pixel), None);
        // A folder merges its contents, and the folder goes with them.
        let inside = layer("Inside", Some(group.id));
        let layers = vec![group.clone(), inside.clone()];
        let plan = merge_plan_for(&layers, &HashSet::new(), &group).expect("a folder with pixels merges");
        assert_eq!(plan.ids, vec![group.id, inside.id]);
        assert_eq!(plan.action, "Merge Group");
        assert_eq!(plan.name, "Folder 1");
        assert_eq!(plan.parent, None);
        assert!(plan.removed.contains(&group.id));
        // A folder holding only folders has nothing to merge.
        let inner = folder("Inner", Some(group.id));
        assert_eq!(
            merge_plan_for(&[group.clone(), inner.clone()], &HashSet::new(), &group),
            None
        );
        // Several selected layers merge together, named after the topmost of them, with anything
        // their folders hold pulled in.
        let group2 = folder("Folder 2", None);
        let inner = layer("Inner", Some(group2.id));
        let top = layer("Top", None);
        let layers = vec![below.clone(), group2.clone(), inner.clone(), top.clone()];
        let selected: HashSet<Id> = [below.id, top.id, group2.id].into_iter().collect();
        let plan = merge_plan_for(&layers, &selected, &top).expect("the selection merges");
        assert_eq!(plan.ids, vec![below.id, group2.id, inner.id, top.id]);
        assert_eq!(plan.name, "Top");
        assert_eq!(plan.anchor, top.id);
        assert_eq!(plan.parent, None);
        assert_eq!(plan.action, "Merge Layers");
        assert!(plan.removed.contains(&inner.id));
        // A selection of folders with no pixels at all has nothing to merge.
        let first = folder("F1", None);
        let second = folder("F2", None);
        let selected: HashSet<Id> = [first.id, second.id].into_iter().collect();
        assert_eq!(
            merge_plan_for(&[first.clone(), second.clone()], &selected, &second),
            None
        );
    }

    /// Group Selected Layers puts the folder at the topmost selected branch's place and takes the
    /// selected roots — a selected folder with its subtree — inside it.
    #[test]
    fn grouping_wraps_the_selected_roots() {
        let layers = vec![layer("A", None), layer("B", None), layer("C", None)];
        let (a, b, c) = (layers[0].id, layers[1].id, layers[2].id);
        let selected: HashSet<Id> = [a, c].into_iter().collect();
        let grouping = grouped_layers(&layers, &selected, Size::new(8.0, 8.0)).expect("a valid grouping");
        assert_eq!(ids(&grouping.layers), vec![b, grouping.group, a, c]);
        assert_eq!(grouping.layers[2].parent_id, Some(grouping.group));
        assert_eq!(grouping.layers[3].parent_id, Some(grouping.group));
        assert_eq!(grouping.parent, None);
        assert_eq!(grouping.layers[1].name, "Folder 1");
        assert!(grouping.layers[1].is_group);
        // A selected folder carries its contents; a further group names itself Folder 2.
        let folder_id = grouping.layers[1].id;
        let selected: HashSet<Id> = [folder_id].into_iter().collect();
        let again = grouped_layers(&grouping.layers, &selected, Size::new(8.0, 8.0)).expect("a valid grouping");
        // `groupSelectedLayers()` puts the new wrapper at the topmost selected branch's place and
        // appends the selected folder, now its child, after it, so the new group is not last.
        let wrapped = again.layers.iter().find(|layer| layer.id == again.group).expect("the new folder");
        assert_eq!(wrapped.name, "Folder 2");
        assert!(wrapped.is_group);
        assert_eq!(
            again.layers.iter().find(|layer| layer.id == folder_id).and_then(|layer| layer.parent_id),
            Some(again.group),
            "the selected folder goes inside the new one"
        );
        assert_eq!(again.parent, None);
    }

    /// A cross-project copy is placed by the copied layer's own middle — or the middle of the box
    /// around everything pictured, folders left out.
    #[test]
    fn copy_anchor_follows_what_is_copied() {
        let mut first = layer("First", None);
        first.transform = LayerTransform {
            origin: Point::new(10.0, 20.0),
            size: Size::new(30.0, 40.0),
            ..Default::default()
        };
        assert_eq!(
            copy_anchor(&[first.clone()], &[first.id], Size::new(100.0, 100.0)),
            Point::new(25.0, 40.0)
        );
        let mut second = layer("Second", None);
        second.transform = LayerTransform {
            origin: Point::new(100.0, 100.0),
            size: Size::new(20.0, 20.0),
            ..Default::default()
        };
        assert_eq!(
            copy_anchor(
                &[first.clone(), second.clone()],
                &[first.id, second.id],
                Size::new(100.0, 100.0)
            ),
            Point::new(65.0, 70.0)
        );
        // A folder has no pixels of its own, so a single folder is placed by its own middle, which
        // covers the canvas.
        let mut group = folder("Folder 1", None);
        group.transform = LayerTransform {
            origin: Point::ZERO,
            size: Size::new(100.0, 100.0),
            ..Default::default()
        };
        assert_eq!(
            copy_anchor(&[group.clone()], &[group.id], Size::new(100.0, 100.0)),
            Point::new(50.0, 50.0)
        );
    }

    /// Ungroup splices the folder's children into the folder's own slot among its siblings, in the
    /// order they had inside it, and refreshes no clipping that is still attached.
    #[test]
    fn ungrouping_splices_children_into_the_folder_slot() {
        let group = folder("Folder 1", None);
        let group_id = group.id;
        let first = layer("First", Some(group_id));
        let second = layer("Second", Some(group_id));
        let top = layer("Top", None);
        let layers = vec![first.clone(), group.clone(), second.clone(), top.clone()];
        let (next, children) = ungrouped_layers(&layers, group_id).expect("a valid ungrouping");
        assert_eq!(children, vec![first.id, second.id]);
        assert_eq!(ids(&next), vec![first.id, second.id, top.id]);
        assert!(next.iter().all(|layer| layer.parent_id.is_none()));
    }
}
