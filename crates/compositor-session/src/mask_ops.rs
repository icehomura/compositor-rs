//! Layer-mask commands: adding (from a selection or solid), enabling and disabling, deleting,
//! copying between layers, linking and unlinking, targeting a layer's mask instead of its pixels,
//! and showing a mask alone on the canvas.
//!
//! Ported from the `extension EditorSession` parts of `Document/LayerMask.swift` — minus the mask
//! transform, which lives with the other transform state in [`crate::transform`]
//! (`displayed_mask_placement`, `commit_mask_transform`), and the mask-alone/drawing helpers the
//! canvas uses. The clipping (live) mask commands — `canLinkMask`/`linkMask`,
//! `canToggleClippingMask`/`toggleClippingMask`, `removeLiveMask` — live in [`crate::live_mask`],
//! and loading a mask's or layer's pixels as a selection (`MaskTracing`) lives in
//! [`crate::selection`].

use std::sync::Arc;

use compositor_core::document::{ImageLayer, ProjectError};
use compositor_core::geom::Rect;
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::{LayerMask, MaskDistortPreviewCache};
use compositor_core::layer_transform::pixel_to_document;
use compositor_core::limits::MAX_SURFACE_PIXELS;
use compositor_core::{Id, SharedGray};
use compositor_pixels::canvas::Canvas;
use compositor_pixels::warp::DistortWarp;
use compositor_render::live_mask_renderer::MaskClip;

use crate::session::EditorSession;
use crate::transform::mask_background;

impl EditorSession {
    /// Layers and folders alike take a mask.
    pub fn can_edit_mask(&self) -> bool {
        self.can_edit_layers() && self.selected_layer_ids.len() == 1 && self.active_layer().is_some()
    }

    /// Targets a layer's mask instead of its pixels (`selectLayerTarget(_:mask:)`): the layer
    /// becomes the active one, and what the tools paint is decided by `mask` — which only sticks
    /// when the layer has a mask at all.
    pub fn select_layer_target(&mut self, id: Id, mask: bool) {
        self.effect_selection = None;
        if self.is_project_busy || self.is_importing || self.brush_stroke.is_some() {
            return;
        }
        self.resolve_gradient();
        self.select_layer(Some(id));
        self.is_mask_selected = mask && self.active_layer().is_some_and(|layer| layer.mask.is_some());
    }

    /// Option-click on a mask thumbnail: shows that mask alone on the canvas, or, when it already is,
    /// the composite again. Either way the mask stays the paint target.
    pub fn toggle_mask_alone(&mut self, id: Id) {
        let showing = self.mask_alone_layer().map(|layer| layer.id) == Some(id);
        self.select_layer_target(id, true);
        if self.active_layer_id != Some(id) || !self.is_mask_selected {
            return;
        }
        self.views_mask_alone = !showing;
    }

    /// Adding a mask from the Layers panel, as Photoshop's Add Layer Mask button does: with no
    /// selection, a mask all white (reveal) or all black (hide); with a selection, Reveal Selection
    /// (white inside, black outside) or, for Option-click, Hide Selection. The selection is used up
    /// and deselected in the same undo step.
    pub fn add_mask(&mut self, revealing: bool) {
        let Some(selection) = self.selection().cloned() else {
            self.add_layer_mask(revealing);
            return;
        };
        if !self.can_edit_mask() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else { return };
        if layer.mask.is_some() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|candidate| candidate.id == layer.id))
        else {
            return;
        };
        // Mask pixels cover the layer's own pixel grid, like every other mask.
        let width = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.width())
            .unwrap_or_else(|| layer.size().width.round() as usize);
        let height = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.height())
            .unwrap_or_else(|| layer.size().height.round() as usize);
        if width == 0 || height == 0 || width.saturating_mul(height) > MAX_SURFACE_PIXELS {
            self.brush_error = Some(ProjectError::TooLarge.to_string());
            return;
        }
        let Some(canvas_size) = self.document.as_ref().map(|document| document.size()) else {
            return;
        };
        let clip = self.selection_clip_for(&selection, canvas_size);
        let mut canvas = Canvas::new_gray(width, height);
        canvas.set_fill_gray(if revealing { 0.0 } else { 1.0 });
        canvas.fill_rect(Rect::new(0.0, 0.0, width as f64, height as f64));
        canvas.concatenate(pixel_to_document(&layer.transform, width, height).inverted());
        // `SelectionClip.apply(to:)`: no coverage clips everything away.
        match clip.coverage.as_ref() {
            Some(coverage) if !clip.rect.is_empty() => canvas.clip_to_image(coverage, clip.rect),
            _ => canvas.clip_to_zero(),
        }
        canvas.set_fill_gray(if revealing { 1.0 } else { 0.0 });
        canvas.fill_rect(clip.rect);
        let image = canvas.snapshot_gray();
        match LayerMask::asset(PixelImage::Gray(Arc::new(image))) {
            Err(error) => self.brush_error = Some(error.to_string()),
            Ok(asset) => {
                self.finish_opacity_edit();
                self.begin_edit(if revealing { "Reveal Selection" } else { "Hide Selection" });
                if let Some(document) = self.document.as_mut() {
                    document.layers[index].mask = Some(LayerMask::new(asset));
                    document.selection = None;
                }
                self.is_mask_selected = true;
                self.end_edit();
            }
        }
    }

    /// A plain all-white (reveal) or all-black (hide) mask, whatever is selected.
    pub fn add_layer_mask(&mut self, revealing: bool) {
        if !self.can_edit_mask() || self.active_layer().is_some_and(|layer| layer.mask.is_some()) {
            return;
        }
        let Some(mask) = LayerMask::solid(revealing) else { return };
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| Some(layer.id) == self.active_layer_id))
        else {
            return;
        };
        self.finish_opacity_edit();
        self.begin_edit(if revealing { "Add Reveal-All Mask" } else { "Add Hide-All Mask" });
        if let Some(document) = self.document.as_mut() {
            document.layers[index].mask = Some(mask);
        }
        self.is_mask_selected = true;
        self.end_edit();
    }

    /// Enable Mask / Disable Mask from the layer's menu: a disabled mask draws nothing, and turns the
    /// canvas's mask-alone view off with it.
    pub fn toggle_layer_mask(&mut self) {
        if !self.can_edit_mask() || !self.active_layer().is_some_and(|layer| layer.mask.is_some()) {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| Some(layer.id) == self.active_layer_id))
        else {
            return;
        };
        let enabled = self
            .document
            .as_ref()
            .and_then(|document| document.layers[index].mask.as_ref())
            .is_some_and(|mask| mask.is_enabled);
        self.finish_opacity_edit();
        self.begin_edit(if enabled {
            "Disable Layer Mask"
        } else {
            "Enable Layer Mask"
        });
        if let Some(document) = self.document.as_mut() {
            if let Some(mask) = document.layers[index].mask.as_mut() {
                mask.is_enabled = !mask.is_enabled;
            }
        }
        self.end_edit();
    }

    /// Delete Mask: the mask goes, and with it the mask target.
    pub fn delete_layer_mask(&mut self) {
        if !self.can_edit_mask() || !self.active_layer().is_some_and(|layer| layer.mask.is_some()) {
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
        self.begin_edit("Delete Layer Mask");
        if let Some(document) = self.document.as_mut() {
            document.layers[index].mask = None;
        }
        self.is_mask_selected = false;
        self.end_edit();
    }

    /// Whether an Option-drag can drop a copy of `source`'s mask on `target`.
    pub fn can_copy_mask(&self, source: Id, target: Id) -> bool {
        if !self.can_edit_layers() || source == target {
            return false;
        }
        let Some(layers) = self.document.as_ref().map(|document| &document.layers) else {
            return false;
        };
        if !layers
            .iter()
            .any(|layer| layer.id == source && layer.mask.is_some())
        {
            return false;
        }
        layers.iter().any(|layer| layer.id == target && !layer.is_group)
    }

    /// Option-dragging a mask thumbnail onto another layer: a copy of the mask, sitting where it sits
    /// on the document, replacing any mask the layer had.
    pub fn copy_mask(&mut self, source: Id, target: Id) {
        if !self.can_copy_mask(source, target) {
            return;
        }
        let Some(layers) = self.document.as_ref().map(|document| document.layers.clone()) else {
            return;
        };
        let Some(from) = layers.iter().find(|layer| layer.id == source).cloned() else {
            return;
        };
        let Some(mut mask) = from.mask.clone() else { return };
        let Some(index) = layers.iter().position(|layer| layer.id == target) else {
            return;
        };
        self.commit_transform();
        self.finish_opacity_edit();
        // The copy is placed where the source mask sits on the document: its own placement, else the
        // source layer's transform.
        mask.placement = Some(from.mask_transform());
        self.begin_edit(if layers[index].mask.is_none() {
            "Copy Layer Mask"
        } else {
            "Replace Layer Mask"
        });
        if let Some(document) = self.document.as_mut() {
            document.layers[index].mask = Some(mask);
        }
        self.select_layer(Some(target));
        self.is_mask_selected = true;
        self.end_edit();
    }

    /// The link between a layer and its mask: linked they move together; unlinked each transforms on
    /// its own.
    pub fn toggle_mask_link(&mut self, id: Id) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
        else {
            return;
        };
        let Some(linked) = self
            .document
            .as_ref()
            .and_then(|document| document.layers[index].mask.as_ref())
            .map(|mask| mask.is_linked)
        else {
            return;
        };
        self.commit_transform();
        self.finish_opacity_edit();
        self.begin_edit(if linked {
            "Unlink Layer Mask"
        } else {
            "Link Layer Mask"
        });
        if let Some(document) = self.document.as_mut() {
            if let Some(mask) = document.layers[index].mask.as_mut() {
                mask.is_linked = !mask.is_linked;
            }
        }
        self.end_edit();
    }

    /// `maskDistortPreview(for:)`: an unlinked mask being distorted on its own — the warped mask
    /// resampled into the layer's grid, for the canvas. The last result is kept while the corners,
    /// the draft, the mask's pixels and the layer are unchanged.
    pub fn mask_distort_preview(&mut self, layer: &ImageLayer) -> Option<SharedGray> {
        let edit = self.transform_edit.as_ref()?;
        if !edit.mask || edit.layer_id != layer.id {
            return None;
        }
        let draft = edit.draft;
        let corners = edit.corners.clone()?;
        let owned = layer.mask.as_ref()?;
        if !owned.is_enabled {
            return None;
        }
        let mask = match &owned.asset.image {
            PixelImage::Gray(gray) => Arc::clone(gray),
            PixelImage::Rgba(_) => return None,
        };
        if let Some(cache) = self.mask_distort_preview_cache.as_ref() {
            if cache.corners == corners
                && cache.draft == draft
                && Arc::ptr_eq(&cache.mask, &mask)
                && cache.layer == layer.transform
            {
                return cache.result.clone();
            }
        }
        let width = layer
            .asset
            .as_ref()
            .map_or_else(|| layer.size().width.round() as usize, |asset| asset.image.width());
        let height = layer
            .asset
            .as_ref()
            .map_or_else(|| layer.size().height.round() as usize, |asset| asset.image.height());
        let result = DistortWarp::warp_mask(
            &owned.asset.image,
            &draft,
            &corners,
            mask_background(&owned.asset.thumbnail),
            Some(2048.0),
        )
        .ok()
        .and_then(|(moved_image, moved_transform)| {
            let asset = ImportedImage::new(moved_image, owned.asset.thumbnail.clone(), owned.asset.name.clone());
            LayerMask::new(asset)
                .clip_image(Some(&moved_transform), &layer.transform, width, height, Some(2048.0))
                .and_then(|image| match image {
                    PixelImage::Gray(gray) => Some(gray),
                    PixelImage::Rgba(_) => None,
                })
        });
        self.mask_distort_preview_cache = Some(MaskDistortPreviewCache {
            corners,
            draft,
            mask,
            layer: layer.transform,
            result: result.clone(),
        });
        result
    }
}
