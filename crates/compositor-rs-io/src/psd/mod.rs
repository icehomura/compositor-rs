//! Photoshop PSD/PSB: reading (8-bit RGB; folders, masks, blend modes, fill rectangles/ellipses and
//! simple horizontal text stay editable) and building a Compositor document from the parsed records.

pub mod builder;
pub mod channel_coder;
pub mod reader;
pub mod text;
pub mod types;
pub mod vector;
