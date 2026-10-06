//! The palette and the color picker: the `extension EditorSession` of `Document/ColorPalette.swift` —
//! the foreground/background swatches (which on a mask are the mask's paint value), swap/reset, the
//! picker's state machine for every `ColorPickerTarget` case, and the eyedropper's sampling of the
//! live composite.
//!
//! The picker's model (`ColorPickerTarget`, `ColorPickerState`, `PickerHSB`) lives in
//! `compositor_rs_core::palette`; this module owns the commands that open, preview and close it.

use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::geom::Point;
use compositor_rs_core::image_ops::FilterKind;
use compositor_rs_core::layer_adjustment::AdjustmentColor;
use compositor_rs_core::layer_effects::LayerEffectKind;
use compositor_rs_core::layer_text::LayerTextStyle;
use compositor_rs_core::palette::{ColorPickerState, ColorPickerTarget};
use compositor_rs_pixels::canvas::Canvas;

use crate::session::EditorSession;

/// `AdjustmentColor(color)`: the adjustment vocabulary's straight sRGB triple, which the filter
/// settings carry.
fn adjustment_color(color: PaletteColor) -> AdjustmentColor {
    AdjustmentColor {
        red: color.red,
        green: color.green,
        blue: color.blue,
    }
}

impl EditorSession {
    /// The foreground swatch: the color every painting tool lays down (`foregroundColor`).
    pub fn foreground_color(&self) -> PaletteColor {
        PaletteColor::new(self.brush_settings.red, self.brush_settings.green, self.brush_settings.blue)
    }

    /// The `didSet` on `foregroundColor`, which is `brushSettings`'s color channels.
    pub fn set_foreground_color(&mut self, color: PaletteColor) {
        self.brush_settings.red = color.red;
        self.brush_settings.green = color.green;
        self.brush_settings.blue = color.blue;
        self.refresh_gradient();
    }

    /// Whether the palette may change now: not while a project operation runs or a stroke is down.
    pub fn can_edit_palette(&self) -> bool {
        // `_ = showsBusy`: the Swift read re-evaluated the UI's observation of the busy flag; here the
        // value is simply part of the state the caller reads.
        let _ = self.shows_busy;
        !self.is_project_busy && self.brush_stroke.is_none()
    }

    /// The swatch a tool would use: on a mask, the white/black paint value, and which of the two is
    /// the foreground follows `maskPaintWhite`, flipped for the background swatch.
    pub fn palette_color(&self, background: bool) -> PaletteColor {
        if self.is_mask_selected {
            let white = if background { !self.mask_paint_white } else { self.mask_paint_white };
            return if white { PaletteColor::WHITE } else { PaletteColor::BLACK };
        }
        if background {
            self.background_color
        } else {
            self.foreground_color()
        }
    }

    pub fn set_palette_color(&mut self, color: PaletteColor, background: bool) {
        if !self.can_edit_palette() {
            return;
        }
        if self.is_mask_selected {
            let white = color == PaletteColor::WHITE;
            self.set_mask_paint_white(if background { !white } else { white });
        } else if background {
            self.set_background_color(color);
        } else {
            self.set_foreground_color(color);
            // Type paints in the foreground color, so text being edited follows the swatch. A text
            // layer merely selected keeps its color: it changes only while its text is open for
            // editing.
            if self.tool == NavigationTool::Type && self.text_draft.is_some() {
                self.set_draft_text_color(color);
            }
        }
    }

    /// The `didSet` on `maskPaintWhite`.
    pub fn set_mask_paint_white(&mut self, value: bool) {
        self.mask_paint_white = value;
        self.refresh_gradient();
    }

    /// The `didSet` on `backgroundColor`.
    pub fn set_background_color(&mut self, color: PaletteColor) {
        self.background_color = color;
        self.refresh_gradient();
    }

    pub fn swap_palette_colors(&mut self) {
        if !self.can_edit_palette() {
            return;
        }
        if self.is_mask_selected {
            self.set_mask_paint_white(!self.mask_paint_white);
        } else {
            let old_foreground = self.foreground_color();
            let old_background = self.background_color;
            self.set_palette_color(old_background, false);
            self.set_background_color(old_foreground);
        }
    }

    pub fn reset_palette_colors(&mut self) {
        if !self.can_edit_palette() {
            return;
        }
        if self.is_mask_selected {
            self.set_mask_paint_white(false);
        } else {
            self.set_palette_color(PaletteColor::BLACK, false);
            self.set_background_color(PaletteColor::WHITE);
        }
    }

    pub fn open_color_picker(&mut self, background: bool) {
        if !self.can_edit_palette() || self.is_mask_selected {
            return;
        }
        let mut picker = ColorPickerState::palette(background, self.palette_color(background));
        // Text being edited follows the foreground color, so it previews the picker's working color as
        // the Type bar's own swatch does, and goes back to its own color on Cancel.
        if !background && self.tool == NavigationTool::Type {
            if let Some(draft) = self.text_draft.as_ref() {
                picker.edited_text = Some((draft.id, draft.style.clone()));
            }
        }
        self.color_picker = Some(picker);
    }

    /// What the Type bar's swatch shows and edits: the text being edited, otherwise the foreground
    /// color the next text will use. A text layer that is only selected is not touched.
    /// With letters selected, it is the color of the first of them; with just a caret, the letter
    /// before it, the color typing there gives.
    pub fn type_color(&self) -> PaletteColor {
        let Some(draft) = self.text_draft.as_ref() else { return self.foreground_color() };
        let selection = draft.selection;
        let index = if selection.length > 0 {
            selection.location
        } else {
            0.max(selection.location - 1)
        };
        draft.style.color(index)
    }

    pub fn open_text_color_picker(&mut self) {
        if !self.can_edit_palette() || self.color_picker.is_some() || self.tool != NavigationTool::Type {
            return;
        }
        let draft_id = self.text_draft.as_ref().map(|draft| draft.id);
        let mut picker = ColorPickerState::new(ColorPickerTarget::Text { draft_id }, self.type_color());
        if let Some(draft) = self.text_draft.as_ref() {
            picker.edited_text = Some((draft.id, draft.style.clone()));
        }
        self.color_picker = Some(picker);
    }

    /// Paints the selected letters of the text being edited, or all of it when nothing is selected.
    pub fn set_draft_text_color(&mut self, color: PaletteColor) {
        let Some(selection) = self.text_draft.as_ref().map(|draft| draft.selection) else { return };
        self.change_text_style(move |style| style.set_color(color, selection));
    }

    /// Puts back the colors the text being edited had when the picker opened.
    fn restore_draft_text_colors(&mut self, original: &LayerTextStyle) {
        let original = original.clone();
        self.change_text_style(move |style| {
            style.red = original.red;
            style.green = original.green;
            style.blue = original.blue;
            style.color_runs = if original.content == style.content { original.color_runs.clone() } else { None };
        });
        self.refresh_canvas_preview();
    }

    /// Opens the app's picker on a layer effect's color.
    pub fn open_effect_color_picker(&mut self, kind: LayerEffectKind) {
        if !self.can_edit_palette() || self.color_picker.is_some() || self.effects_editing.is_none() {
            return;
        }
        let original = self.editing_effects().color(kind).unwrap_or(PaletteColor::BLACK);
        self.color_picker = Some(ColorPickerState::new(ColorPickerTarget::Effect { kind }, original));
    }

    /// The picker's OK/Cancel: every target commits its working color, or puts its original back.
    pub fn close_color_picker(&mut self, commit: bool) {
        if let Some(picker) = self.color_picker.take() {
            match &picker.target {
                ColorPickerTarget::Palette { background } => {
                    if commit && !self.is_mask_selected {
                        self.set_palette_color(picker.color(), *background);
                    } else if !commit {
                        if let Some((draft_id, style)) = picker.edited_text.clone() {
                            if self.tool == NavigationTool::Type
                                && self.text_draft.as_ref().map(|draft| draft.id) == Some(draft_id)
                            {
                                self.restore_draft_text_colors(&style);
                            }
                        }
                    }
                }
                ColorPickerTarget::Text { draft_id } => {
                    if self.tool == NavigationTool::Type && self.text_draft.as_ref().map(|draft| draft.id) == *draft_id {
                        let color = if commit { picker.color() } else { picker.original };
                        if draft_id.is_some() {
                            if commit {
                                self.set_draft_text_color(color);
                                self.refresh_canvas_preview();
                            } else if let Some((_, style)) = picker.edited_text.clone() {
                                self.restore_draft_text_colors(&style);
                            }
                        } else if commit {
                            self.text_defaults.red = color.red;
                            self.text_defaults.green = color.green;
                            self.text_defaults.blue = color.blue;
                        }
                        // The text color is the foreground color: picking one in the Type bar moves
                        // the swatch too.
                        if commit && !self.is_mask_selected {
                            self.set_foreground_color(color);
                        }
                    }
                }
                ColorPickerTarget::Effect { kind } => {
                    let color = if commit { picker.color() } else { picker.original };
                    let kind = *kind;
                    self.change_effects(move |effects| effects.set_color(color, kind));
                }
                ColorPickerTarget::GradientMap { highlights } => {
                    // The end has been previewing the working color; Cancel puts the original back.
                    let color = if commit { picker.color() } else { picker.original };
                    self.set_gradient_map_color(color, *highlights);
                }
                ColorPickerTarget::Vignette => {
                    let color = if commit { picker.color() } else { picker.original };
                    self.set_vignette_color(color);
                }
                ColorPickerTarget::Dither { light } => {
                    let color = if commit { picker.color() } else { picker.original };
                    self.set_dither_color(color, *light);
                }
                ColorPickerTarget::Dialog { .. } => {
                    let color = if commit { picker.color() } else { picker.original };
                    if let Some(mut change) = self.dialog_color_change.take() {
                        change(color);
                    }
                }
            }
        }
        self.color_picker = None;
        // Sampling clicked the canvas, which took the keys from the text: give them back.
        if self.text_draft.is_some() {
            self.canvas_focus_request += 1;
        }
    }

    /// Opens the app's color picker on one end of the Gradient Map being edited (Shadows or
    /// Highlights).
    pub fn open_gradient_map_color_picker(&mut self, highlights: bool) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if !self.can_edit_palette() || self.color_picker.is_some() || edit.kind != FilterKind::GradientMap || edit.committing {
            return;
        }
        let value = if highlights {
            edit.settings.gradient_map.highlights
        } else {
            edit.settings.gradient_map.shadows
        };
        self.color_picker = Some(ColorPickerState::new(
            ColorPickerTarget::GradientMap { highlights },
            PaletteColor::new(value.red, value.green, value.blue),
        ));
    }

    pub fn open_vignette_color_picker(&mut self) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if !self.can_edit_palette() || self.color_picker.is_some() || edit.kind != FilterKind::Vignette || edit.committing {
            return;
        }
        let value = edit.settings.vignette_color;
        self.color_picker = Some(ColorPickerState::new(
            ColorPickerTarget::Vignette,
            PaletteColor::new(value.red, value.green, value.blue),
        ));
    }

    /// While the picker is open on an effect's color, the canvas follows its working color.
    pub fn preview_effect_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        let ColorPickerTarget::Effect { kind } = picker.target else { return };
        let color = picker.color();
        self.change_effects(move |effects| effects.set_color(color, kind));
    }

    /// Preview the picker's working color in the active on-canvas text draft.
    pub fn preview_text_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        let draft_id = match picker.target {
            ColorPickerTarget::Text { draft_id } => draft_id,
            ColorPickerTarget::Palette { background: false } => {
                picker.edited_text.as_ref().map(|(draft_id, _)| *draft_id)
            }
            _ => return,
        };
        let Some(draft_id) = draft_id else { return };
        if self.tool != NavigationTool::Type || self.text_draft.as_ref().map(|draft| draft.id) != Some(draft_id) {
            return;
        }
        let color = picker.color();
        self.set_draft_text_color(color);
        self.refresh_canvas_preview();
    }

    /// While the picker is open on a Gradient Map end, the gradient (and canvas) follow its working
    /// color.
    pub fn preview_gradient_map_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        let ColorPickerTarget::GradientMap { highlights } = picker.target else { return };
        let color = picker.color();
        self.set_gradient_map_color(color, highlights);
    }

    /// Opens the app's picker on a dialog's color. `change` hears the working color as it moves, the
    /// chosen one on OK, and the original again on Cancel.
    pub fn open_dialog_color_picker(
        &mut self,
        title: String,
        color: PaletteColor,
        change: Box<dyn FnMut(PaletteColor) + 'static>,
    ) {
        if self.color_picker.is_some() {
            return;
        }
        self.dialog_color_change = Some(change);
        self.color_picker = Some(ColorPickerState::new(ColorPickerTarget::Dialog { title }, color));
    }

    /// The picker is open for a dialog, which covers the canvas: there's nothing to sample.
    pub fn picking_for_dialog(&self) -> bool {
        matches!(
            self.color_picker.as_ref().map(|picker| &picker.target),
            Some(ColorPickerTarget::Dialog { .. })
        )
    }

    /// While the picker is open on a dialog's color, the dialog follows its working color.
    pub fn preview_dialog_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        if !matches!(picker.target, ColorPickerTarget::Dialog { .. }) {
            return;
        }
        let color = picker.color();
        if let Some(change) = self.dialog_color_change.as_mut() {
            change(color);
        }
    }

    pub fn open_dither_color_picker(&mut self, light: bool) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if !self.can_edit_palette() || self.color_picker.is_some() || edit.kind != FilterKind::Dither || edit.committing {
            return;
        }
        let value = if light { edit.settings.dither.light } else { edit.settings.dither.dark };
        self.color_picker = Some(ColorPickerState::new(
            ColorPickerTarget::Dither { light },
            PaletteColor::new(value.red, value.green, value.blue),
        ));
    }

    pub fn preview_dither_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        let ColorPickerTarget::Dither { light } = picker.target else { return };
        let color = picker.color();
        self.set_dither_color(color, light);
    }

    fn set_dither_color(&mut self, color: PaletteColor, light: bool) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if edit.kind != FilterKind::Dither || edit.committing {
            return;
        }
        let mut settings = edit.settings.clone();
        if light {
            settings.dither.light = adjustment_color(color);
        } else {
            settings.dither.dark = adjustment_color(color);
        }
        if settings == edit.settings {
            return;
        }
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    pub fn preview_vignette_color(&mut self) {
        let Some(picker) = self.color_picker.as_ref() else { return };
        if !matches!(picker.target, ColorPickerTarget::Vignette) {
            return;
        }
        let color = picker.color();
        self.set_vignette_color(color);
    }

    fn set_vignette_color(&mut self, color: PaletteColor) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if edit.kind != FilterKind::Vignette || edit.committing {
            return;
        }
        let mut settings = edit.settings.clone();
        settings.vignette_color = adjustment_color(color);
        if settings == edit.settings {
            return;
        }
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    fn set_gradient_map_color(&mut self, color: PaletteColor, highlights: bool) {
        let Some(edit) = self.filter_edit.as_ref() else { return };
        if edit.kind != FilterKind::GradientMap || edit.committing {
            return;
        }
        let mut settings = edit.settings.clone();
        if highlights {
            settings.gradient_map.highlights = adjustment_color(color);
        } else {
            settings.gradient_map.shadows = adjustment_color(color);
        }
        if settings == edit.settings {
            return;
        }
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    /// Loads the canvas color under a document point into the open picker.
    pub fn sample_into_color_picker(&mut self, point: Point) {
        if self.color_picker.is_none() {
            return;
        }
        let Some(color) = self.sample_composite_color(point) else { return };
        if let Some(picker) = self.color_picker.as_mut() {
            picker.hsb.set_rgb(color);
        }
    }

    /// Composited sRGB color of the visible layers at one document pixel, as shown on the canvas.
    /// Nil outside the canvas or over fully transparent pixels.
    pub fn sample_composite_color(&self, point: Point) -> Option<PaletteColor> {
        let document = self.document.as_ref()?;
        let size = document.size();
        if !(point.x >= 0.0 && point.y >= 0.0 && point.x < size.width && point.y < size.height) {
            return None;
        }
        // Map the target pixel onto the 1×1 context in the renderer's top-left space.
        let mut canvas = Canvas::new_rgba(1, 1);
        canvas.translate(0.0, 1.0);
        canvas.scale(1.0, -1.0);
        canvas.translate(-point.x.floor(), -point.y.floor());
        self.draw_live_composite(document, &mut canvas, false);
        let image = canvas.into_rgba();
        let pixel = image.get(0, 0);
        if pixel[3] == 0 {
            return None;
        }
        let alpha = pixel[3] as f64;
        let channel = |value: u8| (alpha.min(value as f64) / alpha * 255.0).round() / 255.0;
        Some(PaletteColor::new(channel(pixel[0]), channel(pixel[1]), channel(pixel[2])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::palette::PickerHSB;

    fn session() -> EditorSession {
        let mut session = EditorSession::default();
        session.tool = NavigationTool::Brush;
        session
    }

    fn working_color() -> PaletteColor {
        PaletteColor::new(0.5, 0.25, 0.125)
    }

    /// OK writes the foreground swatch; Cancel leaves it alone.
    #[test]
    fn foreground_picker_commits_only_on_ok() {
        let mut session = session();
        session.set_foreground_color(PaletteColor::BLACK);
        session.open_color_picker(false);
        let picker = session.color_picker.as_mut().expect("a picker is open");
        assert_eq!(picker.original, PaletteColor::BLACK);
        picker.hsb = PickerHSB::from_color(working_color());
        session.close_color_picker(true);
        assert_eq!(session.foreground_color(), working_color().quantized());

        session.open_color_picker(false);
        let picker = session.color_picker.as_mut().expect("a picker is open");
        picker.hsb = PickerHSB::from_color(PaletteColor::new(1.0, 1.0, 1.0));
        session.close_color_picker(false);
        assert_eq!(session.foreground_color(), working_color().quantized());
        assert!(session.color_picker.is_none());
    }

    /// The background picker writes the background swatch, and never the foreground.
    #[test]
    fn background_picker_commits_the_background_swatch() {
        let mut session = session();
        session.set_background_color(PaletteColor::WHITE);
        session.open_color_picker(true);
        assert!(session.color_picker.as_ref().expect("open").background());
        let picker = session.color_picker.as_mut().expect("open");
        picker.hsb = PickerHSB::from_color(working_color());
        session.close_color_picker(true);
        assert_eq!(session.background_color, working_color().quantized());
        assert_eq!(session.foreground_color(), PaletteColor::BLACK);
    }

    /// On a mask the swatches are the paint value: the foreground is white while painting white, the
    /// background the other one, and committing flips `maskPaintWhite` accordingly.
    #[test]
    fn mask_swatches_paint_white_and_black() {
        let mut session = session();
        session.is_mask_selected = true;
        assert_eq!(session.palette_color(false), PaletteColor::BLACK);
        assert_eq!(session.palette_color(true), PaletteColor::WHITE);

        session.set_palette_color(PaletteColor::WHITE, false);
        assert!(session.mask_paint_white);
        assert_eq!(session.palette_color(false), PaletteColor::WHITE);

        // The background swatch is the other of the two.
        session.set_palette_color(PaletteColor::BLACK, true);
        assert!(session.mask_paint_white);
        session.set_palette_color(PaletteColor::WHITE, true);
        assert!(!session.mask_paint_white);

        session.swap_palette_colors();
        assert!(session.mask_paint_white);
        session.reset_palette_colors();
        assert!(!session.mask_paint_white);
    }

    /// Swap exchanges the two swatches (art directed by `setPaletteColor`, so a text draft follows
    /// the foreground), reset restores black over white.
    #[test]
    fn swap_and_reset_move_both_swatches() {
        let mut session = session();
        session.set_foreground_color(PaletteColor::new(1.0, 0.0, 0.0));
        session.set_background_color(PaletteColor::new(0.0, 0.0, 1.0));
        session.swap_palette_colors();
        assert_eq!(session.foreground_color(), PaletteColor::new(0.0, 0.0, 1.0));
        assert_eq!(session.background_color, PaletteColor::new(1.0, 0.0, 0.0));
        session.reset_palette_colors();
        assert_eq!(session.foreground_color(), PaletteColor::BLACK);
        assert_eq!(session.background_color, PaletteColor::WHITE);
    }

    /// A dialog's color picker hears the working color, the chosen one on OK and the original on
    /// Cancel; the callback is dropped with the picker.
    #[test]
    fn dialog_picker_reports_working_and_final_colors() {
        fn sink() -> (std::sync::Arc<std::sync::Mutex<Vec<PaletteColor>>>, Box<dyn FnMut(PaletteColor)>) {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let target = seen.clone();
            (
                seen,
                Box::new(move |color| target.lock().expect("the sink is not poisoned").push(color)),
            )
        }
        fn last(seen: &std::sync::Arc<std::sync::Mutex<Vec<PaletteColor>>>) -> Option<PaletteColor> {
            seen.lock().expect("the sink is not poisoned").last().copied()
        }

        let mut session = session();
        let (seen, change) = sink();
        session.open_dialog_color_picker("Export JPEG".to_string(), PaletteColor::WHITE, change);
        let picker = session.color_picker.as_mut().expect("a picker is open");
        assert_eq!(picker.target.title(), "Color Picker (Export JPEG)");
        picker.hsb = PickerHSB::from_color(working_color());
        session.preview_dialog_color();
        assert_eq!(last(&seen), Some(working_color().quantized()));
        assert!(session.picking_for_dialog());
        session.close_color_picker(true);
        assert_eq!(last(&seen), Some(working_color().quantized()));
        assert!(!session.picking_for_dialog());
        assert!(session.dialog_color_change.is_none());

        // Cancel hands the original back.
        let (seen, change) = sink();
        session.open_dialog_color_picker("Export JPEG".to_string(), PaletteColor::WHITE, change);
        let picker = session.color_picker.as_mut().expect("a picker is open");
        picker.hsb = PickerHSB::from_color(PaletteColor::BLACK);
        session.close_color_picker(false);
        assert_eq!(last(&seen), Some(PaletteColor::WHITE));
    }

    /// A text picker opened with no draft open edits the defaults, and always moves the foreground
    /// swatch with the chosen color.
    #[test]
    fn text_picker_without_a_draft_edits_the_defaults() {
        let mut session = session();
        session.tool = NavigationTool::Type;
        assert_eq!(session.type_color(), PaletteColor::BLACK);
        session.open_text_color_picker();
        let picker = session.color_picker.as_mut().expect("a picker is open");
        assert_eq!(picker.target, ColorPickerTarget::Text { draft_id: None });
        picker.hsb = PickerHSB::from_color(working_color());
        session.close_color_picker(true);
        assert_eq!(session.text_defaults.red, working_color().quantized().red);
        assert_eq!(session.text_defaults.green, working_color().quantized().green);
        assert_eq!(session.text_defaults.blue, working_color().quantized().blue);
        assert_eq!(session.foreground_color(), working_color().quantized());
    }

    /// Opening the foreground picker while text is being edited remembers the draft, and Cancel puts
    /// its colors back.
    #[test]
    fn cancelling_the_foreground_picker_restores_the_draft_colors() {
        let mut session = session();
        session.tool = NavigationTool::Type;
        let layer = compositor_rs_core::document::ImageLayer::blank("Text", compositor_rs_core::geom::Size::new(64.0, 64.0));
        let document_id = compositor_rs_core::new_id();
        session.text_draft = Some(compositor_rs_core::layer_text::TextDraft::new(
            document_id,
            Some(layer.id),
            compositor_rs_core::geom::Point::ZERO,
            None,
            LayerTextStyle::default(),
        ));
        let original = session.text_draft.as_ref().expect("a draft").style.clone();
        session.open_color_picker(false);
        let picker = session.color_picker.as_mut().expect("a picker is open");
        assert_eq!(picker.edited_text.as_ref().map(|(id, _)| *id), Some(session.text_draft.as_ref().expect("a draft").id));
        picker.hsb = PickerHSB::from_color(working_color());
        // The preview paints the letters as the picker moves.
        session.preview_text_color();
        assert_ne!(session.text_draft.as_ref().expect("a draft").style.red, original.red);
        session.close_color_picker(false);
        assert_eq!(session.text_draft.as_ref().expect("a draft").style, original);
    }

    /// `type_color`: the caret's color comes from the letter before it; a selection's from its first
    /// letter.
    #[test]
    fn type_color_reads_the_caret_and_the_selection() {
        let mut session = session();
        session.tool = NavigationTool::Type;
        session.text_defaults = LayerTextStyle::default();
        assert_eq!(session.type_color(), PaletteColor::BLACK);

        let mut style = LayerTextStyle::default();
        style.content = "Hello".to_string();
        style.set_color(PaletteColor::new(1.0, 1.0, 1.0), compositor_rs_core::layer_text::TextRange::new(0, 2));
        session.text_draft = Some(compositor_rs_core::layer_text::TextDraft::new(
            compositor_rs_core::new_id(),
            None,
            compositor_rs_core::geom::Point::ZERO,
            None,
            style,
        ));
        session.text_draft.as_mut().expect("a draft").selection = compositor_rs_core::layer_text::TextRange::new(0, 0);
        assert_eq!(session.type_color(), PaletteColor::WHITE, "a caret at the start clamps to the first letter");
        session.text_draft.as_mut().expect("a draft").selection = compositor_rs_core::layer_text::TextRange::new(1, 0);
        assert_eq!(session.type_color(), PaletteColor::WHITE, "a caret after a white letter types white");
        session.text_draft.as_mut().expect("a draft").selection = compositor_rs_core::layer_text::TextRange::new(2, 3);
        assert_eq!(session.type_color(), PaletteColor::BLACK, "a selection takes its first letter's color");
    }
}
