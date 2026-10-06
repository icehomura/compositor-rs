//! Alignment guides, the non-printing layout grid and its appearance, and a guide drag in progress.
//! Ported from `Document/Guides.swift`.

use std::hash::{Hash, Hasher};
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::color::PaletteColor;
use crate::geom::CGFloat;
use crate::Id;

/// The guide's direction (`CanvasGuide.Axis`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CanvasGuideAxis {
    #[serde(rename = "horizontal")]
    Horizontal,
    #[serde(rename = "vertical")]
    Vertical,
}

/// A user-placed alignment line. Horizontal guides sit at a document Y; vertical at a document X.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanvasGuide {
    pub id: Id,
    pub axis: CanvasGuideAxis,
    /// Document pixels: Y for a horizontal guide, X for a vertical one.
    pub position: f64,
}

impl CanvasGuide {
    pub fn new(id: Id, axis: CanvasGuideAxis, position: f64) -> Self {
        CanvasGuide { id, axis, position }
    }

    /// A new guide of the given axis, with a fresh id.
    pub fn at(axis: CanvasGuideAxis, position: f64) -> Self {
        Self::new(crate::new_id(), axis, position)
    }

    pub fn offset(&self, x: CGFloat, y: CGFloat) -> CanvasGuide {
        let mut guide = *self;
        guide.position += if self.axis == CanvasGuideAxis::Vertical { x } else { y };
        guide
    }

    pub fn scaled(&self, x: CGFloat, y: CGFloat) -> CanvasGuide {
        let mut guide = *self;
        guide.position *= if self.axis == CanvasGuideAxis::Vertical { x } else { y };
        guide
    }

    /// Mirrors this guide when it runs perpendicular to the flip, so it stays on the same content.
    pub fn mirrored(&self, horizontally: bool, across: CGFloat) -> CanvasGuide {
        let mut guide = *self;
        if (horizontally && self.axis == CanvasGuideAxis::Vertical)
            || (!horizontally && self.axis == CanvasGuideAxis::Horizontal)
        {
            guide.position = 2.0 * across - self.position;
        }
        guide
    }
}

/// Swift's `Hashable` conformance, synthesized over the three stored properties. Like Swift, a
/// `Double` hashes by its bits, so a guide is only ever hashed by the value it holds.
impl Eq for CanvasGuide {}

impl Hash for CanvasGuide {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.axis.hash(state);
        self.position.to_bits().hash(state);
    }
}

/// Non-printing layout grid: a major line every `spacing` px, split into `subdivisions`
/// (64 px and eight, every 8 px, until changed in View > Grid Settings…).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutGrid {
    /// Pixels between major lines.
    pub spacing: usize,
    /// Parts each major square is split into; never finer than a pixel.
    pub subdivisions: usize,
}

impl LayoutGrid {
    pub const SPACING_RANGE: RangeInclusive<usize> = 2..=4096;
    pub const SUBDIVISION_RANGE: RangeInclusive<usize> = 1..=64;

    /// Clamps both values, and never lets the subdivisions be finer than a pixel.
    pub fn new(spacing: usize, subdivisions: usize) -> Self {
        let spacing = spacing.clamp(*Self::SPACING_RANGE.start(), *Self::SPACING_RANGE.end());
        let subdivisions = subdivisions.clamp(*Self::SUBDIVISION_RANGE.start(), *Self::SUBDIVISION_RANGE.end());
        LayoutGrid {
            spacing,
            subdivisions: subdivisions.min(spacing),
        }
    }

    pub fn spacing(&self) -> usize {
        self.spacing
    }

    pub fn subdivisions(&self) -> usize {
        self.subdivisions
    }

    /// Pixels between one grid line and the next.
    pub fn step(&self) -> CGFloat {
        self.spacing as CGFloat / self.subdivisions as CGFloat
    }

    /// Every grid line along a document edge, including subdivisions, in whole pixels.
    /// Counted from the origin rather than added up, so an uneven step doesn't drift off the majors.
    pub fn lines(&self, length: CGFloat) -> Vec<CGFloat> {
        if !(length >= 0.0) {
            return vec![0.0];
        }
        let count = (length / self.step() + 0.001).floor() as i64;
        (0..=count).map(|index| (index as CGFloat * self.step()).round()).collect()
    }

    pub fn is_major(&self, value: CGFloat) -> bool {
        (value.round() % self.spacing as CGFloat).abs() < 0.001
    }
}

impl Default for LayoutGrid {
    fn default() -> Self {
        LayoutGrid::new(64, 8)
    }
}

/// How the layout grid is drawn (View > Grid Settings…), after Photoshop's Guides, Grid & Slices
/// settings. Lines are drawn at the chosen opacity; subdivisions are dotted and fainter still.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridAppearance {
    pub preset: GridAppearancePreset,
    /// Used while `preset` is Custom; kept when another preset is chosen, so switching back finds it.
    pub custom_color: PaletteColor,
    pub style: GridAppearanceStyle,
    /// The major lines' opacity, in percent.
    pub opacity: usize,
}

impl GridAppearance {
    pub const OPACITY_RANGE: RangeInclusive<usize> = 1..=100;

    /// Nil for Custom, which uses the appearance's own color.
    pub fn color(&self) -> PaletteColor {
        self.preset.color().unwrap_or(self.custom_color)
    }

    pub fn major_alpha(&self) -> CGFloat {
        self.opacity.clamp(*Self::OPACITY_RANGE.start(), *Self::OPACITY_RANGE.end()) as CGFloat / 100.0
    }

    /// Subdivisions at a little over half the majors' opacity: 28% beside the default 45%.
    pub fn subdivision_alpha(&self) -> CGFloat {
        self.major_alpha() * 28.0 / 45.0
    }
}

impl Default for GridAppearance {
    fn default() -> Self {
        GridAppearance {
            preset: GridAppearancePreset::LightGray,
            custom_color: PaletteColor::new(0.7, 0.7, 0.7),
            style: GridAppearanceStyle::Lines,
            opacity: 45,
        }
    }
}

/// The grid's color preset (`GridAppearance.Preset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GridAppearancePreset {
    LightGray,
    LightBlue,
    LightRed,
    Green,
    MediumBlue,
    Yellow,
    Magenta,
    Cyan,
    Black,
    Custom,
}

impl GridAppearancePreset {
    /// `CaseIterable` order, which is also the menu's order.
    pub const ALL: [GridAppearancePreset; 10] = [
        GridAppearancePreset::LightGray,
        GridAppearancePreset::LightBlue,
        GridAppearancePreset::LightRed,
        GridAppearancePreset::Green,
        GridAppearancePreset::MediumBlue,
        GridAppearancePreset::Yellow,
        GridAppearancePreset::Magenta,
        GridAppearancePreset::Cyan,
        GridAppearancePreset::Black,
        GridAppearancePreset::Custom,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            GridAppearancePreset::LightGray => "Light Gray",
            GridAppearancePreset::LightBlue => "Light Blue",
            GridAppearancePreset::LightRed => "Light Red",
            GridAppearancePreset::Green => "Green",
            GridAppearancePreset::MediumBlue => "Medium Blue",
            GridAppearancePreset::Yellow => "Yellow",
            GridAppearancePreset::Magenta => "Magenta",
            GridAppearancePreset::Cyan => "Cyan",
            GridAppearancePreset::Black => "Black",
            GridAppearancePreset::Custom => "Custom",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.raw_value() == value)
    }

    /// Nil for Custom, which uses the appearance's own color.
    pub fn color(self) -> Option<PaletteColor> {
        match self {
            GridAppearancePreset::LightGray => Some(PaletteColor::new(0.7, 0.7, 0.7)),
            GridAppearancePreset::LightBlue => Some(PaletteColor::new(0.29, 0.78, 1.0)),
            GridAppearancePreset::LightRed => Some(PaletteColor::new(1.0, 0.4, 0.4)),
            GridAppearancePreset::Green => Some(PaletteColor::new(0.25, 0.8, 0.25)),
            GridAppearancePreset::MediumBlue => Some(PaletteColor::new(0.2, 0.4, 1.0)),
            GridAppearancePreset::Yellow => Some(PaletteColor::new(1.0, 1.0, 0.0)),
            GridAppearancePreset::Magenta => Some(PaletteColor::new(1.0, 0.0, 1.0)),
            GridAppearancePreset::Cyan => Some(PaletteColor::new(0.0, 1.0, 1.0)),
            GridAppearancePreset::Black => Some(PaletteColor::BLACK),
            GridAppearancePreset::Custom => None,
        }
    }
}

/// The major lines' pattern; subdivisions stay dotted (`GridAppearance.Style`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GridAppearanceStyle {
    Lines,
    DashedLines,
    Dots,
}

impl GridAppearanceStyle {
    /// `CaseIterable` order, which is also the menu's order.
    pub const ALL: [GridAppearanceStyle; 3] = [
        GridAppearanceStyle::Lines,
        GridAppearanceStyle::DashedLines,
        GridAppearanceStyle::Dots,
    ];

    pub fn raw_value(self) -> &'static str {
        match self {
            GridAppearanceStyle::Lines => "Lines",
            GridAppearanceStyle::DashedLines => "Dashed Lines",
            GridAppearanceStyle::Dots => "Dots",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.raw_value() == value)
    }

    /// On and off lengths in screen points; empty for a solid line.
    pub fn dashes(self) -> &'static [CGFloat] {
        match self {
            GridAppearanceStyle::Lines => &[],
            GridAppearanceStyle::DashedLines => &[4.0, 3.0],
            GridAppearanceStyle::Dots => &[1.0, 2.0],
        }
    }
}

/// In-progress create or move; the document is updated only when the drag finishes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuideDrag {
    pub id: Id,
    pub axis: CanvasGuideAxis,
    pub position: f64,
    pub is_new: bool,
    pub original: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_guide_uses_the_manifest_key_spellings() {
        let id = crate::new_id();
        let guide = CanvasGuide::new(id, CanvasGuideAxis::Vertical, 12.5);
        let value = serde_json::to_value(guide).expect("a guide serializes");
        assert_eq!(value, serde_json::json!({ "id": id.to_string(), "axis": "vertical", "position": 12.5 }));
        let round_trip: CanvasGuide = serde_json::from_value(value).expect("a guide reads back");
        assert_eq!(round_trip, guide);

        let horizontal = serde_json::to_value(CanvasGuide::at(CanvasGuideAxis::Horizontal, 0.0)).unwrap();
        assert_eq!(horizontal["axis"], serde_json::json!("horizontal"));
    }

    #[test]
    fn canvas_guide_offsets_scales_and_mirrors() {
        let vertical = CanvasGuide::new(crate::new_id(), CanvasGuideAxis::Vertical, 10.0);
        let horizontal = CanvasGuide::new(crate::new_id(), CanvasGuideAxis::Horizontal, 10.0);

        assert_eq!(vertical.offset(5.0, 7.0).position, 15.0);
        assert_eq!(horizontal.offset(5.0, 7.0).position, 17.0);
        assert_eq!(vertical.scaled(2.0, 3.0).position, 20.0);
        assert_eq!(horizontal.scaled(2.0, 3.0).position, 30.0);

        // A vertical guide mirrors across a horizontal flip, and vice versa; the other axis is left alone.
        assert_eq!(vertical.mirrored(true, 50.0).position, 90.0);
        assert_eq!(vertical.mirrored(false, 50.0).position, 10.0);
        assert_eq!(horizontal.mirrored(false, 50.0).position, 90.0);
        assert_eq!(horizontal.mirrored(true, 50.0).position, 10.0);
    }

    #[test]
    fn layout_grid_clamps_its_values() {
        assert_eq!(LayoutGrid::default(), LayoutGrid::new(64, 8));
        assert_eq!(LayoutGrid::new(0, 0), LayoutGrid::new(2, 1));
        assert_eq!(LayoutGrid::new(99_999, 99), LayoutGrid::new(4096, 64));
        assert_eq!(LayoutGrid::new(64, 8).step(), 8.0);
        // Subdivisions can never be finer than a pixel.
        assert_eq!(LayoutGrid::new(2, 64).subdivisions, 2);
        // Ported from CompositorTests.GuideTests.layoutGridKeepsToItsLimits: spacing 10 caps at 10.
        assert_eq!(LayoutGrid::new(10, 40).subdivisions, 10, "no finer than a pixel");
        assert_eq!(LayoutGrid::new(1_000_000, 1_000).spacing, *LayoutGrid::SPACING_RANGE.end());
        assert_eq!(LayoutGrid::new(1_000_000, 1_000).subdivisions, *LayoutGrid::SUBDIVISION_RANGE.end());
    }

    #[test]
    fn layout_grid_takes_its_spacing_and_subdivisions() {
        // Ported from CompositorTests.GuideTests.layoutGridTakesItsSpacingAndSubdivisions.
        let grid = LayoutGrid::new(100, 4);
        assert_eq!(
            grid.lines(200.0),
            vec![0.0, 25.0, 50.0, 75.0, 100.0, 125.0, 150.0, 175.0, 200.0]
        );
        assert!(grid.is_major(100.0) && !grid.is_major(50.0));

        // An uneven step still lands on every major line.
        let thirds = LayoutGrid::new(100, 3);
        let majors: Vec<CGFloat> = thirds
            .lines(300.0)
            .into_iter()
            .filter(|value| thirds.is_major(*value))
            .collect();
        assert_eq!(majors, vec![0.0, 100.0, 200.0, 300.0]);
        assert_eq!(LayoutGrid::new(50, 1).lines(120.0), vec![0.0, 50.0, 100.0]);
    }

    #[test]
    fn layout_grid_lines_and_majors() {
        let grid = LayoutGrid::new(64, 8);
        assert_eq!(grid.lines(0.0), vec![0.0]);
        assert_eq!(grid.lines(-1.0), vec![0.0]);
        assert_eq!(grid.lines(16.0), vec![0.0, 8.0, 16.0]);
        assert_eq!(grid.lines(100.0), (0..=12).map(|index| (index * 8) as CGFloat).collect::<Vec<_>>());
        assert!(grid.is_major(64.0));
        assert!(grid.is_major(0.0));
        assert!(!grid.is_major(8.0));
    }

    #[test]
    fn grid_appearance_alphas_and_colors() {
        let appearance = GridAppearance::default();
        assert_eq!(appearance.major_alpha(), 0.45);
        // 0.45 * 28 / 45 in binary floating point is a hair under 0.28 — the Swift computes the same
        // expression and lands on the same double.
        assert!((appearance.subdivision_alpha() - 0.28).abs() < 1e-12);
        assert_eq!(appearance.color(), PaletteColor::new(0.7, 0.7, 0.7));

        let custom = GridAppearance {
            preset: GridAppearancePreset::Custom,
            custom_color: PaletteColor::new(0.1, 0.2, 0.3),
            style: GridAppearanceStyle::Dots,
            // Out of range: clamped by the getters, exactly like the Swift.
            opacity: 250,
        };
        assert_eq!(custom.color(), PaletteColor::new(0.1, 0.2, 0.3));
        assert_eq!(custom.major_alpha(), 1.0);

        assert_eq!(GridAppearancePreset::ALL.len(), 10);
        assert_eq!(GridAppearancePreset::LightBlue.color(), Some(PaletteColor::new(0.29, 0.78, 1.0)));
        assert_eq!(GridAppearancePreset::Custom.color(), None);
        assert_eq!(GridAppearancePreset::from_raw("Medium Blue"), Some(GridAppearancePreset::MediumBlue));
        assert_eq!(GridAppearancePreset::Black.raw_value(), "Black");

        assert!(GridAppearanceStyle::Lines.dashes().is_empty());
        assert_eq!(GridAppearanceStyle::DashedLines.dashes(), &[4.0, 3.0]);
        assert_eq!(GridAppearanceStyle::Dots.dashes(), &[1.0, 2.0]);
        assert_eq!(GridAppearanceStyle::from_raw("Dashed Lines"), Some(GridAppearanceStyle::DashedLines));
    }

    #[test]
    fn grid_appearance_colors_and_styles() {
        // Ported from CompositorTests.GuideTests.gridAppearanceColorsAndStyles — the assertions the
        // alpha/color test above does not already make.
        let standard = GridAppearance::default();
        assert_eq!(standard.preset, GridAppearancePreset::LightGray);
        assert_eq!(standard.style, GridAppearanceStyle::Lines);
        assert!(GridAppearancePreset::ALL
            .iter()
            .all(|preset| preset.color().is_none() == (*preset == GridAppearancePreset::Custom)));

        let mut appearance = standard;
        appearance.opacity = 100;
        assert_eq!(appearance.major_alpha(), 1.0);
        assert!(appearance.subdivision_alpha() < 1.0, "subdivisions stay fainter than the majors");
        appearance.opacity = 0;
        assert_eq!(appearance.major_alpha(), 0.01, "a grid that's on never disappears");
    }

    #[test]
    fn guide_drag_is_a_plain_value() {
        let drag = GuideDrag {
            id: crate::new_id(),
            axis: CanvasGuideAxis::Horizontal,
            position: 10.0,
            is_new: true,
            original: None,
        };
        assert_eq!(drag, drag);
        assert!(drag.is_new);
        assert_eq!(drag.original, None);
    }
}
