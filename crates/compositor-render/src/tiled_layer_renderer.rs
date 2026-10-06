//! Draws a layer held as an unchanged image plus replacement tiles — a painted layer's raster
//! snapshot, or a brush stroke in progress — so it looks the same as those pixels drawn as one
//! image by [`LayerRenderer`].
//!
//! Drawing each tile on its own resamples it without its neighbours (seams) and can't use the sharp
//! halvings, and the live stroke used to switch the whole layer to Nearest, so pixels shifted when
//! painting started and again when it ended. Instead the tiled areas are rebuilt as pieces: squares
//! of the layer grid, aligned to every halving, recomposed at full resolution with a margin of
//! surrounding pixels, reduced with the same halvings as the image, and drawn only inside the
//! square. The margin covers everything the halvings and the last resample can reach, so a piece's
//! pixels match the whole image's; the unchanged image fills the rest. Clips are hard-edged so the
//! parts meet without gaps or overlap.
//!
//! Port of `Rendering/TiledLayerRenderer.swift`. The drawing space is top-left, y down: the Swift
//! code's Core Graphics rects are mirrored in y (`bounds.maxY - …`) and become their y-down
//! equivalents here.

use compositor_core::blend::LayerBlendMode;
use compositor_core::geom::{AffineTransform, Point, Rect};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_transform::{LayerSampling, LayerTransform};
use compositor_core::path::{FillRule, Path};
use compositor_core::raster::{BrushPatch, RasterSnapshot};
use compositor_pixels::canvas::Canvas;
use compositor_pixels::raster::Raster;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use crate::downsample_cache::DownsampleCache;
use crate::layer_renderer::LayerRenderer;

/// `TiledLayerRenderer`.
pub enum TiledLayerRenderer {}

/// A piece of a tiled layer: the grid pixels it draws, the grid pixels its image holds (the
/// interior and a margin), and that image.
#[derive(Clone, Debug)]
pub struct Piece {
    /// Grid pixels this piece draws.
    pub interior: Rect,
    /// Grid pixels its image holds: the interior and a margin.
    pub region: Rect,
    pub image: PixelImage,
}

impl Piece {
    pub fn offset_by(&self, offset: Point) -> Piece {
        Piece {
            interior: self.interior.offset_by(offset.x, offset.y),
            region: self.region.offset_by(offset.x, offset.y),
            image: self.image.clone(),
        }
    }
}

/// How one layer's grid maps into the (already transformed) canvas.
pub struct Frame {
    pub bounds: Rect,
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub level: usize,
    pub device: f64,
    pub sampling: LayerSampling,
    /// Grid pixels the canvas's clip can show.
    pub visible: Rect,
}

impl Frame {
    pub fn mapped(&self, rect: Rect) -> Rect {
        Rect::new(
            self.bounds.min_x() + rect.min_x() / self.pixel_width * self.bounds.width(),
            self.bounds.min_y() + rect.min_y() / self.pixel_height * self.bounds.height(),
            rect.width() / self.pixel_width * self.bounds.width(),
            rect.height() / self.pixel_height * self.bounds.height(),
        )
    }
}

impl TiledLayerRenderer {
    /// Grid pixels beyond a change that its reduced, resampled pixels can reach, with room to spare.
    pub fn support(level: usize) -> f64 {
        if level == 0 {
            8.0
        } else {
            (16usize << level) as f64
        }
    }

    /// Piece squares: committed snapshots use large ones (fewer to build, once), live strokes small
    /// ones (little to rebuild per mouse move). Both are whole multiples of every halving used.
    pub const COMMITTED_CELL: f64 = 1024.0;
    pub const STROKE_CELL: f64 = 256.0;

    // MARK: Drawing

    /// A committed raster snapshot (a painted layer).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_raster(
        raster: &RasterSnapshot,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        mask: Option<&PixelImage>,
        canvas: &mut Canvas,
    ) {
        Self::with_frame(
            raster.width,
            raster.height,
            transform,
            center,
            scale,
            opacity,
            blend_mode,
            canvas,
            |frame, canvas| {
                if let Some(mask) = mask {
                    Self::clip_to_mask(
                        mask,
                        Rect::new(0.0, 0.0, raster.width as f64, raster.height as f64),
                        frame,
                        canvas,
                    );
                }
                Self::draw_committed(raster, Point::ZERO, &[], frame, canvas);
            },
        );
    }

    /// A tiled edit in progress (`patches`, in a `width` × `height` grid) over the layer's previous
    /// pixels — `image` or `raster`, sitting at `source_rect` — drawn as the finished layer will
    /// look.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_stroke(
        width: usize,
        height: usize,
        source_rect: Rect,
        patches: &[BrushPatch],
        image: Option<&PixelImage>,
        raster: Option<&RasterSnapshot>,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        mask: Option<&PixelImage>,
        canvas: &mut Canvas,
    ) {
        Self::with_frame(
            width,
            height,
            transform,
            center,
            scale,
            opacity,
            blend_mode,
            canvas,
            |frame, canvas| {
                let origin = Point::new(
                    source_rect.min_x() + raster.map(|raster| raster.alignment.x).unwrap_or(0.0),
                    source_rect.min_y() + raster.map(|raster| raster.alignment.y).unwrap_or(0.0),
                );
                let squares = Self::interiors(
                    &patches.iter().map(|patch| patch.rect).collect::<Vec<_>>(),
                    Self::support(frame.level),
                    Self::STROKE_CELL,
                    (1usize << frame.level) as f64,
                    origin,
                    Some(frame.visible),
                );
                let painted = patches
                    .iter()
                    .fold(Rect::new(0.0, 0.0, width as f64, height as f64), |result, patch| {
                        result.union(patch.rect)
                    });
                let pieces: Vec<Piece> = squares
                    .iter()
                    .filter_map(|square| {
                        Self::piece(*square, frame.level, origin, Some(painted), false, |canvas, region| {
                            if let Some(raster) = raster {
                                draw_raster_into(
                                    raster,
                                    Rect::new(
                                        source_rect.min_x(),
                                        source_rect.min_y(),
                                        raster.width as f64,
                                        raster.height as f64,
                                    ),
                                    canvas,
                                );
                            } else if let Some(image) = image {
                                Self::draw_cropped(image, source_rect, region, false, canvas);
                            }
                            for patch in patches.iter().filter(|patch| patch.rect.intersects(region)) {
                                Raster::draw(&patch.image, patch.rect, false, canvas);
                            }
                        })
                    })
                    .collect();
                canvas.save();
                if let Some(mask) = mask {
                    Self::clip_to_mask(mask, source_rect, frame, canvas);
                }
                let interiors: Vec<Rect> = pieces.iter().map(|piece| piece.interior).collect();
                Self::draw_replacing(
                    &interiors,
                    frame,
                    canvas,
                    |canvas| {
                        if let Some(raster) = raster {
                            Self::draw_committed(raster, source_rect.origin, &[], frame, canvas);
                        } else if let Some(image) = image {
                            Self::draw_base(image, source_rect, &[], frame, canvas);
                        }
                    },
                    |index, canvas| Self::draw_piece(&pieces[index], &[], frame, canvas),
                );
                canvas.restore();
                // Paint beyond the layer's old bounds is revealed, not masked.
                if mask.is_some() {
                    for piece in pieces
                        .iter()
                        .filter(|piece| !source_rect.contains_rect(piece.interior))
                    {
                        Self::draw_piece(piece, std::slice::from_ref(&source_rect), frame, canvas);
                    }
                }
            },
        );
    }

    /// Painting a layer's mask (`patches` of coverage in a `width` × `height` grid, over `old_mask`
    /// at `source_rect`): the layer's pixels — `image` or `raster`, also at `source_rect` — drawn
    /// through the mask as it will be once committed: pieces of the new mask where the stroke's
    /// tiles can show, the old mask elsewhere.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_mask_stroke(
        width: usize,
        height: usize,
        source_rect: Rect,
        patches: &[BrushPatch],
        old_mask: Option<&ImportedImage>,
        image: Option<&PixelImage>,
        raster: Option<&RasterSnapshot>,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        canvas: &mut Canvas,
    ) {
        Self::with_frame(
            width,
            height,
            transform,
            center,
            scale,
            opacity,
            blend_mode,
            canvas,
            |frame, canvas| {
                // Pieces are halved on the mask image's own grid and levels, the way the old mask is.
                let level = if frame.sampling == LayerSampling::Nearest {
                    0
                } else {
                    DownsampleCache::level_for(
                        frame.mapped(source_rect).width() * frame.device / source_rect.width().max(1.0),
                    )
                };
                let found = Self::interiors(
                    &patches.iter().map(|patch| patch.rect).collect::<Vec<_>>(),
                    Self::support(level),
                    Self::STROKE_CELL,
                    (1usize << level) as f64,
                    source_rect.origin,
                    Some(frame.visible),
                );
                let pieces: Vec<Piece> = found
                    .iter()
                    .filter_map(|interior| {
                        Self::piece(
                            *interior,
                            level,
                            source_rect.origin,
                            Some(source_rect),
                            true,
                            |canvas, region| {
                                // Beyond the old mask an edit reveals, as mask edits do.
                                canvas.set_fill_gray(1.0);
                                canvas.fill_rect(region);
                                if let Some(old) = old_mask.and_then(|mask| mask.raster.as_ref()) {
                                    draw_raster_into(
                                        old,
                                        Rect::new(
                                            source_rect.min_x(),
                                            source_rect.min_y(),
                                            old.width as f64,
                                            old.height as f64,
                                        ),
                                        canvas,
                                    );
                                } else if let Some(old) = old_mask.map(|mask| &mask.image) {
                                    Self::draw_cropped(old, source_rect, region, true, canvas);
                                }
                                for patch in patches.iter().filter(|patch| patch.rect.intersects(region)) {
                                    Raster::draw(&patch.image, patch.rect, true, canvas);
                                }
                            },
                        )
                    })
                    .collect();
                let interiors: Vec<Rect> = pieces.iter().map(|piece| piece.interior).collect();
                Self::draw_replacing(
                    &interiors,
                    frame,
                    canvas,
                    |canvas| {
                        canvas.save();
                        if let Some(old) = old_mask.map(|mask| &mask.image) {
                            Self::clip_to_mask(old, source_rect, frame, canvas);
                        }
                        Self::draw_layer(image, raster, source_rect, frame, canvas);
                        canvas.restore();
                    },
                    |index, canvas| {
                        canvas.save();
                        Self::clip_to(pieces[index].interior, &[], frame, canvas);
                        if let Some(gray) = pieces[index].image.as_gray() {
                            canvas.clip_to_image(gray, frame.mapped(pieces[index].region));
                        }
                        Self::draw_layer(image, raster, source_rect, frame, canvas);
                        canvas.restore();
                    },
                );
            },
        );
    }

    /// The layer's own pixels — a raster or an image — at `source_rect` in the frame's grid.
    fn draw_layer(
        image: Option<&PixelImage>,
        raster: Option<&RasterSnapshot>,
        source_rect: Rect,
        frame: &Frame,
        canvas: &mut Canvas,
    ) {
        if let Some(raster) = raster {
            Self::draw_committed(raster, source_rect.origin, &[], frame, canvas);
        } else if let Some(image) = image {
            Self::draw_base(image, source_rect, &[], frame, canvas);
        }
    }

    /// Draws `unchanged` everywhere but `interiors`, and `replace(i)` inside interior `i`.
    ///
    /// Core Graphics's hard clips cover every pixel they touch, so the Swift assembles the pieces
    /// apart in a transparency layer, clearing each interior before its replacement draws, and
    /// composites the lot once. Here every clip is hard and device-snapped (`clip_to`), so the
    /// interiors are disjoint and each pixel is drawn exactly once — the same pixels, without the
    /// extra layer.
    fn draw_replacing(
        interiors: &[Rect],
        frame: &Frame,
        canvas: &mut Canvas,
        unchanged: impl FnOnce(&mut Canvas),
        replace: impl Fn(usize, &mut Canvas),
    ) {
        if interiors.is_empty() {
            unchanged(canvas);
            return;
        }
        let device = affine_rect(canvas.user_space_to_device(), frame.mapped(
            interiors
                .iter()
                .fold(interiors[0], |result, interior| result.union(*interior)),
        ))
        .intersection(
            affine_rect(canvas.user_space_to_device(), user_space_clip_bounds(canvas).inset_by(-2.0, -2.0))
                .integral(),
        );
        if device.is_null() || device.is_empty() {
            unchanged(canvas);
            return;
        }
        canvas.save();
        canvas.set_should_antialias(false);
        // Everything but the interiors: the visible area, less each interior's device-snapped box.
        let outline = user_space_clip_bounds(canvas).inset_by(-64.0, -64.0);
        let holes: Vec<Rect> = interiors
            .iter()
            .map(|interior| Self::snapped(frame.mapped(*interior), canvas))
            .collect();
        Self::clip_minus(outline, &holes, canvas);
        unchanged(canvas);
        canvas.restore();
        for index in 0..interiors.len() {
            canvas.save();
            canvas.set_should_antialias(false);
            Self::clip_minus(Self::snapped(frame.mapped(interiors[index]), canvas), &[], canvas);
            replace(index, canvas);
            canvas.restore();
        }
    }

    fn with_frame(
        pixel_width: usize,
        pixel_height: usize,
        transform: &LayerTransform,
        center: Point,
        scale: f64,
        opacity: f64,
        blend_mode: LayerBlendMode,
        canvas: &mut Canvas,
        body: impl FnOnce(&Frame, &mut Canvas),
    ) {
        let width = transform.size.width * scale;
        let height = transform.size.height * scale;
        if !(width > 0.0) || !(height > 0.0) || pixel_width == 0 || pixel_height == 0 {
            return;
        }
        let device = LayerRenderer::device_scale(canvas);
        let level = if transform.sampling == LayerSampling::Nearest {
            0
        } else {
            DownsampleCache::level_for(width * device / pixel_width as f64)
        };
        canvas.save();
        canvas.set_alpha(opacity);
        canvas.set_blend_mode(blend_mode);
        canvas.set_interpolation_quality(LayerRenderer::interpolation(
            transform.sampling,
            width * device / pixel_width as f64 * (1usize << level) as f64,
            transform.radians() == 0.0,
        ));
        // One placement transform rather than three calls, so the placement doesn't depend on the
        // order the canvas composes CTM calls in (see `LayerRenderer`).
        canvas.concatenate(
            AffineTransform::translation(center.x, center.y)
                .rotated_by(transform.radians())
                .scaled_by(
                    if transform.flip_x { -1.0 } else { 1.0 },
                    if transform.flip_y { -1.0 } else { 1.0 },
                ),
        );
        let bounds = Rect::new(-width / 2.0, -height / 2.0, width, height);
        let clip = user_space_clip_bounds(canvas);
        let sx = pixel_width as f64 / width;
        let sy = pixel_height as f64 / height;
        let visible = Rect::new(
            (clip.min_x() - bounds.min_x()) * sx,
            (clip.min_y() - bounds.min_y()) * sy,
            clip.width() * sx,
            clip.height() * sy,
        );
        let frame = Frame {
            bounds,
            pixel_width: pixel_width as f64,
            pixel_height: pixel_height as f64,
            level,
            device,
            sampling: transform.sampling,
            visible,
        };
        body(&frame, canvas);
        canvas.restore();
    }

    /// A committed raster placed at `offset` in the frame's grid, leaving `holes` for pieces drawn
    /// over it.
    fn draw_committed(
        raster: &RasterSnapshot,
        offset: Point,
        holes: &[Rect],
        frame: &Frame,
        canvas: &mut Canvas,
    ) {
        let pieces: Vec<Piece> = TiledPieceCache::shared()
            .pieces(raster, frame.level)
            .into_iter()
            .map(|piece| piece.offset_by(offset))
            .collect();
        if let Some(base) = &raster.base {
            let mut cut: Vec<Rect> = pieces.iter().map(|piece| piece.interior).collect();
            cut.extend_from_slice(holes);
            Self::draw_base(
                base,
                raster.base_rect.offset_by(offset.x, offset.y),
                &cut,
                frame,
                canvas,
            );
        }
        let visible = frame.visible.inset_by(-2.0, -2.0);
        for piece in pieces.iter().filter(|piece| piece.interior.intersects(visible)) {
            Self::draw_piece(piece, holes, frame, canvas);
        }
    }

    /// The unchanged image (placed at `rect`) reduced by the frame's halvings, everywhere but
    /// `holes`.
    fn draw_base(image: &PixelImage, rect: Rect, holes: &[Rect], frame: &Frame, canvas: &mut Canvas) {
        let (reduced, level) = DownsampleCache::shared().image_at_level(image, frame.level);
        let step = (1usize << level) as f64;
        let covered = Rect::new(
            rect.min_x(),
            rect.min_y(),
            reduced.width() as f64 * step * rect.width() / image.width().max(1) as f64,
            reduced.height() as f64 * step * rect.height() / image.height().max(1) as f64,
        );
        canvas.save();
        // The image's own edges antialias as usual; only the pieces' squares are cut out.
        Self::clip_to(covered.inset_by(-step - 8.0, -step - 8.0), holes, frame, canvas);
        canvas.set_should_antialias(frame.sampling != LayerSampling::Nearest);
        // `context.draw` in the Swift: source-over at the frame's interpolation quality, not
        // `BrushRaster.draw`'s copy at Nearest.
        LayerRenderer::draw_pixels(&reduced, frame.mapped(covered), canvas);
        canvas.restore();
    }

    /// Draws one piece, cut to its interior and leaving `holes`.
    fn draw_piece(piece: &Piece, holes: &[Rect], frame: &Frame, canvas: &mut Canvas) {
        canvas.save();
        Self::clip_to(piece.interior, holes, frame, canvas);
        canvas.set_should_antialias(frame.sampling != LayerSampling::Nearest);
        // Source-over at the frame's interpolation quality, as the Swift's `context.draw`.
        LayerRenderer::draw_pixels(&piece.image, frame.mapped(piece.region), canvas);
        canvas.restore();
    }

    /// Clips to `area` less `holes` (grid pixels) with hard edges, so neighbouring draws meet
    /// exactly.
    fn clip_to(area: Rect, holes: &[Rect], frame: &Frame, canvas: &mut Canvas) {
        canvas.set_should_antialias(false);
        let holes: Vec<Rect> = holes
            .iter()
            .filter(|hole| hole.intersects(area))
            .map(|hole| Self::snapped(frame.mapped(*hole), canvas))
            .collect();
        Self::clip_minus(Self::snapped(frame.mapped(area), canvas), &holes, canvas);
    }

    /// Clips to `area` less `holes` (user space, already device-snapped) with hard edges.
    fn clip_minus(area: Rect, holes: &[Rect], canvas: &mut Canvas) {
        let pieces = subtract_rects(area, holes);
        if pieces.is_empty() {
            canvas.clip_to_zero();
            return;
        }
        let mut path = Path::empty();
        for piece in pieces {
            path.add_rect(piece);
        }
        canvas.clip_path(&path, FillRule::Winding);
    }

    /// A clip edge on a fraction of a screen pixel leaves that pixel to be rounded one way here and
    /// the other way in the neighbouring piece, which shows as a hairline across translucent
    /// pixels. Rounding each edge to whole screen pixels first makes two pieces that share an edge
    /// round it the same way and meet exactly. Skipped for a rotated layer, whose pieces don't lie
    /// along the screen's pixels at all.
    fn snapped(rect: Rect, canvas: &Canvas) -> Rect {
        let to_device = canvas.user_space_to_device();
        if to_device.b.abs() >= 1e-9
            || to_device.c.abs() >= 1e-9
            || to_device.a == 0.0
            || to_device.d == 0.0
        {
            return rect;
        }
        let device = affine_rect(to_device, rect);
        let snapped = Rect::new(
            device.min_x().round(),
            device.min_y().round(),
            (device.max_x().round() - device.min_x().round()).max(0.0),
            (device.max_y().round() - device.min_y().round()).max(0.0),
        );
        affine_rect(to_device.inverted(), snapped).standardized()
    }

    fn clip_to_mask(mask: &PixelImage, rect: Rect, frame: &Frame, canvas: &mut Canvas) {
        let target = frame.mapped(rect);
        let reduced = LayerRenderer::reduced(mask, target.width(), frame.device, frame.sampling);
        if let Some(gray) = reduced.image.as_gray() {
            canvas.clip_to_image(gray, LayerRenderer::coverage(&reduced, target));
        }
    }

    // MARK: Pieces

    /// A piece drawing `interior`: `compose` draws full-resolution grid pixels (into a canvas whose
    /// origin is the grid's) over the interior grown by the support, which is then reduced to
    /// `level`.
    /// `bounds` (grid pixels) is everything that holds pixels — the layer's grid, plus anything
    /// painted past it. The margin is kept inside it: past that edge there is nothing to compose,
    /// and resampling a piece whose margin is empty pulls that emptiness into the layer's edge,
    /// which the whole image's own draw never does.
    pub fn piece(
        interior: Rect,
        level: usize,
        origin: Point,
        bounds: Option<Rect>,
        mask: bool,
        compose: impl FnOnce(&mut Canvas, Rect),
    ) -> Option<Piece> {
        let margin = Self::support(level);
        let step = (1usize << level) as f64;
        let mut region = Self::aligned(interior.inset_by(-margin, -margin), step, origin);
        if let Some(bounds) = bounds {
            let limit = Self::aligned(bounds, step, origin);
            region = region.intersection(limit);
            if region.is_null() || region.is_empty() {
                return None;
            }
        }
        let width = region.width() as usize;
        let height = region.height() as usize;
        if width == 0 || height == 0 || width * height > 64_000_000 {
            return None;
        }
        let mut canvas = if mask {
            Canvas::new_gray(width, height)
        } else {
            Canvas::new_rgba(width, height)
        };
        canvas.save();
        canvas.translate(-region.min_x(), -region.min_y());
        compose(&mut canvas, region);
        canvas.restore();
        let mut image = if mask {
            PixelImage::Gray(Arc::new(canvas.into_gray()))
        } else {
            PixelImage::Rgba(Arc::new(canvas.into_rgba()))
        };
        for _ in 0..level {
            image = DownsampleCache::halve(&image)?;
        }
        let kept = match bounds {
            None => interior,
            Some(_) => interior.intersection(region),
        };
        if kept.is_null() || kept.is_empty() {
            return None;
        }
        Some(Piece {
            interior: kept,
            region,
            image,
        })
    }

    /// Piece interiors: in each `size` square (from `origin`), the part within `margin` of any of
    /// `rects`, grown to the `step` grid. Pieces stay disjoint and go only where changes can show —
    /// clear of the layer's own edges unless something was painted near them. Limited to what
    /// `visible` shows.
    pub fn interiors(
        rects: &[Rect],
        margin: f64,
        size: f64,
        step: f64,
        origin: Point,
        visible: Option<Rect>,
    ) -> Vec<Rect> {
        let mut parts: BTreeMap<[i32; 2], Rect> = BTreeMap::new();
        for rect in rects {
            let grown = rect.inset_by(-margin, -margin);
            if let Some(visible) = visible {
                if !grown.intersects(visible) {
                    continue;
                }
            }
            let x0 = ((grown.min_x() - origin.x) / size).floor() as i32;
            let x1 = ((grown.max_x() - origin.x) / size).ceil() as i32;
            let y0 = ((grown.min_y() - origin.y) / size).floor() as i32;
            let y1 = ((grown.max_y() - origin.y) / size).ceil() as i32;
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            for y in y0..y1 {
                for x in x0..x1 {
                    let square = Rect::new(
                        origin.x + x as f64 * size,
                        origin.y + y as f64 * size,
                        size,
                        size,
                    );
                    let part = grown.intersection(square);
                    if part.is_null() || part.is_empty() {
                        continue;
                    }
                    let key = [x, y];
                    parts.insert(key, parts.get(&key).map(|known| known.union(part)).unwrap_or(part));
                }
            }
        }
        // Squares sit on the step grid, so growing a part to it never leaves its square.
        parts
            .into_values()
            .map(|part| Self::aligned(part, step, origin))
            .filter(|interior| visible.map(|visible| interior.intersects(visible)).unwrap_or(true))
            .collect()
    }

    /// `rect` grown outward to whole multiples of `step` measured from `origin`.
    pub fn aligned(rect: Rect, step: f64, origin: Point) -> Rect {
        let min_x = origin.x + ((rect.min_x() - origin.x) / step).floor() * step;
        let min_y = origin.y + ((rect.min_y() - origin.y) / step).floor() * step;
        let max_x = origin.x + ((rect.max_x() - origin.x) / step).ceil() * step;
        let max_y = origin.y + ((rect.max_y() - origin.y) / step).ceil() * step;
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// The part of `image` (placed at `rect`) inside `region`: cropped when it is 1:1 with the grid,
    /// otherwise (a solid 1 × 1 mask, say) drawn stretched over `rect`.
    fn draw_cropped(image: &PixelImage, rect: Rect, region: Rect, mask: bool, canvas: &mut Canvas) {
        if image.width() as f64 != rect.width() || image.height() as f64 != rect.height() {
            Raster::draw(image, rect, mask, canvas);
            return;
        }
        let local = region
            .offset_by(-rect.min_x(), -rect.min_y())
            .intersection(Rect::new(
                0.0,
                0.0,
                image.width() as f64,
                image.height() as f64,
            ))
            .integral();
        if local.is_null() || local.is_empty() {
            return;
        }
        let Some(crop) = image.cropped(local) else { return };
        Raster::draw(&crop, local.offset_by(rect.min_x(), rect.min_y()), mask, canvas);
    }
}

/// `RasterSnapshot.draw(in:context:)`: the raster's grid stretched over `rect`, its base and every
/// patch blitted with `BrushRaster.draw`'s copy semantics, a mask's background filled first. Built
/// from [`Raster::draw`] calls, as the Swift is, so the display list needs no raw target access.
fn draw_raster_into(raster: &RasterSnapshot, rect: Rect, canvas: &mut Canvas) {
    if rect.is_null() || rect.is_empty() {
        return;
    }
    canvas.save();
    canvas.set_should_antialias(false);
    canvas.clip_rect(rect);
    if raster.is_mask {
        // Past its original extent a grown mask is its background: white reveals, black hides.
        canvas.set_fill_gray(raster.fill);
        canvas.fill_rect(rect);
    }
    let sx = rect.width() / raster.width as f64;
    let sy = rect.height() / raster.height as f64;
    let mapped = |source: Rect| {
        Rect::new(
            rect.min_x() + source.min_x() * sx,
            rect.min_y() + source.min_y() * sy,
            source.width() * sx,
            source.height() * sy,
        )
    };
    if let Some(base) = &raster.base {
        Raster::draw(base, mapped(raster.base_rect), raster.is_mask, canvas);
    }
    for patch in &raster.patches {
        Raster::draw(&patch.image, mapped(patch.rect), raster.is_mask, canvas);
    }
    canvas.restore();
}

/// The bounding box of `rect`'s four corners under `transform`.
fn affine_rect(transform: AffineTransform, rect: Rect) -> Rect {
    let corners = rect.corners().map(|point| transform.applying(point));
    let min_x = corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min);
    let max_x = corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max);
    let min_y = corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min);
    let max_y = corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// `CGContext.boundingBoxOfClipPath` in user space: the canvas's device-space clip mapped back
/// through the CTM.
fn user_space_clip_bounds(canvas: &Canvas) -> Rect {
    affine_rect(canvas.user_space_to_device().inverted(), canvas.clip_bounds()).standardized()
}

/// `rect` less each of `holes`, as disjoint rectangles (Core Graphics' `CGPath.subtracting`).
fn subtract_rects(rect: Rect, holes: &[Rect]) -> Vec<Rect> {
    let mut pieces = vec![rect];
    for hole in holes {
        if hole.is_null() || hole.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(pieces.len());
        for piece in pieces {
            let overlap = piece.intersection(*hole);
            if overlap.is_null() || overlap.is_empty() {
                next.push(piece);
                continue;
            }
            let splits = [
                Rect::new(
                    piece.min_x(),
                    piece.min_y(),
                    piece.width(),
                    overlap.min_y() - piece.min_y(),
                ),
                Rect::new(
                    piece.min_x(),
                    overlap.max_y(),
                    piece.width(),
                    piece.max_y() - overlap.max_y(),
                ),
                Rect::new(
                    piece.min_x(),
                    overlap.min_y(),
                    overlap.min_x() - piece.min_x(),
                    overlap.height(),
                ),
                Rect::new(
                    overlap.max_x(),
                    overlap.min_y(),
                    piece.max_x() - overlap.max_x(),
                    overlap.height(),
                ),
            ];
            for split in splits {
                if split.width() > 0.0 && split.height() > 0.0 {
                    next.push(split);
                }
            }
        }
        pieces = next;
    }
    pieces
}

/// Committed rasters' pieces, built once per snapshot and level (snapshots never change); the least
/// recently used are dropped beyond a pixel budget.
pub struct TiledPieceCache {
    entries: Mutex<FxHashMap<(Signature, usize), Entry>>,
    clock: Mutex<u64>,
}

/// What identifies a snapshot: its base image and every patch, by pointer where the pixels are
/// shared (the Rust rasters are values, and their images are `Arc`s).
#[derive(Clone, PartialEq, Eq, Hash)]
struct Signature {
    base: usize,
    base_rect: (u64, u64, u64, u64),
    width: usize,
    height: usize,
    is_mask: bool,
    fill: u64,
    alignment: (u64, u64),
    patches: Vec<((u64, u64, u64, u64), usize)>,
}

struct Entry {
    pieces: Vec<Piece>,
    last_use: u64,
    pixels: usize,
}

impl TiledPieceCache {
    pub const PIXEL_BUDGET: usize = 150_000_000;

    pub fn shared() -> &'static TiledPieceCache {
        static CACHE: LazyLock<TiledPieceCache> = LazyLock::new(|| TiledPieceCache {
            entries: Mutex::new(FxHashMap::default()),
            clock: Mutex::new(0),
        });
        &CACHE
    }

    pub fn pieces(&self, raster: &RasterSnapshot, level: usize) -> Vec<Piece> {
        let key = (signature(raster), level);
        let mut clock = self.clock.lock();
        *clock += 1;
        let now = *clock;
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.get_mut(&key) {
                entry.last_use = now;
                return entry.pieces.clone();
            }
        }
        let origin = raster.alignment;
        let full = Rect::new(0.0, 0.0, raster.width as f64, raster.height as f64);
        let squares = TiledLayerRenderer::interiors(
            &raster.patches.iter().map(|patch| patch.rect).collect::<Vec<_>>(),
            TiledLayerRenderer::support(level),
            TiledLayerRenderer::COMMITTED_CELL,
            (1usize << level) as f64,
            origin,
            None,
        );
        let pieces: Vec<Piece> = squares
            .iter()
            .filter_map(|square| {
                TiledLayerRenderer::piece(*square, level, origin, Some(full), raster.is_mask, |canvas, _| {
                    draw_raster_into(raster, full, canvas);
                })
            })
            .collect();
        let pixels: usize = pieces
            .iter()
            .map(|piece| piece.image.width() * piece.image.height())
            .sum();
        let mut entries = self.entries.lock();
        entries.insert(
            key.clone(),
            Entry {
                pieces: pieces.clone(),
                last_use: now,
                pixels,
            },
        );
        let mut total: usize = entries.values().map(|entry| entry.pixels).sum();
        while total > Self::PIXEL_BUDGET {
            let oldest = entries
                .iter()
                .filter(|(candidate, _)| **candidate != key)
                .min_by_key(|(_, entry)| entry.last_use)
                .map(|(candidate, entry)| (candidate.clone(), entry.pixels));
            let Some((oldest_key, oldest_pixels)) = oldest else {
                break;
            };
            entries.remove(&oldest_key);
            total -= oldest_pixels;
        }
        pieces
    }
}

/// What identifies a snapshot for the piece cache.
fn signature(raster: &RasterSnapshot) -> Signature {
    let (base, _) = match &raster.base {
        Some(PixelImage::Rgba(image)) => (Arc::as_ptr(image) as usize, ()),
        Some(PixelImage::Gray(image)) => (Arc::as_ptr(image) as usize, ()),
        None => (0, ()),
    };
    let rect = |rect: Rect| {
        (
            rect.origin.x.to_bits(),
            rect.origin.y.to_bits(),
            rect.size.width.to_bits(),
            rect.size.height.to_bits(),
        )
    };
    let image = |image: &PixelImage| match image {
        PixelImage::Rgba(image) => Arc::as_ptr(image) as usize,
        PixelImage::Gray(image) => Arc::as_ptr(image) as usize,
    };
    Signature {
        base,
        base_rect: rect(raster.base_rect),
        width: raster.width,
        height: raster.height,
        is_mask: raster.is_mask,
        fill: raster.fill.to_bits(),
        alignment: (raster.alignment.x.to_bits(), raster.alignment.y.to_bits()),
        patches: raster
            .patches
            .iter()
            .map(|patch| (rect(patch.rect), image(&patch.image)))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::geom::Size;
    use compositor_core::{Gray8Image, Rgba8Image};

    /// Detailed, deterministic pixels, from the LCG the Swift tests build their fixtures with.
    fn noise(width: usize, height: usize, seed: u32, alpha: u8) -> PixelImage {
        let mut image = Rgba8Image::new(width, height);
        let mut state = seed;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let channel = |shift: u32| ((state >> shift) as u8 as u32 * alpha as u32 / 255) as u8;
                image.set(
                    x,
                    y,
                    [channel(24), channel(16), channel(8), alpha],
                );
            }
        }
        PixelImage::Rgba(Arc::new(image))
    }

    fn gray_noise(width: usize, height: usize, seed: u32) -> PixelImage {
        let mut image = Gray8Image::new(width, height);
        let mut state = seed;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                image.set(x, y, (state >> 24) as u8);
            }
        }
        PixelImage::Gray(Arc::new(image))
    }

    /// The base pixels with the patches drawn over them, as one image.
    fn composite(base: &PixelImage, patches: &[BrushPatch]) -> PixelImage {
        let (width, height) = (base.width(), base.height());
        RasterSnapshot::new(
            width,
            height,
            Some(base.clone()),
            Rect::new(0.0, 0.0, width as f64, height as f64),
            patches.to_vec(),
            false,
            None,
        )
        .materialize()
    }

    /// The same for a mask's coverage.
    fn mask_composite(base: &PixelImage, patches: &[BrushPatch]) -> PixelImage {
        let (width, height) = (base.width(), base.height());
        RasterSnapshot::new(
            width,
            height,
            Some(base.clone()),
            Rect::new(0.0, 0.0, width as f64, height as f64),
            patches.to_vec(),
            true,
            None,
        )
        .materialize()
    }

    fn render(side: usize, draw: impl FnOnce(&mut Canvas)) -> Rgba8Image {
        let mut canvas = Canvas::new_rgba(side, side);
        draw(&mut canvas);
        canvas.into_rgba()
    }

    /// Largest channel difference over pixels fully inside the layer, judged from `reference`
    /// (default `expected`) — the layer's own outline may antialias a little differently.
    fn largest_difference(
        expected: &Rgba8Image,
        actual: &Rgba8Image,
        side: usize,
        inside: Option<&Rgba8Image>,
    ) -> i32 {
        let outline = inside.unwrap_or(expected);
        let mut largest = 0;
        for y in 1..side - 1 {
            for x in 1..side - 1 {
                let mut opaque = true;
                for dy in 0..3 {
                    for dx in 0..3 {
                        if outline.get(x + dx - 1, y + dy - 1)[3] < 255 {
                            opaque = false;
                        }
                    }
                }
                if !opaque {
                    continue;
                }
                let (expected, actual) = (expected.get(x, y), actual.get(x, y));
                for channel in 0..4 {
                    largest = largest.max((expected[channel] as i32 - actual[channel] as i32).abs());
                }
            }
        }
        largest
    }

    fn image_identity(image: &PixelImage) -> usize {
        match image {
            PixelImage::Rgba(image) => Arc::as_ptr(image) as usize,
            PixelImage::Gray(image) => Arc::as_ptr(image) as usize,
        }
    }

    fn rgba(width: usize, height: usize, color: [u8; 4]) -> PixelImage {
        let mut image = compositor_core::Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, color);
            }
        }
        PixelImage::Rgba(Arc::new(image))
    }

    fn transform(width: f64, height: f64) -> LayerTransform {
        LayerTransform {
            origin: Point::new(0.0, 0.0),
            size: Size::new(width, height),
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            sampling: LayerSampling::Nearest,
        }
    }

    /// The reach of a change: eight pixels at level 0, doubling with every halving.
    #[test]
    fn support_matches_the_swift() {
        assert_eq!(TiledLayerRenderer::support(0), 8.0);
        assert_eq!(TiledLayerRenderer::support(1), 32.0);
        assert_eq!(TiledLayerRenderer::support(2), 64.0);
        assert_eq!(TiledLayerRenderer::support(3), 128.0);
    }

    /// Pieces grow outward to whole multiples of the halving step, measured from the grid's origin.
    #[test]
    fn aligned_grows_outward() {
        let origin = Point::new(5.0, 5.0);
        // [2, 23] × [3, 23] on a step of 2 from (5, 5): [1, 23] × [3, 23].
        assert_eq!(
            TiledLayerRenderer::aligned(Rect::new(2.0, 3.0, 21.0, 20.0), 2.0, origin),
            Rect::new(1.0, 3.0, 22.0, 20.0)
        );
    }

    /// Each square holds its own part of a change, grown to the step grid; the parts tile the change's
    /// grown bounding box.
    #[test]
    fn interiors_are_grown_to_the_step_grid() {
        let origin = Point::new(5.0, 5.0);
        let interiors =
            TiledLayerRenderer::interiors(&[Rect::new(10.0, 10.0, 5.0, 5.0)], 8.0, 256.0, 2.0, origin, None);
        assert_eq!(
            interiors,
            vec![
                Rect::new(1.0, 1.0, 4.0, 4.0),
                Rect::new(1.0, 5.0, 4.0, 18.0),
                Rect::new(5.0, 1.0, 18.0, 4.0),
                Rect::new(5.0, 5.0, 18.0, 18.0),
            ]
        );
    }

    /// Nothing near a change means no pieces at all.
    #[test]
    fn interiors_skip_changes_outside_the_visible_area() {
        let interiors = TiledLayerRenderer::interiors(
            &[Rect::new(10.0, 10.0, 5.0, 5.0)],
            8.0,
            256.0,
            2.0,
            Point::ZERO,
            Some(Rect::new(500.0, 500.0, 10.0, 10.0)),
        );
        assert!(interiors.is_empty());
    }

    /// A piece's region is the interior grown by the support, limited to the layer's grid.
    #[test]
    fn piece_region_is_limited_to_its_bounds() {
        let full = Rect::new(0.0, 0.0, 100.0, 100.0);
        let piece = TiledLayerRenderer::piece(
            Rect::new(0.0, 0.0, 50.0, 50.0),
            0,
            Point::ZERO,
            Some(full),
            false,
            |canvas, region| {
                canvas.set_fill_gray(214.0 / 255.0);
                canvas.fill_rect(region);
            },
        )
        .expect("a piece");
        assert_eq!(piece.region, Rect::new(0.0, 0.0, 58.0, 58.0));
        assert_eq!(piece.interior, Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(piece.image.width(), 58);
        assert_eq!(piece.image.height(), 58);
        let PixelImage::Rgba(image) = &piece.image else {
            panic!("a color piece")
        };
        assert_eq!(image.get(20, 20), [214, 214, 214, 255]);
    }

    /// A committed raster draws its base pixels where the transform puts them, and its tiles over
    /// the pixels they replaced.
    #[test]
    fn draw_raster_draws_base_and_tiles() {
        let base = rgba(4, 1, [0, 0, 255, 255]);
        let mut raster = RasterSnapshot::new(4, 1, Some(base), Rect::new(0.0, 0.0, 4.0, 1.0), vec![], false, None);
        raster
            .patches
            .push(BrushPatch::new(Rect::new(2.0, 0.0, 2.0, 1.0), rgba(2, 1, [255, 0, 0, 255])));
        let mut canvas = Canvas::new_rgba(4, 1);
        TiledLayerRenderer::draw_raster(
            &raster,
            &transform(4.0, 1.0),
            Point::new(2.0, 0.5),
            1.0,
            1.0,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [0, 0, 255, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 255, 255]);
        assert_eq!(result.get(2, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(3, 0), [255, 0, 0, 255]);
    }

    /// Pieces are built once per snapshot and level, and handed out again for the same raster.
    #[test]
    fn pieces_are_reused_for_the_same_raster() {
        let base = rgba(8, 8, [0, 255, 0, 255]);
        let raster = RasterSnapshot::new(
            8,
            8,
            Some(base),
            Rect::new(0.0, 0.0, 8.0, 8.0),
            vec![BrushPatch::new(Rect::new(2.0, 2.0, 2.0, 2.0), rgba(2, 2, [255, 0, 0, 255]))],
            false,
            None,
        );
        let first = TiledPieceCache::shared().pieces(&raster, 0);
        assert!(!first.is_empty());
        let second = TiledPieceCache::shared().pieces(&raster, 0);
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(image_identity(&a.image), image_identity(&b.image));
        }
    }

    /// A stroke's tiles replace the pixels they cover, and the piece's margin keeps them seamless.
    #[test]
    fn draw_stroke_replaces_tiles() {
        let base = rgba(8, 1, [0, 0, 255, 255]);
        let patches = vec![BrushPatch::new(
            Rect::new(1.0, 0.0, 1.0, 1.0),
            rgba(1, 1, [255, 0, 0, 255]),
        )];
        let mut canvas = Canvas::new_rgba(8, 1);
        TiledLayerRenderer::draw_stroke(
            8,
            1,
            Rect::new(0.0, 0.0, 8.0, 1.0),
            &patches,
            Some(&base),
            None,
            &transform(8.0, 1.0),
            Point::new(4.0, 0.5),
            1.0,
            1.0,
            LayerBlendMode::Normal,
            None,
            &mut canvas,
        );
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [0, 0, 255, 255]);
        assert_eq!(result.get(1, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(7, 0), [0, 0, 255, 255]);
    }

    /// A painted layer (image plus replacement tiles) must look like the same pixels drawn as one
    /// image — a committed snapshot, a live stroke over it, and a first stroke on a plain image.
    #[test]
    fn tiled_layers_draw_like_one_image() {
        for (scale, rotation) in [(0.2_f64, 0.0_f64), (0.7, 0.0), (0.3, 25.0)] {
            let base = noise(1600, 1000, 7, 255);
            let committed = BrushPatch::new(
                Rect::new(1100.0, 400.0, 256.0, 256.0),
                noise(256, 256, 99, 255),
            );
            let raster = RasterSnapshot::new(
                1600,
                1000,
                Some(base.clone()),
                Rect::new(0.0, 0.0, 1600.0, 1000.0),
                vec![committed.clone()],
                false,
                None,
            );
            let mut transform = transform(1600.0, 1000.0);
            transform.sampling = LayerSampling::High;
            transform.rotation = rotation;
            // Rotated, the resample and hard clip edges aren't exactly crop-invariant: on this
            // worst-case noise pieces can land a few levels off, well below anything visible.
            let tolerance = if rotation == 0.0 { 2 } else { 12 };
            let side = (1700.0 * scale).ceil() as usize;
            let center = Point::new(side as f64 / 2.0, side as f64 / 2.0);

            let finished = composite(&base, std::slice::from_ref(&committed));
            let expected = render(side, |canvas| {
                LayerRenderer::draw(
                    &finished,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let drawn = render(side, |canvas| {
                TiledLayerRenderer::draw_raster(
                    &raster,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let committed_difference = largest_difference(&expected, &drawn, side, None);
            assert!(
                committed_difference <= tolerance,
                "a committed painted layer at {scale}x, {rotation} deg: {committed_difference}"
            );

            // A live stroke over the committed layer, crossing a piece boundary.
            let stroke = BrushPatch::new(
                Rect::new(900.0, 500.0, 256.0, 256.0),
                noise(256, 256, 5, 255),
            );
            let expected_live = render(side, |canvas| {
                LayerRenderer::draw(
                    &composite(&finished, std::slice::from_ref(&stroke)),
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let live = render(side, |canvas| {
                TiledLayerRenderer::draw_stroke(
                    1600,
                    1000,
                    Rect::new(0.0, 0.0, 1600.0, 1000.0),
                    std::slice::from_ref(&stroke),
                    None,
                    Some(&raster),
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let live_difference = largest_difference(&expected_live, &live, side, None);
            assert!(
                live_difference <= tolerance,
                "a live stroke on a painted layer at {scale}x, {rotation} deg: {live_difference}"
            );

            // A first stroke on a plain image.
            let expected_first = render(side, |canvas| {
                LayerRenderer::draw(
                    &composite(&base, std::slice::from_ref(&stroke)),
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let first = render(side, |canvas| {
                TiledLayerRenderer::draw_stroke(
                    1600,
                    1000,
                    Rect::new(0.0, 0.0, 1600.0, 1000.0),
                    std::slice::from_ref(&stroke),
                    Some(&base),
                    None,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let first_difference = largest_difference(&expected_first, &first, side, None);
            assert!(
                first_difference <= tolerance,
                "a first stroke on an image at {scale}x, {rotation} deg: {first_difference}"
            );
        }
    }

    /// A live mask stroke draws the layer through the mask it is painting, like the committed mask.
    #[test]
    fn mask_strokes_draw_like_one_mask() {
        for (scale, rotation) in [(0.2_f64, 0.0_f64), (0.7, 0.0), (0.3, 25.0)] {
            let layer = noise(1600, 1000, 11, 255);
            let old_mask = gray_noise(1600, 1000, 12);
            let tile = BrushPatch::new(
                Rect::new(900.0, 500.0, 256.0, 256.0),
                gray_noise(256, 256, 13),
            );
            let mut transform = transform(1600.0, 1000.0);
            transform.sampling = LayerSampling::High;
            transform.rotation = rotation;
            let tolerance = if rotation == 0.0 { 2 } else { 12 };
            let side = (1700.0 * scale).ceil() as usize;
            let center = Point::new(side as f64 / 2.0, side as f64 / 2.0);

            let unmasked = render(side, |canvas| {
                LayerRenderer::draw(
                    &layer,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let expected = render(side, |canvas| {
                LayerRenderer::draw(
                    &layer,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    Some(&mask_composite(&old_mask, std::slice::from_ref(&tile))),
                    canvas,
                );
            });
            let old = ImportedImage::new(old_mask.clone(), old_mask.clone(), "Mask");
            let live = render(side, |canvas| {
                TiledLayerRenderer::draw_mask_stroke(
                    1600,
                    1000,
                    Rect::new(0.0, 0.0, 1600.0, 1000.0),
                    std::slice::from_ref(&tile),
                    Some(&old),
                    Some(&layer),
                    None,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    canvas,
                );
            });
            let mask_difference = largest_difference(&expected, &live, side, Some(&unmasked));
            assert!(
                mask_difference <= tolerance,
                "a live mask stroke at {scale}x, {rotation} deg: {mask_difference}"
            );
        }
    }

    /// Painting at the layer's own edge must not change it: a piece composes its square plus a
    /// margin, and at the edge that margin falls outside the layer, so resampling it must not pull
    /// that emptiness into the border.
    #[test]
    fn painting_at_the_layers_edge_does_not_change_it() {
        fn render_retina(pixels: usize, device: f64, draw: impl FnOnce(&mut Canvas)) -> Rgba8Image {
            render(pixels, |canvas| {
                canvas.scale(device, device);
                draw(canvas);
            })
        }

        let image = noise(3360, 1812, 11, 255);
        let mut transform = transform(336.0, 181.0);
        transform.sampling = LayerSampling::High;
        let grid = Rect::new(0.0, 0.0, 3360.0, 1812.0);
        let (side, device): (f64, f64) = (600.0, 2.0);
        let pixels = (side * device).ceil() as usize;
        let white_mask = PixelImage::Gray(Arc::new(Gray8Image::uniform(1, 1, 255)));
        let old_mask = ImportedImage::new(white_mask.clone(), white_mask.clone(), "Mask");
        let largest = |a: &Rgba8Image, b: &Rgba8Image| {
            let mut worst = 0;
            for y in 0..pixels {
                for x in 0..pixels {
                    let (a, b) = (a.get(x, y), b.get(x, y));
                    for channel in 0..4 {
                        worst = worst.max((a[channel] as i32 - b[channel] as i32).abs());
                    }
                }
            }
            worst
        };

        for zoom in [10.749, 1.0] {
            // The layer's top-left corner sits 50 points into the view.
            let center = Point::new(336.0 * zoom / 2.0 + 50.0, 181.0 * zoom / 2.0 + 50.0);
            let plain = render_retina(pixels, device, |canvas| {
                LayerRenderer::draw(
                    &image,
                    &transform,
                    center,
                    zoom,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let masked = render_retina(pixels, device, |canvas| {
                LayerRenderer::draw(
                    &image,
                    &transform,
                    center,
                    zoom,
                    1.0,
                    LayerBlendMode::Normal,
                    Some(&white_mask),
                    canvas,
                );
            });
            for origin in [Point::new(0.0, 0.0), Point::new(256.0, 256.0)] {
                let rect = Rect::new(origin.x, origin.y, 256.0, 256.0);
                // Each patch holds the pixels already beneath it, so a correct stroke changes nothing.
                let same = match &image {
                    PixelImage::Rgba(rgba) => rgba
                        .cropped(rect)
                        .map(|image| PixelImage::Rgba(Arc::new(image))),
                    PixelImage::Gray(gray) => gray
                        .cropped(rect)
                        .map(|image| PixelImage::Gray(Arc::new(image))),
                }
                .expect("a crop of the layer");
                let painting = render_retina(pixels, device, |canvas| {
                    TiledLayerRenderer::draw_stroke(
                        3360,
                        1812,
                        grid,
                        &[BrushPatch::new(rect, same.clone())],
                        Some(&image),
                        None,
                        &transform,
                        center,
                        zoom,
                        1.0,
                        LayerBlendMode::Normal,
                        None,
                        canvas,
                    );
                });
                assert!(
                    largest(&plain, &painting) <= 2,
                    "painting at {},{} at {zoom}x zoom",
                    origin.x,
                    origin.y
                );

                // The same for a mask stroke, which shows the layer through the mask it is painting.
                let reveal = BrushPatch::new(
                    rect,
                    PixelImage::Gray(Arc::new(Gray8Image::uniform(256, 256, 255))),
                );
                let mask_painting = render_retina(pixels, device, |canvas| {
                    TiledLayerRenderer::draw_mask_stroke(
                        3360,
                        1812,
                        grid,
                        std::slice::from_ref(&reveal),
                        Some(&old_mask),
                        Some(&image),
                        None,
                        &transform,
                        center,
                        zoom,
                        1.0,
                        LayerBlendMode::Normal,
                        canvas,
                    );
                });
                let mask_difference = largest(&masked, &mask_painting);
                assert!(
                    mask_difference <= 2,
                    "painting a mask at {},{} at {zoom}x zoom: {mask_difference}",
                    origin.x,
                    origin.y
                );
            }
        }
    }

    /// Where pieces meet, split pixels must be drawn once: translucent pixels show any double draw
    /// as a line.
    #[test]
    fn translucent_strokes_draw_like_one_image() {
        for (scale, rotation) in [(0.3_f64, 0.0_f64), (0.7, 25.0)] {
            let base = noise(1600, 1000, 21, 128);
            let tile = BrushPatch::new(
                Rect::new(900.0, 500.0, 256.0, 256.0),
                noise(256, 256, 22, 128),
            );
            let silhouette = noise(1600, 1000, 1, 255);
            let mut transform = transform(1600.0, 1000.0);
            transform.sampling = LayerSampling::High;
            transform.rotation = rotation;
            let tolerance = if rotation == 0.0 { 2 } else { 12 };
            let side = (1700.0 * scale).ceil() as usize;
            let center = Point::new(side as f64 / 2.0, side as f64 / 2.0);

            let inside = render(side, |canvas| {
                LayerRenderer::draw(
                    &silhouette,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let expected = render(side, |canvas| {
                LayerRenderer::draw(
                    &composite(&base, std::slice::from_ref(&tile)),
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let live = render(side, |canvas| {
                TiledLayerRenderer::draw_stroke(
                    1600,
                    1000,
                    Rect::new(0.0, 0.0, 1600.0, 1000.0),
                    std::slice::from_ref(&tile),
                    Some(&base),
                    None,
                    &transform,
                    center,
                    scale,
                    1.0,
                    LayerBlendMode::Normal,
                    None,
                    canvas,
                );
            });
            let translucent_difference = largest_difference(&expected, &live, side, Some(&inside));
            assert!(
                translucent_difference <= tolerance,
                "a translucent live stroke at {scale}x, {rotation} deg: {translucent_difference}"
            );
        }
    }
}
