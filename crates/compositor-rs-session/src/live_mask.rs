//! Live (clipping) masks: which layer's coverage hides which, and what happens to the dependents when
//! a supplying layer is deleted.
//!
//! Port of `Document/LiveLayerMask.swift`'s `extension EditorSession`, `drawLiveComposite` included:
//! that one draws through `compositor-rs-render`'s `LiveMaskRenderer` and the session's
//! `displayedTransform`/`displayedBlendMode`/`displayedMaskPlacement`.
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
use std::sync::Arc;

use compositor_rs_core::document::{CanvasDocument, ImageLayer, ProjectError, ProjectLayerRecord};
use compositor_rs_core::geom::{Point, Rect};
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_mask::FolderMaskClip;
use compositor_rs_core::layer_transform::LayerInterpolationQuality;
use compositor_rs_core::{Id, LayerBlendMode};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_rs_pixels::effects::LayerEffectsRenderer;
use compositor_rs_render::adjustment_surface::AdjustmentSurface;
use compositor_rs_render::layer_renderer::LayerRenderer;
use compositor_rs_render::live_mask_renderer::{LiveMaskRenderer, MaskClip};

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
        // Swift's `linkMask` is `@discardableResult`: the toggle has already checked the guard itself.
        let _ = self.link_mask(below_source.unwrap_or(below), id);
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

impl EditorSession {
    /// `drawLiveComposite(_:in:onSurface:)`: the document as the canvas shows it — live (clipping)
    /// masks, the displayed transforms, folder masks, layer effects and adjustment layers — into
    /// `canvas`. `on_surface` is set on the copy drawn inside an [`AdjustmentSurface`], which keeps
    /// the spatial adjustments' halo off the recursion.
    pub fn draw_live_composite(&self, document: &CanvasDocument, canvas: &mut Canvas, on_surface: bool) {
        if !on_surface && document.layers.iter().any(|layer| layer.adjustment.is_some()) {
            AdjustmentSurface::draw(canvas, 0.0, |surface| self.draw_live_composite(document, surface, true));
            return;
        }
        let records: HashMap<Id, ImageLayer> = document.layers.iter().map(|layer| (layer.id, layer.clone())).collect();
        let mut live = LiveMaskRenderer::new(
            user_space_clip_bounds(canvas),
            |id: Id| records.get(&id).and_then(|layer| layer.mask_source_id),
            |id: Id, target: &mut Canvas| {
                let Some(layer) = records.get(&id) else { return };
                let Some(image) = layer.asset.as_ref().map(|asset| &asset.image) else { return };
                let opacity = layer.effective_opacity(&records);
                let transform = self.displayed_transform(layer);
                let mask = layer.mask.as_ref().and_then(|mask| {
                    let placement = self.displayed_mask_placement(layer);
                    mask.clip_image(placement.as_ref(), &transform, image.width(), image.height(), None)
                });
                let effects = image.as_rgba().and_then(|pixels| {
                    LayerEffectsRenderer::cached(pixels, mask.as_ref().and_then(PixelImage::as_gray), layer.effects.as_ref())
                });
                let mode = self.displayed_blend_mode(layer);
                // Core Graphics blended Color Burn, Color Dodge and Soft Light wrong and lacked the
                // rest of the modes `SeparableBlend.needsSurface` names; `SeparableBlend.draw` sent
                // them through a read-back surface and a Core Image filter. The kernels behind
                // `LayerRenderer::draw` compute every mode directly, so this one draw is the pass.
                if let Some(rendered) = effects {
                    let grown = LayerEffectsRenderer::placed(&transform, &rendered.image, rendered.inset);
                    LayerRenderer::draw(
                        &PixelImage::Rgba(Arc::new(rendered.image)),
                        &grown,
                        grown.center(),
                        1.0,
                        opacity,
                        mode,
                        None,
                        target,
                    );
                } else {
                    LayerRenderer::draw(image, &transform, transform.center(), 1.0, opacity, mode, mask.as_ref(), target);
                }
            },
        );
        live.adjustment = Box::new(|id| records.get(&id).and_then(|layer| layer.adjustment.clone()));
        live.adjustment_opacity =
            Box::new(|id| records.get(&id).map_or(1.0, |layer| layer.effective_opacity(&records)));
        live.adjustment_clip = Box::new(|id, target| {
            let Some(layer) = records.get(&id) else { return };
            let Some(PixelImage::Gray(image)) = layer.mask.as_ref().and_then(|mask| mask.enabled_image()) else {
                return;
            };
            let transform = layer.transform;
            let clip = FolderMaskClip { image: Arc::clone(image), transform };
            apply_folder_mask_clip(target, &clip, transform.center());
        });

        let render_ids: Vec<Id> = document.render_layers().iter().map(|layer| layer.id).collect();
        live.prepare_stacks(
            &render_ids,
            |id| records.get(&id).and_then(|layer| layer.parent_id),
            |id| records.get(&id).map_or(LayerBlendMode::Normal, |layer| self.displayed_blend_mode(layer)),
        );

        // `FolderMaskClip.draw`: every layer is clipped by each folder containing it, a folder's clip
        // placed once, and then the live composite draws the layer itself.
        let mut clips: HashMap<Id, Option<FolderMaskClip>> = HashMap::new();
        for id in &render_ids {
            let mut appliers: Vec<FolderMaskClip> = Vec::new();
            let mut folder = records.get(id).and_then(|layer| layer.parent_id);
            let mut depth = 0;
            while let Some(current) = folder {
                if depth >= 64 {
                    break;
                }
                let clip = clips.entry(current).or_insert_with(|| {
                    let folder = records.get(&current)?;
                    let transform = self.displayed_transform(folder);
                    match folder.mask.as_ref().and_then(|mask| mask.enabled_image()) {
                        Some(PixelImage::Gray(image)) => Some(FolderMaskClip { image: Arc::clone(image), transform }),
                        _ => None,
                    }
                });
                if let Some(clip) = clip.as_ref() {
                    appliers.push(clip.clone());
                }
                folder = records.get(&current).and_then(|layer| layer.parent_id);
                depth += 1;
            }
            if appliers.is_empty() {
                live.draw_composite(*id, canvas);
                continue;
            }
            canvas.save();
            for clip in &appliers {
                apply_folder_mask_clip(canvas, clip, clip.transform.center());
            }
            live.draw_composite(*id, canvas);
            canvas.restore();
        }
    }
}

/// `context.boundingBoxOfClipPath`, in the canvas's drawing space.
fn user_space_clip_bounds(canvas: &Canvas) -> Rect {
    let corners = canvas
        .clip_bounds()
        .corners()
        .map(|point| canvas.user_space_to_device().inverted().applying(point));
    let min_x = corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min);
    let max_x = corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max);
    let min_y = corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min);
    let max_y = corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y).standardized()
}

/// `FolderMaskClip.apply` at its default scale: the mask stretched over the folder's box, clipping
/// what is inside it and leaving the canvas's transform as it found it.
fn apply_folder_mask_clip(canvas: &mut Canvas, clip: &FolderMaskClip, center: Point) {
    let (placement, inverse) = clip.placement(1.0, center);
    canvas.set_interpolation_quality(canvas_quality(clip.transform.sampling.quality()));
    canvas.concatenate(placement);
    canvas.clip_to_image(&clip.image, clip.rect(1.0));
    canvas.concatenate(inverse);
}

/// `LayerSampling.quality` as the canvas's interpolation quality — `compositor-rs-render`'s
/// `canvas_quality`, which is crate-private there.
fn canvas_quality(quality: LayerInterpolationQuality) -> InterpolationQuality {
    match quality {
        LayerInterpolationQuality::None => InterpolationQuality::None,
        LayerInterpolationQuality::Low => InterpolationQuality::Low,
        LayerInterpolationQuality::High => InterpolationQuality::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::buffer::Rgba8Image;
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::geom::Point;
    use compositor_rs_core::imported_image::PixelImage;
    use std::sync::Arc;

    fn asset(name: &str) -> ImportedImage {
        ImportedImage::new(
            PixelImage::Rgba(Arc::new(Rgba8Image::new(4, 4))),
            PixelImage::Rgba(Arc::new(Rgba8Image::new(2, 2))),
            name,
        )
    }

    /// Bottom to top, as the document stores them (`LiveMaskTests.fixture`'s shape).
    fn session_with_layers(count: usize) -> (EditorSession, Vec<Id>) {
        let mut session = EditorSession::default();
        let mut document = CanvasDocument::new(8, 8);
        let mut ids = Vec::new();
        for index in 0..count {
            let layer = ImageLayer::from_asset(asset(&format!("Layer {index}")), Point::ZERO);
            ids.push(layer.id);
            document.layers.push(layer);
        }
        session.document = Some(document);
        session.set_active_layer(ids.last().copied());
        (session, ids)
    }

    fn layer(session: &EditorSession, id: Id) -> &ImageLayer {
        session
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == id))
            .expect("the layer is there")
    }

    /// A host that bakes a fixed asset, so the delete path can be checked without a renderer.
    struct Baker(ImportedImage);
    impl SessionHost for Baker {
        fn bake_live_mask(&self, _snapshot: &compositor_rs_core::document::ProjectSnapshot, _target: Id) -> Result<Option<ImportedImage>, String> {
            Ok(Some(self.0.clone()))
        }
    }

    #[test]
    fn option_click_creates_a_shared_stack_and_releases_it_again() {
        let (mut session, ids) = session_with_layers(3);
        assert!(!session.can_toggle_clipping_mask(ids[0]), "the bottom layer has nothing below it");
        assert!(session.can_toggle_clipping_mask(ids[1]));
        session.toggle_clipping_mask(ids[0]);
        assert_eq!(layer(&session, ids[0]).mask_source_id, None);

        session.toggle_clipping_mask(ids[1]);
        session.toggle_clipping_mask(ids[2]);
        assert_eq!(layer(&session, ids[1]).mask_source_id, Some(ids[0]));
        assert_eq!(layer(&session, ids[2]).mask_source_id, Some(ids[0]), "a later layer joins the base's stack");
        assert_eq!(session.history.undo_name(), "Create Clipping Mask");

        session.toggle_clipping_mask(ids[2]);
        assert_eq!(layer(&session, ids[2]).mask_source_id, None);
        assert_eq!(layer(&session, ids[1]).mask_source_id, Some(ids[0]), "the layer below keeps its clip");
        assert_eq!(session.history.undo_name(), "Release Clipping Mask");

        // Releasing the base releases the stack above it that shares the base.
        session.toggle_clipping_mask(ids[2]);
        session.toggle_clipping_mask(ids[1]);
        assert_eq!(layer(&session, ids[1]).mask_source_id, None);
        assert_eq!(layer(&session, ids[2]).mask_source_id, None);
    }

    #[test]
    fn a_folder_can_neither_supply_nor_take_a_live_mask() {
        let (mut session, ids) = session_with_layers(2);
        session.document.as_mut().expect("a document").layers[0].is_group = true;
        assert!(!session.can_link_mask(ids[0], ids[1]), "a folder cannot supply");
        assert!(!session.can_toggle_clipping_mask(ids[1]));
        session.document.as_mut().expect("a document").layers[1].is_group = true;
        assert!(!session.can_link_mask(ids[0], ids[1]), "a folder cannot take one");
    }

    #[test]
    fn a_cycle_is_refused_and_the_graph_rejects_it() {
        let (mut session, ids) = session_with_layers(2);
        assert!(session.link_mask(ids[0], ids[1]));
        assert!(!session.link_mask(ids[1], ids[0]), "a cycle is refused");
        assert!(!session.link_mask(ids[0], ids[0]), "a layer cannot mask itself");

        let records: Vec<ProjectLayerRecord> = session
            .document
            .as_ref()
            .expect("a document")
            .layers
            .iter()
            .map(ImageLayer::hierarchy_record)
            .collect();
        assert!(LiveMaskGraph::validate(&records).is_ok());
        let mut cyclic = records.clone();
        cyclic[0].mask_source_id = Some(cyclic[1].id);
        assert!(LiveMaskGraph::validate(&cyclic).is_err(), "the graph refuses a cycle");
        let mut duplicate = records.clone();
        duplicate.push(records[0].clone());
        assert!(LiveMaskGraph::validate(&duplicate).is_err(), "the graph refuses a duplicate id");
        let mut folder_source = records.clone();
        folder_source[0].mask_source_id = Some(folder_source[1].id);
        folder_source[1].is_group = Some(true);
        assert!(LiveMaskGraph::validate(&folder_source).is_err(), "the graph refuses a folder source");
    }

    #[test]
    fn the_delete_alert_matches_the_swift_wording() {
        let (mut session, ids) = session_with_layers(3);
        assert!(session.live_mask_delete_alert(&[ids[0]]).is_none(), "nothing supplies a mask yet");
        assert!(session.link_mask(ids[0], ids[1]));
        assert!(session.link_mask(ids[1], ids[2]));
        assert_eq!(session.live_mask_delete_targets(&[ids[0]]), vec![ids[1]]);

        let alert = session.live_mask_delete_alert(&[ids[0]]).expect("the supplier asks");
        assert_eq!(alert.message, "This layer supplies a live mask");
        assert_eq!(
            alert.informative,
            "Bake keeps the current masked appearance in the dependent layers’ pixels. Remove Links reveals their pixels. You can undo either choice."
        );
        assert_eq!((alert.bake, alert.cancel, alert.remove_links), ("Bake and Delete", "Cancel", "Remove Links and Delete"));
        // Two ids go, both supply live masks, and a dependent stays: the plural sheet.
        assert_eq!(
            session
                .live_mask_delete_alert(&[ids[0], ids[1]])
                .expect("two suppliers")
                .message,
            "These layers supply live masks"
        );
        // With every dependent going too, Swift's guard has nothing left to ask about.
        assert!(session.live_mask_delete_alert(&ids).is_none(), "no layer stays that a deleted one supplies");
    }

    #[test]
    fn removing_the_links_deletes_without_baking() {
        let (mut session, ids) = session_with_layers(2);
        assert!(session.link_mask(ids[0], ids[1]));
        assert!(session.delete_with_live_mask_choice(&[ids[0]], LiveMaskDeleteChoice::RemoveLinksAndDelete, &Baker(asset("Baked"))));
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 1);
        let survivor = layer(&session, ids[1]);
        assert_eq!(survivor.mask_source_id, None);
        assert_eq!(survivor.asset.as_ref().map(|asset| asset.name.clone()), Some("Layer 1".to_string()));
    }

    #[test]
    fn baking_installs_the_pixels_the_baker_returned() {
        let (mut session, ids) = session_with_layers(2);
        assert!(session.link_mask(ids[0], ids[1]));
        session.begin_project_operation();
        session.end_project_operation();
        assert!(session.delete_with_live_mask_choice(&[ids[0]], LiveMaskDeleteChoice::BakeAndDelete, &Baker(asset("Baked"))));
        let survivor = layer(&session, ids[1]);
        assert_eq!(survivor.mask_source_id, None);
        assert_eq!(survivor.asset.as_ref().map(|asset| asset.name.clone()), Some("Baked".to_string()));
        assert!(!session.is_project_busy, "the busy flag the bake set is cleared again");
    }

    #[test]
    fn cancelling_the_delete_changes_nothing() {
        let (mut session, ids) = session_with_layers(2);
        assert!(session.link_mask(ids[0], ids[1]));
        assert!(session.delete_with_live_mask_choice(&[ids[0]], LiveMaskDeleteChoice::Cancel, &Baker(asset("Baked"))));
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 2);
        assert_eq!(layer(&session, ids[1]).mask_source_id, Some(ids[0]));
    }

    #[test]
    fn deleting_a_supplier_without_an_alert_is_the_callers_job() {
        let (mut session, ids) = session_with_layers(2);
        assert!(!session.delete_with_live_mask_choice(&[ids[0]], LiveMaskDeleteChoice::RemoveLinksAndDelete, &Baker(asset("Baked"))));
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 2, "nothing was deleted");
    }

    #[test]
    fn deleting_a_layer_installs_baked_pixels_from_the_map() {
        let (mut session, ids) = session_with_layers(2);
        assert!(session.link_mask(ids[0], ids[1]));
        let baked = std::collections::HashMap::from([(ids[1], asset("Baked"))]);
        session.finish_deleting_layer(ids[0], &baked);
        assert_eq!(session.history.undo_name(), "Delete Layer");
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 1);
        let survivor = layer(&session, ids[1]);
        assert_eq!(survivor.mask_source_id, None);
        assert_eq!(survivor.asset.as_ref().map(|asset| asset.name.clone()), Some("Baked".to_string()));
    }

    #[test]
    fn deleting_several_layers_is_one_undo_step() {
        let (mut session, ids) = session_with_layers(3);
        let steps = session.history.undo_count();
        session.finish_deleting_layers(&[ids[0], ids[1]], &std::collections::HashMap::new());
        assert_eq!(session.document.as_ref().expect("a document").layers.len(), 1);
        assert_eq!(session.history.undo_count(), steps + 1, "a batch is one step");
        assert_eq!(session.history.undo_name(), "Delete Layers");
    }

    #[test]
    fn a_layer_dropped_into_a_stack_adopts_its_clip() {
        let (_, ids) = session_with_layers(3);
        // Bottom to top: base, moved, client (clipped to the base).
        let base = ImageLayer::from_asset(asset("Base"), Point::ZERO);
        let moved = ImageLayer::from_asset(asset("Moved"), Point::ZERO);
        let mut client = ImageLayer::from_asset(asset("Client"), Point::ZERO);
        client.mask_source_id = Some(base.id);
        let base_id = base.id;
        let moved_id = moved.id;
        let mut layers = vec![base, moved, client];
        let _ = ids;
        EditorSession::adopt_clipping(moved_id, &mut layers);
        assert_eq!(
            layers.iter().find(|layer| layer.id == moved_id).expect("the moved layer").mask_source_id,
            Some(base_id),
            "dropped between a base and its client, it clips to the base too"
        );
    }

    #[test]
    fn a_layer_moved_out_of_its_stack_stops_clipping() {
        let base = ImageLayer::from_asset(asset("Base"), Point::ZERO);
        let mut client = ImageLayer::from_asset(asset("Client"), Point::ZERO);
        client.mask_source_id = Some(base.id);
        let base_id = base.id;
        let client_id = client.id;
        // Moved below its base: the pair is no longer a contiguous stack.
        let mut layers = vec![client.clone(), base.clone()];
        EditorSession::release_detached_clipping(&mut layers);
        assert_eq!(layers.iter().find(|layer| layer.id == client_id).expect("the client").mask_source_id, None);

        // A chain clipped to the same base keeps its clips.
        let mut second = ImageLayer::from_asset(asset("Second"), Point::ZERO);
        second.mask_source_id = Some(base_id);
        let second_id = second.id;
        let mut chain = vec![base, client, second];
        EditorSession::release_detached_clipping(&mut chain);
        assert_eq!(chain.iter().find(|layer| layer.id == client_id).expect("the client").mask_source_id, Some(base_id));
        assert_eq!(
            chain.iter().find(|layer| layer.id == second_id).expect("the second client").mask_source_id,
            Some(base_id),
            "the stack stays contiguous above its base, so both clients keep the shared clip, as Swift's running-base rule has it"
        );
    }
}
