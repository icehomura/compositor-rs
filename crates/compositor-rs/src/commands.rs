//! Every editor command the menus, the key table and the tool rail dispatch: the port of the
//! `.commands` closure bodies in `CompositorApp.swift` and of the session-facing halves of the key
//! handlers in `Rendering/EditorCanvas.swift`, `UI/NativeLayerList.swift` and
//! `Rendering/InlineTextEditor.swift`.
//!
//! [`register`] answers every `compositor_rs::*` action that is a session command, against the front
//! tab's session (`workspace.current().session`, read at dispatch time). The commands that need the
//! file layer or the floating-panel host — New/Open/Save/Export/Close, the recent menu,
//! `ImportImages`, the Keyboard Shortcuts sheet, `GridSettings`, `CanvasSize`, `ImageSize` and
//! `Trim`, Check for Updates — are the app root's (`main.rs`), which owns `ProjectController` and
//! the prompter; nothing here touches the io controller.
//!
//! # Where a key goes
//!
//! gpui has no responder chain: a keystroke is answered by the focused element's key context first.
//! gpui-base installs Ctrl+C/X/V/Z/Y/A and the word-delete keys in its `Input` context, so while a
//! text input holds focus those keys keep their text meaning — the port of the Swift branches that
//! sent `copy:`/`paste:`/`undo:` down the responder chain. A menu item dispatches its action
//! directly, though, so the commands that branched on the first responder ask
//! [`focused_text_input`] and hand the command to the field, `sendAction(_:to: nil)`'s replacement.
//!
//! Element action listeners live for a single frame, so [`register`] must be called every frame
//! from a view's `render`, chaining them onto the root element: the element installs them into the
//! frame's dispatch tree when it paints, which is where gpui allows action registration.
//!
//! # Not bound here
//!
//! `TemporaryHandTool` (space) is left unbound on purpose: it is a hold key, gpui actions are
//! key-down only, and an action handler stops propagation by default — registering one would
//! swallow the key-down the canvas's `spaceHeld` (the Swift `panPhysicalKey`) depends on, with no
//! key-up action to restore the tool. The canvas's own key listeners keep it.

use std::any::TypeId;
use std::sync::Arc;

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::image_ops::FilterKind;
use compositor_rs_core::selection::SelectionMode;
use compositor_rs_pixels::warp::BrushToolMode;
use compositor_rs_session::projects::SessionHost;
use compositor_rs_session::selection::{FillSource, SelectionAmountOperation};
use compositor_rs_session::EditorSession;
use compositor_rs_ui::actions::{
    ActualPixels, AddAdjustment, AddBlackMask, AddEffect, AddWhiteMask, AdjustTextLeading,
    AdjustTextTracking, ApplyCanvasOperation, ArrowDirection, BeginFilter, BlurTool, BrushTool,
    CancelCanvasOperation, ClearGuides, ClearSelectionPixels, CloneStampTool, ColorRange,
    ContentAwareFill, ContractSelection, Copy, CopyMerged, CropTool, Curves, Cut, CycleShapeKind,
    CycleToolMode, DecreaseBrushHardness, DecreaseBrushSize, DeleteKeyPressed, DeleteLayerOrMask,
    DeleteMask, Deselect, DuplicateLayer, EditAdjustment, EraserTool, ExpandSelection,
    EyedropperTool, FeatherSelection, FillWithBackground, FillWithForeground, FinishEditingText,
    FitCanvas, FlipCanvasHorizontal, FlipCanvasVertical, FlipLayerHorizontal, FlipLayerVertical,
    GradientTool, GroupSelectedLayers, HandTool, HideCompositor, HideOthers, HueSaturation,
    IncreaseBrushHardness, IncreaseBrushSize, InvertPixels, InvertSelection, LayerViaCopy,
    LassoTool, Levels, LoadLayerSelection, LoadMaskSelection, MagicWandTool, MarqueeTool,
    MergeLayers, MoveLayerDown, MoveLayerUp, MoveOutOfFolder, MoveSelectedPixels, MoveTool,
    NewBlankLayer, NextBlendMode, Nudge, Paste, PreviousBlendMode, Redo, RenameLayer,
    ResetPaletteColors, SelectAll, SelectSubject, SelectTool, ShapeTool, ShowAll, SpotHealingTool,
    SwapPaletteColors, ToggleClippingMask, ToggleGrid, ToggleGuides, ToggleLayerVisibility,
    ToggleLevelsPreview, ToggleLockGuides, ToggleMask, ToggleMaskLink, TogglePixelGrid,
    ToggleRulers, ToggleSnap, ToggleSnapToDocumentBounds, ToggleSnapToGrid, ToggleSnapToGuides,
    ToggleSnapToLayers, ToggleSnapping, ToggleTransformControls, TransformLayerOrSelection,
    TypeOpacityDigit, TypeTool, Undo, UngroupLayers, ZoomIn, ZoomOut, ZoomTool,
};
use compositor_rs_ui::workspace::ProjectWorkspace;
use gpui_kit::component::input as text_input;
use gpui_kit::*;

/// Registers one handler for every `compositor_rs::*` action that is a session command.
///
/// The handlers read the front tab's session when the action arrives, so a command always reaches
/// the project in front.
pub fn register(
    el: &mut impl InteractiveElement,
    cx: &mut App,
    workspace: Entity<ProjectWorkspace>,
    host: Arc<dyn SessionHost>,
) {
    // No session command needs the platform services or the app handle itself: every body below is
    // an `EditorSession` call. The file-facing commands that do need them — New, Open, Save,
    // Export, Close, the settings sheets — are the app root's.
    let _ = (&host, &cx);

    undo_redo(el, &workspace);
    app_visibility(el, &workspace);
    view_controls(el, &workspace);
    pasteboard(el, &workspace);
    select_menu(el, &workspace);
    image_menu(el, &workspace);
    layer_menu(el, &workspace);
    masks_and_effects(el, &workspace);
    tools(el, &workspace);
    canvas_keys(el, &workspace);
    editing_keys(el, &workspace);
}

/// The actions [`register`] binds — for a test that asserts the list is complete.
///
/// The same list as `register`, in the same order; keep them in step.
pub fn action_types() -> Vec<TypeId> {
    vec![
        // Undo/Redo (`CommandGroup(replacing: .undoRedo)`).
        TypeId::of::<Undo>(),
        TypeId::of::<Redo>(),
        // The app menu's visibility items.
        TypeId::of::<HideCompositor>(),
        TypeId::of::<HideOthers>(),
        TypeId::of::<ShowAll>(),
        // View menu (`CommandGroup(after: .toolbar)` and its Show/Snap To menus).
        TypeId::of::<FitCanvas>(),
        TypeId::of::<ActualPixels>(),
        TypeId::of::<ZoomIn>(),
        TypeId::of::<ZoomOut>(),
        TypeId::of::<TogglePixelGrid>(),
        TypeId::of::<ToggleSnapping>(),
        TypeId::of::<ToggleTransformControls>(),
        TypeId::of::<ToggleGrid>(),
        TypeId::of::<ToggleGuides>(),
        TypeId::of::<ToggleRulers>(),
        TypeId::of::<ToggleSnap>(),
        TypeId::of::<ToggleSnapToGuides>(),
        TypeId::of::<ToggleSnapToGrid>(),
        TypeId::of::<ToggleSnapToLayers>(),
        TypeId::of::<ToggleSnapToDocumentBounds>(),
        TypeId::of::<ToggleLockGuides>(),
        TypeId::of::<ClearGuides>(),
        // Pasteboard and the fills.
        TypeId::of::<Cut>(),
        TypeId::of::<Copy>(),
        TypeId::of::<CopyMerged>(),
        TypeId::of::<Paste>(),
        TypeId::of::<FillWithForeground>(),
        TypeId::of::<FillWithBackground>(),
        TypeId::of::<ClearSelectionPixels>(),
        TypeId::of::<ContentAwareFill>(),
        // Select menu.
        TypeId::of::<SelectAll>(),
        TypeId::of::<Deselect>(),
        TypeId::of::<InvertSelection>(),
        TypeId::of::<LoadLayerSelection>(),
        TypeId::of::<SelectSubject>(),
        TypeId::of::<ColorRange>(),
        TypeId::of::<LoadMaskSelection>(),
        TypeId::of::<ExpandSelection>(),
        TypeId::of::<ContractSelection>(),
        TypeId::of::<FeatherSelection>(),
        // Image and Filter menus.
        TypeId::of::<Curves>(),
        TypeId::of::<Levels>(),
        TypeId::of::<HueSaturation>(),
        TypeId::of::<BeginFilter>(),
        TypeId::of::<InvertPixels>(),
        TypeId::of::<FlipCanvasHorizontal>(),
        TypeId::of::<FlipCanvasVertical>(),
        // Layer menu.
        TypeId::of::<EditAdjustment>(),
        TypeId::of::<DuplicateLayer>(),
        TypeId::of::<TransformLayerOrSelection>(),
        TypeId::of::<LayerViaCopy>(),
        TypeId::of::<ToggleClippingMask>(),
        TypeId::of::<GroupSelectedLayers>(),
        TypeId::of::<UngroupLayers>(),
        TypeId::of::<MoveOutOfFolder>(),
        TypeId::of::<NewBlankLayer>(),
        TypeId::of::<RenameLayer>(),
        TypeId::of::<ToggleLayerVisibility>(),
        TypeId::of::<MoveLayerUp>(),
        TypeId::of::<MoveLayerDown>(),
        TypeId::of::<MergeLayers>(),
        TypeId::of::<FlipLayerHorizontal>(),
        TypeId::of::<FlipLayerVertical>(),
        TypeId::of::<DeleteLayerOrMask>(),
        // Layer masks and effects.
        TypeId::of::<AddWhiteMask>(),
        TypeId::of::<AddBlackMask>(),
        TypeId::of::<ToggleMask>(),
        TypeId::of::<DeleteMask>(),
        TypeId::of::<ToggleMaskLink>(),
        TypeId::of::<AddAdjustment>(),
        TypeId::of::<AddEffect>(),
        // Tools.
        TypeId::of::<SelectTool>(),
        TypeId::of::<MoveTool>(),
        TypeId::of::<HandTool>(),
        TypeId::of::<ZoomTool>(),
        TypeId::of::<BrushTool>(),
        TypeId::of::<EraserTool>(),
        TypeId::of::<SpotHealingTool>(),
        TypeId::of::<CloneStampTool>(),
        TypeId::of::<TypeTool>(),
        TypeId::of::<GradientTool>(),
        TypeId::of::<ShapeTool>(),
        TypeId::of::<EyedropperTool>(),
        TypeId::of::<MarqueeTool>(),
        TypeId::of::<MagicWandTool>(),
        TypeId::of::<LassoTool>(),
        TypeId::of::<BlurTool>(),
        TypeId::of::<CropTool>(),
        // Canvas and layer keys.
        TypeId::of::<SwapPaletteColors>(),
        TypeId::of::<ResetPaletteColors>(),
        TypeId::of::<CycleToolMode>(),
        TypeId::of::<DeleteKeyPressed>(),
        TypeId::of::<ApplyCanvasOperation>(),
        TypeId::of::<CancelCanvasOperation>(),
        TypeId::of::<DecreaseBrushSize>(),
        TypeId::of::<IncreaseBrushSize>(),
        TypeId::of::<DecreaseBrushHardness>(),
        TypeId::of::<IncreaseBrushHardness>(),
        TypeId::of::<PreviousBlendMode>(),
        TypeId::of::<NextBlendMode>(),
        TypeId::of::<CycleShapeKind>(),
        TypeId::of::<TypeOpacityDigit>(),
        TypeId::of::<Nudge>(),
        TypeId::of::<MoveSelectedPixels>(),
        // Text editing.
        TypeId::of::<ToggleLevelsPreview>(),
        TypeId::of::<FinishEditingText>(),
        TypeId::of::<AdjustTextTracking>(),
        TypeId::of::<AdjustTextLeading>(),
    ]
}

// MARK: - Binding

/// Binds one action to a handler that runs against the front tab's session.
///
/// The listener receives the action already typed. Only the bubble phase acts: the capture phase
/// is gpui's, and acting on both would run every command twice. An action handled in the bubble
/// phase stops propagation by default, which is also what keeps a focused element's raw key
/// listeners out of a bound command's way.
fn bind<A: Action + 'static>(
    el: &mut impl InteractiveElement,
    workspace: &Entity<ProjectWorkspace>,
    run: impl Fn(&A, &Entity<ProjectWorkspace>, &mut Window, &mut App) + 'static,
) {
    let workspace = workspace.clone();
    el.interactivity().on_action::<A>(move |action, window, cx| {
        run(action, &workspace, window, cx);
    });
}

/// The front tab's session: the Swift sessions' one global session, as the workspace presents it.
fn front_session(workspace: &Entity<ProjectWorkspace>, cx: &App) -> Entity<EditorSession> {
    workspace.read(cx).current().session.clone()
}

/// Runs `change` on the front tab's session and notifies its observers, as the ported views do
/// after a command (`Entity::update` does not notify on its own).
fn act<R>(
    workspace: &Entity<ProjectWorkspace>,
    cx: &mut App,
    change: impl FnOnce(&mut EditorSession) -> R,
) -> R {
    front_session(workspace, cx).update(cx, |session, cx| {
        let result = change(session);
        cx.notify();
        result
    })
}

// MARK: - Text fields

/// The focused text input, when one holds focus — the port of the Swift `firstResponder is
/// NSTextView` / `is NSText` checks that put a focused field's own editing ahead of the command.
///
/// There is no responder chain to ask, so the check is whether the focused element answers the
/// text-editing actions: every gpui-base input (`Input`, `Textarea`, the editor) registers
/// `input::Copy` on the element that owns its focus handle.
fn focused_text_input(window: &Window, cx: &App) -> Option<FocusHandle> {
    let handle = window.focused(cx)?;
    window
        .is_action_available_in(&text_input::Copy, &handle)
        .then_some(handle)
}

/// Offers `action` to the focused text input, if there is one: `sendAction(_:to: nil)`'s
/// replacement. True when the field took it and the session command must stand down.
///
/// The keyboard already resolves this the same way — the input's own key context outranks these
/// app-wide bindings — but a menu item dispatches its action directly, so the branches that had a
/// text-field path (`Cut`, `Copy`, `Paste`, the fills, `Select All`, Undo/Redo) re-offer it here.
fn forward_to_text_input(action: &dyn Action, window: &mut Window, cx: &mut App) -> bool {
    let Some(input) = focused_text_input(window, cx) else {
        return false;
    };
    input.dispatch_action(action, window, cx);
    true
}

/// The Swift guard that leaves undo/redo to a focused field while an edit is open
/// (`session.textDraft != nil || session.levels != nil || session.isProjectBusy ||
/// session.showsNewDocument || session.showsImporter || session.renamingLayerID != nil ||
/// session.transformEdit?.persistent == true`).
fn document_history_unavailable(session: &EditorSession) -> bool {
    session.text_draft.is_some()
        || session.levels.is_some()
        || session.is_project_busy
        || session.shows_new_document
        || session.shows_importer
        || session.renaming_layer_id.is_some()
        || session
            .transform_edit
            .as_ref()
            .is_some_and(|edit| edit.persistent)
}

/// An arrow key's pixel step: left and up are negative, as the Swift ternaries had them.
fn step(direction: ArrowDirection, distance: i32) -> (f64, f64) {
    let distance = f64::from(distance);
    match direction {
        ArrowDirection::Left => (-distance, 0.0),
        ArrowDirection::Right => (distance, 0.0),
        ArrowDirection::Up => (0.0, -distance),
        ArrowDirection::Down => (0.0, distance),
    }
}

// MARK: - Undo/Redo

/// `CommandGroup(replacing: .undoRedo)`: the document's history. The Swift body has two states —
/// while a dialog, the importer, a rename or a persistent transform has an edit open, history is
/// unavailable and only the focused field's own undo runs; otherwise the document's.
fn undo_redo(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<Undo>(el, workspace, |_, workspace, window, cx| {
        if document_history_unavailable(front_session(workspace, cx).read(cx)) {
            forward_to_text_input(&text_input::Undo, window, cx);
            return;
        }
        act(workspace, cx, |session| session.undo());
    });
    bind::<Redo>(el, workspace, |_, workspace, window, cx| {
        if document_history_unavailable(front_session(workspace, cx).read(cx)) {
            forward_to_text_input(&text_input::Redo, window, cx);
            return;
        }
        act(workspace, cx, |session| session.redo());
    });
}

// MARK: - App menu

/// `CommandGroup(replacing: .appVisibility)`: hide/show the application. ⌘H toggles the Move tool's
/// transform controls instead, so Hide keeps its place without the shortcut.
fn app_visibility(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<HideCompositor>(el, workspace, |_, _, _, cx| cx.hide());
    bind::<HideOthers>(el, workspace, |_, _, _, cx| cx.hide_other_apps());
    bind::<ShowAll>(el, workspace, |_, _, _, cx| cx.unhide_other_apps());
}

// MARK: - View menu

/// `CommandGroup(after: .toolbar)` and the Show/Snap To menus: zoom, the drawing toggles, snapping
/// and the guides. A dialog's preview takes the zoom commands (`route_zoom_command`).
fn view_controls(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<FitCanvas>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.fit_canvas());
    });
    bind::<ActualPixels>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.actual_pixels());
    });
    bind::<ZoomIn>(el, workspace, |_, workspace, window, cx| {
        // `guard !(NSApp.keyWindow?.firstResponder is NSText)`: a focused field keeps ⌘=.
        if focused_text_input(window, cx).is_none() {
            act(workspace, cx, |session| session.zoom_in());
        }
    });
    bind::<ZoomOut>(el, workspace, |_, workspace, window, cx| {
        if focused_text_input(window, cx).is_none() {
            act(workspace, cx, |session| session.zoom_out());
        }
    });
    bind::<TogglePixelGrid>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.shows_pixel_grid = !session.shows_pixel_grid);
    });
    bind::<ToggleSnapping>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.snapping_enabled = !session.snapping_enabled);
    });
    bind::<ToggleTransformControls>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.shows_transform_controls = !session.shows_transform_controls
        });
    });
    bind::<ToggleGrid>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.shows_grid = !session.shows_grid);
    });
    bind::<ToggleGuides>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.shows_guides = !session.shows_guides);
    });
    bind::<ToggleRulers>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.shows_rulers = !session.shows_rulers);
    });
    bind::<ToggleSnap>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.snap_enabled = !session.snap_enabled);
    });
    bind::<ToggleSnapToGuides>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.snap_to_guides = !session.snap_to_guides);
    });
    bind::<ToggleSnapToGrid>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.snap_to_grid = !session.snap_to_grid);
    });
    bind::<ToggleSnapToLayers>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.snap_to_layers = !session.snap_to_layers);
    });
    bind::<ToggleSnapToDocumentBounds>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.snap_to_document_bounds = !session.snap_to_document_bounds
        });
    });
    bind::<ToggleLockGuides>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.locks_guides = !session.locks_guides);
    });
    bind::<ClearGuides>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.clear_guides());
    });
}

// MARK: - Pasteboard

/// `CommandGroup(replacing: .pasteboard)` and `after: .pasteboard`: the canvas pixels when the
/// canvas has focus, the text field's own editing when it has it. What a command depends on (the
/// pasteboard, the copied pixels, the busy flag) is checked when it is chosen, as the Swift did;
/// a refusal was `NSSound.beep()`, which has no sound in the port.
fn pasteboard(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<Cut>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::Cut, window, cx) {
            return;
        }
        act(workspace, cx, |session| {
            if session.selection().is_some() && session.can_copy_pixels() {
                session.cut_selection();
            }
        });
    });
    bind::<Copy>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::Copy, window, cx) {
            return;
        }
        act(workspace, cx, |session| {
            if session.can_copy_pixels() || session.can_copy_layer() {
                session.copy_selection();
            }
        });
    });
    bind::<CopyMerged>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.copy_merged_selection());
    });
    bind::<Paste>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::Paste, window, cx) {
            return;
        }
        // A whole layer copied in any tab pastes as a layer before pixels are tried.
        if workspace.update(cx, |workspace, cx| workspace.paste_copied_layer(cx)) {
            front_session(workspace, cx).update(cx, |_, cx| cx.notify());
            return;
        }
        act(workspace, cx, |session| {
            if session.can_paste() {
                session.paste();
            }
        });
    });
    // Photoshop's fill shortcuts; in a text field they keep their text meaning: ⌥⌫ is
    // deleteWordBackward:, ⌘⌫ deleteToBeginningOfLine:.
    bind::<FillWithForeground>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::DeleteToPreviousWordStart, window, cx) {
            return;
        }
        act(workspace, cx, |session| session.fill_selection(FillSource::Foreground));
    });
    bind::<FillWithBackground>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::DeleteToBeginningOfLine, window, cx) {
            return;
        }
        act(workspace, cx, |session| session.fill_selection(FillSource::Background));
    });
    bind::<ClearSelectionPixels>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.clear_selected_pixels());
    });
    bind::<ContentAwareFill>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.begin_filter(FilterKind::ContentAwareFill));
    });
}

// MARK: - Select menu

/// `CommandMenu("Select")`. Select All is offered to the responder chain first — every kind of text
/// control — and selects the canvas only when nothing there wanted it.
fn select_menu(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<SelectAll>(el, workspace, |_, workspace, window, cx| {
        if forward_to_text_input(&text_input::SelectAll, window, cx) {
            return;
        }
        act(workspace, cx, |session| {
            if session.document.is_some() {
                session.select_all();
            }
        });
    });
    bind::<Deselect>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.deselect());
    });
    bind::<InvertSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.invert_selection());
    });
    bind::<LoadLayerSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(id) = session.active_layer_id {
                session.load_layer_selection(id, SelectionMode::Replace);
            }
        });
    });
    bind::<SelectSubject>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_subject(SelectionMode::Replace));
    });
    bind::<ColorRange>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.begin_color_range());
    });
    bind::<LoadMaskSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(id) = session.active_layer_id {
                session.load_mask_selection(id, SelectionMode::Replace);
            }
        });
    });
    bind::<ExpandSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.prompt_selection_amount(SelectionAmountOperation::Expand);
        });
    });
    bind::<ContractSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.prompt_selection_amount(SelectionAmountOperation::Contract);
        });
    });
    bind::<FeatherSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.prompt_selection_amount(SelectionAmountOperation::Feather);
        });
    });
}

// MARK: - Image and Filter menus

/// `CommandMenu("Image")` and `CommandMenu("Filter")`. One `BeginFilter` action carries the kind,
/// as the Swift `ForEach` loops over `FilterKind` did.
fn image_menu(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<Curves>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.begin_filter(FilterKind::Curves));
    });
    bind::<Levels>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.begin_levels());
    });
    bind::<HueSaturation>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.begin_hue_saturation());
    });
    bind::<BeginFilter>(el, workspace, |action, workspace, _, cx| {
        let kind = action.kind;
        act(workspace, cx, |session| session.begin_filter(kind));
    });
    bind::<InvertPixels>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.invert_pixels());
    });
    bind::<FlipCanvasHorizontal>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.flip_canvas(true));
    });
    bind::<FlipCanvasVertical>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.flip_canvas(false));
    });
}

// MARK: - Layer menu

/// `CommandMenu("Layer")`: adjustments, transform, grouping, ordering, merging and the flips.
fn layer_menu(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<AddAdjustment>(el, workspace, |action, workspace, _, cx| {
        let kind = action.kind;
        act(workspace, cx, |session| session.add_adjustment(kind));
    });
    bind::<EditAdjustment>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.adjustment_editing_id = session.active_layer_id;
        });
    });
    bind::<TransformLayerOrSelection>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.transform_command());
    });
    bind::<LayerViaCopy>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.layer_via_copy());
    });
    bind::<DuplicateLayer>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.duplicate_active_layer());
    });
    bind::<ToggleClippingMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(id) = session.active_layer_id {
                session.toggle_clipping_mask(id);
            }
        });
    });
    bind::<GroupSelectedLayers>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.group_selected_layers());
    });
    bind::<UngroupLayers>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.ungroup_layers());
    });
    bind::<MoveOutOfFolder>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.move_active_layer_out_of_group());
    });
    bind::<NewBlankLayer>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.add_blank_layer());
    });
    bind::<RenameLayer>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.renaming_layer_id = session.active_layer_id;
        });
    });
    bind::<ToggleLayerVisibility>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(id) = session.active_layer_id {
                session.toggle_layer_visibility(id);
            }
        });
    });
    bind::<MoveLayerUp>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.move_active_layer(1));
    });
    bind::<MoveLayerDown>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.move_active_layer(-1));
    });
    bind::<MergeLayers>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.merge_layers());
    });
    bind::<FlipLayerHorizontal>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.flip_layers(true));
    });
    bind::<FlipLayerVertical>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.flip_layers(false));
    });
    bind::<DeleteLayerOrMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.delete_layer_or_mask());
    });
}

// MARK: - Masks and effects

/// The Layers panel's Add Mask menu and context menu, and its effects menu.
fn masks_and_effects(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<AddWhiteMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            let Some(id) = session.active_layer_id else { return };
            session.select_layer_target(id, false);
            session.add_mask(true);
        });
    });
    bind::<AddBlackMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            let Some(id) = session.active_layer_id else { return };
            session.select_layer_target(id, false);
            session.add_mask(false);
        });
    });
    bind::<ToggleMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            let Some(id) = session.active_layer_id else { return };
            session.select_layer_target(id, false);
            session.toggle_layer_mask();
        });
    });
    bind::<DeleteMask>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            let Some(id) = session.active_layer_id else { return };
            session.select_layer_target(id, false);
            session.delete_layer_mask();
        });
    });
    bind::<ToggleMaskLink>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(id) = session.active_layer_id {
                session.toggle_mask_link(id);
            }
        });
    });
    bind::<AddEffect>(el, workspace, |action, workspace, _, cx| {
        let kind = action.kind;
        act(workspace, cx, |session| session.add_effect(kind));
    });
}

// MARK: - Tools

/// The tool rail and the canvas's letter keys. Brush and Eraser are the canvas's `b`/`e`: the brush
/// tool in Paint or Erase mode; Marquee, Wand and Lasso take the key that remembers their last mode.
fn tools(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<SelectTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Idle));
    });
    bind::<MoveTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Move));
    });
    bind::<HandTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Hand));
    });
    bind::<ZoomTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Zoom));
    });
    bind::<BrushTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.select_tool(NavigationTool::Brush);
            session.brush_mode = BrushToolMode::Paint;
        });
    });
    bind::<EraserTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.select_tool(NavigationTool::Brush);
            session.brush_mode = BrushToolMode::Erase;
        });
    });
    bind::<SpotHealingTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::SpotHealing));
    });
    bind::<CloneStampTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::CloneStamp));
    });
    bind::<TypeTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Type));
    });
    bind::<GradientTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Gradient));
    });
    bind::<ShapeTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Shape));
    });
    bind::<EyedropperTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Eyedropper));
    });
    bind::<MarqueeTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.press_marquee_key());
    });
    bind::<MagicWandTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.press_wand_key());
    });
    bind::<LassoTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.press_lasso_key());
    });
    bind::<BlurTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Blur));
    });
    bind::<CropTool>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.select_tool(NavigationTool::Crop));
    });
}

// MARK: - Canvas and layer keys

/// The canvas's and the layers panel's keys: `EditorCanvas.keyDown`'s session halves,
/// `NativeLayerList`'s keyDown, and the key monitor that watches them from outside the canvas.
fn canvas_keys(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    bind::<SwapPaletteColors>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.swap_palette_colors());
    });
    bind::<ResetPaletteColors>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.reset_palette_colors());
    });
    // Tab, while no text draft is open: the current tool's own mode steps on.
    bind::<CycleToolMode>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.cycle_tool_mode());
    });
    bind::<DeleteKeyPressed>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.delete_key_pressed());
    });
    // Return: commits whatever canvas operation is open — Levels, a lasso, a gradient, a crop, a
    // transform — in the Swift's order. A brush stroke swallows the key without committing.
    bind::<ApplyCanvasOperation>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.levels.is_some() {
                session.commit_levels();
            } else if session.brush_stroke.is_some() || session.warp_stroke.is_some() {
                // Nothing: the Swift returned before the rest.
            } else if session.lasso_draft.is_some() {
                session.finish_lasso();
            } else if session.gradient_edit.is_some() {
                session.commit_gradient();
            } else if session.tool == NavigationTool::Crop {
                session.commit_crop();
            } else if session.transform_edit.is_some() {
                session.commit_transform();
            }
        });
    });
    // Escape: cancels whatever is open, in the Swift's order.
    bind::<CancelCanvasOperation>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.text_draft.is_some() {
                session.cancel_text();
            } else if session.levels.is_some() {
                session.cancel_levels();
            } else if session.brush_stroke.is_some() || session.warp_stroke.is_some() {
                if !session.is_project_busy {
                    session.cancel_brush();
                }
            } else if session.lasso_draft.is_some() {
                session.cancel_lasso();
            } else if session.shape_draft.is_some() {
                session.cancel_shape();
            } else if session.gradient_edit.is_some() {
                session.cancel_gradient();
            } else if session.tool == NavigationTool::Crop {
                session.cancel_crop();
            } else if session.guide_drag.is_some() {
                session.cancel_guide_drag();
            } else if session.transform_edit.is_some() {
                session.cancel_transform();
            }
        });
    });
    // `[` / `]`: the brush's size; a Levels edit takes the keyboard instead.
    bind::<DecreaseBrushSize>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.tool.is_brush_tool() && session.levels.is_none() {
                session.change_brush_size(false);
            }
        });
    });
    bind::<IncreaseBrushSize>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.tool.is_brush_tool() && session.levels.is_none() {
                session.change_brush_size(true);
            }
        });
    });
    // Shift-[ / Shift-]: hardness in Photoshop's 25% steps.
    bind::<DecreaseBrushHardness>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.tool.is_brush_tool() && session.levels.is_none() {
                session.change_brush_hardness(false);
            }
        });
    });
    bind::<IncreaseBrushHardness>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.tool.is_brush_tool() && session.levels.is_none() {
                session.change_brush_hardness(true);
            }
        });
    });
    // Shift-− / Shift-=: the active layer's blend mode, in every tool.
    bind::<PreviousBlendMode>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.cycle_blend_mode(false));
    });
    bind::<NextBlendMode>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| session.cycle_blend_mode(true));
    });
    // Shift-U: the Shape tool's kind, when the Shape tool is already up; otherwise the Shape tool.
    bind::<CycleShapeKind>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if session.tool == NavigationTool::Shape {
                session.toggle_shape_kind();
            } else {
                session.select_tool(NavigationTool::Shape);
            }
        });
    });
    // `1`–`0`: the opacity of the brush, gradient or selected layers; two digits in quick
    // succession are an exact percent.
    bind::<TypeOpacityDigit>(el, workspace, |action, workspace, _, cx| {
        let digit = action.digit;
        act(workspace, cx, |session| session.type_opacity_digit_now(digit));
    });
    // Arrows: a selection tool with a selection nudges the selection; the Move tool (or a
    // transform being edited, from the layers panel) moves the layer.
    bind::<Nudge>(el, workspace, |action, workspace, _, cx| {
        let (dx, dy) = step(action.direction, action.distance);
        act(workspace, cx, |session| {
            if session.tool.is_selection_tool()
                && session.lasso_draft.is_none()
                && session.selection().is_some_and(|selection| !selection.is_empty())
            {
                session.nudge_selection(dx, dy);
            } else if session.transform_edit.is_some() || session.tool == NavigationTool::Move {
                session.nudge_layer(dx, dy);
            }
        });
    });
    // ⌘-arrows: moves the selected pixels, whatever tool is active.
    bind::<MoveSelectedPixels>(el, workspace, |action, workspace, _, cx| {
        let (dx, dy) = step(action.direction, action.distance);
        act(workspace, cx, |session| {
            if session.lasso_draft.is_none()
                && session.selection().is_some_and(|selection| !selection.is_empty())
            {
                session.nudge_pixels(dx, dy);
            }
        });
    });
}

// MARK: - Editing keys

/// The Levels preview key and the in-editor text keys (`InlineTextEditor.keyDown`).
fn editing_keys(el: &mut impl InteractiveElement, workspace: &Entity<ProjectWorkspace>) {
    // ⌥P while the Levels dialog is up: the preview switch flips.
    bind::<ToggleLevelsPreview>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            if let Some(edit) = session.levels.as_ref() {
                let settings = edit.settings;
                let preview = !edit.preview;
                session.update_levels(&settings, preview);
            }
        });
    });
    // ⌘↩ in the text editor: the draft becomes its layer.
    bind::<FinishEditingText>(el, workspace, |_, workspace, _, cx| {
        act(workspace, cx, |session| {
            session.finish_text();
        });
    });
    // ⌥←/→ and ⌥↑/↓, Shift by ten: the tracking and the leading, as in Photoshop.
    bind::<AdjustTextTracking>(el, workspace, |action, workspace, _, cx| {
        let step = action.step;
        act(workspace, cx, |session| {
            session.change_text_style(|style| style.tracking += step);
        });
    });
    bind::<AdjustTextLeading>(el, workspace, |action, workspace, _, cx| {
        let step = action.step;
        act(workspace, cx, |session| {
            session.change_text_style(|style| {
                // Up (a negative step) closes the lines up, counting from whatever Auto works out
                // to; down opens them out. The floor of 1 keeps the lines apart.
                style.leading = if step < 0.0 {
                    (style.line_height() + step).max(1.0)
                } else {
                    style.line_height() + step
                };
            });
        });
    });
}
