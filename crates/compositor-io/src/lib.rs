//! Files: the `.comp` project package, Photoshop PSD/PSB reading and writing, image import/export,
//! camera RAW, recent projects, the file watcher and the document digests.

pub mod image_exporter;
pub mod image_importer;
pub mod project_controller;
pub mod project_digest;
pub mod project_manifest;
pub mod project_store;
pub mod project_watcher;
pub mod psd;
pub mod raw_importer;
pub mod recent_projects;
pub mod resize;

pub use project_manifest::ProjectManifest;
pub use project_store::{ProjectError, ProjectSnapshot, ProjectStore};
