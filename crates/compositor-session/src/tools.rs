//! The tool-option value types the tool rail, the tool headers and the options bars read.
//!
//! Ported from `Document/CloneStamp.swift` (`CloneSettings`), `Document/MagicWand.swift`
//! (`WandSettings`, `WandSampleSize`) and `Document/ObjectSelection.swift`
//! (`ObjectSelectionSettings`). `SpotHealingMode` is the pixels crate's kernel-side enum; it is
//! re-exported here so every tool header reaches the tool options in one place.

/// The Spot Healing Brush's modes (`SpotHealingMode`, `Document/BrushStroke.swift`).
pub use compositor_pixels::brush::SpotHealingMode;

/// Clone Stamp's options-bar settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloneSettings {
    /// The source moves with the brush and keeps its offset between strokes; off, every stroke
    /// starts again at the source point.
    pub aligned: bool,
    /// Copy from every visible layer as shown rather than the active layer alone.
    pub sample_all_layers: bool,
}

impl Default for CloneSettings {
    fn default() -> Self {
        Self {
            aligned: true,
            sample_all_layers: false,
        }
    }
}

/// How much of the image the wand averages around the click.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WandSampleSize {
    Point,
    ThreeByThree,
    FiveByFive,
}

impl WandSampleSize {
    /// `CaseIterable` order: the options bar's order.
    pub const ALL: [WandSampleSize; 3] = [
        WandSampleSize::Point,
        WandSampleSize::ThreeByThree,
        WandSampleSize::FiveByFive,
    ];

    /// The raw value is the enum's `Int` case order, as in Swift.
    pub fn raw_value(self) -> i32 {
        match self {
            WandSampleSize::Point => 0,
            WandSampleSize::ThreeByThree => 1,
            WandSampleSize::FiveByFive => 2,
        }
    }

    pub fn from_raw(value: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|size| size.raw_value() == value)
    }

    pub fn title(self) -> &'static str {
        ["Point Sample", "3 by 3 Average", "5 by 5 Average"][self.raw_value() as usize]
    }

    /// Pixels either side of the click that are averaged into the color to match.
    pub fn radius(self) -> i32 {
        self.raw_value()
    }
}

impl Default for WandSampleSize {
    fn default() -> Self {
        WandSampleSize::Point
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
        Self {
            tolerance: 32,
            sample_size: WandSampleSize::Point,
            contiguous: true,
            sample_all_layers: false,
        }
    }
}

/// Object Selection's options-bar settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectSelectionSettings {
    /// Read the visible composite rather than just the active layer.
    pub sample_all_layers: bool,
    /// Positive values erode the detected mask inward; negative values expand it outward.
    pub edge_offset: i32,
}

impl Default for ObjectSelectionSettings {
    fn default() -> Self {
        Self {
            sample_all_layers: true,
            edge_offset: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_swift_declarations() {
        assert_eq!(
            CloneSettings::default(),
            CloneSettings {
                aligned: true,
                sample_all_layers: false
            }
        );
        assert_eq!(
            WandSettings::default(),
            WandSettings {
                tolerance: 32,
                sample_size: WandSampleSize::Point,
                contiguous: true,
                sample_all_layers: false
            }
        );
        assert_eq!(
            ObjectSelectionSettings::default(),
            ObjectSelectionSettings {
                sample_all_layers: true,
                edge_offset: 0
            }
        );
    }

    #[test]
    fn wand_sample_sizes_keep_their_titles_and_radii() {
        assert_eq!(
            WandSampleSize::ALL.map(WandSampleSize::title),
            ["Point Sample", "3 by 3 Average", "5 by 5 Average"]
        );
        assert_eq!(WandSampleSize::ALL.map(WandSampleSize::radius), [0, 1, 2]);
        for size in WandSampleSize::ALL {
            assert_eq!(WandSampleSize::from_raw(size.raw_value()), Some(size));
        }
        assert_eq!(WandSampleSize::from_raw(3), None);
    }
}
