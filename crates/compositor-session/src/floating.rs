//! Cmd-T with a selection: the selected pixels float on a temporary layer, edited with the
//! normal transform handles, then merge back into their layer. The whole thing is one
//! "Transform Selection" undo step; Escape restores the document exactly.
//!
//! Port of `Document/FloatingSelection.swift` plus the pixel-move half of
//! `Document/SelectionEdits.swift` (`PixelMove`, Cmd-drag / Cmd-arrow).

use std::sync::Arc;

use compositor_core::document::ImageLayer;
use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_effects::LayerEffects;
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_transform::{
    pixel_to_document, FloatingTransform, LayerTransform, TransformEdit,
};
use compositor_core::limits;
use compositor_core::selection::DocumentSelection;
use compositor_core::{CoreError, Gray8Image};
use compositor_pixels::brush::{BrushSettings, BrushStroke};
use compositor_pixels::canvas::Canvas;
use compositor_pixels::warp::DistortWarp;
use compositor_render::LayerRenderer;

use crate::clipboard::rgba_thumbnail;
use crate::selection::{draw_gray_scaled, mask_imported_image, rect_applying};
use crate::EditorSession;

/// Selected pixels being dragged: the lifted raster plus the outline it started from.
///
/// `PixelMove` (`Document/SelectionEdits.swift`).
pub struct PixelMove {
    /// The tiled raster edit the pixels were lifted from.
    pub raster: BrushStroke,
    /// The selection outline the pixels started under.
    pub origin: DocumentSelection,
    /// Duplicating moves leave the pixels behind and lift a copy.
    pub duplicate: bool,
    /// The drag's offset from the start, in whole document pixels.
    pub offset: Size,
    /// The offset the raster's tiles were last rebuilt for. They're rebuilt only when something
    /// reads them — the [`Canvas`], or the commit — as the GPU canvas draws the move from the
    /// lifted pixels as they are.
    applied: Size,
}

impl PixelMove {
    /// `PixelMove.init(raster:origin:duplicate:)`.
    pub fn new(raster: BrushStroke, origin: DocumentSelection, duplicate: bool) -> Self {
        PixelMove {
            raster,
            origin,
            duplicate,
            offset: Size::ZERO,
            applied: Size::ZERO,
        }
    }

    /// `PixelMove.applyOffset()`: rebuilds the raster's tiles at the current offset, once per offset.
    pub fn apply_offset(&mut self) -> Result<(), CoreError> {
        if self.offset == self.applied {
            return Ok(());
        }
        self.raster.move_lifted(self.offset, self.duplicate)?;
        self.applied = self.offset;
        Ok(())
    }

    /// Whether the GPU canvas can draw this move: not a layer with a mask or effects, which the
    /// Core Graphics (CPU) canvas draws.
    pub fn draws_on_gpu(&self) -> bool {
        self.raster.layer.mask.is_none()
            && self
                .raster
                .layer
                .effects
                .as_ref()
                .map(|effects| effects_empty(&effects.visible()))
                .unwrap_or(true)
            && if self.duplicate {
                self.raster.original().is_some()
            } else {
                self.raster.holed.is_some()
            }
    }

    /// `PixelMove.movedSelection`: the outline shifted by the drag.
    pub fn moved_selection(&self) -> DocumentSelection {
        let shift = AffineTransform::translation(self.offset.width, self.offset.height);
        DocumentSelection::with_style(
            self.origin.path.transformed(&shift),
            self.origin.antialiased,
            self.origin.feather,
        )
    }
}

/// `LayerEffects.visible.isEmpty`: whether the layer draws no effects at all.
fn effects_empty(effects: &LayerEffects) -> bool {
    effects.stroke.is_none()
        && effects.shadow.is_none()
        && effects.color_overlay.is_none()
        && effects.inner_shadow.is_none()
        && effects.outer_glow.is_none()
        && effects.inner_glow.is_none()
}

/// What [`FloatingMerge::merge`] hands back: the merged pixels on the source layer's own grid, the
/// transform that places them, and the mask grown to cover them.
pub struct MergedFloating {
    pub asset: ImportedImage,
    pub transform: LayerTransform,
    pub mask: Option<LayerMask>,
}

/// `FloatingMerge` (`Document/FloatingSelection.swift`).
pub struct FloatingMerge;

impl FloatingMerge {
    /// Draws the floating pixels (with their transform) onto the source layer's own pixel
    /// grid, growing the layer where they now extend past it. A mask grows with it, revealing
    /// the new area.
    pub fn merge(
        pixels: &PixelImage,
        transform: &LayerTransform,
        source: &ImageLayer,
    ) -> Result<MergedFloating, CoreError> {
        let Some(source_image) = source
            .asset
            .as_ref()
            .and_then(|asset| asset.image.as_rgba())
        else {
            return Err(CoreError::MissingLayer(source.name.clone()));
        };
        let width = source_image.width();
        let height = source_image.height();
        let to_document = pixel_to_document(&source.transform, width, height);
        let to_pixels = to_document.inverted();
        let pixel_width = pixels.width();
        let pixel_height = pixels.height();
        let floating_bounds = rect_applying(
            Rect::new(0.0, 0.0, pixel_width as f64, pixel_height as f64),
            &pixel_to_document(transform, pixel_width, pixel_height),
        );
        let floating_bounds = rect_applying(floating_bounds, &to_pixels);
        let original = Rect::new(0.0, 0.0, width as f64, height as f64);
        let extent = original.union(floating_bounds).integral();
        if extent.width() > limits::MAX_SIDE_EXTENT
            || extent.height() > limits::MAX_SIDE_EXTENT
            || extent.width() * extent.height() > limits::MAX_SURFACE_EXTENT
        {
            return Err(CoreError::TooLarge(limits::MAX_SURFACE_PIXELS));
        }
        let mut canvas = Canvas::new_rgba(
            extent.width().max(0.0) as usize,
            extent.height().max(0.0) as usize,
        );
        let placed = original.offset_by(-extent.min_x(), -extent.min_y());
        canvas.draw_image(source_image, placed);
        canvas.save();
        canvas.translate(-extent.min_x(), -extent.min_y());
        canvas.concatenate(to_pixels);
        LayerRenderer::draw(
            pixels,
            transform,
            transform.center(),
            1.0,
            1.0,
            compositor_core::blend::LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        canvas.restore();
        let image = canvas.into_rgba();
        let thumbnail = rgba_thumbnail(&image);
        let asset = ImportedImage::new(
            PixelImage::Rgba(Arc::new(image)),
            thumbnail,
            source.name.clone(),
        );
        let mut merged = source.transform;
        merged.size = Size::new(
            extent.width() * source.size().width / width as f64,
            extent.height() * source.size().height / height as f64,
        );
        let center = to_document.applying(Point::new(extent.mid_x(), extent.mid_y()));
        merged.origin = Point::new(
            center.x - merged.size.width / 2.0,
            center.y - merged.size.height / 2.0,
        );
        let mut mask = source.mask.clone();
        if let Some(current) = source.mask.as_ref() {
            if current.placement.is_none() && extent != original {
                if let Some(mask_image) = current.asset.image.as_gray() {
                    let mut grown = Gray8Image::new(
                        extent.width().max(0.0) as usize,
                        extent.height().max(0.0) as usize,
                    );
                    // The Swift filled the whole context white, then drew the mask with its black
                    // fill + clipped white fill, so `placed` becomes the mask and the rest stays white.
                    grown.data_mut().fill(255);
                    draw_gray_scaled(&mut grown, mask_image, placed);
                    mask = Some(current.replacing(mask_imported_image(grown)));
                }
            }
        }
        Ok(MergedFloating {
            asset,
            transform: merged,
            mask,
        })
    }
}

impl EditorSession {
    /// `canTransformSelection`: Cmd-T with a selection floats the selected pixels.
    pub fn can_transform_selection(&self) -> bool {
        self.transform_edit.is_none()
            && self.can_edit_pixels()
            && !self.is_mask_selected
            && self
                .selection()
                .is_some_and(|selection| !selection.is_empty())
            && self
                .active_layer()
                .is_some_and(|layer| layer.asset.is_some())
    }

    /// Cmd-T: transforms the selected pixels when there is a selection, else the layer.
    pub fn transform_command(&mut self) {
        if self.can_transform_selection() {
            self.begin_selection_transform();
        } else {
            self.begin_transform(true);
        }
    }

    /// Cmd-T with a selection: lifts the selected pixels onto a "Floating Selection" layer and opens
    /// the outer "Transform Selection" edit, closed by [`Self::merge_floating_transform`] or
    /// [`Self::cancel_floating_transform`].
    ///
    /// The Swift `await`s the pixel render and the clear; the port runs them synchronously (the busy
    /// flag covered the wait), so a refused render (no pixels under the selection) leaves the
    /// document untouched, exactly as the Swift `guard`s did.
    pub fn begin_selection_transform(&mut self) {
        if !self.can_transform_selection() {
            return;
        }
        let Some(before) = self.document.clone() else {
            return;
        };
        let Some(source) = self.active_layer().cloned() else {
            return;
        };
        let (lifted_image, lifted_region) = match self.render_selected_pixels(&source, false) {
            Ok(Some(lifted)) => lifted,
            Ok(None) => return,
            Err(error) => {
                self.brush_error = Some(error.to_string());
                return;
            }
        };
        let before_active = self.active_layer_id;
        // Outer edit: closed by commitTransform (merge) or cancelTransform (restore).
        self.begin_edit("Transform Selection");
        self.clear_selected_pixels();
        let Some(index) = self.document.as_ref().and_then(|document| {
            document
                .layers
                .iter()
                .position(|layer| layer.id == source.id)
        }) else {
            self.document = Some(before);
            self.end_edit();
            return;
        };
        let thumbnail = rgba_thumbnail(&lifted_image);
        let mut floating = ImageLayer::from_asset(
            ImportedImage::new(
                PixelImage::Rgba(Arc::new(lifted_image)),
                thumbnail,
                "Floating Selection",
            ),
            lifted_region.origin,
        );
        floating.name = "Floating Selection".to_string();
        floating.parent_id = source.parent_id;
        floating.opacity = source.opacity;
        floating.blend_mode = source.blend_mode;
        let floating_id = floating.id;
        let draft = floating.transform;
        if let Some(document) = self.document.as_mut() {
            document.layers.insert(index + 1, floating);
        }
        self.set_active_layer(Some(floating_id));
        self.tool = compositor_core::document::NavigationTool::Move;
        let mut edit = TransformEdit::new(floating_id, draft, true);
        edit.floating = Some(FloatingTransform {
            source_id: source.id,
            before,
            before_active,
            original: draft,
            pixel_size: lifted_region.size,
        });
        self.transform_edit = Some(edit);
    }

    /// Maps the original selection to where the floating pixels are now.
    pub fn floating_selection_transform(&self, edit: &TransformEdit) -> Option<AffineTransform> {
        let floating = edit.floating.as_ref()?;
        let width = floating.pixel_size.width as usize;
        let height = floating.pixel_size.height as usize;
        Some(
            pixel_to_document(&floating.original, width, height)
                .inverted()
                // Swift `.concatenating` reads receiver-first: `then` here.
                .then(pixel_to_document(&edit.draft, width, height)),
        )
    }

    /// Composites the transformed pixels back into their layer, moves the selection with
    /// them, and closes the undo step. Synchronous so tool/layer switches and Save can call it.
    ///
    /// `commitTransform` calls this when `edit.floating` is set and the draft moved; the Swift
    /// `defer { endEdit() }` means the undo step closes whether the merge succeeded or restored the
    /// document.
    pub fn merge_floating_transform(&mut self, edit: &TransformEdit, floating: &FloatingTransform) {
        if let Err(error) = self.merge_floating_transform_inner(edit, floating) {
            self.document = Some(floating.before.clone());
            self.set_active_layer(floating.before_active);
            self.brush_error = Some(error.to_string());
        }
        self.end_edit();
    }

    fn merge_floating_transform_inner(
        &mut self,
        edit: &TransformEdit,
        floating: &FloatingTransform,
    ) -> Result<(), CoreError> {
        if !edit.draft.is_valid() {
            return Err(CoreError::Message(
                "This is not a valid Compositor project, or its metadata is damaged.".to_string(),
            ));
        }
        let Some(document) = self.document.clone() else {
            return Err(CoreError::NoDocument);
        };
        let Some(pixels) = document
            .layers
            .iter()
            .find(|layer| layer.id == edit.layer_id)
            .and_then(|layer| layer.asset.as_ref())
            .map(|asset| asset.image.clone())
        else {
            return Err(CoreError::MissingLayer(edit.layer_id.to_string()));
        };
        let Some(source) = document
            .layers
            .iter()
            .find(|layer| layer.id == floating.source_id)
            .cloned()
        else {
            return Err(CoreError::MissingLayer(floating.source_id.to_string()));
        };
        // A distorted selection is warped into its new shape first, then merged like any other.
        let placed = match edit.corners.as_ref() {
            Some(corners) => {
                let (image, transform, _crop) =
                    DistortWarp::warp_trimmed(&pixels, &edit.draft, corners)?;
                (image, transform)
            }
            None => (pixels.clone(), edit.draft),
        };
        let merged = FloatingMerge::merge(&placed.0, &placed.1, &source)?;
        let selection = self.selection().cloned();
        let moved = if let Some(corners) = edit.corners.as_ref() {
            let placement = pixel_to_document(
                &floating.original,
                floating.pixel_size.width as usize,
                floating.pixel_size.height as usize,
            );
            selection.and_then(|selection| {
                DistortWarp::map_path(
                    &selection.path,
                    placement,
                    floating.pixel_size,
                    &edit.draft,
                    corners,
                )
                .map(|path| {
                    DocumentSelection::with_style(path, selection.antialiased, selection.feather)
                })
            })
        } else {
            self.floating_selection_transform(edit)
                .and_then(|transform| {
                    selection.map(|selection| {
                        DocumentSelection::with_style(
                            selection.path.transformed(&transform),
                            selection.antialiased,
                            selection.feather,
                        )
                    })
                })
        };
        if let Some(document) = self.document.as_mut() {
            document.layers.retain(|layer| layer.id != edit.layer_id);
        }
        let Some(index) = self.document.as_ref().and_then(|document| {
            document
                .layers
                .iter()
                .position(|layer| layer.id == source.id)
        }) else {
            return Err(CoreError::MissingLayer(source.id.to_string()));
        };
        if let Some(document) = self.document.as_mut() {
            document.layers[index] = ImageLayer::with_id(
                source.id,
                Some(merged.asset),
                source.name,
                source.is_visible,
                merged.transform,
                source.parent_id,
                false,
                source.opacity,
                source.blend_mode,
                merged.mask,
                source.mask_source_id,
                None,
                None,
                None,
                None,
            );
            document.selection = moved;
        }
        self.set_active_layer(Some(source.id));
        Ok(())
    }

    /// Escape during a floating transform: the document goes back exactly as it was.
    pub fn cancel_floating_transform(&mut self, floating: &FloatingTransform) {
        self.document = Some(floating.before.clone());
        self.set_active_layer(floating.before_active);
        self.end_edit();
    }

    /// The outline to draw: during a pixel move, the original shifted by the drag; while
    /// transforming selected pixels, the outline follows the handles.
    pub fn displayed_selection(&self) -> Option<DocumentSelection> {
        if let Some(pixel_move) = self.pixel_move.as_ref() {
            return Some(pixel_move.moved_selection());
        }
        if let Some(edit) = self.transform_edit.as_ref() {
            if let Some(transform) = self.floating_selection_transform(edit) {
                if let Some(selection) = self.selection() {
                    return Some(DocumentSelection::with_style(
                        selection.path.transformed(&transform),
                        selection.antialiased,
                        selection.feather,
                    ));
                }
            }
        }
        self.selection().cloned()
    }

    /// Starts moving the selected image pixels; false when there is nothing to move
    /// (no selection, a mask target, or no pixels under the selection).
    pub fn begin_pixel_move(&mut self, duplicate: bool) -> bool {
        if self.pixel_move.is_some() {
            return false;
        }
        let Some(selection) = self.selection().cloned() else {
            return false;
        };
        if selection.is_empty() || !self.can_paint() || self.is_mask_selected {
            return false;
        }
        let Some(layer) = self.active_layer().cloned() else {
            return false;
        };
        if layer.asset.is_none() {
            return false;
        }
        match self.make_raster_edit(&layer, BrushSettings::default(), false) {
            Ok(mut raster) => match raster.lift_selection() {
                Ok(true) => {
                    self.finish_opacity_edit();
                    self.pixel_move = Some(PixelMove::new(raster, selection, duplicate));
                    true
                }
                Ok(false) => false,
                Err(error) => {
                    self.brush_error = Some(error.to_string());
                    false
                }
            },
            Err(error) => {
                self.brush_error = Some(error.to_string());
                false
            }
        }
    }

    /// Previews the pixels `offset` document pixels (whole pixels) away. The stored
    /// selection stays put until commit; the outline is drawn from [`Self::displayed_selection`].
    pub fn move_pixels(&mut self, offset: Size) {
        let Some(pixel_move) = self.pixel_move.as_mut() else {
            return;
        };
        pixel_move.offset = Size::new(offset.width.round(), offset.height.round());
        self.brush_revision += 1;
    }

    /// Commits the pixels and the moved outline together as one "Move Pixels" undo step.
    /// The outline keeps showing at its new place throughout, so nothing jumps back.
    pub fn finish_pixel_move(&mut self) {
        let Some(mut pixel_move) = self.pixel_move.take() else {
            return;
        };
        if self.is_project_busy {
            self.pixel_move = Some(pixel_move);
            return;
        }
        if pixel_move.offset != Size::ZERO {
            let moved = pixel_move.moved_selection();
            let name = if pixel_move.duplicate {
                "Duplicate Pixels"
            } else {
                "Move Pixels"
            };
            let result = pixel_move.apply_offset().and_then(|()| {
                let mut also_apply = |session: &mut EditorSession| {
                    if let Some(document) = session.document.as_mut() {
                        document.selection = Some(moved.clone());
                    }
                };
                self.commit_raster_edit(&pixel_move.raster, name, Some(&mut also_apply))
            });
            if let Err(error) = result {
                self.brush_error = Some(error.to_string());
            }
        }
        self.brush_revision += 1;
    }

    /// Escape during a pixel move: the lifted pixels are dropped and the layer keeps its own.
    pub fn cancel_pixel_move(&mut self) {
        if self.pixel_move.is_none() {
            return;
        }
        self.pixel_move = None;
        self.brush_revision += 1;
    }

    /// Cmd-arrow: moves the selected pixels 1 px (10 px with Shift) as one undo step.
    pub fn nudge_pixels(&mut self, dx: f64, dy: f64) {
        if !self.begin_pixel_move(false) {
            // The Swift beeps when there is nothing to move; the port has no sound.
            return;
        }
        self.move_pixels(Size::new(dx, dy));
        self.finish_pixel_move();
    }
}
