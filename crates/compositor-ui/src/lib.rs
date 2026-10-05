//! The Compositor editor's interface, in gpui (through `gpui-kit`, which re-exports gpui at its root and
//! the `gpui-component` widget library as `gpui_kit::component`).
//!
//! The views read `compositor_session::EditorSession` and call its commands; this crate holds no editor
//! logic of its own beyond presentation state.

pub mod actions;
pub mod canvas;
pub mod content_view;
pub mod panels;
pub mod sheets;
pub mod shortcuts;
pub mod theme;
pub mod tool_controls;
pub mod tool_header;
pub mod toolbar;
pub mod widgets;
pub mod workspace;

pub use content_view::ContentView;
pub use workspace::ProjectWorkspaceView;
