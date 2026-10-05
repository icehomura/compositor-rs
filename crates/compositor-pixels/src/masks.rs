//! Mask and selection algorithms: the Magic Wand, Select > Color Range, mask tracing, guided-filter
//! matting, the on-device subject/matte extraction and the content-aware fill.
//!
//! Ports `Document/MagicWand.swift`, `Document/MaskTracing.swift`, `Document/GuidedMatte.swift`,
//! `Document/SubjectRemoval.swift`, `Document/ObjectSelection.swift` and `Document/ContentFill.swift`,
//! plus the mask Select > Color Range builds in `Document/ColorRangeSelection.swift`. Matching and
//! tracing call the C kernels in [`crate::wand_pixels`], so the tolerance and contiguity semantics
//! stay byte-for-byte the ones `WandPixels.h` documents.
//!
//! # Vision and Core Image
//!
//! Vision (`VNGenerateForegroundInstanceMaskRequest`, `CIEdgePreserveUpsampleFilter`) has no
//! cross-platform equivalent, so the port detects the foreground instances on the CPU
//! ([`ForegroundInstances`]): a k-means background model over the image's border ring, a normalized
//! RGB distance test, connected components and an area filter, all at a working resolution and
//! scaled up for the caller. Everything downstream of that mask is the Swift algorithm unchanged —
//! [`GuidedMatte`]'s guided filter, the blur-and-threshold edge shift, the matte contrast stretch,
//! the erosion/dilation steps and the outline tracing — and the Core Image chains those steps used
//! are computed here as plain 8-bit sRGB arithmetic, as `docs/PORTING.md` §4 requires.

use std::cmp::Ordering;
use std::sync::{LazyLock, Mutex};

use compositor_core::geom::{AffineTransform, Point, Rect, Size};
use compositor_core::image_ops::{BackgroundQuality, FilterJob, FilterSettings};
use compositor_core::path::{Path, PathElement};
use compositor_core::selection::SelectionClip;
use compositor_core::{Gray8Image, Rgba8Image};
use rayon::prelude::*;

use crate::canvas::{Canvas, InterpolationQuality};
use crate::wand_pixels::{
    color_range_mask as color_range_kernel, wand_mask, wand_trace, WandTraceError,
};

// MARK: - MagicWand.swift

/// `WandSampleSize`: how much of the neighborhood around the click is averaged into the color to
/// match.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WandSampleSize {
    #[default]
    Point,
    ThreeByThree,
    FiveByFive,
}

impl WandSampleSize {
    /// `CaseIterable`, in the Swift case order.
    pub const ALL: [WandSampleSize; 3] = [
        WandSampleSize::Point,
        WandSampleSize::ThreeByThree,
        WandSampleSize::FiveByFive,
    ];

    /// The case's `Int` raw value (`CaseIterable` index), the number the tool defaults store.
    pub fn raw_value(self) -> usize {
        match self {
            WandSampleSize::Point => 0,
            WandSampleSize::ThreeByThree => 1,
            WandSampleSize::FiveByFive => 2,
        }
    }

    pub fn from_raw(value: usize) -> Option<Self> {
        Self::ALL.get(value).copied()
    }

    pub fn title(self) -> &'static str {
        ["Point Sample", "3 by 3 Average", "5 by 5 Average"][self.raw_value()]
    }

    /// Pixels either side of the click that are averaged into the color to match.
    pub fn radius(self) -> usize {
        self.raw_value()
    }
}

/// The Magic Wand's options-bar settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WandSettings {
    /// How far (0–255) each channel may differ from the sampled color and still be selected.
    pub tolerance: i32,
    pub sample_size: WandSampleSize,
    /// Only similar pixels connected to the clicked one, rather than every similar pixel.
    pub contiguous: bool,
    /// Read the visible composite rather than just the active layer.
    pub sample_all_layers: bool,
}

impl Default for WandSettings {
    fn default() -> Self {
        WandSettings {
            tolerance: 32,
            sample_size: WandSampleSize::Point,
            contiguous: true,
            sample_all_layers: false,
        }
    }
}

/// `MagicWand.Failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MagicWandError {
    /// The outline has too many pixel edges to be worth drawing.
    TooDetailed,
    /// A buffer could not be allocated. Unreachable here — Rust aborts rather than returning null —
    /// but kept because the Swift error carries its own message.
    Memory,
}

impl std::fmt::Display for MagicWandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MagicWandError::TooDetailed => formatter.write_str(
                "That selection is too detailed to outline. Try a different Tolerance, or turn on Contiguous.",
            ),
            MagicWandError::Memory => {
                formatter.write_str("There isn’t enough memory to make that selection.")
            }
        }
    }
}

impl std::error::Error for MagicWandError {}

impl From<WandTraceError> for MagicWandError {
    fn from(error: WandTraceError) -> Self {
        match error {
            WandTraceError::TooDetailed => MagicWandError::TooDetailed,
            WandTraceError::OutOfMemory => MagicWandError::Memory,
        }
    }
}

/// Selects pixels similar to a clicked one. Matching and tracing run in the C kernels
/// (`WandPixels.c`): in Swift they would crawl on a large canvas in an unoptimized build.
pub struct MagicWand;

impl MagicWand {
    /// The outline, in the image's top-left pixel coordinates, of the pixels matching the one at
    /// `point`. `None` when nothing matches or the point is outside the image.
    ///
    /// The Swift drew the image into a fresh premultiplied RGBA context first; an [`Rgba8Image`] is
    /// already that canonical buffer, so the normalization is the identity and the image's own
    /// pixels are matched directly.
    pub fn select(
        image: &Rgba8Image,
        at: Point,
        settings: &WandSettings,
    ) -> Result<Option<Path>, MagicWandError> {
        let width = image.width();
        let height = image.height();
        let x = at.x.floor() as i64;
        let y = at.y.floor() as i64;
        if !at.x.is_finite()
            || !at.y.is_finite()
            || x < 0
            || y < 0
            || x >= width as i64
            || y >= height as i64
        {
            return Ok(None);
        }
        let mut selected = vec![0u8; width * height];
        let count = wand_mask(
            image.data(),
            width,
            height,
            image.stride(),
            x as usize,
            y as usize,
            settings.sample_size.radius(),
            settings.tolerance.clamp(0, 255),
            settings.contiguous as i32,
            &mut selected,
        );
        // `count < 0` (allocation failure) cannot happen in Rust; `count == 0` is "nothing matched".
        if count <= 0 {
            return Ok(None);
        }
        Self::outline(&selected, width, height)
    }

    /// Outline of a mask's nonzero pixels along exact pixel edges (winding rule); `None` when empty.
    ///
    /// The C kernel's loops are corners in pixel-edge coordinates with the hole boundaries running
    /// the other way around, so the winding fill reproduces exactly the traced pixels.
    pub fn outline(
        mask: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Option<Path>, MagicWandError> {
        if width == 0 || height == 0 || mask.len() != width * height {
            return Ok(None);
        }
        let traced = match wand_trace(mask, width, height) {
            Ok(Some(traced)) => traced,
            Ok(None) => return Ok(None),
            Err(error) => return Err(MagicWandError::from(error)),
        };
        let (points, loops) = traced;
        if loops.is_empty() {
            return Ok(None);
        }
        let mut path = Path::empty();
        let mut index = 0usize;
        for length in loops {
            if index + length > points.len() / 2 {
                break;
            }
            let corners: Vec<Point> = (index..index + length)
                .map(|corner| Point::new(points[corner * 2] as f64, points[corner * 2 + 1] as f64))
                .collect();
            add_loop(&mut path, &corners);
            index += length;
        }
        Ok(Some(path))
    }
}

/// `CGMutablePath.addLines(between:)` followed by `closeSubpath()`: a move to the first point, lines
/// to the rest, and a closed subpath.
fn add_loop(path: &mut Path, corners: &[Point]) {
    let Some(first) = corners.first() else {
        return;
    };
    path.move_to(*first);
    path.add_lines(&corners[1..]);
    path.close_subpath();
}

/// Select > Color Range's mask (`ColorRangeSelection.colorRangeMask`): the C kernel over the image's
/// own premultiplied pixels. `include` and `exclude` hold straight sRGB colors, 3 bytes each.
pub fn color_range_mask(
    image: &Rgba8Image,
    include: &[u8],
    exclude: &[u8],
    fuzziness: i32,
    invert: bool,
) -> Vec<u8> {
    let width = image.width();
    let height = image.height();
    let mut mask = vec![0u8; width * height];
    color_range_kernel(
        image.data(),
        width,
        height,
        image.stride(),
        include,
        (include.len() / 3) as i32,
        exclude,
        (exclude.len() / 3) as i32,
        fuzziness,
        invert as i32,
        &mut mask,
    );
    mask
}

/// Select > Color Range's selection (`EditorSession.colorRangeResult`): the mask's outline, `None`
/// when nothing matched.
pub fn color_range_path(
    image: &Rgba8Image,
    include: &[u8],
    exclude: &[u8],
    fuzziness: i32,
    invert: bool,
) -> Result<Option<Path>, MagicWandError> {
    let mask = color_range_mask(image, include, exclude, fuzziness, invert);
    if mask.iter().all(|&value| value == 0) {
        return Ok(None);
    }
    MagicWand::outline(&mask, image.width(), image.height())
}

// MARK: - MaskTracing.swift

/// Turns raster coverage into a selection outline along exact pixel edges.
pub struct MaskTracing;

impl MaskTracing {
    /// Outline of a mask's pixels darker than 50% gray.
    pub fn dark_pixels(image: &Gray8Image) -> Option<Path> {
        Self::trace(image.data(), image.width(), image.height(), 1, 0, |value| {
            value < 128
        })
    }

    /// Outline of a mask's pixels lighter than 50% gray — what a mask shows.
    pub fn white_pixels(image: &Gray8Image) -> Option<Path> {
        Self::trace(image.data(), image.width(), image.height(), 1, 0, |value| {
            value >= 128
        })
    }

    /// Outline of an image's pixels that are at least 50% opaque.
    pub fn opaque_pixels(image: &Rgba8Image) -> Option<Path> {
        Self::trace(image.data(), image.width(), image.height(), 4, 3, |value| {
            value >= 128
        })
    }

    /// Outline, in the image's top-left pixel coordinates, of pixels whose gray value (or alpha)
    /// passes `test`. Outer boundaries run clockwise and holes counterclockwise, so the winding fill
    /// rule reproduces exactly the traced pixels. `None` when none pass.
    ///
    /// The Swift drew the image into a fresh gray (or premultiplied RGBA) bitmap and read the tested
    /// channel from it; the canonical buffers already hold those bytes, so `bytes` is read directly
    /// at `channels` bytes a pixel with the tested channel at `offset`.
    fn trace(
        bytes: &[u8],
        width: usize,
        height: usize,
        channels: usize,
        offset: usize,
        test: impl Fn(u8) -> bool,
    ) -> Option<Path> {
        if width == 0 || height == 0 || bytes.len() < width * height * channels || channels == 0 {
            return None;
        }
        let selected = |x: i64, y: i64| -> bool {
            x >= 0
                && y >= 0
                && x < width as i64
                && y < height as i64
                && test(bytes[(y as usize * width + x as usize) * channels + offset])
        };
        // Directed unit edges between selected and unselected pixels, keyed by their start vertex.
        // Swift iterated a `Dictionary`; the loops are the same whichever one is walked first, and a
        // `BTreeMap` makes the order (and the path it builds) deterministic across runs.
        let stride = width + 1;
        let mut outgoing: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        let mut edge = |x0: i64, y0: i64, x1: i64, y1: i64| {
            outgoing
                .entry(y0 as usize * stride + x0 as usize)
                .or_default()
                .push(y1 as usize * stride + x1 as usize);
        };
        for y in 0..height as i64 {
            for x in 0..width as i64 {
                if !selected(x, y) {
                    continue;
                }
                if !selected(x, y - 1) {
                    edge(x, y, x + 1, y);
                }
                if !selected(x + 1, y) {
                    edge(x + 1, y, x + 1, y + 1);
                }
                if !selected(x, y + 1) {
                    edge(x + 1, y + 1, x, y + 1);
                }
                if !selected(x - 1, y) {
                    edge(x, y + 1, x, y);
                }
            }
        }
        if outgoing.is_empty() {
            return None;
        }
        let mut path = Path::empty();
        while let Some((&start, _)) = outgoing.iter().next() {
            let mut walked: Vec<usize> = Vec::new();
            let mut current = start;
            loop {
                let Some(ends) = outgoing.get_mut(&current) else {
                    break;
                };
                let Some(end) = ends.pop() else {
                    // Unreachable: a vertex only keeps a key while it has an edge left. Clearing it
                    // keeps a corrupted map from spinning forever.
                    outgoing.remove(&current);
                    break;
                };
                if ends.is_empty() {
                    outgoing.remove(&current);
                }
                walked.push(current);
                current = end;
                if current == start {
                    break;
                }
            }
            // Keep only corners: drop vertices that continue in a straight line.
            let mut corners: Vec<Point> = Vec::new();
            for (index, vertex) in walked.iter().enumerate() {
                let previous = walked[(index + walked.len() - 1) % walked.len()];
                let following = walked[(index + 1) % walked.len()];
                let in_x = vertex % stride - previous % stride;
                let in_y = vertex / stride - previous / stride;
                let out_x = following % stride - vertex % stride;
                let out_y = following / stride - vertex / stride;
                if in_x != out_x || in_y != out_y {
                    corners.push(Point::new(
                        (vertex % stride) as f64,
                        (vertex / stride) as f64,
                    ));
                }
            }
            if corners.len() >= 3 {
                add_loop(&mut path, &corners);
            }
        }
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
}

// MARK: - GuidedMatte.swift

/// Guided filtering (He, Sun & Tang): a mask pulled onto the edges of the image it came from, which
/// is what recovers hair and fur that a segmentation model cuts straight through. Core Image's own
/// `CIGuidedFilter` does nothing on this system and its edge-preserving upsample barely moves the
/// mask, so this does the arithmetic directly.
pub struct GuidedMatte;

impl GuidedMatte {
    /// `box(_:width:height:radius:)` — `box` is a reserved word in Rust. Mean over a (2r+1)² square,
    /// as two running-sum passes — the cost doesn't grow with the radius.
    ///
    /// The Swift ran the second pass down each column; the port transposes around a row pass, which
    /// is the same arithmetic in the same order with the sums staying in cache.
    pub fn box_mean(source: &[f32], width: usize, height: usize, radius: usize) -> Vec<f32> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        assert_eq!(source.len(), width * height, "box source size mismatch");
        let pass = rows_pass(source, width, height, radius);
        let transposed = transpose(&pass, width, height);
        let vertical = rows_pass(&transposed, height, width, radius);
        transpose(&vertical, height, width)
    }

    /// `mask` refined by `guide` (both 0–1, the same size). A bigger radius reaches further for
    /// detail; `epsilon` decides how much of an edge in the guide counts, so a small one follows fine
    /// strands.
    pub fn filter(
        mask: &[f32],
        guide: &[f32],
        width: usize,
        height: usize,
        radius: usize,
        epsilon: f32,
    ) -> Vec<f32> {
        let count = width * height;
        assert_eq!(mask.len(), count, "filter mask size mismatch");
        assert_eq!(guide.len(), count, "filter guide size mismatch");
        if count == 0 {
            return Vec::new();
        }
        let mean_guide = Self::box_mean(guide, width, height, radius);
        let mean_mask = Self::box_mean(mask, width, height, radius);
        let mut squares = vec![0f32; count];
        let mut products = vec![0f32; count];
        for i in 0..count {
            squares[i] = guide[i] * guide[i];
            products[i] = guide[i] * mask[i];
        }
        let mean_squares = Self::box_mean(&squares, width, height, radius);
        let mean_products = Self::box_mean(&products, width, height, radius);
        let mut slope = vec![0f32; count];
        let mut offset = vec![0f32; count];
        for i in 0..count {
            let variance = mean_squares[i] - mean_guide[i] * mean_guide[i];
            let covariance = mean_products[i] - mean_guide[i] * mean_mask[i];
            slope[i] = covariance / (variance + epsilon);
            offset[i] = mean_mask[i] - slope[i] * mean_guide[i];
        }
        let mean_slope = Self::box_mean(&slope, width, height, radius);
        let mean_offset = Self::box_mean(&offset, width, height, radius);
        let mut result = vec![0f32; count];
        for i in 0..count {
            let value = mean_slope[i] * guide[i] + mean_offset[i];
            // Swift's `min(1, max(0, value))` answers 0 for a NaN; `clamp` would keep it.
            result[i] = if value.is_nan() {
                0.0
            } else {
                value.clamp(0.0, 1.0)
            };
        }
        result
    }

    /// `levels(of:width:height:)` for a gray image: its 0–1 levels, drawn at `width` × `height`.
    pub fn levels_gray(image: &Gray8Image, width: usize, height: usize) -> Vec<f32> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let mut canvas = Canvas::new_gray(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        if !image.is_empty() {
            canvas.draw_gray(image, Rect::new(0.0, 0.0, width as f64, height as f64));
        }
        let scaled = canvas.into_gray();
        scaled
            .data()
            .iter()
            .map(|&byte| byte as f32 / 255.0)
            .collect()
    }

    /// `levels(of:width:height:)` for a color image: its gray levels, drawn at `width` × `height`.
    ///
    /// Core Graphics converted the image into a DeviceGray context with a color-managed transform;
    /// the port resamples in sRGB and takes Rec. 709 luma of the (premultiplied, i.e. composited over
    /// black) components, the gamma-space conversion `docs/PORTING.md` §4 prescribes.
    pub fn levels_rgba(image: &Rgba8Image, width: usize, height: usize) -> Vec<f32> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let mut canvas = Canvas::new_rgba(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        if !image.is_empty() {
            canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
        }
        let scaled = canvas.into_rgba();
        scaled
            .pixels()
            .map(|pixel| {
                (0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32)
                    / 255.0
            })
            .collect()
    }

    /// 0–1 levels back to a gray image (`image(_:width:height:)`).
    pub fn gray_image(levels: &[f32], width: usize, height: usize) -> Gray8Image {
        let count = width * height;
        assert_eq!(levels.len(), count, "gray image size mismatch");
        let bytes = levels
            .iter()
            .map(|&level| (level * 255.0 + 0.5).clamp(0.0, 255.0) as u8)
            .collect();
        Gray8Image::from_data(width, height, bytes)
    }

    /// `mask` refined against `guide`, both full size. Done on a copy no larger than `limit` on its
    /// longest side (the radius shrinks with it), then drawn back up: fine detail comes from the
    /// guide either way, and a preview stays quick to redraw while a slider moves.
    pub fn refine(mask: &Gray8Image, guide: &Rgba8Image, radius: f64, limit: f64) -> Gray8Image {
        let full_width = mask.width();
        let full_height = mask.height();
        if full_width == 0 || full_height == 0 {
            return mask.clone();
        }
        let full = Size::new(full_width as f64, full_height as f64);
        let factor = (limit / full.width.max(full.height)).min(1.0);
        let width = ((full.width * factor).round() as usize).max(1);
        let height = ((full.height * factor).round() as usize).max(1);
        let steps = ((radius * factor).round().max(1.0)) as usize;
        let refined = Self::filter(
            &Self::levels_gray(mask, width, height),
            &Self::levels_rgba(guide, width, height),
            width,
            height,
            steps,
            1e-4,
        );
        let small = Self::gray_image(&refined, width, height);
        if width == full_width && height == full_height {
            return small;
        }
        let mut canvas = Canvas::new_gray(full_width, full_height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        canvas.draw_gray(&small, Rect::new(0.0, 0.0, full.width, full.height));
        canvas.into_gray()
    }
}

/// One sliding-sum pass along every row, reading with the edges clamped (`min(width-1, max(0, x))`).
fn rows_pass(source: &[f32], width: usize, height: usize, radius: usize) -> Vec<f32> {
    let span = (radius * 2 + 1) as f32;
    let reach = radius as isize;
    let last = width as isize - 1;
    let mut result = vec![0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, out_row)| {
            let row = y * width;
            let mut sum = 0f32;
            for dx in -reach..=reach {
                sum += source[row + dx.clamp(0, last) as usize];
            }
            for x in 0..width {
                out_row[x] = sum / span;
                sum -= source[row + ((x as isize - reach).clamp(0, last)) as usize];
                sum += source[row + ((x as isize + reach + 1).clamp(0, last)) as usize];
            }
        });
    result
}

/// A (height × width) transposition of a (width × height) buffer.
fn transpose(source: &[f32], width: usize, height: usize) -> Vec<f32> {
    let mut result = vec![0f32; width * height];
    result
        .par_chunks_mut(height)
        .enumerate()
        .for_each(|(x, out_column)| {
            for y in 0..height {
                out_column[y] = source[y * width + x];
            }
        });
    result
}

// MARK: - The on-device subject extraction (Vision's replacement)

/// The longest side the segmentation works at, in pixels. Vision's instance masks come back at the
/// model's own, much lower resolution; this is the port's stand-in for it, and the mask is scaled up
/// to the image's size from here.
const SUBJECT_WORKING_SIDE: usize = 256;

/// Images smaller than this on a side have no distinguishable subject.
const SUBJECT_MIN_SIDE: usize = 8;

/// How many colors the border background model keeps.
const BACKGROUND_CLUSTERS: usize = 4;

/// Lloyd iterations for the border background model.
const BACKGROUND_ITERATIONS: usize = 8;

/// The share of the shorter side treated as the image's background border.
const BACKGROUND_BORDER_DIVISOR: usize = 32;

/// How far (Euclidean distance over the RGB cube, normalized to 0–1) a pixel must be from every
/// color of the background model to count as foreground.
const BACKGROUND_DISTANCE: f32 = 0.18;

/// A component smaller than this share of the working pixels is noise, not an object.
const MIN_INSTANCE_SHARE: f32 = 0.002;

/// …and never smaller than this many pixels.
const MIN_INSTANCE_PIXELS: usize = 24;

/// The label map stores an instance index in a byte, so there can be at most 255 of them.
const MAX_INSTANCES: usize = 255;

/// The foreground instances a segmentation found, at the working resolution — the port's replacement
/// for Vision's `VNGenerateForegroundInstanceMaskRequest` observation.
struct ForegroundInstances {
    /// 0 is background; 1… are instance indices, largest instance first.
    labels: Gray8Image,
}

impl ForegroundInstances {
    /// Vision's request, on the CPU: `None` when nothing separates from the background.
    fn detect(image: &Rgba8Image) -> Option<Self> {
        if image.is_empty() {
            return None;
        }
        let (full_width, full_height) = (image.width(), image.height());
        if full_width < SUBJECT_MIN_SIDE || full_height < SUBJECT_MIN_SIDE {
            return None;
        }
        let scale = SUBJECT_WORKING_SIDE as f64 / full_width.max(full_height) as f64;
        let (width, height) = if scale < 1.0 {
            (
                ((full_width as f64) * scale).round().max(1.0) as usize,
                ((full_height as f64) * scale).round().max(1.0) as usize,
            )
        } else {
            (full_width, full_height)
        };
        let working = resize_rgba(image, width, height);
        let background = background_colors(&working);
        if background.is_empty() {
            return None;
        }
        let mut foreground = vec![false; width * height];
        for y in 0..height {
            for x in 0..width {
                let pixel = working.get(x, y);
                let rgb = [
                    pixel[0] as f32 / 255.0,
                    pixel[1] as f32 / 255.0,
                    pixel[2] as f32 / 255.0,
                ];
                foreground[y * width + x] =
                    background_distance(&background, &rgb) > BACKGROUND_DISTANCE;
            }
        }
        let labels = label_instances(&foreground, width, height);
        if labels.data().iter().all(|&label| label == 0) {
            return None;
        }
        Some(ForegroundInstances { labels })
    }

    /// `observation.instanceMask` read at `point`: the instance index under the point, in the
    /// image's own size. `None` over the background or outside.
    fn instance_index(&self, at: Point, image_size: Size) -> Option<u8> {
        let width = self.labels.width();
        let height = self.labels.height();
        if width == 0 || height == 0 || !(image_size.width > 0.0) || !(image_size.height > 0.0) {
            return None;
        }
        let x = (at.x / image_size.width * width as f64) as i64;
        let y = (at.y / image_size.height * height as f64) as i64;
        let x = x.clamp(0, width as i64 - 1) as usize;
        let y = y.clamp(0, height as i64 - 1) as usize;
        let value = self.labels.get(x, y);
        if value == 0 {
            None
        } else {
            Some(value)
        }
    }

    /// `observation.generateMask(forInstances:)`: one instance's mask at the working resolution.
    fn mask(&self, instance: u8) -> Gray8Image {
        Gray8Image::from_data(
            self.labels.width(),
            self.labels.height(),
            self.labels
                .data()
                .iter()
                .map(|&label| if label == instance { 255 } else { 0 })
                .collect(),
        )
    }

    /// `observation.generateScaledMaskForImage(forInstances:from:)`: every instance, scaled up to the
    /// image's own size.
    fn scaled_mask(&self, width: usize, height: usize) -> Gray8Image {
        if width == 0 || height == 0 {
            return Gray8Image::new(0, 0);
        }
        let union = Gray8Image::from_data(
            self.labels.width(),
            self.labels.height(),
            self.labels
                .data()
                .iter()
                .map(|&label| if label != 0 { 255 } else { 0 })
                .collect(),
        );
        let mut canvas = Canvas::new_gray(width, height);
        canvas.set_interpolation_quality(InterpolationQuality::High);
        canvas.draw_gray(&union, Rect::new(0.0, 0.0, width as f64, height as f64));
        canvas.into_gray()
    }
}

/// The image resampled to `width` × `height` with high interpolation.
fn resize_rgba(image: &Rgba8Image, width: usize, height: usize) -> Rgba8Image {
    let mut canvas = Canvas::new_rgba(width, height);
    canvas.set_interpolation_quality(InterpolationQuality::High);
    canvas.draw_image(image, Rect::new(0.0, 0.0, width as f64, height as f64));
    canvas.into_rgba()
}

/// The colors of the image's border ring, as a k-means model with a deterministic initialization
/// (the ring sorted by luma, the cluster centers seeded at its quantiles).
fn background_colors(image: &Rgba8Image) -> Vec<[f32; 3]> {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let depth = (width.min(height) / BACKGROUND_BORDER_DIVISOR).max(1);
    let mut samples: Vec<[f32; 3]> = Vec::new();
    for y in 0..height {
        for x in 0..width {
            if x >= depth && y >= depth && x + depth < width && y + depth < height {
                continue;
            }
            let pixel = image.get(x, y);
            samples.push([
                pixel[0] as f32 / 255.0,
                pixel[1] as f32 / 255.0,
                pixel[2] as f32 / 255.0,
            ]);
        }
    }
    if samples.is_empty() {
        return Vec::new();
    }
    samples.sort_by(|a, b| luma(a).partial_cmp(&luma(b)).unwrap_or(Ordering::Equal));
    let mut centers: Vec<[f32; 3]> = (0..BACKGROUND_CLUSTERS)
        .map(|index| {
            let position =
                ((index as f64 + 0.5) / BACKGROUND_CLUSTERS as f64 * samples.len() as f64) as usize;
            samples[position.min(samples.len() - 1)]
        })
        .collect();
    for _ in 0..BACKGROUND_ITERATIONS {
        let mut sums = [[0f64; 3]; BACKGROUND_CLUSTERS];
        let mut counts = [0usize; BACKGROUND_CLUSTERS];
        for sample in &samples {
            let index = nearest_center(&centers, sample);
            for channel in 0..3 {
                sums[index][channel] += sample[channel] as f64;
            }
            counts[index] += 1;
        }
        for index in 0..BACKGROUND_CLUSTERS {
            if counts[index] == 0 {
                continue;
            }
            for channel in 0..3 {
                centers[index][channel] = (sums[index][channel] / counts[index] as f64) as f32;
            }
        }
    }
    centers
}

fn luma(rgb: &[f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// The nearest background cluster to `rgb`.
fn nearest_center(centers: &[[f32; 3]], rgb: &[f32; 3]) -> usize {
    let mut best = 0usize;
    let mut best_distance = f32::MAX;
    for (index, center) in centers.iter().enumerate() {
        let distance = (rgb[0] - center[0]).powi(2)
            + (rgb[1] - center[1]).powi(2)
            + (rgb[2] - center[2]).powi(2);
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }
    best
}

/// How far `rgb` is from the closest color of the background model, 0–1 over the RGB cube.
fn background_distance(centers: &[[f32; 3]], rgb: &[f32; 3]) -> f32 {
    let mut best = f32::MAX;
    for center in centers {
        let distance = (rgb[0] - center[0]).powi(2)
            + (rgb[1] - center[1]).powi(2)
            + (rgb[2] - center[2]).powi(2);
        best = best.min(distance);
    }
    (best / 3.0).sqrt()
}

/// The foreground bitmap's 4-connected components, the small ones dropped: the instance label map,
/// with the largest object as 1 and the rest in the order of their area.
fn label_instances(foreground: &[bool], width: usize, height: usize) -> Gray8Image {
    let count = width * height;
    let mut temp = vec![0u32; count];
    let mut components: Vec<(usize, usize, u32)> = Vec::new();
    let mut next_id = 0u32;
    for start in 0..count {
        if !foreground[start] || temp[start] != 0 {
            continue;
        }
        next_id += 1;
        let id = next_id;
        let mut area = 0usize;
        let mut stack = vec![start];
        temp[start] = id;
        while let Some(point) = stack.pop() {
            area += 1;
            let x = point % width;
            let y = point / width;
            if x > 0 && foreground[point - 1] && temp[point - 1] == 0 {
                temp[point - 1] = id;
                stack.push(point - 1);
            }
            if x + 1 < width && foreground[point + 1] && temp[point + 1] == 0 {
                temp[point + 1] = id;
                stack.push(point + 1);
            }
            if y > 0 && foreground[point - width] && temp[point - width] == 0 {
                temp[point - width] = id;
                stack.push(point - width);
            }
            if y + 1 < height && foreground[point + width] && temp[point + width] == 0 {
                temp[point + width] = id;
                stack.push(point + width);
            }
        }
        components.push((area, start, id));
    }
    let minimum_area =
        ((count as f32 * MIN_INSTANCE_SHARE).max(MIN_INSTANCE_PIXELS as f32)) as usize;
    let mut kept: Vec<(usize, usize, u32)> = components
        .into_iter()
        .filter(|component| component.0 >= minimum_area)
        .collect();
    kept.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    kept.truncate(MAX_INSTANCES);
    let mut relabel = vec![0u8; next_id as usize + 1];
    for (index, component) in kept.iter().enumerate() {
        relabel[component.2 as usize] = (index + 1) as u8;
    }
    let labels = temp
        .iter()
        .map(|&id| relabel[id as usize])
        .collect::<Vec<u8>>();
    Gray8Image::from_data(width, height, labels)
}

// MARK: - SubjectRemoval.swift

/// `SubjectRemoval.Failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectRemovalError {
    /// Nothing in the image separates from its background.
    NoSubject,
}

impl std::fmt::Display for SubjectRemovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "No foreground subject was detected in this layer. Try an image with a more distinct subject.",
        )
    }
}

impl std::error::Error for SubjectRemovalError {}

/// Vision's own mask for an image, kept while the panel is open so moving a slider only redoes the
/// refining. The Swift keyed the cache on the `CGImage`'s object identity; the port keys it on the
/// pixel buffer's address (plus its size), which is the same identity for as long as the caller keeps
/// the image alive.
static SUBJECT_MASK_CACHE: LazyLock<Mutex<SubjectMaskCache>> =
    LazyLock::new(|| Mutex::new(SubjectMaskCache::default()));

#[derive(Default)]
struct SubjectMaskCache {
    key: Option<(usize, usize, usize)>,
    value: Option<Gray8Image>,
}

/// Remove Background: the subject mask the panel's settings are applied to, and the layer with its
/// background made transparent.
pub struct SubjectRemoval;

impl SubjectRemoval {
    /// The segmentation's raw subject mask, white over the subject: the model has no settings of its
    /// own, so everything the panel offers is done to this afterwards by [`Self::refined`].
    fn vision(image: &Rgba8Image) -> Result<Gray8Image, SubjectRemovalError> {
        let key = (
            image.data().as_ptr() as usize,
            image.width(),
            image.height(),
        );
        {
            let cache = SUBJECT_MASK_CACHE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.key == Some(key) {
                if let Some(mask) = &cache.value {
                    return Ok(mask.clone());
                }
            }
        }
        let mask = ForegroundInstances::detect(image)
            .ok_or(SubjectRemovalError::NoSubject)?
            .scaled_mask(image.width(), image.height());
        let mut cache = SUBJECT_MASK_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.key = Some(key);
        cache.value = Some(mask.clone());
        Ok(mask)
    }

    /// The panel's three controls, in the order they help:
    /// - Refine pulls the mask onto the image's own edges (a guided filter with the layer as its guide), which is
    ///   what recovers hair and fur the model cuts straight through.
    /// - Contrast pushes the mask's grays apart, clearing the haze that leaves background showing through.
    /// - Shift Edge grows or shrinks the mask, usually inwards, to drop the rim of background color around a cutout.
    fn refined(
        mask: &Gray8Image,
        guide: &Rgba8Image,
        settings: &FilterSettings,
        limit: f64,
    ) -> Gray8Image {
        // Basic is the segmentation's mask as it comes, which is quick; everything below is Advanced.
        if settings.background_quality != BackgroundQuality::Advanced {
            return mask.clone();
        }
        let width = mask.width();
        let height = mask.height();
        let mut levels: Vec<f32> = mask
            .data()
            .iter()
            .map(|&byte| byte as f32 / 255.0)
            .collect();
        if settings.refine_edges > 0.0 {
            let refined = GuidedMatte::refine(mask, guide, settings.refine_edges, limit);
            levels = refined
                .data()
                .iter()
                .map(|&byte| byte as f32 / 255.0)
                .collect();
        }
        if settings.shift_edge != 0.0 {
            // A blur then a hard threshold at the matching level moves the edge by the blur's reach.
            let reach = settings.shift_edge.abs();
            let blurred = blur_levels(&levels, width, height, reach / 2.0);
            let level = if settings.shift_edge < 0.0 {
                0.75f32
            } else {
                0.25f32
            };
            // `CIColorClamp` to [level, level + 0.001] then the `CIColorMatrix` (×1000 − level × 1000):
            // a ramp 0…1 across that band, i.e. a hard threshold at `level`.
            levels = blurred
                .iter()
                .map(|&value| {
                    let clamped = value.clamp(level, level + 0.001);
                    ((clamped - level) * (1.0 / 0.001)).clamp(0.0, 1.0)
                })
                .collect();
        }
        if settings.matte_contrast > 0.0 {
            // 0 leaves the mask as it is; 100 is a hard cut at the middle.
            let strength = settings.matte_contrast / 100.0;
            let slope = 1.0 / (1.0 - strength * 0.98).max(0.02);
            let bias = (1.0 - slope) / 2.0;
            levels = levels
                .iter()
                .map(|&value| (value as f64 * slope + bias).clamp(0.0, 1.0) as f32)
                .collect();
        }
        GuidedMatte::gray_image(&levels, width, height)
    }

    /// Where the subject is: white over it, black over the background, the size of the layer's own
    /// pixels — a layer mask that hides the background instead of erasing it. `under` is the layer's
    /// existing mask, kept as well.
    pub fn subject_mask(
        image: &Rgba8Image,
        under: Option<&Gray8Image>,
        settings: &FilterSettings,
    ) -> Result<Gray8Image, SubjectRemovalError> {
        let subject = Self::refined(&Self::vision(image)?, image, settings, f64::MAX);
        let Some(existing) = under else {
            return Ok(subject);
        };
        if existing.width() != subject.width() || existing.height() != subject.height() {
            return Ok(subject);
        }
        // Both masks hide: what either one hides stays hidden. The Swift multiplied the two on a gray
        // `CGContext`; [`Canvas`] composites coverage source-over on gray targets, so the product is
        // computed directly (the same 8-bit multiply).
        let product = existing
            .data()
            .par_iter()
            .zip(subject.data().par_iter())
            .map(|(&existing, &subject)| ((existing as u32 * subject as u32 + 127) / 255) as u8)
            .collect();
        Ok(Gray8Image::from_data(
            subject.width(),
            subject.height(),
            product,
        ))
    }

    /// The preview: the layer with its background made transparent by the same mask the commit lays
    /// down.
    pub fn run(
        image: &Rgba8Image,
        settings: &FilterSettings,
    ) -> Result<Rgba8Image, SubjectRemovalError> {
        // The preview refines on a copy at most this big, so dragging a slider stays responsive.
        let mask = Self::refined(&Self::vision(image)?, image, settings, 1400.0);
        // `CIBlendWithMask` over a clear background: the layer's pixels scaled by the mask.
        let mut output = image.clone();
        output
            .data_mut()
            .par_chunks_exact_mut(4)
            .zip(mask.data().par_iter())
            .for_each(|(pixel, &mask)| {
                let coverage = mask as f64 / 255.0;
                for channel in pixel.iter_mut() {
                    *channel = (*channel as f64 * coverage).round().clamp(0.0, 255.0) as u8;
                }
            });
        Ok(output)
    }
}

/// `CIImage.applyingGaussianBlur(sigma:)` on clamped edges: a separable Gaussian whose standard
/// deviation is `sigma` (`CIGaussianBlur`'s radius *is* its sigma), truncated at three deviations.
/// The Swift clamped the image to its extent first and cropped back afterwards, so the edges read
/// clamped rather than fading out.
fn blur_levels(levels: &[f32], width: usize, height: usize, sigma: f64) -> Vec<f32> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    assert_eq!(levels.len(), width * height, "blur size mismatch");
    if !(sigma > 0.0) || !sigma.is_finite() {
        return levels.to_vec();
    }
    let radius = (sigma * 3.0).ceil() as usize;
    if width <= 1 && height <= 1 {
        return levels.to_vec();
    }
    let mut weights = vec![0f32; radius + 1];
    let mut total = 0f32;
    for (offset, weight) in weights.iter_mut().enumerate() {
        let value = (-((offset * offset) as f64) / (2.0 * sigma * sigma)).exp() as f32;
        *weight = value;
        // The symmetric kernel is counted once for the center and twice for the rest.
        total += if offset == 0 { value } else { value * 2.0 };
    }
    for weight in weights.iter_mut() {
        *weight /= total;
    }
    let pass = convolve_rows(levels, width, height, &weights, radius);
    let transposed = transpose(&pass, width, height);
    let vertical = convolve_rows(&transposed, height, width, &weights, radius);
    transpose(&vertical, height, width)
}

/// One clamped-edge Gaussian convolution pass along every row.
fn convolve_rows(
    source: &[f32],
    width: usize,
    height: usize,
    weights: &[f32],
    radius: usize,
) -> Vec<f32> {
    let last = width as isize - 1;
    let mut result = vec![0f32; width * height];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, out_row)| {
            let row = y * width;
            for x in 0..width {
                let mut sum = weights[0] * source[row + x];
                for offset in 1..=radius {
                    let left = (x as isize - offset as isize).clamp(0, last) as usize;
                    let right = (x as isize + offset as isize).clamp(0, last) as usize;
                    sum += weights[offset] * (source[row + left] + source[row + right]);
                }
                out_row[x] = sum;
            }
        });
    result
}

// MARK: - ObjectSelection.swift

/// The Object Selection tool's options-bar settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectSelectionSettings {
    /// Read the visible composite rather than just the active layer.
    pub sample_all_layers: bool,
    /// Positive values erode the detected mask inward; negative values expand it outward.
    pub edge_offset: i32,
}

impl Default for ObjectSelectionSettings {
    fn default() -> Self {
        ObjectSelectionSettings {
            sample_all_layers: true,
            edge_offset: 0,
        }
    }
}

/// `ObjectSelection.Failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectSelectionError {
    /// Vision's instance masks need macOS 14. The CPU port always has one, so this is unreachable —
    /// it is kept because the Swift error carries its own message.
    Unsupported,
    /// The mask could not be rendered. Unreachable here: the port's buffers are infallible.
    Render,
}

impl std::fmt::Display for ObjectSelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectSelectionError::Unsupported => {
                formatter.write_str("Object Selection requires macOS 14 or later.")
            }
            ObjectSelectionError::Render => {
                formatter.write_str("The object mask could not be rendered.")
            }
        }
    }
}

impl std::error::Error for ObjectSelectionError {}

/// Selects the foreground object under a clicked point, then traces that mask into the document's
/// normal path-based selection. The instance mask comes from the port's CPU segmentation (Vision has
/// no cross-platform equivalent); the edge-preserving upsample, the edge offset, the tracing and the
/// smoothing that follow it are the Swift algorithm.
pub struct ObjectSelection;

impl ObjectSelection {
    /// `CIEdgePreserveUpsampleFilter`'s `inputSpatialSigma`.
    const EDGE_PRESERVE_RADIUS: usize = 5;

    /// `CIEdgePreserveUpsampleFilter`'s `inputLumaSigma`, as the variance the guided filter divides by.
    const EDGE_PRESERVE_EPSILON: f32 = 0.15 * 0.15;

    /// The outline, in the image's top-left pixel coordinates, of the foreground object at `point`.
    /// `None` when the point is outside the image, on background, or no object is found.
    pub fn select(
        image: &Rgba8Image,
        at: Point,
        edge_offset: i32,
        smooth_edges: bool,
    ) -> Result<Option<Path>, ObjectSelectionError> {
        let width = image.width();
        let height = image.height();
        let x = at.x.floor() as i64;
        let y = at.y.floor() as i64;
        if !at.x.is_finite()
            || !at.y.is_finite()
            || x < 0
            || y < 0
            || x >= width as i64
            || y >= height as i64
        {
            return Ok(None);
        }
        // `#available(macOS 14.0, *)` has no port: the CPU segmentation is always available.
        let Some(instances) = ForegroundInstances::detect(image) else {
            return Ok(None);
        };
        let image_size = Size::new(width as f64, height as f64);
        let Some(instance) = instances.instance_index(at, image_size) else {
            return Ok(None);
        };
        let coarse = instances.mask(instance);
        let binary = Self::edge_preserved_binary_mask(&coarse, image, width, height);
        let mask = Self::adjusted(&binary, width, height, edge_offset);
        let Some(outline) =
            MagicWand::outline(&mask, width, height).map_err(|error| match error {
                MagicWandError::TooDetailed | MagicWandError::Memory => {
                    ObjectSelectionError::Render
                }
            })?
        else {
            return Ok(None);
        };
        Ok(Some(if smooth_edges {
            Self::smoothed(&outline)
        } else {
            outline
        }))
    }

    /// The low-resolution mask pulled onto the guide's edges and upsampled to the image's size, then
    /// binarized at 50% — `CIEdgePreserveUpsampleFilter` followed by the `>= 128` threshold. The port
    /// runs [`GuidedMatte`] with the filter's sigma settings instead of the Core Image filter.
    fn edge_preserved_binary_mask(
        coarse: &Gray8Image,
        guide: &Rgba8Image,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        let mask = GuidedMatte::levels_gray(coarse, width, height);
        let guide = GuidedMatte::levels_rgba(guide, width, height);
        let refined = GuidedMatte::filter(
            &mask,
            &guide,
            width,
            height,
            Self::EDGE_PRESERVE_RADIUS,
            Self::EDGE_PRESERVE_EPSILON,
        );
        refined
            .iter()
            .map(|&value| if value >= 0.5 { 255 } else { 0 })
            .collect()
    }

    /// Erosion or dilation by `edge_offset` 3 × 3 steps, at most ten of them.
    fn adjusted(mask: &[u8], width: usize, height: usize, edge_offset: i32) -> Vec<u8> {
        let steps = edge_offset.unsigned_abs().min(10) as usize;
        if steps == 0 || width == 0 || height == 0 {
            return mask.to_vec();
        }
        let mut mask = mask.to_vec();
        for _ in 0..steps {
            mask = if edge_offset > 0 {
                Self::eroded(&mask, width, height)
            } else {
                Self::dilated(&mask, width, height)
            };
        }
        mask
    }

    /// A 3 × 3 minimum: a set pixel with any clear neighbor is cleared.
    fn eroded(mask: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut result = mask.to_vec();
        for y in 0..height {
            for x in 0..width {
                if mask[y * width + x] == 0 {
                    continue;
                }
                let mut keep = true;
                for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                    for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                        if mask[ny * width + nx] == 0 {
                            keep = false;
                        }
                    }
                }
                result[y * width + x] = if keep { 255 } else { 0 };
            }
        }
        result
    }

    /// A 3 × 3 maximum: a clear pixel with any set neighbor is filled in.
    fn dilated(mask: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut result = mask.to_vec();
        for y in 0..height {
            for x in 0..width {
                if mask[y * width + x] != 0 {
                    continue;
                }
                let mut fill = false;
                for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                    for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                        if mask[ny * width + nx] != 0 {
                            fill = true;
                        }
                    }
                }
                if fill {
                    result[y * width + x] = 255;
                }
            }
        }
        result
    }

    /// Rounds off the one-pixel stair steps created by tracing a binary mask. The winding and
    /// subpath order are preserved, so holes continue to subtract from the selected region.
    fn smoothed(path: &Path) -> Path {
        let mut subpaths: Vec<Vec<Point>> = Vec::new();
        let mut current: Vec<Point> = Vec::new();
        let mut finish_current = |current: &mut Vec<Point>| {
            if current.len() >= 3 {
                subpaths.push(std::mem::take(current));
            } else {
                current.clear();
            }
        };
        for element in path.elements() {
            match element {
                PathElement::MoveTo(point) => {
                    finish_current(&mut current);
                    current = vec![*point];
                }
                PathElement::LineTo(point) => current.push(*point),
                PathElement::QuadCurveTo { to, .. } => current.push(*to),
                PathElement::CurveTo { to, .. } => current.push(*to),
                PathElement::CloseSubpath => finish_current(&mut current),
            }
        }
        finish_current(&mut current);

        let mut result = Path::empty();
        for subpath in subpaths {
            let simplified = Self::simplify_closed(&subpath, 1.6);
            let points = Self::chaikin(&simplified, 3);
            let Some(first) = points.first() else {
                continue;
            };
            result.move_to(*first);
            result.add_lines(&points[1..]);
            result.close_subpath();
        }
        result
    }

    fn simplify_closed(input: &[Point], tolerance: f64) -> Vec<Point> {
        let mut points = input.to_vec();
        if points.first() == points.last() {
            points.pop();
        }
        if points.len() < 4 {
            return points;
        }
        // Break at a stable extreme so the open-polyline simplifier can preserve the whole closed
        // contour.
        let start = (0..points.len())
            .min_by(|&lhs, &rhs| {
                if points[lhs].x == points[rhs].x {
                    points[lhs]
                        .y
                        .partial_cmp(&points[rhs].y)
                        .unwrap_or(Ordering::Equal)
                } else {
                    points[lhs]
                        .x
                        .partial_cmp(&points[rhs].x)
                        .unwrap_or(Ordering::Equal)
                }
            })
            .unwrap_or(0);
        let mut rotated = points[start..].to_vec();
        rotated.extend_from_slice(&points[..start]);
        let mut open = rotated.clone();
        open.push(rotated[0]);
        let mut open = Self::simplify_open(&open, 0, open.len() - 1, tolerance);
        if open.first() == open.last() {
            open.pop();
        }
        if open.len() >= 3 {
            open
        } else {
            points
        }
    }

    fn simplify_open(points: &[Point], first: usize, last: usize, tolerance: f64) -> Vec<Point> {
        if last <= first + 1 {
            return vec![points[first], points[last]];
        }
        let mut farthest = first + 1;
        let mut greatest_distance = 0.0f64;
        for index in (first + 1)..last {
            let distance = Self::perpendicular_distance(points[index], points[first], points[last]);
            if distance > greatest_distance {
                greatest_distance = distance;
                farthest = index;
            }
        }
        if greatest_distance <= tolerance {
            return vec![points[first], points[last]];
        }
        let mut left = Self::simplify_open(points, first, farthest, tolerance);
        let right = Self::simplify_open(points, farthest, last, tolerance);
        left.pop();
        left.extend_from_slice(&right);
        left
    }

    fn perpendicular_distance(point: Point, from: Point, to: Point) -> f64 {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let length = (dx * dx + dy * dy).sqrt();
        if length <= 0.0 {
            return ((point.x - from.x).powi(2) + (point.y - from.y).powi(2)).sqrt();
        }
        (dy * point.x - dx * point.y + to.x * from.y - to.y * from.x).abs() / length
    }

    fn chaikin(input: &[Point], iterations: usize) -> Vec<Point> {
        let mut points = input.to_vec();
        if points.first() == points.last() {
            points.pop();
        }
        if points.len() < 3 {
            return points;
        }
        for _ in 0..iterations {
            let mut next: Vec<Point> = Vec::with_capacity(points.len() * 2);
            for index in 0..points.len() {
                let a = points[index];
                let b = points[(index + 1) % points.len()];
                next.push(Point::new(a.x * 0.75 + b.x * 0.25, a.y * 0.75 + b.y * 0.25));
                next.push(Point::new(a.x * 0.25 + b.x * 0.75, a.y * 0.25 + b.y * 0.75));
            }
            points = next;
        }
        points
    }
}

// MARK: - ContentFill.swift

/// `ContentFill.Failure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentFillError {
    /// There is no selection to fill, or too few unselected opaque pixels to synthesize one from.
    NoSource,
}

impl std::fmt::Display for ContentFillError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "Not enough unselected, opaque image pixels to synthesize a fill. Use a smaller selection with some surrounding image.",
        )
    }
}

impl std::error::Error for ContentFillError {}

/// The content-aware fill: the `content_fill` C kernel behind the selection's mask.
pub struct ContentFill;

impl ContentFill {
    /// `ContentFill.run(_:)`: the job's pixels, its selection painted into a mask through the job's
    /// mapping, then the kernel. `Err(ContentFillError::NoSource)` when there is no selection or no
    /// unselected, opaque pixel to fill from.
    pub fn run(job: &FilterJob) -> Result<Rgba8Image, ContentFillError> {
        Self::fill(&job.image, job.selection.as_ref(), job.mapping)
    }

    /// The body of [`Self::run`], over the three things it reads from the job.
    fn fill(
        image: &Rgba8Image,
        selection: Option<&SelectionClip>,
        mapping: AffineTransform,
    ) -> Result<Rgba8Image, ContentFillError> {
        let Some(selection) = selection else {
            return Err(ContentFillError::NoSource);
        };
        let width = image.width();
        let height = image.height();
        let rect = Rect::new(0.0, 0.0, width as f64, height as f64);
        let mut pixels = Canvas::new_rgba(width, height);
        pixels.draw_image(image, rect);
        let mut mask = Canvas::new_gray(width, height);
        mask.concatenate(mapping.inverted());
        apply_selection_clip(&mut mask, selection);
        mask.set_fill_gray(1.0);
        mask.fill_rect(mapped_rect(rect, mapping));
        let mask = mask.into_gray();
        let mut pixels = pixels.into_rgba();
        let result = crate::content_fill::content_fill(
            pixels.data_mut(),
            pixels.stride(),
            mask.data(),
            mask.stride(),
            width as i32,
            height as i32,
        );
        if result == 0 {
            return Err(ContentFillError::NoSource);
        }
        // `result == -1` (allocation failure) cannot happen in Rust, which aborts instead.
        Ok(pixels)
    }
}

/// `SelectionClip.apply(to:)`: clips to the coverage's region, or clips everything away when the
/// selection is empty.
fn apply_selection_clip(canvas: &mut Canvas, clip: &SelectionClip) {
    match &clip.coverage {
        Some(coverage) if !clip.rect.is_empty() => canvas.clip_to_image(coverage, clip.rect),
        _ => canvas.clip_to_zero(),
    }
}

/// `CGRect.applying(_:)`: the bounding box of the transformed rectangle.
fn mapped_rect(rect: Rect, transform: AffineTransform) -> Rect {
    let corners = rect.corners().map(|corner| transform.applying(corner));
    let min_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f64::INFINITY, f64::min);
    let min_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f64::INFINITY, f64::min);
    let max_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f64::NEG_INFINITY, f64::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compositor_core::image_ops::FilterKind;
    use compositor_core::path::{flatten, FillRule};
    use std::collections::BTreeSet;

    /// The pixels a traced outline covers, by winding number at each pixel's center — how the Swift
    /// tests rasterized the path's coverage.
    fn covered_pixels(path: &Path, width: usize, height: usize) -> BTreeSet<usize> {
        let polygons = flatten(path, &AffineTransform::IDENTITY);
        let mut covered = BTreeSet::new();
        for y in 0..height {
            for x in 0..width {
                let px = x as f64 + 0.5;
                let py = y as f64 + 0.5;
                let mut winding = 0i32;
                for polygon in &polygons {
                    let points = &polygon.points;
                    if points.len() < 2 {
                        continue;
                    }
                    for index in 0..points.len() {
                        let a = points[index];
                        let b = points[(index + 1) % points.len()];
                        let cross = (b.x - a.x) * (py - a.y) - (px - a.x) * (b.y - a.y);
                        if a.y <= py {
                            if b.y > py && cross > 0.0 {
                                winding += 1;
                            }
                        } else if b.y <= py && cross < 0.0 {
                            winding -= 1;
                        }
                    }
                }
                if winding != 0 {
                    covered.insert(y * width + x);
                }
            }
        }
        covered
    }

    fn blank_rgba(width: usize, height: usize) -> Rgba8Image {
        Rgba8Image::new(width, height)
    }

    fn rgba_image(
        width: usize,
        height: usize,
        color: impl Fn(usize, usize) -> [u8; 4],
    ) -> Rgba8Image {
        let mut image = Rgba8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, color(x, y));
            }
        }
        image
    }

    fn gray_image(width: usize, height: usize, value: impl Fn(usize, usize) -> u8) -> Gray8Image {
        let mut image = Gray8Image::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, value(x, y));
            }
        }
        image
    }

    fn mask_of(width: usize, height: usize, values: &[(usize, usize)]) -> Vec<u8> {
        let mut mask = vec![0u8; width * height];
        for &(x, y) in values {
            mask[y * width + x] = 255;
        }
        mask
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    fn wand_settings(tolerance: i32, size: WandSampleSize, contiguous: bool) -> WandSettings {
        WandSettings {
            tolerance,
            sample_size: size,
            contiguous,
            sample_all_layers: false,
        }
    }

    #[test]
    fn wand_tolerance_applies_to_every_channel_including_alpha() {
        let columns: [[u8; 4]; 4] = [
            [100, 100, 100, 255],
            [132, 100, 100, 255],
            [133, 100, 100, 255],
            [100, 100, 100, 222],
        ];
        let row = rgba_image(4, 1, |x, _| columns[x]);
        let run = |tolerance: i32| {
            let path = MagicWand::select(
                &row,
                Point::new(0.5, 0.5),
                &wand_settings(tolerance, WandSampleSize::Point, false),
            )
            .unwrap()
            .unwrap();
            covered_pixels(&path, 4, 1)
        };
        // The reference is the sampled pixel: white 255 for the point sample.
        assert_eq!(run(0), BTreeSet::from([0]));
        assert_eq!(run(32), BTreeSet::from([0, 1]));
        assert_eq!(run(33), BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn wand_contiguous_stops_at_other_colors_while_non_contiguous_finds_every_match() {
        let stripes = rgba_image(10, 4, |x, _| if x < 3 || x >= 6 { RED } else { BLUE });
        let left: BTreeSet<usize> = (0..4)
            .flat_map(|y| (0..3).map(move |x| y * 10 + x))
            .collect();
        let right: BTreeSet<usize> = (0..4)
            .flat_map(|y| (6..10).map(move |x| y * 10 + x))
            .collect();
        let at = Point::new(1.5, 2.5);
        let connected = MagicWand::select(
            &stripes,
            at,
            &wand_settings(32, WandSampleSize::Point, true),
        )
        .unwrap()
        .unwrap();
        assert_eq!(covered_pixels(&connected, 10, 4), left);
        let everywhere = MagicWand::select(
            &stripes,
            at,
            &wand_settings(32, WandSampleSize::Point, false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            covered_pixels(&everywhere, 10, 4),
            left.union(&right).copied().collect()
        );
        // Rows stay the right way up: clicking the top row selects the top row.
        let banded = rgba_image(4, 3, |_, y| if y == 0 { RED } else { BLUE });
        let top = MagicWand::select(
            &banded,
            Point::new(1.0, 0.0),
            &wand_settings(32, WandSampleSize::Point, true),
        )
        .unwrap()
        .unwrap();
        assert_eq!(covered_pixels(&top, 4, 3), BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn wand_select_outside_the_image_or_on_an_empty_layer_is_none() {
        let image = rgba_image(4, 3, |_, _| RED);
        let settings = wand_settings(32, WandSampleSize::Point, true);
        assert_eq!(
            MagicWand::select(&image, Point::new(9.0, 0.0), &settings).unwrap(),
            None
        );
        assert_eq!(
            MagicWand::select(&image, Point::new(-0.5, 0.0), &settings).unwrap(),
            None
        );
        assert_eq!(
            MagicWand::select(&image, Point::new(f64::NAN, 0.0), &settings).unwrap(),
            None
        );
        // A fully transparent layer matches everywhere: the sample is transparent too.
        let empty = blank_rgba(4, 3);
        let path = MagicWand::select(&empty, Point::new(1.0, 1.0), &settings)
            .unwrap()
            .unwrap();
        assert_eq!(covered_pixels(&path, 4, 3).len(), 12);
    }

    #[test]
    fn wand_sample_size_averages_the_pixels_around_the_click() {
        let dot = rgba_image(5, 5, |x, y| {
            if x == 2 && y == 2 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            }
        });
        let center = Point::new(2.5, 2.5);
        let point = MagicWand::select(
            &dot,
            center,
            &wand_settings(10, WandSampleSize::Point, false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(covered_pixels(&point, 5, 5), BTreeSet::from([12]));
        // A 3 × 3 average is gray 28: black is within 30 of it, the white center is not.
        let averaged = MagicWand::select(
            &dot,
            center,
            &wand_settings(30, WandSampleSize::ThreeByThree, false),
        )
        .unwrap()
        .unwrap();
        let expected: BTreeSet<usize> = (0..25).filter(|&index| index != 12).collect();
        assert_eq!(covered_pixels(&averaged, 5, 5), expected);
        assert_eq!(WandSampleSize::ThreeByThree.radius(), 1);
        assert_eq!(WandSampleSize::FiveByFive.radius(), 2);
        assert_eq!(WandSampleSize::FiveByFive.title(), "5 by 5 Average");
        assert_eq!(
            WandSampleSize::from_raw(1),
            Some(WandSampleSize::ThreeByThree)
        );
        assert_eq!(WandSettings::default().tolerance, 32);
        assert!(WandSettings::default().contiguous);
    }

    #[test]
    fn wand_outline_reproduces_its_pixels_with_holes_and_corner_touches() {
        let (width, height) = (8usize, 6usize);
        let mut mask = vec![0u8; width * height];
        // A 3 × 3 ring around a hole, against the image's corner, and two pixels touching only
        // diagonally.
        for y in 0..3 {
            for x in 0..3 {
                if !(x == 1 && y == 1) {
                    mask[y * width + x] = 255;
                }
            }
        }
        mask[4 * width + 5] = 255;
        mask[5 * width + 6] = 255;
        let expected: BTreeSet<usize> = mask
            .iter()
            .enumerate()
            .filter(|(_, &value)| value != 0)
            .map(|(index, _)| index)
            .collect();
        let path = MagicWand::outline(&mask, width, height).unwrap().unwrap();
        assert_eq!(covered_pixels(&path, width, height), expected);
        assert_eq!(
            MagicWand::outline(&vec![0u8; 4], 2, 2).unwrap(),
            None,
            "an empty mask has no outline"
        );
        assert_eq!(MagicWand::outline(&[], 0, 0).unwrap(), None);
        assert_eq!(
            MagicWand::outline(&[255, 0, 0, 0], 2, 2).unwrap(),
            None,
            "a mask whose size does not match its pixel count has no outline"
        );
    }

    #[test]
    fn mask_tracing_splits_white_and_dark_at_fifty_percent() {
        let mask = gray_image(3, 3, |x, y| {
            if x == 1 && y == 1 {
                0
            } else if x == 2 && y == 0 {
                128
            } else if x == 0 && y == 2 {
                127
            } else {
                255
            }
        });
        // Gray 127 is a dark pixel, gray 128 a white one — the 50% split the Swift tests use.
        let white = MaskTracing::white_pixels(&mask).unwrap();
        let white_pixels: BTreeSet<usize> = BTreeSet::from([1, 2, 3, 5, 6, 7, 8]);
        assert_eq!(covered_pixels(&white, 3, 3), white_pixels);
        let dark = MaskTracing::dark_pixels(&mask).unwrap();
        assert_eq!(covered_pixels(&dark, 3, 3), BTreeSet::from([0, 4]));
        assert_eq!(MaskTracing::white_pixels(&Gray8Image::new(2, 2)), None);
        assert_eq!(
            MaskTracing::dark_pixels(&Gray8Image::uniform(2, 2, 255)),
            None
        );
    }

    #[test]
    fn mask_tracing_opaque_pixels_uses_the_alpha_channel() {
        let image = rgba_image(2, 2, |x, y| match (x, y) {
            (0, 0) => [10, 20, 30, 255],
            (1, 0) => [10, 20, 30, 0],
            (0, 1) => [10, 20, 30, 128],
            _ => [10, 20, 30, 127],
        });
        assert_eq!(
            MaskTracing::opaque_pixels(&image)
                .map(|path| covered_pixels(&path, 2, 2))
                .unwrap(),
            BTreeSet::from([0, 2])
        );
        assert_eq!(
            MaskTracing::opaque_pixels(&blank_rgba(4, 4)),
            None,
            "a transparent image has no opaque pixels"
        );
    }

    #[test]
    fn color_range_selects_within_fuzziness_and_inverts() {
        let image = rgba_image(4, 1, |x, _| {
            if x < 2 {
                [100, 100, 100, 255]
            } else {
                [140, 100, 100, 255]
            }
        });
        let include = [100u8, 100, 100];
        assert_eq!(
            color_range_mask(&image, &include, &[], 40, false),
            [255, 255, 255, 0]
        );
        assert_eq!(
            color_range_mask(&image, &include, &[], 39, false),
            [255, 255, 0, 0]
        );
        assert_eq!(
            color_range_mask(&image, &include, &[], 40, true),
            [0, 0, 0, 255]
        );
        let path = color_range_path(&image, &include, &[], 40, false)
            .unwrap()
            .unwrap();
        assert_eq!(covered_pixels(&path, 4, 1), BTreeSet::from([0, 1, 2]));
        assert_eq!(
            color_range_path(&image, &[1, 2, 3], &[], 0, false).unwrap(),
            None
        );
    }

    #[test]
    fn guided_matte_box_mean_averages_with_clamped_edges() {
        let source = [0f32, 0.0, 0.0, 1.0];
        assert_eq!(
            GuidedMatte::box_mean(&source, 4, 1, 0),
            vec![0.0, 0.0, 0.0, 1.0],
            "a zero radius is the source itself"
        );
        // Radius 1 at x = 0 clamps the left neighbor to the pixel itself: (0 + 0 + 0) / 3.
        let boxed = GuidedMatte::box_mean(&source, 4, 1, 1);
        assert_eq!(boxed[0], 0.0);
        assert_eq!(boxed[3], (1.0 + 1.0 + 1.0) / 3.0);
        let constant = vec![0.25f32; 12];
        assert_eq!(GuidedMatte::box_mean(&constant, 4, 3, 2), constant);
        assert_eq!(GuidedMatte::box_mean(&[], 0, 0, 1), Vec::<f32>::new());
    }

    #[test]
    fn guided_matte_filter_with_a_flat_guide_returns_the_mask() {
        let width = 4;
        let height = 3;
        let mask: Vec<f32> = (0..width * height).map(|i| i as f32 / 11.0).collect();
        let guide = vec![0.5f32; width * height];
        let filtered = GuidedMatte::filter(&mask, &guide, width, height, 1, 1e-4);
        for (value, &source) in filtered.iter().zip(mask.iter()) {
            assert!(
                (value - source).abs() < 1e-6,
                "a flat guide has no variance to lean on: {value} vs {source}"
            );
        }
    }

    #[test]
    fn guided_matte_gray_image_rounds_half_up_and_clamps() {
        let image = GuidedMatte::gray_image(&[0.5, 0.0, 1.0, 2.0, -0.1, 127.0 / 255.0], 3, 2);
        assert_eq!(image.data(), [128, 0, 255, 255, 0, 127]);
    }

    #[test]
    fn guided_matte_refine_keeps_the_mask_size_and_follows_the_guide() {
        let size = 16usize;
        let mask = gray_image(size, size, |x, _| if x < size / 2 { 255 } else { 0 });
        // The guide's step sits exactly where the mask's does.
        let guide = rgba_image(size, size, |x, _| {
            if x < size / 2 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            }
        });
        let refined = GuidedMatte::refine(&mask, &guide, 2.0, f64::MAX);
        assert_eq!(refined.width(), size);
        assert_eq!(refined.height(), size);
        assert!(refined.get(4, 4) > 250, "the mask side stays white");
        assert!(refined.get(12, 12) < 5, "the background side stays black");
    }

    #[test]
    fn subject_removal_finds_a_distinct_block_and_combines_masks() {
        let size = 32usize;
        let image = rgba_image(size, size, |x, y| {
            if (8..24).contains(&x) && (8..24).contains(&y) {
                [255, 255, 255, 255]
            } else {
                [100, 100, 100, 255]
            }
        });
        // Basic is the raw segmentation mask, which at this size has no soft edge.
        let basic = FilterSettings {
            background_quality: BackgroundQuality::Basic,
            ..Default::default()
        };
        let mask = SubjectRemoval::subject_mask(&image, None, &basic).unwrap();
        assert_eq!(mask.get(16, 16), 255);
        assert_eq!(mask.get(0, 0), 0);
        assert_eq!(mask.get(20, 4), 0);

        // Advanced refines and stretches the mask; the block is still the subject.
        let advanced = FilterSettings {
            background_quality: BackgroundQuality::Advanced,
            refine_edges: 4.0,
            matte_contrast: 25.0,
            shift_edge: 0.0,
            ..Default::default()
        };
        let refined = SubjectRemoval::subject_mask(&image, None, &advanced).unwrap();
        assert!(refined.get(16, 16) >= 250);
        assert!(refined.get(1, 1) <= 5);

        // An existing mask is kept: what either one hides stays hidden.
        let existing = Gray8Image::uniform(size, size, 64);
        let combined = SubjectRemoval::subject_mask(&image, Some(&existing), &basic).unwrap();
        assert!(combined.data().iter().all(|&value| value <= 64));
        assert_eq!(combined.get(16, 16), 64);
        assert_eq!(combined.get(0, 0), 0);

        // The preview scales the layer by the same mask.
        let preview = SubjectRemoval::run(&image, &basic).unwrap();
        assert_eq!(preview.get(16, 16), [255, 255, 255, 255]);
        assert_eq!(preview.get(1, 1), [0, 0, 0, 0]);

        // A uniform image has nothing that separates from its background.
        let flat = rgba_image(size, size, |_, _| [100, 100, 100, 255]);
        assert_eq!(
            SubjectRemoval::subject_mask(&flat, None, &basic),
            Err(SubjectRemovalError::NoSubject)
        );
    }

    #[test]
    fn object_selection_selects_the_object_under_the_click() {
        let size = 32usize;
        let image = rgba_image(size, size, |x, y| {
            if (8..24).contains(&x) && (8..24).contains(&y) {
                [255, 255, 255, 255]
            } else {
                [100, 100, 100, 255]
            }
        });
        let path = ObjectSelection::select(&image, Point::new(16.0, 16.0), 0, false)
            .unwrap()
            .unwrap();
        let covered = covered_pixels(&path, size, size);
        assert!(covered.contains(&(16 * size + 16)));
        assert!(
            !covered.contains(&0),
            "the background corner is not selected"
        );
        assert!(!covered.contains(&(12 * size + 30)));

        let eroded = ObjectSelection::select(&image, Point::new(16.0, 16.0), 3, false)
            .unwrap()
            .unwrap();
        let eroded_pixels = covered_pixels(&eroded, size, size);
        assert!(eroded_pixels.contains(&(16 * size + 16)));
        assert!(
            eroded_pixels.is_subset(&covered) && eroded_pixels.len() < covered.len(),
            "a positive edge offset erodes the mask"
        );

        let smoothed = ObjectSelection::select(&image, Point::new(16.0, 16.0), 0, true)
            .unwrap()
            .unwrap();
        assert!(covered_pixels(&smoothed, size, size).contains(&(16 * size + 16)));

        // A click on the background finds nothing.
        assert_eq!(
            ObjectSelection::select(&image, Point::new(1.5, 1.5), 0, false).unwrap(),
            None
        );
        assert_eq!(
            ObjectSelection::select(&image, Point::new(40.0, 40.0), 0, false).unwrap(),
            None
        );
        assert_eq!(ObjectSelectionSettings::default().sample_all_layers, true);
        assert_eq!(ObjectSelectionSettings::default().edge_offset, 0);
    }

    #[test]
    fn content_fill_wraps_the_kernel_and_its_failure_cases() {
        let width = 13usize;
        let height = 13usize;
        let original = rgba_image(width, height, |_, _| [90, 140, 200, 255]);

        // No selection at all cannot be filled.
        assert_eq!(
            ContentFill::fill(&original, None, AffineTransform::IDENTITY),
            Err(ContentFillError::NoSource)
        );

        // An empty selection clips everything away, so the kernel has nothing to fill.
        let empty = SelectionClip::new(Rect::ZERO, None);
        let untouched =
            ContentFill::fill(&original, Some(&empty), AffineTransform::IDENTITY).unwrap();
        assert_eq!(untouched, original);

        // A selection over the whole image leaves no donor pixels.
        let coverage = Gray8Image::uniform(width, height, 255);
        let everything = SelectionClip::new(
            Rect::new(0.0, 0.0, width as f64, height as f64),
            Some(coverage),
        );
        assert_eq!(
            ContentFill::fill(&original, Some(&everything), AffineTransform::IDENTITY),
            Err(ContentFillError::NoSource)
        );

        // A single selected pixel of a uniform image is filled with the same color.
        let mut coverage = Gray8Image::new(width, height);
        coverage.set(6, 6, 255);
        let hole = SelectionClip::new(
            Rect::new(0.0, 0.0, width as f64, height as f64),
            Some(coverage),
        );
        let filled = ContentFill::fill(&original, Some(&hole), AffineTransform::IDENTITY).unwrap();
        assert_eq!(filled, original);

        // `run(_:)` reads exactly those three things from the job.
        let job = FilterJob::new(
            FilterKind::ContentAwareFill,
            original.clone(),
            FilterSettings::default(),
            1.0,
            None,
            AffineTransform::IDENTITY,
        );
        assert_eq!(ContentFill::run(&job), Err(ContentFillError::NoSource));
        let job = FilterJob::new(
            FilterKind::ContentAwareFill,
            original.clone(),
            FilterSettings::default(),
            1.0,
            Some(hole),
            AffineTransform::IDENTITY,
        );
        assert_eq!(ContentFill::run(&job).unwrap(), original);
    }
}
