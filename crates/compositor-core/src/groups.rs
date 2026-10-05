//! The layer hierarchy: the flat row list the Layers panel shows, the folder opacity that multiplies
//! into what is inside it, and the cached drawing order the canvas walks. Ported from
//! `Document/LayerGroups.swift`.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::document::{CanvasDocument, ImageLayer, ProjectError, ProjectLayerRecord};
use crate::Id;

/// One row of the flattened layer tree (`LayerHierarchy.Entry`).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerHierarchyEntry {
    pub layer: ProjectLayerRecord,
    pub depth: usize,
    /// False when this layer or one of its folders is hidden.
    pub visible: bool,
}

/// The flattened layer tree, and the parent/cycle validation the project store runs before loading.
pub struct LayerHierarchy;

impl LayerHierarchy {
    /// Every layer and folder under `layers`, depth first, each row knowing whether it shows.
    /// `top_first` lists each folder above its contents, as the panel does; a folder in `collapsed`
    /// keeps its contents out of the list.
    pub fn entries<S: BuildHasher>(
        layers: &[ProjectLayerRecord],
        top_first: bool,
        collapsed: &HashSet<Id, S>,
    ) -> Vec<LayerHierarchyEntry> {
        let mut children: FxHashMap<Option<Id>, Vec<usize>> = FxHashMap::default();
        for (index, layer) in layers.iter().enumerate() {
            children.entry(layer.parent_id).or_default().push(index);
        }
        let mut result = Vec::new();
        visit(layers, &children, None, 0, true, top_first, collapsed, &mut result);
        result
    }

    /// Every layer that shows, bottom to top, folders left out.
    pub fn visible_layers(layers: &[ProjectLayerRecord]) -> Vec<ProjectLayerRecord> {
        Self::entries(layers, false, &HashSet::new())
            .into_iter()
            .filter(|entry| entry.visible && entry.layer.is_group != Some(true))
            .map(|entry| entry.layer)
            .collect()
    }

    /// Duplicate ids, a folder carrying pixels, a missing parent, a parent that is not a folder, a
    /// cycle, or a chain over 64 ancestors deep are all invalid.
    pub fn validate(layers: &[ProjectLayerRecord]) -> Result<(), ProjectError> {
        let mut by_id: FxHashMap<Id, &ProjectLayerRecord> = FxHashMap::default();
        for layer in layers {
            if by_id.insert(layer.id, layer).is_some() || (layer.is_group == Some(true) && layer.image_file.is_some()) {
                return Err(ProjectError::Invalid);
            }
        }
        for layer in layers {
            let mut seen: FxHashSet<Id> = FxHashSet::default();
            seen.insert(layer.id);
            let mut parent = layer.parent_id;
            while let Some(id) = parent {
                if seen.len() > 64 || !seen.insert(id) {
                    return Err(ProjectError::Invalid);
                }
                let Some(node) = by_id.get(&id) else { return Err(ProjectError::Invalid) };
                if node.is_group != Some(true) {
                    return Err(ProjectError::Invalid);
                }
                parent = node.parent_id;
            }
            if layer.is_group == Some(true) && seen.len() > 64 {
                return Err(ProjectError::Invalid);
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn visit<S: BuildHasher>(
    layers: &[ProjectLayerRecord],
    children: &FxHashMap<Option<Id>, Vec<usize>>,
    parent: Option<Id>,
    depth: usize,
    visible: bool,
    top_first: bool,
    collapsed: &HashSet<Id, S>,
    result: &mut Vec<LayerHierarchyEntry>,
) {
    if depth > 64 {
        return;
    }
    let Some(indices) = children.get(&parent) else { return };
    let count = indices.len();
    for position in 0..count {
        let index = indices[if top_first { count - 1 - position } else { position }];
        let layer = &layers[index];
        let effective = visible && layer.is_visible;
        result.push(LayerHierarchyEntry {
            layer: layer.clone(),
            depth,
            visible: effective,
        });
        if layer.is_group == Some(true) && !collapsed.contains(&layer.id) {
            visit(
                layers,
                children,
                Some(layer.id),
                depth + 1,
                effective,
                top_first,
                collapsed,
                result,
            );
        }
    }
}

/// A folder's opacity multiplies into everything inside it: a layer at 50% in a folder at 50%
/// shows at 25%, while the layer itself still reads 50% in the panel. Folders are pass-through —
/// what's inside is drawn straight onto what is below, never composited as a unit — so the
/// folder's opacity is applied to each of those layers rather than to the folder as a whole.
pub struct LayerOpacity;

impl LayerOpacity {
    /// Walks up the folder chain, at most 64 folders deep, multiplying each folder's own opacity in.
    /// `folder` answers with a folder's own opacity and its parent, or nil when the id is not a
    /// folder in the document any more.
    pub fn effective<F>(own: f64, parent: Option<Id>, folder: F) -> f64
    where
        F: Fn(Id) -> Option<(f64, Option<Id>)>,
    {
        let mut opacity = own;
        let mut id = parent;
        let mut depth = 0;
        while let Some(current) = id {
            if depth >= 64 {
                break;
            }
            let Some(node) = folder(current) else { break };
            opacity *= node.0;
            id = node.1;
            depth += 1;
        }
        opacity
    }
}

impl ImageLayer {
    /// The opacity this layer is drawn at, folders included (see [`LayerOpacity`]).
    pub fn effective_opacity<S: BuildHasher>(&self, by_id: &HashMap<Id, ImageLayer, S>) -> f64 {
        LayerOpacity::effective(self.opacity, self.parent_id, |id| {
            by_id.get(&id).map(|layer| (layer.opacity, layer.parent_id))
        })
    }

    /// The layer as the manifest stores it. The asset and mask filenames are `<id>.png` and
    /// `<id>.mask.png`, with the upper-case UUID spelling `UUID.uuidString` uses.
    pub fn hierarchy_record(&self) -> ProjectLayerRecord {
        let stem = self.id.to_string().to_uppercase();
        ProjectLayerRecord {
            id: self.id,
            name: self.name.clone(),
            is_visible: self.is_visible,
            transform: self.transform,
            image_file: self.asset.as_ref().map(|_| format!("{stem}.png")),
            parent_id: self.parent_id,
            is_group: Some(self.is_group),
            opacity: Some(self.opacity),
            blend_mode: Some(self.blend_mode),
            mask_file: self.mask.as_ref().map(|_| format!("{stem}.mask.png")),
            mask_enabled: self.mask.as_ref().map(|mask| mask.is_enabled),
            mask_source_id: self.mask_source_id,
            adjustment: self.adjustment.clone(),
            mask_placement: self.mask.as_ref().and_then(|mask| mask.placement),
            mask_linked: self.mask.as_ref().map(|mask| mask.is_linked),
            // The Swift helper stops at the mask: a shape, its effects and its text are left out.
            shape: None,
            effects: None,
            text: None,
        }
    }
}

impl ProjectLayerRecord {
    /// The opacity this layer is drawn at, folders included (see [`LayerOpacity`]). Missing
    /// opacities are full opacity, as older manifests leave them.
    pub fn effective_opacity<S: BuildHasher>(&self, by_id: &HashMap<Id, ProjectLayerRecord, S>) -> f64 {
        LayerOpacity::effective(self.opacity.unwrap_or(1.0), self.parent_id, |id| {
            by_id.get(&id).map(|record| (record.opacity.unwrap_or(1.0), record.parent_id))
        })
    }
}

impl CanvasDocument {
    /// Every layer's drawn opacity, folders included.
    pub fn effective_opacities(&self) -> FxHashMap<Id, f64> {
        let folders: FxHashMap<Id, (f64, Option<Id>)> = self
            .layers
            .iter()
            .map(|layer| (layer.id, (layer.opacity, layer.parent_id)))
            .collect();
        folders
            .iter()
            .map(|(id, (opacity, parent))| {
                (*id, LayerOpacity::effective(*opacity, *parent, |key| folders.get(&key).copied()))
            })
            .collect()
    }

    /// Every layer and folder in drawing order (each folder before what's inside it), and which of
    /// them show.
    pub fn hierarchy(&self) -> LayerOrderResult {
        LayerOrder::resolve(&self.layers)
    }

    pub fn effective_visible_ids(&self) -> FxHashSet<Id> {
        self.hierarchy().visible
    }

    /// The layers that show, folders left out, bottom to top.
    pub fn render_layers(&self) -> Vec<&ImageLayer> {
        let order = self.hierarchy().drawn;
        if order.is_empty() {
            return Vec::new();
        }
        let index: FxHashMap<Id, usize> = self
            .layers
            .iter()
            .enumerate()
            .map(|(position, layer)| (layer.id, position))
            .collect();
        order
            .iter()
            .filter_map(|id| index.get(id).map(|position| &self.layers[*position]))
            .collect()
    }
}

/// The hierarchy worked out from only what shapes it — each layer's id, folder, and visibility
/// (`LayerOrder.Node`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerOrderNode {
    pub id: Id,
    pub parent_id: Option<Id>,
    pub is_group: bool,
    pub is_visible: bool,
}

/// The resolved hierarchy (`LayerOrder.Result`).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerOrderResult {
    /// Every layer and folder, in the order `LayerHierarchy.entries` lists them.
    pub order: Vec<Id>,
    /// The ones that show: visible, in folders that are.
    pub visible: FxHashSet<Id>,
    /// The layers that show, folders left out, bottom to top.
    pub drawn: Vec<Id>,
}

/// The layer hierarchy worked out from only what shapes it — each layer's id, folder, and visibility —
/// and kept until one of those changes. The canvas asks for it several times on every event; rebuilt
/// each time from whole layer records, a document of hundreds of layers spent most of its time on it.
pub struct LayerOrder;

/// The last resolution, kept beside the nodes it was made from, as the Swift caches it.
static LAST: LazyLock<Mutex<Option<Arc<(Vec<LayerOrderNode>, LayerOrderResult)>>>> =
    LazyLock::new(|| Mutex::new(None));

impl LayerOrder {
    pub fn resolve(layers: &[ImageLayer]) -> LayerOrderResult {
        let nodes: Vec<LayerOrderNode> = layers
            .iter()
            .map(|layer| LayerOrderNode {
                id: layer.id,
                parent_id: layer.parent_id,
                is_group: layer.is_group,
                is_visible: layer.is_visible,
            })
            .collect();
        let known = LAST.lock().clone();
        if let Some(known) = known {
            if known.0 == nodes {
                return known.1.clone();
            }
        }

        let mut children: FxHashMap<Option<Id>, Vec<usize>> = FxHashMap::default();
        for (index, node) in nodes.iter().enumerate() {
            children.entry(node.parent_id).or_default().push(index);
        }
        let mut order = Vec::new();
        let mut visible = FxHashSet::default();
        let mut drawn = Vec::new();
        visit_order(
            &nodes,
            &children,
            None,
            0,
            true,
            &mut order,
            &mut visible,
            &mut drawn,
        );
        let result = LayerOrderResult { order, visible, drawn };
        *LAST.lock() = Some(Arc::new((nodes, result.clone())));
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_order(
    nodes: &[LayerOrderNode],
    children: &FxHashMap<Option<Id>, Vec<usize>>,
    parent: Option<Id>,
    depth: usize,
    shown: bool,
    order: &mut Vec<Id>,
    visible: &mut FxHashSet<Id>,
    drawn: &mut Vec<Id>,
) {
    if depth > 64 {
        return;
    }
    let Some(indices) = children.get(&parent) else { return };
    for &index in indices {
        let node = &nodes[index];
        let effective = shown && node.is_visible;
        order.push(node.id);
        if effective {
            visible.insert(node.id);
            if !node.is_group {
                drawn.push(node.id);
            }
        }
        if node.is_group {
            visit_order(
                nodes,
                children,
                Some(node.id),
                depth + 1,
                effective,
                order,
                visible,
                drawn,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::transform_at;
    use crate::geom::{Point, Size};

    fn record(id: Id, group: Option<bool>, parent: Option<Id>, image_file: Option<&str>) -> ProjectLayerRecord {
        ProjectLayerRecord {
            id,
            name: "Layer".to_string(),
            is_visible: true,
            transform: transform_at(Point::ZERO, Size::new(10.0, 10.0)),
            image_file: image_file.map(str::to_string),
            parent_id: parent,
            is_group: group,
            opacity: None,
            blend_mode: None,
            mask_file: None,
            mask_enabled: None,
            mask_source_id: None,
            adjustment: None,
            mask_placement: None,
            mask_linked: None,
            shape: None,
            effects: None,
            text: None,
        }
    }

    fn layer(name: &str, group: bool, parent: Option<Id>) -> ImageLayer {
        let mut layer = ImageLayer::blank(name, Size::new(10.0, 10.0));
        layer.is_group = group;
        layer.parent_id = parent;
        layer
    }

    #[test]
    fn entries_nest_folders_and_respect_collapse_cases() {
        let plain = crate::new_id();
        let folder = crate::new_id();
        let first = crate::new_id();
        let second = crate::new_id();
        let records = vec![
            record(plain, Some(false), None, Some("plain.png")),
            record(folder, Some(true), None, None),
            record(first, Some(false), Some(folder), Some("first.png")),
            record(second, Some(false), Some(folder), Some("second.png")),
        ];

        let bottom_first = LayerHierarchy::entries(&records, false, &HashSet::new());
        assert_eq!(
            bottom_first.iter().map(|entry| entry.layer.id).collect::<Vec<_>>(),
            vec![plain, folder, first, second]
        );
        assert_eq!(bottom_first.iter().map(|entry| entry.depth).collect::<Vec<_>>(), vec![0, 0, 1, 1]);
        assert!(bottom_first.iter().all(|entry| entry.visible));

        // Top first reverses every sibling list, folder contents included.
        let top_first = LayerHierarchy::entries(&records, true, &HashSet::new());
        assert_eq!(
            top_first.iter().map(|entry| entry.layer.id).collect::<Vec<_>>(),
            vec![folder, second, first, plain]
        );

        // A collapsed folder keeps its contents out of the list entirely.
        let collapsed: FxHashSet<Id> = [folder].into_iter().collect();
        let collapsed_entries = LayerHierarchy::entries(&records, false, &collapsed);
        assert_eq!(
            collapsed_entries.iter().map(|entry| entry.layer.id).collect::<Vec<_>>(),
            vec![plain, folder]
        );

        assert_eq!(
            LayerHierarchy::visible_layers(&records).iter().map(|layer| layer.id).collect::<Vec<_>>(),
            vec![plain, first, second]
        );
    }

    #[test]
    fn a_hidden_folder_hides_everything_inside_it() {
        let folder = crate::new_id();
        let child = crate::new_id();
        let mut folder_record = record(folder, Some(true), None, None);
        folder_record.is_visible = false;
        let records = vec![
            record(child, Some(false), Some(folder), Some("child.png")),
            folder_record,
        ];

        let entries = LayerHierarchy::entries(&records, false, &HashSet::new());
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].visible, "the child of a hidden folder does not show");
        assert!(!entries[1].visible);
        assert!(LayerHierarchy::visible_layers(&records).is_empty());
    }

    #[test]
    fn validate_rejects_duplicates_cycles_and_bad_parents() {
        fn invalid(result: Result<(), ProjectError>) -> bool {
            matches!(result, Err(ProjectError::Invalid))
        }

        let id = crate::new_id();
        let child = crate::new_id();
        let group = record(id, Some(true), Some(child), None);
        let nested = record(child, Some(true), Some(id), None);
        assert!(invalid(LayerHierarchy::validate(&[group.clone(), nested.clone()])), "a cycle");

        // A parent that does not exist.
        assert!(invalid(LayerHierarchy::validate(&[group.clone()])));

        // A parent that exists but is not a folder.
        let mut raster_parent = group.clone();
        raster_parent.parent_id = None;
        raster_parent.is_group = Some(false);
        assert!(invalid(LayerHierarchy::validate(&[raster_parent, nested])));

        // A folder carrying pixels.
        let mut carrying = record(crate::new_id(), Some(true), None, Some("group.png"));
        assert!(invalid(LayerHierarchy::validate(&[carrying.clone()])));
        carrying.is_group = Some(false);
        assert!(LayerHierarchy::validate(&[carrying]).is_ok());

        // A duplicate id.
        let duplicate = record(id, Some(false), None, None);
        assert!(invalid(LayerHierarchy::validate(&[group, duplicate])));
    }

    #[test]
    fn validate_rejects_chains_deeper_than_sixty_four() {
        let chain_of = |count: usize| -> Vec<ProjectLayerRecord> {
            let ids: Vec<Id> = (0..count).map(|_| crate::new_id()).collect();
            ids.iter()
                .enumerate()
                .map(|(index, id)| record(*id, Some(true), index.checked_sub(1).map(|previous| ids[previous]), None))
                .collect()
        };

        // The depth counts the layer itself, so a folder may have 63 ancestors (64 levels) — exactly
        // the Swift guard's arithmetic — and one more is rejected.
        assert!(LayerHierarchy::validate(&chain_of(64)).is_ok());
        assert!(LayerHierarchy::validate(&chain_of(65)).is_err(), "sixty-four ancestors is too deep");
        assert!(LayerHierarchy::validate(&chain_of(70)).is_err());
    }

    #[test]
    fn folder_opacity_multiplies_through_the_chain() {
        let folder = crate::new_id();
        let child = crate::new_id();
        let mut folder_layer = layer("Folder", true, None);
        folder_layer.id = folder;
        folder_layer.opacity = 0.5;
        let mut child_layer = layer("Child", false, Some(folder));
        child_layer.id = child;
        child_layer.opacity = 0.5;

        let by_id: HashMap<Id, ImageLayer> = [(folder, folder_layer), (child, child_layer)].into_iter().collect();
        assert_eq!(by_id[&child].effective_opacity(&by_id), 0.25);
        assert_eq!(LayerOpacity::effective(0.5, Some(folder), |id| by_id.get(&id).map(|l| (l.opacity, l.parent_id))), 0.25);
        // A missing folder stops the walk, as the Swift dictionary lookup does.
        assert_eq!(LayerOpacity::effective(0.5, Some(crate::new_id()), |_| None), 0.5);

        let records: HashMap<Id, ProjectLayerRecord> = [
            (folder, record(folder, Some(true), None, None)),
            (child, record(child, Some(false), Some(folder), Some("child.png"))),
        ]
        .into_iter()
        .collect();
        assert_eq!(records[&child].effective_opacity(&records), 1.0, "missing opacities are full opacity");
    }

    #[test]
    fn canvas_document_resolves_opacities_order_and_visibility() {
        let base = crate::new_id();
        let folder = crate::new_id();
        let inside = crate::new_id();
        let inside_hidden = crate::new_id();

        let mut base_layer = layer("Base", false, None);
        base_layer.id = base;
        let mut folder_layer = layer("Folder", true, None);
        folder_layer.id = folder;
        folder_layer.opacity = 0.5;
        let mut inside_layer = layer("Inside", false, Some(folder));
        inside_layer.id = inside;
        inside_layer.opacity = 0.5;
        let mut hidden_layer = layer("Hidden", false, Some(folder));
        hidden_layer.id = inside_hidden;
        hidden_layer.is_visible = false;

        let mut document = CanvasDocument::new(10, 10);
        document.layers = vec![base_layer, folder_layer, inside_layer, hidden_layer];

        let opacities = document.effective_opacities();
        assert_eq!(opacities[&base], 1.0);
        assert_eq!(opacities[&folder], 0.5);
        assert_eq!(opacities[&inside], 0.25);

        // Hidden layers are still ordered, but neither they nor anything in a hidden folder is drawn.
        let hierarchy = document.hierarchy();
        assert_eq!(hierarchy.order, vec![base, folder, inside, inside_hidden]);
        assert_eq!(hierarchy.drawn, vec![base, inside]);
        assert!(!hierarchy.visible.contains(&inside_hidden));
        assert_eq!(document.effective_visible_ids(), hierarchy.visible);
        assert_eq!(
            document.render_layers().iter().map(|layer| layer.id).collect::<Vec<_>>(),
            vec![base, inside]
        );

        // The resolution is kept until the nodes change: the same layers answer the same thing.
        assert_eq!(LayerOrder::resolve(&document.layers), hierarchy);

        document.layers[0].is_visible = false;
        let resolved = document.hierarchy();
        assert_eq!(resolved.drawn, vec![inside]);
        assert!(!resolved.visible.contains(&base));
    }
}
