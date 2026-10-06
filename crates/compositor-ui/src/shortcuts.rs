//! The remappable keyboard shortcuts (`UI/KeyboardShortcuts.swift`).
//!
//! `ShortcutChord` is one key plus the four stored modifier bits, `ShortcutDefinition::all()` is the table
//! the menus, the canvas and the Keyboard Shortcuts sheet all read, and `ShortcutSettings` is the person's
//! `keyboardShortcuts.v1` overrides over that table. The overrides live in the same
//! `compositor_core::settings` store as the rest of the app, and a stored map that `problem(in:)` refuses
//! is ignored, as it was when it came back out of `UserDefaults`.
//!
//! There is no `NSEvent` and no responder chain in gpui, so the two methods that translated one event at
//! a time — `canvasEvent(_:)` at the canvas/layer boundary and `textEvent(_:)` in the text editor —
//! become `ShortcutSettings::bindings()`: the resolved chords of the whole table, which the app hands to
//! `App::bind_keys` for gpui to dispatch from. The rules those methods encoded are the rules the bindings
//! keep. Only a definition's *resolved* chord is bound, so a reassigned shortcut no longer answers at its
//! old chord (the canvas does not see it, which is the `nil` those methods returned) and answers at its
//! new one; a Shift-only letter key falls back to the base assignment unless Shift has its own command,
//! which `ShortcutSettings::shift_fallback` binds as the base key's Shift variant (a chord of the table's
//! own — cycle shape, brush hardness, the blend-mode steps, content-aware fill — answers first, and Tab's
//! own branch takes no modifiers, so Shift+Tab stays the toolkit's); and text-editing
//! shortcuts keep Command, Option or Control, since `problem(in:)` never stores a map that takes them
//! away. Canvas & layer keys bind in
//! [`CANVAS_CONTEXT`] and text-editing keys in [`TEXT_EDITING_CONTEXT`], the port's stand-in for the two
//! responder boundaries, and menu shortcuts bind app-wide because menu items are.
//!
//! The Command bit is gpui's *platform* modifier, spelled `secondary` in a keystroke string: `cmd` on
//! macOS and `ctrl` on Windows, which is what gpui-pre's parser makes of it — `cmd`, `super` and `win`
//! are separate spellings for the Windows key, which is not the primary modifier here. The Control bit is
//! spelled `ctrl` and on Windows is the same physical key as `secondary`, so a remap that puts Control on
//! one shortcut where another has Command collides. `label()` renders the macOS glyphs as
//! `Ctrl`/`Alt`/`Shift`/`Cmd`, in the order the glyphs were drawn; on Windows the `Cmd` part of a label is
//! the Ctrl key.

use std::collections::HashMap;
use std::sync::LazyLock;

use compositor_core::settings;
use gpui_kit::{KeyBinding, Keystroke, Modifiers};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::actions::{
    ActualPixels, AdjustTextLeading, AdjustTextTracking, ApplyCanvasOperation, ArrowDirection,
    BlurTool, BrushTool, CancelCanvasOperation, CanvasSize, CloseProject, CloneStampTool,
    ContentAwareFill, Copy, CopyMerged, CropTool, Curves, Cut, CycleShapeKind, CycleToolMode,
    DecreaseBrushHardness, DecreaseBrushSize, DeleteKeyPressed, Deselect,
    EraserTool, ExportJpeg, ExportPng, EyedropperTool, FillWithBackground, FillWithForeground,
    FinishEditingText, FitCanvas, GradientTool, GroupSelectedLayers, HandTool, HideCompositor,
    HueSaturation, ImageSize, IncreaseBrushHardness, IncreaseBrushSize, InvertPixels,
    InvertSelection, LayerViaCopy, LassoTool, Levels, MagicWandTool, MarqueeTool, MergeLayers,
    MoveLayerDown, MoveLayerUp, MoveSelectedPixels, MoveTool, NewBlankLayer, NewCanvas, NextBlendMode,
    Nudge, OpenProject, Paste, PreviousBlendMode, Redo, ResetPaletteColors, SaveProject,
    SaveProjectAs, SelectAll, SelectSubject, SelectTool, ShapeTool, SpotHealingTool,
    SwapPaletteColors, TemporaryHandTool, ToggleClippingMask, ToggleGrid, ToggleGuides,
    ToggleLevelsPreview, ToggleLockGuides, ToggleRulers, ToggleSnapping,
    ToggleTransformControls, TransformLayerOrSelection, TypeOpacityDigit, TypeTool, Undo,
    UngroupLayers, ZoomIn, ZoomOut, ZoomTool,
};

/// The stored Command (⌘) bit — gpui's *platform* modifier, the primary one.
pub const COMMAND: i32 = 1;
/// The stored Option (⌥) bit.
pub const OPTION: i32 = 2;
/// The stored Control (⌃) bit.
pub const CONTROL: i32 = 4;
/// The stored Shift (⇧) bit.
pub const SHIFT: i32 = 8;

/// The group the menu items' shortcuts are in (`isMenu`).
pub const MENUS_GROUP: &str = "Menus";
/// The group of the canvas's and the layers panel's keys.
pub const CANVAS_GROUP: &str = "Canvas & Layers";
/// The group of the in-canvas text editor's keys.
pub const TEXT_EDITING_GROUP: &str = "Text Editing";
/// The three groups, in the order the Keyboard Shortcuts sheet lists them.
pub const GROUPS: [&str; 3] = [MENUS_GROUP, CANVAS_GROUP, TEXT_EDITING_GROUP];

/// The key-binding context of the canvas/layer responder boundary (`canvasEvent(_:)`). The canvas element
/// carries it (`key_context`), so these keys stay away from text fields and dialog controls, which keep
/// their normal typing and navigation behavior.
pub const CANVAS_CONTEXT: &str = "Canvas";
/// The key-binding context of the in-canvas text editor (`textEvent(_:)`).
pub const TEXT_EDITING_CONTEXT: &str = "TextEditing";

/// One key combination: the key and the four stored modifier bits.
///
/// The bits are stable — they are what `keyboardShortcuts.v1` stores — so they keep the Swift order:
/// Command, Option, Control, Shift. `Codable` is the serde derive, and it writes the same object the
/// Swift one did: a `{"key": "z", "modifiers": 1}` per definition id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShortcutChord {
    pub key: String,
    pub modifiers: i32,
}

impl ShortcutChord {
    /// `init(_ key:_ modifiers:)`.
    pub fn new(key: impl Into<String>, modifiers: i32) -> Self {
        Self {
            key: key.into(),
            modifiers,
        }
    }

    /// `init(_ event: NSEvent)`: the gpui keystroke in the AppKit event's place.
    ///
    /// Windows reports the shifted character for `[`, `]`, `=` and `-` with the Shift bit already
    /// cleared (`gpui-pre-windows`' keyboard layout), so mapping that character back to its plain key
    /// puts the Shift bit back as well: the chord reads exactly as the AppKit event would have read it.
    pub fn from_keystroke(keystroke: &Keystroke) -> Self {
        let mut modifiers = modifier_bits(&keystroke.modifiers);
        let key = match keystroke.key.as_str() {
            "delete" | "backspace" => "\u{7f}".to_string(),
            "enter" => "\r".to_string(),
            "escape" => "\u{1b}".to_string(),
            "tab" => "\t".to_string(),
            "space" => " ".to_string(),
            "left" => "\u{f702}".to_string(),
            "right" => "\u{f703}".to_string(),
            "down" => "\u{f701}".to_string(),
            "up" => "\u{f700}".to_string(),
            typed => {
                let typed = typed.to_lowercase();
                match UNSHIFTED_KEYS
                    .iter()
                    .find(|(shifted, _)| *shifted == typed.as_str())
                {
                    Some((_, plain)) => {
                        modifiers |= SHIFT;
                        (*plain).to_string()
                    }
                    None => typed,
                }
            }
        };
        Self { key, modifiers }
    }

    /// `eventModifiers` (and `cocoaModifiers`: gpui has the one modifier type).
    pub fn event_modifiers(&self) -> Modifiers {
        modifier_flags(self.modifiers)
    }

    /// `label`: the modifiers in the glyphs' order, joined with `+`, then the key.
    pub fn label(&self) -> String {
        let mut label = String::new();
        if self.modifiers & CONTROL != 0 {
            label.push_str("Ctrl+");
        }
        if self.modifiers & OPTION != 0 {
            label.push_str("Alt+");
        }
        if self.modifiers & SHIFT != 0 {
            label.push_str("Shift+");
        }
        if self.modifiers & COMMAND != 0 {
            label.push_str("Cmd+");
        }
        match SPECIAL_KEYS.iter().find(|(key, _)| *key == self.key) {
            Some((_, name)) => label.push_str(name),
            None => label.push_str(&self.key.to_uppercase()),
        }
        label
    }

    /// The keystroke strings `KeyBinding::new` takes — `event(like:)`'s replacement, since a binding is
    /// what gpui dispatches on. A chord is usually one keystroke; the Mac's one Delete key and a
    /// top-row digit with Shift are two on Windows, so those chords bind to both spellings.
    pub fn keystrokes(&self) -> Vec<String> {
        let mut modifiers = String::new();
        if self.modifiers & CONTROL != 0 {
            modifiers.push_str("ctrl-");
        }
        if self.modifiers & OPTION != 0 {
            modifiers.push_str("alt-");
        }
        if self.modifiers & COMMAND != 0 {
            modifiers.push_str("secondary-");
        }
        self.bound_keys()
            .into_iter()
            .map(|(key, shift)| format!("{modifiers}{}{key}", if shift { "shift-" } else { "" }))
            .collect()
    }

    /// The gpui names for this chord's key, each with the `shift-` prefix that goes with it.
    fn bound_keys(&self) -> Vec<(String, bool)> {
        let shift = self.modifiers & SHIFT != 0;
        if self.key == "\u{7f}" {
            return vec![
                ("delete".to_string(), shift),
                ("backspace".to_string(), shift),
            ];
        }
        if let Some((_, names)) = KEY_NAMES.iter().find(|(key, _)| *key == self.key) {
            return names.iter().map(|name| ((*name).to_string(), shift)).collect();
        }
        if shift {
            let spellings = shifted_spellings(&self.key);
            if !spellings.is_empty() {
                return spellings;
            }
        }
        vec![(self.key.clone(), shift)]
    }
}

/// One line of the shortcut table: what it is called, which group it is in and the chord it started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutDefinition {
    pub title: String,
    pub group: String,
    pub original: ShortcutChord,
}

impl ShortcutDefinition {
    /// `init(title:group:original:)`.
    pub fn new(title: &str, group: &str, original: ShortcutChord) -> Self {
        Self {
            title: title.to_string(),
            group: group.to_string(),
            original,
        }
    }

    /// Every remappable shortcut, in the order the Keyboard Shortcuts sheet lists them.
    pub fn all() -> &'static [ShortcutDefinition] {
        static ALL: LazyLock<Vec<ShortcutDefinition>> = LazyLock::new(build_all);
        &ALL
    }

    /// `id`: the overrides' key for this definition.
    pub fn id(&self) -> String {
        format!("{}:{}", self.group, self.title)
    }

    /// `isMenu`.
    pub fn is_menu(&self) -> bool {
        self.group == MENUS_GROUP
    }

    /// The gpui key-binding context this definition's responder boundary corresponds to: `None` for the
    /// menu items, which are app-wide, then the canvas and the text editor.
    pub fn context(&self) -> Option<&'static str> {
        if self.group == CANVAS_GROUP {
            Some(CANVAS_CONTEXT)
        } else if self.group == TEXT_EDITING_GROUP {
            Some(TEXT_EDITING_CONTEXT)
        } else {
            None
        }
    }

    /// The gpui binding this definition runs at `keystroke`: the `compositor` action the Swift menu item
    /// or canvas key ran, in its group's context.
    fn binding(&self, keystroke: &str) -> KeyBinding {
        let context = self.context();
        match self.title.as_str() {
            // Menus.
            "Undo" => KeyBinding::new(keystroke, Undo, context),
            "Redo" => KeyBinding::new(keystroke, Redo, context),
            "New Canvas" => KeyBinding::new(keystroke, NewCanvas, context),
            "Open Project" => KeyBinding::new(keystroke, OpenProject, context),
            "Save" => KeyBinding::new(keystroke, SaveProject, context),
            "Save As" => KeyBinding::new(keystroke, SaveProjectAs, context),
            "Export PNG" => KeyBinding::new(keystroke, ExportPng, context),
            "Export JPEG" => KeyBinding::new(keystroke, ExportJpeg, context),
            "Close Project" => KeyBinding::new(keystroke, CloseProject, context),
            "Fit Canvas" => KeyBinding::new(keystroke, FitCanvas, context),
            "Actual Pixels" => KeyBinding::new(keystroke, ActualPixels, context),
            "Zoom In" => KeyBinding::new(keystroke, ZoomIn, context),
            "Zoom Out" => KeyBinding::new(keystroke, ZoomOut, context),
            "Show Transform Controls" => KeyBinding::new(keystroke, ToggleTransformControls, context),
            "Hide Compositor" => KeyBinding::new(keystroke, HideCompositor, context),
            "Cut" => KeyBinding::new(keystroke, Cut, context),
            "Copy" => KeyBinding::new(keystroke, Copy, context),
            "Copy Merged" => KeyBinding::new(keystroke, CopyMerged, context),
            "Paste" => KeyBinding::new(keystroke, Paste, context),
            "Fill with Foreground" => KeyBinding::new(keystroke, FillWithForeground, context),
            "Fill with Background" => KeyBinding::new(keystroke, FillWithBackground, context),
            "Content-Aware Fill" => KeyBinding::new(keystroke, ContentAwareFill, context),
            "Select All" => KeyBinding::new(keystroke, SelectAll, context),
            "Deselect" => KeyBinding::new(keystroke, Deselect, context),
            "Inverse Selection" => KeyBinding::new(keystroke, InvertSelection, context),
            "Select Subject" => KeyBinding::new(keystroke, SelectSubject, context),
            "Curves" => KeyBinding::new(keystroke, Curves, context),
            "Levels" => KeyBinding::new(keystroke, Levels, context),
            "Hue/Saturation" => KeyBinding::new(keystroke, HueSaturation, context),
            "Invert Pixels / Mask" => KeyBinding::new(keystroke, InvertPixels, context),
            "Canvas Size" => KeyBinding::new(keystroke, CanvasSize, context),
            "Image Size" => KeyBinding::new(keystroke, ImageSize, context),
            "Transform Layer / Selection" => {
                KeyBinding::new(keystroke, TransformLayerOrSelection, context)
            }
            "Duplicate / Layer via Copy" => KeyBinding::new(keystroke, LayerViaCopy, context),
            "Toggle Clipping Mask" => KeyBinding::new(keystroke, ToggleClippingMask, context),
            "Group Layers" => KeyBinding::new(keystroke, GroupSelectedLayers, context),
            "Ungroup Layers" => KeyBinding::new(keystroke, UngroupLayers, context),
            "New Blank Layer" => KeyBinding::new(keystroke, NewBlankLayer, context),
            "Move Layer Up" => KeyBinding::new(keystroke, MoveLayerUp, context),
            "Move Layer Down" => KeyBinding::new(keystroke, MoveLayerDown, context),
            "Merge Layers" => KeyBinding::new(keystroke, MergeLayers, context),
            "Show Grid" => KeyBinding::new(keystroke, ToggleGrid, context),
            "Show Guides" => KeyBinding::new(keystroke, ToggleGuides, context),
            "Show Rulers" => KeyBinding::new(keystroke, ToggleRulers, context),
            "Snap" => KeyBinding::new(keystroke, ToggleSnapping, context),
            "Lock Guides" => KeyBinding::new(keystroke, ToggleLockGuides, context),
            // Tools.
            "Select tool" => KeyBinding::new(keystroke, SelectTool, context),
            "Move / Transform tool" => KeyBinding::new(keystroke, MoveTool, context),
            "Hand tool" => KeyBinding::new(keystroke, HandTool, context),
            "Zoom tool" => KeyBinding::new(keystroke, ZoomTool, context),
            "Brush tool" => KeyBinding::new(keystroke, BrushTool, context),
            "Eraser" => KeyBinding::new(keystroke, EraserTool, context),
            "Spot Healing" => KeyBinding::new(keystroke, SpotHealingTool, context),
            "Clone Stamp" => KeyBinding::new(keystroke, CloneStampTool, context),
            "Type tool" => KeyBinding::new(keystroke, TypeTool, context),
            "Gradient tool" => KeyBinding::new(keystroke, GradientTool, context),
            "Shape tool" => KeyBinding::new(keystroke, ShapeTool, context),
            "Eyedropper tool" => KeyBinding::new(keystroke, EyedropperTool, context),
            "Marquee / cycle shape" => KeyBinding::new(keystroke, MarqueeTool, context),
            "Magic" => KeyBinding::new(keystroke, MagicWandTool, context),
            "Lasso / cycle mode" => KeyBinding::new(keystroke, LassoTool, context),
            "Blur / Smudge / Liquify" => KeyBinding::new(keystroke, BlurTool, context),
            "Crop tool" => KeyBinding::new(keystroke, CropTool, context),
            "Swap foreground/background" => KeyBinding::new(keystroke, SwapPaletteColors, context),
            "Reset colors" => KeyBinding::new(keystroke, ResetPaletteColors, context),
            "Cycle tool mode" => KeyBinding::new(keystroke, CycleToolMode, context),
            "Temporary Hand tool (hold)" => KeyBinding::new(keystroke, TemporaryHandTool, context),
            "Delete selection / layer / effect / lasso point" => {
                KeyBinding::new(keystroke, DeleteKeyPressed, context)
            }
            "Apply current canvas operation" => {
                KeyBinding::new(keystroke, ApplyCanvasOperation, context)
            }
            "Cancel current canvas operation" => {
                KeyBinding::new(keystroke, CancelCanvasOperation, context)
            }
            "Decrease brush size" => KeyBinding::new(keystroke, DecreaseBrushSize, context),
            "Increase brush size" => KeyBinding::new(keystroke, IncreaseBrushSize, context),
            "Decrease brush hardness" => KeyBinding::new(keystroke, DecreaseBrushHardness, context),
            "Increase brush hardness" => KeyBinding::new(keystroke, IncreaseBrushHardness, context),
            "Previous blend mode" => KeyBinding::new(keystroke, PreviousBlendMode, context),
            "Next blend mode" => KeyBinding::new(keystroke, NextBlendMode, context),
            "Cycle shape kind" => KeyBinding::new(keystroke, CycleShapeKind, context),
            "Opacity digit 0 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 0 }, context)
            }
            "Opacity digit 1 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 1 }, context)
            }
            "Opacity digit 2 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 2 }, context)
            }
            "Opacity digit 3 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 3 }, context)
            }
            "Opacity digit 4 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 4 }, context)
            }
            "Opacity digit 5 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 5 }, context)
            }
            "Opacity digit 6 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 6 }, context)
            }
            "Opacity digit 7 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 7 }, context)
            }
            "Opacity digit 8 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 8 }, context)
            }
            "Opacity digit 9 (type two for exact %)" => {
                KeyBinding::new(keystroke, TypeOpacityDigit { digit: 9 }, context)
            }
            "Nudge Left 1 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Left,
                    distance: 1,
                },
                context,
            ),
            "Nudge Left 10 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Left,
                    distance: 10,
                },
                context,
            ),
            "Move selected pixels Left 1 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Left,
                    distance: 1,
                },
                context,
            ),
            "Move selected pixels Left 10 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Left,
                    distance: 10,
                },
                context,
            ),
            "Nudge Right 1 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Right,
                    distance: 1,
                },
                context,
            ),
            "Nudge Right 10 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Right,
                    distance: 10,
                },
                context,
            ),
            "Move selected pixels Right 1 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Right,
                    distance: 1,
                },
                context,
            ),
            "Move selected pixels Right 10 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Right,
                    distance: 10,
                },
                context,
            ),
            "Nudge Up 1 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Up,
                    distance: 1,
                },
                context,
            ),
            "Nudge Up 10 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Up,
                    distance: 10,
                },
                context,
            ),
            "Move selected pixels Up 1 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Up,
                    distance: 1,
                },
                context,
            ),
            "Move selected pixels Up 10 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Up,
                    distance: 10,
                },
                context,
            ),
            "Nudge Down 1 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Down,
                    distance: 1,
                },
                context,
            ),
            "Nudge Down 10 px" => KeyBinding::new(
                keystroke,
                Nudge {
                    direction: ArrowDirection::Down,
                    distance: 10,
                },
                context,
            ),
            "Move selected pixels Down 1 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Down,
                    distance: 1,
                },
                context,
            ),
            "Move selected pixels Down 10 px" => KeyBinding::new(
                keystroke,
                MoveSelectedPixels {
                    direction: ArrowDirection::Down,
                    distance: 10,
                },
                context,
            ),
            "Toggle Levels preview" => KeyBinding::new(keystroke, ToggleLevelsPreview, context),
            // Text editing.
            "Finish editing text" => KeyBinding::new(keystroke, FinishEditingText, context),
            "Decrease tracking" => {
                KeyBinding::new(keystroke, AdjustTextTracking { step: -1.0 }, context)
            }
            "Increase tracking" => {
                KeyBinding::new(keystroke, AdjustTextTracking { step: 1.0 }, context)
            }
            "Decrease tracking by 10" => {
                KeyBinding::new(keystroke, AdjustTextTracking { step: -10.0 }, context)
            }
            "Increase tracking by 10" => {
                KeyBinding::new(keystroke, AdjustTextTracking { step: 10.0 }, context)
            }
            "Decrease leading" => {
                KeyBinding::new(keystroke, AdjustTextLeading { step: -1.0 }, context)
            }
            "Increase leading" => {
                KeyBinding::new(keystroke, AdjustTextLeading { step: 1.0 }, context)
            }
            "Decrease leading by 10" => {
                KeyBinding::new(keystroke, AdjustTextLeading { step: -10.0 }, context)
            }
            "Increase leading by 10" => {
                KeyBinding::new(keystroke, AdjustTextLeading { step: 10.0 }, context)
            }
            title => unreachable!("the shortcut table has no compositor action for {title:?}"),
        }
    }
}

/// The `keyboardShortcuts.v1` overrides over the table's originals (`ShortcutSettings.shared`).
#[derive(Clone, Debug, Default)]
pub struct ShortcutSettings {
    overrides: HashMap<String, ShortcutChord>,
}

impl ShortcutSettings {
    /// Where the overrides are stored, as `UserDefaults` stored them.
    pub const STORAGE_KEY: &'static str = "keyboardShortcuts.v1";

    /// `ShortcutSettings.shared`: the one table the app registers bindings from and the sheet saves into.
    pub fn shared() -> &'static Mutex<ShortcutSettings> {
        static SHARED: LazyLock<Mutex<ShortcutSettings>> =
            LazyLock::new(|| Mutex::new(ShortcutSettings::load()));
        &SHARED
    }

    /// `init()`: reads the stored overrides, keeping them only when `problem(in:)` accepts them.
    ///
    /// A stored map that does not decode is thrown away whole, the way `JSONDecoder`'s `try?` threw it
    /// away.
    pub fn load() -> Self {
        if let Some(stored) = settings::json_value(Self::STORAGE_KEY) {
            if let Ok(overrides) = serde_json::from_value::<HashMap<String, ShortcutChord>>(stored) {
                if Self::problem(&overrides).is_none() {
                    return Self { overrides };
                }
            }
        }
        Self::default()
    }

    /// The person's overrides, as the Keyboard Shortcuts sheet drafts them.
    pub fn overrides(&self) -> &HashMap<String, ShortcutChord> {
        &self.overrides
    }

    /// `chord(_:)`: the chord in force, which is the override when there is one.
    pub fn chord(&self, definition: &ShortcutDefinition) -> ShortcutChord {
        self.overrides
            .get(&definition.id())
            .cloned()
            .unwrap_or_else(|| definition.original.clone())
    }

    /// `menu(_:modifiers:)`: the chord a menu item shows, looked up by the chord it started with.
    pub fn menu(&self, key: char, modifiers: Modifiers) -> ShortcutChord {
        let original = ShortcutChord::new(key.to_string(), modifier_bits(&modifiers));
        match ShortcutDefinition::all()
            .iter()
            .find(|definition| definition.is_menu() && definition.original == original)
        {
            Some(definition) => self.chord(definition),
            None => original,
        }
    }

    /// `native(_:modifiers:)`: the same, for the keys that are not menu items.
    pub fn native(&self, key: char, modifiers: Modifiers) -> ShortcutChord {
        let original = ShortcutChord::new(key.to_string(), modifier_bits(&modifiers));
        match ShortcutDefinition::all()
            .iter()
            .find(|definition| !definition.is_menu() && definition.original == original)
        {
            Some(definition) => self.chord(definition),
            None => original,
        }
    }

    /// `save(_:)`: stores the map when it passes `problem(in:)`. The sheet closes itself where the Swift
    /// method closed the panel; the bool is the guard that stood in front of both.
    pub fn save(&mut self, values: HashMap<String, ShortcutChord>) -> bool {
        if Self::problem(&values).is_some() {
            return false;
        }
        let Ok(stored) = serde_json::to_value(&values) else {
            return false;
        };
        self.overrides = values;
        settings::set_json(stored, Self::STORAGE_KEY);
        true
    }

    /// `problem(in:)`: the first thing wrong with a draft map, in the order the Swift checks ran.
    pub fn problem(values: &HashMap<String, ShortcutChord>) -> Option<String> {
        let mut assigned: HashMap<ShortcutChord, String> = HashMap::new();
        for definition in ShortcutDefinition::all() {
            let chord = values
                .get(&definition.id())
                .cloned()
                .unwrap_or_else(|| definition.original.clone());
            if chord.key.chars().count() != 1 || !(0..=15).contains(&chord.modifiers) {
                return Some("Choose a single key with optional modifiers.".to_string());
            }
            if definition.group == TEXT_EDITING_GROUP && chord.modifiers & 7 == 0 {
                return Some(
                    "Text-editing shortcuts need Command, Option, or Control so they do not replace \
                     normal typing."
                        .to_string(),
                );
            }
            if [
                ShortcutChord::new("q", COMMAND),
                ShortcutChord::new(",", COMMAND),
                ShortcutChord::new("m", COMMAND | OPTION),
            ]
            .contains(&chord)
            {
                return Some(format!("{} is reserved by macOS.", chord.label()));
            }
            if let Some(other) = assigned.get(&chord) {
                return Some(format!(
                    "{} is assigned to both {} and {}.",
                    chord.label(),
                    other,
                    definition.title
                ));
            }
            assigned.insert(chord, definition.title.clone());
        }
        None
    }

    /// Every definition's resolved chord as a gpui binding, in table order — the app hands these to
    /// `App::bind_keys` in place of `canvasEvent(_:)` and `textEvent(_:)` (see the module docs) — plus
    /// the canvas's Shift fallback: every base canvas key also binds with Shift, the way the Swift method
    /// let Shift+A still select a tool.
    pub fn bindings(&self) -> Vec<KeyBinding> {
        let mut bindings = Vec::new();
        for definition in ShortcutDefinition::all() {
            for keystroke in self.chord(definition).keystrokes() {
                bindings.push(definition.binding(&keystroke));
            }
            if let Some(shifted) = self.shift_fallback(definition) {
                for keystroke in shifted.keystrokes() {
                    bindings.push(definition.binding(&keystroke));
                }
            }
        }
        bindings
    }

    /// `canvasEvent(_:)`'s last rule as a binding: a base canvas key held with Shift runs the base
    /// assignment (letter tool keys traditionally also accept Shift), unless Shift has a command of its
    /// own for that key — then that command answers, and nothing answers if it was reassigned away, which
    /// is the `nil` the Swift method returned.
    fn shift_fallback(&self, definition: &ShortcutDefinition) -> Option<ShortcutChord> {
        if definition.is_menu() || definition.original.modifiers != 0 {
            return None;
        }
        // The base assignment has to be a key of its own: Shift only falls back for a chord without
        // modifiers in force (a reassignment to Shift+key, say, is not a base key any more).
        let chord = self.chord(definition);
        if chord.modifiers != 0 {
            return None;
        }
        // The canvas's own Tab branch wants no modifiers held, so Tab's base assignment does not answer
        // for Shift+Tab.
        if definition.original.key == "\t" {
            return None;
        }
        let shifted = ShortcutChord::new(chord.key.clone(), SHIFT);
        let all = ShortcutDefinition::all();
        // A chord of the table's own answers Shift+key first: the Shift commands (cycle shape, brush
        // hardness, the blend-mode steps, content-aware fill, the ten-pixel nudges), and whatever a
        // shortcut was remapped onto.
        if all.iter().any(|other| self.chord(other) == shifted) {
            return None;
        }
        // Shift has a command of its own for this key and it moved away: Shift+key is swallowed.
        if all.iter().any(|other| {
            other.group != TEXT_EDITING_GROUP
                && other.original == shifted
                && self.chord(other) != shifted
        }) {
            return None;
        }
        Some(shifted)
    }
}

/// The bindings the current overrides resolve to, ready for `App::bind_keys` at startup and again after
/// the Keyboard Shortcuts sheet saves.
pub fn bindings() -> Vec<KeyBinding> {
    ShortcutSettings::shared().lock().bindings()
}

/// The table, built the way the Swift literal built it — the menu entries, then the generated ones,
/// in the same order.
fn build_all() -> Vec<ShortcutDefinition> {
    fn entry(title: &str, key: &str, modifiers: i32, menu: bool) -> ShortcutDefinition {
        ShortcutDefinition::new(
            title,
            if menu { MENUS_GROUP } else { CANVAS_GROUP },
            ShortcutChord::new(key, modifiers),
        )
    }

    let mut result: Vec<ShortcutDefinition> = vec![
        entry("Undo", "z", 1, true),
        entry("Redo", "z", 9, true),
        entry("New Canvas", "n", 1, true),
        entry("Open Project", "o", 1, true),
        entry("Save", "s", 1, true),
        entry("Save As", "s", 9, true),
        entry("Export PNG", "e", 9, true),
        entry("Export JPEG", "s", 11, true),
        entry("Close Project", "w", 1, true),
        entry("Fit Canvas", "0", 1, true),
        entry("Actual Pixels", "1", 1, true),
        entry("Zoom In", "=", 1, true),
        entry("Zoom Out", "-", 1, true),
        entry("Show Transform Controls", "h", 1, true),
        entry("Hide Compositor", "h", 3, true),
        entry("Cut", "x", 1, true),
        entry("Copy", "c", 1, true),
        entry("Copy Merged", "c", 9, true),
        entry("Paste", "v", 1, true),
        entry("Fill with Foreground", "\u{7f}", 2, true),
        entry("Fill with Background", "\u{7f}", 1, true),
        entry("Content-Aware Fill", "\u{7f}", 8, true),
        entry("Select All", "a", 1, true),
        entry("Deselect", "d", 1, true),
        entry("Inverse Selection", "i", 9, true),
        entry("Select Subject", "a", 3, true),
        entry("Curves", "m", 1, true),
        entry("Levels", "l", 1, true),
        entry("Hue/Saturation", "u", 1, true),
        entry("Invert Pixels / Mask", "i", 1, true),
        entry("Canvas Size", "c", 3, true),
        entry("Image Size", "i", 3, true),
        entry("Transform Layer / Selection", "t", 1, true),
        entry("Duplicate / Layer via Copy", "j", 1, true),
        entry("Toggle Clipping Mask", "g", 3, true),
        entry("Group Layers", "g", 1, true),
        entry("Ungroup Layers", "g", 9, true),
        entry("New Blank Layer", "n", 9, true),
        entry("Move Layer Up", "]", 1, true),
        entry("Move Layer Down", "[", 1, true),
        entry("Merge Layers", "e", 1, true),
        entry("Show Grid", "'", 1, true),
        entry("Show Guides", ";", 1, true),
        entry("Show Rulers", "r", 1, true),
        entry("Snap", ";", 9, true),
        entry("Lock Guides", ";", 3, true),
    ];
    for (title, key) in [
        ("Select tool", "a"),
        ("Move / Transform tool", "v"),
        ("Hand tool", "h"),
        ("Zoom tool", "z"),
        ("Brush tool", "b"),
        ("Eraser", "e"),
        ("Spot Healing", "j"),
        ("Clone Stamp", "s"),
        ("Type tool", "t"),
        ("Gradient tool", "g"),
        ("Shape tool", "u"),
        ("Eyedropper tool", "i"),
        ("Marquee / cycle shape", "m"),
        ("Magic", "w"),
        ("Lasso / cycle mode", "l"),
        ("Blur / Smudge / Liquify", "r"),
        ("Crop tool", "c"),
        ("Swap foreground/background", "x"),
        ("Reset colors", "d"),
        ("Cycle tool mode", "\t"),
        ("Temporary Hand tool (hold)", " "),
        ("Delete selection / layer / effect / lasso point", "\u{7f}"),
        ("Apply current canvas operation", "\r"),
        ("Cancel current canvas operation", "\u{1b}"),
        ("Decrease brush size", "["),
        ("Increase brush size", "]"),
    ] {
        result.push(entry(title, key, 0, false));
    }
    result.extend([
        entry("Decrease brush hardness", "[", 8, false),
        entry("Increase brush hardness", "]", 8, false),
        entry("Previous blend mode", "-", 8, false),
        entry("Next blend mode", "=", 8, false),
        entry("Cycle shape kind", "u", 8, false),
    ]);
    for digit in 0..=9 {
        result.push(entry(
            &format!("Opacity digit {digit} (type two for exact %)"),
            &digit.to_string(),
            0,
            false,
        ));
    }
    for (direction, key) in [
        ("Left", "\u{f702}"),
        ("Right", "\u{f703}"),
        ("Up", "\u{f700}"),
        ("Down", "\u{f701}"),
    ] {
        result.extend([
            entry(&format!("Nudge {direction} 1 px"), key, 0, false),
            entry(&format!("Nudge {direction} 10 px"), key, 8, false),
            entry(&format!("Move selected pixels {direction} 1 px"), key, 1, false),
            entry(&format!("Move selected pixels {direction} 10 px"), key, 9, false),
        ]);
    }
    result.push(ShortcutDefinition::new(
        "Finish editing text",
        TEXT_EDITING_GROUP,
        ShortcutChord::new("\r", 1),
    ));
    for (title, key) in [
        ("Decrease tracking", "\u{f702}"),
        ("Increase tracking", "\u{f703}"),
        ("Decrease leading", "\u{f700}"),
        ("Increase leading", "\u{f701}"),
    ] {
        result.push(ShortcutDefinition::new(
            title,
            TEXT_EDITING_GROUP,
            ShortcutChord::new(key, 2),
        ));
        result.push(ShortcutDefinition::new(
            &format!("{title} by 10"),
            TEXT_EDITING_GROUP,
            ShortcutChord::new(key, 10),
        ));
    }
    result.push(entry("Toggle Levels preview", "p", 2, false));
    result
}

/// `label`'s names for the keys that have no character of their own.
const SPECIAL_KEYS: [(&str, &str); 9] = [
    ("\u{7f}", "Delete"),
    ("\r", "Return"),
    ("\u{1b}", "Esc"),
    ("\t", "Tab"),
    (" ", "Space"),
    ("\u{f702}", "←"),
    ("\u{f703}", "→"),
    ("\u{f701}", "↓"),
    ("\u{f700}", "↑"),
];

/// The same keys under the names gpui's keystroke parser knows; `\u{7f}` is handled separately because
/// Windows has two keys where the Mac has one.
const KEY_NAMES: [(&str, &[&str]); 8] = [
    ("\r", &["enter"]),
    ("\u{1b}", &["escape"]),
    ("\t", &["tab"]),
    (" ", &["space"]),
    ("\u{f702}", &["left"]),
    ("\u{f703}", &["right"]),
    ("\u{f701}", &["down"]),
    ("\u{f700}", &["up"]),
];

/// The shifted characters, back to the keys that produce them (`init(_ event:)`'s table).
const UNSHIFTED_KEYS: [(&str, &str); 4] = [("{", "["), ("}", "]"), ("+", "="), ("_", "-")];

/// The gpui spellings Windows' keyboard layout reports for this key with Shift down: the OEM keys and
/// the top row's digits hand over the shifted character with the Shift bit already cleared, while the
/// keypad's digits keep the plain key and the bit — so those chords have both spellings. `event(like:)`
/// substituted the same characters before it built its event. Elsewhere the key and the `shift-` prefix
/// stay apart.
#[cfg(target_os = "windows")]
fn shifted_spellings(key: &str) -> Vec<(String, bool)> {
    let shifted = match key {
        "[" => "{",
        "]" => "}",
        "=" => "+",
        "-" => "_",
        "0" => ")",
        "1" => "!",
        "2" => "@",
        "3" => "#",
        "4" => "$",
        "5" => "%",
        "6" => "^",
        "7" => "&",
        "8" => "*",
        "9" => "(",
        _ => return Vec::new(),
    };
    let mut spellings = vec![(shifted.to_string(), false)];
    if key.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        spellings.push((key.to_string(), true));
    }
    spellings
}

#[cfg(not(target_os = "windows"))]
fn shifted_spellings(_key: &str) -> Vec<(String, bool)> {
    Vec::new()
}

/// The stored bits as gpui's modifier state.
fn modifier_flags(bits: i32) -> Modifiers {
    Modifiers {
        control: bits & CONTROL != 0,
        alt: bits & OPTION != 0,
        shift: bits & SHIFT != 0,
        platform: bits & COMMAND != 0,
        function: false,
    }
}

/// gpui's modifier state as the stored bits.
fn modifier_bits(modifiers: &Modifiers) -> i32 {
    (if modifiers.platform { COMMAND } else { 0 })
        | (if modifiers.alt { OPTION } else { 0 })
        | (if modifiers.control { CONTROL } else { 0 })
        | (if modifiers.shift { SHIFT } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the Swift table: 113 entries, in its order, chords included.
    #[test]
    fn the_table_is_the_swifts_table() {
        let all = ShortcutDefinition::all();
        assert_eq!(all.len(), 113);
        assert_eq!(
            all[0],
            ShortcutDefinition::new("Undo", MENUS_GROUP, ShortcutChord::new("z", COMMAND))
        );
        assert_eq!(all[45].title, "Lock Guides");
        assert_eq!(all[45].original, ShortcutChord::new(";", COMMAND | OPTION));
        assert_eq!(
            all[46],
            ShortcutDefinition::new("Select tool", CANVAS_GROUP, ShortcutChord::new("a", 0))
        );
        assert_eq!(
            all[63],
            ShortcutDefinition::new("Swap foreground/background", CANVAS_GROUP, ShortcutChord::new("x", 0))
        );
        assert_eq!(
            all[64],
            ShortcutDefinition::new("Reset colors", CANVAS_GROUP, ShortcutChord::new("d", 0))
        );
        assert_eq!(
            all[71],
            ShortcutDefinition::new("Increase brush size", CANVAS_GROUP, ShortcutChord::new("]", 0))
        );
        assert_eq!(
            all[72],
            ShortcutDefinition::new("Decrease brush hardness", CANVAS_GROUP, ShortcutChord::new("[", SHIFT))
        );
        assert_eq!(
            all[78],
            ShortcutDefinition::new(
                "Opacity digit 1 (type two for exact %)",
                CANVAS_GROUP,
                ShortcutChord::new("1", 0)
            )
        );
        assert_eq!(
            all[87],
            ShortcutDefinition::new("Nudge Left 1 px", CANVAS_GROUP, ShortcutChord::new("\u{f702}", 0))
        );
        assert_eq!(
            all[88],
            ShortcutDefinition::new("Nudge Left 10 px", CANVAS_GROUP, ShortcutChord::new("\u{f702}", SHIFT))
        );
        assert_eq!(
            all[89],
            ShortcutDefinition::new(
                "Move selected pixels Left 1 px",
                CANVAS_GROUP,
                ShortcutChord::new("\u{f702}", COMMAND)
            )
        );
        assert_eq!(
            all[90],
            ShortcutDefinition::new(
                "Move selected pixels Left 10 px",
                CANVAS_GROUP,
                ShortcutChord::new("\u{f702}", COMMAND | SHIFT)
            )
        );
        assert_eq!(
            all[103],
            ShortcutDefinition::new(
                "Finish editing text",
                TEXT_EDITING_GROUP,
                ShortcutChord::new("\r", COMMAND)
            )
        );
        assert_eq!(
            all[112],
            ShortcutDefinition::new(
                "Toggle Levels preview",
                CANVAS_GROUP,
                ShortcutChord::new("p", OPTION)
            )
        );
        assert_eq!(all.last().unwrap(), &all[112]);
    }

    /// The generated halves are the size the Swift loops made them.
    #[test]
    fn the_generated_groups_are_the_swifts_sizes() {
        let all = ShortcutDefinition::all();
        assert_eq!(all.iter().filter(|entry| entry.is_menu()).count(), 46);
        assert_eq!(
            all.iter().filter(|entry| entry.group == TEXT_EDITING_GROUP).count(),
            9
        );
        assert_eq!(
            all.iter()
                .filter(|entry| entry.title.starts_with("Opacity digit "))
                .count(),
            10
        );
        assert_eq!(
            all.iter()
                .filter(|entry| entry.title.starts_with("Nudge "))
                .count(),
            8
        );
        assert_eq!(
            all.iter()
                .filter(|entry| entry.title.starts_with("Move selected pixels "))
                .count(),
            8
        );
        assert_eq!(GROUPS, [MENUS_GROUP, CANVAS_GROUP, TEXT_EDITING_GROUP]);
        assert_eq!(all[0].id(), "Menus:Undo");
        assert!(all[0].is_menu());
        assert!(!all[46].is_menu());
    }

    /// The stored bits are gpui's modifier state, Command being the platform modifier.
    #[test]
    fn the_stored_bits_map_to_the_gpui_modifiers() {
        let chord = ShortcutChord::new("z", COMMAND | OPTION | CONTROL | SHIFT);
        let flags = chord.event_modifiers();
        assert!(flags.platform);
        assert!(flags.alt);
        assert!(flags.control);
        assert!(flags.shift);
        assert!(!flags.function);
        assert_eq!(modifier_bits(&flags), COMMAND | OPTION | CONTROL | SHIFT);
        assert_eq!(chord.event_modifiers(), chord.event_modifiers());
        assert_eq!(ShortcutChord::new("z", 0).event_modifiers(), Modifiers::none());
    }

    /// `label` spells the macOS glyphs out, in the glyphs' order.
    #[test]
    fn labels_spell_out_the_modifiers() {
        assert_eq!(ShortcutChord::new("z", COMMAND).label(), "Cmd+Z");
        assert_eq!(
            ShortcutChord::new("z", COMMAND | SHIFT).label(),
            "Shift+Cmd+Z"
        );
        assert_eq!(ShortcutChord::new("\u{7f}", OPTION).label(), "Alt+Delete");
        assert_eq!(ShortcutChord::new(",", COMMAND).label(), "Cmd+,");
        assert_eq!(ShortcutChord::new("m", COMMAND | OPTION).label(), "Alt+Cmd+M");
        assert_eq!(ShortcutChord::new("\r", COMMAND).label(), "Cmd+Return");
        assert_eq!(ShortcutChord::new("\u{1b}", 0).label(), "Esc");
        assert_eq!(ShortcutChord::new("\t", 0).label(), "Tab");
        assert_eq!(ShortcutChord::new(" ", 0).label(), "Space");
        assert_eq!(ShortcutChord::new("\u{f702}", 0).label(), "←");
        assert_eq!(ShortcutChord::new("\u{f703}", 0).label(), "→");
        assert_eq!(ShortcutChord::new("\u{f701}", 0).label(), "↓");
        assert_eq!(ShortcutChord::new("\u{f700}", 0).label(), "↑");
    }

    /// A chord is bound under the keystroke gpui's parser takes: `secondary` for the Command bit, the
    /// platform's key names for the keys that have no character, and the shifted character where the
    /// platform hands it over shifted.
    #[test]
    fn chords_bind_to_the_platforms_keystroke_spelling() {
        assert_eq!(ShortcutChord::new("z", COMMAND).keystrokes(), ["secondary-z"]);
        assert_eq!(
            ShortcutChord::new("z", COMMAND | SHIFT).keystrokes(),
            ["secondary-shift-z"]
        );
        assert_eq!(
            ShortcutChord::new("\u{7f}", COMMAND).keystrokes(),
            ["secondary-delete", "secondary-backspace"]
        );
        assert_eq!(ShortcutChord::new("\r", COMMAND).keystrokes(), ["secondary-enter"]);
        assert_eq!(ShortcutChord::new("\u{1b}", 0).keystrokes(), ["escape"]);
        assert_eq!(ShortcutChord::new("\t", 0).keystrokes(), ["tab"]);
        assert_eq!(ShortcutChord::new(" ", 0).keystrokes(), ["space"]);
        assert_eq!(ShortcutChord::new("\u{f700}", OPTION).keystrokes(), ["alt-up"]);
        assert_eq!(ShortcutChord::new("u", SHIFT).keystrokes(), ["shift-u"]);
        assert_eq!(
            ShortcutChord::new("u", COMMAND | OPTION | SHIFT).keystrokes(),
            ["alt-secondary-shift-u"]
        );
        #[cfg(target_os = "windows")]
        {
            assert_eq!(ShortcutChord::new("[", SHIFT).keystrokes(), ["{"]);
            assert_eq!(ShortcutChord::new("]", SHIFT).keystrokes(), ["}"]);
            assert_eq!(ShortcutChord::new("-", SHIFT).keystrokes(), ["_"]);
            assert_eq!(ShortcutChord::new("=", SHIFT).keystrokes(), ["+"]);
            // The keypad's digits keep the Shift bit, so a digit binds under both spellings.
            assert_eq!(
                ShortcutChord::new("3", SHIFT).keystrokes(),
                ["#", "shift-3"]
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(ShortcutChord::new("[", SHIFT).keystrokes(), ["shift-["]);
            assert_eq!(ShortcutChord::new("]", SHIFT).keystrokes(), ["shift-]"]);
            assert_eq!(ShortcutChord::new("-", SHIFT).keystrokes(), ["shift--"]);
            assert_eq!(ShortcutChord::new("=", SHIFT).keystrokes(), ["shift-="]);
            assert_eq!(ShortcutChord::new("3", SHIFT).keystrokes(), ["shift-3"]);
        }
    }

    /// The recorded keystroke is the chord `init(_ event:)` would have read off the AppKit event.
    #[test]
    fn keystrokes_read_back_as_the_chords_they_came_from() {
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("delete").unwrap()),
            ShortcutChord::new("\u{7f}", 0)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("backspace").unwrap()),
            ShortcutChord::new("\u{7f}", 0)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("ctrl-enter").unwrap()),
            ShortcutChord::new("\r", CONTROL)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("ctrl-alt-shift-up").unwrap()),
            ShortcutChord::new("\u{f700}", CONTROL | OPTION | SHIFT)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("S").unwrap()),
            ShortcutChord::new("s", SHIFT)
        );
        // The shifted characters come back as the keys that make them, Shift included.
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("{").unwrap()),
            ShortcutChord::new("[", SHIFT)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("+").unwrap()),
            ShortcutChord::new("=", SHIFT)
        );
        assert_eq!(
            ShortcutChord::from_keystroke(&Keystroke::parse("ctrl-z").unwrap()),
            ShortcutChord::new("z", CONTROL)
        );
    }

    /// The table as it stands is a map with no problem in it.
    #[test]
    fn a_valid_map_has_no_problem() {
        assert_eq!(ShortcutSettings::problem(&HashMap::new()), None);
        let values = HashMap::from([("Menus:Undo".to_string(), ShortcutChord::new("y", COMMAND))]);
        assert_eq!(ShortcutSettings::problem(&values), None);
    }

    /// One key, four bit positions.
    #[test]
    fn a_chord_must_be_one_key_with_at_most_four_modifiers() {
        let message = Some("Choose a single key with optional modifiers.".to_string());
        let multi_key = HashMap::from([("Menus:Undo".to_string(), ShortcutChord::new("zz", COMMAND))]);
        assert_eq!(ShortcutSettings::problem(&multi_key), message);
        let no_key = HashMap::from([("Menus:Undo".to_string(), ShortcutChord::new("", COMMAND))]);
        assert_eq!(ShortcutSettings::problem(&no_key), message);
        let too_many_bits = HashMap::from([("Menus:Undo".to_string(), ShortcutChord::new("z", 16))]);
        assert_eq!(ShortcutSettings::problem(&too_many_bits), message);
    }

    /// Two commands cannot share a chord.
    #[test]
    fn a_duplicate_chord_is_a_problem() {
        let values = HashMap::from([
            ("Menus:Undo".to_string(), ShortcutChord::new("y", COMMAND)),
            ("Menus:Redo".to_string(), ShortcutChord::new("y", COMMAND)),
        ]);
        assert_eq!(
            ShortcutSettings::problem(&values),
            Some("Cmd+Y is assigned to both Undo and Redo.".to_string())
        );
    }

    /// Text editing keeps its modifiers, so normal typing stays normal.
    #[test]
    fn a_text_editing_chord_needs_command_option_or_control() {
        let values = HashMap::from([(
            "Text Editing:Finish editing text".to_string(),
            ShortcutChord::new("\r", SHIFT),
        )]);
        assert_eq!(
            ShortcutSettings::problem(&values),
            Some(
                "Text-editing shortcuts need Command, Option, or Control so they do not replace \
                 normal typing."
                    .to_string()
            )
        );
    }

    /// The system's three chords stay the system's.
    #[test]
    fn a_reserved_chord_is_a_problem() {
        for chord in [
            ShortcutChord::new("q", COMMAND),
            ShortcutChord::new(",", COMMAND),
            ShortcutChord::new("m", COMMAND | OPTION),
        ] {
            let values = HashMap::from([("Menus:Undo".to_string(), chord.clone())]);
            assert_eq!(
                ShortcutSettings::problem(&values),
                Some(format!("{} is reserved by macOS.", chord.label()))
            );
        }
    }

    /// Every definition has its action, so the whole table turns into bindings.
    #[test]
    fn every_definition_binds_to_a_compositor_action() {
        let bindings = ShortcutSettings::default().bindings();
        // 113 definitions, four of those keys spelled twice on Windows, and 41 Shift fallbacks (31
        // base-key definitions, the ten digits binding under both spellings): 117 + 41.
        assert_eq!(bindings.len(), 158);
        assert_eq!(bindings[0].action().name(), "compositor::Undo");
        assert_eq!(bindings[0].keystrokes()[0].key(), "z");
        assert_eq!(bindings[0].predicate(), None);
        let select_tool = bindings
            .iter()
            .find(|binding| binding.action().name() == "compositor::SelectTool")
            .expect("the Select tool key is bound");
        assert_eq!(select_tool.keystrokes()[0].key(), "a");
        assert!(select_tool.predicate().is_some());
    }

    /// Shift with a base canvas key runs the base assignment, as `canvasEvent(_:)` let it — but a Shift
    /// chord of the table's own answers first, and a reassigned base key no longer falls back.
    #[test]
    fn shift_falls_back_to_the_base_canvas_key() {
        let bound = |settings: &ShortcutSettings, name: &str| -> Vec<String> {
            settings
                .bindings()
                .into_iter()
                .filter(|binding| binding.action().name() == name)
                .map(|binding| binding.keystrokes()[0].unparse())
                .collect()
        };
        let settings = ShortcutSettings::default();
        assert_eq!(bound(&settings, "compositor::SelectTool"), ["a", "shift-a"]);
        assert_eq!(bound(&settings, "compositor::ShapeTool"), ["u"]);
        // Shift has its own commands for these keys, so the base key does not answer for them.
        assert_eq!(bound(&settings, "compositor::CycleShapeKind"), ["shift-u"]);
        assert_eq!(bound(&settings, "compositor::DecreaseBrushSize"), ["["]);
        assert_eq!(
            bound(&settings, "compositor::DecreaseBrushHardness"),
            [ShortcutChord::new("[", SHIFT).keystrokes()[0].clone()]
        );
        // Tab's own branch takes no modifiers, so Shift+Tab is not the canvas's.
        assert_eq!(bound(&settings, "compositor::CycleToolMode"), ["tab"]);
        assert!(!settings
            .bindings()
            .iter()
            .any(|binding| binding.keystrokes()[0].unparse() == "shift-tab"));
        // The digit keys fall back too, under every spelling the platform gives the shifted key.
        let digits = bound(&settings, "compositor::TypeOpacityDigit");
        for keystroke in ShortcutChord::new("3", SHIFT).keystrokes() {
            assert!(digits.contains(&keystroke), "{keystroke} types the digit");
        }

        // Moving the base key moves the fallback with it.
        let mut moved = ShortcutSettings::default();
        let select_tool =
            ShortcutDefinition::new("Select tool", CANVAS_GROUP, ShortcutChord::new("a", 0));
        assert!(moved.save(HashMap::from([(
            select_tool.id(),
            ShortcutChord::new("y", 0)
        )])));
        assert_eq!(bound(&moved, "compositor::SelectTool"), ["y", "shift-y"]);
    }

    /// The canvas and the text editor have their own bindings, the menus are app-wide.
    #[test]
    fn the_groups_bind_in_their_responder_boundaries() {
        assert_eq!(
            ShortcutDefinition::new("Undo", MENUS_GROUP, ShortcutChord::new("z", COMMAND)).context(),
            None
        );
        assert_eq!(
            ShortcutDefinition::new("Cycle tool mode", CANVAS_GROUP, ShortcutChord::new("\t", 0))
                .context(),
            Some(CANVAS_CONTEXT)
        );
        assert_eq!(
            ShortcutDefinition::new(
                "Decrease tracking",
                TEXT_EDITING_GROUP,
                ShortcutChord::new("\u{f702}", OPTION)
            )
            .context(),
            Some(TEXT_EDITING_CONTEXT)
        );
    }

    /// `menu` and `native` answer with the chord in force; a reassignment moves both.
    #[test]
    fn menu_and_native_look_the_original_chord_up() {
        let mut settings = ShortcutSettings::default();
        assert_eq!(
            settings.menu('z', Modifiers::command()),
            ShortcutChord::new("z", COMMAND)
        );
        assert_eq!(
            settings.native('\t', Modifiers::none()),
            ShortcutChord::new("\t", 0)
        );
        assert_eq!(
            settings.menu('p', Modifiers::command()),
            ShortcutChord::new("p", COMMAND)
        );
        assert_eq!(
            settings.native('z', Modifiers::command()),
            ShortcutChord::new("z", COMMAND)
        );
        assert!(settings.save(HashMap::from([(
            "Menus:Undo".to_string(),
            ShortcutChord::new("y", COMMAND)
        )])));
        assert_eq!(
            settings.menu('z', Modifiers::command()),
            ShortcutChord::new("y", COMMAND)
        );
        assert_eq!(
            settings.native('z', Modifiers::command()),
            ShortcutChord::new("z", COMMAND)
        );
        assert_eq!(
            settings.chord(&ShortcutDefinition::new(
                "Undo",
                MENUS_GROUP,
                ShortcutChord::new("z", COMMAND)
            )),
            ShortcutChord::new("y", COMMAND)
        );
    }

    /// A map that fails `problem(in:)` is not stored.
    #[test]
    fn an_unacceptable_map_is_not_saved() {
        let mut settings = ShortcutSettings::default();
        assert!(!settings.save(HashMap::from([(
            "Menus:Undo".to_string(),
            ShortcutChord::new("q", COMMAND)
        )])));
        assert!(settings.overrides().is_empty());
    }

    /// The override map survives the settings store, which is where the next launch reads it; while tests
    /// are running the store keeps its hands off it instead.
    #[test]
    fn override_map_round_trips_through_the_settings_store() {
        // `set_testing` is process-wide, so this test does not share it with another in this module.
        static STORE: Mutex<()> = Mutex::new(());
        let _guard = STORE.lock();

        settings::set_testing(true);
        assert_eq!(settings::json_value(ShortcutSettings::STORAGE_KEY), None);
        let mut inert = ShortcutSettings::default();
        assert!(inert.save(values()));
        assert!(ShortcutSettings::load().overrides().is_empty());

        // The store is the real one while `is_testing` is off, so the round trip runs in a scratch config
        // directory and puts the person's own directory back afterwards.
        let scratch = std::env::temp_dir().join("compositor-shortcuts-settings-test");
        let saved_directory = std::env::var_os("COMPOSITOR_CONFIG_DIR");
        std::env::set_var("COMPOSITOR_CONFIG_DIR", &scratch);
        settings::set_testing(false);

        let mut settings = ShortcutSettings::default();
        assert!(settings.save(values()));
        assert_eq!(settings.overrides(), &values());
        assert_eq!(ShortcutSettings::load().overrides(), &values());
        assert!(scratch.join("tool-defaults.json").exists());
        let stored = settings::json_value(ShortcutSettings::STORAGE_KEY).expect("the map is stored");
        assert_eq!(stored, serde_json::to_value(values()).unwrap());
        assert_eq!(stored["Menus:Undo"]["key"].as_str(), Some("y"));
        assert_eq!(stored["Menus:Undo"]["modifiers"].as_i64(), Some(1));

        // A stored map `problem(in:)` refuses is read as no map at all.
        let mut reserved = stored;
        reserved["Menus:Undo"]["key"] = "q".into();
        settings::set_json(reserved, ShortcutSettings::STORAGE_KEY);
        assert!(ShortcutSettings::load().overrides().is_empty());

        settings::set_testing(false);
        match saved_directory {
            Some(directory) => std::env::set_var("COMPOSITOR_CONFIG_DIR", directory),
            None => std::env::remove_var("COMPOSITOR_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The map the round trip uses.
    fn values() -> HashMap<String, ShortcutChord> {
        HashMap::from([("Menus:Undo".to_string(), ShortcutChord::new("y", COMMAND))])
    }
}
