//! Canvas Size: the sheet's units and draft state, where the extension lands, and the color the
//! added canvas is filled with.

use crate::geom::{CGFloat, Point};
use crate::limits::MAX_SIDE_EXTENT;

/// The units the Canvas Size sheet offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanvasUnit {
    Pixels,
    Percent,
    Inches,
    Centimeters,
}

impl CanvasUnit {
    /// `CaseIterable` order, which is also the picker's order.
    pub const ALL: [CanvasUnit; 4] = [
        CanvasUnit::Pixels,
        CanvasUnit::Percent,
        CanvasUnit::Inches,
        CanvasUnit::Centimeters,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            CanvasUnit::Pixels => "Pixels",
            CanvasUnit::Percent => "Percent",
            CanvasUnit::Inches => "Inches",
            CanvasUnit::Centimeters => "Centimeters",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|unit| unit.raw_value() == value)
    }
}

/// The Canvas Size sheet's editable state.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasSizeDraft {
    pub original_width: usize,
    pub original_height: usize,
    pub resolution: f64,
    pub width: f64,
    pub height: f64,
    pub relative: bool,
    pub locked: bool,
    pub unit: CanvasUnit,
}

impl CanvasSizeDraft {
    pub fn new(width: usize, height: usize, resolution: f64) -> Self {
        Self {
            original_width: width,
            original_height: height,
            resolution,
            width: width as f64,
            height: height as f64,
            relative: false,
            locked: false,
            unit: CanvasUnit::Pixels,
        }
    }

    pub fn valid(&self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && (1.0..=MAX_SIDE_EXTENT).contains(&self.width.round())
            && (1.0..=MAX_SIDE_EXTENT).contains(&self.height.round())
    }

    pub fn displayed(&self, width_axis: bool) -> f64 {
        let original = if width_axis {
            self.original_width
        } else {
            self.original_height
        } as f64;
        let pixels = (if width_axis { self.width } else { self.height })
            - if self.relative { original } else { 0.0 };
        match self.unit {
            CanvasUnit::Pixels => pixels,
            CanvasUnit::Percent => pixels / original * 100.0,
            CanvasUnit::Inches => pixels / self.resolution,
            CanvasUnit::Centimeters => pixels / self.resolution * 2.54,
        }
    }

    pub fn set(&mut self, value: f64, width_axis: bool) {
        let original = if width_axis {
            self.original_width
        } else {
            self.original_height
        } as f64;
        let pixels = match self.unit {
            CanvasUnit::Pixels => value,
            CanvasUnit::Percent => value / 100.0 * original,
            CanvasUnit::Inches => value * self.resolution,
            CanvasUnit::Centimeters => value / 2.54 * self.resolution,
        };
        let final_value = pixels + if self.relative { original } else { 0.0 };
        if width_axis {
            self.width = final_value;
            if self.locked {
                self.height =
                    final_value * self.original_height as f64 / self.original_width as f64;
            }
        } else {
            self.height = final_value;
            if self.locked {
                self.width =
                    final_value * self.original_width as f64 / self.original_height as f64;
            }
        }
    }
}

/// What the Canvas Size sheet (or Crop) asks the resizer for.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasSizeOptions {
    pub width: usize,
    pub height: usize,
    /// Row-major, top-left through bottom-right.
    pub anchor: usize,
    pub fill: Option<CanvasExtensionColor>,
    /// Crop supplies an explicit document-space translation.
    pub content_offset: Option<Point>,
}

impl CanvasSizeOptions {
    /// The sheet's options: middle anchor, transparent extension, no explicit offset.
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            anchor: 4,
            fill: None,
            content_offset: None,
        }
    }

    pub fn offset(&self, from_width: usize, old_height: usize) -> Point {
        if let Some(content_offset) = self.content_offset {
            return content_offset;
        }
        // Floor puts the extra pixel on the right/bottom when expanding,
        // and removes it from the left/top when shrinking around the center.
        Point::new(
            ((self.width as f64 - from_width as f64) * (self.anchor % 3) as f64 / 2.0).floor(),
            ((self.height as f64 - old_height as f64) * (self.anchor / 3) as f64 / 2.0).floor(),
        )
    }
}

/// The straight sRGB color the canvas extension is filled with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasExtensionColor {
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
}

impl CanvasExtensionColor {
    pub fn new(red: CGFloat, green: CGFloat, blue: CGFloat) -> Self {
        Self { red, green, blue }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_keep_their_raw_values() {
        let values: Vec<&str> = CanvasUnit::ALL.iter().map(|unit| unit.raw_value()).collect();
        assert_eq!(values, ["Pixels", "Percent", "Inches", "Centimeters"]);
        assert_eq!(CanvasUnit::from_raw("Centimeters"), Some(CanvasUnit::Centimeters));
        assert_eq!(CanvasUnit::from_raw("Points"), None);
    }

    #[test]
    fn relative_ratio_and_units_use_final_dimensions() {
        // Ported from CompositorTests.CanvasSizeTests.
        let mut draft = CanvasSizeDraft::new(1000, 500, 100.0);
        draft.relative = true;
        draft.locked = true;
        draft.set(200.0, true);
        assert!(draft.width == 1200.0 && draft.height == 600.0);
        assert_eq!(draft.displayed(false), 100.0);
        draft.set(-250.0, false);
        assert!(draft.width == 500.0 && draft.height == 250.0);
        draft.relative = false;
        draft.unit = CanvasUnit::Inches;
        draft.set(10.0, true);
        assert!(draft.width == 1000.0 && draft.height == 500.0);
        draft.unit = CanvasUnit::Percent;
        draft.set(50.0, true);
        assert!(draft.width == 500.0 && draft.height == 250.0);
        draft.unit = CanvasUnit::Centimeters;
        assert!((draft.displayed(true) - 12.7).abs() < 0.001);
        draft.unit = CanvasUnit::Pixels;
        draft.set(0.0, true);
        assert!(!draft.valid());
    }

    #[test]
    fn every_anchor_offsets_the_old_content() {
        // Ported from CompositorTests.CanvasSizeTests: the expected offsets for a 64×32 canvas.
        for delta in [5i64, -5] {
            for anchor in 0..9usize {
                let options = CanvasSizeOptions {
                    anchor,
                    ..CanvasSizeOptions::new((64 + delta) as usize, (32 + delta) as usize)
                };
                let offset = options.offset(64, 32);
                let expected = [0.0, if delta == 5 { 2.0 } else { -3.0 }, delta as f64];
                assert_eq!(offset.x, expected[anchor % 3], "anchor {anchor}");
                assert_eq!(offset.y, expected[anchor / 3], "anchor {anchor}");
            }
        }
    }

    #[test]
    fn explicit_content_offset_wins_over_the_anchor() {
        let mut options = CanvasSizeOptions::new(10, 10);
        options.anchor = 0;
        options.content_offset = Some(Point::new(-2.0, 3.0));
        assert_eq!(options.offset(8, 8), Point::new(-2.0, 3.0));
        options.content_offset = None;
        assert_eq!(options.offset(8, 8), Point::new(0.0, 0.0));
    }

    #[test]
    fn valid_needs_whole_pixels_within_the_side_limit() {
        let mut draft = CanvasSizeDraft::new(10, 10, 72.0);
        assert!(draft.valid());
        draft.width = 0.6; // Rounds to 1, the smallest side.
        assert!(draft.valid());
        draft.width = 0.4;
        assert!(!draft.valid());
        draft.width = MAX_SIDE_EXTENT + 1.0;
        assert!(!draft.valid());
        draft.width = f64::INFINITY;
        assert!(!draft.valid());
    }
}
