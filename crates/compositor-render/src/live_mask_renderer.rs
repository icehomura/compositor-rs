//! Live clipping masks: the per-render dependency renderer (`LiveMaskRenderer`), the mask-source graph
//! (`LiveMaskGraph`) and the baker (`LiveMaskBaker`).
//!
//! Ported from `Rendering/LiveMaskRenderer.swift` and `Document/LiveLayerMask.swift` (the graph and
//! baker; the `EditorSession` commands around them belong to `compositor-session`). The bitmap
//! `CGContext` is [`Canvas`], `CGImage` is [`PixelImage`]/[`SharedGray`], and the alpha-extraction C
//! kernels are the ports in [`compositor_pixels::brush_pixels`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use compositor_core::blend::LayerBlendMode;
use compositor_core::document::{ProjectError, ProjectLayerRecord, ProjectSnapshot};
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_adjustment::LayerAdjustment;
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_transform::{pixel_to_document, LayerTransform};
use compositor_core::limits::MAX_SURFACE_EXTENT;
use compositor_core::raster::THUMBNAIL_MAX_SIDE;
use compositor_core::{CoreError, Gray8Image, Id, Rgba8Image, SharedGray};
use compositor_pixels::blend::blend_over;
use compositor_pixels::brush_pixels::{layer_extract_alpha, layer_restore_alpha, layer_unpremultiply_opaque};
use compositor_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_pixels::raster::Raster;
use rayon::prelude::*;

use crate::adjustment_surface::AdjustmentApply;
use crate::layer_renderer::LayerRenderer;

/// Per-render dependency cache. Coverage uses source alpha including its own masks,
/// independent of source visibility and color. Only the current clipped region is allocated.
pub struct LiveMaskRenderer<'a> {
    pub bounds: Rect,
    source: Box<dyn Fn(Id) -> Option<Id> + 'a>,
    draw_own: Box<dyn Fn(Id, &mut Canvas) + 'a>,
    cache: HashMap<Id, SharedGray>,
    visiting: HashSet<Id>,
    stacks: HashMap<Id, Vec<Id>>,
    stacked: HashSet<Id>,
    stack_modes: HashMap<Id, LayerBlendMode>,
    blend_mode: Box<dyn Fn(Id) -> LayerBlendMode + 'a>,
    pub adjustment: Box<dyn Fn(Id) -> Option<LayerAdjustment> + 'a>,
    pub adjustment_opacity: Box<dyn Fn(Id) -> f64 + 'a>,
    pub adjustment_clip: Box<dyn Fn(Id, &mut Canvas) + 'a>,
    pub adjustment_scale: f64,
    /// The part of the document `bounds` shows, when the context isn't laid out in document pixels (the
    /// canvas), so Grain's and Add Noise's patterns stay with the document.
    pub adjustment_region: Option<Box<dyn Fn(Rect) -> Rect + 'a>>,
    /// Pixels per unit of `bounds` for the surfaces made along the way (a clipping stack, a coverage):
    /// the screen's, on the canvas, so they're as sharp as what they're drawn into.
    pub resolution: f64,
}

impl<'a> LiveMaskRenderer<'a> {
    /// `LiveMaskRenderer.init(bounds:source:drawOwn:)`: the adjustment hooks default to none, one and a
    /// no-op clip, and `resolution` to 1, as the Swift stored properties do.
    pub fn new(
        bounds: Rect,
        source: impl Fn(Id) -> Option<Id> + 'a,
        draw_own: impl Fn(Id, &mut Canvas) + 'a,
    ) -> Self {
        LiveMaskRenderer {
            bounds: bounds.integral(),
            source: Box::new(source),
            draw_own: Box::new(draw_own),
            cache: HashMap::new(),
            visiting: HashSet::new(),
            stacks: HashMap::new(),
            stacked: HashSet::new(),
            stack_modes: HashMap::new(),
            blend_mode: Box::new(|_| LayerBlendMode::Normal),
            adjustment: Box::new(|_| None),
            adjustment_opacity: Box::new(|_| 1.0),
            adjustment_clip: Box::new(|_, _| {}),
            adjustment_scale: 1.0,
            adjustment_region: None,
            resolution: 1.0,
        }
    }

    /// `bounds` in the surfaces' pixels, and whether a surface that size is allowed.
    fn pixel_size(&self) -> Option<(usize, usize)> {
        let width = (self.bounds.width() * self.resolution).round();
        let height = (self.bounds.height() * self.resolution).round();
        if !(width > 0.0 && height > 0.0) {
            return None;
        }
        if width * height > MAX_SURFACE_EXTENT {
            return None;
        }
        Some((width as usize, height as usize))
    }

    fn adjust(&mut self, id: Id, canvas: &mut Canvas) {
        let Some(settings) = (self.adjustment)(id) else { return };
        let original = canvas.snapshot();
        let region = match &self.adjustment_region {
            Some(region) => region(self.bounds),
            None => self.bounds,
        };
        let Ok(mut adjusted) = settings.apply(&original, Some(region), self.adjustment_scale) else {
            return;
        };
        let mode = (self.blend_mode)(id);
        if mode != LayerBlendMode::Normal {
            // Blend colors at full coverage, then restore the original alpha.
            // Source-over of two translucent copies would thicken soft edges.
            let (width, height) = (original.width(), original.height());
            let mut base = original.clone();
            let mut top = adjusted.clone();
            let mut alpha = Gray8Image::new(width, height);
            let base_stride = base.stride();
            let alpha_stride = alpha.stride();
            layer_extract_alpha(base.data(), base_stride, alpha.data_mut(), alpha_stride, width, height);
            let base_stride = base.stride();
            layer_unpremultiply_opaque(base.data_mut(), base_stride, width, height);
            let top_stride = top.stride();
            layer_unpremultiply_opaque(top.data_mut(), top_stride, width, height);
            // Core Graphics has no Linear Dodge and the like, and gets Color Burn and Dodge wrong (see
            // `SeparableBlend`); the kernels here compute every mode directly.
            blend_over(mode, &mut base, &top, 1.0);
            let base_stride = base.stride();
            let alpha_stride = alpha.stride();
            layer_restore_alpha(base.data_mut(), base_stride, alpha.data(), alpha_stride, width, height);
            adjusted = base;
        }
        let opacity = (self.adjustment_opacity)(id);
        if opacity < 1.0 {
            // `CIBlendWithMask` with a constant coverage: the adjusted colors stand in for the original
            // by the adjustment layer's opacity.
            cross_fade(&mut adjusted, &original, opacity);
        }
        canvas.save();
        (self.adjustment_clip)(id, canvas);
        Raster::draw(&PixelImage::Rgba(Arc::new(adjusted)), self.bounds, false, canvas);
        canvas.restore();
    }

    /// Clipping stacks share the base's alpha instead of painting that
    /// alpha over itself. Other dependency links retain independent-mask behavior.
    pub fn prepare_stacks(
        &mut self,
        ids: &[Id],
        parent: impl Fn(Id) -> Option<Id>,
        blend: impl Fn(Id) -> LayerBlendMode,
    ) {
        let modes: HashMap<Id, LayerBlendMode> = ids.iter().map(|id| (*id, blend(*id))).collect();
        self.blend_mode = Box::new(move |id| modes.get(&id).copied().unwrap_or(LayerBlendMode::Normal));
        for (index, base) in ids.iter().enumerate() {
            if (self.source)(*base).is_some() || (self.adjustment)(*base).is_some() {
                continue;
            }
            let mut children: Vec<Id> = Vec::new();
            for child in ids.iter().skip(index + 1) {
                if (self.source)(*child) == Some(*base) && parent(*child) == parent(*base) {
                    children.push(*child);
                } else {
                    break;
                }
            }
            if children.is_empty() {
                continue;
            }
            self.stacks.insert(*base, children.clone());
            self.stack_modes.insert(*base, blend(*base));
            self.stacked.extend(children);
        }
    }

    pub fn draw_composite(&mut self, id: Id, canvas: &mut Canvas) {
        if self.stacked.contains(&id) {
            return;
        }
        if (self.adjustment)(id).is_some() {
            if (self.source)(id).is_none() {
                self.adjust(id, canvas);
            }
            return;
        }
        let children = self.stacks.get(&id).cloned();
        let pixel_size = self.pixel_size();
        let (Some(children), Some((width, height))) = (children, pixel_size) else {
            if let Some(children) = self.stacks.get(&id).cloned() {
                self.stacked.retain(|child| !children.contains(child));
            }
            self.draw(id, canvas);
            return;
        };
        let mut group = Canvas::new_rgba(width, height);
        group.scale(self.resolution, self.resolution);
        group.translate(-self.bounds.min_x(), -self.bounds.min_y());
        (self.draw_own)(id, &mut group);
        // Swift mutates the group context's buffer in place; here the image moves out of the canvas and
        // back in, so there is no extra copy of the surface.
        let mut pixels = group.into_rgba();
        let mut alpha = Gray8Image::new(width, height);
        let pixels_stride = pixels.stride();
        let alpha_stride = alpha.stride();
        layer_extract_alpha(pixels.data(), pixels_stride, alpha.data_mut(), alpha_stride, width, height);
        let pixels_stride = pixels.stride();
        layer_unpremultiply_opaque(pixels.data_mut(), pixels_stride, width, height);
        let mut group = Canvas::from_rgba(pixels);
        for child in &children {
            if (self.adjustment)(*child).is_some() {
                self.adjust(*child, &mut group);
            } else {
                (self.draw_own)(*child, &mut group);
            }
        }
        let mut pixels = group.into_rgba();
        let pixels_stride = pixels.stride();
        let alpha_stride = alpha.stride();
        layer_restore_alpha(pixels.data_mut(), pixels_stride, alpha.data(), alpha_stride, width, height);
        // The stack blends in its base's mode — through the kernels for the ones Core Graphics can't
        // draw (Linear Dodge and the rest), which it would otherwise draw as Normal.
        let mode = self
            .stack_modes
            .get(&id)
            .copied()
            .unwrap_or(LayerBlendMode::Normal);
        canvas.save();
        canvas.set_blend_mode(mode);
        canvas.draw_image(&pixels, self.bounds);
        canvas.restore();
    }

    pub fn draw(&mut self, id: Id, canvas: &mut Canvas) {
        canvas.save();
        if let Some(source_id) = (self.source)(id) {
            let Some(coverage) = self.coverage(source_id) else {
                canvas.restore();
                return;
            };
            // CGImage rows are top-down; the canvas clips a mask onto its rect the same way.
            canvas.clip_to_image(&coverage, self.bounds);
        }
        (self.draw_own)(id, canvas);
        canvas.restore();
    }

    fn coverage(&mut self, id: Id) -> Option<SharedGray> {
        if let Some(image) = self.cache.get(&id) {
            return Some(image.clone());
        }
        if self.visiting.contains(&id) || self.visiting.len() >= 256 {
            return None;
        }
        let (width, height) = self.pixel_size()?;
        self.visiting.insert(id);
        let mut pixels = Canvas::new_rgba(width, height);
        pixels.scale(self.resolution, self.resolution);
        pixels.translate(-self.bounds.min_x(), -self.bounds.min_y());
        self.draw(id, &mut pixels);
        let rgba = pixels.into_rgba();
        let mut image = Gray8Image::new(width, height);
        let rgba_stride = rgba.stride();
        let image_stride = image.stride();
        layer_extract_alpha(rgba.data(), rgba_stride, image.data_mut(), image_stride, width, height);
        self.visiting.remove(&id);
        let image: SharedGray = Arc::new(image);
        self.cache.insert(id, image.clone());
        Some(image)
    }
}

/// `LayerTransform(origin:size:)` — the Swift memberwise initializer's defaults for the fields the
/// callers here don't set.
fn transform_at(origin: Point, size: Size) -> LayerTransform {
    LayerTransform {
        origin,
        size,
        ..Default::default()
    }
}

/// `CIBlendWithMask` with a constant mask: the foreground's premultiplied components against the
/// background's, `foreground·mask + background·(1 − mask)`, rounded once per byte.
fn cross_fade(foreground: &mut Rgba8Image, background: &Rgba8Image, mask: f64) {
    let mask = if mask.is_nan() { 0.0 } else { mask.clamp(0.0, 1.0) };
    let height = foreground.height().min(background.height());
    let width = foreground.width().min(background.width());
    if width == 0 || height == 0 {
        return;
    }
    let stride = foreground.stride();
    foreground
        .data_mut()
        .par_chunks_exact_mut(stride)
        .zip(background.data().par_chunks_exact(background.stride()))
        .take(height)
        .for_each(|(row, background)| {
            for (pixel, back) in row.chunks_exact_mut(4).zip(background.chunks_exact(4)).take(width) {
                for channel in 0..4 {
                    let value = pixel[channel] as f64 * mask + back[channel] as f64 * (1.0 - mask);
                    pixel[channel] = value.round().clamp(0.0, 255.0) as u8;
                }
            }
        });
}

/// The rasterizing half of `LayerMask`: `LayerMask.clipImage` and the `placed`/`drawSmooth` helpers it
/// draws with need the drawing engine, so they live here (`compositor-core`'s `layer_mask` documents the
/// split), driven by `compositor-render`.
pub trait MaskClip {
    /// The mask as a layer's renderers take it: an image stretched over the layer's `width` × `height`
    /// pixel grid at `layer`. Covering the layer (`placement` nil) that's the mask itself; placed apart,
    /// it is resampled into that grid — at most `limit` pixels across — and cached. `None` while disabled.
    fn clip_image(
        &self,
        placement: Option<&LayerTransform>,
        over: &LayerTransform,
        width: usize,
        height: usize,
        limit: Option<f64>,
    ) -> Option<PixelImage>;
}

impl MaskClip for LayerMask {
    fn clip_image(
        &self,
        placement: Option<&LayerTransform>,
        over: &LayerTransform,
        width: usize,
        height: usize,
        limit: Option<f64>,
    ) -> Option<PixelImage> {
        let image = self.enabled_image()?;
        let Some(placement) = placement else {
            return Some(image.clone());
        };
        if placement.same_placement(*over) || width == 0 || height == 0 {
            return Some(image.clone());
        }
        let PixelImage::Gray(mask) = image else {
            return Some(image.clone());
        };
        let factor = limit
            .map(|limit| 1.0f64.min(limit.max(1.0) / width.max(height) as f64))
            .unwrap_or(1.0);
        let source_width = mask.width();
        let source_height = mask.height();
        let target_width = ((width as f64 * factor).ceil() as usize).max(1);
        let target_height = ((height as f64 * factor).ceil() as usize).max(1);
        let background = self
            .asset
            .thumbnail
            .as_gray()
            .map(LayerMask::background)
            .unwrap_or(1.0);
        compositor_core::layer_mask::MaskPlacementCache::shared()
            .image(
                mask,
                placement,
                over,
                target_width,
                target_height,
                || {
                // Drawn from a sharp halving near the size the mask covers in the grid.
                let covered = placement.size.width / over.size.width.max(1.0) * target_width as f64;
                let halved =
                    crate::downsample_cache::DownsampleCache::shared().image(image, covered / source_width.max(1) as f64);
                let halved = halved.as_gray()?;
                let mut canvas = Canvas::new_gray(target_width, target_height);
                canvas.set_fill_gray(background);
                canvas.fill_rect(Rect::from_origin_size(
                    Point::ZERO,
                    Size::new(target_width as f64, target_height as f64),
                ));
                canvas.concatenate(LayerMask::placement_in_layer(
                    over,
                    placement,
                    target_width,
                    target_height,
                    source_width,
                    source_height,
                ));
                canvas.set_interpolation_quality(InterpolationQuality::High);
                draw_smooth(
                    &mut canvas,
                    halved,
                    Rect::from_origin_size(Point::ZERO, Size::new(source_width as f64, source_height as f64)),
                );
                Some(Arc::new(canvas.into_gray()))
                },
            )
            .map(PixelImage::Gray)
    }
}

/// `LayerMask.drawSmooth`: `image`'s mask values drawn over `rect` (y down), resampled smoothly.
pub fn draw_smooth(canvas: &mut Canvas, image: &Gray8Image, rect: Rect) {
    canvas.save();
    canvas.set_fill_gray(0.0);
    canvas.fill_rect(rect);
    canvas.clip_to_image(image, rect);
    canvas.set_fill_gray(1.0);
    canvas.fill_rect(rect);
    canvas.restore();
}

/// `LiveMaskGraph.validate(_:)`: layer ids are unique; every layer's `maskSourceID` chain stays
pub struct LiveMaskGraph;

impl LiveMaskGraph {
    /// `LiveMaskGraph.validate(_:)`: layer ids are unique; every layer's `maskSourceID` chain stays
    /// under 256 nodes and never revisits one; every endpoint exists, is not a folder or an adjustment
    /// layer; and no folder depends on a mask source.
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
                let Some(record) = records.get(&id).copied() else {
                    return Err(ProjectError::Invalid);
                };
                if let Some(source) = record.mask_source_id {
                    let source = records.get(&source).copied();
                    if record.is_group.unwrap_or(false)
                        || source.is_none()
                        || source.is_some_and(|record| record.is_group == Some(true))
                        || source.is_some_and(|record| record.adjustment.is_some())
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

/// Bakes a layer's live mask dependency into its own pixels (`LiveMaskBaker`).
pub struct LiveMaskBaker;

impl LiveMaskBaker {
    /// `LiveMaskBaker.bake(_:target:)`. `None` when the target layer or its pixels are missing.
    pub fn bake(snapshot: &ProjectSnapshot, target: Id) -> Result<Option<ImportedImage>, CoreError> {
        let Some(record) = snapshot.manifest.layers.iter().find(|record| record.id == target) else {
            return Ok(None);
        };
        let Some(original) = snapshot.images.get(&target) else {
            return Ok(None);
        };
        let width = original.image.width();
        let height = original.image.height();
        let bounds = Rect::from_origin_size(Point::ZERO, Size::new(width as f64, height as f64));
        let transform = transform_at(Point::ZERO, bounds.size);
        let mut canvas = Canvas::new_rgba(width, height);
        {
            let by_id: HashMap<Id, ProjectLayerRecord> = snapshot
                .manifest
                .layers
                .iter()
                .map(|record| (record.id, record.clone()))
                .collect();
            let inverse = pixel_to_document(&record.transform, width, height).inverted();
            let images = &snapshot.images;
            let masks = &snapshot.masks;
            let mut live = LiveMaskRenderer::new(
                bounds,
                |id| by_id.get(&id).and_then(|record| record.mask_source_id),
                |id, canvas| {
                    let Some(layer) = by_id.get(&id) else { return };
                    let Some(asset) = images.get(&id) else { return };
                    if id == target {
                        // Bake only the live dependency into pixels; retain the target's raster mask and
                        // appearance.
                        LayerRenderer::draw(
                            &asset.image,
                            &transform,
                            transform.center(),
                            1.0,
                            1.0,
                            LayerBlendMode::Normal,
                            None,
                            canvas,
                        );
                    } else {
                        let mask = project_mask(layer, masks);
                        let clip = mask.and_then(|mask| {
                            mask.clip_image(
                                mask.placement.as_ref(),
                                &layer.transform,
                                asset.image.width(),
                                asset.image.height(),
                                None,
                            )
                        });
                        canvas.save();
                        canvas.concatenate(inverse);
                        LayerRenderer::draw(
                            &asset.image,
                            &layer.transform,
                            layer.transform.center(),
                            1.0,
                            layer.effective_opacity(&by_id),
                            LayerBlendMode::Normal,
                            clip.as_ref(),
                            canvas,
                        );
                        canvas.restore();
                    }
                },
            );
            live.draw(target, &mut canvas);
        }
        let image = canvas.into_rgba();
        let shared: compositor_core::SharedImage = Arc::new(image);
        let thumbnail = compositor_pixels::resample::thumbnail(
            &PixelImage::Rgba(shared.clone()),
            THUMBNAIL_MAX_SIDE,
        );
        Ok(Some(ImportedImage::new(
            PixelImage::Rgba(shared),
            thumbnail,
            original.name.clone(),
        )))
    }
}

/// `ProjectSnapshot.mask(for:)`: the layer's mask asset, with the manifest's enabled/linked defaults.
fn project_mask(layer: &ProjectLayerRecord, masks: &HashMap<Id, ImportedImage>) -> Option<LayerMask> {
    layer.mask_file.as_ref()?;
    let asset = masks.get(&layer.id)?;
    Some(LayerMask::with_placement(
        asset.clone(),
        layer.mask_enabled.unwrap_or(true),
        layer.mask_placement,
        layer.mask_linked.unwrap_or(true),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::PaletteColor;

    fn record(id: Id, name: &str) -> ProjectLayerRecord {
        ProjectLayerRecord {
            id,
            name: name.to_string(),
            is_visible: true,
            transform: transform_at(Point::ZERO, Size::new(8.0, 8.0)),
            image_file: None,
            parent_id: None,
            is_group: None,
            opacity: None,
            blend_mode: None,
            mask_file: None,
            mask_enabled: None,
            mask_source_id: None,
            adjustment: None,
            mask_placement: None,
            mask_linked: None,
            shape: None,
            effects: None,
            text: None,
        }
    }

    /// A chain of three layers validates; a duplicated id does not.
    #[test]
    fn validate_accepts_acyclic_chains_and_rejects_duplicate_ids() {
        let a = compositor_core::new_id();
        let b = compositor_core::new_id();
        let c = compositor_core::new_id();
        let mut first = record(a, "Base");
        let mut second = record(b, "Middle");
        let mut third = record(c, "Top");
        second.mask_source_id = Some(a);
        third.mask_source_id = Some(a);
        let layers = vec![first.clone(), second.clone(), third.clone()];
        assert!(LiveMaskGraph::validate(&layers).is_ok());

        // A cycle b → c → b is refused.
        first.mask_source_id = Some(c);
        let layers = vec![
            ProjectLayerRecord { mask_source_id: Some(c), ..first.clone() },
            ProjectLayerRecord { mask_source_id: Some(c), ..second.clone() },
            ProjectLayerRecord { mask_source_id: Some(b), ..third.clone() },
        ];
        assert!(matches!(LiveMaskGraph::validate(&layers), Err(ProjectError::Invalid)));

        // A duplicate id is refused.
        let layers = vec![first.clone(), first.clone()];
        assert!(matches!(LiveMaskGraph::validate(&layers), Err(ProjectError::Invalid)));
    }

    /// A dangling source, a folder endpoint, an adjustment endpoint and a folder dependent are all
    /// refused.
    #[test]
    fn validate_rejects_bad_endpoints() {
        let a = compositor_core::new_id();
        let b = compositor_core::new_id();
        let folder = compositor_core::new_id();
        let adjustment_id = compositor_core::new_id();

        // Dangling: b depends on a layer that isn't in the manifest.
        let mut dependent = record(b, "Top");
        dependent.mask_source_id = Some(a);
        assert!(matches!(LiveMaskGraph::validate(&[dependent.clone()]), Err(ProjectError::Invalid)));

        // A folder as the endpoint.
        let mut group = record(folder, "Folder");
        group.is_group = Some(true);
        let mut on_folder = record(b, "On folder");
        on_folder.mask_source_id = Some(folder);
        let mut base = record(a, "Base");
        assert!(matches!(
            LiveMaskGraph::validate(&[base.clone(), group, on_folder]),
            Err(ProjectError::Invalid)
        ));

        // The dependent itself a folder.
        let mut folder_dependent = record(b, "Folder dependent");
        folder_dependent.is_group = Some(true);
        folder_dependent.mask_source_id = Some(a);
        base.is_group = None;
        assert!(matches!(
            LiveMaskGraph::validate(&[base.clone(), folder_dependent]),
            Err(ProjectError::Invalid)
        ));

        // An adjustment layer as the endpoint.
        let mut adjustment = record(adjustment_id, "Adjustment");
        adjustment.adjustment = Some(LayerAdjustment::default());
        let mut on_adjustment = record(b, "On adjustment");
        on_adjustment.mask_source_id = Some(adjustment_id);
        assert!(matches!(
            LiveMaskGraph::validate(&[base, adjustment, on_adjustment]),
            Err(ProjectError::Invalid)
        ));
    }

    /// The 256-node ceiling: a 300-long chain is refused, a 100-long one is fine.
    #[test]
    fn validate_enforces_the_two_hundred_and_fifty_six_node_ceiling() {
        let ids: Vec<Id> = (0..300).map(|_| compositor_core::new_id()).collect();
        let chain: Vec<ProjectLayerRecord> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut layer = record(*id, "Layer");
                if index > 0 {
                    layer.mask_source_id = Some(ids[index - 1]);
                }
                layer
            })
            .collect();
        assert!(matches!(LiveMaskGraph::validate(&chain), Err(ProjectError::Invalid)));

        let short: Vec<ProjectLayerRecord> = chain.into_iter().take(100).collect();
        assert!(LiveMaskGraph::validate(&short).is_ok());
    }

    /// `prepare_stacks` groups a base and its contiguous children; the children then draw only through
    /// the base.
    #[test]
    fn prepare_stacks_finds_the_contiguous_children() {
        let base = compositor_core::new_id();
        let child = compositor_core::new_id();
        let other = compositor_core::new_id();
        let mut renderer = LiveMaskRenderer::new(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            // The child's mask source is the base, as `EditorCanvas` wires `maskSourceID`; the stack is
            // formed from those dependency edges.
            move |id| (id == child).then_some(base),
            |_, _| {},
        );
        renderer.prepare_stacks(
            &[other, base, child],
            |_| None,
            |_| LayerBlendMode::Normal,
        );
        assert_eq!(renderer.stacks.get(&base), Some(&vec![child]));
        assert!(renderer.stacked.contains(&child));
        assert!(!renderer.stacked.contains(&base));

        // The child draws nothing on its own now.
        let mut canvas = Canvas::new_rgba(4, 4);
        renderer.draw_composite(child, &mut canvas);
        assert!(canvas.rgba().pixels().all(|pixel| pixel == [0, 0, 0, 0]));

        // The other layer is not stacked and draws normally.
        renderer.draw_composite(other, &mut canvas);
        assert!(canvas.rgba().pixels().all(|pixel| pixel == [0, 0, 0, 0]));
    }

    /// A clipping stack shares the base's alpha: the child's paint only shows where the base does.
    #[test]
    fn a_clipping_stack_keeps_the_bases_alpha() {
        let base = compositor_core::new_id();
        let child = compositor_core::new_id();
        let mut renderer = LiveMaskRenderer::new(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            // The child is clipped to the base (`maskSourceID`), so the two form a clipping stack.
            move |id| (id == child).then_some(base),
            move |id, canvas| {
                let rect = if id == base {
                    Rect::new(0.0, 0.0, 2.0, 2.0)
                } else {
                    Rect::new(0.0, 0.0, 4.0, 4.0)
                };
                canvas.set_fill_color(if id == base {
                    PaletteColor::new(1.0, 0.0, 0.0)
                } else {
                    PaletteColor::new(0.0, 1.0, 0.0)
                });
                canvas.fill_rect(rect);
            },
        );
        renderer.prepare_stacks(&[base, child], |_| None, |_| LayerBlendMode::Normal);
        let mut canvas = Canvas::new_rgba(4, 4);
        renderer.draw_composite(base, &mut canvas);
        // Green where the base was, transparent elsewhere.
        assert_eq!(canvas.rgba().get(1, 1), [0, 255, 0, 255]);
        assert_eq!(canvas.rgba().get(3, 3), [0, 0, 0, 0]);
    }

    /// A layer clipped to another is drawn through that layer's alpha coverage.
    #[test]
    fn draw_clips_through_the_source_coverage() {
        let base = compositor_core::new_id();
        let child = compositor_core::new_id();
        let mut renderer = LiveMaskRenderer::new(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            move |id| (id == child).then_some(base),
            move |id, canvas| {
                let rect = if id == base {
                    Rect::new(0.0, 0.0, 2.0, 2.0)
                } else {
                    Rect::new(0.0, 0.0, 4.0, 4.0)
                };
                canvas.set_fill_color(if id == base {
                    PaletteColor::new(1.0, 0.0, 0.0)
                } else {
                    PaletteColor::new(0.0, 1.0, 0.0)
                });
                canvas.fill_rect(rect);
            },
        );
        let mut canvas = Canvas::new_rgba(4, 4);
        renderer.draw(child, &mut canvas);
        // The child's paint shows only inside the base's alpha.
        assert_eq!(canvas.rgba().get(1, 1), [0, 255, 0, 255]);
        assert_eq!(canvas.rgba().get(3, 3), [0, 0, 0, 0]);
    }

    /// A cycle in the source chain terminates: the re-entered node yields no coverage instead of
    /// recursing forever, and the outer call still gets the (empty) surface.
    #[test]
    fn a_source_cycle_terminates_without_coverage() {
        let a = compositor_core::new_id();
        let b = compositor_core::new_id();
        let mut renderer = LiveMaskRenderer::new(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            move |id| if id == a { Some(b) } else { Some(a) },
            |_, _| {},
        );
        let coverage = renderer.coverage(a).expect("the cycle ends with an empty coverage");
        assert!(coverage.data().iter().all(|value| *value == 0));
    }

    /// The coverage surface is cached per node: a second request returns the same allocation.
    #[test]
    fn coverage_is_cached_by_identity() {
        let a = compositor_core::new_id();
        let mut renderer = LiveMaskRenderer::new(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            |_| None,
            |_, canvas| {
                canvas.set_fill_color(PaletteColor::BLACK);
                canvas.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0));
            },
        );
        let first = renderer.coverage(a).unwrap();
        let second = renderer.coverage(a).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// `LayerMask.clipImage`: covering the layer it is the mask itself; placed apart it is resampled
    /// into the layer's grid (at most `limit` pixels across) and cached; disabled it is nothing.
    #[test]
    fn clip_image_resamples_a_placed_mask_into_the_layer_grid() {
        let source: SharedGray = Arc::new(Gray8Image::uniform(4, 4, 255));
        let asset = ImportedImage::new(
            PixelImage::Gray(source.clone()),
            PixelImage::Gray(source.clone()),
            "Layer Mask",
        );
        let layer = transform_at(Point::ZERO, Size::new(16.0, 16.0));
        let placement = transform_at(Point::new(4.0, 4.0), Size::new(8.0, 8.0));

        // Covering the layer: the mask's own pixels, by identity.
        let covering = LayerMask::new(asset.clone());
        let clip = covering.clip_image(None, &layer, 16, 16, None).unwrap();
        match &clip {
            PixelImage::Gray(gray) => assert!(Arc::ptr_eq(gray, &source)),
            _ => panic!("a mask's clip is gray"),
        }

        // Placed apart: resampled into the layer's grid, and cached between calls.
        let placed = LayerMask::with_placement(asset.clone(), true, Some(placement), true);
        let first = placed.clip_image(Some(&placement), &layer, 16, 16, None).unwrap();
        assert_eq!((first.width(), first.height()), (16, 16));
        let second = placed.clip_image(Some(&placement), &layer, 16, 16, None).unwrap();
        match (&first, &second) {
            (PixelImage::Gray(a), PixelImage::Gray(b)) => assert!(Arc::ptr_eq(a, b)),
            _ => panic!("a mask's clip is gray"),
        }

        // The limit caps the grid the mask is resampled into.
        let limited = placed.clip_image(Some(&placement), &layer, 16, 16, Some(4.0)).unwrap();
        assert_eq!((limited.width(), limited.height()), (4, 4));

        // A uniform black mask hides the whole grid.
        let black: SharedGray = Arc::new(Gray8Image::uniform(4, 4, 0));
        let black_asset = ImportedImage::new(
            PixelImage::Gray(black.clone()),
            PixelImage::Gray(black),
            "Layer Mask",
        );
        let hiding = LayerMask::with_placement(black_asset, true, Some(placement), true);
        let clip = hiding.clip_image(Some(&placement), &layer, 16, 16, None).unwrap();
        assert!(clip.as_gray().unwrap().data().iter().all(|value| *value == 0));

        // Disabled: nothing at all.
        let disabled = LayerMask::with_placement(asset, false, Some(placement), true);
        assert!(disabled.clip_image(Some(&placement), &layer, 16, 16, None).is_none());
    }
}
