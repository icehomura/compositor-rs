//! Canvas Size, Image Size, Trim and Flip Canvas — the commands that change the document's canvas
//! rather than its layers.
//!
//! Ported from `IO/CanvasResizer.swift`, `IO/ImageResizer.swift`, the `extension EditorSession` in
//! `Document/ImageTrim.swift`, `Document/LayerFlip.swift` and the `CanvasSizeSheet`/`ImageSizeSheet`
//! command bodies of `IO/ProjectController.swift`.
//!
//! The Swift resizers moved a `ProjectSnapshot` — a transport for the manifest plus the images,
//! built by `projectSnapshot()` and turned back into a `CanvasDocument` by `applyDocumentSize`. The
//! port works on the `CanvasDocument` itself, which already carries both, so `apply_document_size`
//! installs the resized document directly.

use std::sync::Arc;

use compositor_core::buffer::Rgba8Image;
use compositor_core::canvas_size::{CanvasExtensionColor, CanvasSizeDraft, CanvasSizeOptions};
use compositor_core::document::{CanvasDocument, ImageLayer, ProjectError};
use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::imported_image::{ImportedImage, PixelImage};
use compositor_core::layer_mask::LayerMask;
use compositor_core::layer_transform::{LayerSampling, LayerTransform};
use compositor_core::limits::{document_pixel_budget, MAX_SIDE, MAX_SURFACE_PIXELS};
use compositor_pixels::canvas::{Canvas, InterpolationQuality};
use compositor_pixels::trim::{
    calculate_trim_rect, trim_canvas_options, trim_is_noop, TrimOptions,
};
use compositor_render::layer_renderer::LayerRenderer;

use crate::session::EditorSession;

/// The thumbnail the panels show for a freshly rasterized layer or canvas
/// (`min(1, 96 / max(width, height))`, drawn with high-quality interpolation).
fn thumbnail_of(image: &Rgba8Image) -> Rgba8Image {
    let factor = 1.0f64.min(96.0 / image.width().max(image.height()).max(1) as f64);
    let width = ((image.width() as f64 * factor) as usize).max(1);
    let height = ((image.height() as f64 * factor) as usize).max(1);
    let mut canvas = Canvas::new_rgba(width, height);
    canvas.set_interpolation_quality(InterpolationQuality::High);
    canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
    canvas.snapshot()
}

/// `CanvasResizer`: changes the canvas rectangle and moves the content with it.
pub struct CanvasResizer;

impl CanvasResizer {
    /// `CanvasResizer.resize(_:to:)`.
    ///
    /// The old canvas keeps its pixels and moves to the anchor's position; the added area is
    /// transparent, or — when the sheet asked for a color and the canvas grew — a separate
    /// bottom "Canvas Extension" layer filled with it, with the old canvas' rectangle cleared out
    /// of it (holes in the existing artwork stay holes).
    pub fn resize(
        document: &CanvasDocument,
        options: &CanvasSizeOptions,
    ) -> Result<CanvasDocument, ProjectError> {
        if !(1..=MAX_SIDE).contains(&options.width)
            || !(1..=MAX_SIDE).contains(&options.height)
            || options.anchor > 8
        {
            return Err(ProjectError::TooLarge);
        }
        let offset = options.offset(document.width, document.height);
        if !offset.is_finite() || offset.x.abs() > 1_000_000.0 || offset.y.abs() > 1_000_000.0 {
            return Err(ProjectError::Invalid);
        }
        if options.width == document.width
            && options.height == document.height
            && offset == Point::ZERO
        {
            return Ok(document.clone());
        }
        let mut layers: Vec<ImageLayer> = Vec::with_capacity(document.layers.len() + 1);
        for layer in &document.layers {
            let mut moved = layer.clone();
            moved.transform.origin.x += offset.x;
            moved.transform.origin.y += offset.y;
            if !moved.transform.is_valid() {
                return Err(ProjectError::TooLarge);
            }
            // Upstream's `ProjectLayerRecord` construction here passes `shape` and `text` along but
            // not `effects`, so a canvas resize drops the stroke/shadow, as it does upstream.
            moved.effects = None;
            if let Some(placement) = moved.mask.as_mut().and_then(|mask| mask.placement.as_mut()) {
                placement.origin.x += offset.x;
                placement.origin.y += offset.y;
            }
            layers.push(moved);
        }
        let guides = document
            .guides
            .iter()
            .map(|guide| guide.offset(offset.x, offset.y))
            .collect();
        let mut result = CanvasDocument::with_id(
            document.id,
            options.width,
            options.height,
            layers,
            document.resolution,
            guides,
        );
        // A colored extension is separate bottom-layer content. The old canvas
        // intersection remains transparent, including holes in the existing artwork.
        if let Some(color) = options.fill {
            if options.width > document.width || options.height > document.height {
                let used: usize = document
                    .layers
                    .iter()
                    .filter_map(|layer| layer.asset.as_ref())
                    .map(|asset| asset.image.pixel_count())
                    .sum();
                if options.width * options.height > document_pixel_budget().saturating_sub(used)
                    || result.layers.len() >= 10_000
                {
                    return Err(ProjectError::TooLarge);
                }
                if ![color.red, color.green, color.blue]
                    .iter()
                    .all(|component| component.is_finite() && (0.0..=1.0).contains(component))
                {
                    return Err(ProjectError::Invalid);
                }
                let mut canvas = Canvas::new_rgba(options.width, options.height);
                canvas.set_fill_color(compositor_core::color::PaletteColor::new(
                    color.red,
                    color.green,
                    color.blue,
                ));
                canvas.fill_rect(Rect::new(
                    0.0,
                    0.0,
                    options.width as f64,
                    options.height as f64,
                ));
                canvas.clear(Rect::new(
                    offset.x,
                    offset.y,
                    document.width as f64,
                    document.height as f64,
                ));
                let thumbnail = thumbnail_of(canvas.rgba());
                let image = canvas.into_rgba();
                let asset = ImportedImage::new(
                    PixelImage::Rgba(Arc::new(image)),
                    PixelImage::Rgba(Arc::new(thumbnail)),
                    "Canvas Extension",
                );
                result
                    .layers
                    .insert(0, ImageLayer::from_asset(asset, Point::ZERO));
            }
        }
        Ok(result)
    }
}

/// The Image Size sheet's result (`ImageSizeOptions`): the new pixel dimensions and resolution, and
/// the sampling the layers are resampled with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageSizeOptions {
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    pub sampling: LayerSampling,
}

impl ImageSizeOptions {
    pub fn new(width: usize, height: usize, resolution: f64) -> Self {
        Self {
            width,
            height,
            resolution,
            sampling: LayerSampling::High,
        }
    }
}

/// `ImageResizer`: resamples every layer's pixels and bounds to the new canvas size.
pub struct ImageResizer;

impl ImageResizer {
    /// `ImageResizer.resize(_:to:)`.
    ///
    /// Each transformed layer is rasterized on its own — a rotated rectangle scaled
    /// non-uniformly can shear, which an origin/size/angle cannot hold — and mask placements scale
    /// with the canvas.
    pub fn resize(
        document: &CanvasDocument,
        options: &ImageSizeOptions,
    ) -> Result<CanvasDocument, ProjectError> {
        if !(1..=MAX_SIDE).contains(&options.width)
            || !(1..=MAX_SIDE).contains(&options.height)
            || !options.resolution.is_finite()
            || !(1.0..=9600.0).contains(&options.resolution)
        {
            return Err(ProjectError::TooLarge);
        }
        if document.width == options.width && document.height == options.height {
            let mut same = document.clone();
            same.resolution = options.resolution;
            return Ok(same);
        }
        if options.width * options.height > MAX_SURFACE_PIXELS {
            return Err(ProjectError::TooLarge);
        }
        let sx = options.width as f64 / document.width as f64;
        let sy = options.height as f64 / document.height as f64;
        let guides = document
            .guides
            .iter()
            .map(|guide| guide.scaled(sx, sy))
            .collect();
        let mut layers: Vec<ImageLayer> = Vec::with_capacity(document.layers.len());
        let mut used_pixels = 0usize;
        let mut used_mask_pixels = 0usize;
        let budget = document_pixel_budget();
        for layer in &document.layers {
            let corners: Vec<Point> = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]
                .into_iter()
                .map(|(x, y)| layer.transform.point(Point::new(x, y)))
                .map(|point| Point::new(point.x * sx, point.y * sy))
                .collect();
            let left = corners
                .iter()
                .map(|point| point.x)
                .fold(f64::INFINITY, f64::min)
                .floor();
            let top = corners
                .iter()
                .map(|point| point.y)
                .fold(f64::INFINITY, f64::min)
                .floor();
            let width_f = corners
                .iter()
                .map(|point| point.x)
                .fold(f64::NEG_INFINITY, f64::max)
                .ceil()
                - left;
            let height_f = corners
                .iter()
                .map(|point| point.y)
                .fold(f64::NEG_INFINITY, f64::max)
                .ceil()
                - top;
            if !(width_f.is_finite() && height_f.is_finite() && width_f >= 0.0 && height_f >= 0.0) {
                return Err(ProjectError::TooLarge);
            }
            let width = width_f as usize;
            let height = height_f as usize;
            let transform = LayerTransform {
                origin: Point::new(left, top),
                size: Size::new(width as f64, height as f64),
                sampling: options.sampling,
                ..LayerTransform::default()
            };
            if !transform.is_valid() {
                return Err(ProjectError::TooLarge);
            }
            let mut moved = layer.clone();
            moved.transform = transform;
            // Upstream's `ProjectLayerRecord` construction in ImageResizer passes none of `shape`,
            // `effects` or `text`, so an image resize drops them from every layer; kept as upstream.
            moved.shape = None;
            moved.effects = None;
            moved.text = None;
            if let Some(source) = layer.asset.as_ref() {
                if !(1..=MAX_SIDE).contains(&width)
                    || !(1..=MAX_SIDE).contains(&height)
                    || width * height > budget.saturating_sub(used_pixels)
                {
                    return Err(ProjectError::TooLarge);
                }
                used_pixels += width * height;
                let mut canvas = Canvas::new_rgba(width, height);
                canvas.translate(-left, -top);
                canvas.scale(sx, sy);
                let mut source_transform = layer.transform;
                source_transform.sampling = options.sampling;
                LayerRenderer::draw(
                    &source.image,
                    &source_transform,
                    source_transform.center(),
                    1.0,
                    1.0,
                    compositor_core::blend::LayerBlendMode::Normal,
                    None,
                    &mut canvas,
                );
                let thumbnail = thumbnail_of(canvas.rgba());
                let image = canvas.into_rgba();
                moved.asset = Some(ImportedImage::new(
                    PixelImage::Rgba(Arc::new(image)),
                    PixelImage::Rgba(Arc::new(thumbnail)),
                    source.name.clone(),
                ));
            }
            if let Some(mask) = layer.mask.as_ref() {
                // Uniform masks are resolution independent; avoid allocating a full canvas for
                // reveal/hide-all. A mask on its own placement keeps its pixels; the placement
                // scales with the canvas.
                let uniform = mask.asset.image.width() == 1 && mask.asset.image.height() == 1;
                if !(uniform || mask.placement.is_some()) {
                    if !(1..=MAX_SIDE).contains(&width)
                        || !(1..=MAX_SIDE).contains(&height)
                        || width * height > budget.saturating_sub(used_mask_pixels)
                    {
                        return Err(ProjectError::TooLarge);
                    }
                    used_mask_pixels += width * height;
                    let source = mask
                        .asset
                        .image
                        .as_gray()
                        .ok_or(ProjectError::MissingImage)?;
                    let mut canvas = Canvas::new_gray(width, height);
                    canvas.translate(-left, -top);
                    canvas.scale(sx, sy);
                    let mut source_transform = layer.transform;
                    source_transform.sampling = options.sampling;
                    LayerRenderer::draw_coverage(source, &source_transform, &mut canvas);
                    let image = canvas.into_gray();
                    let asset = LayerMask::asset(PixelImage::Gray(Arc::new(image)))
                        .map_err(|_| ProjectError::Invalid)?;
                    if let Some(moved_mask) = moved.mask.as_mut() {
                        *moved_mask = moved_mask.replacing(asset);
                    }
                }
                if let Some(placement) = mask.placement {
                    let scaled = placement.placing(
                        // Swift `A.concatenating(B)` runs the receiver first: the placement maps
                        // to the old document, then the canvas scale lands on those coordinates.
                        placement
                            .unit_to_document()
                            .then(AffineTransform::scale(sx, sy)),
                    );
                    if let Some(moved_mask) = moved.mask.as_mut() {
                        moved_mask.placement = Some(scaled);
                    }
                }
            }
            layers.push(moved);
        }
        Ok(CanvasDocument::with_id(
            document.id,
            options.width,
            options.height,
            layers,
            options.resolution,
            guides,
        ))
    }
}

/// New Canvas sizes: common screens and resolutions, in pixels, upright as the device is usually
/// held (`NewCanvasSheet.CanvasPreset`). Identity is the title, as `Identifiable`'s is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CanvasPreset {
    pub title: &'static str,
    pub width: usize,
    pub height: usize,
}

impl CanvasPreset {
    pub const fn new(title: &'static str, width: usize, height: usize) -> Self {
        CanvasPreset {
            title,
            width,
            height,
        }
    }

    /// Resolutions, Apple screens, then social formats; the menu divides them.
    pub const GROUPS: [&'static [CanvasPreset]; 3] = [
        &[
            CanvasPreset::new("4K", 3840, 2160),
            CanvasPreset::new("1440p", 2560, 1440),
            CanvasPreset::new("1080p", 1920, 1080),
        ],
        &[
            CanvasPreset::new("iPhone 18 Pro", 1206, 2622),
            CanvasPreset::new("iPhone 18 Pro Max", 1320, 2868),
            CanvasPreset::new("MacBook Pro 14\"", 3024, 1964),
            CanvasPreset::new("MacBook Pro 16\"", 3456, 2234),
            CanvasPreset::new("Studio Display", 5120, 2880),
        ],
        &[
            CanvasPreset::new("Instagram Square", 1080, 1080),
            CanvasPreset::new("Instagram Portrait", 1080, 1350),
            CanvasPreset::new("Instagram Story", 1080, 1920),
            CanvasPreset::new("YouTube Thumb", 1080, 608),
        ],
    ];

    /// Every preset, in menu order (`CanvasPreset.all`).
    pub fn all() -> Vec<CanvasPreset> {
        Self::GROUPS
            .iter()
            .flat_map(|group| group.iter().copied())
            .collect()
    }

    /// The preset the fields match, or nil (Custom), as the sheet's picker computes it.
    pub fn matching(width: usize, height: usize) -> Option<CanvasPreset> {
        Self::all()
            .into_iter()
            .find(|preset| preset.width == width && preset.height == height)
    }
}

impl EditorSession {
    /// Installs a resized document (`applyDocumentSize(_:actionName:)`), one undo step under
    /// `action_name`, and fits the view to the new size.
    pub fn apply_document_size(&mut self, document: CanvasDocument, action_name: &str) {
        if self.document.as_ref().map(|current| current.id) != Some(document.id) {
            return;
        }
        self.begin_edit(action_name);
        self.document = Some(document);
        self.end_edit();
        if let Some(document) = self.document.as_ref() {
            let size = document.size();
            self.viewport.fit(size);
        }
    }

    /// `applyImageSize(_:)`.
    pub fn apply_image_size(&mut self, document: CanvasDocument) {
        self.apply_document_size(document, "Image Size");
    }

    /// File > Canvas Size… (`ProjectController.canvasSize`): the sheet's options become the
    /// document's new canvas, one undo step named "Canvas Size". The controller's
    /// `canStartProjectOperation` gate, `cancelCrop`/`commitTransform` prelude and `isProjectBusy`
    /// span are folded into this call; the port runs the resizer inline, so the busy flag is only
    /// ever observed inside it.
    pub fn change_canvas_size(&mut self, options: &CanvasSizeOptions) -> Result<(), ProjectError> {
        if !self.can_start_project_operation() {
            return Ok(());
        }
        let Some(document) = self.document.clone() else {
            return Ok(());
        };
        self.cancel_crop();
        self.commit_transform();
        self.is_project_busy = true;
        let result = CanvasResizer::resize(&document, options);
        self.is_project_busy = false;
        let resized = result?;
        self.apply_document_size(resized, "Canvas Size");
        Ok(())
    }

    /// File > Image Size… (`ProjectController.imageSize`): resamples every layer and sets the
    /// resolution, one undo step named "Image Size".
    pub fn change_image_size(&mut self, options: &ImageSizeOptions) -> Result<(), ProjectError> {
        if !self.can_start_project_operation() {
            return Ok(());
        }
        let Some(document) = self.document.clone() else {
            return Ok(());
        };
        self.cancel_crop();
        self.commit_transform();
        self.is_project_busy = true;
        let result = ImageResizer::resize(&document, options);
        self.is_project_busy = false;
        let resized = result?;
        self.apply_image_size(resized);
        Ok(())
    }

    /// The Canvas Size sheet's starting draft for the open document, nil without one.
    pub fn canvas_size_draft(&self) -> Option<CanvasSizeDraft> {
        self.document.as_ref().map(|document| {
            CanvasSizeDraft::new(document.width, document.height, document.resolution)
        })
    }

    /// The Image Size sheet's starting options for the open document, nil without one.
    pub fn image_size_options(&self) -> Option<ImageSizeOptions> {
        self.document.as_ref().map(|document| {
            ImageSizeOptions::new(document.width, document.height, document.resolution)
        })
    }

    /// Image > Trim… (`ImageTrim.trim` + `EditorSession.trim(options:)`).
    ///
    /// The document is composited as the canvas shows it, the trim rectangle found from the chosen
    /// basis, and the canvas resized to it with the content offset — one undo step named "Trim".
    /// `Ok(false)` means no content remained to trim; the caller keeps the document.
    ///
    /// Swift ran this through `ImageExporter` and `CanvasResizer` actors with `isProjectBusy` set;
    /// the port renders and resizes inline, so the busy flag is only observable inside the call.
    pub fn trim(&mut self, options: &TrimOptions) -> Result<bool, ProjectError> {
        if !self.can_start_project_operation() {
            return Ok(false);
        }
        let Some(document) = self.document.clone() else {
            return Ok(false);
        };
        self.is_project_busy = true;
        let raster = self.rendered_document(&document);
        let result: Result<Option<CanvasDocument>, ProjectError> =
            match calculate_trim_rect(&raster, options) {
                None => Ok(None),
                Some(rect) => {
                    if trim_is_noop(rect, document.width, document.height) {
                        Ok(Some(document.clone()))
                    } else {
                        CanvasResizer::resize(&document, &trim_canvas_options(rect)).map(Some)
                    }
                }
            };
        self.is_project_busy = false;
        match result? {
            None => Ok(false),
            Some(trimmed) => {
                self.apply_document_size(trimmed, "Trim");
                Ok(true)
            }
        }
    }

    /// The document as the canvas shows it, for the trim scan. Kept a method so a renderer with
    /// more state (strokes, previews) can replace it in one place.
    fn rendered_document(&self, document: &CanvasDocument) -> Rgba8Image {
        let mut canvas = Canvas::new_rgba(document.width, document.height);
        compositor_render::composite::Composite::draw(document, 1.0, &|point| point, &mut canvas);
        canvas.into_rgba()
    }

    /// Image > Flip Canvas Horizontal / Vertical (`flipCanvas(horizontally:)`): every layer, folder
    /// and placed mask, the selection and the guides — mirrored across the canvas' middle, one undo
    /// step named after the direction.
    pub fn flip_canvas(&mut self, horizontally: bool) {
        self.commit_transform();
        self.cancel_crop();
        if !self.can_edit_layers() {
            return;
        }
        let Some(current) = self.document.clone() else {
            return;
        };
        let axis = if horizontally {
            current.size().width / 2.0
        } else {
            current.size().height / 2.0
        };
        self.finish_opacity_edit();
        self.begin_edit(if horizontally {
            "Flip Canvas Horizontal"
        } else {
            "Flip Canvas Vertical"
        });
        if let Some(document) = self.document.as_mut() {
            let size = document.size();
            for layer in document.layers.iter_mut() {
                layer.transform = layer.transform.mirrored(horizontally, axis);
                if let Some(placement) =
                    layer.mask.as_mut().and_then(|mask| mask.placement.as_mut())
                {
                    *placement = placement.mirrored(horizontally, axis);
                }
            }
            if let Some(selection) = document.selection.as_mut() {
                let mirror = if horizontally {
                    AffineTransform::new(-1.0, 0.0, 0.0, 1.0, size.width, 0.0)
                } else {
                    AffineTransform::new(1.0, 0.0, 0.0, -1.0, 0.0, size.height)
                };
                selection.path = selection.path.transformed(&mirror);
            }
            document.guides = document
                .guides
                .iter()
                .map(|guide| guide.mirrored(horizontally, axis))
                .collect();
        }
        self.end_edit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::layer_mask::LayerMask as Mask;
    use compositor_pixels::trim::TrimBasedOn;

    fn layer_at(x: f64, y: f64, width: usize, height: usize) -> ImageLayer {
        let raster = Arc::new(Rgba8Image::opaque(width, height, [255, 0, 0, 255]));
        let asset = ImportedImage::new(
            PixelImage::Rgba(raster.clone()),
            PixelImage::Rgba(raster),
            "Layer",
        );
        let mut layer = ImageLayer::from_asset(asset, Point::new(x, y));
        layer.transform.size = Size::new(width as f64, height as f64);
        layer
    }

    fn document_with_layer(width: usize, height: usize, x: f64, y: f64) -> CanvasDocument {
        let mut document = CanvasDocument::new(width, height);
        document.layers = vec![layer_at(x, y, 10, 10)];
        document
    }

    #[test]
    fn canvas_size_anchors_place_the_old_content() {
        let document = document_with_layer(64, 32, 5.0, 7.0);
        // The expected offsets for a 64×32 canvas grown by 5 (and shrunk by 5), per anchor.
        let expected_x = [0.0, 2.0, 5.0];
        let expected_y = [0.0, 2.0, 5.0];
        for anchor in 0..9usize {
            let options = CanvasSizeOptions {
                anchor,
                ..CanvasSizeOptions::new(69, 37)
            };
            let resized = CanvasResizer::resize(&document, &options).expect("grows");
            let origin = resized.layers[0].transform.origin;
            assert_eq!(origin.x, 5.0 + expected_x[anchor % 3], "anchor {anchor}");
            assert_eq!(origin.y, 7.0 + expected_y[anchor / 3], "anchor {anchor}");
            assert_eq!((resized.width, resized.height), (69, 37));
        }
        // Shrinking around the center removes the extra pixel from the left and top.
        let options = CanvasSizeOptions {
            anchor: 4,
            ..CanvasSizeOptions::new(59, 27)
        };
        let resized = CanvasResizer::resize(&document, &options).expect("shrinks");
        let origin = resized.layers[0].transform.origin;
        assert_eq!(origin, Point::new(2.0, 4.0));
    }

    #[test]
    fn canvas_size_moves_guides_and_keeps_the_pixels_transparent() {
        let mut document = document_with_layer(8, 8, 1.0, 1.0);
        document.guides = vec![compositor_core::guides::CanvasGuide::at(
            compositor_core::guides::CanvasGuideAxis::Vertical,
            2.0,
        )];
        let options = CanvasSizeOptions {
            anchor: 0,
            ..CanvasSizeOptions::new(12, 12)
        };
        let resized = CanvasResizer::resize(&document, &options).expect("grows");
        assert_eq!(resized.guides[0].position, 2.0);
        assert_eq!(
            resized.layers.len(),
            1,
            "a transparent extension adds no layer"
        );
        assert_eq!(resized.layers[0].transform.origin, Point::new(1.0, 1.0));
    }

    #[test]
    fn colored_extension_adds_a_bottom_layer_with_the_old_canvas_cleared() {
        let document = CanvasDocument::new(8, 8);
        let options = CanvasSizeOptions {
            anchor: 4,
            fill: Some(CanvasExtensionColor::new(1.0, 0.0, 0.0)),
            ..CanvasSizeOptions::new(12, 12)
        };
        let resized = CanvasResizer::resize(&document, &options).expect("grows");
        assert_eq!(resized.layers.len(), 1);
        let layer = &resized.layers[0];
        assert_eq!(layer.name, "Canvas Extension");
        assert_eq!(layer.transform.origin, Point::ZERO);
        assert_eq!(layer.transform.size, Size::new(12.0, 12.0));
        let image = layer
            .asset
            .as_ref()
            .expect("pixels")
            .image
            .as_rgba()
            .expect("rgba");
        assert_eq!((image.width(), image.height()), (12, 12));
        assert_eq!(
            image.get(0, 0),
            [255, 0, 0, 255],
            "the added area is the fill color"
        );
        // The 8×8 canvas sat at (2, 2): its area, holes included, is cleared.
        assert_eq!(image.get(5, 5), [0, 0, 0, 0]);
        assert_eq!(image.get(11, 11), [255, 0, 0, 255]);
    }

    #[test]
    fn canvas_size_without_a_change_is_a_no_op() {
        let document = document_with_layer(8, 8, 1.0, 1.0);
        let options = CanvasSizeOptions::new(8, 8);
        let resized = CanvasResizer::resize(&document, &options).expect("unchanged");
        assert_eq!(resized, document);
    }

    #[test]
    fn image_size_resamples_each_layers_pixels_and_bounds() {
        let document = document_with_layer(100, 50, 10.0, 10.0);
        let options = ImageSizeOptions::new(200, 100, 144.0);
        let resized = ImageResizer::resize(&document, &options).expect("grows");
        assert_eq!((resized.width, resized.height), (200, 100));
        assert_eq!(resized.resolution, 144.0);
        let layer = &resized.layers[0];
        assert_eq!(layer.transform.origin, Point::new(20.0, 20.0));
        assert_eq!(layer.transform.size, Size::new(20.0, 20.0));
        let image = layer
            .asset
            .as_ref()
            .expect("pixels")
            .image
            .as_rgba()
            .expect("rgba");
        assert_eq!((image.width(), image.height()), (20, 20));
        assert_eq!(layer.asset.as_ref().unwrap().name, "Layer");
    }

    #[test]
    fn image_size_at_the_same_pixels_only_changes_the_resolution() {
        let mut document = document_with_layer(100, 50, 10.0, 10.0);
        document.guides = vec![compositor_core::guides::CanvasGuide::at(
            compositor_core::guides::CanvasGuideAxis::Horizontal,
            25.0,
        )];
        let options = ImageSizeOptions::new(100, 50, 300.0);
        let resized = ImageResizer::resize(&document, &options).expect("same size");
        assert_eq!(resized.resolution, 300.0);
        assert_eq!(resized.layers[0].transform, document.layers[0].transform);
        assert_eq!(resized.guides[0].position, 25.0);
        assert!(resized.layers[0]
            .asset
            .as_ref()
            .unwrap()
            .image
            .as_rgba()
            .is_some());
    }

    #[test]
    fn image_size_scales_mask_placements_with_the_canvas() {
        let mut document = CanvasDocument::new(100, 100);
        let mut layer = layer_at(0.0, 0.0, 10, 10);
        layer.mask = Some(Mask::solid(true).expect("a 1×1 reveal mask"));
        document.layers = vec![layer];
        let options = ImageSizeOptions::new(200, 200, 72.0);
        let resized = ImageResizer::resize(&document, &options).expect("grows");
        // A uniform mask keeps its pixels, not a canvas-sized copy.
        let mask = resized.layers[0].mask.as_ref().expect("mask");
        assert_eq!(
            (mask.asset.image.width(), mask.asset.image.height()),
            (1, 1)
        );
    }

    #[test]
    fn image_size_maps_mask_placements_to_the_new_canvas_scale() {
        // A placement at document (10, 10) over a 100×100 canvas must move to (20, 10) when the
        // canvas doubles only in x. The Swift composes the placement map first and the canvas
        // scale after (`.concatenating`, receiver-first); the reversed chain would leave the
        // origin at (10, 10).
        let mut document = CanvasDocument::new(100, 100);
        let mut layer = layer_at(0.0, 0.0, 10, 10);
        let mut mask = Mask::solid(true).expect("a 1×1 reveal mask");
        mask.placement = Some(LayerTransform {
            origin: Point::new(10.0, 10.0),
            size: Size::new(4.0, 6.0),
            ..Default::default()
        });
        layer.mask = Some(mask);
        document.layers = vec![layer];
        let options = ImageSizeOptions::new(200, 100, 72.0);
        let resized = ImageResizer::resize(&document, &options).expect("grows in x only");
        let mask = resized.layers[0].mask.as_ref().expect("mask");
        let placement = mask.placement.expect("kept placement");
        assert_eq!(placement.origin, Point::new(20.0, 10.0));
        assert_eq!(placement.size, Size::new(8.0, 6.0));
    }

    /// A raster with an opaque block on a transparent field.
    fn block_raster(width: usize, height: usize, rect: Rect) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in rect.min_y() as usize..(rect.max_y() as usize).min(height) {
            for x in rect.min_x() as usize..(rect.max_x() as usize).min(width) {
                image.set(x, y, [10, 20, 30, 255]);
            }
        }
        image
    }

    #[test]
    fn trim_edge_switches_keep_the_sides_they_leave_out() {
        let raster = block_raster(10, 10, Rect::new(3.0, 4.0, 2.0, 2.0));
        let cases = [
            ((true, true, true, true), Some(Rect::new(3.0, 4.0, 2.0, 2.0))),
            ((false, true, true, true), Some(Rect::new(3.0, 0.0, 2.0, 6.0))),
            ((true, false, true, true), Some(Rect::new(3.0, 4.0, 2.0, 6.0))),
            ((true, true, false, true), Some(Rect::new(0.0, 4.0, 5.0, 2.0))),
            ((true, true, true, false), Some(Rect::new(3.0, 4.0, 7.0, 2.0))),
            // No edge enabled: nothing to trim at all.
            ((false, false, false, false), None),
        ];
        for ((top, bottom, left, right), expected) in cases {
            let options = TrimOptions::new(TrimBasedOn::TransparentPixels, top, bottom, left, right, 0);
            assert_eq!(calculate_trim_rect(&raster, &options), expected, "{options:?}");
        }
    }

    #[test]
    fn trim_on_transparent_pixels_finds_the_content_bounds() {
        let raster = block_raster(8, 8, Rect::new(2.0, 3.0, 3.0, 3.0));
        let rect = calculate_trim_rect(&raster, &TrimOptions::default()).expect("content");
        assert_eq!(rect, Rect::new(2.0, 3.0, 3.0, 3.0));
        // A fully transparent image has no content to trim.
        let empty = Rgba8Image::new(8, 8);
        assert_eq!(calculate_trim_rect(&empty, &TrimOptions::default()), None);
        // The edge switches keep the outermost pixel on the sides they leave out.
        let mut options = TrimOptions::default();
        options.left = false;
        options.top = false;
        let rect = calculate_trim_rect(&raster, &options).expect("content");
        assert_eq!(rect, Rect::new(0.0, 0.0, 5.0, 6.0));
    }

    #[test]
    fn trim_on_the_top_left_pixel_color_keeps_the_rest() {
        let mut raster = Rgba8Image::opaque(6, 6, [0, 0, 255, 255]);
        raster.set(3, 3, [255, 0, 0, 255]);
        let options = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            ..TrimOptions::default()
        };
        let rect = calculate_trim_rect(&raster, &options).expect("content");
        assert_eq!(rect, Rect::new(3.0, 3.0, 1.0, 1.0));
        // One solid color leaves nothing.
        assert_eq!(
            calculate_trim_rect(&Rgba8Image::opaque(6, 6, [0, 0, 255, 255]), &options),
            None
        );
    }

    #[test]
    fn trim_on_the_bottom_right_pixel_color_keeps_the_rest() {
        let mut raster = Rgba8Image::opaque(6, 6, [0, 0, 255, 255]);
        raster.set(1, 1, [0, 255, 0, 255]);
        raster.set(4, 0, [0, 255, 0, 255]);
        let options = TrimOptions {
            based_on: TrimBasedOn::BottomRightPixelColor,
            ..TrimOptions::default()
        };
        let rect = calculate_trim_rect(&raster, &options).expect("content");
        assert_eq!(rect, Rect::new(1.0, 0.0, 4.0, 2.0));
    }

    #[test]
    fn trim_tolerance_widens_what_counts_as_the_sample_color() {
        let mut raster = Rgba8Image::opaque(4, 4, [100, 100, 100, 255]);
        raster.set(2, 2, [110, 100, 100, 255]);
        let mut options = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            ..TrimOptions::default()
        };
        // Within the tolerance the pixel matches the sample and is trimmed away.
        options.tolerance = 16;
        assert_eq!(calculate_trim_rect(&raster, &options), None);
        // Below it, the pixel is content.
        options.tolerance = 4;
        let rect = calculate_trim_rect(&raster, &options).expect("content");
        assert_eq!(rect, Rect::new(2.0, 2.0, 1.0, 1.0));
    }

    #[test]
    fn canvas_presets_keep_the_menu_order_and_sizes() {
        let all = CanvasPreset::all();
        assert_eq!(all.len(), 12);
        assert_eq!(all[0].title, "4K");
        assert_eq!((all[0].width, all[0].height), (3840, 2160));
        assert_eq!(all[2].title, "1080p");
        assert_eq!(all[3].title, "iPhone 18 Pro");
        assert_eq!(all[6].title, "MacBook Pro 16\"");
        assert_eq!((all[6].width, all[6].height), (3456, 2234));
        assert_eq!(all[11].title, "YouTube Thumb");
        assert_eq!((all[11].width, all[11].height), (1080, 608));
        assert_eq!(
            CanvasPreset::matching(1080, 1080).unwrap().title,
            "Instagram Square"
        );
        assert_eq!(CanvasPreset::matching(1000, 1000), None);
        assert_eq!(CanvasPreset::GROUPS.len(), 3);
        assert_eq!(CanvasPreset::GROUPS[1].len(), 5);
        assert_eq!(CanvasPreset::GROUPS[2].len(), 4);
    }

    #[test]
    fn trim_rects_become_canvas_options_with_a_content_offset() {
        let options = trim_canvas_options(Rect::new(2.0, 3.0, 5.0, 4.0));
        assert_eq!((options.width, options.height), (5, 4));
        assert_eq!(options.content_offset, Some(Point::new(-2.0, -3.0)));
        assert!(trim_is_noop(Rect::new(0.0, 0.0, 8.0, 6.0), 8, 6));
        assert!(!trim_is_noop(Rect::new(1.0, 0.0, 8.0, 6.0), 8, 6));
    }

    #[test]
    fn crop_options_carry_the_content_offset() {
        let options = crate::crop::crop_options(Rect::new(4.0, 5.0, 10.0, 20.0));
        assert_eq!((options.width, options.height), (10, 20));
        assert_eq!(options.content_offset, Some(Point::new(-4.0, -5.0)));
        assert_eq!(options.anchor, 4);
    }

    fn session_with_document(width: usize, height: usize) -> EditorSession {
        let mut session = EditorSession::default();
        let mut document = CanvasDocument::new(width, height);
        document.layers = vec![layer_at(10.0, 5.0, 10, 10)];
        session.document = Some(document);
        session
    }

    #[test]
    fn canvas_size_command_is_one_undo_step_named_canvas_size() {
        let mut session = session_with_document(64, 32);
        let options = CanvasSizeOptions {
            anchor: 8,
            ..CanvasSizeOptions::new(74, 42)
        };
        session.change_canvas_size(&options).expect("resized");
        assert!(!session.is_project_busy);
        let document = session.document.as_ref().expect("document");
        assert_eq!((document.width, document.height), (74, 42));
        assert_eq!(document.layers[0].transform.origin, Point::new(20.0, 15.0));
        assert_eq!(session.history.undo_name(), "Canvas Size");
    }

    #[test]
    fn image_size_command_is_one_undo_step_named_image_size() {
        let mut session = session_with_document(100, 50);
        session
            .change_image_size(&ImageSizeOptions::new(200, 100, 150.0))
            .expect("resized");
        let document = session.document.as_ref().expect("document");
        assert_eq!((document.width, document.height), (200, 100));
        assert_eq!(document.resolution, 150.0);
        assert_eq!(document.layers[0].transform.size, Size::new(20.0, 20.0));
        assert_eq!(session.history.undo_name(), "Image Size");
    }

    #[test]
    fn apply_document_size_ignores_another_documents_snapshot() {
        let mut session = session_with_document(64, 32);
        let other = CanvasDocument::new(8, 8);
        session.apply_document_size(other, "Canvas Size");
        assert_eq!(session.document.as_ref().unwrap().width, 64);
        assert!(!session.history.can_undo());
        // The same id is installed, one undo step, and the view fits the new size.
        let current = session.document.as_ref().unwrap();
        let same =
            CanvasDocument::with_id(current.id, 40, 20, current.layers.clone(), 72.0, Vec::new());
        session.apply_document_size(same, "Canvas Size");
        assert_eq!(session.document.as_ref().unwrap().width, 40);
        assert_eq!(session.history.undo_name(), "Canvas Size");
    }

    #[test]
    fn flip_canvas_mirrors_content_guides_and_selection() {
        use compositor_core::path::Path;
        let mut session = session_with_document(100, 50);
        session.document.as_mut().unwrap().guides = vec![compositor_core::guides::CanvasGuide::at(
            compositor_core::guides::CanvasGuideAxis::Vertical,
            30.0,
        )];
        session.document.as_mut().unwrap().selection =
            Some(compositor_core::selection::DocumentSelection::new(
                Path::rect(Rect::new(10.0, 5.0, 20.0, 10.0)),
            ));
        session.flip_canvas(true);
        let document = session.document.as_ref().expect("document");
        assert_eq!(document.layers[0].transform.origin, Point::new(80.0, 5.0));
        assert!(document.layers[0].transform.flip_x);
        assert_eq!(document.guides[0].position, 70.0);
        let bounds = document
            .selection
            .as_ref()
            .expect("selection")
            .path
            .bounding_box();
        assert_eq!(bounds, Rect::new(70.0, 5.0, 20.0, 10.0));
        assert_eq!(session.history.undo_name(), "Flip Canvas Horizontal");

        session.flip_canvas(false);
        let document = session.document.as_ref().expect("document");
        assert_eq!(document.layers[0].transform.origin, Point::new(80.0, 35.0));
        assert!(document.layers[0].transform.flip_y);
        assert_eq!(session.history.undo_name(), "Flip Canvas Vertical");
    }
}
