//! Adjustment layers: adding them, and the dialog that edits their settings.
//!
//! Ports `Document/LayerAdjustment.swift`'s `extension EditorSession` (`addAdjustment`,
//! `updateAdjustment`) and all of `Document/AdjustmentEditing.swift` (`beginAdjustmentEditing`,
//! `previewAdjustmentEditing`, `finishAdjustmentEditing`, `editedAdjustment`).
//!
//! **Async → sync.** `beginAdjustmentEditing` was `async` because it flattened the sampling image with
//! `await ImageExporter.shared.render(source)`. The port renders through [`SessionHost::render`] on the
//! calling thread; the guard after the render (`adjustmentEditingID == id`, `adjustmentOriginal == nil`)
//! and the failure path (`brushError`, `adjustmentEditingID = nil`) are exactly the Swift ones.
//!
//! This module names the filter-editing editors (`LevelsEdit`, `HueSaturationEdit`, `FilterEdit`) the
//! way their own modules must expose them:
//! `LevelsEdit::new(layer, selection) -> Result<_, CoreError>` with `settings: LevelsSettings`,
//! `start_histogram()` and `cancel_previews()`; `HueSaturationEdit::new(layer_id, original, selection,
//! transform)` with `settings: HueSaturationSettings` and `cancel_previews()`; `FilterEdit::new(kind,
//! layer, selection, settings)` with `settings: FilterSettings` and `cancel_previews()`.

use std::sync::atomic::{AtomicU32, Ordering};

use compositor_core::document::ImageLayer;
use compositor_core::geom::Point;
use compositor_core::image_ops::FilterSettings;
use compositor_core::imported_image::ImportedImage;
use compositor_core::layer_adjustment::{AdjustmentColor, AdjustmentKind, GradientMapSettings, LayerAdjustment};
use compositor_core::Id;

use crate::filters::{FilterEdit, HueSaturationEdit, LevelsEdit};
use crate::projects::{ProjectSnapshot, SessionHost};
use crate::EditorSession;

impl EditorSession {
    /// `addAdjustment(_:)`: a new adjustment layer above the active one, in the active folder when a
    /// folder is selected, as one undo step, and — unless it is Invert, which has nothing to set —
    /// with its dialog open.
    pub fn add_adjustment(&mut self, kind: AdjustmentKind) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(document) = self.document.as_ref() else { return };
        if document.layers.len() >= 10_000 {
            return;
        }
        let document_size = document.size();
        let mut layer = ImageLayer::blank(kind.raw_value(), document_size);
        let mut adjustment = LayerAdjustment::new(kind);
        // A new Gradient Map runs from the foreground to the background color, as in Photoshop; each
        // Grain layer gets a pattern of its own.
        if kind == AdjustmentKind::GradientMap {
            adjustment.gradient_map_settings = Some(GradientMapSettings {
                shadows: AdjustmentColor::from(self.foreground_color),
                highlights: AdjustmentColor::from(self.background_color),
                ..GradientMapSettings::default()
            });
        }
        if kind == AdjustmentKind::Grain {
            let mut grain = adjustment.grain_settings.clone().unwrap_or_default();
            grain.seed = random_seed();
            adjustment.grain_settings = Some(grain);
        }
        if kind == AdjustmentKind::AddNoise {
            adjustment.noise_seed = Some(random_seed());
        }
        layer.adjustment = Some(adjustment);
        let active = self.active_layer();
        layer.parent_id = if active.is_some_and(|layer| layer.is_group) {
            self.active_layer_id
        } else {
            active.and_then(|layer| layer.parent_id)
        };
        let parent = layer.parent_id;
        let index = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == self.active_layer_id))
            .map(|index| index + 1)
            .unwrap_or_else(|| self.document.as_ref().map_or(0, |document| document.layers.len()));
        let layer_id = layer.id;
        self.begin_edit(&format!("New {} Adjustment", kind.raw_value()));
        if let Some(document) = self.document.as_mut() {
            document.layers.insert(index.min(document.layers.len()), layer);
        }
        if let Some(parent) = parent {
            self.collapsed_group_ids.remove(&parent);
        }
        self.set_active_layer(Some(layer_id));
        self.end_edit();
        // Invert has nothing to set, so the new layer just applies rather than opening an editor.
        if kind.is_editable() {
            self.adjustment_editing_id = Some(layer_id);
        }
    }

    /// `updateAdjustment(_:value:)`: writes the dialog's settings into the layer. Not an undo step of
    /// its own — the dialog's `beginEdit`/`endEdit` bracket the whole edit — but it bumps the brush
    /// revision so the canvas redraws.
    pub fn update_adjustment(&mut self, id: Id, value: LayerAdjustment) {
        if !value.is_valid() {
            return;
        }
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == id))
        else {
            return;
        };
        if let Some(document) = self.document.as_mut() {
            document.layers[index].adjustment = Some(value);
        }
        self.brush_revision += 1;
    }

    /// `beginAdjustmentEditing(_:)`: flattens what is under the adjustment layer, hides everything from
    /// it upwards, and opens the editor its kind uses. Returns whether the dialog opened.
    pub fn begin_adjustment_editing(&mut self, id: Id, host: &dyn SessionHost) -> bool {
        if self.adjustment_editing_id != Some(id) || self.adjustment_original.is_some() {
            return false;
        }
        if self.levels.is_some() || self.hue_saturation.is_some() || self.filter_edit.is_some() {
            return false;
        }
        let Some(snapshot) = self.project_snapshot() else { return false };
        let Some(original) = snapshot
            .manifest
            .layers
            .iter()
            .find(|record| record.id == id)
            .and_then(|record| record.adjustment.clone())
        else {
            return false;
        };
        // Keep records for live-mask references and group ancestry, but exclude the adjustment itself
        // and everything above it from the sampling image.
        let mut manifest = snapshot.manifest.clone();
        let underneath = compositor_core::groups::LayerHierarchy::entries(&manifest.layers, false, &std::collections::HashSet::new())
            .into_iter()
            .take_while(|entry| entry.layer.id != id)
            .map(|entry| entry.layer.id)
            .collect::<std::collections::HashSet<_>>();
        for record in manifest.layers.iter_mut() {
            if !record.is_group.unwrap_or(false) && !underneath.contains(&record.id) {
                record.is_visible = false;
            }
        }
        let source = ProjectSnapshot { manifest, images: snapshot.images.clone(), masks: snapshot.masks.clone() };
        let raster = match host.render(&source) {
            Ok(raster) => raster,
            Err(message) => {
                if self.adjustment_editing_id != Some(id) {
                    return false;
                }
                self.adjustment_editing_id = None;
                self.brush_error = Some(message);
                return false;
            }
        };
        if self.adjustment_editing_id != Some(id) || self.adjustment_original.is_some() {
            return false;
        }
        let asset = ImportedImage::new(
            raster.materialize(),
            raster.thumbnail(96.0),
            "Adjustment input",
        );
        let layer = ImageLayer::from_asset(asset.clone(), Point::ZERO);
        let layer_id = layer.id;
        let transform = layer.transform;
        match original.kind {
            AdjustmentKind::Invert => {}
            AdjustmentKind::Levels => {
                let Ok(mut edit) = LevelsEdit::new(layer, None) else {
                    self.adjustment_editing_id = None;
                    return false;
                };
                edit.settings = original.levels.clone();
                // The histogram is computed off the UI thread; the editor owns that job
                // (`edit.histogramTask` in Swift).
                edit.start_histogram();
                self.levels = Some(edit);
            }
            AdjustmentKind::Hsv => {
                let Ok(mut edit) = HueSaturationEdit::new(layer_id, asset, None, transform) else {
                    self.adjustment_editing_id = None;
                    return false;
                };
                edit.settings = original.resolved_hsv();
                self.hue_saturation = Some(edit);
            }
            _ => {
                let settings = FilterSettings {
                    curves: original.curves.clone(),
                    exposure: original.exposure(),
                    gradient_map: original.gradient_map(),
                    grain: original.grain(),
                    black_white: original.black_white(),
                    color_balance: original.color_balance(),
                    radius: original.gaussian_radius(),
                    angle: original.resolved_motion_angle(),
                    distance: original.resolved_motion_distance(),
                    amount: original.resolved_noise_amount(),
                    gaussian: original.resolved_noise_gaussian(),
                    monochromatic: original.resolved_noise_monochromatic(),
                    ..FilterSettings::default()
                };
                let kind = original.kind.filter_kind().unwrap_or(compositor_core::image_ops::FilterKind::Curves);
                let Ok(edit) = FilterEdit::new(kind, layer, None, settings) else {
                    self.adjustment_editing_id = None;
                    return false;
                };
                self.filter_edit = Some(edit);
            }
        }
        self.adjustment_original = Some(original);
        self.begin_edit(&format!("Edit {} Adjustment", original.kind.raw_value()));
        true
    }

    /// `previewAdjustmentEditing(preview:)`: the dialog's OK-preview — its settings, or the original
    /// ones back again. False when no dialog is open.
    pub fn preview_adjustment_editing(&mut self, preview: bool) -> bool {
        let (Some(id), Some(original)) = (self.adjustment_editing_id, self.adjustment_original.clone()) else {
            return false;
        };
        let Some(value) = self.edited_adjustment() else { return false };
        self.update_adjustment(id, if preview { value } else { original });
        true
    }

    /// `finishAdjustmentEditing(commit:)`: OK keeps the editor's settings, Cancel (including the
    /// window's close button) restores the originals. Always ends the undo step the dialog opened.
    pub fn finish_adjustment_editing(&mut self, commit: bool) -> bool {
        let (Some(id), Some(original)) = (self.adjustment_editing_id, self.adjustment_original.clone()) else {
            return false;
        };
        let value = if commit { self.edited_adjustment().unwrap_or_else(|| original.clone()) } else { original };
        self.update_adjustment(id, value);
        // Swift cancelled the open editors' preview and histogram tasks here.
        if let Some(levels) = self.levels.as_ref() {
            levels.cancel_previews();
        }
        if let Some(filter) = self.filter_edit.as_ref() {
            filter.cancel_previews();
        }
        if let Some(hsv) = self.hue_saturation.as_ref() {
            hsv.cancel_previews();
        }
        self.hue_saturation_pending = None;
        self.hue_sample_mode = None;
        self.hue_targeting = false;
        self.hue_target_drag = None;
        self.levels = None;
        self.hue_saturation = None;
        self.filter_edit = None;
        self.end_edit();
        self.adjustment_original = None;
        self.adjustment_editing_id = None;
        self.canvas_focus_request += 1;
        self.brush_revision += 1;
        true
    }

    /// `editedAdjustment`: the value the open editor would carry back, or nil when the editor it needs
    /// is not the one open.
    fn edited_adjustment(&self) -> Option<LayerAdjustment> {
        let mut value = self.adjustment_original.clone()?;
        match value.kind {
            // Nothing to carry back: Invert has no settings.
            AdjustmentKind::Invert => {}
            AdjustmentKind::Levels => {
                let levels = self.levels.as_ref()?;
                value.levels = levels.settings.clone();
            }
            AdjustmentKind::Hsv => {
                let hsv = self.hue_saturation.as_ref()?;
                value.hsv_settings = Some(hsv.settings.clone());
            }
            _ => {
                let edit = self.filter_edit.as_ref()?;
                let settings = &edit.settings;
                match value.kind {
                    AdjustmentKind::Exposure => value.exposure_settings = Some(settings.exposure.clone()),
                    AdjustmentKind::GradientMap => value.gradient_map_settings = Some(settings.gradient_map.clone()),
                    AdjustmentKind::Grain => value.grain_settings = Some(settings.grain.clone()),
                    AdjustmentKind::BlackWhite => value.black_white_settings = Some(settings.black_white.clone()),
                    AdjustmentKind::ColorBalance => value.color_balance_settings = Some(settings.color_balance.clone()),
                    AdjustmentKind::GaussianBlur => value.blur_radius = Some(settings.radius),
                    AdjustmentKind::MotionBlur => {
                        value.motion_angle = Some(settings.angle);
                        value.motion_distance = Some(settings.distance);
                    }
                    AdjustmentKind::AddNoise => {
                        value.noise_amount = Some(settings.amount);
                        value.noise_gaussian = Some(settings.gaussian);
                        value.noise_monochromatic = Some(settings.monochromatic);
                    }
                    _ => value.curves = settings.curves.clone(),
                }
            }
        }
        Some(value)
    }
}

/// A fresh seed of its own (`UInt32.random(in: .min ... .max)`), so every Grain and Add Noise
/// adjustment layer gets its own pattern. The port has no RNG dependency; the clock, the process and a
/// per-call counter go through SplitMix64's finalizer so consecutive calls differ.
fn random_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = now
        .wrapping_add(std::process::id().wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed) as u64);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}
