//! The tool-option value types the tool rail, the tool headers and the options bars read.
//!
//! Ported from `Document/CloneStamp.swift` (`CloneSettings`), `Document/MagicWand.swift`
//! (`WandSettings`, `WandSampleSize`) and `Document/ObjectSelection.swift`
//! (`ObjectSelectionSettings`). The wand and object-selection settings — and the Spot Healing
//! Brush's `SpotHealingMode` — are the same value types the pixel kernels take
//! (`compositor_pixels::masks`, `compositor_pixels::brush`); they are re-exported here so every tool
//! header reaches the tool options in one place, and so the session's fields and the kernel calls
//! are the same types.

/// The Spot Healing Brush's modes (`SpotHealingMode`, `Document/BrushStroke.swift`).
pub use compositor_pixels::brush::SpotHealingMode;
/// The Magic Wand's options-bar settings and sample sizes (`Document/MagicWand.swift`).
pub use compositor_pixels::masks::{WandSampleSize, WandSettings};
/// Object Selection's options-bar settings (`Document/ObjectSelection.swift`).
pub use compositor_pixels::masks::ObjectSelectionSettings;

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

/// How much of the image the wand averages around the click, and the wand's and Object Selection's
/// options-bar settings, are the pixels crate's own value types; `tools.rs` re-exports them above.

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
