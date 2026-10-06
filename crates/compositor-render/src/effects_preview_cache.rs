//! Canvas-only effect previews (`EffectsPreviewCache`).
//!
//! Ported from `Rendering/EffectsPreviewCache.swift`. Full-resolution export continues to use
//! `LayerEffectsRenderer.render` on its worker. A single worker, superseded-request cancellation and a
//! fixed pixel budget keep slider drags off the UI thread; the `@MainActor` state of the Swift class is
//! a mutex here, so the worker can publish finished previews while the caller keeps reading
//! synchronously.
//!
//! `CGImage` is [`SharedImage`]/[`SharedGray`], and the identity compares (`image === other.image`,
//! `maskSource === other.maskSource`) are `Arc` pointer equality.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use compositor_core::document::ImageLayer;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::PixelImage;
use compositor_core::layer_effects::LayerEffects;
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_transform::LayerTransform;
use compositor_core::{Gray8Image, Id, Rgba8Image, SharedGray, SharedImage};
use compositor_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_pixels::effects::LayerEffectsRenderer;
use parking_lot::Mutex;

/// A few recent finished previews per layer, newest last. Undo and redo put a layer's earlier pixels
/// back, and the effects for them are taken from here instead of blinking off while they're rendered
/// again.
const RECENT_PER_LAYER: usize = 3;
/// The default side limit, before `prepare` shares the output budget across the visible effect layers.
const DEFAULT_SIDE_LIMIT: usize = 1536;
/// The output budget `prepare` shares across all effect layers (64 MiB of RGBA).
const OUTPUT_BUDGET: f64 = 16_777_216.0;

/// A preview the worker was asked for, and can be cancelled when it is superseded.
struct PreviewRequest {
    id: Id,
    image: SharedImage,
    mask: Option<SharedGray>,
    mask_source: Option<SharedGray>,
    placement: Option<LayerTransform>,
    transform: LayerTransform,
    effects: LayerEffects,
    side_limit: usize,
    cancelled: AtomicBool,
}

impl PreviewRequest {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn matches(&self, other: &PreviewRequest) -> bool {
        // Effects are rendered in layer pixels; moving, scaling or rotating the layer
        // only changes where the cached image is drawn. An independently placed mask
        // is the exception: its coverage must be resampled when either transform changes.
        let same_mask_geometry = (self.mask_source.is_none() && other.mask_source.is_none())
            || (self.placement.is_none() && other.placement.is_none())
            || (self.placement == other.placement && self.transform == other.transform);
        Arc::ptr_eq(&self.image, &other.image)
            && same_shared(&self.mask_source, &other.mask_source)
            && same_mask_geometry
            && self.effects == other.effects
            && self.side_limit == other.side_limit
    }
}

fn same_shared(lhs: &Option<SharedGray>, rhs: &Option<SharedGray>) -> bool {
    match (lhs, rhs) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// A finished preview.
#[derive(Clone)]
struct PreviewResult {
    image: SharedImage,
    inset: f64,
    /// Set only on a seeded result: where that image belongs on the document, which an inset can't
    /// express when the layer's own box was cropped as well as warped.
    placement: Option<LayerTransform>,
}

impl PreviewResult {
    fn tuple(&self) -> (SharedImage, f64, Option<LayerTransform>) {
        (self.image.clone(), self.inset, self.placement)
    }
}

#[derive(Clone)]
struct Entry {
    request: Arc<PreviewRequest>,
    result: Option<PreviewResult>,
}

/// The cache's shared state: what the caller reads and what the worker publishes into.
struct State {
    entries: HashMap<Id, Entry>,
    recent: HashMap<Id, Vec<Entry>>,
    /// A result handed in from elsewhere — the effects warped with a distortion as it is applied — shown
    /// until the worker has rendered the layer's new pixels, so the effects don't blink off for a frame.
    seeds: HashMap<Id, PreviewResult>,
    side_limit: usize,
}

impl State {
    fn new() -> Self {
        State {
            entries: HashMap::new(),
            recent: HashMap::new(),
            seeds: HashMap::new(),
            side_limit: DEFAULT_SIDE_LIMIT,
        }
    }
}

struct Job {
    layer_id: Id,
    request: Arc<PreviewRequest>,
    state: Arc<Mutex<State>>,
    completion: Option<Box<dyn FnOnce() + Send>>,
}

/// The single worker all previews render on, mirroring the Swift `DispatchQueue`.
static WORKER: LazyLock<Sender<Job>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<Job>();
    thread::Builder::new()
        .name("compositor-effects-preview".to_string())
        .spawn(move || {
            while let Ok(job) = receiver.recv() {
                // The Swift worker waits 0.06 s before starting, so a dragged slider only renders its
                // final value.
                thread::sleep(Duration::from_millis(60));
                if job.request.is_cancelled() {
                    continue;
                }
                let result = render(&job.request);
                if job.request.is_cancelled() {
                    continue;
                }
                let published = {
                    let mut state = job.state.lock();
                    match state.entries.get(&job.layer_id) {
                        Some(entry) if entry.request.id == job.request.id => {
                            if let Some(result) = &result {
                                state.seeds.remove(&job.layer_id);
                                let mut kept = state.recent.get(&job.layer_id).cloned().unwrap_or_default();
                                kept.retain(|entry| !entry.request.matches(&job.request));
                                kept.push(Entry {
                                    request: job.request.clone(),
                                    result: Some(result.clone()),
                                });
                                let drop_count = kept.len().saturating_sub(RECENT_PER_LAYER);
                                kept.drain(..drop_count);
                                state.recent.insert(job.layer_id, kept);
                            }
                            if let Some(entry) = state.entries.get_mut(&job.layer_id) {
                                entry.result = result.clone();
                            }
                            true
                        }
                        _ => false,
                    }
                };
                if published {
                    if let Some(completion) = job.completion {
                        completion();
                    }
                }
            }
        })
        .expect("the effects preview worker thread starts");
    sender
});

/// Canvas-only previews of a layer's effects, rendered on one background worker.
pub struct EffectsPreviewCache {
    state: Arc<Mutex<State>>,
}

impl Default for EffectsPreviewCache {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectsPreviewCache {
    pub fn new() -> Self {
        EffectsPreviewCache {
            state: Arc::new(Mutex::new(State::new())),
        }
    }

    /// Shows `image` at `placement` for a layer until a fresh preview is ready.
    pub fn seed(&self, id: Id, image: SharedImage, placement: LayerTransform) {
        // Whatever is being rendered is for the pixels this replaces, and landing later would drop the seed.
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.remove(&id) {
            entry.request.cancel();
        }
        state.seeds.insert(
            id,
            PreviewResult {
                image,
                inset: 0.0,
                placement: Some(placement),
            },
        );
    }

    /// What is already rendered for a layer, without asking for anything new.
    pub fn rendered(&self, id: Id) -> Option<(SharedImage, f64, Option<LayerTransform>)> {
        let state = self.state.lock();
        state
            .entries
            .get(&id)
            .and_then(|entry| entry.result.as_ref())
            .or_else(|| state.seeds.get(&id))
            .map(PreviewResult::tuple)
    }

    pub fn prepare(&self, layers: &[ImageLayer]) {
        let ids: std::collections::HashSet<Id> = layers
            .iter()
            .filter(|layer| {
                layer
                    .effects
                    .as_ref()
                    .map(|effects| !effects.visible().is_empty())
                    .unwrap_or(false)
            })
            .map(|layer| layer.id)
            .collect();
        let mut state = self.state.lock();
        let stale: Vec<Id> = state
            .entries
            .keys()
            .filter(|id| !ids.contains(id))
            .copied()
            .collect();
        for id in stale {
            if let Some(entry) = state.entries.remove(&id) {
                entry.request.cancel();
            }
        }
        state.seeds.retain(|id, _| ids.contains(id));
        state.recent.retain(|id, _| ids.contains(id));
        // Share a ~64 MiB output budget across all effect layers. Do not evict visible layers in a
        // redraw cycle: that would repeatedly rebuild evicted previews when more layers are visible.
        state.side_limit = DEFAULT_SIDE_LIMIT.min(32.max(
            (OUTPUT_BUDGET / ids.len().max(1) as f64).sqrt() as usize,
        ));
    }

    /// The layer's effects preview, asked for if it is not already rendered. The returned tuple is what
    /// can be shown right now (the previous result, or a seed); `completion` runs on the worker once the
    /// fresh one is in.
    pub fn preview(
        &self,
        layer: &ImageLayer,
        mask: Option<SharedGray>,
        transform: &LayerTransform,
        mask_placement: Option<&LayerTransform>,
        completion: impl FnOnce() + Send + 'static,
    ) -> Option<(SharedImage, f64, Option<LayerTransform>)> {
        let visible = layer.effects.as_ref().map(LayerEffects::visible);
        let Some(effects) = visible.filter(|effects| !effects.is_empty() && effects.is_valid()) else {
            let mut state = self.state.lock();
            if let Some(entry) = state.entries.remove(&layer.id) {
                entry.request.cancel();
            }
            return None;
        };
        let Some(image) = layer_image(layer) else {
            let mut state = self.state.lock();
            if let Some(entry) = state.entries.remove(&layer.id) {
                entry.request.cancel();
            }
            return None;
        };
        let mask_source = mask_source_of(layer);
        let request = Arc::new({
            let state = self.state.lock();
            PreviewRequest {
                id: compositor_core::new_id(),
                image,
                mask,
                mask_source,
                placement: mask_placement.copied(),
                transform: *transform,
                effects,
                side_limit: state.side_limit,
                cancelled: AtomicBool::new(false),
            }
        });

        let mut previous = None;
        let mut known_result = None;
        {
            let mut state = self.state.lock();
            if let Some(entry) = state.entries.get(&layer.id) {
                if entry.request.matches(&request) {
                    return entry.result.as_ref().map(PreviewResult::tuple);
                }
            }
            let old = state.entries.get(&layer.id).cloned();
            if let Some(old) = &old {
                old.request.cancel();
            }
            if let Some(entry) = state.recent.get(&layer.id).and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| entry.request.matches(&request))
            }) {
                if let Some(result) = &entry.result {
                    known_result = Some(result.clone());
                }
            }
            if known_result.is_none() {
                // Keep effects visible during transforms and setting changes on the same pixels.
                previous = old
                    .as_ref()
                    .filter(|entry| Arc::ptr_eq(&entry.request.image, &request.image))
                    .and_then(|entry| entry.result.clone())
                    .or_else(|| state.seeds.get(&layer.id).cloned());
                state.entries.insert(
                    layer.id,
                    Entry {
                        request: request.clone(),
                        result: previous.clone(),
                    },
                );
            }
        }
        if let Some(result) = known_result {
            let mut state = self.state.lock();
            state.entries.insert(
                layer.id,
                Entry {
                    request: request,
                    result: Some(result.clone()),
                },
            );
            state.seeds.remove(&layer.id);
            return Some(result.tuple());
        }
        let _ = WORKER.send(Job {
            layer_id: layer.id,
            request,
            state: self.state.clone(),
            completion: Some(Box::new(completion)),
        });
        previous.as_ref().map(PreviewResult::tuple)
    }

    /// Renders effects straight away, at the preview size: text being typed is small, and its effects
    /// shouldn't lag a keystroke behind it.
    pub fn render_now(
        &self,
        image: &SharedImage,
        mask: Option<&Gray8Image>,
        effects: &LayerEffects,
    ) -> Option<(SharedImage, f64)> {
        let side_limit = {
            let state = self.state.lock();
            state.side_limit
        };
        let request = PreviewRequest {
            id: compositor_core::new_id(),
            image: image.clone(),
            mask: mask.map(|mask| Arc::new(mask.clone())),
            mask_source: None,
            placement: None,
            transform: LayerTransform::default(),
            effects: effects.clone(),
            side_limit,
            cancelled: AtomicBool::new(false),
        };
        render(&request).map(|result| (result.image, result.inset))
    }
}

/// `layer.asset?.image`, as the shared raster the request holds: layer pixels are RGBA.
fn layer_image(layer: &ImageLayer) -> Option<SharedImage> {
    match layer.asset.as_ref().map(|asset| &asset.image) {
        Some(PixelImage::Rgba(image)) => Some(image.clone()),
        _ => None,
    }
}

/// `LayerTransform(origin:size:)` — the Swift memberwise initializer's defaults.
fn transform_at(origin: Point, size: Size) -> LayerTransform {
    LayerTransform {
        origin,
        size,
        ..Default::default()
    }
}

/// `layer.mask?.enabledImage`: the mask's pixels while enabled, as the shared gray raster.
fn mask_source_of(layer: &ImageLayer) -> Option<SharedGray> {
    match layer.mask.as_ref().and_then(LayerMask::enabled_image) {
        Some(PixelImage::Gray(gray)) => Some(gray.clone()),
        _ => None,
    }
}

/// One preview render, at the budgeted size.
fn render(request: &PreviewRequest) -> Option<PreviewResult> {
    let image = &request.image;
    let margin = LayerEffectsRenderer::margin(&request.effects);
    // Include stroke/shadow margins in the budget; even a 500px stroke stays bounded.
    let factor = 1.0f64.min(
        (request.side_limit as f64 - 8.0) / (image.width().max(image.height()) as f64 + 2.0 * margin),
    );
    let width = ((image.width() as f64 * factor).round() as usize).max(1);
    let height = ((image.height() as f64 * factor).round() as usize).max(1);
    let resized_rgba = |source: &Rgba8Image| {
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        canvas.draw_image(source, Rect::from_origin_size(Point::ZERO, Size::new(width as f64, height as f64)));
        canvas.into_rgba()
    };
    let resized_gray = |source: &Gray8Image| {
        let mut canvas = Canvas::new_gray(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        // A DeviceGray image drawn into a gray context lands its own samples; the port's
        // `draw_gray` paints through them, so the paint is white.
        canvas.set_fill_gray(1.0);
        canvas.draw_gray(source, Rect::from_origin_size(Point::ZERO, Size::new(width as f64, height as f64)));
        canvas.into_gray()
    };
    let mut scaled_pixels = None;
    let mut scaled_mask = None;
    if factor != 1.0 {
        scaled_pixels = Some(resized_rgba(image));
        scaled_mask = request.mask.as_ref().map(|mask| resized_gray(mask));
    }
    let pixels = scaled_pixels.as_ref().unwrap_or(image);
    let mask = scaled_mask.as_ref().or(request.mask.as_deref());
    let mut effects = request.effects.clone();
    if let Some(stroke) = effects.stroke.as_mut() {
        stroke.size *= factor;
    }
    if let Some(shadow) = effects.shadow.as_mut() {
        shadow.distance *= factor;
        shadow.blur *= factor;
    }
    let rendered = LayerEffectsRenderer::render(pixels, mask, &effects).ok()?;
    Some(PreviewResult {
        image: Arc::new(rendered.image),
        inset: rendered.inset,
        placement: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::imported_image::PixelImage;
    use compositor_core::layer_effects::StrokeEffect;

    fn shared_rgba(width: usize, height: usize) -> SharedImage {
        Arc::new(Rgba8Image::new(width, height))
    }

    fn request(image: &SharedImage, mask_source: Option<&SharedGray>) -> PreviewRequest {
        PreviewRequest {
            id: compositor_core::new_id(),
            image: image.clone(),
            mask: None,
            mask_source: mask_source.cloned(),
            placement: None,
            transform: LayerTransform::default(),
            effects: LayerEffects::default(),
            side_limit: DEFAULT_SIDE_LIMIT,
            cancelled: AtomicBool::new(false),
        }
    }

    /// The identity and geometry rules `matches` encodes: same pixels, same mask source, same effects
    /// and budget; transforms only matter while an independently placed mask is in play.
    #[test]
    fn match_rules_pin_the_cache_invalidation() {
        let image = shared_rgba(8, 8);
        let other = shared_rgba(8, 8);

        // Same pixels: moving the layer does not invalidate a preview without a mask source.
        let mut moved = request(&image, None);
        moved.transform = transform_at(Point::new(40.0, 4.0), Size::new(8.0, 8.0));
        moved.placement = Some(LayerTransform::default());
        assert!(request(&image, None).matches(&moved));

        // Different pixels never match.
        assert!(!request(&image, None).matches(&request(&other, None)));

        // Different effects or budget never match.
        let mut stronger = request(&image, None);
        stronger.effects.stroke = Some(StrokeEffect { size: 6.0, ..Default::default() });
        assert!(!request(&image, None).matches(&stronger));
        let mut budgeted = request(&image, None);
        budgeted.side_limit = 512;
        assert!(!request(&image, None).matches(&budgeted));

        // An independently placed mask is the exception: its coverage must be resampled when either
        // transform changes.
        let mask = Arc::new(Gray8Image::new(8, 8));
        let mut first = request(&image, Some(&mask));
        first.placement = Some(transform_at(Point::ZERO, Size::new(4.0, 4.0)));
        first.transform = transform_at(Point::ZERO, Size::new(8.0, 8.0));
        let mut second = request(&image, Some(&mask));
        second.placement = first.placement;
        second.transform = transform_at(Point::new(1.0, 0.0), Size::new(8.0, 8.0));
        assert!(!first.matches(&second));
        second.transform = first.transform;
        assert!(first.matches(&second));

        // A different mask source never matches.
        let other_mask = Arc::new(Gray8Image::new(8, 8));
        assert!(!first.matches(&request(&image, Some(&other_mask))));

        // With no mask source at all, different placements are still fine.
        let mut placed = request(&image, None);
        placed.placement = Some(transform_at(Point::ZERO, Size::new(4.0, 4.0)));
        let mut placed_elsewhere = request(&image, None);
        placed_elsewhere.placement = Some(transform_at(Point::new(9.0, 9.0), Size::new(4.0, 4.0)));
        assert!(placed.matches(&placed_elsewhere));
    }

    /// A seeded preview is what `rendered` returns, with the seed's placement.
    #[test]
    fn seed_then_rendered_returns_the_seeded_image() {
        let cache = EffectsPreviewCache::new();
        let id = compositor_core::new_id();
        let image = shared_rgba(10, 10);
        let placement = transform_at(Point::new(3.0, 4.0), Size::new(10.0, 10.0));
        assert!(cache.rendered(id).is_none());
        cache.seed(id, image.clone(), placement);
        let (shown, inset, placed) = cache.rendered(id).expect("the seed is shown");
        assert!(Arc::ptr_eq(&shown, &image));
        assert_eq!(inset, 0.0);
        assert_eq!(placed, Some(placement));
    }

    /// `prepare` drops layers that aren't showing effects and re-shares the output budget.
    #[test]
    fn prepare_shares_the_output_budget_across_effect_layers() {
        let cache = EffectsPreviewCache::new();
        let layer = |effects: Option<LayerEffects>| {
            let mut layer = ImageLayer::blank("Layer", Size::new(4.0, 4.0));
            layer.effects = effects;
            layer
        };
        let stroked = || {
            Some(LayerEffects {
                stroke: Some(StrokeEffect::default()),
                ..Default::default()
            })
        };

        // One effect layer: the full 1536 side.
        cache.prepare(&[layer(stroked())]);
        assert_eq!(cache.state.lock().side_limit, 1536);

        // 64 effect layers: sqrt(16_777_216 / 64) = 512.
        let many: Vec<ImageLayer> = (0..64).map(|_| layer(stroked())).collect();
        cache.prepare(&many);
        assert_eq!(cache.state.lock().side_limit, 512);

        // 1024 effect layers: sqrt(16384) = 128.
        let many: Vec<ImageLayer> = (0..1024).map(|_| layer(stroked())).collect();
        cache.prepare(&many);
        assert_eq!(cache.state.lock().side_limit, 128);

        // Nothing hiding: the floor stays 32.
        let many: Vec<ImageLayer> = (0..100_000).map(|_| layer(stroked())).collect();
        cache.prepare(&many);
        assert_eq!(cache.state.lock().side_limit, 32);

        // A layer with no effects (or only hidden ones) is dropped, not budgeted.
        let id = compositor_core::new_id();
        let mut absent = layer(None);
        absent.id = id;
        cache.seed(id, shared_rgba(4, 4), LayerTransform::default());
        cache.prepare(&[absent]);
        assert!(cache.rendered(id).is_none());
        assert_eq!(cache.state.lock().side_limit, 1536);

        // A hidden effect does not count either.
        let mut hidden = layer(stroked());
        hidden.effects.as_mut().unwrap().set_enabled(false, compositor_core::layer_effects::LayerEffectKind::Stroke);
        cache.prepare(&[hidden]);
        assert!(cache.state.lock().entries.is_empty());
    }

    /// A layer whose effects are invalid, hidden or absent gives no preview and cancels what was queued.
    #[test]
    fn preview_refuses_layers_without_visible_effects() {
        let cache = EffectsPreviewCache::new();
        let mut layer = ImageLayer::blank("Layer", Size::new(8.0, 8.0));
        let asset = compositor_core::imported_image::ImportedImage::new(
            PixelImage::Rgba(shared_rgba(8, 8)),
            PixelImage::Rgba(shared_rgba(8, 8)),
            "Photo",
        );
        layer.asset = Some(asset);
        assert!(cache
            .preview(&layer, None, &LayerTransform::default(), None, || {})
            .is_none());

        // A blank layer has no pixels to render either.
        let mut with_effects = layer.clone();
        with_effects.effects = Some(LayerEffects {
            stroke: Some(StrokeEffect::default()),
            ..Default::default()
        });
        with_effects.asset = None;
        assert!(cache
            .preview(&with_effects, None, &LayerTransform::default(), None, || {})
            .is_none());
    }

    /// `render_now` renders synchronously at the budgeted size, with the effects' margin as its inset.
    #[test]
    fn render_now_reports_the_effects_inset() {
        let cache = EffectsPreviewCache::new();
        let image = shared_rgba(32, 24);
        let (rendered, inset) = cache
            .render_now(&image, None, &LayerEffects::default())
            .expect("an empty effects record still renders");
        assert_eq!(inset, 2.0, "the guard margin, exactly like LayerEffectsRenderer.margin");
        assert_eq!((rendered.width(), rendered.height()), (36, 28));
    }
}
