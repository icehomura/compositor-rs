//! Tool option settings.
//!
//! The per-person tool toggles (`ToolDefaults`: Auto Select, the transform box, rulers, guides, the
//! grid and its spacing/look, the snapping switches) are persisted by [`crate::settings`], which
//! owns the `tool.`-prefixed keys. Their typed accessors on `EditorSession` are session state and
//! are ported with the session, so there are no additional core model types here.
//!
//! `UI/SliderSnap.swift` (clicking a slider's track moves the knob to the click) is a view-layer
//! behavior and is ported with the gpui sliders, not in core.
