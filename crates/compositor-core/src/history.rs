//! `DocumentHistory`: undo/redo over value snapshots of the document.
//!
//! Value snapshots share immutable images; layer edits copy no pixels.

use crate::document::{CanvasDocument, ImageLayer};
use crate::imported_image::{ImportedImage, PixelImage};
use crate::Id;
use std::collections::HashSet;
use std::sync::Arc;

/// The default undo depth (`DocumentHistory.init(entryLimit:)`).
pub const DEFAULT_ENTRY_LIMIT: usize = 100;
/// The default ceiling on the bytes history may keep alive (`retainedByteLimit:`).
pub const DEFAULT_RETAINED_BYTE_LIMIT: usize = 256 * 1024 * 1024;

/// A snapshot of the document as an edit began or ended, with the selection that went with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub document: Option<CanvasDocument>,
    pub active_layer_id: Option<Id>,
    pub revision: Id,
}

#[derive(Clone)]
struct Entry {
    name: String,
    before: Snapshot,
    after: Snapshot,
}

pub struct DocumentHistory {
    past: Vec<Entry>,
    future: Vec<Entry>,
    revision: Id,
    saved_revision: Option<Id>,
    pending: Option<Snapshot>,
    pending_name: String,
    depth: usize,
    pub entry_limit: usize,
    pub retained_byte_limit: usize,
}

impl DocumentHistory {
    pub fn new(entry_limit: usize, retained_byte_limit: usize) -> Self {
        let revision = Id::new_v4();
        Self {
            past: Vec::new(),
            future: Vec::new(),
            revision,
            saved_revision: Some(revision),
            pending: None,
            pending_name: "Edit".to_string(),
            depth: 0,
            entry_limit,
            retained_byte_limit,
        }
    }

    pub fn can_undo(&self) -> bool {
        self.depth == 0 && !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        self.depth == 0 && !self.future.is_empty()
    }

    pub fn undo_name(&self) -> String {
        self.past.last().map(|entry| entry.name.clone()).unwrap_or_default()
    }

    pub fn redo_name(&self) -> String {
        self.future.last().map(|entry| entry.name.clone()).unwrap_or_default()
    }

    pub fn is_modified(&self) -> bool {
        Some(self.revision) != self.saved_revision
    }

    pub fn undo_count(&self) -> usize {
        self.past.len()
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = Some(self.revision);
    }

    /// The document as it stands, for a save that captures it now and finishes later.
    pub fn current_revision(&self) -> Id {
        self.revision
    }

    /// A save of `saved` finished. Edits made while it was writing leave the document modified;
    /// undoing back to it does not.
    pub fn mark_saved_at(&mut self, saved: Id) {
        self.saved_revision = Some(saved);
    }

    pub fn reset(&mut self) {
        self.past.clear();
        self.future.clear();
        self.pending = None;
        self.depth = 0;
        self.revision = Id::new_v4();
        self.saved_revision = Some(self.revision);
    }

    pub fn begin(&mut self, name: &str, document: Option<&CanvasDocument>, selection: Option<Id>) {
        if self.depth == 0 {
            self.pending = Some(Snapshot {
                document: document.cloned(),
                active_layer_id: selection,
                revision: self.revision,
            });
            self.pending_name = name.to_string();
        }
        self.depth += 1;
    }

    pub fn end(&mut self, document: Option<&CanvasDocument>, selection: Option<Id>) {
        if self.depth == 0 {
            return;
        }
        self.depth -= 1;
        if self.depth != 0 {
            return;
        }
        let Some(before) = self.pending.take() else {
            return;
        };
        // Selecting, navigating, and no-op edits must preserve redo history.
        if before.document.as_ref() == document {
            return;
        }
        self.revision = Id::new_v4();
        self.past.push(Entry {
            name: self.pending_name.clone(),
            before,
            after: Snapshot {
                document: document.cloned(),
                active_layer_id: selection,
                revision: self.revision,
            },
        });
        self.future.clear();
        self.trim(document);
    }

    /// Reverts the last edit; the snapshot to restore, or `None` when there is nothing to undo.
    pub fn undo(&mut self) -> Option<Snapshot> {
        if !self.can_undo() {
            return None;
        }
        let entry = self.past.pop()?;
        self.future.push(entry);
        let entry = self.future.last()?;
        self.revision = entry.before.revision;
        let snapshot = entry.before.clone();
        let current = snapshot.document.as_ref();
        self.trim(current);
        Some(snapshot)
    }

    /// Reapplies the last undone edit; the snapshot to restore, or `None` when there is nothing to redo.
    pub fn redo(&mut self) -> Option<Snapshot> {
        if !self.can_redo() {
            return None;
        }
        let entry = self.future.pop()?;
        self.past.push(entry);
        let entry = self.past.last()?;
        self.revision = entry.after.revision;
        let snapshot = entry.after.clone();
        let current = snapshot.document.as_ref();
        self.trim(current);
        Some(snapshot)
    }

    /// Bytes retained only by history, excluding images in the live document.
    pub fn retained_bytes(&self, current: Option<&CanvasDocument>) -> usize {
        let mut seen: HashSet<*const ()> = HashSet::new();
        if let Some(document) = current {
            for layer in &document.layers {
                for asset in layer_assets(layer).into_iter().flatten() {
                    seen.insert(image_identity(&asset.image));
                    seen.insert(image_identity(&asset.thumbnail));
                }
            }
        }
        let mut bytes = 0;
        for entry in self.past.iter().chain(self.future.iter()) {
            for snapshot in [&entry.before, &entry.after] {
                let Some(document) = snapshot.document.as_ref() else {
                    continue;
                };
                for layer in &document.layers {
                    for asset in layer_assets(layer).into_iter().flatten() {
                        for image in [&asset.image, &asset.thumbnail] {
                            if seen.insert(image_identity(image)) {
                                bytes += image_bytes(image);
                            }
                        }
                    }
                }
            }
        }
        bytes
    }

    fn trim(&mut self, current: Option<&CanvasDocument>) {
        while self.past.len() + self.future.len() > self.entry_limit
            || self.retained_bytes(current) > self.retained_byte_limit
        {
            if !self.past.is_empty() {
                self.past.remove(0);
            } else if !self.future.is_empty() {
                self.future.remove(0);
            } else {
                break;
            }
        }
    }
}

impl Default for DocumentHistory {
    fn default() -> Self {
        Self::new(DEFAULT_ENTRY_LIMIT, DEFAULT_RETAINED_BYTE_LIMIT)
    }
}

/// A layer's own pixels and its mask's, the two the history counts.
fn layer_assets(layer: &ImageLayer) -> [Option<&ImportedImage>; 2] {
    [layer.asset.as_ref(), layer.mask.as_ref().map(|mask| &mask.asset)]
}

/// The raster's identity, the way `ObjectIdentifier` identified a `CGImage`: history counts an
/// image once, however many snapshots and layers share it.
fn image_identity(image: &PixelImage) -> *const () {
    match image {
        PixelImage::Rgba(shared) => Arc::as_ptr(shared) as *const (),
        PixelImage::Gray(shared) => Arc::as_ptr(shared) as *const (),
    }
}

/// The raster's bytes (`bytesPerRow * height`): 4 a pixel for RGBA, 1 for a mask's gray.
fn image_bytes(image: &PixelImage) -> usize {
    image.pixel_count() * if image.is_mask() { 1 } else { 4 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Rgba8Image;
    use crate::geom::Point;

    /// 4×3 pixels plus a 2×1 thumbnail, RGBA8.
    const IMAGE_BYTES: usize = 4 * 3 * 4 + 2 * 1 * 4;

    fn make_asset(tag: u8) -> ImportedImage {
        ImportedImage::new(
            PixelImage::Rgba(Arc::new(Rgba8Image::opaque(4, 3, [tag, 0, 0, 255]))),
            PixelImage::Rgba(Arc::new(Rgba8Image::opaque(2, 1, [tag, 0, 0, 255]))),
            format!("asset-{tag}.png"),
        )
    }

    fn document_with_asset(tag: u8) -> CanvasDocument {
        let mut document = CanvasDocument::new(64, 32);
        document.layers = vec![ImageLayer::from_asset(make_asset(tag), Point::ZERO)];
        document
    }

    #[test]
    fn default_limits_match_the_swift_defaults() {
        let history = DocumentHistory::default();
        assert_eq!(history.entry_limit, 100);
        assert_eq!(history.retained_byte_limit, 256 * 1024 * 1024);
    }

    #[test]
    fn history_bounds_entries_and_unique_retained_pixels() {
        let mut history = DocumentHistory::new(2, 0);
        let mut document = document_with_asset(1);
        let id = document.layers[0].id;
        for name in ["A", "B", "C"] {
            history.begin("Rename", Some(&document), Some(id));
            document.layers[0].name = name.to_string();
            history.end(Some(&document), Some(id));
        }
        assert_eq!(history.undo_count(), 2);
        assert_eq!(history.retained_bytes(Some(&document)), 0);
        history.begin("Delete", Some(&document), Some(id));
        document.layers.clear();
        history.end(Some(&document), None);
        assert_eq!(history.undo_count(), 0);
        assert_eq!(history.retained_bytes(Some(&document)), 0);
    }

    #[test]
    fn retained_bytes_counts_only_images_history_alone_keeps() {
        let mut history = DocumentHistory::default();
        let mut document = document_with_asset(1);
        let id = document.layers[0].id;
        history.begin("Rename", Some(&document), Some(id));
        document.layers[0].name = "A".to_string();
        history.end(Some(&document), Some(id));
        assert_eq!(
            history.retained_bytes(Some(&document)),
            0,
            "the live document still shares both images"
        );

        history.begin("Replace", Some(&document), Some(id));
        document.layers[0].asset = Some(make_asset(2));
        history.end(Some(&document), Some(id));
        assert_eq!(history.retained_bytes(Some(&document)), IMAGE_BYTES);

        document.layers.clear();
        assert_eq!(
            history.retained_bytes(Some(&document)),
            IMAGE_BYTES * 2,
            "each unique image is counted once"
        );
    }

    #[test]
    fn no_op_edits_preserve_redo_history() {
        let mut history = DocumentHistory::default();
        let mut document = document_with_asset(1);
        history.begin("Rename", Some(&document), None);
        document.layers[0].name = "Changed".to_string();
        history.end(Some(&document), None);
        assert_eq!(history.undo_count(), 1);
        assert_eq!(history.undo_name(), "Rename");
        assert!(history.can_undo() && !history.can_redo());

        history.undo().unwrap();
        assert!(history.can_redo() && !history.can_undo());
        assert_eq!(history.redo_name(), "Rename");

        // Selecting, navigating, and no-op edits must preserve redo history.
        history.begin("Nothing", Some(&document), None);
        history.end(Some(&document), None);
        assert_eq!(history.undo_count(), 0);
        assert!(history.can_redo(), "a no-op edit keeps redo history");

        let redone = history.redo().unwrap();
        assert_eq!(
            redone.document.as_ref().map(|document| document.layers[0].name.clone()),
            Some("Changed".to_string())
        );
        assert_eq!(history.undo_count(), 1);
        assert!(!history.can_redo());
    }

    #[test]
    fn entry_limit_drops_the_oldest_edit_first() {
        let mut history = DocumentHistory::new(2, usize::MAX);
        let mut document = CanvasDocument::new(8, 8);
        document.layers.push(ImageLayer::blank("Layer 1", document.size()));
        for name in ["A", "B", "C"] {
            history.begin(name, Some(&document), None);
            document.layers[0].name = name.to_string();
            history.end(Some(&document), None);
        }
        assert_eq!(history.undo_count(), 2);
        assert_eq!(history.undo_name(), "C");
    }

    #[test]
    fn save_revision_tracks_modification() {
        let mut history = DocumentHistory::default();
        let mut document = CanvasDocument::new(8, 8);
        document.layers.push(ImageLayer::blank("Layer 1", document.size()));
        history.mark_saved();
        assert!(!history.is_modified());

        history.begin("Rename", Some(&document), None);
        document.layers[0].name = "Changed".to_string();
        history.end(Some(&document), None);
        assert!(history.is_modified());

        history.undo();
        assert!(!history.is_modified(), "undoing back to the saved revision is unmodified");

        history.redo();
        assert!(history.is_modified());

        let writing = history.current_revision();
        history.mark_saved_at(writing);
        assert!(!history.is_modified());

        history.begin("Another", Some(&document), None);
        document.layers[0].name = "Later".to_string();
        history.end(Some(&document), None);
        assert!(history.is_modified(), "edits after the save was captured leave it modified");

        history.reset();
        assert!(!history.is_modified());
        assert!(!history.can_undo() && !history.can_redo());
        assert_eq!(history.undo_count(), 0);
        assert_eq!(history.undo_name(), "");
    }
}
