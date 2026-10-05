//! Port of `references/Compositor/Compositor/Document/ImageTrim.swift`: Image > Trim… — the scan that
//! finds the box the image can be cropped to, the crop itself, and the canvas resize the caller applies
//! the result with.
//!
//! The Swift drew the `CGImage` into a `BrushRaster` bitmap and scanned its bytes; the canonical
//! [`Rgba8Image`] *is* that bitmap (premultiplied sRGB RGBA8, rows top-down, no padding), so the scan
//! runs over it directly with the same `stride` arithmetic.

use compositor_core::buffer::Rgba8Image;
use compositor_core::canvas_size::CanvasSizeOptions;
use compositor_core::geom::{Point, Rect};

use crate::brush_pixels::brush_alpha_bounds;

/// What the scan treats as background (`TrimBasedOn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrimBasedOn {
    /// Everything fully transparent.
    TransparentPixels,
    /// Everything matching the top-left pixel, within the tolerance.
    TopLeftPixelColor,
    /// Everything matching the bottom-right pixel, within the tolerance.
    BottomRightPixelColor,
}

impl TrimBasedOn {
    /// `CaseIterable` order, which is also the radio group's order.
    pub const ALL: [TrimBasedOn; 3] = [
        TrimBasedOn::TransparentPixels,
        TrimBasedOn::TopLeftPixelColor,
        TrimBasedOn::BottomRightPixelColor,
    ];

    /// The picker's label, and the raw value the manifest stores.
    pub fn raw_value(self) -> &'static str {
        match self {
            TrimBasedOn::TransparentPixels => "Transparent Pixels",
            TrimBasedOn::TopLeftPixelColor => "Top Left Pixel Color",
            TrimBasedOn::BottomRightPixelColor => "Bottom Right Pixel Color",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.raw_value() == value)
    }
}

/// Which edges may be trimmed and how close a color has to be (`TrimOptions`). The defaults are the
/// Swift initializer's: transparent pixels, all four edges, no tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrimOptions {
    pub based_on: TrimBasedOn,
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
    /// How far each channel of a pixel may differ from the sampled color and still count as background.
    pub tolerance: u8,
}

impl Default for TrimOptions {
    fn default() -> Self {
        Self {
            based_on: TrimBasedOn::TransparentPixels,
            top: true,
            bottom: true,
            left: true,
            right: true,
            tolerance: 0,
        }
    }
}

impl TrimOptions {
    pub fn new(
        based_on: TrimBasedOn,
        top: bool,
        bottom: bool,
        left: bool,
        right: bool,
        tolerance: u8,
    ) -> Self {
        Self {
            based_on,
            top,
            bottom,
            left,
            right,
            tolerance,
        }
    }

    /// Whether any edge is set to be trimmed; without one, nothing can be trimmed.
    pub fn trims_any(&self) -> bool {
        self.top || self.bottom || self.left || self.right
    }
}

/// `TrimError`. `calculate_trim_rect` reports the first case as `None` rather than an error, exactly as
/// the Swift's `nil` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimError {
    NoContentToTrim,
    InvalidDimensions,
}

impl TrimError {
    pub fn error_description(self) -> &'static str {
        match self {
            TrimError::NoContentToTrim => "No content remained after trimming.",
            TrimError::InvalidDimensions => "The trimmed image dimensions are invalid.",
        }
    }
}

impl std::fmt::Display for TrimError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.error_description())
    }
}

impl std::error::Error for TrimError {}

/// `ImageTrim.calculateColorTrimRect`: scans every row for the first and last pixel that is not the
/// sampled color, and keeps the union of those runs. A row that is nothing but the sampled color is
/// skipped, so it contributes no bound.
fn color_trim_rect(
    image: &Rgba8Image,
    sample: (usize, usize),
    options: &TrimOptions,
) -> Option<Rect> {
    let width = image.width();
    let height = image.height();
    let target = image.get(sample.0, sample.1);
    let tolerance = options.tolerance as i32;

    let matches = |x: usize, y: usize| -> bool {
        let pixel = image.get(x, y);
        (0..4).all(|channel| {
            (pixel[channel] as i32 - target[channel] as i32).abs() <= tolerance
        })
    };

    let mut left = width;
    let mut right = 0usize;
    let mut top = height;
    let mut bottom = 0usize;
    for y in 0..height {
        let mut first = 0usize;
        while first < width && matches(first, y) {
            first += 1;
        }
        if first == width {
            continue;
        }
        let mut last = width;
        while last > first && matches(last - 1, y) {
            last -= 1;
        }
        if first < left {
            left = first;
        }
        if last > right {
            right = last;
        }
        if y < top {
            top = y;
        }
        bottom = y + 1;
    }

    // If right == 0, every pixel matched the sample color.
    if right == 0 {
        return None;
    }
    trim_bounds(left, top, right, bottom, width, height, options)
}

/// Applies the "Trim Away" edges to the scanned half-open bounds and turns them into a rectangle.
fn trim_bounds(
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    width: usize,
    height: usize,
    options: &TrimOptions,
) -> Option<Rect> {
    let min_x = if options.left { left } else { 0 };
    let min_y = if options.top { top } else { 0 };
    let max_x = if options.right { right } else { width };
    let max_y = if options.bottom { bottom } else { height };
    if !(max_x > min_x && max_y > min_y) {
        return None;
    }
    Some(Rect::new(
        min_x as f64,
        min_y as f64,
        (max_x - min_x) as f64,
        (max_y - min_y) as f64,
    ))
}

/// `ImageTrim.calculateTrimRect(in:options:)`: the crop rectangle, in the image's own pixel
/// coordinates, that removes the background from the enabled edges.
///
/// Returns `None` when no edge is enabled, when the image is empty, or when no non-background content
/// remains — a fully transparent image, or one that is a single solid color (and so entirely
/// background to its own sample).
pub fn calculate_trim_rect(image: &Rgba8Image, options: &TrimOptions) -> Option<Rect> {
    if !options.trims_any() || image.is_empty() {
        return None;
    }
    let width = image.width();
    let height = image.height();
    match options.based_on {
        TrimBasedOn::TransparentPixels => {
            let edges = brush_alpha_bounds(image.data(), width, height, image.stride());
            // If edges[2] == 0 (right == 0), the entire image has alpha == 0.
            if edges[2] == 0 {
                return None;
            }
            trim_bounds(edges[0], edges[1], edges[2], edges[3], width, height, options)
        }
        TrimBasedOn::TopLeftPixelColor => {
            color_trim_rect(image, (0, 0), options)
        }
        TrimBasedOn::BottomRightPixelColor => {
            color_trim_rect(image, (width - 1, height - 1), options)
        }
    }
}

/// `ImageTrim.trimImage(_:options:)`: the image cropped to [`calculate_trim_rect`], or `None` when
/// there is nothing left to trim to.
pub fn trim_image(image: &Rgba8Image, options: &TrimOptions) -> Option<Rgba8Image> {
    let rect = calculate_trim_rect(image, options)?;
    image.cropped(rect)
}

/// `ImageTrim.trim(_:options:)`'s last step: the resize that crops the document to `rect` and keeps the
/// content where it is.
///
/// The caller renders the canvas, scans that raster with [`calculate_trim_rect`], and — unless
/// [`trim_is_noop`] says the rectangle already covers the canvas — resizes the document with these
/// options. The content offset is what the whole document moves by, so the caller moves the selection
/// (and anything else that lives in document space and is not part of the resize) by
/// [`CanvasSizeOptions::offset`] as well.
pub fn trim_canvas_options(rect: Rect) -> CanvasSizeOptions {
    let mut options = CanvasSizeOptions::new(rect.width() as usize, rect.height() as usize);
    options.content_offset = Some(Point::new(-rect.min_x(), -rect.min_y()));
    options
}

/// `ImageTrim.trim(_:options:)`: the trim is a no-op when the rectangle is the whole canvas at its
/// origin, in which case the snapshot is returned untouched and nothing is undone.
pub fn trim_is_noop(rect: Rect, width: usize, height: usize) -> bool {
    rect.width() as usize == width
        && rect.height() as usize == height
        && rect.min_x() == 0.0
        && rect.min_y() == 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Swift tests' `makeRGBAImage`: straight bytes, opaque unless the painter says otherwise.
    fn image(width: usize, height: usize, painter: impl Fn(usize, usize) -> [u8; 4]) -> Rgba8Image {
        let mut data = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) * 4;
                data[offset..offset + 4].copy_from_slice(&painter(x, y));
            }
        }
        Rgba8Image::from_data(width, height, data)
    }

    #[test]
    fn calculate_trim_rect_transparent_pixels() {
        // 20x20 image: left transparent 2 px, right 3 px, top 4 px, bottom 5 px.
        // Content: x 2..<17 (width 15), y 4..<15 (height 11).
        let image = image(20, 20, |x, y| {
            if x >= 2 && x < 17 && y >= 4 && y < 15 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 0, 0]
            }
        });

        let all = TrimOptions::default();
        assert_eq!(
            calculate_trim_rect(&image, &all),
            Some(Rect::new(2.0, 4.0, 15.0, 11.0))
        );

        let top_bottom = TrimOptions {
            left: false,
            right: false,
            ..TrimOptions::default()
        };
        assert_eq!(
            calculate_trim_rect(&image, &top_bottom),
            Some(Rect::new(0.0, 4.0, 20.0, 11.0))
        );

        let left_right = TrimOptions {
            top: false,
            bottom: false,
            ..TrimOptions::default()
        };
        assert_eq!(
            calculate_trim_rect(&image, &left_right),
            Some(Rect::new(2.0, 0.0, 15.0, 20.0))
        );
    }

    #[test]
    fn calculate_trim_rect_top_left_pixel_color() {
        // 16x16: a 3 px blue border around a 10x10 yellow center.
        let image = image(16, 16, |x, y| {
            if x >= 3 && x < 13 && y >= 3 && y < 13 {
                [255, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        });

        let options = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            ..TrimOptions::default()
        };
        assert_eq!(
            calculate_trim_rect(&image, &options),
            Some(Rect::new(3.0, 3.0, 10.0, 10.0))
        );

        let top_only = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            bottom: false,
            left: false,
            right: false,
            ..TrimOptions::default()
        };
        assert_eq!(
            calculate_trim_rect(&image, &top_only),
            Some(Rect::new(0.0, 3.0, 16.0, 13.0))
        );
    }

    #[test]
    fn calculate_trim_rect_bottom_right_pixel_color() {
        // 16x16: magenta for x >= 12 or y >= 12, cyan elsewhere; the bottom-right pixel is magenta.
        let image = image(16, 16, |x, y| {
            if x >= 12 || y >= 12 {
                [255, 0, 255, 255]
            } else {
                [0, 255, 255, 255]
            }
        });

        let options = TrimOptions {
            based_on: TrimBasedOn::BottomRightPixelColor,
            ..TrimOptions::default()
        };
        assert_eq!(
            calculate_trim_rect(&image, &options),
            Some(Rect::new(0.0, 0.0, 12.0, 12.0))
        );
    }

    #[test]
    fn uniform_image_returns_none() {
        let solid = image(8, 8, |_, _| [100, 150, 200, 255]);
        let options = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            ..TrimOptions::default()
        };
        assert_eq!(calculate_trim_rect(&solid, &options), None);

        let transparent = image(8, 8, |_, _| [0, 0, 0, 0]);
        assert_eq!(calculate_trim_rect(&transparent, &TrimOptions::default()), None);
    }

    #[test]
    fn tolerance_decides_how_much_of_a_color_counts_as_background() {
        // A 6x6 white field with a nearly white border: one level off, and within two at the corners.
        let image = image(6, 6, |x, y| {
            if x == 0 || y == 0 {
                [253, 253, 253, 255]
            } else {
                [255, 255, 255, 255]
            }
        });
        let options = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            tolerance: 2,
            ..TrimOptions::default()
        };
        // Everything is within the tolerance of the sampled corner, so nothing remains.
        assert_eq!(calculate_trim_rect(&image, &options), None);

        let exact = TrimOptions {
            based_on: TrimBasedOn::TopLeftPixelColor,
            ..TrimOptions::default()
        };
        // A tolerance of zero leaves the white field: columns and rows 1..<6.
        assert_eq!(
            calculate_trim_rect(&image, &exact),
            Some(Rect::new(1.0, 1.0, 5.0, 5.0))
        );
    }

    #[test]
    fn no_enabled_edge_trims_nothing() {
        let image = image(4, 4, |x, y| if x < 2 && y < 2 { [0, 0, 0, 0] } else { [9, 9, 9, 255] });
        let options = TrimOptions {
            top: false,
            bottom: false,
            left: false,
            right: false,
            ..TrimOptions::default()
        };
        assert!(!options.trims_any());
        assert_eq!(calculate_trim_rect(&image, &options), None);
        assert_eq!(calculate_trim_rect(&Rgba8Image::new(0, 0), &TrimOptions::default()), None);
    }

    #[test]
    fn trim_image_crops_directly() {
        let image = image(10, 10, |x, y| {
            if x >= 2 && x < 8 && y >= 2 && y < 8 {
                [255, 128, 0, 255]
            } else {
                [0, 0, 0, 0]
            }
        });
        let trimmed = trim_image(&image, &TrimOptions::default()).expect("content to trim");
        assert_eq!((trimmed.width(), trimmed.height()), (6, 6));
        assert_eq!(trimmed.get(0, 0), [255, 128, 0, 255]);
    }

    #[test]
    fn trim_turns_into_a_canvas_resize_that_keeps_the_content_in_place() {
        let rect = Rect::new(20.0, 30.0, 40.0, 40.0);
        let options = trim_canvas_options(rect);
        assert_eq!((options.width, options.height), (40, 40));
        assert_eq!(options.content_offset, Some(Point::new(-20.0, -30.0)));
        // The session moves everything in document space by that offset.
        assert_eq!(options.offset(100, 100), Point::new(-20.0, -30.0));
        assert!(!trim_is_noop(rect, 100, 100));
        assert!(trim_is_noop(Rect::new(0.0, 0.0, 100.0, 100.0), 100, 100));
        assert!(!trim_is_noop(Rect::new(0.0, 0.0, 100.0, 100.0), 100, 99));
    }

    #[test]
    fn trim_based_on_raw_values_round_trip() {
        assert_eq!(TrimBasedOn::ALL.len(), 3);
        for kind in TrimBasedOn::ALL {
            assert_eq!(TrimBasedOn::from_raw(kind.raw_value()), Some(kind));
        }
        assert_eq!(TrimBasedOn::from_raw("Transparent Pixels"), Some(TrimBasedOn::TransparentPixels));
        assert_eq!(TrimBasedOn::from_raw("nope"), None);
        assert_eq!(TrimError::NoContentToTrim.error_description(), "No content remained after trimming.");
        assert_eq!(TrimError::InvalidDimensions.error_description(), "The trimmed image dimensions are invalid.");
    }
}
