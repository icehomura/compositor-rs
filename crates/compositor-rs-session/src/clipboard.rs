//! Copy, cut and paste — of pixels through the selection, of whole layers, and of the merged
//! canvas — plus the system pasteboard seam.
//!
//! Port of `Document/SelectionClipboard.swift`. The Swift read and wrote `NSPasteboard.general`
//! directly; the port reaches the system pasteboard through the [`Clipboard`] trait, which the app
//! crate implements. Without a handle (headless tests, no pasteboard) the in-session
//! [`PixelClipboard`] still round-trips copies made in this session, which is the observable
//! behaviour that matters.
//!
//! A note on `async`: the Swift `cutSelection()` awaited the clear, and `paste()` was synchronous.
//! The port's commands are all synchronous; the busy flag is set around the raster work where the
//! Swift would have suspended.

use std::sync::Arc;

use compositor_rs_core::blend::LayerBlendMode;
use compositor_rs_core::buffer::{Rgba8Image, SharedImage};
use compositor_rs_core::document::ImageLayer;
use compositor_rs_core::geom::{Point, Rect};
use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::layer_shape::LayerShape;
use compositor_rs_core::layer_text::LayerText;
use compositor_rs_core::selection::SelectionClip;
use compositor_rs_core::{CoreError, Id};
use compositor_rs_pixels::canvas::Canvas;
use compositor_rs_render::LayerRenderer;

use crate::selection::mask_background;

use crate::EditorSession;

/// The system pasteboard, implemented by the app crate (the Swift `NSPasteboard.general`).
///
/// Only what the clipboard commands use: a change counter to tell whether another app has copied
/// since, PNG image in/out (`setData(_:forType:.png)` and the image read `paste` falls back to), and
/// the private `com.compositor.copied-layer` string that whole-layer copies write.
pub trait Clipboard: Send {
    /// `NSPasteboard.changeCount`.
    fn change_count(&self) -> i64;
    /// `NSPasteboard.clearContents()`.
    fn clear_contents(&mut self);
    /// `setData(png, forType: .png)`: the app encodes the canonical pixels as PNG.
    fn write_image(&mut self, image: &Rgba8Image);
    /// `setString(id, forType: "com.compositor.copied-layer")`.
    fn write_copied_layer(&mut self, id: &str);
    /// `canReadObject(forClasses: [NSImage.self])`.
    fn can_read_image(&self) -> bool;
    /// The pasteboard's image, normalized to premultiplied sRGB RGBA8 (the Swift `sRGBCopy`).
    fn read_image(&mut self) -> Option<Rgba8Image>;
    /// The `com.compositor.copied-layer` string, when one is present.
    fn read_copied_layer(&self) -> Option<String>;
}

/// Pixels copied from the canvas, with where they came from so Paste can put them back in place.
#[derive(Clone, Debug)]
pub struct PixelClipboard {
    pub image: SharedImage,
    pub origin: Point,
    /// The system pasteboard's change count right after writing; a mismatch means another app copied since.
    pub change_count: i64,
}

/// A whole layer copied with no selection. Paste brings it back complete — folder contents, mask, effects, editable
/// text: in this project as a copy above it, in another as dragging it onto that project's tab does.
#[derive(Clone, Debug)]
pub struct CopiedLayer {
    /// Top to bottom as the document lists them; a layer inside a copied folder comes with the folder, not on its own.
    pub ids: Vec<Id>,
    /// As `PixelClipboard.change_count`: anything copied since replaces it.
    pub change_count: i64,
}

impl EditorSession {
    /// Whole-pixel bounds of what Copy takes: the selection, or the whole canvas without one.
    /// Path boolean operations leave tiny float noise (59.9999999), so round with a tolerance
    /// rather than letting it add a whole pixel.
    pub fn selection_copy_region(&self) -> Option<Rect> {
        let document = self.document.as_ref()?;
        let canvas = Rect::new(0.0, 0.0, document.width as f64, document.height as f64);
        let bounds = self
            .selection()
            .map(|selection| selection.coverage_bounds())
            .unwrap_or(canvas);
        let tolerance = 0.001;
        let min_x = (bounds.min_x() + tolerance).floor();
        let min_y = (bounds.min_y() + tolerance).floor();
        let region = Rect::new(
            min_x,
            min_y,
            (bounds.max_x() - tolerance).ceil() - min_x,
            (bounds.max_y() - tolerance).ceil() - min_y,
        )
        .intersection(canvas);
        if region.is_null() || region.width() < 1.0 || region.height() < 1.0 {
            return None;
        }
        Some(region)
    }

    /// `canCopyPixels`: whether Copy takes the active layer's pixels (or its mask).
    pub fn can_copy_pixels(&self) -> bool {
        if !self.can_edit_layers() {
            return false;
        }
        let Some(layer) = self.active_layer() else {
            return false;
        };
        if layer.is_group && !self.is_mask_selected {
            return false;
        }
        if self.selection().map(|selection| selection.is_empty()) == Some(true) {
            return false;
        }
        if self.is_mask_selected {
            layer.mask.is_some()
        } else {
            layer.asset.is_some()
        }
    }

    /// The active layer's pixels (or mask as opaque gray) exactly as they sit on the canvas,
    /// clipped to the selection (soft edges kept), or the whole canvas without one.
    pub fn render_selected_pixels(
        &self,
        layer: &ImageLayer,
        mask: bool,
    ) -> Result<Option<(Rgba8Image, Rect)>, CoreError> {
        if self.document.is_none() {
            return Ok(None);
        }
        let clip = self.selection_clip();
        if let Some(clip) = clip.as_ref() {
            if clip.coverage.is_none() {
                return Ok(None);
            }
        }
        let Some(region) = self.selection_copy_region() else {
            return Ok(None);
        };
        let mut canvas = Canvas::new_rgba(region.width() as usize, region.height() as usize);
        canvas.translate(-region.min_x(), -region.min_y());
        apply_selection_clip(&mut canvas, clip.as_ref());
        let transform = self.displayed_transform(layer);
        if mask {
            let Some(owned) = layer.mask.as_ref() else {
                return Ok(None);
            };
            let placement = self.displayed_mask_placement(layer);
            // Beyond the mask's pixels, the fill is black when the mask is placed on its own, else the
            // mask's own background, as the Swift's `setFillColor(gray:)` chose.
            let background = if placement.is_none() {
                mask_background(&owned.asset.thumbnail)
            } else {
                0.0
            };
            canvas.set_fill_gray(background);
            canvas.fill_rect(region);
            if let Some(gray) = owned.asset.image.as_gray() {
                LayerRenderer::draw_coverage(gray, &placement.unwrap_or(transform), &mut canvas);
            }
        } else {
            let Some(asset) = layer.asset.as_ref() else {
                return Ok(None);
            };
            if asset.image.as_rgba().is_none() {
                return Ok(None);
            }
            LayerRenderer::draw(
                &asset.image,
                &transform,
                transform.center(),
                1.0,
                1.0,
                LayerBlendMode::Normal,
                None,
                &mut canvas,
            );
        }
        Ok(Some((canvas.into_rgba(), region)))
    }

    /// `canCopyMerged`: Shift-Cmd-C needs a document with visible pixels.
    pub fn can_copy_merged(&self) -> bool {
        self.can_edit_layers()
            && self.selection().map(|selection| selection.is_empty()) != Some(true)
            && self.document.as_ref().is_some_and(|document| {
                document
                    .render_layers()
                    .iter()
                    .any(|layer| layer.asset.is_some())
            })
    }

    /// Shift-Cmd-C (Copy Merged): the selection across every visible layer, composited as
    /// the canvas shows it, including opacity, blend modes, and masks.
    pub fn render_merged_pixels(&self) -> Result<Option<(Rgba8Image, Rect)>, CoreError> {
        let Some(document) = self.document.as_ref() else {
            return Ok(None);
        };
        let clip = self.selection_clip();
        if let Some(clip) = clip.as_ref() {
            if clip.coverage.is_none() {
                return Ok(None);
            }
        }
        let Some(region) = self.selection_copy_region() else {
            return Ok(None);
        };
        // Composited on its own first, then drawn through the selection: a transparency layer would do the same,
        // but Color Burn and Color Dodge need to read what they are blending with, which a group hides.
        let mut composite = Canvas::new_rgba(region.width() as usize, region.height() as usize);
        composite.translate(-region.min_x(), -region.min_y());
        self.draw_live_composite(document, &mut composite, false);
        let merged = composite.into_rgba();
        let mut canvas = Canvas::new_rgba(region.width() as usize, region.height() as usize);
        canvas.translate(-region.min_x(), -region.min_y());
        apply_selection_clip(&mut canvas, clip.as_ref());
        canvas.draw_image(&merged, region);
        Ok(Some((canvas.into_rgba(), region)))
    }

    /// Shift-Cmd-C: copies the merged selection to the pasteboard.
    pub fn copy_merged_selection(&mut self) {
        if !self.can_copy_merged() {
            return;
        }
        match self.render_merged_pixels() {
            Ok(Some(copied)) => self.store(copied),
            Ok(None) => {}
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// Copy with no selection copies the layer itself, for Paste here or in another project. That works for folders
    /// and adjustments too, which have no pixels of their own to copy.
    pub fn can_copy_layer(&self) -> bool {
        self.can_edit_layers()
            && self.active_layer().is_some()
            && self.selection().is_none()
            && !self.is_mask_selected
    }

    /// Cmd-C: copies the selected pixels (or the whole layer) for Paste, and to the system
    /// pasteboard as PNG for other apps.
    pub fn copy_selection(&mut self) {
        if !self.can_copy_pixels() && !self.can_copy_layer() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else {
            return;
        };
        if !self.can_copy_pixels() {
            let ids = self.copied_layer_ids();
            let change_count = match self.clipboard.as_mut() {
                Some(clipboard) => {
                    clipboard.clear_contents();
                    clipboard.write_copied_layer(&layer.id.to_string());
                    clipboard.change_count()
                }
                None => 0,
            };
            self.pixel_clipboard = None;
            self.copied_layer = Some(CopiedLayer { ids, change_count });
            return;
        }
        match self.render_selected_pixels(&layer, self.is_mask_selected) {
            Ok(Some(copied)) => {
                self.store(copied);
                if self.can_copy_layer() {
                    let change_count = self
                        .clipboard
                        .as_ref()
                        .map(|clipboard| clipboard.change_count())
                        .unwrap_or(0);
                    self.copied_layer = Some(CopiedLayer {
                        ids: self.copied_layer_ids(),
                        change_count,
                    });
                }
            }
            Ok(None) => {}
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// Keeps pixels for Paste and puts them on the system pasteboard as PNG.
    fn store(&mut self, copied: (Rgba8Image, Rect)) {
        let (image, region) = copied;
        let change_count = match self.clipboard.as_mut() {
            Some(clipboard) => {
                clipboard.clear_contents();
                clipboard.write_image(&image);
                clipboard.change_count()
            }
            None => 0,
        };
        self.pixel_clipboard = Some(PixelClipboard {
            image: Arc::new(image),
            origin: region.origin,
            change_count,
        });
        self.copied_layer = None;
    }

    /// Cmd-X: copy, then clear the selected pixels.
    pub fn cut_selection(&mut self) {
        if self.selection().is_none() || !self.can_copy_pixels() {
            return;
        }
        self.copy_selection();
        self.clear_selected_pixels();
    }

    /// `canPaste`: the in-session pixels when the pasteboard has not changed since, else any image
    /// another app copied.
    pub fn can_paste(&self) -> bool {
        if self.document.is_none() || !self.can_edit_layers() {
            return false;
        }
        if let Some(pixel_clipboard) = self.pixel_clipboard.as_ref() {
            let fresh = match self.clipboard.as_ref() {
                Some(clipboard) => clipboard.change_count() == pixel_clipboard.change_count,
                None => true,
            };
            if fresh {
                return true;
            }
        }
        self.clipboard
            .as_ref()
            .is_some_and(|clipboard| clipboard.can_read_image())
    }

    /// Cmd-V: pastes as a new layer above the active one. Pixels copied here go back exactly
    /// where they came from; images copied in other apps are centered.
    pub fn paste(&mut self) {
        if !self.can_paste() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let fresh = self
            .pixel_clipboard
            .as_ref()
            .is_some_and(|pixel_clipboard| match self.clipboard.as_ref() {
                Some(clipboard) => clipboard.change_count() == pixel_clipboard.change_count,
                None => true,
            });
        if fresh {
            let Some(clip) = self.pixel_clipboard.clone() else {
                return;
            };
            let name = self.next_layer_name();
            self.add_pixel_layer(clip.image, clip.origin, &name, "Paste", true, None, None);
            return;
        }
        let external = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.read_image());
        let Some(image) = external else {
            return;
        };
        let origin = Point::new(
            ((document.width as f64 - image.width() as f64) / 2.0).floor(),
            ((document.height as f64 - image.height() as f64) / 2.0).floor(),
        );
        let name = self.next_layer_name();
        self.add_pixel_layer(Arc::new(image), origin, &name, "Paste", true, None, None);
    }

    /// Cmd-J (Layer via Copy): the selection's pixels become a new layer in place; with no
    /// selection the whole layer is duplicated.
    pub fn layer_via_copy(&mut self) {
        if !self.can_edit_layers() {
            return;
        }
        let Some(layer) = self.active_layer().cloned() else {
            return;
        };
        if self.selection().map(|selection| selection.is_empty()) == Some(true) {
            return;
        }
        // Without a selection it duplicates, folders included; with one it copies pixels, which a folder has none of.
        if self.selection().is_none() {
            self.duplicate_active_layer();
            return;
        }
        if layer.is_group {
            return;
        }
        match self.render_selected_pixels(&layer, self.is_mask_selected) {
            Ok(Some(copied)) => {
                let name = self.next_layer_name();
                let (image, region) = copied;
                self.add_pixel_layer(
                    Arc::new(image),
                    region.origin,
                    &name,
                    "Layer via Copy",
                    true,
                    None,
                    None,
                );
            }
            Ok(None) => {}
            Err(error) => self.brush_error = Some(error.to_string()),
        }
    }

    /// Inserts pixels as a new layer above the active one (inside its folder), all in one undo
    /// step. Pasting drops the selection, as in Photoshop; a drawn shape keeps it.
    pub fn add_pixel_layer(
        &mut self,
        image: SharedImage,
        at: Point,
        name: &str,
        edit_name: &str,
        drops_selection: bool,
        shape: Option<LayerShape>,
        text: Option<LayerText>,
    ) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let thumbnail = rgba_thumbnail(&image);
        let mut layer = ImageLayer::from_asset(
            ImportedImage::new(PixelImage::Rgba(image), thumbnail, name),
            at,
        );
        layer.name = name.to_string();
        layer.shape = shape;
        layer.text = text;
        layer.parent_id = if self.active_layer().is_some_and(|active| active.is_group) {
            self.active_layer_id
        } else {
            self.active_layer().and_then(|active| active.parent_id)
        };
        let index = document
            .layers
            .iter()
            .position(|layer| Some(layer.id) == self.active_layer_id)
            .map(|index| index + 1)
            .unwrap_or(document.layers.len());
        let layer_id = layer.id;
        self.finish_opacity_edit();
        self.begin_edit(edit_name);
        if let Some(document) = self.document.as_mut() {
            document.layers.insert(index, layer);
            if drops_selection {
                document.selection = None;
            }
        }
        self.set_active_layer(Some(layer_id));
        self.end_edit();
    }
}

/// `SelectionClip.apply(to:)` for the port's y-down [`Canvas`]: multiplies the clip by the
/// coverage, with no coverage clipping everything away.
fn apply_selection_clip(canvas: &mut Canvas, clip: Option<&SelectionClip>) {
    match clip {
        Some(SelectionClip {
            rect,
            coverage: Some(coverage),
        }) if !rect.is_empty() => {
            canvas.clip_to_image(coverage, *rect);
        }
        Some(_) => canvas.clip_to_zero(),
        None => {}
    }
}

/// A thumbnail no larger than 96 pixels on its longest side (`PixelAdjust.thumbnail(of:)`, which the
/// pixel crate landed; the Swift drew with the nearest source pixel).
pub(crate) fn rgba_thumbnail(image: &Rgba8Image) -> PixelImage {
    PixelImage::Rgba(Arc::new(compositor_rs_pixels::adjustments::PixelAdjust::thumbnail(image)))
}
