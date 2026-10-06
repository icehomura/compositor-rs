//! Port of `references/Compositor/Compositor/Document/BrushStroke.swift`: the tiled brush engine, its
//! falloff tip, and the commit that turns the stroke's tiles into an immutable raster.
//!
//! Only touched 256-pixel tiles allocate writable pixels, so a stroke never copies the whole layer on
//! a mouse-move event. `MetalBrushCoverage`'s compute kernel is [`continuous_brush`], a CPU kernel
//! with the same arithmetic, run over tiles with `rayon`; the software dab path (`use_gpu == false`)
//! is ported as it was, dabs at the shared deposition spacing. Soft tips accumulate paint within the
//! stroke; hard tips keep their antialiased silhouette; the stroke-wide opacity cap is preserved.

use crate::brush_pixels::brush_alpha_bounds;
use crate::canvas::{Canvas, GradientExtend, GradientKind, GradientPaint, InterpolationQuality};
use crate::heal_pixels::{heal_coverage_bounds, spot_heal};
use crate::raster::{rect_applying, Raster};
use compositor_core::document::ImageLayer;
use compositor_core::error::CoreError;
use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_transform::LayerTransform;
use compositor_core::limits::{document_pixel_budget, max_surface_megapixels, MAX_SIDE, MAX_SIDE_EXTENT};
use compositor_core::raster::{BrushPatch, PatchImage, RasterSnapshot, THUMBNAIL_MAX_SIDE};
use compositor_core::selection::SelectionClip;
use compositor_core::{Gray8Image, PaletteColor};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// `SpotHealingMode: String, CaseIterable`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SpotHealingMode {
    #[default]
    ContentAware,
    CreateTexture,
    ProximityMatch,
}

impl SpotHealingMode {
    /// `CaseIterable` order.
    pub const ALL: [SpotHealingMode; 3] = [
        SpotHealingMode::ContentAware,
        SpotHealingMode::CreateTexture,
        SpotHealingMode::ProximityMatch,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            SpotHealingMode::ContentAware => "Content-Aware",
            SpotHealingMode::CreateTexture => "Create Texture",
            SpotHealingMode::ProximityMatch => "Proximity Match",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.raw_value() == value)
    }
}

/// `GradientShape`'s two cases, as the brush's `fillGradient` takes them. The session's own
/// `GradientShape` (`compositor_core::image_ops`) is the persisted vocabulary; this is the pixel
/// engine's copy of the same two shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GradientShape {
    #[default]
    Linear,
    Radial,
}

impl GradientShape {
    /// `CaseIterable` order.
    pub const ALL: [GradientShape; 2] = [GradientShape::Linear, GradientShape::Radial];

    pub fn raw_value(self) -> &'static str {
        match self {
            GradientShape::Linear => "Linear",
            GradientShape::Radial => "Radial",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.raw_value() == value)
    }
}

/// `BrushSettings`.
#[derive(Clone, Debug, PartialEq)]
pub struct BrushSettings {
    pub diameter: f64,
    pub hardness: f64,
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    /// Caps the whole stroke, as in Photoshop: overlapping dabs never exceed it.
    pub opacity: f64,
    /// 0–100. The brush trails the pointer on a string of this length, so a shaky hand
    /// draws a smooth line; 0 follows the pointer exactly.
    pub smoothing: f64,
    /// Blur: how far it softens, in canvas pixels, whatever the brush's size. Strength sets how much.
    pub blur_radius: f64,
    /// Spot-healing uses nearby source pixels instead of the foreground color.
    /// Erase: the stroke clears the layer's pixels instead of painting color on them.
    pub erasing: bool,
    pub healing: bool,
    pub healing_mode: SpotHealingMode,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            diameter: 40.0,
            hardness: 1.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            opacity: 1.0,
            smoothing: 0.0,
            blur_radius: 5.0,
            erasing: false,
            healing: false,
            healing_mode: SpotHealingMode::ContentAware,
        }
    }
}

/// Only touched 256px tiles allocate writable pixels. Snapshots copy at most those tiles, never the
/// entire layer on a mouse-move event.
///
/// The Swift class held a `CGContext` per tile; here a tile is a [`Canvas`] over its own pixels,
/// snapshotted into `image` as the stroke publishes.
pub struct BrushStroke {
    /// The layer the stroke paints on (`layer`).
    pub layer: ImageLayer,
    /// Painting the layer's mask rather than its pixels.
    pub is_mask: bool,
    /// The stroke's grid, in pixels.
    pub width: usize,
    pub height: usize,
    pub settings: BrushSettings,
    /// The document (`canvas`).
    pub canvas: Size,
    /// Where the stroke's grid sits on the document (`pixelToDocument`).
    pub pixel_to_document: AffineTransform,
    /// Where the grid sits in the layer's own pixels (`sourceRect`).
    pub source_rect: Rect,
    /// The grid placed on the document (`paintTransform`).
    pub paint_transform: LayerTransform,
    /// What a mask is past its pixels, and so what new mask area starts as (`maskBackground`): white
    /// reveals, black hides.
    pub mask_background: f64,
    /// How many document pixels the stroke's tiles may allocate (`pixelLimit`).
    pub pixel_limit: usize,
    /// Limits every edit to the document selection; `None` when nothing is selected.
    pub selection_clip: Option<SelectionClip>,
    /// Clone Stamp, Blur, Smudge and Liquify: an image painted through the tip, already shifted by
    /// any source offset (`clone`).
    pub clone: Option<CloneSample>,
    /// Makes part of `clone`'s image, given a rect of its pixels (top-left rows), in place of
    /// cropping it (`cloneRender`).
    pub clone_render: Option<Arc<dyn Fn(Rect) -> Option<PixelImage> + Send + Sync>>,
    /// A Blur stroke: `clone` holds the layer blurred, painted in place through the tip.
    pub is_blur: bool,
    /// The clone sample replaces what's under the tip rather than drawing over it, so it can also
    /// clear pixels.
    pub replaces_with_clone: bool,
    /// The undo name, when the stroke's kind doesn't say it.
    pub edit_name: Option<String>,
    /// Selected image pixels cut out of the layer, kept as this with the lifted pixels over it
    /// (`holed`).
    pub holed: Option<PixelImage>,
    /// The part of the document the stroke touched since the last publish (`dirtyDocumentRect`).
    pub dirty_document_rect: Option<Rect>,

    /// The layer's pixels (or the mask's), as they were when the stroke was made (`source`).
    source: Option<PixelImage>,
    /// The tiles painted so far, keyed by `y * columns + x`.
    tiles: HashMap<usize, Tile>,
    /// The bounds of every allocated tile (`allocatedBounds`).
    allocated_bounds: Option<Rect>,
    /// The brush tip, rendered once and stamped for every dab. Only worth it up to `stampLimit`:
    /// past that, blitting the big tip costs more than drawing the falloff (`stamp`).
    stamp: Option<Gray8Image>,
    /// The tip again, sized in layer pixels so dabs land 1:1 on the grid with nothing for the
    /// sampler to resample (`gridTip`).
    grid_tip: Option<Gray8Image>,
    /// The kernel path (`useGPU`): [`continuous_brush`], the CPU port of `MetalBrushCoverage`'s
    /// compute kernel, replaces the software dab path.
    gpu: bool,
    /// Coverage under the provisional tail; `None` where the tile had no coverage yet
    /// (`tailBackup`).
    tail_backup: HashMap<usize, Option<Gray8Image>>,
    /// The newest mouse samples, at most four (`samples`).
    samples: Vec<Point>,
    /// The last dab position (`previous`).
    previous: Option<Point>,
    /// Distance left before the next dab (`distanceToNext`).
    distance_to_next: f64,
    /// The part of each tile the stroke touched since the last publish, in tile-local pixels
    /// (`dirtyTiles`).
    dirty_tiles: HashMap<usize, Rect>,
    /// The tiles showing the replaceable tail (`gpuTailKeys`).
    gpu_tail_keys: HashSet<usize>,
    /// The stroke's paint (`paintColor`): the color tip, or a mask's gray value.
    paint_color: PaletteColor,
    /// The part of `clone` each tile draws, cut once (`clonePieces`).
    clone_pieces: HashMap<usize, (PixelImage, Rect)>,
    /// Selected image pixels cut out of the layer, in layer pixel coordinates (`lifted`).
    lifted: Option<(PixelImage, Rect)>,
    /// The tiles a move touched (`moveTiles`).
    move_tiles: HashSet<usize>,
}

/// Tile edge in layer pixels. Wider tiles were measured to be no faster for wide brushes and slower
/// for narrow ones (`BrushStroke.tileSize`).
const TILE_SIZE: usize = 256;

/// One tile of the stroke's grid (`BrushStroke.Tile`).
struct Tile {
    /// The tile's rectangle in the stroke's grid.
    rect: Rect,
    /// The tile's pixels: a color context, or a coverage context on a mask.
    canvas: Canvas,
    /// The tile's content before the stroke touched it (`base`).
    base: Option<PatchImage>,
    /// The composited tile's last snapshot (`image`).
    image: Option<PatchImage>,
    /// Per-tile grayscale coverage, for the software dabs (`coverage`).
    coverage: Option<Gray8Image>,
    /// The kernel's permanent paint buffer, never re-accumulated for a replaceable tail.
    permanent: Option<Vec<f32>>,
    /// The kernel's 8-bit coverage preview.
    preview: Option<Vec<u8>>,
}

/// `ProjectError.tooLarge`.
fn too_large() -> CoreError {
    CoreError::TooLarge(max_surface_megapixels())
}

impl BrushStroke {
    /// `init(layer:mask:settings:canvas:useGPU:growsMask:)`, on the kernel path: the CPU port of
    /// `MetalBrushCoverage`'s compute kernel, which the app's GPU canvas used.
    pub fn new(
        layer: &ImageLayer,
        mask: bool,
        settings: BrushSettings,
        canvas: Size,
        grows_mask: bool,
    ) -> Result<BrushStroke, CoreError> {
        Self::new_with_gpu(layer, mask, settings, canvas, true, grows_mask)
    }

    /// The `useGPU: false` form: the software dab path, still laid at the shared deposition spacing.
    pub fn new_with_gpu(
        layer: &ImageLayer,
        mask: bool,
        settings: BrushSettings,
        canvas: Size,
        use_gpu: bool,
        grows_mask: bool,
    ) -> Result<BrushStroke, CoreError> {
        // A mask on its own placement is painted in its own pixel grid; otherwise the grid is the
        // layer's.
        let placed_mask = if mask {
            layer
                .mask
                .as_ref()
                .and_then(|mask| mask.placement.map(|placement| (&mask.asset.image, placement)))
        } else {
            None
        };
        let base = placed_mask.map(|(_, placement)| placement).unwrap_or(layer.transform);
        // A solid mask is a single pixel stretched over its place; painted, it gets one pixel per
        // document pixel.
        let solid_placed = placed_mask.map(|(image, _)| image.width() <= 2 && image.height() <= 2) == Some(true);
        let original_width = if solid_placed {
            base.size.width.round().max(1.0) as usize
        } else {
            placed_mask
                .map(|(image, _)| image.width())
                .or_else(|| layer.asset.as_ref().map(|asset| asset.image.width()))
                .unwrap_or_else(|| layer.size().width.round().max(1.0) as usize)
        };
        let original_height = if solid_placed {
            base.size.height.round().max(1.0) as usize
        } else {
            placed_mask
                .map(|(image, _)| image.height())
                .or_else(|| layer.asset.as_ref().map(|asset| asset.image.height()))
                .unwrap_or_else(|| layer.size().height.round().max(1.0) as usize)
        };
        let original_mapping = Raster::pixel_to_document(&base, original_width as f64, original_height as f64);
        let original_bounds = Rect::new(0.0, 0.0, original_width as f64, original_height as f64);
        let document_rect = Rect::new(0.0, 0.0, canvas.width, canvas.height);
        // A mask painted by a brush can grow past its layer; every other mask edit stays within it.
        let extent = if mask && !grows_mask {
            original_bounds
        } else {
            original_bounds.union(rect_applying(document_rect, original_mapping.inverted()).integral())
        };
        let mask_background = if mask {
            layer
                .mask
                .as_ref()
                .and_then(|mask| mask.asset.thumbnail.as_gray().map(LayerMask::background))
                .unwrap_or(1.0)
        } else {
            1.0
        };
        let width = extent.width() as usize;
        let height = extent.height() as usize;
        let source_rect = original_bounds.offset_by(-extent.min_x(), -extent.min_y());
        let pixel_to_document = original_mapping.translated_by(extent.min_x(), extent.min_y());
        let mut expanded = base;
        expanded.size = Size::new(
            width as f64 * base.size.width / original_width as f64,
            height as f64 * base.size.height / original_height as f64,
        );
        let center = original_mapping.applying(Point::new(extent.mid_x(), extent.mid_y()));
        expanded.origin = Point::new(
            center.x - expanded.size.width / 2.0,
            center.y - expanded.size.height / 2.0,
        );
        let paint_transform = expanded;
        if !(1..=1_000_000_000).contains(&width)
            || !(1..=1_000_000_000).contains(&height)
            || !(1..=MAX_SIDE).contains(&original_width)
            || !(1..=MAX_SIDE).contains(&original_height)
            || !settings.diameter.is_finite()
            || !(1.0..=2100.0).contains(&settings.diameter)
            || !settings.hardness.is_finite()
            || !(0.0..=1.0).contains(&settings.hardness)
            || !settings.opacity.is_finite()
            || !(0.01..=1.0).contains(&settings.opacity)
        {
            return Err(too_large());
        }
        let source = if mask {
            layer.mask.as_ref().map(|mask| mask.asset.image.clone())
        } else {
            layer.asset.as_ref().map(|asset| asset.image.clone())
        };
        let scale_x = (pixel_to_document.a * pixel_to_document.a + pixel_to_document.b * pixel_to_document.b).sqrt();
        let scale_y = (pixel_to_document.c * pixel_to_document.c + pixel_to_document.d * pixel_to_document.d).sqrt();
        let square = pixel_to_document.b.abs() < 1e-9
            && pixel_to_document.c.abs() < 1e-9
            && scale_x > 1e-9
            && (scale_x - scale_y).abs() < 1e-9;
        let grid_diameter = settings.diameter / scale_x;
        let grid_tip = if !use_gpu && square && (1.0..=3000.0).contains(&grid_diameter) {
            Some(Self::tip(grid_diameter, settings.hardness))
        } else {
            None
        };
        // Drawn through the tile transform, the stamp is rendered as finely as the layer's pixels:
        // magnified onto the finer grid of a scaled-down layer, it left blocky dabs that showed once
        // the layer was scaled back up.
        let stamp_diameter = settings.diameter / 1.0f64.min(scale_x).min(scale_y);
        let stamp = if !use_gpu && grid_tip.is_none() && stamp_diameter <= 160.0 {
            Some(Self::tip(stamp_diameter, settings.hardness))
        } else {
            None
        };
        let paint_color = if mask {
            PaletteColor::new(settings.red, settings.red, settings.red)
        } else {
            PaletteColor::new(settings.red, settings.green, settings.blue)
        };
        Ok(BrushStroke {
            layer: layer.clone(),
            is_mask: mask,
            width,
            height,
            settings,
            canvas,
            pixel_to_document,
            source_rect,
            paint_transform,
            mask_background,
            pixel_limit: document_pixel_budget(),
            selection_clip: None,
            clone: None,
            clone_render: None,
            is_blur: false,
            replaces_with_clone: false,
            edit_name: None,
            holed: None,
            dirty_document_rect: None,
            source,
            tiles: HashMap::new(),
            allocated_bounds: None,
            stamp,
            grid_tip,
            gpu: use_gpu,
            tail_backup: HashMap::new(),
            samples: Vec::new(),
            previous: None,
            distance_to_next: 0.0,
            dirty_tiles: HashMap::new(),
            gpu_tail_keys: HashSet::new(),
            paint_color,
            clone_pieces: HashMap::new(),
            lifted: None,
            move_tiles: HashSet::new(),
        })
    }

    /// The tiles painted so far (`patches`).
    pub fn patches(&self) -> Vec<BrushPatch> {
        self.tiles
            .values()
            .filter_map(|tile| tile.image.as_ref().map(|image| BrushPatch::new(tile.rect, image.clone())))
            .collect()
    }

    /// The stroke's pixels so far, as a rectangle among whole tiles (`committedBounds`).
    pub fn committed_bounds(&self) -> Rect {
        self.allocated_bounds.unwrap_or(self.source_rect).integral()
    }

    /// Where the committed pixels sit on the document (`committedTransform`).
    pub fn committed_transform(&self) -> LayerTransform {
        self.transform_for(self.committed_bounds())
    }

    /// Where `rect` of the stroke's grid sits on the document (`transform(for:)`).
    pub fn transform_for(&self, bounds: Rect) -> LayerTransform {
        let center = self
            .pixel_to_document
            .applying(Point::new(bounds.mid_x(), bounds.mid_y()));
        let mut result = self.paint_transform;
        result.size = Size::new(
            bounds.width() * self.paint_transform.size.width / self.width as f64,
            bounds.height() * self.paint_transform.size.height / self.height as f64,
        );
        result.origin = Point::new(
            center.x - result.size.width / 2.0,
            center.y - result.size.height / 2.0,
        );
        result
    }

    /// Where `rect` of the stroke's grid sits to be copied from `offset` document pixels away: the
    /// offset carried into the grid, turned, scaled and flipped as the layer is (`gridRect`).
    pub fn grid_rect(&self, rect: Rect, offset: Size) -> Rect {
        let shift = self
            .pixel_to_document
            .inverted()
            .applying(Point::new(offset.width, offset.height));
        rect.offset_by(-shift.x, -shift.y)
    }

    /// The layer's own pixels, for a duplicating move, which leaves them all in place (`original`).
    pub fn original(&self) -> Option<&PixelImage> {
        self.source.as_ref()
    }

    /// The immutable pixels the stroke commits, where they sit and the bounds of the stroke's grid
    /// they were taken from (`paintSnapshot()`).
    pub fn paint_snapshot(&self) -> Result<PaintSnapshot, CoreError> {
        let mut bounds: Option<Rect> = if self.source.is_none() { None } else { Some(self.source_rect) };
        if !self.is_mask {
            for tile in self.tiles.values() {
                let image = tile.canvas.rgba();
                let edges = brush_alpha_bounds(image.data(), image.width(), image.height(), image.stride());
                if edges[2] <= edges[0] || edges[3] <= edges[1] {
                    continue;
                }
                let rect = Rect::new(
                    edges[0] as f64,
                    edges[1] as f64,
                    (edges[2] - edges[0]) as f64,
                    (edges[3] - edges[1]) as f64,
                )
                .offset_by(tile.rect.min_x(), tile.rect.min_y());
                bounds = Some(match bounds {
                    Some(bounds) => bounds.union(rect),
                    None => rect,
                });
            }
        }
        // A mask keeps every tile the stroke touched: painted past its old pixels, it grows to hold
        // them.
        let crop = if self.is_mask {
            self.committed_bounds()
        } else {
            bounds.unwrap_or_else(|| self.committed_bounds())
        };
        let raster = RasterSnapshot::replacing(
            if self.is_mask {
                self.layer.mask.as_ref().map(|mask| &mask.asset)
            } else {
                self.layer.asset.as_ref()
            },
            self.source_rect,
            &self.patches(),
            crop,
            self.is_mask,
            self.mask_background,
        );
        let image = raster.materialize();
        let thumbnail = raster.thumbnail(THUMBNAIL_MAX_SIDE);
        Ok(PaintSnapshot {
            asset: ImportedImage::with_raster(image, thumbnail, self.layer.name.clone(), Some(raster)),
            transform: self.transform_for(crop),
            bounds: crop,
        })
    }

    /// The stroke's grid, source pixels and tiles, ready to assemble off the pointer's path
    /// (`commitInput()`).
    pub fn commit_input(&self) -> BrushCommitInput {
        let bounds = self.committed_bounds();
        BrushCommitInput {
            width: bounds.width() as usize,
            height: bounds.height() as usize,
            source: self.source.clone(),
            patches: self
                .patches()
                .into_iter()
                .map(|patch| {
                    BrushPatch::new(
                        patch.rect.offset_by(-bounds.min_x(), -bounds.min_y()),
                        patch.image,
                    )
                })
                .collect(),
            mask: self.is_mask,
            name: self.layer.name.clone(),
            source_rect: self.source_rect.offset_by(-bounds.min_x(), -bounds.min_y()),
            fill: self.mask_background,
        }
    }

    /// Soft-tip deposition rate, shared with the continuous kernel. The software fallback lays
    /// actual dabs at this spacing (`spacingFraction(_:)`).
    pub fn spacing_fraction(hardness: f64) -> f64 {
        if hardness >= 1.0 {
            0.015
        } else {
            0.025
        }
    }

    /// The tip as grayscale coverage: white at full strength, fading to black at the rim (`tip`).
    fn tip(diameter: f64, hardness: f64) -> Gray8Image {
        let size = 1.max(diameter.ceil() as usize);
        let radius = size as f64 / 2.0;
        let mut image = Gray8Image::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let distance = ((x as f64 + 0.5 - radius).powi(2) + (y as f64 + 0.5 - radius).powi(2)).sqrt();
                image.set(x, y, (falloff_coverage(distance, radius, hardness) * 255.0).round() as u8);
            }
        }
        image
    }

    /// Mouse samples arrive sparsely, so dabs follow a smooth curve through them rather than
    /// straight chords. A curve piece needs the sample after it, so the newest piece is first drawn
    /// as a provisional straight tail (the stroke never trails the cursor), then erased and replaced
    /// by the curve when the next sample arrives or on `flush()` (`append(_:)`).
    pub fn append(&mut self, point: Point) -> Result<(), CoreError> {
        if !point.x.is_finite() || !point.y.is_finite() || point.x.abs() > 10_000_000.0 || point.y.abs() > 10_000_000.0 {
            return Ok(());
        }
        if self.samples.last() == Some(&point) {
            return Ok(());
        }
        if self.gpu {
            return self.append_continuous(point);
        }
        let mut changed = self.remove_tail();
        self.samples.push(point);
        if self.samples.len() > 4 {
            self.samples.remove(0);
        }
        let count = self.samples.len();
        if count == 1 {
            self.walk(point, &mut changed)?;
        } else if count >= 3 {
            let start = self.samples[count - 3];
            let end = self.samples[count - 2];
            let before = self.samples[count.saturating_sub(4)];
            let after = self.samples[count - 1];
            self.curve(start, end, before, after, &mut changed)?;
        }
        if count >= 2 {
            let start = self.samples[count - 2];
            self.draw_tail(start, point, &mut changed)?;
        }
        self.publish(&changed)
    }

    /// Replaces the provisional tail with the stroke's final curve piece. Safe to repeat (`flush()`).
    pub fn flush(&mut self) -> Result<(), CoreError> {
        if self.gpu {
            return self.flush_continuous();
        }
        let mut changed = self.remove_tail();
        let count = self.samples.len();
        if count >= 2 {
            let start = self.samples[count - 2];
            let end = self.samples[count - 1];
            let before = self.samples[count.saturating_sub(3)];
            self.curve(start, end, before, end, &mut changed)?;
            self.samples = vec![self.samples[count - 1]];
        }
        self.publish(&changed)
    }

    /// The continuous kernel's `append(_:)`: the settled piece is integrated once, the newest piece
    /// left as a replaceable tail.
    fn append_continuous(&mut self, point: Point) -> Result<(), CoreError> {
        self.samples.push(point);
        if self.samples.len() > 4 {
            self.samples.remove(0);
        }
        let n = self.samples.len();
        let settled = if n == 1 {
            vec![segment(point, point)]
        } else if n >= 3 {
            Self::continuous_curve(
                self.samples[n - 3],
                self.samples[n - 2],
                self.samples[n.saturating_sub(4)],
                point,
            )
        } else {
            Vec::new()
        };
        let tail = if n >= 2 { vec![segment(self.samples[n - 2], point)] } else { Vec::new() };
        self.render_continuous(&settled, &tail)
    }

    /// `flushContinuous()`: the last piece settles, and its tail disappears.
    fn flush_continuous(&mut self) -> Result<(), CoreError> {
        let n = self.samples.len();
        if n < 2 {
            return Ok(());
        }
        let settled = Self::continuous_curve(
            self.samples[n - 2],
            self.samples[n - 1],
            self.samples[n.saturating_sub(3)],
            self.samples[n - 1],
        );
        self.render_continuous(&settled, &[])?;
        self.samples = vec![self.samples[n - 1]];
        Ok(())
    }

    /// Adaptive chord subdivision keeps the centerline within 0.2 document pixels of the spline.
    /// Straight movement requires just one segment even at 4K (`continuousCurve`).
    fn continuous_curve(start: Point, end: Point, before: Point, after: Point) -> Vec<[f32; 4]> {
        fn knot(t: f64, a: Point, b: Point) -> f64 {
            t + 0.0001f64.max(((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt())
        }
        fn mix(a: Point, b: Point, ta: f64, tb: f64, t: f64) -> Point {
            let wa = (tb - t) / (tb - ta);
            let wb = (t - ta) / (tb - ta);
            Point::new(a.x * wa + b.x * wb, a.y * wa + b.y * wb)
        }
        let t0 = 0.0;
        let t1 = knot(t0, before, start);
        let t2 = knot(t1, start, end);
        let t3 = knot(t2, end, after);
        let point = |u: f64| -> Point {
            if u == 0.0 {
                return start;
            }
            if u == 1.0 {
                return end;
            }
            let t = t1 + (t2 - t1) * u;
            let a = mix(before, start, t0, t1, t);
            let b = mix(start, end, t1, t2, t);
            let c = mix(end, after, t2, t3, t);
            mix(mix(a, b, t0, t2, t), mix(b, c, t1, t3, t), t1, t2, t)
        };
        fn subdivide(
            point: &dyn Fn(f64) -> Point,
            result: &mut Vec<[f32; 4]>,
            a: Point,
            b: Point,
            lo: f64,
            hi: f64,
            depth: u32,
        ) {
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let length_squared = dx * dx + dy * dy;
            let error = |p: Point| -> f64 {
                let t = if length_squared > 0.0 {
                    (((p.x - a.x) * dx + (p.y - a.y) * dy) / length_squared).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let ex = p.x - a.x - t * dx;
                let ey = p.y - a.y - t * dy;
                (ex * ex + ey * ey).sqrt()
            };
            let mid = (lo + hi) / 2.0;
            let m = point(mid);
            let deviation = error(m)
                .max(error(point((lo + mid) / 2.0)))
                .max(error(point((mid + hi) / 2.0)));
            if deviation <= 0.2 || depth >= 10 {
                result.push(segment(a, b));
                return;
            }
            subdivide(point, result, a, m, lo, mid, depth + 1);
            subdivide(point, result, m, b, mid, hi, depth + 1);
        }
        let mut result = Vec::new();
        subdivide(&point, &mut result, start, end, 0.0, 1.0, 0);
        result
    }

    /// The tiles a segment reaches, from its box in document coordinates (`continuousKeys`).
    fn continuous_keys(&self, segments: &[[f32; 4]]) -> HashSet<usize> {
        let mut keys = HashSet::new();
        let reach = self.settings.diameter / 2.0 + 2.0;
        let inverse = self.pixel_to_document.inverted();
        let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
        for s in segments {
            let box_ = Rect::new(
                s[0].min(s[2]) as f64,
                s[1].min(s[3]) as f64,
                (s[2] - s[0]).abs() as f64,
                (s[3] - s[1]).abs() as f64,
            )
            .inset_by(-reach, -reach)
            .intersection(self.canvas_rect());
            if box_.is_null() || box_.is_empty() {
                continue;
            }
            let affected = rect_applying(box_, inverse).integral().intersection(self.grid_bounds());
            if affected.is_null() || affected.is_empty() {
                continue;
            }
            for (key, _, _) in affected_tiles(affected, columns) {
                keys.insert(key);
            }
        }
        keys
    }

    /// Runs the compute kernel over every tile the segments reach, then composites
    /// (`renderContinuous(settled:tail:)`).
    fn render_continuous(&mut self, settled: &[[f32; 4]], tail: &[[f32; 4]]) -> Result<(), CoreError> {
        if !self.gpu {
            return Ok(());
        }
        let tail_keys = self.continuous_keys(tail);
        let mut changed = self.continuous_keys(settled);
        changed.extend(tail_keys.iter().copied());
        changed.extend(self.gpu_tail_keys.iter().copied());
        let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
        let mut segments = Vec::with_capacity(settled.len() + tail.len());
        segments.extend_from_slice(settled);
        segments.extend_from_slice(tail);
        let mapping = self.pixel_to_document;
        let canvas = self.canvas;
        let diameter = self.settings.diameter;
        let hardness = self.settings.hardness;
        let scale_x = (mapping.a * mapping.a + mapping.b * mapping.b).sqrt();
        let scale_y = (mapping.c * mapping.c + mapping.d * mapping.d).sqrt();
        let antialias = 0.001f64.max(scale_x.min(scale_y));
        let spacing = 0.25f64.max(diameter * Self::spacing_fraction(hardness));
        for key in changed.iter().copied() {
            self.allocate_tile(key, key % columns, key / columns)?;
            let Some(tile) = self.tiles.get_mut(&key) else { continue };
            let rect = tile.rect;
            let width = rect.width() as usize;
            let height = rect.height() as usize;
            if tile.coverage.is_none() {
                tile.coverage = Some(Gray8Image::new(width, height));
            }
            if tile.permanent.is_none() {
                tile.permanent = Some(vec![0.0; width * height]);
            }
            if tile.preview.is_none() {
                tile.preview = Some(vec![0u8; width * height]);
            }
            let origin = mapping.applying(Point::new(rect.min_x(), rect.min_y()));
            let uniforms = BrushUniforms {
                mapping: [mapping.a as f32, mapping.b as f32, mapping.c as f32, mapping.d as f32],
                geometry: [origin.x as f32, origin.y as f32, (diameter / 2.0) as f32, hardness as f32],
                canvas: [canvas.width as f32, canvas.height as f32, antialias as f32, spacing as f32],
                counts: [
                    rect.width() as u32,
                    rect.height() as u32,
                    settled.len() as u32,
                    segments.len() as u32,
                ],
            };
            let Tile { permanent, preview, coverage, .. } = tile;
            let permanent = permanent.as_mut().expect("just set");
            let preview = preview.as_mut().expect("just set");
            continuous_brush(permanent, preview, &uniforms, &segments);
            coverage
                .as_mut()
                .expect("just set")
                .data_mut()
                .copy_from_slice(preview);
        }
        self.gpu_tail_keys = tail_keys;
        self.publish(&changed)
    }

    /// Lays evenly spaced dabs along a straight run from the previous dab position (`walk(to:)`).
    fn walk(&mut self, point: Point, changed: &mut HashSet<usize>) -> Result<(), CoreError> {
        let spacing = 0.25f64.max(self.settings.diameter * Self::spacing_fraction(self.settings.hardness));
        if let Some(previous) = self.previous {
            let dx = point.x - previous.x;
            let dy = point.y - previous.y;
            let length = (dx * dx + dy * dy).sqrt();
            if length > 0.0 {
                let mut distance = self.distance_to_next;
                while distance <= length {
                    self.dab(
                        Point::new(previous.x + dx * distance / length, previous.y + dy * distance / length),
                        changed,
                    )?;
                    distance += spacing;
                }
                self.distance_to_next = distance - length;
            }
        } else {
            self.dab(point, changed)?;
            self.distance_to_next = spacing;
        }
        self.previous = Some(point);
        Ok(())
    }

    /// Centripetal Catmull–Rom between `start` and `end`: it passes through every sample without the
    /// loops or overshoot uniform splines make at uneven mouse speeds
    /// (`curve(from:to:before:after:changed:)`).
    fn curve(
        &mut self,
        start: Point,
        end: Point,
        before: Point,
        after: Point,
        changed: &mut HashSet<usize>,
    ) -> Result<(), CoreError> {
        fn knot(t: f64, a: Point, b: Point) -> f64 {
            t + 0.0001f64.max(((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt())
        }
        fn mix(a: Point, b: Point, ta: f64, tb: f64, t: f64) -> Point {
            let wa = (tb - t) / (tb - ta);
            let wb = (t - ta) / (tb - ta);
            Point::new(a.x * wa + b.x * wb, a.y * wa + b.y * wb)
        }
        let t0 = 0.0;
        let t1 = knot(t0, before, start);
        let t2 = knot(t1, start, end);
        let t3 = knot(t2, end, after);
        let pieces = 1.max((((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt() / 2.0).ceil() as usize);
        for index in 1..=pieces {
            let t = t1 + (t2 - t1) * index as f64 / pieces as f64;
            let a1 = mix(before, start, t0, t1, t);
            let a2 = mix(start, end, t1, t2, t);
            let a3 = mix(end, after, t2, t3, t);
            let b1 = mix(a1, a2, t0, t2, t);
            let b2 = mix(a2, a3, t1, t3, t);
            let to = if index == pieces { end } else { mix(b1, b2, t1, t2, t) };
            self.walk(to, changed)?;
        }
        Ok(())
    }

    /// One dab of the software path: the tip, stamped into each touched tile's coverage with
    /// `.lighten` (hard tips) or `.screen` (soft tips) (`dab(_:changed:)`).
    fn dab(&mut self, point: Point, changed: &mut HashSet<usize>) -> Result<(), CoreError> {
        let radius = self.settings.diameter / 2.0;
        let circle = Rect::new(point.x - radius, point.y - radius, radius * 2.0, radius * 2.0);
        let clipped = circle.intersection(self.canvas_rect());
        if clipped.is_null() || clipped.is_empty() {
            return Ok(());
        }
        let inverse = self.pixel_to_document.inverted();
        let pixel_canvas = rect_applying(self.canvas_rect(), inverse);
        // Snapped to whole pixels so the tip lands 1:1 with nothing to resample.
        let blit = self.grid_tip.as_ref().map(|tip| {
            let center = inverse.applying(point);
            Rect::new(
                (center.x - tip.width() as f64 / 2.0).round(),
                (center.y - tip.height() as f64 / 2.0).round(),
                tip.width() as f64,
                tip.height() as f64,
            )
        });
        let area = match blit {
            Some(blit) => blit,
            None => rect_applying(clipped, inverse),
        };
        let affected = area.intersection(pixel_canvas).integral().intersection(self.grid_bounds());
        if affected.is_null() || affected.is_empty() {
            return Ok(());
        }
        let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
        let hard = self.settings.hardness >= 1.0;
        for (key, x, y) in affected_tiles(affected, columns) {
            self.allocate_tile(key, x, y)?;
            let touched = self
                .tiles
                .get(&key)
                .map(|tile| affected.intersection(tile.rect).offset_by(-tile.rect.min_x(), -tile.rect.min_y()))
                .unwrap_or(Rect::ZERO);
            if !touched.is_null() && !touched.is_empty() {
                let entry = self.dirty_tiles.entry(key).or_insert(touched);
                *entry = entry.union(touched);
            }
            let Some(tile) = self.tiles.get_mut(&key) else { continue };
            if tile.coverage.is_none() {
                tile.coverage = Some(Gray8Image::new(tile.rect.width() as usize, tile.rect.height() as usize));
            }
            let (min_x, min_y, max_x, max_y) = (
                affected.min_x().max(tile.rect.min_x()) as i64,
                affected.min_y().max(tile.rect.min_y()) as i64,
                affected.max_x().min(tile.rect.max_x()) as i64,
                affected.max_y().min(tile.rect.max_y()) as i64,
            );
            let grid_tip = self.grid_tip.as_ref();
            let stamp = self.stamp.as_ref();
            let hardness = self.settings.hardness;
            let mapping = self.pixel_to_document;
            let canvas = self.canvas;
            let point_x = point.x;
            let point_y = point.y;
            let circle_x = circle.min_x();
            let circle_y = circle.min_y();
            let (circle_w, circle_h) = (circle.width(), circle.height());
            let coverage = tile.coverage.as_mut().expect("just set");
            for py in min_y..max_y {
                for px in min_x..max_x {
                    let p = mapping.applying(Point::new(px as f64 + 0.5, py as f64 + 0.5));
                    if p.x < 0.0 || p.y < 0.0 || p.x >= canvas.width || p.y >= canvas.height {
                        continue;
                    }
                    let source = if let (Some(tip), Some(blit)) = (grid_tip, blit.as_ref()) {
                        let tx = px as f64 - blit.min_x();
                        let ty = py as f64 - blit.min_y();
                        if tx < 0.0 || ty < 0.0 || tx >= tip.width() as f64 || ty >= tip.height() as f64 {
                            0.0
                        } else {
                            tip.get(tx as usize, ty as usize) as f64 / 255.0
                        }
                    } else if let Some(stamp) = stamp {
                        let u = (p.x - circle_x) / circle_w;
                        let v = (p.y - circle_y) / circle_h;
                        sample_gray_bilinear(stamp, u * stamp.width() as f64 - 0.5, v * stamp.height() as f64 - 0.5)
                    } else {
                        let dx = p.x - point_x;
                        let dy = p.y - point_y;
                        falloff_coverage((dx * dx + dy * dy).sqrt(), radius, hardness)
                    };
                    if source <= 0.0 {
                        continue;
                    }
                    let lx = (px - tile.rect.min_x() as i64) as usize;
                    let ly = (py - tile.rect.min_y() as i64) as usize;
                    let existing = coverage.get(lx, ly) as f64 / 255.0;
                    let blended = if hard {
                        existing.max(source)
                    } else {
                        1.0 - (1.0 - existing) * (1.0 - source)
                    };
                    coverage.set(lx, ly, (blended * 255.0).round() as u8);
                }
            }
            changed.insert(key);
        }
        Ok(())
    }

    /// Puts back the coverage the provisional tail touched (`removeTail()`).
    fn remove_tail(&mut self) -> HashSet<usize> {
        let mut restored = HashSet::new();
        let backups = std::mem::take(&mut self.tail_backup);
        for (key, backup) in backups {
            let Some(tile) = self.tiles.get_mut(&key) else { continue };
            match backup {
                Some(image) => tile.coverage = Some(image),
                None => {
                    if let Some(coverage) = tile.coverage.as_mut() {
                        coverage.data_mut().fill(0);
                    }
                }
            }
            let local = Rect::new(0.0, 0.0, tile.rect.width(), tile.rect.height());
            self.dirty_tiles.insert(key, local);
            restored.insert(key);
        }
        restored
    }

    /// Draws a straight tail to the cursor, first saving the coverage it can touch and the dab
    /// spacing state, so [`Self::remove_tail`] can put both back exactly
    /// (`drawTail(from:to:changed:)`).
    fn draw_tail(&mut self, start: Point, end: Point, changed: &mut HashSet<usize>) -> Result<(), CoreError> {
        let reach = self.settings.diameter / 2.0 + 2.0;
        let box_ = Rect::new(
            start.x.min(end.x),
            start.y.min(end.y),
            (end.x - start.x).abs(),
            (end.y - start.y).abs(),
        )
        .inset_by(-reach, -reach)
        .intersection(self.canvas_rect());
        if !box_.is_null() && !box_.is_empty() {
            let affected = rect_applying(box_, self.pixel_to_document.inverted())
                .integral()
                .intersection(self.grid_bounds());
            if !affected.is_null() && !affected.is_empty() {
                let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
                for (key, _, _) in affected_tiles(affected, columns) {
                    let backup = self.tiles.get(&key).and_then(|tile| tile.coverage.clone());
                    self.tail_backup.insert(key, backup);
                }
            }
        }
        let saved = (self.previous, self.distance_to_next);
        self.walk(end, changed)?;
        self.previous = saved.0;
        self.distance_to_next = saved.1;
        Ok(())
    }

    /// Rebuilds each touched tile as original + the stroke's paint through the coverage, preserving
    /// the stroke-wide opacity cap (`publish(_:)`).
    fn publish(&mut self, changed: &HashSet<usize>) -> Result<(), CoreError> {
        self.dirty_document_rect = None;
        let canvas_rect = self.canvas_rect();
        let pixel_to_document = self.pixel_to_document;
        let is_mask = self.is_mask;
        let opacity = self.settings.opacity;
        for key in changed.iter().copied() {
            let Some(tile_rect) = self.tiles.get(&key).map(|tile| tile.rect) else { continue };
            let local = Rect::new(0.0, 0.0, tile_rect.width(), tile_rect.height());
            let has_coverage = self.tiles.get(&key).is_some_and(|tile| tile.coverage.is_some());
            if has_coverage {
                let dirty = self
                    .dirty_tiles
                    .get(&key)
                    .copied()
                    .unwrap_or(local)
                    .integral()
                    .intersection(local);
                self.dirty_tiles.remove(&key);
                if dirty.is_null() || dirty.is_empty() {
                    continue;
                }
                let clone = self.clone.clone();
                let piece = clone
                    .as_ref()
                    .filter(|_| !is_mask || self.is_blur)
                    .and_then(|clone| self.clone_piece(key, tile_rect, clone));
                let healing = self.settings.healing;
                let erasing = self.settings.erasing;
                let mask_gray = self.settings.red;
                let paint = self.paint_color;
                let replaces = self.replaces_with_clone;
                let Some(tile) = self.tiles.get_mut(&key) else { continue };
                let Tile { canvas, base, coverage, .. } = tile;
                let coverage = coverage.as_ref().expect("checked");
                canvas.save();
                canvas.clip_rect(dirty);
                canvas.clear(dirty);
                if let Some(base) = base.as_ref() {
                    Raster::draw(base, local, is_mask, canvas);
                }
                if let Some(selection) = self.selection_clip.as_ref() {
                    canvas.translate(-tile_rect.min_x(), -tile_rect.min_y());
                    canvas.concatenate(pixel_to_document.inverted());
                    apply_selection_clip(canvas, selection);
                    canvas.concatenate(pixel_to_document);
                    canvas.translate(tile_rect.min_x(), tile_rect.min_y());
                }
                if let Some((image, placed)) = piece.as_ref() {
                    let clone = clone.as_ref().expect("the piece came from it");
                    canvas.save();
                    canvas.clip_to_image(coverage, local);
                    canvas.set_alpha(opacity);
                    canvas.set_interpolation_quality(InterpolationQuality::Low);
                    canvas.translate(-tile_rect.min_x(), -tile_rect.min_y());
                    if !clone.in_grid {
                        canvas.concatenate(pixel_to_document.inverted());
                    }
                    canvas.translate(placed.min_x(), placed.max_y());
                    canvas.scale(1.0, -1.0);
                    let rect = Rect::new(0.0, 0.0, placed.width(), placed.height());
                    // `.copy`, when the sample replaces what is under the tip.
                    if replaces {
                        canvas.clear(rect);
                    }
                    match image {
                        PixelImage::Rgba(image) => canvas.draw_image(image, rect),
                        PixelImage::Gray(image) => {
                            // The image's own samples land in a mask: the paint is white.
                            canvas.set_fill_gray(1.0);
                            canvas.draw_gray(image, rect);
                        }
                    }
                    canvas.restore();
                } else if healing && !is_mask {
                    // While painting, the area to heal shows as a dark wash; [`Self::heal`]
                    // rebuilds it from its surroundings when the stroke ends.
                    Raster::fill_color(&PaletteColor::new(0.12, 0.12, 0.12), coverage, local, 0.45, canvas);
                } else if erasing && !is_mask {
                    // Erasing takes the coverage out of the layer's alpha.
                    canvas.save();
                    canvas.clip_to_image(coverage, local);
                    canvas.set_alpha(opacity);
                    canvas.fill_destination_out(local);
                    canvas.restore();
                } else if is_mask {
                    Raster::fill_gray(mask_gray, coverage, local, opacity, canvas);
                } else {
                    Raster::fill_color(&paint, coverage, local, opacity, canvas);
                }
                canvas.restore();
            }
            let Some(tile) = self.tiles.get_mut(&key) else { continue };
            tile.image = Some(if is_mask {
                PatchImage::Gray(Arc::new(tile.canvas.snapshot_gray()))
            } else {
                PatchImage::Rgba(Arc::new(tile.canvas.snapshot()))
            });
            let rect = rect_applying(tile.rect, pixel_to_document).intersection(canvas_rect);
            if !rect.is_null() && !rect.is_empty() {
                self.dirty_document_rect = Some(match self.dirty_document_rect {
                    Some(dirty) => dirty.union(rect),
                    None => rect,
                });
            }
        }
        Ok(())
    }

    /// Allocates a tile with its original content (`allocateTile(_:x:y:)`).
    fn allocate_tile(&mut self, key: usize, x: usize, y: usize) -> Result<(), CoreError> {
        if self.tiles.contains_key(&key) {
            return Ok(());
        }
        let rect = Rect::new(
            (x * TILE_SIZE) as f64,
            (y * TILE_SIZE) as f64,
            TILE_SIZE.min(self.width.saturating_sub(x * TILE_SIZE)) as f64,
            TILE_SIZE.min(self.height.saturating_sub(y * TILE_SIZE)) as f64,
        );
        let next_bounds = match self.allocated_bounds {
            Some(bounds) => bounds.union(rect),
            None => {
                if self.source.is_none() {
                    rect
                } else {
                    self.source_rect.union(rect)
                }
            }
        };
        if next_bounds.width() > MAX_SIDE_EXTENT
            || next_bounds.height() > MAX_SIDE_EXTENT
            || next_bounds.width() * next_bounds.height() > self.pixel_limit as f64
        {
            return Err(too_large());
        }
        self.allocated_bounds = Some(next_bounds);
        let mut canvas = Raster::context(rect.width() as usize, rect.height() as usize, self.is_mask);
        if self.is_mask {
            // A tile past the mask's pixels starts as the mask's background.
            canvas.set_fill_gray(self.mask_background);
            canvas.fill_rect(Rect::new(0.0, 0.0, rect.width(), rect.height()));
        }
        let raster = if self.is_mask {
            self.layer.mask.as_ref().and_then(|mask| mask.asset.raster.as_ref())
        } else {
            self.layer.asset.as_ref().and_then(|asset| asset.raster.as_ref())
        };
        if let Some(raster) = raster {
            draw_raster_into(
                &mut canvas,
                raster,
                self.source_rect.offset_by(-rect.min_x(), -rect.min_y()),
                self.is_mask,
            );
        } else if let Some(source) = self.source.as_ref() {
            // Draw just this tile's share of the source: handing the whole image to every new tile
            // is what made long strokes stutter; cropping first leaves a 256 px blit.
            let initial = self.source_rect.offset_by(-rect.min_x(), -rect.min_y());
            let overlap = rect.intersection(self.source_rect);
            if source.width() > 2 && source.height() > 2 && !overlap.is_null() && !overlap.is_empty() {
                let scale_x = source.width() as f64 / self.source_rect.width();
                let scale_y = source.height() as f64 / self.source_rect.height();
                let crop = Rect::new(
                    (overlap.min_x() - self.source_rect.min_x()) * scale_x,
                    (overlap.min_y() - self.source_rect.min_y()) * scale_y,
                    overlap.width() * scale_x,
                    overlap.height() * scale_y,
                )
                .integral();
                match source.cropped(crop) {
                    Some(cropped) => Raster::draw(
                        &cropped,
                        overlap.offset_by(-rect.min_x(), -rect.min_y()),
                        self.is_mask,
                        &mut canvas,
                    ),
                    None => Raster::draw(source, initial, self.is_mask, &mut canvas),
                }
            } else {
                Raster::draw(source, initial, self.is_mask, &mut canvas);
            }
        }
        let base = if self.is_mask {
            PatchImage::Gray(Arc::new(canvas.snapshot_gray()))
        } else {
            PatchImage::Rgba(Arc::new(canvas.snapshot()))
        };
        self.tiles.insert(
            key,
            Tile {
                rect,
                canvas,
                base: Some(base),
                image: None,
                coverage: None,
                permanent: None,
                preview: None,
            },
        );
        Ok(())
    }

    /// The part of the clone sample under a tile, with a couple of pixels' margin so it's resampled
    /// at its edges just as the whole sample was, and where that part sits. `None` when the sample
    /// doesn't reach the tile (`clonePiece(_:tile:clone:)`).
    fn clone_piece(&mut self, key: usize, tile: Rect, clone: &CloneSample) -> Option<(PixelImage, Rect)> {
        if let Some(piece) = self.clone_pieces.get(&key) {
            return Some(piece.clone());
        }
        let placed = clone.placed;
        if placed.width() <= 0.0 || placed.height() <= 0.0 {
            return None;
        }
        let area = if clone.in_grid { tile } else { rect_applying(tile, self.pixel_to_document) };
        let scale_x = clone.image.width() as f64 / placed.width();
        let scale_y = clone.image.height() as f64 / placed.height();
        let pixels = Rect::new(
            (area.min_x() - placed.min_x()) * scale_x,
            (area.min_y() - placed.min_y()) * scale_y,
            area.width() * scale_x,
            area.height() * scale_y,
        )
        .inset_by(-2.0, -2.0)
        .integral()
        .intersection(Rect::new(0.0, 0.0, clone.image.width() as f64, clone.image.height() as f64));
        if pixels.is_null() || pixels.is_empty() {
            return None;
        }
        let image = match self.clone_render.as_ref().and_then(|render| render(pixels)) {
            Some(image) => image,
            None => clone.image.cropped(pixels)?,
        };
        let piece = (
            image,
            Rect::new(
                placed.min_x() + pixels.min_x() / scale_x,
                placed.min_y() + pixels.min_y() / scale_y,
                pixels.width() / scale_x,
                pixels.height() / scale_y,
            ),
        );
        self.clone_pieces.insert(key, piece.clone());
        Some(piece)
    }

    /// Rebuilds the affected tiles from the original: the selection becomes a transparent hole and
    /// the lifted pixels are placed `offset` document pixels away (`moveLifted(by:duplicate:)`).
    pub fn move_lifted(&mut self, offset: Size, duplicate: bool) -> Result<(), CoreError> {
        let Some(target) = self.lifted_target(offset) else { return Ok(()) };
        let Some(lifted_rect) = self.lifted.as_ref().map(|(_, rect)| *rect) else { return Ok(()) };
        let inverse = self.pixel_to_document.inverted();
        let whole = target.min_x() == target.min_x().round() && target.min_y() == target.min_y().round();
        let needed = lifted_rect
            .union(target)
            .integral()
            .intersection(self.grid_bounds());
        let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
        let mut keys = self.move_tiles.clone();
        if !needed.is_null() && !needed.is_empty() {
            for (key, x, y) in affected_tiles(needed, columns) {
                self.allocate_tile(key, x, y)?;
                keys.insert(key);
            }
        }
        let canvas_rect = self.canvas_rect();
        for key in keys.iter().copied() {
            let (Some(tile), Some(selection), Some((lifted_image, _))) =
                (self.tiles.get_mut(&key), self.selection_clip.as_ref(), self.lifted.as_ref())
            else {
                continue;
            };
            let local = Rect::new(0.0, 0.0, tile.rect.width(), tile.rect.height());
            let (tile_min_x, tile_min_y) = (tile.rect.min_x(), tile.rect.min_y());
            let Tile { canvas, base, .. } = tile;
            canvas.clear(local);
            if let Some(base) = base.as_ref() {
                Raster::draw(base, local, false, canvas);
            }
            canvas.save();
            canvas.translate(-tile_min_x, -tile_min_y);
            canvas.concatenate(inverse);
            apply_selection_clip(canvas, selection);
            if !duplicate {
                canvas.set_alpha(1.0);
                canvas.fill_destination_out(selection.rect);
            }
            canvas.restore();
            canvas.save();
            canvas.set_interpolation_quality(if whole {
                InterpolationQuality::None
            } else {
                InterpolationQuality::High
            });
            canvas.translate(target.min_x() - tile_min_x, target.max_y() - tile_min_y);
            canvas.scale(1.0, -1.0);
            if let PixelImage::Rgba(image) = lifted_image {
                canvas.draw_image(image, Rect::new(0.0, 0.0, target.width(), target.height()));
            }
            canvas.restore();
            tile.image = Some(PatchImage::Rgba(Arc::new(canvas.snapshot())));
        }
        self.move_tiles = keys;
        self.dirty_document_rect = Some(canvas_rect);
        Ok(())
    }

    /// Where the lifted pixels land, in layer pixel coordinates, moved `offset` document pixels
    /// (`liftedTarget(offset:)`).
    pub fn lifted_target(&self, offset: Size) -> Option<Rect> {
        let (_, rect) = self.lifted.as_ref()?;
        let inverse = self.pixel_to_document.inverted();
        let zero = inverse.applying(Point::ZERO);
        let moved = inverse.applying(Point::new(offset.width, offset.height));
        Some(rect.offset_by(moved.x - zero.x, moved.y - zero.y))
    }

    /// Cuts the selected pixels out of the original image. False when nothing is lifted
    /// (`liftSelection()`).
    pub fn lift_selection(&mut self) -> Result<bool, CoreError> {
        let (Some(source), Some(selection)) = (self.source.as_ref(), self.selection_clip.as_ref()) else {
            return Ok(false);
        };
        if self.is_mask || selection.coverage.is_none() {
            return Ok(false);
        }
        let inverse = self.pixel_to_document.inverted();
        let region = rect_applying(selection.rect, inverse).integral().intersection(self.source_rect);
        if region.is_null() || region.width() < 1.0 || region.height() < 1.0 {
            return Ok(false);
        }
        let mut canvas = Raster::context(region.width() as usize, region.height() as usize, false);
        canvas.translate(-region.min_x(), -region.min_y());
        canvas.concatenate(inverse);
        apply_selection_clip(&mut canvas, selection);
        canvas.concatenate(self.pixel_to_document);
        canvas.translate(region.min_x(), region.min_y());
        Raster::draw(
            source,
            self.source_rect.offset_by(-region.min_x(), -region.min_y()),
            false,
            &mut canvas,
        );
        let lifted_image = PatchImage::Rgba(Arc::new(canvas.into_rgba()));
        let mut rest = Raster::context(
            self.source_rect.width() as usize,
            self.source_rect.height() as usize,
            false,
        );
        Raster::draw(
            source,
            Rect::new(0.0, 0.0, self.source_rect.width(), self.source_rect.height()),
            false,
            &mut rest,
        );
        rest.translate(-self.source_rect.min_x(), -self.source_rect.min_y());
        rest.concatenate(inverse);
        apply_selection_clip(&mut rest, selection);
        rest.set_alpha(1.0);
        rest.fill_destination_out(selection.rect);
        let holed_image = PatchImage::Rgba(Arc::new(rest.into_rgba()));
        self.lifted = Some((lifted_image, region));
        self.holed = Some(holed_image);
        Ok(true)
    }

    /// Fills the selection (or the whole canvas) with a solid color (`fill(_:)`).
    pub fn fill(&mut self, color: PaletteColor) -> Result<(), CoreError> {
        let canvas_rect = self.canvas_rect();
        self.paint_canvas(false, |canvas| {
            canvas.set_fill_color(color);
            canvas.fill_rect(canvas_rect);
        })
    }

    /// Replaces this edit with a gradient over the whole canvas (or the selection), composited onto
    /// the original pixels (`fillGradient(_:from:to:colors:opacity:)`).
    pub fn fill_gradient(
        &mut self,
        shape: GradientShape,
        start: Point,
        end: Point,
        colors: &[[f64; 4]],
        opacity: f64,
    ) -> Result<(), CoreError> {
        if colors.len() < 2 {
            return Err(crate::filters::render_failed());
        }
        let gradient = GradientPaint {
            kind: match shape {
                GradientShape::Linear => GradientKind::Linear { from: start, to: end },
                GradientShape::Radial => GradientKind::Radial {
                    center: start,
                    radius: ((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt(),
                },
            },
            stops: vec![(0.0, colors[0]), (1.0, colors[1])],
            extend: GradientExtend::Pad,
        };
        let canvas_rect = self.canvas_rect();
        self.paint_canvas(false, |canvas| {
            canvas.set_alpha(opacity.clamp(0.0, 1.0));
            canvas.fill_gradient(canvas_rect, &gradient);
        })
    }

    /// Erases image pixels to transparency inside the selection, only where pixels exist
    /// (`clearPixels()`).
    pub fn clear_pixels(&mut self) -> Result<(), CoreError> {
        let canvas_rect = self.canvas_rect();
        self.paint_canvas(true, |canvas| {
            canvas.set_alpha(1.0);
            canvas.fill_destination_out(canvas_rect);
        })
    }

    /// Runs `draw` in document coordinates over every tile the canvas and selection cover (only the
    /// layer's existing pixels with `within_source`), clipped to both, starting from each tile's
    /// original content (`paintCanvas(withinSource:_:)`).
    fn paint_canvas(&mut self, within_source: bool, mut draw: impl FnMut(&mut Canvas)) -> Result<(), CoreError> {
        let canvas_rect = self.canvas_rect();
        let mut area = canvas_rect;
        if let Some(selection) = self.selection_clip.as_ref() {
            area = area.intersection(selection.rect);
        }
        if area.is_null() || area.is_empty() {
            return Ok(());
        }
        let inverse = self.pixel_to_document.inverted();
        let mut affected = rect_applying(area, inverse).integral().intersection(self.grid_bounds());
        if within_source {
            affected = affected.intersection(self.source_rect);
        }
        if affected.is_null() || affected.is_empty() {
            return Ok(());
        }
        let columns = (self.width + TILE_SIZE - 1) / TILE_SIZE;
        for (key, x, y) in affected_tiles(affected, columns) {
            self.allocate_tile(key, x, y)?;
            let is_mask = self.is_mask;
            let has_source = self.source.is_some();
            let Some(tile) = self.tiles.get_mut(&key) else { continue };
            let local = Rect::new(0.0, 0.0, tile.rect.width(), tile.rect.height());
            let (tile_min_x, tile_min_y) = (tile.rect.min_x(), tile.rect.min_y());
            let Tile { canvas, base, .. } = tile;
            canvas.clear(local);
            if has_source {
                if let Some(base) = base.as_ref() {
                    Raster::draw(base, local, is_mask, canvas);
                }
            }
            canvas.save();
            canvas.translate(-tile_min_x, -tile_min_y);
            canvas.concatenate(inverse);
            canvas.clip_rect(canvas_rect);
            if let Some(selection) = self.selection_clip.as_ref() {
                apply_selection_clip(canvas, selection);
            }
            draw(canvas);
            canvas.restore();
            tile.image = Some(if is_mask {
                PatchImage::Gray(Arc::new(canvas.snapshot_gray()))
            } else {
                PatchImage::Rgba(Arc::new(canvas.snapshot()))
            });
        }
        self.dirty_document_rect = Some(canvas_rect);
        Ok(())
    }

    /// Spot Healing, once the stroke ends: rebuilds the painted area from nearby texture and writes
    /// it into the stroke's tiles, so the usual commit applies it as one undo step. Reads the
    /// layer's original pixels, never the dark wash shown while painting (`heal()`).
    pub fn heal(&mut self) -> Result<(), CoreError> {
        if !self.settings.healing || self.is_mask {
            return Ok(());
        }
        let mut painted: Option<Rect> = None;
        for tile in self.tiles.values() {
            let Some(coverage) = tile.coverage.as_ref() else { continue };
            let mut edges = [0i64; 4];
            heal_coverage_bounds(
                coverage.data(),
                coverage.width(),
                coverage.height(),
                coverage.stride(),
                &mut edges,
            );
            if edges[2] <= edges[0] || edges[3] <= edges[1] {
                continue;
            }
            let rect = Rect::new(
                edges[0] as f64,
                edges[1] as f64,
                (edges[2] - edges[0]) as f64,
                (edges[3] - edges[1]) as f64,
            )
            .offset_by(tile.rect.min_x(), tile.rect.min_y());
            painted = Some(match painted {
                Some(painted) => painted.union(rect),
                None => rect,
            });
        }
        let Some(painted) = painted else { return Ok(()) };
        // Room for the kernel's patch search, which looks up to about three spot-widths away.
        let reach = (painted.width().max(painted.height()) + 32.0) * 3.2;
        let region = painted
            .inset_by(-reach, -reach)
            .intersection(self.grid_bounds())
            .integral();
        let (width, height) = (region.width() as usize, region.height() as usize);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let mut pixels = Raster::context(width, height, false);
        let placed = self.source_rect.offset_by(-region.min_x(), -region.min_y());
        if let Some(raster) = self.layer.asset.as_ref().and_then(|asset| asset.raster.as_ref()) {
            draw_raster_into(&mut pixels, raster, placed, false);
        } else if let Some(source) = self.source.as_ref() {
            Raster::draw(source, placed, false, &mut pixels);
        }
        let mut painting = Raster::context(width, height, true);
        for tile in self.tiles.values() {
            let Some(coverage) = tile.coverage.as_ref() else { continue };
            Raster::draw(
                &PatchImage::Gray(Arc::new(coverage.clone())),
                tile.rect.offset_by(-region.min_x(), -region.min_y()),
                true,
                &mut painting,
            );
        }
        let mut rgba = pixels.into_rgba();
        let gray = painting.into_gray();
        let mode = SpotHealingMode::ALL
            .iter()
            .position(|mode| *mode == self.settings.healing_mode)
            .unwrap_or(0) as i32;
        let stride = rgba.stride();
        if spot_heal(
            rgba.data_mut(),
            gray.data(),
            width,
            height,
            stride,
            self.settings.opacity as f32,
            mode,
            random_seed(),
        ) != 0
        {
            return Err(too_large());
        }
        let healed = PatchImage::Rgba(Arc::new(rgba));
        let pixel_to_document = self.pixel_to_document;
        let keys: Vec<usize> = self
            .tiles
            .iter()
            .filter(|(_, tile)| tile.coverage.is_some())
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            let (Some(tile), Some(selection)) = (self.tiles.get_mut(&key), self.selection_clip.as_ref()) else {
                continue;
            };
            let local = Rect::new(0.0, 0.0, tile.rect.width(), tile.rect.height());
            let (tile_min_x, tile_min_y) = (tile.rect.min_x(), tile.rect.min_y());
            let Tile { canvas, base, .. } = tile;
            canvas.save();
            canvas.clear(local);
            if let Some(base) = base.as_ref() {
                Raster::draw(base, local, false, canvas);
            }
            canvas.translate(-tile_min_x, -tile_min_y);
            canvas.concatenate(pixel_to_document.inverted());
            apply_selection_clip(canvas, selection);
            canvas.concatenate(pixel_to_document);
            canvas.translate(tile_min_x, tile_min_y);
            Raster::draw(&healed, region.offset_by(-tile_min_x, -tile_min_y), false, canvas);
            canvas.restore();
            tile.image = Some(PatchImage::Rgba(Arc::new(canvas.snapshot())));
        }
        Ok(())
    }

    /// The document's rectangle (`CGRect(origin: .zero, size: canvas)`).
    fn canvas_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.canvas.width, self.canvas.height)
    }

    /// The stroke's grid, `(0, 0, width, height)`.
    fn grid_bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f64, self.height as f64)
    }
}

/// `segment(_:_:)`: a settled (or tail) piece of the continuous stroke, in document coordinates.
fn segment(a: Point, b: Point) -> [f32; 4] {
    [a.x as f32, a.y as f32, b.x as f32, b.y as f32]
}

/// The tiles a rectangle of the stroke's grid covers, as `(key, x, y)`; `columns` is the grid's tile
/// columns. The Swift ranges ran from `Int(min)/tileSize` to `Int(ceil(max) - 1)/tileSize` inclusive.
fn affected_tiles(affected: Rect, columns: usize) -> Vec<(usize, usize, usize)> {
    let mut keys = Vec::new();
    if affected.is_null() || affected.is_empty() {
        return keys;
    }
    let min_x = affected.min_x().max(0.0) as usize;
    let min_y = affected.min_y().max(0.0) as usize;
    let max_x = affected.max_x().ceil().max(0.0) as usize;
    let max_y = affected.max_y().ceil().max(0.0) as usize;
    for y in (min_y / TILE_SIZE)..=(max_y.saturating_sub(1) / TILE_SIZE) {
        for x in (min_x / TILE_SIZE)..=(max_x.saturating_sub(1) / TILE_SIZE) {
            keys.push((y * columns + x, x, y));
        }
    }
    keys
}

/// One pixel's tip coverage: `brushCoverage`'s arithmetic, for the software path's falloff and the
/// tip bitmaps.
fn falloff_coverage(distance: f64, radius: f64, hardness: f64) -> f64 {
    if hardness >= 1.0 {
        return (radius - distance + 0.5).clamp(0.0, 1.0);
    }
    let t = ((distance / radius - hardness) / (1.0 - hardness)).clamp(0.0, 1.0);
    (((-2.5 * t * t).exp() - (-2.5f64).exp()) / (1.0 - (-2.5f64).exp())).max(0.0)
}

/// Bilinear sampling of a gray image at `(x, y)` in pixels, as `interpolationQuality = .low` did;
/// outside the image there is nothing to sample.
fn sample_gray_bilinear(image: &Gray8Image, x: f64, y: f64) -> f64 {
    if image.is_empty() {
        return 0.0;
    }
    let (width, height) = (image.width() as f64, image.height() as f64);
    if x < -1.0 || y < -1.0 || x > width || y > height {
        return 0.0;
    }
    let x0 = x.floor().clamp(0.0, width - 1.0) as usize;
    let y0 = y.floor().clamp(0.0, height - 1.0) as usize;
    let x1 = (x0 + 1).min(image.width() - 1);
    let y1 = (y0 + 1).min(image.height() - 1);
    let fx = (x - x0 as f64).clamp(0.0, 1.0);
    let fy = (y - y0 as f64).clamp(0.0, 1.0);
    let (v00, v10, v01, v11) = (
        image.get(x0, y0) as f64,
        image.get(x1, y0) as f64,
        image.get(x0, y1) as f64,
        image.get(x1, y1) as f64,
    );
    let top = v00 + (v10 - v00) * fx;
    let bottom = v01 + (v11 - v01) * fx;
    ((top + (bottom - top) * fy) / 255.0).clamp(0.0, 1.0)
}

/// `SelectionClip.apply(to:)`: clips to the coverage's region, or clips everything away when the
/// selection is empty.
fn apply_selection_clip(canvas: &mut Canvas, clip: &SelectionClip) {
    match &clip.coverage {
        Some(coverage) if !clip.rect.is_empty() => canvas.clip_to_image(coverage, clip.rect),
        _ => canvas.clip_to_zero(),
    }
}

/// `RasterSnapshot.draw(in:context:)`: the snapshot's base and patches, placed at `dest`.
fn draw_raster_into(canvas: &mut Canvas, raster: &RasterSnapshot, dest: Rect, is_mask: bool) {
    if let Some(base) = raster.base.as_ref() {
        Raster::draw(base, raster.base_rect.offset_by(dest.min_x(), dest.min_y()), is_mask, canvas);
    }
    for patch in &raster.patches {
        Raster::draw(
            &patch.image,
            patch.rect.offset_by(dest.min_x(), dest.min_y()),
            is_mask,
            canvas,
        );
    }
}

/// `UInt32.random(in: .min ... .max)`, for [`spot_heal`]'s patch search.
fn random_seed() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = now
        .wrapping_add((std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed) as u64);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// Clone Stamp, Blur, Smudge and Liquify: an image painted through the tip, and where it sits,
/// already shifted by any source offset. Either in document pixels (a composite of the canvas, at
/// document size) or, `in_grid`, in the stroke's own pixel grid: the layer's own pixels at their own
/// resolution, so a layer scaled down and painted keeps its detail when it's scaled back up.
#[derive(Clone, Debug)]
pub struct CloneSample {
    pub image: PixelImage,
    pub placed: Rect,
    pub in_grid: bool,
}

/// `paintSnapshot()`'s result: the immutable pixels the stroke commits, where they sit and the
/// bounds of the stroke's grid they were taken from.
pub struct PaintSnapshot {
    pub asset: ImportedImage,
    pub transform: LayerTransform,
    pub bounds: Rect,
}

/// `BrushCommit.Input`: the stroke's grid, source pixels and tiles, ready to assemble.
pub struct BrushCommitInput {
    pub width: usize,
    pub height: usize,
    pub source: Option<PixelImage>,
    pub patches: Vec<BrushPatch>,
    pub mask: bool,
    pub name: String,
    pub source_rect: Rect,
    /// A mask's background: what a grown mask is where neither its old pixels nor the edit reach.
    pub fill: f64,
}

/// `BrushCommit.Output`: the assembled pixels and the bounds they actually cover.
pub struct BrushCommitOutput {
    pub asset: ImportedImage,
    pub pixel_bounds: Rect,
}

/// `BrushCommit`: assembles a stroke's tiles into an immutable image, off the pointer's path. The
/// Swift was an actor (`await`ed by the session); its work is synchronous here, as
/// `docs/PORTING.md` §4 requires.
pub struct BrushCommit;

impl BrushCommit {
    /// Grows a mask's pixels to cover a stroke that painted past them, keeping the existing
    /// coverage aligned.
    pub fn expand_mask(
        asset: &ImportedImage,
        input: &BrushCommitInput,
        crop: Rect,
    ) -> Result<ImportedImage, CoreError> {
        if input.source_rect == crop {
            return Ok(asset.clone());
        }
        let bounds = Rect::new(0.0, 0.0, crop.width(), crop.height());
        let mut canvas = Raster::context(crop.width() as usize, crop.height() as usize, true);
        // New canvas area has no pre-existing mask restriction. Existing coverage stays aligned.
        canvas.set_fill_gray(1.0);
        canvas.fill_rect(bounds);
        Raster::draw(
            &asset.image,
            input.source_rect.offset_by(-crop.min_x(), -crop.min_y()),
            true,
            &mut canvas,
        );
        LayerMask::asset(PixelImage::Gray(Arc::new(canvas.into_gray())))
    }

    /// Assembles the stroke: the layer's original pixels (or a grown mask's background), then every
    /// painted tile, cropped to the pixels that are actually there.
    pub fn render(input: &BrushCommitInput) -> Result<BrushCommitOutput, CoreError> {
        let mut canvas = Raster::context(input.width, input.height, input.mask);
        let full_bounds = Rect::new(0.0, 0.0, input.width as f64, input.height as f64);
        if input.mask {
            canvas.set_fill_gray(input.fill);
            canvas.fill_rect(full_bounds);
        }
        if let Some(source) = input.source.as_ref() {
            Raster::draw(source, input.source_rect, input.mask, &mut canvas);
        }
        for patch in &input.patches {
            Raster::draw(&patch.image, patch.rect, input.mask, &mut canvas);
        }
        if input.mask {
            let image = PixelImage::Gray(Arc::new(canvas.into_gray()));
            return Ok(BrushCommitOutput {
                asset: LayerMask::asset(image)?,
                pixel_bounds: full_bounds,
            });
        }
        let full = canvas.into_rgba();
        // Scan once on the commit, never during pointer movement. Keep every
        // nonzero-alpha pixel, including the faint outer edge of a soft brush.
        let edges = brush_alpha_bounds(full.data(), input.width, input.height, full.stride());
        let crop = if edges[2] <= edges[0] {
            full_bounds
        } else {
            Rect::new(
                edges[0] as f64,
                edges[1] as f64,
                (edges[2] - edges[0]) as f64,
                (edges[3] - edges[1]) as f64,
            )
        };
        let image = if crop == full_bounds {
            full
        } else {
            full.cropped(crop).ok_or_else(crate::filters::render_failed)?
        };
        let factor = (96.0 / image.width().max(image.height()) as f64).min(1.0);
        let width = 1.max((image.width() as f64 * factor) as usize);
        let height = 1.max((image.height() as f64 * factor) as usize);
        let mut thumb = Canvas::new_rgba(width, height);
        thumb.draw_image(&image, Rect::new(0.0, 0.0, width as f64, height as f64));
        let thumbnail = thumb.into_rgba();
        Ok(BrushCommitOutput {
            asset: ImportedImage::new(
                PixelImage::Rgba(Arc::new(image)),
                PixelImage::Rgba(Arc::new(thumbnail)),
                input.name.clone(),
            ),
            pixel_bounds: crop,
        })
    }
}

/// `MetalBrushCoverage.Uniforms`, the values `continuousBrush` reads, in the shader's own packing.
#[derive(Clone, Copy, Debug)]
pub struct BrushUniforms {
    /// `a, b, c, d` of the map from tile-local pixels to document pixels.
    pub mapping: [f32; 4],
    /// Document origin of the tile, the tip's radius and its hardness.
    pub geometry: [f32; 4],
    /// Canvas width and height, the antialias width, and the deposition spacing.
    pub canvas: [f32; 4],
    /// Tile width, tile height, committed segment count, total segment count.
    pub counts: [u32; 4],
}

/// `MetalBrushCoverage`'s `continuousBrush` compute kernel, on the CPU: one tile's pixels, the same
/// arithmetic the Metal shader used (`float` stays `f32`). `permanent` is the tile's accumulated
/// paint — the compute kernel's `permanent` buffer, never re-accumulated for a replaceable tail —
/// `preview` receives the 8-bit coverage the tile shows, and `segments` are the settled pieces
/// followed by the tail (`segment.zw - segment.xy` is the piece).
///
/// A stroke's own pixels are independent, so the tile's rows run on `rayon`, the way Metal
/// dispatched a thread per pixel.
pub fn continuous_brush(
    permanent: &mut [f32],
    preview: &mut [u8],
    uniforms: &BrushUniforms,
    segments: &[[f32; 4]],
) {
    let width = uniforms.counts[0] as usize;
    let height = uniforms.counts[1] as usize;
    if width == 0 || height == 0 {
        return;
    }
    let rows = (permanent.len() / width).min(preview.len() / width).min(height);
    let committed = (uniforms.counts[2] as usize).min(segments.len());
    let total = (uniforms.counts[3] as usize).min(segments.len());
    permanent
        .par_chunks_mut(width)
        .zip(preview.par_chunks_mut(width))
        .take(rows)
        .enumerate()
        .for_each(|(y, (permanent_row, preview_row))| {
            for (x, preview_pixel) in preview_row.iter_mut().enumerate() {
                let p = brush_pixel(&uniforms.geometry, &uniforms.mapping, x, y);
                if p[0] < 0.0 || p[1] < 0.0 || p[0] >= uniforms.canvas[0] || p[1] >= uniforms.canvas[1] {
                    *preview_pixel = 0;
                    continue;
                }
                let permanent_pixel = &mut permanent_row[x];
                if uniforms.geometry[3] >= 1.0 {
                    // Hard tips already have a solid interior. Preserve pixel-edge antialiasing.
                    let mut settled = f32::INFINITY;
                    let mut tail = f32::INFINITY;
                    for segment in segments.iter().take(committed) {
                        settled = settled.min(segment_distance_squared(p, *segment));
                    }
                    for segment in segments.iter().take(total).skip(committed) {
                        tail = tail.min(segment_distance_squared(p, *segment));
                    }
                    let value = permanent_pixel.max(brush_coverage(settled, uniforms));
                    *permanent_pixel = value;
                    *preview_pixel = (255.0 * value.max(brush_coverage(tail, uniforms))).round() as u8;
                } else {
                    let mut value = *permanent_pixel;
                    let mut tail = 0.0f32;
                    for segment in segments.iter().take(committed) {
                        value += segment_density(p, *segment, uniforms);
                    }
                    for segment in segments.iter().take(total).skip(committed) {
                        tail += segment_density(p, *segment, uniforms);
                    }
                    *permanent_pixel = value.min(20.0);
                    *preview_pixel = (255.0 * (1.0 - (-(value + tail).min(20.0)).exp())).round() as u8;
                }
            }
        });
}

/// `segmentDistanceSquared` from the shader.
fn segment_distance_squared(p: [f32; 2], segment: [f32; 4]) -> f32 {
    let v = [segment[2] - segment[0], segment[3] - segment[1]];
    let d = [p[0] - segment[0], p[1] - segment[1]];
    let length_squared = (v[0] * v[0] + v[1] * v[1]).max(1e-12);
    let t = ((d[0] * v[0] + d[1] * v[1]) / length_squared).clamp(0.0, 1.0);
    let delta = [d[0] - t * v[0], d[1] - t * v[1]];
    delta[0] * delta[0] + delta[1] * delta[1]
}

/// `brushCoverage` from the shader.
fn brush_coverage(distance_squared: f32, uniforms: &BrushUniforms) -> f32 {
    let distance = distance_squared.sqrt();
    let radius = uniforms.geometry[2];
    if uniforms.geometry[3] >= 1.0 {
        return ((radius - distance) / uniforms.canvas[2] + 0.5).clamp(0.0, 1.0);
    }
    let t = ((distance / radius - uniforms.geometry[3]) / (1.0 - uniforms.geometry[3])).clamp(0.0, 1.0);
    (((-2.5 * t * t).exp() - (-2.5f32).exp()) / (1.0 - (-2.5f32).exp())).max(0.0)
}

/// `tipDensity` from the shader.
fn tip_density(distance_squared: f32, uniforms: &BrushUniforms) -> f32 {
    -(1.0 - brush_coverage(distance_squared, uniforms)).max(0.001).ln()
}

/// `segmentDensity` from the shader: the eight-point Gauss-Legendre quadrature, clipped to the tip's
/// support, so long sparse events and short dense events produce the same paint coverage.
fn segment_density(p: [f32; 2], segment: [f32; 4], uniforms: &BrushUniforms) -> f32 {
    let v = [segment[2] - segment[0], segment[3] - segment[1]];
    let length = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if length < 1e-6 {
        let dx = p[0] - segment[0];
        let dy = p[1] - segment[1];
        return tip_density(dx * dx + dy * dy, uniforms);
    }
    let direction = [v[0] / length, v[1] / length];
    let dx = p[0] - segment[0];
    let dy = p[1] - segment[1];
    let projection = dx * direction[0] + dy * direction[1];
    let perpendicular = [dx - projection * direction[0], dy - projection * direction[1]];
    let perpendicular_squared = perpendicular[0] * perpendicular[0] + perpendicular[1] * perpendicular[1];
    let radius_squared = uniforms.geometry[2] * uniforms.geometry[2];
    if perpendicular_squared >= radius_squared {
        return 0.0;
    }
    let reach = (radius_squared - perpendicular_squared).sqrt();
    let lo = (projection - reach).max(0.0);
    let hi = (projection + reach).min(length);
    if hi <= lo {
        return 0.0;
    }
    let midpoint = (lo + hi) * 0.5;
    let half_length = (hi - lo) * 0.5;
    const NODES: [f32; 4] = [0.1834346425, 0.5255324099, 0.7966664774, 0.9602898565];
    const WEIGHTS: [f32; 4] = [0.3626837834, 0.3137066459, 0.2223810345, 0.1012285363];
    let mut integral = 0.0f32;
    for i in 0..4 {
        let a = midpoint - half_length * NODES[i] - projection;
        let b = midpoint + half_length * NODES[i] - projection;
        integral += WEIGHTS[i]
            * (tip_density(perpendicular_squared + a * a, uniforms)
                + tip_density(perpendicular_squared + b * b, uniforms));
    }
    integral * half_length / uniforms.canvas[3]
}

/// One pixel's `p = geometry.xy + local.x * mapping.xy + local.y * mapping.zw`.
fn brush_pixel(geometry: &[f32; 4], mapping: &[f32; 4], x: usize, y: usize) -> [f32; 2] {
    let local = [x as f32 + 0.5, y as f32 + 0.5];
    [
        geometry[0] + local[0] * mapping[0] + local[1] * mapping[2],
        geometry[1] + local[0] * mapping[1] + local[1] * mapping[3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::Rgba8Image;

    /// A blank layer of `width` × `height`, as the Swift tests' `addBlankLayer()` makes: its pixels
    /// start transparent, so the stroke's own coverage is what the commit shows. The layer and the
    /// document share a size, so document coordinates are the stroke's grid coordinates.
    fn layer(width: usize, height: usize) -> ImageLayer {
        ImageLayer::blank("Layer", Size::new(width as f64, height as f64))
    }

    /// The committed pixel at document `(x, y)`. The tests' layers sit at the origin untransformed,
    /// so a committed pixel is `document - bounds.origin`.
    fn rgba_at(snapshot: &PaintSnapshot, x: i64, y: i64) -> [u8; 4] {
        let point = Point::new(x as f64 + 0.5, y as f64 + 0.5);
        if !snapshot.bounds.contains(point) {
            return [0, 0, 0, 0];
        }
        let Some(image) = snapshot.asset.image.as_rgba() else {
            return [0, 0, 0, 0];
        };
        let ix = (point.x - snapshot.bounds.min_x()) as usize;
        let iy = (point.y - snapshot.bounds.min_y()) as usize;
        if ix >= image.width() || iy >= image.height() {
            return [0, 0, 0, 0];
        }
        image.get(ix, iy)
    }

    fn alpha_at(snapshot: &PaintSnapshot, x: i64, y: i64) -> u8 {
        rgba_at(snapshot, x, y)[3]
    }

    /// The alpha the live stroke shows at `(x, y)`, from its published tiles.
    fn live_alpha(stroke: &BrushStroke, x: i64, y: i64) -> u8 {
        let point = Point::new(x as f64 + 0.5, y as f64 + 0.5);
        for patch in stroke.patches() {
            if patch.rect.contains(point) {
                if let Some(image) = patch.image.as_rgba() {
                    let ix = (point.x - patch.rect.min_x()) as usize;
                    let iy = (point.y - patch.rect.min_y()) as usize;
                    if ix < image.width() && iy < image.height() {
                        return image.get(ix, iy)[3];
                    }
                }
            }
        }
        0
    }

    #[test]
    fn the_kernel_paints_a_hard_tip_and_clears_outside_the_canvas() {
        let uniforms = BrushUniforms {
            mapping: [1.0, 0.0, 0.0, 1.0],
            geometry: [0.0, 0.0, 10.0, 1.0],
            canvas: [4.0, 4.0, 1.0, 1.0],
            counts: [8, 8, 1, 1],
        };
        let segments = [[1.0f32, 1.0, 1.0, 1.0]];
        let mut permanent = vec![0.0f32; 64];
        let mut preview = vec![0u8; 64];
        continuous_brush(&mut permanent, &mut preview, &uniforms, &segments);
        assert_eq!(preview[1 * 8 + 1], 255, "the tip's center is solid");
        assert_eq!(preview[5], 0, "a pixel whose document position is outside the canvas draws nothing");
        // The same call leaves the same preview behind.
        let mut again = vec![0u8; 64];
        continuous_brush(&mut permanent, &mut again, &uniforms, &segments);
        assert_eq!(preview, again);
    }

    #[test]
    fn a_hard_stroke_crosses_tiles_and_commits() {
        let layer = layer(600, 80);
        let settings = BrushSettings {
            diameter: 20.0,
            ..BrushSettings::default()
        };
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(600.0, 80.0), false).expect("a stroke");
        stroke.append(Point::new(50.0, 40.0)).expect("begin");
        stroke.append(Point::new(400.0, 40.0)).expect("continue");
        let columns = (stroke.width + TILE_SIZE - 1) / TILE_SIZE;
        assert!(
            stroke.tiles.keys().any(|key| key % columns >= 1),
            "the stroke reached the second 256-pixel column"
        );
        stroke.flush().expect("flush");
        let snapshot = stroke.paint_snapshot().expect("a snapshot");
        assert!(alpha_at(&snapshot, 400, 40) > 0, "the stroke's end is painted");
        assert!(snapshot.bounds.width() < 600.0, "the commit is trimmed to the painted pixels");
        assert_eq!(alpha_at(&snapshot, 400, 78), 0, "the canvas' edge is untouched");
    }

    #[test]
    fn a_soft_stroke_builds_coverage_while_keeping_its_feathered_rim() {
        let layer = layer(200, 80);
        let settings = BrushSettings {
            diameter: 40.0,
            hardness: 0.0,
            ..BrushSettings::default()
        };
        let mut single = BrushStroke::new(&layer, false, settings.clone(), Size::new(200.0, 80.0), false).expect("a stroke");
        single.append(Point::new(100.0, 40.0)).expect("one dab");
        single.flush().expect("flush");
        let one = alpha_at(&single.paint_snapshot().expect("a snapshot"), 100, 50);
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(200.0, 80.0), false).expect("a stroke");
        stroke.append(Point::new(20.0, 40.0)).expect("begin");
        stroke.append(Point::new(180.0, 40.0)).expect("continue");
        stroke.flush().expect("flush");
        let many = alpha_at(&stroke.paint_snapshot().expect("a snapshot"), 100, 50) as i32;
        assert!(many > one as i32 + 60, "the stroke {many} builds past one dab {one}");
        assert!(many <= 255);
    }

    #[test]
    fn spaced_dabs_leave_no_visible_ripple_along_the_stroke() {
        let layer = layer(900, 300);
        for hardness in [0.0, 0.5, 1.0] {
            let settings = BrushSettings {
                diameter: 120.0,
                hardness,
                ..BrushSettings::default()
            };
            let mut stroke =
                BrushStroke::new_with_gpu(&layer, false, settings, Size::new(900.0, 300.0), false, false).expect("a stroke");
            stroke.append(Point::new(100.0, 150.0)).expect("begin");
            stroke.append(Point::new(800.0, 150.0)).expect("continue");
            stroke.flush().expect("flush");
            let snapshot = stroke.paint_snapshot().expect("a snapshot");
            for offset in [0, 30, 50] {
                let run: Vec<i32> = (300..=600)
                    .map(|x| alpha_at(&snapshot, x, 150 + offset) as i32)
                    .collect();
                let ripple = run.iter().max().unwrap() - run.iter().min().unwrap();
                assert!(ripple <= 16, "hardness {hardness} offset {offset} rippled by {ripple}");
            }
        }
    }

    #[test]
    fn sparse_samples_follow_a_curve_not_straight_chords() {
        let layer = layer(300, 300);
        let settings = BrushSettings {
            diameter: 4.0,
            ..BrushSettings::default()
        };
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(300.0, 300.0), false).expect("a stroke");
        let on_circle = |degrees: f64| {
            let radians = degrees.to_radians();
            Point::new(150.0 + radians.cos() * 100.0, 150.0 + radians.sin() * 100.0)
        };
        stroke.append(on_circle(0.0)).expect("begin");
        for degrees in (30..=180).step_by(30) {
            stroke.append(on_circle(degrees as f64)).expect("continue");
        }
        stroke.flush().expect("flush");
        let snapshot = stroke.paint_snapshot().expect("a snapshot");
        // A straight chord between 30° and 60° passes 3.4 px inside the arc, farther than this 2 px
        // brush reaches; the curve passes through the arc itself.
        for degrees in [45.0, 75.0, 105.0, 135.0] {
            let point = on_circle(degrees);
            assert!(
                alpha_at(&snapshot, point.x as i64, point.y as i64) > 0,
                "the arc at {degrees}°"
            );
        }
    }

    #[test]
    fn a_live_stroke_reaches_the_newest_sample_and_the_tail_leaves_nothing() {
        let layer = layer(300, 120);
        let settings = BrushSettings {
            diameter: 8.0,
            ..BrushSettings::default()
        };
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(300.0, 120.0), false).expect("a stroke");
        stroke.append(Point::new(20.0, 60.0)).expect("begin");
        stroke.append(Point::new(150.0, 20.0)).expect("continue");
        stroke.append(Point::new(280.0, 60.0)).expect("continue");
        // No lag: the provisional tail already reaches the cursor.
        assert_eq!(live_alpha(&stroke, 278, 60), 255);
        stroke.flush().expect("flush");
        let snapshot = stroke.paint_snapshot().expect("a snapshot");
        assert_eq!(alpha_at(&snapshot, 278, 60), 255, "the committed curve ends at the cursor");
        assert_eq!(alpha_at(&snapshot, 215, 40), 0, "the straight chord left nothing behind");
    }

    #[test]
    fn opacity_caps_the_whole_stroke_even_where_it_overlaps_itself() {
        let layer = layer(200, 80);
        let settings = BrushSettings {
            diameter: 40.0,
            red: 1.0,
            opacity: 0.5,
            ..BrushSettings::default()
        };
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(200.0, 80.0), false).expect("a stroke");
        stroke.append(Point::new(20.0, 40.0)).expect("begin");
        for x in [180.0, 20.0, 180.0, 20.0, 100.0] {
            stroke.append(Point::new(x, 40.0)).expect("continue");
        }
        stroke.flush().expect("flush");
        let snapshot = stroke.paint_snapshot().expect("a snapshot");
        let pixel = rgba_at(&snapshot, 100, 40);
        assert!((pixel[3] as i32 - 128).abs() <= 1, "the cap held, alpha {}", pixel[3]);
        assert!((pixel[0] as i32 - 128).abs() <= 1, "the premultiplied paint {}", pixel[0]);
        assert_eq!(pixel[1], 0);
        assert_eq!(alpha_at(&snapshot, 100, 0), 0, "off the stroke nothing was painted");
    }

    #[test]
    fn a_clone_sample_paints_through_the_tip() {
        let layer = layer(200, 80);
        let settings = BrushSettings {
            diameter: 30.0,
            ..BrushSettings::default()
        };
        let mut stroke = BrushStroke::new(&layer, false, settings, Size::new(200.0, 80.0), false).expect("a stroke");
        let sample = Rgba8Image::opaque(200, 80, [0, 255, 0, 255]);
        stroke.clone = Some(CloneSample {
            image: PixelImage::Rgba(Arc::new(sample)),
            placed: Rect::new(0.0, 0.0, 200.0, 80.0),
            in_grid: false,
        });
        stroke.is_blur = true;
        stroke.append(Point::new(100.0, 40.0)).expect("one dab");
        stroke.flush().expect("flush");
        let snapshot = stroke.paint_snapshot().expect("a snapshot");
        let pixel = rgba_at(&snapshot, 100, 40);
        assert!(pixel[1] > 200, "the sample's green shows through the tip, got {pixel:?}");
        assert_eq!(alpha_at(&snapshot, 5, 5), 0, "outside the tip nothing was painted");
    }

    #[test]
    fn the_spacing_rates_follow_the_swift_ones() {
        assert_eq!(BrushStroke::spacing_fraction(1.0), 0.015);
        assert_eq!(BrushStroke::spacing_fraction(0.99), 0.025);
        assert_eq!(BrushStroke::spacing_fraction(0.0), 0.025);
    }
}
