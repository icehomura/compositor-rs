//! Every editor command, as a gpui action in the `compositor` namespace.
//!
//! The names are the Swift commands' own names (`NewCanvas`, `OpenProject`, `MergeLayers`, …) so a view's
//! `on_action` handler reads like the `Button` it replaces, and the menu bar (built by `compositor-app`)
//! can name a command in one place. Commands that carry no data are unit structs: the view that answers
//! them reads `EditorSession` for its target. The few that do carry a choice — which filter, adjustment,
//! effect, tool mode, nudge direction or opacity digit — are declared with fields instead, so one action
//! stands for a family the way the Swift `ForEach` loops that built those menu items did.
//!
//! `compositor-ui/src/shortcuts.rs` maps the remappable shortcut table onto these actions, and both the
//! `actions!` list and the field-carrying types below are the whole vocabulary the UI listens for.

use compositor_core::image_ops::FilterKind;
use compositor_core::layer_adjustment::AdjustmentKind;
use compositor_core::layer_effects::LayerEffectKind;

/// The action namespace. Every name is `compositor::<Name>`; registering a second action under the same
/// name panics when the app starts, so this is the one list.
pub const NAMESPACE: &str = "compositor";

/// Which way a nudge moves, as the canvas's arrow-key keys read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArrowDirection {
    Left,
    Right,
    Up,
    Down,
}

gpui_kit::actions!(compositor, [
    // Undo/Redo (CommandGroup(replacing: .undoRedo)).
    Undo,
    Redo,
    // New/Open (CommandGroup(replacing: .newItem)).
    NewCanvas,
    OpenProject,
    ClearRecentProjects,
    ImportImages,
    // Save/Export (CommandGroup(replacing: .saveItem)).
    SaveProject,
    SaveProjectAs,
    ExportPng,
    ExportJpeg,
    CloseProject,
    // App menu.
    CheckForUpdates,
    ShowKeyboardShortcuts,
    HideCompositor,
    HideOthers,
    ShowAll,
    // View menu (CommandGroup(after: .toolbar) and its Show submenu).
    FitCanvas,
    ActualPixels,
    ZoomIn,
    ZoomOut,
    TogglePixelGrid,
    ToggleSnapping,
    ToggleTransformControls,
    ToggleGrid,
    ToggleGuides,
    GridSettings,
    ToggleRulers,
    ToggleSnap,
    ToggleSnapToGuides,
    ToggleSnapToGrid,
    ToggleSnapToLayers,
    ToggleSnapToDocumentBounds,
    ToggleLockGuides,
    ClearGuides,
    // Pasteboard and the fills (CommandGroup(replacing: .pasteboard) and after: .pasteboard).
    Cut,
    Copy,
    CopyMerged,
    Paste,
    FillWithForeground,
    FillWithBackground,
    ClearSelectionPixels,
    ContentAwareFill,
    // Select menu.
    SelectAll,
    Deselect,
    InvertSelection,
    LoadLayerSelection,
    SelectSubject,
    ColorRange,
    LoadMaskSelection,
    ExpandSelection,
    ContractSelection,
    FeatherSelection,
    // Image menu.
    Curves,
    Levels,
    HueSaturation,
    InvertPixels,
    CanvasSize,
    ImageSize,
    Trim,
    FlipCanvasHorizontal,
    FlipCanvasVertical,
    // Layer menu.
    EditAdjustment,
    DuplicateLayer,
    TransformLayerOrSelection,
    LayerViaCopy,
    ToggleClippingMask,
    GroupSelectedLayers,
    UngroupLayers,
    MoveOutOfFolder,
    NewBlankLayer,
    RenameLayer,
    ToggleLayerVisibility,
    MoveLayerUp,
    MoveLayerDown,
    MergeLayers,
    FlipLayerHorizontal,
    FlipLayerVertical,
    DeleteLayerOrMask,
    // Layer masks (the Layers panel's Add Mask menu and its context menu).
    AddWhiteMask,
    AddBlackMask,
    ToggleMask,
    DeleteMask,
    ToggleMaskLink,
    // Tools (the rail and the canvas's letter keys).
    SelectTool,
    MoveTool,
    HandTool,
    ZoomTool,
    BrushTool,
    EraserTool,
    SpotHealingTool,
    CloneStampTool,
    TypeTool,
    GradientTool,
    ShapeTool,
    EyedropperTool,
    MarqueeTool,
    MagicWandTool,
    LassoTool,
    BlurTool,
    CropTool,
    // Canvas and layer keys.
    SwapPaletteColors,
    ResetPaletteColors,
    CycleToolMode,
    TemporaryHandTool,
    DeleteKeyPressed,
    ApplyCanvasOperation,
    CancelCanvasOperation,
    DecreaseBrushSize,
    IncreaseBrushSize,
    DecreaseBrushHardness,
    IncreaseBrushHardness,
    PreviousBlendMode,
    NextBlendMode,
    CycleShapeKind,
    // Text editing.
    ToggleLevelsPreview,
    FinishEditingText,
]);

/// Opens a recent project's `.comp` file. The path is the project's own, as `RecentProjects` stores it.
///
/// Field-carrying actions are declared here rather than by the `actions!` macro; they carry no JSON form
/// (nothing loads them from a keymap file), so they are marked `no_json`.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct OpenRecentProject {
    pub path: String,
}

/// Opens one of the filters' or adjustments' editors, as the Filter menu's and Image menu's
/// `ForEach(FilterKind…)` loops did (`session.beginFilter(kind)`).
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct BeginFilter {
    pub kind: FilterKind,
}

/// Adds an adjustment layer of this kind, as `Layer > New Adjustment Layer` did
/// (`session.addAdjustment(kind)`).
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct AddAdjustment {
    pub kind: AdjustmentKind,
}

/// Adds a layer effect of this kind, as the Layers panel's effects menu did
/// (`session.addEffect(kind)`).
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct AddEffect {
    pub kind: LayerEffectKind,
}

/// Types one digit into the brush's opacity, as the canvas's `1`–`0` keys did
/// (`session.typeOpacityDigit(digit)`); two digits in quick succession are an exact percent.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct TypeOpacityDigit {
    pub digit: i32,
}

/// Nudges the layer (or, with a selection tool, the selection) by `distance` pixels.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct Nudge {
    pub direction: ArrowDirection,
    pub distance: i32,
}

/// Moves the selected pixels by `distance` pixels, whatever tool is active.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct MoveSelectedPixels {
    pub direction: ArrowDirection,
    pub distance: i32,
}

/// Changes a text layer's tracking; negative steps close the letters up.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct AdjustTextTracking {
    pub step: f64,
}

/// Changes a text layer's leading; the canvas counts from the Auto line height, with the up arrow closing
/// the lines up.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = compositor, no_json)]
pub struct AdjustTextLeading {
    pub step: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Action as _;

    /// The names the menu bar and the shortcut table look up are the Swift command names, namespaced.
    #[test]
    fn action_names_are_the_namespaced_swift_names() {
        assert_eq!(Undo.name(), "compositor::Undo");
        assert_eq!(SaveProject.name(), "compositor::SaveProject");
        assert_eq!(CycleToolMode.name(), "compositor::CycleToolMode");
        assert_eq!(
            Nudge {
                direction: ArrowDirection::Left,
                distance: 1,
            }
            .name(),
            "compositor::Nudge"
        );
        assert_eq!(
            AddEffect {
                kind: LayerEffectKind::ALL[0],
            }
            .name(),
            "compositor::AddEffect"
        );
        assert_eq!(NAMESPACE, "compositor");
    }

    /// Two actions of the same type but different data are not the same command.
    #[test]
    fn field_carrying_actions_compare_their_data() {
        let one_pixel = Nudge {
            direction: ArrowDirection::Left,
            distance: 1,
        };
        let ten_pixels = Nudge {
            direction: ArrowDirection::Left,
            distance: 10,
        };
        assert_ne!(one_pixel, ten_pixels);
        assert_eq!(one_pixel.direction, ArrowDirection::Left);
    }
}
