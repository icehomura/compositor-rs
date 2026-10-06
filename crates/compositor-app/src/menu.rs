//! The application menu bar: a port of `CompositorApp.swift`'s `.commands`.
//!
//! The SwiftUI command groups are rebuilt by SwiftUI whenever the state they read changes; the menu
//! bar here is rebuilt the same way, from [`MenuState`], which is every value a label or an enabled
//! flag reads. [`MenuState::read`] takes the front tab's session, so the bar follows the tab in front.
//!
//! Windows has no menu bar of its own for gpui to fill: `App::set_menus` is stored by the platform
//! (macOS draws it natively) and the `AppMenuBar` widget draws the same menus from
//! `GlobalState::app_menus`, so [`crate::AppRoot`] feeds both.

use std::path::PathBuf;

use compositor_core::Id;
use compositor_core::image_ops::FilterKind;
use compositor_core::layer_adjustment::AdjustmentKind;
use compositor_session::EditorSession;

use compositor_ui::actions;
use gpui_kit::*;

/// Every value the menu bar's labels and enabled flags read, in the order the Swift groups appear.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuState {
    /// `session.textDraft != nil`.
    text_draft: bool,
    levels: bool,
    is_project_busy: bool,
    shows_new_document: bool,
    shows_importer: bool,
    renaming_layer: bool,
    transform_persistent: bool,
    can_undo: bool,
    can_redo: bool,
    undo_name: String,
    redo_name: String,
    can_start: bool,
    document: bool,
    is_importing: bool,
    recent: Vec<PathBuf>,
    tool: compositor_core::document::NavigationTool,
    shows_pixel_grid: bool,
    snapping_enabled: bool,
    shows_transform_controls: bool,
    shows_grid: bool,
    shows_guides: bool,
    shows_rulers: bool,
    snap_enabled: bool,
    snap_to_guides: bool,
    snap_to_grid: bool,
    snap_to_layers: bool,
    snap_to_document_bounds: bool,
    locks_guides: bool,
    can_clear_guides: bool,
    selection: bool,
    can_copy_pixels: bool,
    can_copy_layer: bool,
    can_copy_merged: bool,
    can_paste: bool,
    can_edit_pixels: bool,
    can_content_aware_fill: bool,
    can_edit_selection: bool,
    active_asset: bool,
    active_mask: bool,
    can_select_subject: bool,
    can_select_color_range: bool,
    can_modify_selection: bool,
    can_adjust_colors: bool,
    hue_saturation: bool,
    is_mask_selected: bool,
    can_invert: bool,
    can_edit_layers: bool,
    can_vignette: bool,
    active_layer: Option<Id>,
    has_adjustment: bool,
    has_parent: bool,
    is_visible: bool,
    mask_source: bool,
    can_toggle_clipping_mask: bool,
    can_transform: bool,
    can_transform_selection: bool,
    can_ungroup_layers: bool,
    can_merge_layers: bool,
    merge_title: String,
    can_move_up: bool,
    can_move_down: bool,
    selected_effect: Option<&'static str>,
    selected_layer_count: usize,
}

impl MenuState {
    /// Reads the front tab's session. `is_managing` is `workspace.isManaging`; `can_start` is the
    /// controller's `canStart`.
    pub fn read(session: &EditorSession, is_managing: bool, recent: &[PathBuf]) -> Self {
        let active = session.active_layer();
        Self {
            text_draft: session.text_draft.is_some(),
            levels: session.levels.is_some(),
            is_project_busy: session.is_project_busy,
            shows_new_document: session.shows_new_document,
            shows_importer: session.shows_importer,
            renaming_layer: session.renaming_layer_id.is_some(),
            transform_persistent: session.transform_edit.as_ref().is_some_and(|edit| edit.persistent),
            can_undo: session.can_undo(),
            can_redo: session.can_redo(),
            undo_name: session.history.undo_name(),
            redo_name: session.history.redo_name(),
            can_start: session.can_start_project_operation() && !is_managing,
            document: session.document.is_some(),
            is_importing: session.is_importing,
            recent: recent.to_vec(),
            tool: session.tool,
            shows_pixel_grid: session.shows_pixel_grid,
            snapping_enabled: session.snapping_enabled,
            shows_transform_controls: session.shows_transform_controls,
            shows_grid: session.shows_grid,
            shows_guides: session.shows_guides,
            shows_rulers: session.shows_rulers,
            snap_enabled: session.snap_enabled,
            snap_to_guides: session.snap_to_guides,
            snap_to_grid: session.snap_to_grid,
            snap_to_layers: session.snap_to_layers,
            snap_to_document_bounds: session.snap_to_document_bounds,
            locks_guides: session.locks_guides,
            can_clear_guides: session.can_clear_guides(),
            selection: session.selection().is_some(),
            can_copy_pixels: session.can_copy_pixels(),
            can_copy_layer: session.can_copy_layer(),
            can_copy_merged: session.can_copy_merged(),
            can_paste: session.can_paste(),
            can_edit_pixels: session.can_edit_pixels(),
            can_content_aware_fill: session.can_content_aware_fill(),
            can_edit_selection: session.can_edit_selection(),
            active_asset: active.is_some_and(|layer| layer.asset.is_some()),
            active_mask: active.is_some_and(|layer| layer.mask.is_some()),
            can_select_subject: session.can_select_subject(),
            can_select_color_range: session.can_select_color_range(),
            can_modify_selection: session.can_modify_selection(),
            can_adjust_colors: session.can_adjust_colors(),
            hue_saturation: session.hue_saturation.is_some(),
            is_mask_selected: session.is_mask_selected,
            can_invert: session.can_invert(),
            can_edit_layers: session.can_edit_layers(),
            can_vignette: session.can_vignette(),
            active_layer: session.active_layer_id,
            has_adjustment: active.is_some_and(|layer| layer.adjustment.is_some()),
            has_parent: active.is_some_and(|layer| layer.parent_id.is_some()),
            is_visible: active.is_some_and(|layer| layer.is_visible),
            mask_source: active.is_some_and(|layer| layer.mask_source_id.is_some()),
            can_toggle_clipping_mask: session
                .active_layer_id
                .is_some_and(|id| session.can_toggle_clipping_mask(id)),
            can_transform: session.can_transform(),
            can_transform_selection: session.can_transform_selection(),
            can_ungroup_layers: session.can_ungroup_layers(),
            can_merge_layers: session.can_merge_layers(),
            merge_title: session.merge_title().to_string(),
            can_move_up: session.can_move_active_layer(1),
            can_move_down: session.can_move_active_layer(-1),
            selected_effect: session.selected_effect().map(|selected| selected.kind.raw_value()),
            selected_layer_count: session.selected_layer_ids.len(),
        }
    }

    /// The whole bar, in the order `CompositorApp.swift` declares the command groups and menus.
    pub fn menus(&self) -> Vec<Menu> {
        vec![
            self.application_menu(),
            self.file_menu(),
            self.edit_menu(),
            self.view_menu(),
            self.select_menu(),
            self.image_menu(),
            self.filter_menu(),
            self.layer_menu(),
        ]
    }

    /// The app menu: `after: .appInfo` and `replacing: .appVisibility`.
    fn application_menu(&self) -> Menu {
        let items = vec![
            MenuItem::action("Check for Updates…", actions::CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Hide Compositor", actions::HideCompositor),
            MenuItem::action("Hide Others", actions::HideOthers),
            MenuItem::action("Show All", actions::ShowAll),
        ];
        Menu::new("Compositor").items(items)
    }

    /// `CommandGroup(replacing: .newItem)` then `CommandGroup(replacing: .saveItem)`.
    fn file_menu(&self) -> Menu {
        let mut recent = Vec::new();
        for url in &self.recent {
            let name = url
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| url.to_string_lossy().into_owned());
            let action = actions::OpenRecentProject {
                path: url.to_string_lossy().into_owned(),
            };
            recent.push(MenuItem::action(name, action));
        }
        recent.push(MenuItem::separator());
        recent.push(MenuItem::action("Clear Menu", actions::ClearRecentProjects).disabled(self.recent.is_empty()));
        Menu::new("File").items([
            MenuItem::action("New Canvas…", actions::NewCanvas).disabled(!self.can_start),
            MenuItem::action("Open Project…", actions::OpenProject).disabled(!self.can_start),
            MenuItem::submenu(Menu::new("Open Recent").items(recent).disabled(!self.can_start)),
            MenuItem::action("Import Images…", actions::ImportImages).disabled(
                self.levels || self.shows_busy() || self.is_importing || self.shows_new_document,
            ),
            MenuItem::separator(),
            MenuItem::action("Save", actions::SaveProject).disabled(!self.document || !self.can_start),
            MenuItem::action("Save As…", actions::SaveProjectAs).disabled(!self.document || !self.can_start),
            MenuItem::separator(),
            MenuItem::action("Export PNG…", actions::ExportPng).disabled(!self.document || !self.can_start),
            MenuItem::action("Export JPEG…", actions::ExportJpeg).disabled(!self.document || !self.can_start),
            MenuItem::separator(),
            MenuItem::action("Close Project", actions::CloseProject).disabled(!self.can_start),
        ])
    }

    /// `CommandGroup(replacing: .undoRedo)`, the pasteboard group and the fills after it.
    fn edit_menu(&self) -> Menu {
        let mut items = Vec::new();
        // Dialog text fields keep native text undo; document history is unavailable while an import or
        // modal edit is active.
        if self.dialog_holds_undo() {
            items.push(MenuItem::action("Undo", actions::Undo));
            items.push(MenuItem::action("Redo", actions::Redo));
        } else {
            items.push(
                MenuItem::action(self.undo_title(), actions::Undo).disabled(!self.can_undo),
            );
            items.push(
                MenuItem::action(self.redo_title(), actions::Redo).disabled(!self.can_redo),
            );
        }
        items.push(MenuItem::separator());
        // Cut, Copy and Paste check when chosen rather than through `.disabled`: what they depend on
        // (the pasteboard, the copied pixels, the busy flag) is not observed, so a disabled state could
        // go stale.
        items.push(MenuItem::action("Cut", actions::Cut));
        items.push(MenuItem::action("Copy", actions::Copy));
        items
            .push(MenuItem::action("Copy Merged", actions::CopyMerged).disabled(!self.can_copy_merged));
        items.push(MenuItem::action("Paste", actions::Paste));
        items.push(MenuItem::separator());
        items.push(MenuItem::action("Keyboard Shortcuts…", actions::ShowKeyboardShortcuts));
        // Photoshop's fill shortcuts; in a text field they keep their text meaning.
        items.push(
            MenuItem::action("Fill with Foreground Color", actions::FillWithForeground)
                .disabled(!self.can_edit_pixels),
        );
        items.push(
            MenuItem::action("Fill with Background Color", actions::FillWithBackground)
                .disabled(!self.can_edit_pixels),
        );
        items.push(
            MenuItem::action("Clear Selection Pixels", actions::ClearSelectionPixels)
                .disabled(!self.selection || !self.can_edit_pixels),
        );
        items.push(
            MenuItem::action("Content-Aware Fill…", actions::ContentAwareFill)
                .disabled(!self.can_content_aware_fill),
        );
        Menu::new("Edit").items(items)
    }

    /// `CommandGroup(after: .toolbar)` and its `Show` / `Snap To` submenus.
    fn view_menu(&self) -> Menu {
        let items = vec![
            MenuItem::action("Fit Canvas", actions::FitCanvas).disabled(!self.document),
            MenuItem::action("Actual Pixels", actions::ActualPixels).disabled(!self.document),
            MenuItem::action("Zoom In", actions::ZoomIn).disabled(!self.document),
            MenuItem::action("Zoom Out", actions::ZoomOut).disabled(!self.document),
            MenuItem::action("Pixel Grid (800% and above)", actions::TogglePixelGrid).checked(self.shows_pixel_grid),
            MenuItem::action("Snap", actions::ToggleSnapping).checked(self.snapping_enabled),
            MenuItem::action("Show Transform Controls", actions::ToggleTransformControls)
                .checked(self.shows_transform_controls)
                .disabled(self.tool != compositor_core::document::NavigationTool::Move || !self.document),
            MenuItem::separator(),
            MenuItem::submenu(
                Menu::new("Show").items([
                    MenuItem::action("Grid", actions::ToggleGrid).checked(self.shows_grid).disabled(!self.document),
                    MenuItem::action("Guides", actions::ToggleGuides).checked(self.shows_guides).disabled(!self.document),
                ]),
            ),
            MenuItem::action("Grid Settings…", actions::GridSettings).disabled(!self.document),
            MenuItem::action("Rulers", actions::ToggleRulers).checked(self.shows_rulers).disabled(!self.document),
            MenuItem::separator(),
            MenuItem::action("Snap", actions::ToggleSnap).checked(self.snap_enabled).disabled(!self.document),
            MenuItem::submenu(
                Menu::new("Snap To").items([
                    MenuItem::action("Guides", actions::ToggleSnapToGuides)
                        .checked(self.snap_to_guides)
                        .disabled(!self.document),
                    MenuItem::action("Grid", actions::ToggleSnapToGrid)
                        .checked(self.snap_to_grid)
                        .disabled(!self.document),
                    MenuItem::action("Layers", actions::ToggleSnapToLayers)
                        .checked(self.snap_to_layers)
                        .disabled(!self.document),
                    MenuItem::action("Document Bounds", actions::ToggleSnapToDocumentBounds)
                        .checked(self.snap_to_document_bounds)
                        .disabled(!self.document),
                ]),
            ),
            MenuItem::separator(),
            MenuItem::action("Lock Guides", actions::ToggleLockGuides)
                .checked(self.locks_guides)
                .disabled(!self.document),
            MenuItem::action("Clear Guides", actions::ClearGuides).disabled(!self.can_clear_guides),
        ];
        Menu::new("View").items(items)
    }

    /// `CommandMenu("Select")`.
    fn select_menu(&self) -> Menu {
        Menu::new("Select").items([
            // Never disabled: on macOS this menu item is what binds Cmd-A to selectAll:, so switching
            // it off takes Select All away from every text field too. With no document and nothing
            // being edited the action simply does nothing.
            MenuItem::action("All", actions::SelectAll),
            MenuItem::action("Deselect", actions::Deselect).disabled(!self.selection || !self.can_edit_selection),
            MenuItem::action("Inverse", actions::InvertSelection).disabled(!self.selection || !self.can_edit_selection),
            MenuItem::action("Layer's Pixels", actions::LoadLayerSelection)
                .disabled(!self.active_asset || !self.can_edit_selection),
            MenuItem::action("Subject", actions::SelectSubject).disabled(!self.can_select_subject),
            MenuItem::action("Color Range…", actions::ColorRange).disabled(!self.can_select_color_range),
            MenuItem::action("Mask's Black Areas", actions::LoadMaskSelection)
                .disabled(!self.active_mask || !self.can_edit_selection),
            MenuItem::separator(),
            MenuItem::action("Expand…", actions::ExpandSelection).disabled(!self.can_modify_selection),
            MenuItem::action("Contract…", actions::ContractSelection).disabled(!self.can_modify_selection),
            MenuItem::action("Feather…", actions::FeatherSelection).disabled(!self.can_modify_selection),
        ])
    }

    /// `CommandMenu("Image")`.
    fn image_menu(&self) -> Menu {
        let mut items = vec![
            MenuItem::action("Curves…", actions::Curves)
                .disabled(!self.can_adjust_colors || self.hue_saturation),
            MenuItem::action("Levels…", actions::Levels)
                .disabled(!self.can_adjust_colors || self.hue_saturation),
            MenuItem::action("Hue/Saturation…", actions::HueSaturation).disabled(!self.can_adjust_colors),
        ];
        for kind in [
            FilterKind::BlackWhite,
            FilterKind::ColorBalance,
            FilterKind::Exposure,
            FilterKind::GradientMap,
            FilterKind::Grain,
        ] {
            items.push(
                MenuItem::action(format!("{}…", kind.raw_value()), actions::BeginFilter { kind })
                    .disabled(!self.can_adjust_colors || self.hue_saturation),
            );
        }
        items.push(
            MenuItem::action(self.invert_title(), actions::InvertPixels).disabled(!self.can_invert),
        );
        items.push(MenuItem::separator());
        items.push(
            MenuItem::action("Canvas Size…", actions::CanvasSize).disabled(!self.document || !self.can_start),
        );
        items.push(
            MenuItem::action("Image Size…", actions::ImageSize).disabled(!self.document || !self.can_start),
        );
        items.push(MenuItem::action("Trim…", actions::Trim).disabled(!self.document || !self.can_start));
        items.push(MenuItem::separator());
        items.push(
            MenuItem::action("Flip Canvas Horizontal", actions::FlipCanvasHorizontal).disabled(!self.can_edit_layers),
        );
        items.push(
            MenuItem::action("Flip Canvas Vertical", actions::FlipCanvasVertical).disabled(!self.can_edit_layers),
        );
        Menu::new("Image").items(items)
    }

    /// `CommandMenu("Filter")`: every kind but Content-Aware Fill and the Image menu's five.
    fn filter_menu(&self) -> Menu {
        let items = FilterKind::ALL
            .into_iter()
            .filter(|kind| *kind != FilterKind::ContentAwareFill && !kind.is_image_adjustment())
            .map(|kind| {
                let adjustable = if kind == FilterKind::Vignette { self.can_vignette } else { self.can_adjust_colors };
                MenuItem::action(format!("{}…", kind.raw_value()), actions::BeginFilter { kind })
                    .disabled(!adjustable || self.hue_saturation)
            });
        Menu::new("Filter").items(items)
    }

    /// `CommandMenu("Layer")`.
    fn layer_menu(&self) -> Menu {
        let adjustment_layer = Menu::new("New Adjustment Layer")
            .items(AdjustmentKind::ALL.into_iter().map(|kind| {
                let title = if kind.is_editable() {
                    format!("{}…", kind.raw_value())
                } else {
                    kind.raw_value().to_string()
                };
                MenuItem::action(title, actions::AddAdjustment { kind })
            }))
            .disabled(!self.can_edit_layers || !self.document);
        Menu::new("Layer").items([
            MenuItem::submenu(adjustment_layer),
            MenuItem::action("Edit Adjustment…", actions::EditAdjustment)
                .disabled(!self.can_edit_layers || !self.has_adjustment),
            MenuItem::separator(),
            MenuItem::action(self.transform_title(), actions::TransformLayerOrSelection)
                .disabled(!self.can_transform && !self.can_transform_selection),
            self.duplicate_item().disabled(!self.can_duplicate()),
            MenuItem::separator(),
            MenuItem::action(self.clipping_title(), actions::ToggleClippingMask)
                .disabled(!self.can_toggle_clipping_mask),
            MenuItem::separator(),
            MenuItem::action("Group Selected Layers", actions::GroupSelectedLayers).disabled(!self.can_edit_layers),
            MenuItem::action("Ungroup Layers", actions::UngroupLayers).disabled(!self.can_ungroup_layers),
            MenuItem::action("Move Out of Folder", actions::MoveOutOfFolder)
                .disabled(!self.can_edit_layers || !self.has_parent),
            MenuItem::action("New Blank Layer", actions::NewBlankLayer).disabled(!self.can_edit_layers),
            MenuItem::action("Rename Layer…", actions::RenameLayer)
                .disabled(!self.can_edit_layers || self.active_layer.is_none()),
            MenuItem::action(self.visibility_title(), actions::ToggleLayerVisibility)
                .disabled(!self.can_edit_layers || self.active_layer.is_none()),
            MenuItem::separator(),
            MenuItem::action("Move Layer Up", actions::MoveLayerUp).disabled(!self.can_move_up),
            MenuItem::action("Move Layer Down", actions::MoveLayerDown).disabled(!self.can_move_down),
            MenuItem::action(self.merge_title.clone(), actions::MergeLayers).disabled(!self.can_merge_layers),
            MenuItem::separator(),
            MenuItem::action("Flip Layer Horizontal", actions::FlipLayerHorizontal).disabled(!self.can_transform),
            MenuItem::action("Flip Layer Vertical", actions::FlipLayerVertical).disabled(!self.can_transform),
            MenuItem::separator(),
            MenuItem::action(self.delete_title(), actions::DeleteLayerOrMask)
                .disabled(!self.can_edit_layers || self.active_layer.is_none()),
        ])
    }

    /// The controller's `showError` busy flag: an import or modal edit keeps the history away, too.
    fn shows_busy(&self) -> bool {
        self.is_project_busy
    }

    /// The draft branch of `CommandGroup(replacing: .undoRedo)`.
    fn dialog_holds_undo(&self) -> bool {
        self.text_draft
            || self.levels
            || self.is_project_busy
            || self.shows_new_document
            || self.shows_importer
            || self.renaming_layer
            || self.transform_persistent
    }

    fn undo_title(&self) -> String {
        if self.can_undo {
            format!("Undo {}", self.undo_name)
        } else {
            "Undo".to_string()
        }
    }

    fn redo_title(&self) -> String {
        if self.can_redo {
            format!("Redo {}", self.redo_name)
        } else {
            "Redo".to_string()
        }
    }

    fn invert_title(&self) -> String {
        if self.is_mask_selected {
            "Invert Mask".to_string()
        } else {
            "Invert".to_string()
        }
    }

    fn transform_title(&self) -> String {
        if self.can_transform_selection {
            "Transform Selection".to_string()
        } else {
            "Transform Layer".to_string()
        }
    }

    /// `session.layerViaCopy()`: with a selection the item is a copy of it, without one it duplicates
    /// the layer.
    fn duplicate_item(&self) -> MenuItem {
        if self.selection {
            MenuItem::action("Layer via Copy", actions::LayerViaCopy)
        } else {
            MenuItem::action("Duplicate Layer", actions::DuplicateLayer)
        }
    }

    fn can_duplicate(&self) -> bool {
        self.can_copy_pixels || (!self.selection && self.can_edit_layers && self.active_layer.is_some())
    }

    fn clipping_title(&self) -> String {
        if self.mask_source {
            "Release Clipping Mask".to_string()
        } else {
            "Create Clipping Mask".to_string()
        }
    }

    fn visibility_title(&self) -> String {
        if self.is_visible {
            "Hide Layer".to_string()
        } else {
            "Show Layer".to_string()
        }
    }

    fn delete_title(&self) -> String {
        if let Some(kind) = self.selected_effect {
            format!("Delete {kind}")
        } else if self.is_mask_selected && self.active_mask {
            "Delete Layer Mask".to_string()
        } else if self.selected_layer_count > 1 {
            "Delete Layers".to_string()
        } else {
            "Delete Layer".to_string()
        }
    }
}
