//! A layer's effects kept at full resolution while it is painted (`LayerEffectsSurface`).
//!
//! A layer's effects kept at full resolution while it is painted, and brought up to date only where the
//! paint changed. Rebuilding every effect over a whole layer for each dab is what made painting drag;
//! the cost here follows the brush instead, so a small brush on a big layer is cheap however large the
//! layer is.
//!
//! Ported from `Rendering/LayerEffectsSurface.swift`: the bitmap `CGContext` is [`Canvas`], `CGImage`
//! is [`Rgba8Image`]/[`PixelImage`], and the `MetalLayerEffects` pass — the one that composited all six
//! effects at once — is the CPU kernel path in [`compositor_rs_pixels::effects`]. The Swift Core Graphics
//! fallback (which ran only when Metal was unavailable, and drew a subset of the effects) has no
//! equivalent here: the kernels are always available, so the surface keeps only the full-effects path.

use std::collections::HashMap;
use std::sync::Arc;

use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_effects::LayerEffects;
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::raster::BrushPatch;
use compositor_rs_core::{Gray8Image, Id, Rgba8Image, SharedGray};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_rs_pixels::effects::LayerEffectsRenderer;
use compositor_rs_pixels::raster::{rect_applying, Raster};

use crate::downsample_cache::image_identity;

/// `BrushRaster.draw` into a color target: nearest sampling and Core Graphics' `.copy` blend mode, so
/// even transparent pixels of `image` replace what was under them.
fn draw_copy(canvas: &mut Canvas, image: &Rgba8Image, rect: Rect) {
    canvas.save();
    canvas.set_interpolation_quality(InterpolationQuality::None);
    canvas.clear(rect);
    canvas.draw_image(image, rect);
    canvas.restore();
}

/// A mask being painted: its stroke's tiles, where they land in this surface's grid, and the mask as the
/// stroke leaves it over a region of that grid (white shows, top row first).
///
/// The coverage closure owns or `Arc`-shares everything it reads (the stroke's tiles are already shared
/// rasters), so the surface can hold it across frames.
pub struct MaskStroke {
    pub patches: Vec<BrushPatch>,
    pub to_grid: AffineTransform,
    pub coverage: Arc<dyn Fn(Rect) -> Option<SharedGray>>,
}

/// One layer's effects at full resolution, recomposited only where the paint changed.
pub struct LayerEffectsSurface {
    pub layer_id: Id,
    /// The grid the stroke paints in, in layer pixels, and where the layer's own pixels sit inside it.
    pub grid: Size,
    pub source_rect: Rect,
    /// The room the effects need around the pixels.
    pub margin: f64,
    effects: LayerEffects,
    context: Canvas,
    /// Which tile images have already been taken in, so only new paint is redone.
    taken: HashMap<(i64, i64), (u8, usize)>,
    image: Option<Rgba8Image>,
    /// Where the surface was last drawn, so what it holds can be handed on when the stroke ends.
    pub placement: Option<LayerTransform>,
    mask_stroke: Option<MaskStroke>,
}

impl LayerEffectsSurface {
    /// `LayerEffectsSurface.init?(layerID:effects:grid:sourceRect:)`.
    pub fn new(layer_id: Id, effects: LayerEffects, grid: Size, source_rect: Rect) -> Option<Self> {
        let margin = LayerEffectsRenderer::margin(&effects);
        let width = grid.width + margin * 2.0;
        let height = grid.height + margin * 2.0;
        if !(width > 0.0 && height > 0.0) {
            return None;
        }
        let width = width as usize;
        let height = height as usize;
        if width == 0 || height == 0 || width * height > 80_000_000 {
            return None;
        }
        Some(LayerEffectsSurface {
            layer_id,
            effects,
            grid,
            source_rect,
            margin,
            context: Canvas::new_rgba(width, height),
            taken: HashMap::new(),
            image: None,
            placement: None,
            mask_stroke: None,
        })
    }

    /// Whether this surface still fits the stroke and settings it was made for.
    pub fn matches(&self, layer_id: Id, effects: &LayerEffects, grid: Size, source_rect: Rect) -> bool {
        self.layer_id == layer_id
            && self.effects == *effects
            && self.grid == grid
            && self.source_rect == source_rect
    }

    /// The surface as it stands, after the last update.
    pub fn image(&self) -> Option<&Rgba8Image> {
        self.image.as_ref()
    }

    /// How far a pixel can reach into its surroundings: everything within this of a change may need redoing.
    fn reach(&self) -> f64 {
        let mut reach: f64 = 1.0;
        if let Some(stroke) = &self.effects.stroke {
            if stroke.is_enabled() {
                reach = reach.max(stroke.size + 2.0);
            }
        }
        if let Some(shadow) = &self.effects.shadow {
            if shadow.is_enabled() {
                reach = reach.max(shadow.distance + shadow.blur * 3.0 + 2.0);
            }
        }
        if let Some(glow) = &self.effects.outer_glow {
            if glow.is_enabled() {
                reach = reach.max(glow.size * 3.0 + 2.0);
            }
        }
        if let Some(glow) = &self.effects.inner_glow {
            if glow.is_enabled() {
                reach = reach.max(glow.size * 3.0 + 2.0);
            }
        }
        reach.ceil()
    }

    /// Brings the surface up to date: everything on the first pass, and after that only where the paint
    /// changed. `base` is the layer's committed pixels and `patches` the stroke's tiles as they stand.
    /// Painting the layer's mask instead, `mask_stroke` holds the mask as it was and the stroke's tiles
    /// of it; the pixels themselves don't change.
    pub fn update(
        &mut self,
        base: Option<&Rgba8Image>,
        patches: &[BrushPatch],
        mask: Option<&Gray8Image>,
        mask_stroke: Option<MaskStroke>,
    ) {
        self.mask_stroke = mask_stroke;
        let mut dirty: Option<Rect> = None;
        let mut seen: HashMap<(i64, i64), (u8, usize)> = HashMap::new();
        let stroke_patches: &[BrushPatch] = self
            .mask_stroke
            .as_ref()
            .map(|stroke| stroke.patches.as_slice())
            .unwrap_or(patches);
        for patch in stroke_patches {
            let key = (patch.rect.min_x() as i64, patch.rect.min_y() as i64);
            let identity = image_identity(&patch.image);
            seen.insert(key, identity);
            if self.taken.get(&key) == Some(&identity) {
                continue;
            }
            // A mask on its own placement is painted in its own grid; what it touched is found in the layer's.
            let rect = match &self.mask_stroke {
                Some(stroke) => rect_applying(patch.rect, stroke.to_grid).inset_by(-1.0, -1.0),
                None => patch.rect,
            };
            dirty = Some(match dirty {
                Some(current) => current.union(rect),
                None => rect,
            });
        }
        let first = self.image.is_none();
        self.taken = seen;
        let region = if first {
            Some(Rect::from_origin_size(Point::ZERO, self.grid).inset_by(-self.margin, -self.margin))
        } else {
            dirty
        };
        let Some(region) = region else { return };
        self.compose(region.integral(), base, patches, mask);
        self.image = Some(self.context.snapshot());
    }

    /// Redraws one region of the surface: the effects there, then the pixels over them.
    fn compose(
        &mut self,
        region: Rect,
        base: Option<&Rgba8Image>,
        patches: &[BrushPatch],
        mask: Option<&Gray8Image>,
    ) {
        let bounds = Rect::from_origin_size(Point::ZERO, self.grid).inset_by(-self.margin, -self.margin);
        let inner = region
            .inset_by(-self.margin, -self.margin)
            .integral()
            .intersection(bounds);
        if inner.is_null() || !(inner.width() >= 1.0 && inner.height() >= 1.0) {
            return;
        }
        // Everything that can reach into `inner` has to be looked at.
        let outer = inner.inset_by(-self.reach(), -self.reach()).integral();
        let Some(pixels) = self.window(outer, base, patches, mask) else {
            return;
        };
        // In the surface's own coordinates, the grid starts at the margin.
        let placed = |rect: Rect| rect.offset_by(self.margin, self.margin);
        // The effects in one pass — the Swift Metal path, here the CPU kernels: the outline's reach and
        // the shadow's blur are what cost.
        match compositor_rs_pixels::effects::render_effects(&pixels, &self.effects) {
            Ok(built) => {
                self.context.save();
                self.context.clip_rect(placed(inner));
                self.context.clear(placed(inner));
                Raster::draw(&PixelImage::Rgba(Arc::new(built)), placed(outer), false, &mut self.context);
                self.context.restore();
            }
            Err(_) => {
                // The effects can't be made (an invalid or oversized settings record): the stroke's own
                // pixels are still drawn, so painting never blanks out.
                self.context.save();
                self.context.clip_rect(placed(inner));
                self.context.clear(placed(inner));
                Raster::draw(
                    &PixelImage::Rgba(Arc::new(pixels)),
                    placed(outer),
                    false,
                    &mut self.context,
                );
                self.context.restore();
            }
        }
    }

    /// The layer as the stroke has it, over one region: its committed pixels, the tiles painted since,
    /// and its mask.
    fn window(
        &self,
        region: Rect,
        base: Option<&Rgba8Image>,
        patches: &[BrushPatch],
        mask: Option<&Gray8Image>,
    ) -> Option<Rgba8Image> {
        if !(region.width() >= 1.0 && region.height() >= 1.0) {
            return None;
        }
        let mut window = Canvas::new_rgba(region.width() as usize, region.height() as usize);
        window.translate(-region.min_x(), -region.min_y());
        if let Some(mask_stroke) = &self.mask_stroke {
            let live = (mask_stroke.coverage)(region)?;
            // Clipped the way the raster draws, so the mask's top row lands on the region's top row.
            window.clip_to_image(&live, region);
            if let Some(base) = base {
                draw_copy(&mut window, base, self.source_rect);
            }
            return Some(window.snapshot());
        }
        let mut draw_pixels = |window: &mut Canvas| {
            if let Some(base) = base {
                draw_copy(window, base, self.source_rect);
            }
            for patch in patches {
                if patch.rect.intersects(region) {
                    Raster::draw(&patch.image, patch.rect, false, window);
                }
            }
        };
        if let Some(mask) = mask {
            // The layer's own pixels are shown through its mask; paint laid down past them is not masked at all.
            // Clipped the way the raster draws, so the mask's top row lands on the layer's top row.
            window.save();
            window.clip_to_image(mask, self.source_rect);
            draw_pixels(&mut window);
            window.restore();
            window.save();
            let mut outside = compositor_rs_core::path::Path::empty();
            outside.add_rect(region);
            outside.add_rect(self.source_rect);
            window.clip_path(&outside, compositor_rs_core::path::FillRule::EvenOdd);
            draw_pixels(&mut window);
            window.restore();
        } else {
            draw_pixels(&mut window);
        }
        Some(window.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::layer_effects::StrokeEffect;

    fn base_image(width: usize, height: usize, pixel: [u8; 4]) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, pixel);
            }
        }
        image
    }

    fn patch(rect: Rect, pixel: [u8; 4]) -> BrushPatch {
        let mut image = Rgba8Image::new(rect.width() as usize, rect.height() as usize);
        for y in 0..image.height() {
            for x in 0..image.width() {
                image.set(x, y, pixel);
            }
        }
        BrushPatch::new(rect, PixelImage::Rgba(Arc::new(image)))
    }

    /// The first update lays the whole surface out: the grid starts at the margin, and the layer's
    /// pixels land inside it.
    #[test]
    fn the_first_update_composes_the_grid_at_the_margin() {
        let mut surface = LayerEffectsSurface::new(
            compositor_rs_core::new_id(),
            LayerEffects::default(),
            Size::new(16.0, 16.0),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        )
        .expect("a 16×16 surface fits");
        assert!(surface.image().is_none());
        assert_eq!(surface.margin, 2.0, "an empty effects record still keeps the 2px guard margin");

        let base = base_image(16, 16, [255, 0, 0, 255]);
        surface.update(Some(&base), &[], None, None);
        let image = surface.image().expect("the update produced a surface");
        assert_eq!(image.width(), 20);
        assert_eq!(image.height(), 20);
        assert_eq!(image.get(2, 2), [255, 0, 0, 255]);
        // The margin stays transparent.
        assert_eq!(image.get(0, 0), [0, 0, 0, 0]);
        assert_eq!(image.get(19, 19), [0, 0, 0, 0]);
    }

    /// A second update recomposites only the patch's region; earlier pixels survive.
    #[test]
    fn later_updates_keep_the_surface_and_add_new_paint() {
        let mut surface = LayerEffectsSurface::new(
            compositor_rs_core::new_id(),
            LayerEffects::default(),
            Size::new(16.0, 16.0),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        )
        .unwrap();
        let base = base_image(16, 16, [255, 0, 0, 255]);
        surface.update(Some(&base), &[], None, None);

        let green = patch(Rect::new(5.0, 5.0, 4.0, 4.0), [0, 255, 0, 255]);
        surface.update(Some(&base), std::slice::from_ref(&green), None, None);
        let image = surface.image().unwrap();
        // The new paint is in.
        assert_eq!(image.get(7, 7), [0, 255, 0, 255]);
        // The layer's pixels outside the dirty region are still there.
        assert_eq!(image.get(2, 2), [255, 0, 0, 255]);
    }

    /// The same patch image is not redone on the next update; a new image at the same key is.
    #[test]
    fn taken_tiles_skip_unchanged_paint() {
        let mut surface = LayerEffectsSurface::new(
            compositor_rs_core::new_id(),
            LayerEffects::default(),
            Size::new(16.0, 16.0),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        )
        .unwrap();
        let base = base_image(16, 16, [255, 0, 0, 255]);
        surface.update(Some(&base), &[], None, None);

        let green = patch(Rect::new(5.0, 5.0, 4.0, 4.0), [0, 255, 0, 255]);
        surface.update(Some(&base), std::slice::from_ref(&green), None, None);
        let after_first = surface.image().unwrap().get(7, 7);
        assert_eq!(after_first, [0, 255, 0, 255]);

        // A replacement image at the same key is taken in and repainted.
        let blue = patch(Rect::new(5.0, 5.0, 4.0, 4.0), [0, 0, 255, 255]);
        surface.update(Some(&base), std::slice::from_ref(&blue), None, None);
        assert_eq!(surface.image().unwrap().get(7, 7), [0, 0, 255, 255]);
    }

    /// `matches` answers exactly the four fields the surface was made for.
    #[test]
    fn matches_compares_the_stroke_it_was_made_for() {
        let id = compositor_rs_core::new_id();
        let effects = LayerEffects::default();
        let surface = LayerEffectsSurface::new(
            id,
            effects.clone(),
            Size::new(16.0, 16.0),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        )
        .unwrap();
        assert!(surface.matches(id, &effects, Size::new(16.0, 16.0), Rect::new(0.0, 0.0, 16.0, 16.0)));
        assert!(!surface.matches(compositor_rs_core::new_id(), &effects, Size::new(16.0, 16.0), Rect::new(0.0, 0.0, 16.0, 16.0)));
        assert!(!surface.matches(id, &effects, Size::new(20.0, 16.0), Rect::new(0.0, 0.0, 16.0, 16.0)));
        assert!(!surface.matches(id, &effects, Size::new(16.0, 16.0), Rect::new(1.0, 0.0, 16.0, 16.0)));
        let mut changed = effects.clone();
        changed.stroke = Some(StrokeEffect { size: 6.0, ..Default::default() });
        assert!(!surface.matches(id, &changed, Size::new(16.0, 16.0), Rect::new(0.0, 0.0, 16.0, 16.0)));
    }

    /// A stroke's mask is applied to the layer's pixels through the coverage closure.
    #[test]
    fn a_mask_stroke_clips_the_committed_pixels() {
        let mut surface = LayerEffectsSurface::new(
            compositor_rs_core::new_id(),
            LayerEffects::default(),
            Size::new(16.0, 16.0),
            Rect::new(0.0, 0.0, 16.0, 16.0),
        )
        .unwrap();
        let base = base_image(16, 16, [255, 0, 0, 255]);
        // A mask that hides everything: white shows, black hides.
        let hidden = Arc::new(Gray8Image::uniform(16, 16, 0));
        let coverage: Arc<dyn Fn(Rect) -> Option<SharedGray>> = Arc::new(move |_| Some(hidden.clone()));
        surface.update(
            Some(&base),
            &[],
            None,
            Some(MaskStroke {
                patches: Vec::new(),
                to_grid: AffineTransform::IDENTITY,
                coverage,
            }),
        );
        let image = surface.image().unwrap();
        assert_eq!(image.get(2, 2), [0, 0, 0, 0]);

        // White shows the pixels again. A mask update only recomposites the region its new paint touched
        // (the Swift takes `region` from the patches the first time, `dirty` after), so the stroke's tile
        // stands in for the dab that would drive this pass.
        let shown = Arc::new(Gray8Image::uniform(16, 16, 255));
        let coverage: Arc<dyn Fn(Rect) -> Option<SharedGray>> = Arc::new(move |_| Some(shown.clone()));
        surface.update(
            Some(&base),
            &[],
            None,
            Some(MaskStroke {
                patches: vec![patch(Rect::new(0.0, 0.0, 16.0, 16.0), [0, 0, 0, 255])],
                to_grid: AffineTransform::IDENTITY,
                coverage,
            }),
        );
        assert_eq!(surface.image().unwrap().get(2, 2), [255, 0, 0, 255]);
    }

    /// An oversized grid is refused up front, exactly like the Swift initializer.
    #[test]
    fn an_oversized_grid_is_refused() {
        // 80,000,000 pixels overflows the cap once the margin is added.
        let refused = LayerEffectsSurface::new(
            compositor_rs_core::new_id(),
            LayerEffects::default(),
            Size::new(9000.0, 9000.0),
            Rect::new(0.0, 0.0, 9000.0, 9000.0),
        );
        assert!(refused.is_none());
    }
}
