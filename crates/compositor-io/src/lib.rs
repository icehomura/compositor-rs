//! Files: the `.comp` project package, Photoshop PSD/PSB reading and writing, image import/export,
//! camera RAW, recent projects, the file watcher and the document digests.

pub mod image_exporter;
pub mod project_manifest;
pub mod project_store;

pub use project_manifest::ProjectManifest;
pub use project_store::{ProjectError, ProjectSnapshot, ProjectStore};
