//! Live (clipping) masks: which layer's coverage hides which, and what happens to the dependents when
//! a supplying layer is deleted.
//!
//! Port of `Document/LiveLayerMask.swift`'s `extension EditorSession`, minus `drawLiveComposite` —
//! that one takes a `CGContext` and belongs to the drawing pipeline in `compositor-render`
//! (`Composite`/`LiveMaskRenderer`); the session half it reads (`displayedTransform(for:)`,
//! `displayedBlendMode(for:)`, `displayedMaskPlacement(for:)`) lives with the transform/view modules.
//!
//! **Async → sync.** `deleteWithLiveMaskChoice` baked on a background `Task` behind `isProjectBusy`;
//! the port bakes through [`SessionHost::bake_live_mask`] on the calling thread with the same busy flag
//! set and cleared around it, and the same guard that does nothing when no layer supplies a live mask.
//! Swift's `NSAlert` becomes [`LiveMaskDeleteAlert`], whose strings are the alert's, plus the
//! [`LiveMaskDeleteChoice`] the sheet returns.
//!
//! This module expects `descendant_ids(_:)` (the port of `descendantIDs(of:)`, `LayerGroups.swift`) on
//! `EditorSession`.

use std::collections::{HashMap, HashSet};

use compositor_core::document::{ImageLayer, ProjectError, ProjectLayerRecord};
use compositor_core::imported_image::ImportedImage;
use compositor_core::Id;

use crate::projects::SessionHost;
use crate::EditorSession;

/// The mask-source graph of a manifest: a layer may clip to one other layer, the chain must not cycle,
/// and a source must be an image layer that is not an adjustment (`LiveMaskGraph.validate`).
pub struct LiveMaskGraph;

impl LiveMaskGraph {
    /// Throws `ProjectError::invalid` on a duplicate id, a cycle, a chain over 256 deep, a missing or
    /// folder source, a source that is itself clipped, or an adjustment source.
    pub fn validate(layers: &[ProjectLayerRecord]) -> Result<(), ProjectError> {
        let mut records: HashMap<Id, &ProjectLayerRecord> = HashMap::new();
        for layer in layers {
            if records.insert(layer.id, layer).is_some() {
                return Err(ProjectError::Invalid);
            }
        }
        for layer in layers {
            let mut path: HashSet<Id> = HashSet::new();
            let mut current = Some(layer.id);
            while let Some(id) = current {
                if path.len() >= 256 || !path.insert(id) {
                    return Err(ProjectError::Invalid);
                }
                let Some(record) = records.get(&id) else { return Err(ProjectError::Invalid) };
                if let Some(source) = record.mask_source_id {
                    let Some(source_record) = records.get(&source) else { return Err(ProjectError::Invalid) };
                    if record.is_group.unwrap_or(false)
                        || source_record.is_group == Some(true)
                        || source_record.adjustment.is_some()
                    {
                        return Err(ProjectError::Invalid);
                    }
                }
                current = record.mask_source_id;
            }
        }
        Ok(())
    }
}

/// The alert `deleteWithLiveMaskChoice` puts up. Its wording is the Swift alert's; the port's sheet
/// shows it and hands the choice back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveMaskDeleteAlert {
    pub message: String,
    pub informative: String,
    /// "Bake and Delete"
    pub bake: &'static str,
    /// "Cancel"
    pub cancel: &'static str,
    /// "Remove Links and Delete"
    pub remove_links: &'static str,
}

/// What the sheet answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveMaskDeleteChoice {
    BakeAndDelete,
    Cancel,
    RemoveLinksAndDelete,
}

impl EditorSession {
    /// `canLinkMask(source:target:)`: an image layer that is not an adjustment may supply, and the
    /// target must be a layer that is not a folder, and the result must still be a valid graph.
    pub fn can_link_mask(&self, source: Id, target: Id) -> bool {
        if !self.can_edit_layers() || source == target {
            return false;
        }
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else { return false };
        if !layers
            .iter()
            .any(|layer| layer.id == source && !layer.is_group && layer.adjustment.is_none())
            || !layers.iter().any(|layer| layer.id == target && !layer.is_group)
        {
            return false;
        }
        let mut records: Vec<ProjectLayerRecord> = layers.iter().map(ImageLayer::hierarchy_record).collect();
        if let Some(record) = records.iter_mut().find(|record| record.id == target) {
            record.mask_source_id = Some(source);
        }
        LiveMaskGraph::validate(&records).is_ok()
    }

    /// `linkMask(source:target:)`: clips the target to the source as one undo step. Re-linking to the
    /// same source is a no-op success.
    #[must_use]
    pub fn link_mask(&mut self, source: Id, target: Id) -> bool {
        if !self.can_link_mask(source, target) {
            return false;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == target))
        else {
            return false;
        };
        if self.document.as_ref().is_some_and(|document| document.layers[index].mask_source_id == Some(source)) {
            return true;
        }
        self.begin_edit("Create Clipping Mask");
        if let Some(document) = self.document.as_mut() {
            document.layers[index].mask_source_id = Some(source);
        }
        self.end_edit();
        true
    }

    /// `adoptClipping(_:in:)`: a layer dropped into the middle of a clipping group joins it, as in
    /// Photoshop — between a base and a layer clipped to it, it is clipped to that base too. Run while
    /// the layers are being rearranged, before [`Self::release_detached_clipping`].
    pub fn adopt_clipping(id: Id, layers: &mut [ImageLayer]) {
        let Some(layer) = layers.iter().find(|layer| layer.id == id) else { return };
        if layer.is_group {
            return;
        }
        let parent = layer.parent_id;
        let siblings: Vec<usize> = layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.parent_id == parent)
            .map(|(index, _)| index)
            .collect();
        let Some(position) = siblings.iter().position(|index| layers[*index].id == id) else { return };
        if position == 0 || position + 1 >= siblings.len() {
            return;
        }
        let Some(source) = layers[siblings[position + 1]].mask_source_id else { return };
        if source == id {
            return;
        }
        let below = siblings[position - 1];
        if layers[below].id != source && layers[below].mask_source_id != Some(source) {
            return;
        }
        let Some(index) = layers.iter().position(|layer| layer.id == id) else { return };
        layers[index].mask_source_id = Some(source);
    }

    /// `removeLiveMask(from:)`: releasing a base releases its clipped children above it that share that
    /// base; releasing a child leaves lower siblings untouched.
    pub fn remove_live_mask(&mut self, target: Id) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else { return };
        let Some(target_layer) = document.layers.iter().find(|layer| layer.id == target) else { return };
        let Some(source) = target_layer.mask_source_id else { return };
        let parent = target_layer.parent_id;
        let siblings: Vec<(Id, Option<Id>)> = document
            .layers
            .iter()
            .filter(|layer| layer.parent_id == parent)
            .map(|layer| (layer.id, layer.mask_source_id))
            .collect();
        let Some(target_index) = siblings.iter().position(|(id, _)| *id == target) else { return };
        let releases: Vec<Id> = siblings[target_index..]
            .iter()
            .take_while(|(id, sibling_source)| *id == target || *sibling_source == Some(source))
            .map(|(id, _)| *id)
            .collect();
        self.begin_edit("Release Clipping Mask");
        for id in releases {
            if let Some(index) = self
                .document
                .as_ref()
                .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
            {
                if let Some(document) = self.document.as_mut() {
                    document.layers[index].mask_source_id = None;
                }
            }
        }
        self.end_edit();
    }

    /// `canToggleClippingMask(_:)`: an unclipped layer can clip to the sibling below it, sharing that
    /// sibling's base when it is already clipped.
    pub fn can_toggle_clipping_mask(&self, id: Id) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else { return false };
        let Some(layer) = layers.iter().find(|layer| layer.id == id) else { return false };
        if layer.is_group {
            return false;
        }
        if layer.mask_source_id.is_some() {
            return true;
        }
        let parent = layer.parent_id;
        let siblings: Vec<&ImageLayer> = layers.iter().filter(|layer| layer.parent_id == parent).collect();
        let Some(index) = siblings.iter().position(|layer| layer.id == id) else { return false };
        if index == 0 || siblings[index - 1].is_group {
            return false;
        }
        let below = siblings[index - 1];
        self.can_link_mask(below.mask_source_id.unwrap_or(below.id), id)
    }

    /// `toggleClippingMask(_:)`: clips to the next lower sibling, or releases the layer's own clip.
    pub fn toggle_clipping_mask(&mut self, id: Id) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else { return };
        let Some(layer) = layers.iter().find(|layer| layer.id == id) else { return };
        if layer.is_group {
            return;
        }
        if layer.mask_source_id.is_some() {
            self.remove_live_mask(id);
            return;
        }
        let parent = layer.parent_id;
        let siblings: Vec<(Id, Option<Id>, bool)> = layers
            .iter()
            .filter(|layer| layer.parent_id == parent)
            .map(|layer| (layer.id, layer.mask_source_id, layer.is_group))
            .collect();
        let Some(index) = siblings.iter().position(|(sibling, _, _)| *sibling == id) else { return };
        if index == 0 {
            return;
        }
        let (below, below_source, below_is_group) = siblings[index - 1];
        if below_is_group {
            return;
        }
        self.link_mask(below_source.unwrap_or(below), id);
    }

    /// `releaseDetachedClipping(in:)`: a moved layer stops clipping when it no longer belongs to the
    /// contiguous stack above its base.
    pub fn release_detached_clipping(layers: &mut [ImageLayer]) {
        let mut parents: HashMap<Option<Id>, Vec<usize>> = HashMap::new();
        for (index, layer) in layers.iter().enumerate() {
            parents.entry(layer.parent_id).or_default().push(index);
        }
        let mut release: HashSet<Id> = HashSet::new();
        for stack in parents.values() {
            let mut base: Option<Id> = None;
            for index in stack {
                let layer = &layers[*index];
                if let Some(source) = layer.mask_source_id {
                    if base != Some(source) {
                        release.insert(layer.id);
                        base = Some(layer.id);
                    }
                } else {
                    base = if layer.is_group { None } else { Some(layer.id) };
                }
            }
        }
        for layer in layers.iter_mut() {
            if release.contains(&layer.id) {
                layer.mask_source_id = None;
            }
        }
    }

    /// The layers that stay and are supplied by the ones being deleted — what
    /// `delete_with_live_mask_choice` decides between baking and unlinking.
    pub fn live_mask_delete_targets(&self, ids: &[Id]) -> Vec<Id> {
        let removed = self.removed_layer_ids(ids);
        self.document
            .as_ref()
            .map(|document| {
                document
                    .layers
                    .iter()
                    .filter(|layer| !removed.contains(&layer.id) && layer.mask_source_id.is_some_and(|source| removed.contains(&source)))
                    .map(|layer| layer.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The alert `deleteWithLiveMaskChoice` shows, or `None` when none of the layers supply live masks
    /// to layers that stay (the delete then runs without asking).
    pub fn live_mask_delete_alert(&self, ids: &[Id]) -> Option<LiveMaskDeleteAlert> {
        if self.live_mask_delete_targets(ids).is_empty() {
            return None;
        }
        Some(LiveMaskDeleteAlert {
            message: if ids.len() == 1 {
                "This layer supplies a live mask".to_string()
            } else {
                "These layers supply live masks".to_string()
            },
            informative: "Bake keeps the current masked appearance in the dependent layers’ pixels. Remove Links reveals their pixels. You can undo either choice.".to_string(),
            bake: "Bake and Delete",
            cancel: "Cancel",
            remove_links: "Remove Links and Delete",
        })
    }

    /// `deleteWithLiveMaskChoice(_:)`: when layers being deleted supply live masks to layers that stay,
    /// asks (here: takes the sheet's answer) whether to bake or unlink, then deletes them all. Returns
    /// false, having done nothing, when none do — the caller deletes them normally.
    pub fn delete_with_live_mask_choice(&mut self, ids: &[Id], choice: LiveMaskDeleteChoice, host: &dyn SessionHost) -> bool {
        let targets = self.live_mask_delete_targets(ids);
        if targets.is_empty() {
            return false;
        }
        match choice {
            LiveMaskDeleteChoice::RemoveLinksAndDelete => {
                self.finish_deleting_layers(ids, &HashMap::new());
                true
            }
            LiveMaskDeleteChoice::Cancel => true,
            LiveMaskDeleteChoice::BakeAndDelete => {
                let Some(snapshot) = self.project_snapshot() else { return true };
                self.is_project_busy = true;
                let mut baked: HashMap<Id, ImportedImage> = HashMap::new();
                let mut failure = None;
                for target in targets {
                    match host.bake_live_mask(&snapshot, target) {
                        Ok(Some(asset)) => {
                            baked.insert(target, asset);
                        }
                        Ok(None) => {}
                        Err(message) => {
                            failure = Some(message);
                            break;
                        }
                    }
                }
                self.is_project_busy = false;
                match failure {
                    Some(message) => self.brush_error = Some(message),
                    None => self.finish_deleting_layers(ids, &baked),
                }
                true
            }
        }
    }

    /// `finishDeletingLayer(_:baked:)`: deletes one layer (and, for a folder, its contents), unlinks
    /// what it supplied, and installs the baked pixels for the layers that keep the appearance.
    pub fn finish_deleting_layer(&mut self, id: Id, baked: &HashMap<Id, ImportedImage>) {
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
        else {
            return;
        };
        let mut removed = self.descendant_ids(id);
        removed.insert(id);
        self.begin_edit("Delete Layer");
        if let Some(document) = self.document.as_mut() {
            document.layers.retain(|layer| !removed.contains(&layer.id));
            for layer in document.layers.iter_mut() {
                if layer.mask_source_id.is_some_and(|source| removed.contains(&source)) {
                    layer.mask_source_id = None;
                    if let Some(asset) = baked.get(&layer.id) {
                        layer.asset = Some(asset.clone());
                    }
                }
            }
        }
        if self.active_layer_id.is_some_and(|active| removed.contains(&active)) {
            let layers = self.document.as_ref().map(|document| document.layers.len()).unwrap_or(0);
            let next = if layers == 0 {
                None
            } else {
                self.document
                    .as_ref()
                    .and_then(|document| document.layers.get(index.min(layers - 1)))
                    .map(|layer| layer.id)
            };
            self.set_active_layer(next);
        }
        self.end_edit();
    }

    /// `finishDeletingLayers(_:baked:)`: deletes several layers (a folder with its contents) as one
    /// undo step.
    pub fn finish_deleting_layers(&mut self, ids: &[Id], baked: &HashMap<Id, ImportedImage>) {
        if ids.len() <= 1 {
            if let Some(id) = ids.first() {
                self.finish_deleting_layer(*id, baked);
            }
            return;
        }
        self.begin_edit("Delete Layers");
        for id in ids {
            self.finish_deleting_layer(*id, baked);
        }
        self.end_edit();
    }

    /// The layers being deleted with everything inside them (`descendantIDs(of:)` union the ids).
    fn removed_layer_ids(&self, ids: &[Id]) -> HashSet<Id> {
        let mut removed: HashSet<Id> = HashSet::new();
        for id in ids {
            removed.extend(self.descendant_ids(*id));
            removed.insert(*id);
        }
        removed
    }
}
