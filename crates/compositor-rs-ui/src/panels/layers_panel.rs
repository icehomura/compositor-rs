//! The Layers panel: the header with its count, the appearance row, the layer list itself and the
//! footer's commands.
//!
//! Ported from `UI/LayersPanel.swift` together with `UI/NativeLayerList.swift`. The AppKit table is
//! a gpui list here, one row per layer or folder: the eye, the disclosure triangle, the
//! canvas-framed thumbnails, the mask's link button and the effect rows, answering the same
//! clicks — select on mouse-down, drag to reorder (Option to copy), a right-click menu, an inline
//! rename, the eye's visibility swipe, Option-click on a mask thumbnail to view the mask alone and
//! Option-drag to copy it onto another layer.
//!
//! Documented substitutions: AppKit's proposed drop operation is the pointer's half of a folder's
//! row (the lower half drops into it), and the Option that makes a drop a copy is read when the
//! drop lands rather than from the drag's operation mask.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::Hash as _;
use std::rc::Rc;
use std::sync::Arc;

use compositor_rs_core::document::{CanvasDocument, ImageLayer};
use compositor_rs_core::geom::Size;
use compositor_rs_core::groups::LayerHierarchyEntry;
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::layer_adjustment::AdjustmentKind;
use compositor_rs_core::layer_effects::LayerEffectKind;
use compositor_rs_core::layer_transform::LayerTransform;
use compositor_rs_core::selection::SelectionMode;
use compositor_rs_core::{Id, Rgba8Image};
use compositor_rs_io::image_exporter::{shared, ExportRaster, ImageExporter};
use compositor_rs_session::EditorSession;

use crate::canvas::thumbnail::CanvasThumbnail;
use crate::panels::layer_appearance::LayerAppearanceControls;
use crate::panels::layer_mask_menu::LayerMaskMenu;

use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

/// The panel's narrowest and widest frames (`LayersPanel.widths`).
pub const WIDTHS: std::ops::RangeInclusive<f64> = 202.0..=352.0;
/// A row's height (`table.rowHeight = 52`).
const ROW_HEIGHT: f32 = 52.0;
/// The extra height each effect adds (`52 + effects.kinds.count * 24`).
const EFFECT_ROW_HEIGHT: f32 = 24.0;
/// How far a folder's contents step in, per level (`min(depth, 8) * 24`).
const INDENT: f32 = 24.0;
/// How far a clipped layer steps in (`+ 24`).
const CLIP_INDENT: f32 = 24.0;
/// The layer thumbnail's box (a 36-point slot).
const THUMB_BOX: f32 = 36.0;
/// The mask thumbnail's box (a 30-point slot).
const MASK_THUMB_BOX: f32 = 30.0;
/// Option-click clips along the bottom edge of a row: a fixed strip, not a share of the row's
/// height.
const CLIPPING_STRIP: f32 = 10.0;
/// `footerHitArea()`: the padding is the clickable area.
const FOOTER_HIT_X: f32 = 8.0;
const FOOTER_HIT_Y: f32 = 12.0;

/// The Layers panel.
pub struct LayersPanel {
    session: Entity<EditorSession>,
    /// The thumbnails already drawn, by what they show (`LayerCell`'s thumbnail keys).
    thumbnails: Rc<RefCell<HashMap<ThumbnailKey, Arc<RenderImage>>>>,
    /// The row being renamed, its field, and whether it had the keyboard last frame.
    rename: Option<(Id, Entity<InputState>, bool)>,
    /// The visibility swipe in progress: the state every eye passed over takes (`EyeSwipeButton`).
    swipe: Option<(Id, bool)>,
    /// The row a drop would land on, and whether it would fall into that row's folder.
    drop_target: Option<(Id, bool)>,
    /// The row a Shift-click extends from.
    selection_anchor: Option<Id>,
}

impl LayersPanel {
    pub fn new(session: Entity<EditorSession>) -> Self {
        Self {
            session,
            thumbnails: Rc::new(RefCell::new(HashMap::new())),
            rename: None,
            swipe: None,
            drop_target: None,
            selection_anchor: None,
        }
    }

    /// `LayersPanel.widths`: dragging the panel's left edge sets it, within this range.
    pub fn widths() -> std::ops::RangeInclusive<f64> {
        WIDTHS
    }

    /// `ImageLayer.sizeLabel`: the layer's size on the canvas and, once it's scaled, by how much. A
    /// photo shrunk to 5% keeps every one of its pixels; the percentage says so, where the size
    /// alone reads as if it had been resampled small.
    fn size_label(layer: &ImageLayer) -> String {
        let text = format!(
            "{} × {} px",
            layer.transform.size.width.round() as i64,
            layer.transform.size.height.round() as i64
        );
        let Some(pixels) = layer
            .asset
            .as_ref()
            .map(|asset| asset.image.width())
            .filter(|width| *width > 0)
        else {
            return text;
        };
        // Measured across the width, as the Transform bar's Scale field is.
        let percent = layer.transform.size.width / pixels as f64 * 100.0;
        if (percent - 100.0).abs() < 0.05 {
            return text;
        }
        // `.number.precision(.fractionLength(0...1))`: no decimals on a whole percent, one otherwise.
        let formatted = if (percent - percent.round()).abs() < 0.05 {
            format!("{}", percent.round() as i64)
        } else {
            format!("{percent:.1}")
        };
        text + " · " + &formatted + "%"
    }

    /// Every row the list shows, read out of the session before anything is drawn.
    fn rows(&self, cx: &App) -> Vec<RowData> {
        let session = self.session.read(cx);
        let Some(document) = session.document.as_ref() else {
            return Vec::new();
        };
        let by_id: HashMap<Id, &ImageLayer> = document
            .layers
            .iter()
            .map(|layer| (layer.id, layer))
            .collect();
        let rows = session.layer_rows();
        rows.iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let layer = *by_id.get(&entry.layer.id)?;
                Some(RowData::read(index, entry, layer, document, session))
            })
            .collect()
    }

    /// A row's thumbnail, drawn once per change (`updateNSView`'s thumbnail keys).
    #[allow(clippy::too_many_arguments)]
    fn thumbnail(
        &self,
        key: ThumbnailKey,
        image: Option<&PixelImage>,
        mask: bool,
        transform: &LayerTransform,
        canvas: Size,
        box_: f64,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        if let Some(hit) = self.thumbnails.borrow().get(&key) {
            return Some(hit.clone());
        }
        let rgba: Rgba8Image = if mask {
            CanvasThumbnail::mask(image?, transform, canvas, box_)
        } else {
            CanvasThumbnail::layer(image, transform, canvas, box_)
        };
        // GPUI paints `RenderImage`s; the pixels reach it as PNG, the one image format the toolkit
        // takes without an image-crate dependency.
        let raster = ExportRaster::new(shared(rgba));
        let bytes = ImageExporter::png_data(&raster).ok()?;
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
        let render_image = image.use_render_image(window, cx)?;
        let mut cache = self.thumbnails.borrow_mut();
        // A panel's worth of rows is small; a long session rebuilds rather than growing forever.
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(key, render_image.clone());
        Some(render_image)
    }

    /// `tableViewSelectionDidChange` and the table's own mouse-down: what a click on a row selects.
    fn select_row(&mut self, id: Id, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let (selected, contains) = {
            let session = self.session.read(cx);
            (
                session.selected_layer_ids.iter().copied().collect::<Vec<_>>(),
                session.selected_layer_ids.contains(&id),
            )
        };
        if event.modifiers.platform {
            // Cmd-click toggles; a list keeps at least the clicked row.
            let mut next: Vec<Id> = selected
                .iter()
                .copied()
                .filter(|candidate| *candidate != id)
                .collect();
            if !contains {
                next.push(id);
            }
            if next.is_empty() {
                next.push(id);
            }
            let primary = next.contains(&id).then_some(id).or_else(|| next.first().copied());
            self.session
                .update(cx, |session, _| session.select_layers(next, primary));
        } else if event.modifiers.shift {
            if let Some(anchor) = self.selection_anchor {
                let rows = self.rows(cx);
                if let (Some(from), Some(to)) = (
                    rows.iter().position(|row| row.id == anchor),
                    rows.iter().position(|row| row.id == id),
                ) {
                    let (from, to) = if from <= to { (from, to) } else { (to, from) };
                    let range: Vec<Id> = rows[from..=to].iter().map(|row| row.id).collect();
                    self.session
                        .update(cx, |session, _| session.select_layers(range, Some(anchor)));
                    return;
                }
            }
            self.session.update(cx, |session, _| session.select_layer(Some(id)));
        } else {
            self.session.update(cx, |session, _| session.select_layer(Some(id)));
        }
        self.selection_anchor = Some(id);
    }

    /// `clickedLayer(_:)`: a click on a row's name targets the layer itself, even when its mask was
    /// selected — so transforming then moves layer and mask together.
    fn click_name(&mut self, id: Id, cx: &mut Context<Self>) {
        let targets_layer = {
            let session = self.session.read(cx);
            session.is_mask_selected
                && session.selected_layer_ids.len() == 1
                && session.selected_layer_ids.contains(&id)
        };
        if targets_layer {
            self.session.update(cx, |session, _| {
                session.commit_transform();
                session.select_layer_target(id, false);
            });
        }
    }

    /// `renameClickedLayer(_:)`: on a thumbnail a text or adjustment layer opens what it holds; on
    /// the name the layer is renamed.
    fn double_click(&mut self, row: &RowData, on_control: bool, cx: &mut Context<Self>) {
        if on_control {
            if row.is_text {
                self.session.update(cx, |session, _| session.edit_active_text());
                return;
            }
            if row
                .adjustment
                .as_ref()
                .is_some_and(|kind| kind.is_editable())
            {
                self.session.update(cx, |session, _| {
                    session.adjustment_editing_id = Some(row.id)
                });
                return;
            }
        }
        if !self.session.read(cx).can_edit_layers() {
            return;
        }
        self.session.update(cx, |session, _| {
            session.active_layer_id = Some(row.id);
            session.renaming_layer_id = Some(row.id);
        });
    }

    /// `place(_:at:intoFolder:copying:)`, through the session's own command.
    fn place(&mut self, ids: &[Id], row: usize, into_folder: bool, copying: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, _| session.place_layers(ids, row, into_folder, copying));
    }

    /// `draggedLayers(_:)`: every layer being dragged, leaving out anything inside a dragged folder,
    /// which the folder brings along itself.
    fn dragged_layers(&self, ids: &[Id], cx: &App) -> Vec<Id> {
        let session = self.session.read(cx);
        let dragged: HashSet<Id> = ids.iter().copied().collect();
        let mut carried: HashSet<Id> = HashSet::new();
        for id in &dragged {
            carried.extend(session.descendant_ids(*id));
        }
        session
            .layer_rows()
            .iter()
            .map(|entry| entry.layer.id)
            .filter(|id| dragged.contains(id) && !carried.contains(id))
            .collect()
    }

    /// `endRenaming(keeping:)`: the typed name is kept or dropped, and the field put away.
    fn end_rename(&mut self, keeping: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, field, _)) = self.rename.take() else {
            return;
        };
        let name = field.read(cx).value().to_string();
        self.session.update(cx, |session, _| {
            if keeping {
                session.rename_layer(id, &name);
            }
            if session.renaming_layer_id == Some(id) {
                session.renaming_layer_id = None;
            }
        });
        // Hand the keyboard back, so tool shortcuts work straight away.
        window.blur(cx);
    }

    /// The right-click menu (`contextMenu(for:)`).
    fn context_menu(menu: PopupMenu, entity: Entity<Self>, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
        let session = entity.read(cx).session.clone();
        let rows = entity.read(cx).rows(cx);
        let active_id = session.read(cx).active_layer_id;
        let row = active_id.and_then(|id| rows.into_iter().find(|row| row.id == id));
        let Some(row) = row else {
            return menu;
        };

        let duplicate = {
            let session = session.clone();
            PopupMenuItem::new("Duplicate Layer")
                .disabled(!(session.read(cx).can_edit_layers() && session.read(cx).active_layer().is_some()))
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.duplicate_active_layer()))
        };
        let rename = {
            let session = session.clone();
            let row_id = row.id;
            PopupMenuItem::new("Rename…")
                .disabled(!{
                    let session = session.read(cx);
                    session.can_edit_layers() && session.active_layer().is_some() && session.selected_layer_ids.len() == 1
                })
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, _| session.renaming_layer_id = Some(row_id))
                })
        };
        let delete_title = {
            let session = session.read(cx);
            if session.is_mask_selected && session.active_layer().is_some_and(|layer| layer.mask.is_some()) {
                "Delete Mask"
            } else if session.selected_layer_ids.len() > 1 {
                "Delete Selected Layers"
            } else {
                "Delete Layer"
            }
        };
        let delete = {
            let session = session.clone();
            PopupMenuItem::new(delete_title)
                .disabled(!(session.read(cx).can_edit_layers() && session.read(cx).active_layer().is_some()))
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.delete_layer_or_mask()))
        };
        let clipping = {
            let session = session.clone();
            let row_id = row.id;
            let releasing = row.mask_source_id.is_some();
            PopupMenuItem::new(if releasing { "Release Clipping Mask" } else { "Create Clipping Mask" })
                .disabled(!session.read(cx).can_toggle_clipping_mask(row_id))
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.toggle_clipping_mask(row_id)))
        };
        let group = {
            let session = session.clone();
            PopupMenuItem::new("Group Selected Layers")
                .disabled(!{
                    let session = session.read(cx);
                    session.can_edit_layers()
                        && session.document.is_some()
                        && session.document.as_ref().map(|document| document.layers.len()).unwrap_or(0) < 10_000
                        && !session.selected_layer_ids.is_empty()
                })
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.group_selected_layers()))
        };
        let ungroup = {
            let session = session.clone();
            PopupMenuItem::new("Ungroup Layers")
                .disabled(!session.read(cx).can_ungroup_layers())
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.ungroup_layers()))
        };
        let move_out = {
            let session = session.clone();
            PopupMenuItem::new("Move Out of Folder")
                .disabled(!{
                    let session = session.read(cx);
                    session.can_edit_layers() && session.active_layer().is_some_and(|layer| layer.parent_id.is_some())
                })
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.move_active_layer_out_of_group()))
        };
        let merge = {
            let session = session.clone();
            PopupMenuItem::new(session.read(cx).merge_title())
                .disabled(!session.read(cx).can_merge_layers())
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.merge_layers()))
        };
        let adding_mask = session.read(cx).can_edit_mask()
            && session
                .read(cx)
                .active_layer()
                .is_some_and(|layer| layer.mask.is_none());
        let mask_disabled = !session.read(cx).can_edit_mask()
            || session
                .read(cx)
                .active_layer()
                .is_some_and(|layer| layer.mask.is_none());
        let toggle_mask_title = if row.mask_enabled { "Disable Mask" } else { "Enable Mask" };
        let toggle_mask = {
            let session = session.clone();
            let row_id = row.id;
            PopupMenuItem::new(toggle_mask_title)
                .disabled(mask_disabled)
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, _| {
                        session.select_layer_target(row_id, false);
                        session.toggle_layer_mask();
                    })
                })
        };
        let delete_mask = {
            let session = session.clone();
            let row_id = row.id;
            PopupMenuItem::new("Delete Mask")
                .disabled(mask_disabled)
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, _| {
                        session.select_layer_target(row_id, false);
                        session.delete_layer_mask();
                    })
                })
        };
        let link_mask_title = if row.mask_linked { "Unlink Mask" } else { "Link Mask" };
        let link_mask = {
            let session = session.clone();
            let row_id = row.id;
            PopupMenuItem::new(link_mask_title)
                .disabled(!{
                    let session = session.read(cx);
                    session.can_edit_layers()
                        && session.active_layer().is_some_and(|layer| {
                            layer.mask.is_some() && !layer.is_group && layer.adjustment.is_none()
                        })
                })
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.toggle_mask_link(row_id)))
        };
        let visibility_title = if row.visible { "Hide Layer" } else { "Show Layer" };
        let visibility = {
            let session = session.clone();
            let row_id = row.id;
            PopupMenuItem::new(visibility_title)
                .disabled(!(session.read(cx).can_edit_layers() && session.read(cx).active_layer().is_some()))
                .on_click(move |_, _, cx| session.update(cx, |session, _| session.toggle_layer_visibility(row_id)))
        };

        let menu = menu
            .item(duplicate)
            .item(rename)
            .item(delete)
            .separator()
            .item(clipping)
            .item(group);
        let menu = if row.is_group { menu.item(ungroup) } else { menu };
        let row_id = row.id;
        menu.item(move_out)
            .item(merge)
            .separator()
            .submenu("Add Mask", window, cx, move |menu, _, _| {
                // The submenu's builder runs whenever it opens, so the two items are made here:
                // a `PopupMenuItem` cannot be built once and moved into an `Fn`.
                let reveal = PopupMenuItem::new("Reveal All (White)")
                    .disabled(!adding_mask)
                    .on_click({
                        let session = session.clone();
                        move |_, _, cx| {
                            session.update(cx, |session, _| {
                                session.select_layer_target(row_id, false);
                                session.add_mask(true);
                            })
                        }
                    });
                let hide = PopupMenuItem::new("Hide All (Black)")
                    .disabled(!adding_mask)
                    .on_click({
                        let session = session.clone();
                        move |_, _, cx| {
                            session.update(cx, |session, _| {
                                session.select_layer_target(row_id, false);
                                session.add_mask(false);
                            })
                        }
                    });
                menu.item(reveal).item(hide)
            })
            .item(toggle_mask)
            .item(delete_mask)
            .item(link_mask)
            .separator()
            .item(visibility)
    }

    /// The layer thumbnail: a canvas-framed picture, or the folder, text or adjustment symbol the
    /// Swift drew into the button (`LayerCell.configure`).
    fn layer_thumbnail(&self, row: &RowData, canvas: Size, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let box_size = THUMB_BOX;
        let fitted = CanvasThumbnail::fitted_size(canvas, f64::from(box_size));
        let (width, height) = if row.framed() {
            (fitted.width as f32, fitted.height as f32)
        } else {
            (box_size, box_size)
        };
        let key = ThumbnailKey {
            image: row.asset.as_ref().map(pixel_identity),
            transform: row.transform,
            canvas,
            box_: box_size as i32,
            mask: false,
            editable_text: row.is_text,
        };
        let picture = if row.adjustment.is_some() || row.is_group || row.is_text {
            None
        } else {
            self.thumbnail(
                key,
                row.asset.as_ref(),
                false,
                &row.transform,
                canvas,
                f64::from(box_size),
                window,
                cx,
            )
        };
        let accent = cx.theme().primary;
        let border = if row.active && !row.mask_selected {
            2.0
        } else {
            0.0
        };
        let entity = cx.entity();
        let id = row.id;

        let is_text = row.is_text;

        div()
            .id(panel_key("layer-thumb", id))
            .flex_none()
            .w(px(box_size))
            .h(px(box_size))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .when(border > 0.0, |this| {
                this.border(px(border)).border_color(accent)
            })
            .cursor(CursorStyle::PointingHand)
            .tooltip({
                // `configure` overwrites the cell's initial string, so this is the tooltip a hover shows.
                let help = if is_text {
                    "Editable text layer"
                } else {
                    "Select image pixels"
                };
                move |window, cx| Tooltip::new(help).build(window, cx)
            })
            .aria_label(format!("Select {}: {}", if is_text { "text" } else { "image" }, row.name))
            .child(match picture {
                Some(image) => img(image)
                    .w(px(width))
                    .h(px(height))
                    .into_any_element(),
                None => {
                    let icon = if let Some(kind) = row.adjustment.as_ref() {
                        adjustment_icon(*kind)
                    } else if row.is_group {
                        IconName::Folder
                    } else {
                        IconName::Type
                    };
                    Icon::new(icon).size(px(box_size * 0.8)).into_any_element()
                }
            })
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
                // Cmd-click on a thumbnail loads a selection; elsewhere in the row it multi-selects.
                let mode = load_mode(event.modifiers);
                entity.update(cx, |panel, cx| {
                    if event.modifiers.platform {
                        panel.session.update(cx, |session, _| {
                            session.load_layer_selection(id, mode)
                        });
                        return;
                    }
                    if event.modifiers.shift {
                        return;
                    }
                    panel.session.update(cx, |session, _| {
                        session.select_layer_target(id, false)
                    });
                });
                cx.stop_propagation();
            })
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .into_any_element()
    }

    /// The mask thumbnail: selecting it targets the mask, Shift-click enables or disables it, and
    /// Cmd-click loads its black areas as a selection.
    fn mask_thumbnail(&self, row: &RowData, canvas: Size, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let box_size = MASK_THUMB_BOX;
        let fitted = CanvasThumbnail::fitted_size(canvas, f64::from(box_size));
        let key = ThumbnailKey {
            image: row.mask.as_ref().map(pixel_identity),
            transform: row.mask_transform,
            canvas,
            box_: box_size as i32,
            mask: true,
            editable_text: false,
        };
        let picture = row.mask.as_ref().and_then(|mask| {
            self.thumbnail(
                key,
                Some(mask),
                true,
                &row.mask_transform,
                canvas,
                f64::from(box_size),
                window,
                cx,
            )
        });
        let accent = cx.theme().primary;
        let entity = cx.entity();
        let id = row.id;
        let alone = row.mask_alone;
        let enabled = row.mask_enabled;
        // `maskThumbnail.isHidden = layer.mask == nil`: the slot is empty without a mask.
        if row.mask.is_none() {
            return div().into_any_element();
        }

        div()
            .id(panel_key("layer-mask-thumb", id))
            .relative()
            .flex_none()
            .w(px(box_size))
            .h(px(box_size))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .when(row.active && row.mask_selected, |this| {
                this.border_2().border_color(if alone {
                    hsla(0.0, 0.0, 1.0, 1.0)
                } else {
                    accent
                })
            })
            .cursor(CursorStyle::PointingHand)
            .tooltip(|window, cx| {
                Tooltip::new("Select layer mask; Option-click to view it alone; Shift-click to enable/disable; Cmd-click to select its black areas (Cmd-Shift adds, Cmd-Option subtracts)").build(window, cx)
            })
            .aria_label(format!("Select mask: {}", row.name))
            .when_some(picture, |this, image| {
                this.child(
                    img(image)
                        .w(px(fitted.width as f32))
                        .h(px(fitted.height as f32)),
                )
            })
            // A disabled mask shows `╱` over it.
            .when(!enabled, |this| {
                this.child(
                    div()
                        .absolute()
                        .text_size(px(32.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(hsla(0.0, 1.0, 0.5, 1.0))
                        .child("╱"),
                )
            })
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
                let mode = load_mode(event.modifiers);
                entity.update(cx, |panel, cx| {
                    if event.modifiers.alt && !event.modifiers.platform {
                        // A click without a drag shows the mask alone (an Option-drag copies it).
                        panel.session.update(cx, |session, _| session.toggle_mask_alone(id));
                        return;
                    }
                    if event.modifiers.platform {
                        panel
                            .session
                            .update(cx, |session, _| session.load_mask_selection(id, mode));
                        return;
                    }
                    panel.session.update(cx, |session, _| {
                        session.select_layer_target(id, true)
                    });
                    if event.modifiers.shift {
                        panel.session.update(cx, |session, _| session.toggle_layer_mask());
                    }
                });
                cx.stop_propagation();
            })
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_drag(MaskDrag { layer_id: id }, |_, _, _, cx| cx.new(|_| EmptyGhost))
            .into_any_element()
    }

    /// The chain between the two thumbnails: linked, layer and mask move together; unlinked, each
    /// transforms on its own.
    fn link_button(&self, row: &RowData, cx: &mut Context<Self>) -> AnyElement {
        let linkable = row.mask.is_some() && row.adjustment.is_none() && !row.is_group;
        // `linkButton.isHidden = !linkable`: an unlinkable row's chain is not drawn.
        if !linkable {
            return div().into_any_element();
        }
        let linked = row.mask_linked;
        let entity = cx.entity();
        let id = row.id;
        div()
            .id(panel_key("layer-mask-link", id))
            .flex_none()
            .w(px(9.0))
            .h(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(hsla(0.0, 0.0, 1.0, 0.55))
            .cursor(CursorStyle::PointingHand)
            .tooltip(move |window, cx| {
                Tooltip::new(if linked {
                    "Unlink layer and mask to move or transform them separately"
                } else {
                    "Link layer and mask so they move together"
                })
                .build(window, cx)
            })
            .aria_label(format!(
                "{} mask: {}",
                if linked { "Unlink" } else { "Link" },
                row.name
            ))
            // The chain is drawn while the two are linked; an unlinked pair keeps the clickable gap.
            .when(linked, |this| {
                this.child(
                    // `linkImage`: the chain runs corner to corner, turned 45° it stands upright.
                    Icon::new(IconName::Link)
                        .size(px(10.0))
                        .rotate(Radians(std::f32::consts::FRAC_PI_4)),
                )
            })
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.stop_propagation();
                entity.update(cx, |panel, cx| {
                    panel
                        .session
                        .update(cx, |session, _| session.toggle_mask_link(id));
                });
            })
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .into_any_element()
    }
}

/// A row's element id: gpui's ids are one name plus a payload, so a row's id is spelled out.
fn panel_key(name: &str, id: Id) -> ElementId {
    ElementId::Name(format!("{name}:{id}").into())
}

/// The backing pixels' identity, so a thumbnail is redrawn only when its picture changes.
fn pixel_identity(image: &PixelImage) -> usize {
    match image {
        PixelImage::Rgba(image) => Arc::as_ptr(image) as usize,
        PixelImage::Gray(image) => Arc::as_ptr(image) as usize,
    }
}

/// `SelectionMode` from the click's modifiers: Cmd-Shift adds, Cmd-Option subtracts, Cmd replaces.
fn load_mode(modifiers: Modifiers) -> SelectionMode {
    if modifiers.alt {
        SelectionMode::Subtract
    } else if modifiers.shift {
        SelectionMode::Add
    } else {
        SelectionMode::Replace
    }
}

/// The SF symbol each adjustment kind's row shows, in the port's icon set.
fn adjustment_icon(kind: AdjustmentKind) -> IconName {
    match kind.symbol() {
        "point.topleft.down.to.point.bottomright.curvepath" => IconName::Spline,
        "slider.horizontal.3" => IconName::SlidersHorizontal,
        "circle.lefthalf.filled" => IconName::Contrast,
        "plusminus.circle" => IconName::CirclePlus,
        "paintpalette" => IconName::Palette,
        "circle.grid.3x3" => IconName::Grid3x3,
        "drop.fill" => IconName::Droplet,
        "wind" => IconName::Wind,
        "circle.dotted" => IconName::CircleDotDashed,
        "circle.righthalf.filled" => IconName::SunMoon,
        "circle.filled.pattern.diagonalline.rectangle" => IconName::SquareSlash,
        "scale.3d" => IconName::SlidersVertical,
        _ => IconName::SlidersHorizontal,
    }
}

/// One row's data, read from the session before the row is drawn.
#[derive(Clone)]
struct RowData {
    index: usize,
    id: Id,
    name: String,
    depth: usize,
    visible: bool,
    is_group: bool,
    collapsed: bool,
    adjustment: Option<AdjustmentKind>,
    is_text: bool,
    mask_source_id: Option<Id>,
    dimensions: String,
    asset: Option<PixelImage>,
    mask: Option<PixelImage>,
    mask_enabled: bool,
    mask_linked: bool,
    transform: LayerTransform,
    mask_transform: LayerTransform,
    effects: Vec<(LayerEffectKind, bool)>,
    active: bool,
    mask_selected: bool,
    mask_alone: bool,
}

impl RowData {
    fn read(
        index: usize,
        entry: &LayerHierarchyEntry,
        layer: &ImageLayer,
        document: &CanvasDocument,
        session: &EditorSession,
    ) -> Self {
        let record = &entry.layer;
        let is_text = record.text.is_some();
        let dimensions = if is_text {
            "Text · Double-click to edit".to_string()
        } else if record.adjustment.is_some() {
            "Adjustment · Double-click to edit".to_string()
        } else if record.is_group == Some(true) {
            "Folder".to_string()
        } else if let Some(source) = record.mask_source_id {
            let source_name = document
                .layers
                .iter()
                .find(|layer| layer.id == source)
                .map(|layer| layer.name.clone())
                .unwrap_or_else(|| "Missing source".to_string());
            format!("Clipped to {source_name}")
        } else {
            LayersPanel::size_label(layer)
        };
        Self {
            index,
            id: record.id,
            name: record.name.clone(),
            depth: entry.depth,
            visible: entry.visible,
            is_group: record.is_group == Some(true),
            collapsed: session.collapsed_group_ids.contains(&record.id),
            adjustment: record.adjustment.as_ref().map(|adjustment| adjustment.kind),
            is_text,
            mask_source_id: record.mask_source_id,
            dimensions,
            asset: layer.asset.as_ref().map(|asset| asset.thumbnail.clone()),
            mask: layer.mask.as_ref().map(|mask| mask.asset.thumbnail.clone()),
            mask_enabled: layer.mask.as_ref().map(|mask| mask.is_enabled).unwrap_or(true),
            mask_linked: layer.mask.as_ref().map(|mask| mask.is_linked).unwrap_or(true),
            transform: record.transform,
            mask_transform: layer
                .mask
                .as_ref()
                .and_then(|mask| mask.placement)
                .unwrap_or(record.transform),
            effects: record
                .effects
                .as_ref()
                .map(|effects| {
                    effects
                        .kinds()
                        .into_iter()
                        .map(|kind| (kind, effects.is_enabled(kind)))
                        .collect()
                })
                .unwrap_or_default(),
            active: session.active_layer_id == Some(record.id) && session.selected_layer_ids.len() == 1,
            mask_selected: session.is_mask_selected,
            mask_alone: session.mask_alone_layer().map(|layer| layer.id) == Some(record.id),
        }
    }

    /// The row's step-in: a folder steps its contents in by the same distance a clipping mask does;
    /// the two add up.
    fn indent(&self) -> f32 {
        (self.depth.min(8) as f32) * INDENT
            + if self.mask_source_id.is_none() { 0.0 } else { CLIP_INDENT }
    }

    /// Pixel layers and masks show the whole canvas with their pixels where they sit; editable text,
    /// adjustments and folders keep a square icon (`framed`).
    fn framed(&self) -> bool {
        self.adjustment.is_none() && !self.is_group && !self.is_text
    }
}

/// What a row's canvas-framed thumbnail shows, so it is redrawn only when one of these changes
/// (`ThumbnailKey`).
#[derive(Clone, PartialEq)]
struct ThumbnailKey {
    image: Option<usize>,
    transform: LayerTransform,
    canvas: Size,
    box_: i32,
    mask: bool,
    editable_text: bool,
}

// The key goes into a `HashMap`, whose lookup needs `Eq` and `Hash`; the transform and the canvas
// are `f64`s, so their bit patterns are what is compared and hashed.
impl Eq for ThumbnailKey {}

impl std::hash::Hash for ThumbnailKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.image.hash(state);
        for value in [
            self.transform.origin.x,
            self.transform.origin.y,
            self.transform.size.width,
            self.transform.size.height,
            self.transform.rotation,
            self.canvas.width,
            self.canvas.height,
        ] {
            value.to_bits().hash(state);
        }
        self.transform.flip_x.hash(state);
        self.transform.flip_y.hash(state);
        self.transform.sampling.hash(state);
        self.box_.hash(state);
        self.mask.hash(state);
        self.editable_text.hash(state);
    }
}

/// A row's drag: the layers it carries (the Swift table wrote one pasteboard item per selected row).
#[derive(Clone)]
struct LayerDrag {
    ids: Vec<Id>,
    name: SharedString,
}

/// An effect's Option-drag, onto another layer (`layerID + ":" + kind.rawValue`).
#[derive(Clone)]
struct EffectDrag {
    layer_id: Id,
    kind: LayerEffectKind,
}

/// A mask thumbnail's Option-drag: the id of the layer whose mask is being copied.
#[derive(Clone)]
struct MaskDrag {
    layer_id: Id,
}

/// The eye's visibility swipe; the state every eye passed over takes is the panel's
/// (`session.beginVisibilitySwipe`).
#[derive(Clone)]
struct VisibilitySwipe;

/// The ghost under the pointer while a row is dragged.
struct DragGhost {
    name: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .bg(hsla(0.0, 0.0, 0.2, 0.9))
            .text_size(px(11.0))
            .child(self.name.clone())
    }
}

/// A drag with nothing to show under the pointer (an effect's or a mask's copy).
struct EmptyGhost;

impl Render for EmptyGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl LayersPanel {
    /// The row's whole cell: the eye, the disclosure, the thumbnails, the names and effect rows.
    fn row(&mut self, row: &RowData, canvas: Size, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let session = self.session.clone();
        let entity = cx.entity();
        let indent = row.indent();
        let height = ROW_HEIGHT + row.effects.len() as f32 * EFFECT_ROW_HEIGHT;
        let renaming = self.session.read(cx).renaming_layer_id == Some(row.id);
        let can_edit_layers = self.session.read(cx).can_edit_layers();
        let selected_effect = self.session.read(cx).selected_effect();


        // The rename is typed in the row itself (`beginRenaming`), in a field keyed to the layer so
        // a reused row never carries another row's half-finished rename.
        let rename = if renaming {
            let field = match &self.rename {
                Some((id, field, _)) if *id == row.id => Some(field.clone()),
                _ => None,
            };
            let field = field.unwrap_or_else(|| {
                window.use_keyed_state(panel_key("layer-rename", row.id), cx, |window, cx| {
                    InputState::new(window, cx)
                })
            });
            if self.rename.as_ref().map(|(id, _, _)| *id) != Some(row.id) {
                let name = row.name.clone();
                field.update(cx, |state, cx| state.set_value(name, window, cx));
                let handle = field.read(cx).focus_handle(cx).clone();
                window.focus(&handle, cx);
                field.update(cx, |state, cx| state.select_all(window, cx));
                self.rename = Some((row.id, field.clone(), true));
            }
            Some(field)
        } else {
            None
        };

        // The eye: pressing it begins a visibility swipe, which every eye passed over follows.
        let eye = div()
            .id(panel_key("layer-eye", row.id))
            .flex_none()
            .w(px(20.0))
            .h(px(32.0))
            .flex()
            .items_center()
            .justify_center()
            .when(can_edit_layers, |this| this.cursor(CursorStyle::PointingHand))
            .when(!can_edit_layers, |this| this.opacity(0.5))
            .aria_label(format!(
                "{} {}",
                if row.visible { "Hide" } else { "Show" },
                row.name
            ))
            .child(
                Icon::new(if row.visible { IconName::Eye } else { IconName::EyeOff })
                    .size(px(13.0)),
            )
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                let session = session.clone();
                let id = row.id;
                move |_, _, cx| {
                    if !session.read(cx).can_edit_layers() {
                        return;
                    }
                    let started = session.update(cx, |session, _| session.begin_visibility_swipe(id));
                    entity.update(cx, |panel, cx| {
                        panel.swipe = started.map(|visible| (id, visible));
                        cx.notify();
                    });
                }
            })
            .on_drag(VisibilitySwipe, |_, _, _, cx| cx.new(|_| EmptyGhost))
            .on_mouse_up(MouseButton::Left, {
                let session = session.clone();
                let entity = entity.clone();
                move |_, _, cx| {
                    session.update(cx, |session, _| session.end_visibility_swipe());
                    entity.update(cx, |panel, _| panel.swipe = None);
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                let session = session.clone();
                let entity = entity.clone();
                move |_, _, cx| {
                    session.update(cx, |session, _| session.end_visibility_swipe());
                    entity.update(cx, |panel, _| panel.swipe = None);
                }
            });

        // The disclosure triangle: a folder's contents fold away (`toggleExpansion`).
        let disclosure = div()
            .flex_none()
            .w(px(16.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .when(row.is_group, |this| {
                this.cursor(CursorStyle::PointingHand).child(
                    Icon::new(if row.collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(px(10.0)),
                )
            })
            .on_mouse_down(MouseButton::Left, {
                let session = session.clone();
                let id = row.id;
                let is_group = row.is_group;
                move |_, _, cx| {
                    if is_group {
                        session.update(cx, |session, _| session.toggle_group_expansion(id));
                    }
                    cx.stop_propagation();
                }
            });

        let thumbnail = self.layer_thumbnail(row, canvas, window, cx);
        let mask_thumbnail = self.mask_thumbnail(row, canvas, window, cx);
        let link = self.link_button(row, cx);
        // `linkable`: a mask on a plain layer, not a folder or an adjustment.
        let linkable = row.mask.is_some() && row.adjustment.is_none() && !row.is_group;

        let clipper = row.mask_source_id.map(|_| div().flex_none().child("↳ "));
        let name: AnyElement = if let Some(field) = rename {
            let key_entity = entity.clone();
            let id = row.id;
            div()
                .flex_1()
                .min_w(px(0.0))
                .capture_key_down(move |event: &KeyDownEvent, window, cx| match event.keystroke.key.as_str() {
                    "enter" => {
                        key_entity.update(cx, |panel, cx| panel.end_rename(true, window, cx));
                        cx.stop_propagation();
                    }
                    "escape" => {
                        key_entity.update(cx, |panel, cx| panel.end_rename(false, window, cx));
                        cx.stop_propagation();
                    }
                    _ => {}
                })
                .child(Input::new(&field))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_row()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(13.0))
                .children(clipper)
                .child(row.name.clone())
                .into_any_element()
        };
        let dimensions = div()
            .text_size(px(10.0))
            .text_color(hsla(0.0, 0.0, 1.0, 0.55))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(row.dimensions.clone())
            .into_any_element();

        let main = h_flex()
            .h(px(ROW_HEIGHT))
            .w_full()
            .items_center()
            .pl(px(8.0))
            .child(eye)
            .child(disclosure)
            .child(
                div()
                    .flex_none()
                    .w(px(THUMB_BOX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(thumbnail),
            )
            .child(
                // `maskGap`: 13 points while the chain can sit between the thumbnails, 5 otherwise.
                div()
                    .flex_none()
                    .w(px(if linkable { 13.0 } else { 5.0 }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(link),
            )
            .child(
                div()
                    .flex_none()
                    // `maskWidth`: the mask's slot is empty without a mask.
                    .w(px(if row.mask.is_some() { MASK_THUMB_BOX } else { 0.0 }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(mask_thumbnail),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .pr(px(8.0))
                    .pt(px(9.0))
                    .child(name)
                    .child(div().pt(px(3.0)).child(dimensions)),
            );

        let effects: Vec<AnyElement> = row
            .effects
            .iter()
            .map(|(kind, enabled)| {
                let kind = *kind;
                let enabled = *enabled;
                let row_id = row.id;
                let effect_entity = entity.clone();
                let effect_session = session.clone();
                let selected = selected_effect
                    .as_ref()
                    .is_some_and(|selection| selection.layer_id == row.id && selection.kind == kind);
                h_flex()
                    .id(ElementId::Name(format!("layer-effect-{}:{}", row.id, kind.raw_value()).into()))
                    .h(px(EFFECT_ROW_HEIGHT))
                    .w_full()
                    .items_center()
                    .gap(px(8.0))
                    .pl(px(38.0 + indent))
                    .pr(px(8.0))
                    .when(selected, |this| this.bg(hsla(0.6, 1.0, 0.6, 0.30)))
                    .tooltip(move |window, cx| {
                        Tooltip::new(format!(
                            "Click to select; double-click to edit; Option-drag to copy {}",
                            kind.raw_value().to_lowercase()
                        ))
                        .build(window, cx)
                    })
                    .aria_label(format!("{} effect", kind.raw_value()))
                    .child(
                        div()
                            .flex_none()
                            .w(px(20.0))
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(MouseButton::Left, {
                                let session = effect_session.clone();
                                move |_, _, cx| {
                                    session.update(cx, |session, _| session.toggle_effect(kind, row_id));
                                    cx.stop_propagation();
                                }
                            })
                            .child(
                                Icon::new(if enabled { IconName::Eye } else { IconName::EyeOff })
                                    .size(px(11.0))
                                    .text_color(hsla(0.0, 0.0, 1.0, 0.55)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(11.0))
                            .text_color(if enabled {
                                hsla(0.0, 0.0, 1.0, 1.0)
                            } else {
                                hsla(0.0, 0.0, 1.0, 0.55)
                            })
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(kind.raw_value()),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        let entity = effect_entity.clone();
                        let session = effect_session.clone();
                        move |event: &MouseDownEvent, _, cx| {
                            let editing = event.click_count > 1;
                            entity.update(cx, |panel, cx| {
                                session.update(cx, |session, _| session.select_effect(kind, row_id, editing));
                                panel.drop_target = None;
                                cx.notify();
                            });
                        }
                    })
                    .on_drag(
                        EffectDrag {
                            layer_id: row.id,
                            kind,
                        },
                        |_, _, _, cx| cx.new(|_| EmptyGhost),
                    )
                    .into_any_element()
            })
            .collect();

        let absolute_index = row.index;
        let row_id = row.id;
        let is_group = row.is_group;
        let cell = v_flex()
            .id(panel_key("layer-row", row.id))
            .relative()
            .w_full()
            .h(px(height))
            .opacity(if row.visible { 1.0 } else { 0.35 })
            .border_b_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.06))
            .when(row.active, |this| this.bg(hsla(0.6, 1.0, 0.6, 0.10)))
            .child(main)
            .children(effects)
            .child(
                // The bottom strip Option-click uses (`isClippingZone`): it belongs to the row, not
                // to the effect rows, so a row listing several effects keeps the rest of itself free.
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(CLIPPING_STRIP))
                    .when(!row.is_group, |this| {
                        this.on_mouse_down(MouseButton::Left, {
                            let session = session.clone();
                            move |event: &MouseDownEvent, _, cx| {
                                if event.modifiers.alt && !event.modifiers.platform {
                                    session.update(cx, |session, _| {
                                        session.effect_selection = None;
                                        session.toggle_clipping_mask(row_id);
                                    });
                                }
                            }
                        })
                    }),
            )
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                move |event: &MouseDownEvent, _, cx| {
                    entity.update(cx, |panel, cx| {
                        let double = event.click_count > 1;
                        panel.session.update(cx, |session, _| session.effect_selection = None);
                        if double {
                            let row = panel.rows(cx).into_iter().find(|candidate| {
                                candidate.id == row_id
                            });
                            if let Some(row) = row {
                                panel.double_click(&row, false, cx);
                            }
                            return;
                        }
                        panel.select_row(row_id, event, cx);
                        panel.click_name(row_id, cx);
                    });
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |panel, cx| {
                        panel.session.update(cx, |session, _| session.effect_selection = None);
                        let (selected, ids) = {
                            let session = panel.session.read(cx);
                            (
                                session.selected_layer_ids.contains(&row_id),
                                session.selected_layer_ids.iter().copied().collect::<Vec<_>>(),
                            )
                        };
                        if selected {
                            panel.session.update(cx, |session, _| {
                                session.select_layers(ids, Some(row_id))
                            });
                        } else {
                            panel
                                .session
                                .update(cx, |session, _| session.select_layer_target(row_id, false));
                        }
                        cx.notify();
                    });
                }
            })
            .on_drag(LayerDrag::new(row, &self.session, cx), |drag: &LayerDrag, _, _, cx| {
                cx.new(|_| DragGhost {
                    name: drag.name.clone(),
                })
            })
            .on_drag_move::<LayerDrag>({
                let entity = entity.clone();
                move |event: &DragMoveEvent<LayerDrag>, _, cx| {
                    // The lower half of a folder's row drops into it; elsewhere a drop falls above.
                    let into = is_group
                        && f32::from(event.event.position.y) > f32::from(ROW_HEIGHT) / 2.0;
                    entity.update(cx, |panel, cx| {
                        panel.drop_target = Some((row_id, into));
                        cx.notify();
                    });
                }
            })
            .on_drag_move::<VisibilitySwipe>({
                let entity = entity.clone();
                let session = session.clone();
                move |_, _, cx| {
                    let swipe = entity.read(cx).swipe;
                    if let Some((_, visible)) = swipe {
                        session.update(cx, |session, _| session.set_visibility_in_swipe(row_id, visible));
                    }
                }
            })
            .drag_over::<LayerDrag>(|style, _, _, _| style.bg(hsla(0.6, 1.0, 0.6, 0.14)))
            .on_drop::<LayerDrag>({
                let entity = entity.clone();
                let session = session.clone();
                move |drag: &LayerDrag, window, cx| {
                    // Option makes the drag a copy, as AppKit's operation mask did.
                    let copying = window.modifiers().alt;
                    let ids = drag.ids.clone();
                    let into = entity
                        .read(cx)
                        .drop_target
                        .map(|(target, into)| target == row_id && into)
                        .unwrap_or(false);
                    entity.update(cx, |panel, cx| {
                        let row_index = session
                            .read(cx)
                            .layer_rows()
                            .iter()
                            .position(|entry| entry.layer.id == row_id)
                            .unwrap_or(absolute_index);
                        let ids = panel.dragged_layers(&ids, cx);
                        let parent = session.read(cx).layer_rows().get(row_index).and_then(|entry| {
                            entry.layer.parent_id
                        });
                        let allowed = ids.iter().all(|id| {
                            session
                                .read(cx)
                                .can_place_layer(*id, if into { Some(row_id) } else { parent })
                        });
                        if !ids.is_empty() && allowed {
                            panel.place(&ids, row_index, into, copying, cx);
                        }
                        panel.drop_target = None;
                        cx.notify();
                    });
                }
            })
            .on_drop::<EffectDrag>({
                let entity = entity.clone();
                move |drag: &EffectDrag, _, cx| {
                    let source = drag.clone();
                    entity.update(cx, |panel, cx| {
                        if panel
                            .session
                            .read(cx)
                            .can_copy_effect(source.kind, source.layer_id, row_id)
                        {
                            panel.session.update(cx, |session, _| {
                                session.copy_effect(source.kind, source.layer_id, row_id)
                            });
                        }
                    });
                }
            })
            .on_drop::<MaskDrag>({
                let entity = entity.clone();
                move |drag: &MaskDrag, _, cx| {
                    let source = drag.layer_id;
                    entity.update(cx, |panel, cx| {
                        if panel.session.read(cx).can_copy_mask(source, row_id) {
                            panel.session.update(cx, |session, _| session.copy_mask(source, row_id));
                        }
                    });
                }
            });

        let menu_entity = cx.entity();
        // The right-click menu (`contextMenu(for:)`).
        cell.context_menu(move |menu, window, cx| {
            LayersPanel::context_menu(menu, menu_entity.clone(), window, cx)
        })
        .into_any_element()
    }

    /// The panel's footer: New blank layer, New folder, the mask button, the effects and adjustment
    /// menus, and Delete.
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.session.clone();
        let (can_edit_layers, can_edit_effects, has_active, mask_selected, effect_selected, selected_count) = {
            let session = self.session.read(cx);
            (
                session.can_edit_layers(),
                session.can_edit_effects(),
                session.active_layer().is_some(),
                session.is_mask_selected,
                session.selected_effect().is_some(),
                session.selected_layer_ids.len(),
            )
        };
        let delete_help = if effect_selected {
            "Delete selected effect"
        } else if mask_selected {
            "Delete layer mask"
        } else if selected_count > 1 {
            "Delete selected layers"
        } else {
            "Delete selected layer"
        };

        let add_entity = cx.entity();
        let group_entity = cx.entity();
        let delete_entity = cx.entity();
        let effects_entity = cx.entity();
        let adjustments_entity = cx.entity();

        h_flex()
            .items_center()
            .w_full()
            .px(px(8.0))
            .py(px(4.0))
            .child(
                Button::new("add-blank-layer")
                    .icon(Icon::new(IconName::SquarePlus))
                    .tooltip("New blank layer (⇧⌘N)")
                    .accessibility_label("New blank layer")
                    .ghost()
                    .px(px(FOOTER_HIT_X))
                    .py(px(FOOTER_HIT_Y))
                    .disabled(!can_edit_layers)
                    .on_click(move |_, _, cx| {
                        add_entity.update(cx, |panel, cx| {
                            panel.session.update(cx, |session, _| session.add_blank_layer())
                        });
                    }),
            )
            .child(
                Button::new("group-layers")
                    .icon(Icon::new(IconName::FolderPlus))
                    .tooltip("Group selected layers (⌘G)")
                    .accessibility_label("New folder")
                    .ghost()
                    .px(px(FOOTER_HIT_X))
                    .py(px(FOOTER_HIT_Y))
                    .disabled(!can_edit_layers)
                    .on_click(move |_, _, cx| {
                        group_entity.update(cx, |panel, cx| {
                            panel
                                .session
                                .update(cx, |session, _| session.group_selected_layers())
                        });
                    }),
            )
            .child(LayerMaskMenu::new(session.clone()))
            .child(
                Button::new("layer-effects")
                    .icon(Icon::new(IconName::Sparkles))
                    .tooltip("Layer effects: stroke and drop shadow")
                    .accessibility_label("Layer effects")
                    .ghost()
                    .px(px(FOOTER_HIT_X))
                    .py(px(FOOTER_HIT_Y))
                    .disabled(!can_edit_effects)
                    .dropdown_menu(move |menu, _window, _cx| {
                        LayerEffectKind::ALL.into_iter().fold(menu, |menu, kind| {
                            let session = effects_entity.clone();
                            menu.item(
                                PopupMenuItem::new(format!("{}…", kind.raw_value())).on_click(
                                    move |_, _, cx| {
                                        session.update(cx, |panel, cx| {
                                            panel
                                                .session
                                                .update(cx, |session, _| session.add_effect(kind));
                                        });
                                    },
                                ),
                            )
                        })
                    }),
            )
            .child(
                Button::new("add-adjustment")
                    .icon(Icon::new(IconName::Contrast))
                    .tooltip("New adjustment layer")
                    .accessibility_label("New adjustment layer")
                    .ghost()
                    .px(px(FOOTER_HIT_X))
                    .py(px(FOOTER_HIT_Y))
                    .disabled(!can_edit_layers)
                    .dropdown_menu(move |menu, _window, _cx| {
                        AdjustmentKind::ALL.into_iter().fold(menu, |menu, kind| {
                            let session = adjustments_entity.clone();
                            menu.item(PopupMenuItem::new(kind.raw_value()).on_click(
                                move |_, _, cx| {
                                    session.update(cx, |panel, cx| {
                                        panel
                                            .session
                                            .update(cx, |session, _| session.add_adjustment(kind));
                                    });
                                },
                            ))
                        })
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("delete-layer")
                    .icon(Icon::new(IconName::Trash))
                    .tooltip(delete_help)
                    .accessibility_label(delete_help)
                    .ghost()
                    .px(px(FOOTER_HIT_X))
                    .py(px(FOOTER_HIT_Y))
                    .disabled(!can_edit_layers || !has_active)
                    .on_click(move |_, _, cx| {
                        delete_entity.update(cx, |panel, cx| {
                            panel
                                .session
                                .update(cx, |session, _| session.delete_layer_or_mask())
                        });
                    }),
            )
            .into_any_element()
    }

    /// `No layers yet`: what the panel shows before there is anything to list.
    fn empty_state(&self, cx: &App) -> AnyElement {
        let has_document = self.session.read(cx).document.is_some();
        v_flex()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .p(px(16.0))
            .flex_1()
            .w_full()
            .text_color(hsla(0.0, 0.0, 1.0, 0.55))
            .child(Icon::new(IconName::Layers).size(px(25.0)))
            .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child("No layers yet"))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_center()
                    .child(if has_document {
                        "Import an image or add a blank layer."
                    } else {
                        "Create a canvas or import an image."
                    }),
            )
            .into_any_element()
    }
}

impl LayerDrag {
    /// The pasteboard writer's rows: the row itself, or the whole selection it belongs to.
    fn new(row: &RowData, session: &Entity<EditorSession>, cx: &App) -> Self {
        let ids = {
            let session = session.read(cx);
            if session.selected_layer_ids.contains(&row.id) {
                session
                    .layer_rows()
                    .iter()
                    .map(|entry| entry.layer.id)
                    .filter(|id| session.selected_layer_ids.contains(id))
                    .collect::<Vec<Id>>()
            } else {
                vec![row.id]
            }
        };
        Self {
            ids,
            name: row.name.clone().into(),
        }
    }
}

/// A one-point hairline between the panel's sections, as the Swift `Divider()` draws.
fn divider_h() -> impl IntoElement {
    div().h(px(1.0)).w_full().bg(hsla(0.0, 0.0, 1.0, 0.10))
}

impl Render for LayersPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `.onChange(of: focused) { if !isFocused { applyPercentage() } }`'s rename counterpart:
        // leaving the field keeps the typed name (`controlTextDidEndEditing`).
        if let Some((id, field, was_focused)) = self.rename.clone() {
            let focused = field.read(cx).focus_handle(cx).is_focused(window);
            if was_focused && !focused {
                self.end_rename(true, window, cx);
            } else {
                self.rename = Some((id, field, focused));
            }
        }

        let session = self.session.clone();
        let (count, active_layer_id, has_document) = {
            let session = self.session.read(cx);
            (
                session
                    .document
                    .as_ref()
                    .map(|document| document.layers.len())
                    .unwrap_or(0),
                session.active_layer_id,
                session.document.is_some(),
            )
        };
        let appearance_key = active_layer_id.map(|id| id.to_string()).unwrap_or_default();
        let appearance = window.use_keyed_state(
            ElementId::Name(format!("layer-appearance-{appearance_key}").into()),
            cx,
            |window, cx| LayerAppearanceControls::new(session.clone(), active_layer_id, window, cx),
        );
        let canvas = session
            .read(cx)
            .document
            .as_ref()
            .map(|document| document.size())
            .unwrap_or_else(|| Size::new(1.0, 1.0));
        let rows = self.rows(cx);
        let list: Vec<AnyElement> = rows
            .iter()
            .map(|row| {
                let row = row.clone();
                self.row(&row, canvas, window, cx)
            })
            .collect();
        let has_layers = !rows.is_empty();
        let footer = self.footer(cx);
        let empty = self.empty_state(cx);

        v_flex()
            .flex_col()
            .h_full()
            .w_full()
            .overflow_hidden()
            .child(
                // `HStack { Text("Layers")… }.padding(18)`.
                h_flex()
                    .items_center()
                    .w_full()
                    .p(px(18.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Layers"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(ElementId::Name("layer-count".into()))
                            .text_size(px(12.0))
                            .text_color(hsla(0.0, 0.0, 1.0, 0.4))
                            .aria_label("layerCount")
                            .child(format!("{count}")),
                    ),
            )
            .child(divider_h())
            .child(appearance)
            .child(divider_h())
            .child(
                div()
                    .id(ElementId::Name("layer-list".into()))
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .overflow_y_scroll()
                    .when(has_layers, |this| this.children(list))
                    .when(!has_layers, |this| this.child(empty)),
            )
            .child(divider_h())
            .child(footer)
    }
}

