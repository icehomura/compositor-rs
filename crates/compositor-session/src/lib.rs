//! `EditorSession`: every piece of editor state and every command it runs — tools, drafts, panels,
//! undo transactions, clipboard, crop, gradient, type, brush, warp, project lifecycle.
//!
//! This is the port of `Document/EditorSession*.swift` plus the command bodies that lived inside the
//! SwiftUI/AppKit views. It holds no UI toolkit types: the views read this state and call these methods.

pub mod adjustments;
pub mod brush;
pub mod camera_raw;
pub mod canvas_ops;
pub mod clipboard;
pub mod color;
pub mod crop;
pub mod effects;
pub mod filters;
pub mod floating;
pub mod gradient;
pub mod guides;
pub mod layers;
pub mod live_mask;
pub mod mask_ops;
pub mod previews;
pub mod projects;
pub mod selection;
pub mod session;
pub mod shape;
pub mod tools;
pub mod transform;
pub mod type_tool;
pub mod view;

pub use session::EditorSession;
