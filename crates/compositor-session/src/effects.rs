//! Layer effects: the panel's editing lifecycle, the per-kind setters and the undo transactions.
//!
//! Port of the `extension EditorSession` in `Document/LayerEffects.swift` — the half that edits the
//! layer's `LayerEffects` metadata (the rasterizer is `compositor-pixels::effects`, the canvas preview
//! cache is `compositor-render`).
//!
//! This module expects the following on `EditorSession` (`session.rs`):
//! `document: Option<CanvasDocument>`, `active_layer_id: Option<Id>`, `selected_layer_ids`,
//! `is_mask_selected: bool`, `effects_editing: Option<LayerEffectSelection>`,
//! `effects_editing_original: Option<LayerEffects>`, `effect_selection: Option<LayerEffectSelection>`,
//! `color_picker: Option<ColorPickerState>`, `background_color: PaletteColor`, and the commands
//! `active_layer()`, `can_edit_layers()`, `select_layer(Option<Id>)`, `finish_opacity_edit()`,
//! `begin_edit(&str)`, `end_edit()`, `close_color_picker(bool)`.

use compositor_core::document::CanvasDocument;
use compositor_core::layer_effects::{LayerEffectKind, LayerEffectSelection, LayerEffects, StrokeEffect, ColorOverlayEffect};
use compositor_core::Id;

use crate::color::ColorPickerTarget;
use crate::EditorSession;

impl EditorSession {
    /// `canEditEffects`: a layer with pixels of its own, not a folder.
    pub fn can_edit_effects(&self) -> bool {
        self.can_edit_layers()
            && self.active_layer().is_some_and(|layer| !layer.is_group && layer.asset.is_some())
    }

    /// `activeEffects`: the active layer's effects, empty when it has none (or none is active).
    pub fn active_effects(&self) -> LayerEffects {
        self.active_layer().and_then(|layer| layer.effects.clone()).unwrap_or_default()
    }

    /// `editingEffects`: the effects of the layer the panel is open on, even when the selection moved on.
    pub fn editing_effects(&self) -> LayerEffects {
        let Some(editing) = self.effects_editing.as_ref() else { return LayerEffects::default() };
        self.document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == editing.layer_id))
            .and_then(|layer| layer.effects.clone())
            .unwrap_or_default()
    }

    /// `selectedEffect`: the panel's selection, only while it is the active layer's and still exists.
    pub fn selected_effect(&self) -> Option<LayerEffectSelection> {
        let selection = self.effect_selection.as_ref()?;
        if selection.layer_id != self.active_layer_id? || !self.active_effects().contains(selection.kind) {
            return None;
        }
        Some(selection.clone())
    }

    /// `addEffect(_:)`: adds the kind to the active layer if it has none, then opens its panel. A new
    /// stroke or color overlay takes the background color; the foreground is what the layer is usually
    /// painted in.
    pub fn add_effect(&mut self, kind: LayerEffectKind) {
        if !self.can_edit_effects() {
            return;
        }
        let Some(id) = self.active_layer_id else { return };
        if self.effects_editing.as_ref() == Some(&LayerEffectSelection::new(id, kind)) {
            return;
        }
        self.finish_effects_editing(false);
        let original = self.active_effects();
        let mut effects = original.clone();
        let background = self.background_color;
        match kind {
            LayerEffectKind::Stroke if effects.stroke.is_none() => {
                effects.stroke = Some(StrokeEffect { red: background.red, green: background.green, blue: background.blue, ..StrokeEffect::default() });
            }
            LayerEffectKind::Shadow if effects.shadow.is_none() => {
                effects.shadow = Some(Default::default());
            }
            LayerEffectKind::ColorOverlay if effects.color_overlay.is_none() => {
                effects.color_overlay = Some(ColorOverlayEffect { red: background.red, green: background.green, blue: background.blue, ..ColorOverlayEffect::default() });
            }
            LayerEffectKind::InnerShadow if effects.inner_shadow.is_none() => {
                effects.inner_shadow = Some(Default::default());
            }
            LayerEffectKind::OuterGlow if effects.outer_glow.is_none() => {
                effects.outer_glow = Some(Default::default());
            }
            LayerEffectKind::InnerGlow if effects.inner_glow.is_none() => {
                effects.inner_glow = Some(Default::default());
            }
            _ => {}
        }
        self.set_effects(effects, Some(id), &format!("Add {}", kind.raw_value()));
        self.select_effect(kind, id, true);
        self.effects_editing_original = Some(original);
    }

    /// `selectEffect(_:on:editing:)`: points the panel at one of a layer's effects, optionally opening
    /// it for editing.
    pub fn select_effect(&mut self, kind: LayerEffectKind, on: Id, editing: bool) {
        if !self.can_edit_layers()
            || !self
                .document
                .as_ref()
                .and_then(|document| document.layers.iter().find(|layer| layer.id == on))
                .and_then(|layer| layer.effects.as_ref())
                .is_some_and(|effects| effects.contains(kind))
        {
            return;
        }
        let selection = LayerEffectSelection::new(on, kind);
        if editing && self.effects_editing.as_ref() != Some(&selection) {
            self.finish_effects_editing(false);
        }
        self.select_layer(Some(on));
        self.selected_layer_ids = [on].into_iter().collect();
        self.is_mask_selected = false;
        self.effect_selection = Some(selection.clone());
        if editing && self.effects_editing.as_ref() != Some(&selection) {
            if self.aliases_effect_picker() {
                self.close_color_picker(false);
            }
            self.effects_editing_original = self
                .document
                .as_ref()
                .and_then(|document| document.layers.iter().find(|layer| layer.id == on))
                .and_then(|layer| layer.effects.clone());
            self.effects_editing = Some(selection);
        }
    }

    /// `finishEffectsEditing(commit:)`. Cancel restores only this panel's effect, preserving edits to
    /// other effects or layers; for a newly added effect the original value is absent, so Cancel
    /// removes it again.
    pub fn finish_effects_editing(&mut self, commit: bool) {
        let Some(editing) = self.effects_editing.clone() else { return };
        if self.aliases_effect_picker() {
            self.close_color_picker(commit);
        }
        if !commit {
            if let Some(original) = self.effects_editing_original.clone() {
                if let Some(mut effects) = self
                    .document
                    .as_ref()
                    .and_then(|document| document.layers.iter().find(|layer| layer.id == editing.layer_id))
                    .and_then(|layer| layer.effects.clone())
                {
                    match editing.kind {
                        LayerEffectKind::Stroke => effects.stroke = original.stroke,
                        LayerEffectKind::Shadow => effects.shadow = original.shadow,
                        LayerEffectKind::ColorOverlay => effects.color_overlay = original.color_overlay,
                        LayerEffectKind::InnerShadow => effects.inner_shadow = original.inner_shadow,
                        LayerEffectKind::OuterGlow => effects.outer_glow = original.outer_glow,
                        LayerEffectKind::InnerGlow => effects.inner_glow = original.inner_glow,
                    }
                    self.set_effects(effects, Some(editing.layer_id), &format!("Cancel {}", editing.kind.raw_value()));
                }
            }
        }
        self.effects_editing = None;
        self.effects_editing_original = None;
        if self.selected_effect().is_none() {
            self.effect_selection = None;
        }
    }

    /// `setEffects(_:on:name:)`: one undo step (named `name`, `"Layer Effects"` by default) that stores
    /// the effects on a layer that has pixels of its own.
    pub fn set_effects(&mut self, effects: LayerEffects, on: Option<Id>, name: &str) {
        if !self.can_edit_layers() || !effects.is_valid() {
            return;
        }
        let Some(target) = on.or(self.active_layer_id) else { return };
        let Some(index) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().position(|layer| layer.id == target))
        else {
            return;
        };
        let stored = if effects.is_empty() { None } else { Some(effects) };
        let Some(document) = self.document.as_ref() else { return };
        let layer = &document.layers[index];
        if layer.is_group || layer.asset.is_none() || layer.effects == stored {
            return;
        }
        self.finish_opacity_edit();
        self.begin_edit(name);
        if let Some(document) = self.document.as_mut() {
            document.layers[index].effects = stored;
        }
        self.end_edit();
    }

    /// `changeEffects`: a panel edit stays bound to the layer that opened the panel, even if the
    /// selection changes.
    pub fn change_effects(&mut self, change: impl FnOnce(&mut LayerEffects)) {
        let Some(editing) = self.effects_editing.clone() else { return };
        let Some(layer) = self.document.as_ref().and_then(|document| {
            document
                .layers
                .iter()
                .find(|layer| layer.id == editing.layer_id)
        }) else {
            return;
        };
        let Some(stored) = layer.effects.clone() else { return };
        if !stored.contains(editing.kind) {
            return;
        }
        let layer_id = layer.id;
        let mut effects = stored;
        change(&mut effects);
        self.set_effects(effects, Some(layer_id), &format!("Edit {}", editing.kind.raw_value()));
    }

    /// `canCopyEffect(_:from:to:)`: an effect can only be copied onto another layer with pixels of its
    /// own that does not already have it from the same layer.
    pub fn can_copy_effect(&self, kind: LayerEffectKind, from: Id, to: Id) -> bool {
        if !self.can_edit_layers() || from == to {
            return false;
        }
        let Some(document) = self.document.as_ref() else { return false };
        let Some(source) = document.layers.iter().find(|layer| layer.id == from) else { return false };
        let Some(target) = document.layers.iter().find(|layer| layer.id == to) else { return false };
        source.effects.as_ref().is_some_and(|effects| effects.contains(kind))
            && !target.is_group
            && target.asset.is_some()
    }

    /// `copyEffect(_:from:to:)`: replaces the destination's effect with the source's, as one undo step.
    pub fn copy_effect(&mut self, kind: LayerEffectKind, from: Id, to: Id) {
        if !self.can_copy_effect(kind, from, to) {
            return;
        }
        let Some(original) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == from))
            .and_then(|layer| layer.effects.clone())
        else {
            return;
        };
        // Close the destination's editor before replacing its effect so a later Cancel cannot undo the
        // copy.
        if self.effects_editing.as_ref() == Some(&LayerEffectSelection::new(to, kind)) {
            self.finish_effects_editing(true);
        }
        let mut effects = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == to))
            .and_then(|layer| layer.effects.clone())
            .unwrap_or_default();
        match kind {
            LayerEffectKind::Stroke => effects.stroke = original.stroke,
            LayerEffectKind::Shadow => effects.shadow = original.shadow,
            LayerEffectKind::ColorOverlay => effects.color_overlay = original.color_overlay,
            LayerEffectKind::InnerShadow => effects.inner_shadow = original.inner_shadow,
            LayerEffectKind::OuterGlow => effects.outer_glow = original.outer_glow,
            LayerEffectKind::InnerGlow => effects.inner_glow = original.inner_glow,
        }
        self.set_effects(effects, Some(to), &format!("Copy {}", kind.raw_value()));
        self.select_effect(kind, to, false);
    }

    /// `toggleEffect(_:on:)`: the layer list's eye on one effect.
    pub fn toggle_effect(&mut self, kind: LayerEffectKind, on: Id) {
        let Some(mut effects) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == on))
            .and_then(|layer| layer.effects.clone())
        else {
            return;
        };
        let enabled = effects.is_enabled(kind);
        effects.set_enabled(!enabled, kind);
        self.set_effects(effects, Some(on), &format!("{} {}", if enabled { "Hide" } else { "Show" }, kind.raw_value()));
    }

    /// `removeSelectedEffect()`: deletes the effect the panel points at (the layer list's Delete).
    pub fn remove_selected_effect(&mut self) {
        let Some(selected) = self.selected_effect() else { return };
        if !self.can_edit_layers() {
            return;
        }
        let Some(mut effects) = self
            .document
            .as_ref()
            .and_then(|document| document.layers.iter().find(|layer| layer.id == selected.layer_id))
            .and_then(|layer| layer.effects.clone())
        else {
            return;
        };
        if self.effects_editing.as_ref() == Some(&selected) {
            if self.aliases_effect_picker() {
                self.close_color_picker(false);
            }
            self.effects_editing = None;
            self.effects_editing_original = None;
        }
        effects.remove(selected.kind);
        self.set_effects(effects, Some(selected.layer_id), &format!("Remove {}", selected.kind.raw_value()));
        self.effect_selection = None;
    }

    /// The Swift guard `if let picker = colorPicker, case .effect = picker.target`.
    fn aliases_effect_picker(&self) -> bool {
        self.color_picker
            .as_ref()
            .is_some_and(|picker| matches!(picker.target, ColorPickerTarget::Effect(_)))
    }
}
