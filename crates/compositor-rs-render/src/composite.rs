//! The canvas compositor: how the document's layers become the canvas.
//!
//! Port of the compositing halves of `Rendering/EditorCanvas.swift` — `drawLayers(_:scale:center:in:)`
//! and `gpuLayers(_:placement:)`/`gpuFrame(_:renderer:size:)` — together with `LiveMaskRenderer`'s
//! clipping stacks and `FolderMaskClip`'s folder masks, under the one CPU path this port has. The
//! Swift had two: Core Graphics drew every layer on the CPU, and the GPU canvas placed textures and
//! blended them through Core Image. Both are reproduced here by [`LayerRenderer`],
//! [`TiledLayerRenderer`] and `compositor_rs_pixels::blend`; `SeparableBlend`'s and `CIImage`'s blend
//! modes are the kernels in `compositor_rs_pixels::blend`, which is why this path needs no read-back
//! surface for them.
//!
//! Order, folder opacity, visibility inheritance, masks and clipping masks are the document's
//! (`compositor_rs_core::groups`); what lives here is the drawing.

// TEMP-PERF: append a timestamped line to one shared timeline file.
fn perf(line: &str) {
    if std::env::var_os("TEMP_PERF").is_none() {
        return;
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("D:/workspace/rust/compositor-rs/target/temp_perf_frames.txt")
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{now} {line}");
    }
}

use compositor_rs_core::blend::LayerBlendMode;
use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::document::{CanvasDocument, ImageLayer};
use compositor_rs_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_adjustment::LayerAdjustment;
use compositor_rs_core::layer_mask::{FolderMaskClip, LayerMask};
use compositor_rs_core::layer_shape::{ShapeDraft, ShapeKind};
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::limits::MAX_SURFACE_PIXELS;
use compositor_rs_core::path::{FillRule, Path};
use compositor_rs_core::path_ops::{LineCap, LineJoin};
use compositor_rs_core::raster::{BrushPatch, RasterSnapshot};
use compositor_rs_core::viewport::CanvasViewport;
use compositor_rs_core::{Gray8Image, Id, Rgba8Image, SharedImage};
use compositor_rs_pixels::brush_pixels::{
    layer_extract_alpha, layer_restore_alpha, layer_unpremultiply_opaque,
};
use compositor_rs_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_rs_pixels::effects::LayerEffectsRenderer;
use compositor_rs_pixels::filters::gaussian_blur;
use compositor_rs_pixels::raster::Raster;
use compositor_rs_pixels::blend;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use crate::adjustment_surface::AdjustmentApply;
use crate::downsample_cache::DownsampleCache;

// The document's shadow is a blurred black rectangle, and the blur is the most expensive thing the
// frame does — a megapixel of it every time the frame is redrawn. Nothing about it depends on the
// frame, though: only on the document's rect, the backing scale and the radius. A window resize or a
// layer drag moves neither, so the sprites are kept and reused until the document itself moves.
//
// Sprite is the *unclipped* blur box: `CIImage(color:).cropped(to: rect).applyingGaussianBlur` blurs
// against transparent surroundings, so the tail is carried past the frame's edge instead of being
// edge-extended to it. A box beyond [`SHADOW_SPRITE_PIXELS`] is cropped to the frame first, which is
// what the port did everywhere — a zoomed-in document's box is far larger than the window.
//
// The document's phase against the pixel grid takes only a few values (a centred view moves it by
// half a pixel at a time), so a handful of sprites covers a whole resize drag.
const SHADOW_SPRITES: usize = 4;
const SHADOW_SPRITE_PIXELS: f64 = 4_000_000.0;

thread_local! {
    static SHADOW_SPRITES_CACHE: std::cell::RefCell<Vec<ShadowSprite>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// What a [`ShadowSprite`] is only valid for. Held as bit patterns: the sprite's bytes have to match
/// the request exactly, and a rounded comparison could reuse one that is a pixel out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ShadowKey {
    width: u64,
    height: u64,
    sigma: u64,
    device: u64,
    phase_x: u64,
    phase_y: u64,
}

struct ShadowSprite {
    key: ShadowKey,
    image: Rgba8Image,
}
use crate::layer_renderer::{canvas_quality, LayerRenderer};
use crate::live_mask_renderer::MaskClip;
use crate::tiled_layer_renderer::TiledLayerRenderer;

/// From 200% (2 screen pixels per document pixel) the canvas shows hard-edged document pixels, as
/// Photoshop does; the pixel grid appears from 800%.
pub const CRISP_ZOOM: f64 = 2.0;
pub const PIXEL_GRID_ZOOM: f64 = 8.0;

/// `Composite` — the composited canvas.
pub enum Composite {}

/// The editor's transient drawing state: everything the Swift's `drawLayers` reads from `session`
/// besides the document itself. A composite of committed state passes [`CompositeState::default`].
#[derive(Default, Clone, Copy)]
pub struct CompositeState<'a> {
    /// `session.maskAloneLayer`: that layer's mask alone, gray across the canvas.
    pub mask_alone: Option<Id>,
    /// `session.brushStroke`, a gradient's or a pixel move's raster: the edit in progress.
    pub stroke: Option<&'a EditStroke<'a>>,
    /// `session.textDraft`: text being typed, drawn as it will be committed.
    pub text_draft: Option<DraftText<'a>>,
    /// `session.shapeDraft`: the shape being dragged out.
    pub shape_draft: Option<&'a ShapeDraft>,
    /// `session.foregroundColor`: the color a shape draft is drawn in.
    pub foreground: PaletteColor,
    /// `session.shapeLineWidth`: the thickness a line draft is drawn with.
    pub shape_line_width: f64,
    /// `session.activeLayerID`: where a shape or text draft is drawn — just above the active layer.
    pub active_layer: Option<Id>,
    /// `session.displayedBlendMode(for:)`.
    pub blend_mode: Option<&'a dyn Fn(Id) -> LayerBlendMode>,
    /// `session.displayedTransform(for:)`.
    pub transform: Option<&'a dyn Fn(Id) -> Option<LayerTransform>>,
    /// `session.displayedMaskPlacement(for:)`.
    pub mask_placement: Option<&'a dyn Fn(Id) -> Option<LayerTransform>>,
    /// `CanvasView.renderBounds`: the document pixels the backdrop covers, in document pixels.
    ///
    /// `nil` outside the crop tool, where the Swift's fallback is the document's own rect; while
    /// cropping it is `document ∪ cropRect`, so the shadow, checkerboard and edge follow a crop
    /// frame dragged past the document.
    pub render_bounds: Option<Rect>,
}

impl<'a> CompositeState<'a> {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A raster edit in progress: `session.brushStroke`, or the raster a gradient or a pixel move fills
/// as it is dragged.
pub struct EditStroke<'a> {
    /// The layer the edit belongs to.
    pub layer: Id,
    /// Painting the layer's mask rather than its pixels.
    pub is_mask: bool,
    /// The grid the tiles are in.
    pub width: usize,
    pub height: usize,
    /// Where the grid sits in the layer's own pixels.
    pub source_rect: Rect,
    /// The tiles painted so far.
    pub patches: &'a [BrushPatch],
    /// Where the grid sits on the document.
    pub paint_transform: LayerTransform,
    /// A mask's value past its old pixels (`stroke.maskBackground`): white reveals, black hides.
    pub mask_background: f64,
}

/// Text being typed (`session.textDraft`'s pixels, drawn as they will be committed).
#[derive(Clone, Copy)]
pub struct DraftText<'a> {
    /// `session.textDraft?.layerID`; nil draws the draft on top of the canvas.
    pub layer: Option<Id>,
    pub image: &'a SharedImage,
    pub transform: LayerTransform,
}

impl Composite {
    /// `drawLayers(_:scale:center:in:)` over the committed document.
    pub fn draw(
        document: &CanvasDocument,
        scale: f64,
        center: &dyn Fn(Point) -> Point,
        canvas: &mut Canvas,
    ) {
        Self::draw_with(document, scale, center, canvas, &CompositeState::default());
    }

    /// `drawLayers(_:scale:center:in:onSurface:)`, with the editor's transient state.
    pub fn draw_with(
        document: &CanvasDocument,
        scale: f64,
        center: &dyn Fn(Point) -> Point,
        canvas: &mut Canvas,
        state: &CompositeState<'_>,
    ) {
        let mut live = LiveComposite::new(document, scale, center, state);
        live.draw_layers(canvas);
    }

    /// The document pixels the view shows, for [`Composite::draw_crisp`]: `view` is in the
    /// viewport's own points.
    pub fn crisp_region(document: &CanvasDocument, viewport: &CanvasViewport, view: Rect) -> Rect {
        let size = document.size();
        let top_left = viewport.document_point(view.origin, size);
        let bottom_right = viewport.document_point(Point::new(view.max_x(), view.max_y()), size);
        Rect::new(
            top_left.x.floor(),
            top_left.y.floor(),
            bottom_right.x.ceil() - top_left.x.floor(),
            bottom_right.y.ceil() - top_left.y.floor(),
        )
        .intersection(Rect::new(0.0, 0.0, size.width, size.height).integral())
    }

    /// `drawDocumentPixels(covering:clippedTo:document:in:)`: composites just the visible document
    /// pixels at 1:1 (the same rendering as export), then enlarges them without smoothing so each
    /// document pixel is a crisp square, even for scaled or rotated layers. Cost is proportional to
    /// what is on screen.
    ///
    /// `region` is in document pixels and `target` in the canvas's own space.
    pub fn draw_crisp(
        document: &CanvasDocument,
        region: Rect,
        target: Rect,
        canvas: &mut Canvas,
        state: &CompositeState<'_>,
    ) {
        if region.is_null() || region.width() < 1.0 || region.height() < 1.0 {
            return;
        }
        let region = Rect::new(region.min_x(), region.min_y(), region.width(), region.height());
        let mut raster = Canvas::new_rgba(region.width() as usize, region.height() as usize);
        let offset = |point: Point| Point::new(point.x - region.min_x(), point.y - region.min_y());
        Self::draw_with(document, 1.0, &offset, &mut raster, state);
        let image = raster.into_rgba();
        canvas.save();
        canvas.set_interpolation_quality(InterpolationQuality::None);
        canvas.draw_image(&image, target);
        canvas.restore();
    }

    /// `drawPixelGrid(in:document:context:)`: one-screen-pixel lines on document pixel boundaries,
    /// over the image only.
    ///
    /// The Swift draws into a view-points context whose CTM carries the backing scale, so its
    /// `1 / backingScale` hairline comes to one device pixel. `canvas` here is already in device
    /// pixels (`draw_view`'s convention), so the view geometry is scaled by `device` and the
    /// hairline is one pixel outright.
    pub fn draw_pixel_grid(
        document: &CanvasDocument,
        viewport: &CanvasViewport,
        device: f64,
        view: Rect,
        canvas: &mut Canvas,
    ) {
        let size = document.size();
        let per_pixel = viewport.points_per_pixel() * device;
        let origin = viewport.document_rect(size).origin;
        let document_rect = Rect::new(
            origin.x * device,
            origin.y * device,
            size.width * per_pixel,
            size.height * per_pixel,
        );
        let area = view.intersection(document_rect);
        if area.is_null() || area.is_empty() {
            return;
        }
        let hairline = 1.0;
        let mut path = Path::empty();
        let mut column = ((area.min_x() - document_rect.min_x()) / per_pixel).ceil() as i64;
        let last_column = ((area.max_x() - document_rect.min_x()) / per_pixel).floor() as i64;
        while column <= last_column {
            let x = document_rect.min_x() + column as f64 * per_pixel;
            path.add_rect(Rect::new(
                x - hairline / 2.0,
                area.min_y(),
                hairline,
                area.height(),
            ));
            column += 1;
        }
        let mut row = ((area.min_y() - document_rect.min_y()) / per_pixel).ceil() as i64;
        let last_row = ((area.max_y() - document_rect.min_y()) / per_pixel).floor() as i64;
        while row <= last_row {
            let y = document_rect.min_y() + row as f64 * per_pixel;
            path.add_rect(Rect::new(
                area.min_x(),
                y - hairline / 2.0,
                area.width(),
                hairline,
            ));
            row += 1;
        }
        canvas.save();
        canvas.set_fill_color(PaletteColor::new(0.55, 0.55, 0.55));
        canvas.set_alpha(0.45);
        canvas.fill_path(&path, FillRule::Winding);
        canvas.restore();
    }

    /// `gpuFrame(_:renderer:size:)`: the whole view as the canvas draws it — the backdrop, the
    /// document's shadow and checkerboard, the layers and the document's edge — in screen pixels,
    /// `size` across, into `target`.
    ///
    /// The Swift's Core Image frame is y-up and is turned over as it is presented; `target` here is
    /// the canonical top-down raster, so the layers are drawn straight into it.
    pub fn draw_view(
        document: &CanvasDocument,
        viewport: &CanvasViewport,
        size: Size,
        target: &mut Rgba8Image,
        state: &CompositeState<'_>,
    ) {
        let device = viewport.backing_scale;
        let document_size = document.size();
        let origin = viewport.document_rect(document_size).origin;
        let per_pixel = viewport.points_per_pixel() * device;
        let full = Rect::new(0.0, 0.0, size.width, size.height);
        // Where the document itself lands: what the layers and the crisp region are mapped with.
        let document_rect = Rect::new(
            origin.x * device,
            origin.y * device,
            document_size.width * per_pixel,
            document_size.height * per_pixel,
        );
        // `renderBounds ?? CGRect(origin: .zero, size: document.size)`: the backdrop, the shadow,
        // the clip and the edge all follow this rect, which the crop tool grows to hold the frame.
        let rect = match state.render_bounds {
            Some(pixels) => {
                let view_origin = viewport.view_point(pixels.origin, document_size);
                Rect::new(
                    view_origin.x * device,
                    view_origin.y * device,
                    pixels.width() * per_pixel,
                    pixels.height() * per_pixel,
                )
            }
            None => document_rect,
        };
        let mut canvas = Canvas::new_rgba(size.width.max(1.0) as usize, size.height.max(1.0) as usize);
        canvas.set_fill_gray(0.105);
        canvas.fill_rect(full);
        // TEMP-PERF
        let mut t_shadow = std::time::Duration::ZERO;
        let mut t_checker = std::time::Duration::ZERO;
        let mut t_layers = std::time::Duration::ZERO;
        if rect.intersects(full) {
            // The document's shadow: a black rectangle three points below the document, blurred.
            // The blur's reach is about three sigma; past that it is nothing, so only that much of
            // the frame is carried.
            let sigma = 7.0 * device;
            let t0 = std::time::Instant::now(); // TEMP-PERF
            let blur_box = rect.inset_by(-3.0 * sigma, -3.0 * sigma);
            // The sprite is rasterized on the destination's own pixel grid and drawn back onto it, so
            // the blit is one source pixel per destination pixel rather than a second resampling. That
            // is what Core Image does — the blur and the composite are in the same space — and it only
            // changes the shadow by the sub-pixel offset the box is rounded out to.
            let area = if blur_box.width() * blur_box.height() <= SHADOW_SPRITE_PIXELS {
                blur_box.integral()
            } else {
                blur_box.intersection(full).integral()
            };
            if !area.is_null() && !area.is_empty() {
                let drawn = rect.offset_by(0.0, 3.0 * device);
                SHADOW_SPRITES_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    let key = ShadowKey {
                        width: area.width().to_bits(),
                        height: area.height().to_bits(),
                        sigma: sigma.to_bits(),
                        device: device.to_bits(),
                        // The sprite's own grid is whole pixels, so it is the shadow box's sub-pixel
                        // offset that decides what its bytes are.
                        phase_x: drawn.min_x().fract().to_bits(),
                        phase_y: drawn.min_y().fract().to_bits(),
                    };
                    if let Some(sprite) = cache.iter().find(|sprite| sprite.key == key) {
                        canvas.draw_image(&sprite.image, area);
                        return;
                    }
                    let t_b = std::time::Instant::now(); // TEMP-PERF
                    let mut shadow = Canvas::new_rgba(area.width() as usize, area.height() as usize);
                    shadow.translate(-area.min_x(), -area.min_y());
                    shadow.set_fill_color(PaletteColor::BLACK);
                    shadow.set_alpha(0.35);
                    shadow.fill_rect(drawn);
                    let t_f = t_b.elapsed(); // TEMP-PERF
                    let blurred =
                        gaussian_blur(&PixelImage::Rgba(Arc::new(shadow.into_rgba())), sigma, true);
                    let t_bl = t_b.elapsed() - t_f; // TEMP-PERF
                    perf(&format!("shadow build fill={t_f:?} blur={t_bl:?}")); // TEMP-PERF
                    let Some(image) = blurred.as_rgba() else { return };
                    cache.insert(
                        0,
                        ShadowSprite {
                            key,
                            image: image.clone(),
                        },
                    );
                    cache.truncate(SHADOW_SPRITES);
                    let t_d = std::time::Instant::now(); // TEMP-PERF
                    canvas.draw_image(&cache[0].image, area); // TEMP-PERF
                    perf(&format!("shadow draw={:?}", t_d.elapsed())); // TEMP-PERF
                });
            }
            t_shadow += t0.elapsed(); // TEMP-PERF
            let t0 = std::time::Instant::now(); // TEMP-PERF
            // Then the document's checkerboard: 10-point squares from its top-left corner.
            let tile = 10.0 * device;
            let visible = rect.intersection(full);
            canvas.save();
            canvas.clip_rect(visible);
            canvas.set_fill_gray(0.30);
            canvas.set_fill_gray(0.30);
            canvas.fill_checkerboard(
                visible,
                Point::new(rect.min_x(), rect.min_y()),
                tile,
                [89, 89, 89, 255],
                [77, 77, 77, 255],
            );
            canvas.restore();
            t_checker += t0.elapsed(); // TEMP-PERF
            let t0 = std::time::Instant::now(); // TEMP-PERF
            canvas.save();
            canvas.clip_rect(visible);
            if viewport.zoom() >= CRISP_ZOOM {
                // From 200% the document's own pixels are composited one to one and enlarged as
                // crisp squares.
                let region = visible_document_region(document_rect, document_size, per_pixel, visible);
                let target = Rect::new(
                    document_rect.min_x() + region.min_x() * per_pixel,
                    document_rect.min_y() + region.min_y() * per_pixel,
                    region.width() * per_pixel,
                    region.height() * per_pixel,
                );
                Self::draw_crisp(document, region, target, &mut canvas, state);
            } else {
                let per_document_pixel = per_pixel;
                let center = |point: Point| {
                    Point::new(
                        document_rect.min_x() + point.x * per_document_pixel,
                        document_rect.min_y() + point.y * per_document_pixel,
                    )
                };
                Self::draw_with(document, per_document_pixel, &center, &mut canvas, state);
            }
            canvas.restore();
            t_layers += t0.elapsed(); // TEMP-PERF
            perf(&format!(
                "draw_view shadow={:?} checker={:?} layers={:?} total_px={}x{}",
                t_shadow,
                t_checker,
                t_layers,
                canvas.width(),
                canvas.height()
            ));
            // The document's edge: a one-pixel line centered on it.
            canvas.save();
            canvas.set_fill_color(PaletteColor::WHITE);
            canvas.set_alpha(0.13);
            canvas.fill_rects(&[
                Rect::new(rect.min_x() - 0.5, rect.min_y() - 0.5, rect.width() + 1.0, 1.0),
                Rect::new(rect.min_x() - 0.5, rect.max_y() - 0.5, rect.width() + 1.0, 1.0),
                Rect::new(rect.min_x() - 0.5, rect.min_y() + 0.5, 1.0, rect.height() - 1.0),
                Rect::new(rect.max_x() - 0.5, rect.min_y() + 0.5, 1.0, rect.height() - 1.0),
            ]);
            canvas.restore();
        }
        *target = canvas.into_rgba();
    }
}

/// The document pixels a view in frame pixels covers: `document_rect` is the document's rect in
/// frame pixels, `view` the visible part of the frame.
fn visible_document_region(
    document_rect: Rect,
    document_size: Size,
    per_pixel: f64,
    view: Rect,
) -> Rect {
    let min_x = ((view.min_x() - document_rect.min_x()) / per_pixel).floor();
    let min_y = ((view.min_y() - document_rect.min_y()) / per_pixel).floor();
    let max_x = ((view.max_x() - document_rect.min_x()) / per_pixel).ceil();
    let max_y = ((view.max_y() - document_rect.min_y()) / per_pixel).ceil();
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
        .intersection(Rect::new(0.0, 0.0, document_size.width, document_size.height).integral())
}

/// The surface's own pixels to the canvas's drawing space: `translate(-bounds.origin)` then
/// `scale(resolution)`, as one transform.
fn surface_placement(bounds: Rect, resolution: f64) -> AffineTransform {
    AffineTransform::translation(-bounds.min_x(), -bounds.min_y()).then(AffineTransform::scale(resolution, resolution))
}

/// `context.boundingBoxOfClipPath`, in the canvas's drawing space.
fn canvas_bounds(canvas: &Canvas) -> Rect {
    let corners = canvas
        .clip_bounds()
        .corners()
        .map(|point| canvas.user_space_to_device().inverted().applying(point));
    let min_x = corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min);
    let max_x = corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max);
    let min_y = corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min);
    let max_y = corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y).standardized()
}

/// One composite's per-draw state: the layer lookup, the clipping stacks, the coverage cache and
/// the placed folder masks — `LiveMaskRenderer` and `FolderMaskClip` folded together, since both
/// walk the same document.
struct LiveComposite<'a> {
    document: &'a CanvasDocument,
    by_id: FxHashMap<Id, &'a ImageLayer>,
    /// `document.effectiveOpacities`: each layer's opacity with its folders' multiplied in.
    opacities: FxHashMap<Id, f64>,
    order: Vec<Id>,
    scale: f64,
    center: &'a dyn Fn(Point) -> Point,
    /// The canvas's visible area, in drawing space.
    bounds: Rect,
    /// Screen pixels per unit of `bounds`.
    resolution: f64,
    state: &'a CompositeState<'a>,
    stacks: FxHashMap<Id, Vec<Id>>,
    stacked: FxHashSet<Id>,
    stack_modes: FxHashMap<Id, LayerBlendMode>,
    covers: FxHashMap<Id, Gray8Image>,
    visiting: FxHashSet<Id>,
    folder_masks: FxHashMap<Id, Option<FolderMaskClip>>,
}

impl<'a> LiveComposite<'a> {
    fn new(
        document: &'a CanvasDocument,
        scale: f64,
        center: &'a dyn Fn(Point) -> Point,
        state: &'a CompositeState<'a>,
    ) -> Self {
        let by_id = document.layers.iter().map(|layer| (layer.id, layer)).collect();
        let opacities = document.effective_opacities();
        let order = document
            .render_layers()
            .into_iter()
            .map(|layer| layer.id)
            .collect();
        Self {
            document,
            by_id,
            opacities,
            order,
            scale,
            center,
            bounds: Rect::ZERO,
            resolution: 1.0,
            state,
            stacks: FxHashMap::default(),
            stacked: FxHashSet::default(),
            stack_modes: FxHashMap::default(),
            covers: FxHashMap::default(),
            visiting: FxHashSet::default(),
            folder_masks: FxHashMap::default(),
        }
    }

    /// The one entry point: everything visible, bottom to top.
    fn draw_layers(&mut self, canvas: &mut Canvas) {
        self.bounds = canvas_bounds(canvas);
        self.resolution = canvas.device_scale();
        if let Some(id) = self.state.mask_alone {
            if self.draw_mask_alone(id, canvas) {
                return;
            }
        }
        self.prepare_stacks();
        let order = self.order.clone();
        let mut drew_new_text = false;
        for id in order {
            self.draw_with_folder_clips(id, canvas);
            // A shape being dragged out previews where its layer will go — above the active layer —
            // rather than over everything, so the layers above it cover it as they will once it is
            // made.
            if self.state.active_layer == Some(id) {
                self.draw_shape_draft(canvas);
                if self.state.text_draft.is_some() {
                    drew_new_text = true;
                }
            }
        }
        if !drew_new_text {
            self.draw_new_text(canvas);
        }
    }

    /// `FolderMaskClip.draw`: draws `id` with every containing folder's mask clipping it.
    fn draw_with_folder_clips(&mut self, id: Id, canvas: &mut Canvas) {
        let mut folders: Vec<FolderMaskClip> = Vec::new();
        let mut folder = self.by_id.get(&id).and_then(|layer| layer.parent_id);
        let mut depth = 0;
        while let Some(current) = folder {
            if depth >= 64 {
                break;
            }
            if let Some(clip) = self.folder_mask(current) {
                folders.push(clip);
            }
            folder = self.by_id.get(&current).and_then(|layer| layer.parent_id);
            depth += 1;
        }
        if folders.is_empty() {
            self.draw_composite(id, canvas);
            return;
        }
        canvas.save();
        for clip in &folders {
            self.apply_mask_clip(clip, canvas);
        }
        self.draw_composite(id, canvas);
        canvas.restore();
    }

    /// `LiveMaskRenderer.drawComposite`: a clipping stack's base is drawn with its clipped children
    /// in one surface, sharing the base's alpha; the stack blends in its base's mode.
    fn draw_composite(&mut self, id: Id, canvas: &mut Canvas) {
        if self.stacked.contains(&id) {
            return;
        }
        if self.adjustment(id).is_some() {
            if self.source(id).is_none() {
                self.adjust(id, canvas);
            }
            return;
        }
        let children = self.stacks.get(&id).cloned();
        let Some((width, height)) = self.pixel_size() else {
            if let Some(children) = children {
                for child in children {
                    self.stacked.remove(&child);
                }
            }
            self.draw(id, canvas);
            return;
        };
        let Some(children) = children else {
            self.draw(id, canvas);
            return;
        };
        let mut group = Canvas::new_rgba(width, height);
        group.concatenate(surface_placement(self.bounds, self.resolution));
        self.draw_own(id, &mut group);
        let mut pixels = group.into_rgba();
        let mut coverage = Gray8Image::new(width, height);
        let (pixels_stride, coverage_stride) = (pixels.stride(), coverage.stride());
        layer_extract_alpha(
            pixels.data(),
            pixels_stride,
            coverage.data_mut(),
            coverage_stride,
            width,
            height,
        );
        layer_unpremultiply_opaque(pixels.data_mut(), pixels_stride, width, height);
        let mut group = Canvas::from_rgba(pixels);
        group.concatenate(surface_placement(self.bounds, self.resolution));
        for child in &children {
            if self.adjustment(*child).is_some() {
                self.adjust(*child, &mut group);
            } else {
                self.draw_own(*child, &mut group);
            }
        }
        let mut placed = group.into_rgba();
        let placed_stride = placed.stride();
        layer_restore_alpha(
            placed.data_mut(),
            placed_stride,
            coverage.data(),
            coverage_stride,
            width,
            height,
        );
        let bounds = self.bounds;
        canvas.save();
        canvas.set_blend_mode(self.stack_modes.get(&id).copied().unwrap_or(LayerBlendMode::Normal));
        canvas.draw_image(&placed, bounds);
        canvas.restore();
    }

    /// `LiveMaskRenderer.draw`: clipped by the layer whose alpha this one shares, if any.
    fn draw(&mut self, id: Id, canvas: &mut Canvas) {
        canvas.save();
        if let Some(source) = self.source(id) {
            let Some(coverage) = self.coverage(source) else {
                canvas.restore();
                return;
            };
            canvas.clip_to_image(&coverage, self.bounds);
        }
        self.draw_own(id, canvas);
        canvas.restore();
    }

    /// `LiveMaskRenderer.coverage`: the layer's own alpha, including its own masks, drawn on its own
    /// and independent of whether it shows. Cached for the composite; a chain is followed at most
    /// 256 deep.
    fn coverage(&mut self, id: Id) -> Option<Gray8Image> {
        if let Some(known) = self.covers.get(&id) {
            return Some(known.clone());
        }
        if self.visiting.contains(&id) || self.visiting.len() >= 256 {
            return None;
        }
        let (width, height) = self.pixel_size()?;
        self.visiting.insert(id);
        let mut surface = Canvas::new_rgba(width, height);
        surface.scale(self.resolution, self.resolution);
        surface.translate(-self.bounds.min_x(), -self.bounds.min_y());
        self.draw(id, &mut surface);
        self.visiting.remove(&id);
        let pixels = surface.into_rgba();
        let mut coverage = Gray8Image::new(width, height);
        let (pixels_stride, coverage_stride) = (pixels.stride(), coverage.stride());
        layer_extract_alpha(
            pixels.data(),
            pixels_stride,
            coverage.data_mut(),
            coverage_stride,
            width,
            height,
        );
        self.covers.insert(id, coverage.clone());
        Some(coverage)
    }

    /// `LiveMaskRenderer.adjust`: the adjustment runs on the surface's pixels, at screen resolution,
    /// blended in the layer's mode and at its opacity, then drawn through the adjustment's clip.
    ///
    /// Note: `LayerAdjustment::apply` is the model's; the composite supplies the region (so Grain's
    /// and Add Noise's patterns stay with the document) and the scale.
    fn adjust(&mut self, id: Id, canvas: &mut Canvas) {
        let Some(settings) = self
            .by_id
            .get(&id)
            .and_then(|layer| layer.adjustment.as_ref())
            .cloned()
        else {
            return;
        };
        let original = canvas.snapshot();
        let region = self.adjustment_region(self.bounds);
        let Some(mut adjusted) = settings.apply(&original, Some(region), self.adjustment_scale()).ok()
        else {
            return;
        };
        if self.blend(id) != LayerBlendMode::Normal {
            // Blend colors at full coverage, then restore the original alpha.
            // Source-over of two translucent copies would thicken soft edges.
            let (width, height) = (original.width(), original.height());
            let mut alpha = Gray8Image::new(width, height);
            let mut base = original.clone();
            let mut top = adjusted.clone();
            let (base_stride, alpha_stride) = (base.stride(), alpha.stride());
            layer_extract_alpha(
                base.data(),
                base_stride,
                alpha.data_mut(),
                alpha_stride,
                width,
                height,
            );
            layer_unpremultiply_opaque(base.data_mut(), base_stride, width, height);
            let top_stride = top.stride();
            layer_unpremultiply_opaque(top.data_mut(), top_stride, width, height);
            blend::blend_over(self.blend(id), &mut base, &top, 1.0);
            layer_restore_alpha(
                base.data_mut(),
                base_stride,
                alpha.data(),
                alpha_stride,
                width,
                height,
            );
            adjusted = base;
        }
        let opacity = self.opacity(id);
        if opacity < 1.0 {
            // `CIBlendWithMask` against the original at a constant opacity.
            for (pixel, source) in adjusted
                .data_mut()
                .chunks_exact_mut(4)
                .zip(original.data().chunks_exact(4))
            {
                for channel in 0..4 {
                    pixel[channel] = (pixel[channel] as f64 * opacity
                        + source[channel] as f64 * (1.0 - opacity))
                        .round()
                        .clamp(0.0, 255.0) as u8;
                }
            }
        }
        let bounds = self.bounds;
        canvas.save();
        self.adjustment_clip(id, canvas);
        Raster::draw(&PixelImage::Rgba(Arc::new(adjusted)), bounds, false, canvas);
        canvas.restore();
    }

    /// `drawOwn`: one layer's pixels, through its mask and at its opacity — text being edited, a
    /// stroke in progress, its effects, or its asset.
    fn draw_own(&mut self, id: Id, canvas: &mut Canvas) {
        let Some(layer) = self.by_id.get(&id).copied() else {
            return;
        };
        let opacity = self.opacity(id);
        // Text being edited draws as it will be committed, in its place among the layers.
        if let Some(draft) = self.state.text_draft {
            if draft.layer == Some(id) {
                let transform = draft.transform;
                LayerRenderer::draw(
                    &PixelImage::Rgba(draft.image.clone()),
                    &transform,
                    (self.center)(transform.center()),
                    self.scale,
                    opacity,
                    self.blend(id),
                    None,
                    canvas,
                );
                return;
            }
        }
        // A raster edit in progress: the layer as the stroke leaves it.
        let stroke = self.state.stroke.filter(|stroke| stroke.layer == id);
        if let Some(stroke) = stroke {
            self.draw_stroke(layer, stroke, canvas);
            return;
        }
        // A stroke and drop shadow are drawn around the layer's pixels, on a canvas grown to hold
        // them.
        if let Some((image, inset)) = self.effects(layer) {
            let transform = self.transform(id).unwrap_or(layer.transform);
            let grown = LayerEffectsRenderer::placed(&transform, &image, inset);
            LayerRenderer::draw(
                &PixelImage::Rgba(Arc::new(image)),
                &grown,
                (self.center)(grown.center()),
                self.scale,
                opacity,
                self.blend(id),
                None,
                canvas,
            );
            return;
        }
        let transform = self.transform(id).unwrap_or(layer.transform);
        let mask = self.layer_mask(layer, &transform, canvas);
        let Some(asset) = layer.asset.as_ref() else {
            return;
        };
        if let Some(raster) = asset.raster.as_ref() {
            TiledLayerRenderer::draw_raster(
                raster,
                &transform,
                (self.center)(transform.center()),
                self.scale,
                opacity,
                self.blend(id),
                mask.as_ref(),
                canvas,
            );
        } else {
            LayerRenderer::draw(
                &asset.image,
                &transform,
                (self.center)(transform.center()),
                self.scale,
                opacity,
                self.blend(id),
                mask.as_ref(),
                canvas,
            );
        }
    }

    /// A raster edit drawn as the finished layer will look, through the layer's own mask where its
    /// old pixels were (`TiledLayerRenderer.drawStroke`/`drawMaskStroke`).
    fn draw_stroke(&mut self, layer: &ImageLayer, stroke: &EditStroke<'_>, canvas: &mut Canvas) {
        let id = layer.id;
        let opacity = self.opacity(id);
        let placed_apart = stroke.is_mask
            && layer
                .mask
                .as_ref()
                .map(|mask| mask.placement.is_some())
                .unwrap_or(false);
        // A mask on its own placement paints in its own grid; the layer draws where it is.
        let transform = if placed_apart {
            self.transform(id).unwrap_or(layer.transform)
        } else {
            stroke.paint_transform
        };
        let center = (self.center)(transform.center());
        let previous = layer.asset.as_ref();
        let image = previous.and_then(|asset| asset.raster.is_none().then_some(&asset.image));
        let raster: Option<&RasterSnapshot> = previous.and_then(|asset| asset.raster.as_ref());
        if stroke.is_mask {
            TiledLayerRenderer::draw_mask_stroke(
                stroke.width,
                stroke.height,
                stroke.source_rect,
                stroke.patches,
                layer.mask.as_ref().map(|mask| &mask.asset),
                image,
                raster,
                &transform,
                center,
                self.scale,
                opacity,
                self.blend(id),
                canvas,
            );
            return;
        }
        let mask = self.layer_mask(layer, &transform, canvas);
        TiledLayerRenderer::draw_stroke(
            stroke.width,
            stroke.height,
            stroke.source_rect,
            stroke.patches,
            image,
            raster,
            &transform,
            center,
            self.scale,
            opacity,
            self.blend(id),
            mask.as_ref(),
            canvas,
        );
    }

    /// `drawMaskAlone`: the mask as Photoshop shows it after an Option-click — grayscale across the
    /// whole canvas, white revealing and black hiding, with what a stroke has painted into it so
    /// far. Past its pixels, a mask is its edge tone.
    fn draw_mask_alone(&mut self, id: Id, canvas: &mut Canvas) -> bool {
        let Some(layer) = self.by_id.get(&id).copied() else {
            return false;
        };
        let Some(mask) = layer.mask.as_ref() else {
            return false;
        };
        let size = self.document.size();
        let origin = (self.center)(Point::ZERO);
        let background = mask
            .asset
            .thumbnail
            .as_gray()
            .map(LayerMask::background)
            .unwrap_or(1.0);
        canvas.save();
        canvas.set_fill_gray(background);
        canvas.fill_rect(Rect::new(
            origin.x,
            origin.y,
            size.width * self.scale,
            size.height * self.scale,
        ));
        canvas.restore();
        if let Some(stroke) = self
            .state
            .stroke
            .filter(|stroke| stroke.is_mask && stroke.layer == id)
        {
            let placed = stroke.paint_transform;
            LayerRenderer::draw_brush_preview(
                Some(&mask.asset.image),
                &placed,
                (self.center)(placed.center()),
                self.scale,
                1.0,
                LayerBlendMode::Normal,
                None,
                stroke.patches,
                stroke.width,
                stroke.height,
                false,
                Some(stroke.source_rect),
                None,
                None,
                canvas,
            );
            return true;
        }
        // Where it shows while a transform is being dragged, as in the composite.
        let placed = self
            .mask_placement(id)
            .unwrap_or(layer.transform);
        LayerRenderer::draw(
            &mask.asset.image,
            &placed,
            (self.center)(placed.center()),
            self.scale,
            1.0,
            LayerBlendMode::Normal,
            None,
            canvas,
        );
        true
    }

    /// The shape being dragged out with the Shape tool, drawn in the color it will be made in.
    fn draw_shape_draft(&mut self, canvas: &mut Canvas) {
        let Some(draft) = self.state.shape_draft else {
            return;
        };
        // A flat or upright line has a box with no height or width, which is not "empty" for this
        // purpose.
        let valid = if draft.kind == ShapeKind::Line {
            draft.rect.width() > 0.0 || draft.rect.height() > 0.0
        } else {
            !draft.rect.is_empty()
        };
        if !valid {
            return;
        }
        let middle = (self.center)(Point::new(draft.rect.mid_x(), draft.rect.mid_y()));
        let rect = Rect::new(
            middle.x - draft.rect.width() * self.scale / 2.0,
            middle.y - draft.rect.height() * self.scale / 2.0,
            draft.rect.width() * self.scale,
            draft.rect.height() * self.scale,
        );
        canvas.save();
        canvas.set_fill_color(self.state.foreground);
        canvas.set_alpha(1.0);
        if draft.kind == ShapeKind::Line {
            let Some(end) = draft.end else {
                canvas.restore();
                return;
            };
            let thickness = (self.state.shape_line_width * self.scale).max(1.0);
            let mut path = Path::empty();
            path.move_to((self.center)(draft.anchor));
            path.add_line((self.center)(end));
            canvas.stroke_path(
                &path,
                FillRule::Winding,
                thickness,
                LineCap::Round,
                LineJoin::Round,
                10.0,
            );
        } else {
            let path = draft.kind.path(rect, draft.corner_radius * self.scale);
            canvas.fill_path(&path, FillRule::Winding);
        }
        canvas.restore();
    }

    /// New text, on top of everything when its layer isn't drawn.
    fn draw_new_text(&mut self, canvas: &mut Canvas) {
        let Some(draft) = self.state.text_draft else {
            return;
        };
        if draft.layer.is_some() {
            return;
        }
        let transform = draft.transform;
        LayerRenderer::draw(
            &PixelImage::Rgba(draft.image.clone()),
            &transform,
            (self.center)(transform.center()),
            self.scale,
            1.0,
            LayerBlendMode::Normal,
            None,
            canvas,
        );
    }

    /// The layer's effects for the pixels being composited, reusing the last result for the same
    /// pixels, mask and settings (`LayerEffectsRenderer.cached`). The canvas's asynchronous preview
    /// cache is the session's; a composite always renders what it is compositing.
    fn effects(&mut self, layer: &ImageLayer) -> Option<(Rgba8Image, f64)> {
        let pixels = layer.asset.as_ref()?.image.as_rgba()?;
        let mask = self.placed_mask(layer, &layer.transform);
        let rendered = LayerEffectsRenderer::cached(pixels, mask.as_ref().and_then(PixelImage::as_gray), layer.effects.as_ref())?;
        Some((rendered.image, rendered.inset))
    }

    /// The mask as the layer's renderers take it: an image stretched over the layer's pixel grid.
    /// Nil while disabled.
    fn layer_mask(
        &mut self,
        layer: &ImageLayer,
        transform: &LayerTransform,
        canvas: &Canvas,
    ) -> Option<PixelImage> {
        let owned = layer.mask.as_ref()?;
        if !owned.is_enabled {
            return None;
        }
        let Some(placement) = self.mask_placement(layer.id) else {
            return owned.enabled_image().cloned();
        };
        let width = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.width())
            .unwrap_or(transform.size.width.round() as usize)
            .max(1);
        let height = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.height())
            .unwrap_or(transform.size.height.round() as usize)
            .max(1);
        let drawn = transform.size.width.max(transform.size.height) * self.scale * canvas.device_scale();
        let steady = 2f64.powf(drawn.max(64.0).log2().ceil());
        owned.clip_image(Some(&placement), transform, width, height, Some(steady))
    }

    /// The mask placed into the layer's own grid, for the effects renderer.
    fn placed_mask(&mut self, layer: &ImageLayer, transform: &LayerTransform) -> Option<PixelImage> {
        let owned = layer.mask.as_ref()?;
        if !owned.is_enabled {
            return None;
        }
        let Some(placement) = self.mask_placement(layer.id) else {
            return owned.enabled_image().cloned();
        };
        let width = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.width())
            .unwrap_or(transform.size.width.round() as usize)
            .max(1);
        let height = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.height())
            .unwrap_or(transform.size.height.round() as usize)
            .max(1);
        owned.clip_image(Some(&placement), transform, width, height, Some(2048.0))
    }

    /// `LiveMaskRenderer.adjustmentClip`: the adjustment's own mask, where it has an enabled one.
    fn adjustment_clip(&mut self, id: Id, canvas: &mut Canvas) {
        let Some(layer) = self.by_id.get(&id).copied() else {
            return;
        };
        let Some(mask) = layer.mask.as_ref() else {
            return;
        };
        if !mask.is_enabled {
            return;
        }
        let Some(image) = mask.enabled_image().and_then(PixelImage::as_gray) else {
            return;
        };
        let transform = self.transform(id).unwrap_or(layer.transform);
        let clip = FolderMaskClip {
            image: Arc::new(image.clone()),
            transform,
        };
        self.apply_mask_clip(&clip, canvas);
    }

    /// `FolderMaskClip.apply`: the mask stretched over the folder's box, clipping what is inside it
    /// and leaving the canvas's transform as it found it.
    fn apply_mask_clip(&self, clip: &FolderMaskClip, canvas: &mut Canvas) {
        let (placement, inverse) = clip.placement(self.scale, (self.center)(clip.transform.center()));
        canvas.set_interpolation_quality(canvas_quality(clip.transform.sampling.quality()));
        canvas.concatenate(placement);
        canvas.clip_to_image(&clip.image, clip.rect(self.scale));
        canvas.concatenate(inverse);
    }

    /// A folder's mask, placed once each — `FolderMaskClip.draw`'s `clip(id)`.
    fn folder_mask(&mut self, id: Id) -> Option<FolderMaskClip> {
        if let Some(known) = self.folder_masks.get(&id) {
            return known.clone();
        }
        let placed = self.by_id.get(&id).and_then(|folder| {
            let mask = folder.mask.as_ref()?;
            if !mask.is_enabled {
                return None;
            }
            let transform = self.transform(id).unwrap_or(folder.transform);
            let source = mask.enabled_image()?;
            let reduced = DownsampleCache::shared()
                .image(source, transform.size.width * self.scale * self.resolution);
            Some(FolderMaskClip {
                image: Arc::new(reduced.as_gray()?.clone()),
                transform,
            })
        });
        self.folder_masks.insert(id, placed.clone());
        placed
    }

    /// `LiveMaskRenderer.prepareStacks`: a clipping stack is a base layer followed by the layers
    /// that name it as their mask source in the same folder, and the stack shares the base's alpha.
    fn prepare_stacks(&mut self) {
        let ids = self.order.clone();
        for (index, base) in ids.iter().enumerate() {
            if self.source(*base).is_some() || self.adjustment(*base).is_some() {
                continue;
            }
            let mut children = Vec::new();
            for child in ids.iter().skip(index + 1) {
                if self.source(*child) != Some(*base) {
                    break;
                }
                let same_parent = self.by_id.get(child).and_then(|layer| layer.parent_id)
                    == self.by_id.get(base).and_then(|layer| layer.parent_id);
                if !same_parent {
                    break;
                }
                children.push(*child);
            }
            if children.is_empty() {
                continue;
            }
            self.stacks.insert(*base, children.clone());
            self.stack_modes.insert(*base, self.blend(*base));
            for child in children {
                self.stacked.insert(child);
            }
        }
    }

    fn pixel_size(&self) -> Option<(usize, usize)> {
        let width = (self.bounds.width() * self.resolution).round();
        let height = (self.bounds.height() * self.resolution).round();
        if !(width >= 1.0) || !(height >= 1.0) {
            return None;
        }
        if width * height > MAX_SURFACE_PIXELS as f64 {
            return None;
        }
        Some((width as usize, height as usize))
    }

    fn source(&self, id: Id) -> Option<Id> {
        self.by_id.get(&id).and_then(|layer| layer.mask_source_id)
    }

    fn adjustment(&self, id: Id) -> Option<&LayerAdjustment> {
        self.by_id.get(&id).and_then(|layer| layer.adjustment.as_ref())
    }

    fn blend(&self, id: Id) -> LayerBlendMode {
        if let Some(blend) = self.state.blend_mode {
            return blend(id);
        }
        self.by_id
            .get(&id)
            .map(|layer| layer.blend_mode)
            .unwrap_or(LayerBlendMode::Normal)
    }

    fn opacity(&self, id: Id) -> f64 {
        self.opacities.get(&id).copied().unwrap_or(1.0)
    }

    fn adjustment_scale(&self) -> f64 {
        self.scale * self.resolution
    }

    fn transform(&self, id: Id) -> Option<LayerTransform> {
        self.state.transform.and_then(|transform| transform(id))
    }

    fn mask_placement(&self, id: Id) -> Option<LayerTransform> {
        self.state.mask_placement.and_then(|placement| placement(id))
    }

    /// The part of the document the surface covers, so Grain's and Add Noise's patterns stay with
    /// the document.
    fn adjustment_region(&self, rect: Rect) -> Rect {
        let corner = (self.center)(Point::ZERO);
        Rect::new(
            (rect.min_x() - corner.x) / self.scale,
            (rect.min_y() - corner.y) / self.scale,
            rect.width() / self.scale,
            rect.height() / self.scale,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_rs_core::document::CanvasDocument;
    use compositor_rs_core::geom::Size;
    use compositor_rs_core::imported_image::ImportedImage;

    fn layer(id: Id, image: PixelImage, origin: Point, size: Size) -> ImageLayer {
        let asset = ImportedImage::new(image.clone(), image, "Layer");
        let mut layer = ImageLayer::from_asset(asset, origin);
        layer.id = id;
        layer.transform.size = size;
        layer
    }

    fn solid(width: usize, height: usize, color: [u8; 4]) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, color);
            }
        }
        image
    }

    /// Black pixels with the given alphas, top row first — a layer that only supplies coverage.
    fn black_with_alpha(width: usize, height: usize, alphas: &[u8]) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for (index, alpha) in alphas.iter().enumerate() {
            image.set(index % width, index / width, [0, 0, 0, *alpha]);
        }
        image
    }

    fn mask(values: &[u8], width: usize, height: usize) -> LayerMask {
        let mut gray = Gray8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                gray.set(x, y, values[y * width + x]);
            }
        }
        let gray = PixelImage::Gray(Arc::new(gray));
        LayerMask::with_placement(
            ImportedImage::new(gray.clone(), gray, "Layer Mask"),
            true,
            None,
            true,
        )
    }

    fn document_with(layers: Vec<ImageLayer>, size: Size) -> CanvasDocument {
        let mut document = CanvasDocument::new(size.width as usize, size.height as usize);
        document.layers = layers;
        document
    }

    fn identity(point: Point) -> Point {
        point
    }

    /// Layers composite bottom to top: the top layer's pixels win where it is opaque.
    #[test]
    fn layers_draw_bottom_to_top() {
        let bottom = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [0, 0, 255, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        let top = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(1, 1, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(1.0, 1.0),
        );
        let document = document_with(vec![bottom, top], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 255, 255]);
        assert_eq!(result.get(0, 1), [0, 0, 255, 255]);
        assert_eq!(result.get(1, 1), [0, 0, 255, 255]);
    }

    /// A folder's opacity multiplies into everything inside it: a layer at 50% in a folder at 50%
    /// shows at 25%, while the layer itself still reads 50%.
    #[test]
    fn folder_opacity_multiplies_into_descendants() {
        let mut folder = ImageLayer::blank("Folder", Size::new(2.0, 2.0));
        folder.is_group = true;
        folder.opacity = 0.5;
        folder.id = compositor_rs_core::new_id();
        let mut child = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [0, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        child.parent_id = Some(folder.id);
        child.opacity = 0.5;
        let document = document_with(vec![folder, child], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0)[3], 64, "50% × 50% = 25% coverage");
    }

    /// A folder that is hidden hides what is inside it, without changing the child's own flag.
    #[test]
    fn a_hidden_folder_hides_its_children() {
        let mut folder = ImageLayer::blank("Folder", Size::new(2.0, 2.0));
        folder.is_group = true;
        folder.is_visible = false;
        folder.id = compositor_rs_core::new_id();
        let mut child = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [0, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        child.parent_id = Some(folder.id);
        assert!(child.is_visible);
        let document = document_with(vec![folder, child], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        assert_eq!(canvas.into_rgba().get(0, 0), [0, 0, 0, 0]);
    }

    /// A layer's mask multiplies its coverage: half a mask hides half the layer.
    #[test]
    fn the_layer_mask_multiplies_coverage() {
        let id = compositor_rs_core::new_id();
        let mut layer = layer(
            id,
            PixelImage::Rgba(Arc::new(solid(2, 1, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 1.0),
        );
        layer.mask = Some(mask(&[255, 0], 2, 1));
        let document = document_with(vec![layer], Size::new(2.0, 1.0));
        let mut canvas = Canvas::new_rgba(2, 1);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 0, 0]);
    }

    /// A clipping mask limits a layer to the coverage of the layer below it (`maskSourceID`).
    #[test]
    fn clipping_masks_limit_a_layer_to_its_base() {
        let base = compositor_rs_core::new_id();
        let mut base_layer = layer(
            base,
            PixelImage::Rgba(Arc::new(solid(2, 1, [0, 0, 255, 255]))),
            Point::ZERO,
            Size::new(2.0, 1.0),
        );
        base_layer.mask = Some(mask(&[255, 0], 2, 1));
        let mut clipped = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 1, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 1.0),
        );
        clipped.mask_source_id = Some(base);
        let document = document_with(vec![base_layer, clipped], Size::new(2.0, 1.0));
        let mut canvas = Canvas::new_rgba(2, 1);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        // Where the base is masked away, the clipped layer is too.
        assert_eq!(result.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(result.get(1, 0), [0, 0, 0, 0]);
    }

    /// Mask alone shows the mask's gray values across the canvas, its edge tone past its pixels.
    #[test]
    fn mask_alone_shows_the_mask_across_the_canvas() {
        let id = compositor_rs_core::new_id();
        let mut layer = layer(
            id,
            PixelImage::Rgba(Arc::new(solid(1, 1, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        layer.mask = Some(mask(&[255], 1, 1));
        let document = document_with(vec![layer], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        let state = CompositeState {
            mask_alone: Some(id),
            ..CompositeState::default()
        };
        Composite::draw_with(&document, 1.0, &identity, &mut canvas, &state);
        let result = canvas.into_rgba();
        assert_eq!(result.get(0, 0), [255, 255, 255, 255]);
        assert_eq!(result.get(1, 1), [255, 255, 255, 255]);
    }

    /// A clipping stack blends in its base's mode, where the clipped layer covers and where it does
    /// not: with Linear Dodge the covered region is the base plus the clipped layer's pixels, the
    /// rest the base plus the stack's own base.
    #[test]
    fn clipping_stacks_blend_in_their_bases_mode() {
        let stack_base = compositor_rs_core::new_id();
        let base = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(100, 100, [102, 51, 26, 255]))),
            Point::ZERO,
            Size::new(100.0, 100.0),
        );
        let mut blended = layer(
            stack_base,
            PixelImage::Rgba(Arc::new(solid(100, 100, [77, 77, 77, 255]))),
            Point::ZERO,
            Size::new(100.0, 100.0),
        );
        blended.blend_mode = LayerBlendMode::LinearDodge;
        let mut clipped = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(50, 50, [51, 13, 0, 255]))),
            Point::ZERO,
            Size::new(50.0, 50.0),
        );
        clipped.mask_source_id = Some(stack_base);
        let document = document_with(vec![base, blended, clipped], Size::new(100.0, 100.0));
        let mut canvas = Canvas::new_rgba(100, 100);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        let pixel = |x: usize, y: usize| {
            let pixel = result.get(x, y);
            [pixel[0] as i32, pixel[1] as i32, pixel[2] as i32]
        };
        // Where the clipped layer covers the stack: base plus the clipped layer.
        let covered = pixel(20, 20);
        assert!(
            covered.iter().zip([153, 64, 26]).all(|(got, want)| (got - want).abs() <= 1),
            "base plus the clipped layer: {covered:?}"
        );
        // Where it does not: base plus the stack's own base.
        let bare = pixel(80, 80);
        assert!(
            bare.iter().zip([179, 128, 102]).all(|(got, want)| (got - want).abs() <= 1),
            "base plus the stack's own base: {bare:?}"
        );
    }

    /// A hidden layer linked as another's mask source supplies its alpha without drawing: the
    /// layer's pixels are clipped to it, and the layer's own raster mask multiplies the coverage.
    #[test]
    fn a_hidden_source_supplies_its_alpha_and_masks_multiply() {
        let mut source = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(black_with_alpha(2, 2, &[255, 0, 128, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        source.is_visible = false;
        let mut clipped = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        clipped.mask_source_id = Some(source.id);
        let document = document_with(vec![clipped.clone(), source.clone()], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        let alpha: Vec<u8> = (0..4).map(|index| result.get(index % 2, index / 2)[3]).collect();
        assert_eq!(alpha, vec![255, 0, 128, 255], "the hidden source's alpha");

        // The layer's own raster mask multiplies the coverage.
        let mut with_mask = clipped.clone();
        with_mask.mask = Some(mask(&[128, 128, 128, 128], 2, 2));
        let document = document_with(vec![with_mask, source], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let masked = canvas.into_rgba();
        assert!(
            masked.get(0, 0)[3] < alpha[0],
            "a 50% mask halves the coverage: {}",
            masked.get(0, 0)[3]
        );
    }

    /// A clipping stack whose base has a soft alpha keeps it, without a black fringe: the stack's
    /// pixels stay within the base's alpha, and a translucent child leaves the alpha alone.
    #[test]
    fn clipping_color_preserves_soft_base_alpha_without_black_fringe() {
        let base = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(black_with_alpha(2, 2, &[255, 128, 32, 0]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        let mut child = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        child.mask_source_id = Some(base.id);
        let white = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [255, 255, 255, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );

        let mut document = document_with(vec![base.clone(), child.clone()], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let result = canvas.into_rgba();
        let pixels: Vec<[u8; 4]> = (0..4).map(|index| result.get(index % 2, index / 2)).collect();
        assert_eq!(
            pixels.iter().map(|pixel| pixel[3]).collect::<Vec<u8>>(),
            vec![255, 128, 32, 0],
            "the base's soft alpha"
        );
        for pixel in &pixels {
            assert_eq!(pixel[0], pixel[3], "no color above its alpha: {pixel:?}");
            assert_eq!((pixel[1], pixel[2]), (0, 0), "no fringe: {pixel:?}");
        }

        // A translucent child leaves the stack's alpha alone.
        document.layers[1].opacity = 0.5;
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let translucent = canvas.into_rgba();
        assert_eq!(
            (0..4)
                .map(|index| translucent.get(index % 2, index / 2)[3])
                .collect::<Vec<u8>>(),
            vec![255, 128, 32, 0]
        );

        // Flattened over white, the stack's soft pixels fill in: the red channel comes back to 255
        // wherever the stack is, whatever its alpha, and the alpha with it.
        document.layers[1].opacity = 1.0;
        document.layers.insert(0, white);
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let flattened = canvas.into_rgba();
        for index in 0..4 {
            let pixel = flattened.get(index % 2, index / 2);
            assert_eq!(pixel[0], 255, "flattened red: {pixel:?}");
            assert_eq!(pixel[3], 255, "flattened alpha: {pixel:?}");
        }
    }

    /// Moving the source changes what it covers, and clipping chains multiply down the chain.
    #[test]
    fn moving_source_changes_coverage_and_chains_multiply() {
        let source_id = compositor_rs_core::new_id();
        let mut clipped = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(solid(2, 2, [255, 0, 0, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        clipped.mask_source_id = Some(source_id);
        let mut source = layer(
            source_id,
            PixelImage::Rgba(Arc::new(black_with_alpha(2, 2, &[255, 0, 128, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        source.is_visible = false;
        source.transform.origin.x += 1.0;
        let document = document_with(vec![clipped.clone(), source.clone()], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let moved = canvas.into_rgba();
        let alpha: Vec<u8> = (0..4).map(|index| moved.get(index % 2, index / 2)[3]).collect();
        assert_eq!(alpha, vec![0, 255, 0, 128], "the moved source's coverage");

        // A third layer above the source, which the source is itself linked to: the chain multiplies.
        source.transform.origin.x -= 1.0;
        let mut chain = layer(
            compositor_rs_core::new_id(),
            PixelImage::Rgba(Arc::new(black_with_alpha(2, 2, &[0, 255, 255, 255]))),
            Point::ZERO,
            Size::new(2.0, 2.0),
        );
        chain.is_visible = false;
        let mut source = source;
        source.mask_source_id = Some(chain.id);
        let document = document_with(vec![clipped, source, chain], Size::new(2.0, 2.0));
        let mut canvas = Canvas::new_rgba(2, 2);
        Composite::draw(&document, 1.0, &identity, &mut canvas);
        let chained = canvas.into_rgba();
        let alpha: Vec<u8> = (0..4).map(|index| chained.get(index % 2, index / 2)[3]).collect();
        assert_eq!(alpha, vec![0, 0, 128, 255], "the chain's coverage");
    }

    /// The pixel grid draws into a device-pixel raster, so its geometry has to be scaled by the
    /// backing scale: on a 2x display a line must still land on a document pixel boundary, one
    /// device pixel wide, instead of halving everything into the raster's top-left corner.
    #[test]
    fn the_pixel_grid_lands_on_document_boundaries_in_device_pixels() {
        let document = CanvasDocument::new(4, 4);
        let size = document.size();
        let mut viewport = CanvasViewport::default();
        viewport.view_size = Size::new(100.0, 100.0);
        viewport.backing_scale = 2.0;
        viewport.set_zoom(8.0, viewport.center(), size);

        let device = viewport.backing_scale;
        let pixels = Size::new(100.0 * device, 100.0 * device);
        let mut canvas = Canvas::new_rgba(pixels.width as usize, pixels.height as usize);
        Composite::draw_pixel_grid(
            &document,
            &viewport,
            device,
            Rect::new(0.0, 0.0, pixels.width, pixels.height),
            &mut canvas,
        );
        let raster = canvas.into_rgba();

        // 4 document pixels at 8 device pixels each, centred in the 200-pixel raster.
        let first = 100.0 * device / 2.0 - 4.0 * 8.0 / 2.0;
        let step = viewport.points_per_pixel() * device;
        // Read a line that runs between the perpendicular grid lines, so every covered pixel is
        // one of the lines being measured.
        let between = (first + step / 2.0) as usize;
        let runs = |along_x: bool| {
            let last = pixels.width as usize;
            let mut runs: Vec<f64> = Vec::new();
            let mut previous: Option<usize> = None;
            for index in 0..last {
                let (x, y) = if along_x {
                    (index, between)
                } else {
                    (between, index)
                };
                if raster.get(x, y)[3] > 0 {
                    if previous != Some(index.wrapping_sub(1)) {
                        runs.push(index as f64);
                    }
                    previous = Some(index);
                }
            }
            runs
        };
        for (name, runs) in [("columns", runs(true)), ("rows", runs(false))] {
            assert_eq!(runs.len(), 5, "{name}: one line per document pixel boundary");
            for (index, start) in runs.iter().enumerate() {
                let expected = first + index as f64 * step;
                assert!(
                    (start - expected).abs() <= 1.0,
                    "{name} line {index} at {start}, expected {expected}"
                );
            }
        }
    }
}
