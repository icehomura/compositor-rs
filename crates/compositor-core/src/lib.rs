//! Compositor's core: geometry, color, pixel buffers, the document model, undo history, selection
//! geometry and the tool settings. No UI toolkit, no platform IO, no GPU.
//!
//! Naming, numeric conventions and the crate map are documented in `docs/PORTING.md`.

pub mod blend;
pub mod buffer;
pub mod canvas_size;
pub mod color;
pub mod document;
pub mod error;
pub mod geom;
pub mod groups;
pub mod guides;
pub mod history;
pub mod image_ops;
pub mod imported_image;
pub mod layer_adjustment;
pub mod layer_appearance;
pub mod layer_effects;
pub mod layer_mask;
pub mod layer_shape;
pub mod layer_text;
pub mod layer_transform;
pub mod limits;
pub mod palette;
pub mod path;
pub mod raster;
pub mod selection;
pub mod settings;
pub mod snap;
pub mod tool_settings;
pub mod viewport;

pub use blend::LayerBlendMode;
pub use buffer::{Gray8Image, Rgba8Image, SharedGray, SharedImage, GRAY_PIXEL, RGBA_PIXEL, TILE};
pub use color::PaletteColor;
pub use error::{CoreError, Result};
pub use geom::{AffineTransform, CGFloat, Point, Rect, Size};
pub use limits::{MAX_SIDE, MAX_SIDE_EXTENT, MAX_SURFACE_EXTENT, MAX_SURFACE_PIXELS};

// Re-enabled as the model modules land (see docs/PORTING.md).
// pub use document::{CanvasDocument, ImageLayer, NavigationTool};
// pub use layer_transform::{LayerSampling, LayerTransform};
// pub use selection::DocumentSelection;

/// The document UUIDs the model uses.
pub type Id = uuid::Uuid;

/// A fresh identifier.
pub fn new_id() -> Id {
    uuid::Uuid::new_v4()
}
