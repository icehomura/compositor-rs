//! The compositing pipeline: how layers become the canvas — drawing, tiling, masks, adjustment layers,
//! layer effects, previews and the caches that keep it interactive.
//!
//! The original ran this on Metal (`GPUCanvas`) with a Core Graphics fallback (`LayerRenderer`,
//! `TiledLayerRenderer`); here every path is CPU code over `compositor-pixels` kernels, parallelized with
//! `rayon`, with the same visuals and the same tile/preview caching behaviour.

pub mod adjustment_surface;
pub mod downsample_cache;
pub mod effects_preview_cache;
pub mod layer_renderer;
pub mod live_mask_renderer;
pub mod tiled_layer_renderer;
