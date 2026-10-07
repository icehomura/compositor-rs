//! The editor window's content: the tool headers, the tool rail, the canvas with its rulers, the
//! Layers panel, the status bar and the sheets the session opens.
//!
//! Ported from `ContentView` in `Compositor/ContentView.swift`.

use std::sync::Arc;

use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::settings;
use compositor_rs_session::projects::SessionHost;
use compositor_rs_session::EditorSession;

use crate::canvas::canvas_view::CanvasView;
use crate::canvas::rulers::{CanvasRuler, CanvasRulerCorner, CanvasRulerView};
use crate::panels::color_picker::ColorPickerPanelController;
use crate::panels::floating_panel::{FloatingPanelController, FloatingPanelPlacement};
use crate::panels::layer_mask_menu::MaskAloneBadge;
use crate::panels::layers_panel::LayersPanel;
use crate::panels::transform_inspector::TransformInspector;
use crate::sheets::color_range::ColorRangeSheet;
use crate::sheets::effects::EffectsSheet;
use crate::sheets::hue_saturation::HueSaturationSheet;
use crate::sheets::levels::LevelsSheet;
use crate::sheets::new_canvas::{NewCanvasSheet, NewCanvasSheetCallbacks};
use crate::tool_controls::brush::BrushControls;
use crate::tool_controls::filter::FilterSheet;
use crate::tool_controls::lasso::SelectionAmountSheet;
use crate::tool_controls::crop::CropControls;
use crate::tool_controls::gradient::GradientControls;
use crate::tool_controls::lasso::LassoControls;
use crate::tool_controls::navigation::NavigationToolHeader;
use crate::tool_controls::shape::ShapeControls;
use crate::tool_controls::type_tool::TypeControls;
use crate::tool_header::{
    tool_header_bar, tool_header_spacer, tool_header_title, tool_header_title_bar, ToolHeaderStyle,
    TOOL_HEADER_HEIGHT, TOOL_HEADER_PADDING,
};
use crate::toolbar::status_bar::{self, StatusBar};
use crate::toolbar::tool_rail::ToolRail;
use crate::widgets::indicatorless_scroll::IndicatorlessScrollView;

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::guides::CanvasGuideAxis;
use compositor_rs_core::image_ops::FilterKind;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The editor's backdrop (`Color(white: 0.14)`).
pub const EDITOR_BACKDROP: Hsla = hsla(0.0, 0.0, 0.14, 1.0);

/// The thickness of the `Divider()`s the Swift stacks between the header, the canvas row and the
/// status bar — they come out of the canvas row's height, as they do there.
pub const DIVIDER_HEIGHT: f32 = 1.0;

/// The Layers panel's width key (`@AppStorage("layersPanelWidth")`).
pub const LAYERS_PANEL_WIDTH_KEY: &str = "layersPanelWidth";

/// The width the panel is given before anything is remembered (`252.0`).
pub const LAYERS_PANEL_DEFAULT_WIDTH: f64 = 252.0;

/// The window's minimum content size (`frame(minWidth: 800, minHeight: 520)`).
pub const MIN_WIDTH: f32 = 800.0;
pub const MIN_HEIGHT: f32 = 520.0;

/// The editor's content view.
pub struct ContentView {
    session: Entity<EditorSession>,
    host: Arc<dyn SessionHost>,
    /// The Layers panel's width, remembered across launches.
    layers_panel_width: f64,
    /// Whether a drag is over the editor, so the drop border shows.
    is_drop_targeted: bool,
    /// The tab rail and the status bar.
    tool_rail: Entity<ToolRail>,
    status_bar: Entity<StatusBar>,
    /// The canvas element.
    canvas: Entity<CanvasView>,
    /// The Layers panel.
    layers_panel: Entity<LayersPanel>,
    /// The tool headers, built the first time their tool is selected.
    headers: Vec<(NavigationTool, AnyView)>,
    /// The transform inspector, rebuilt when the active layer changes (`id(session.activeLayerID)`).
    transform_inspector: Option<(Option<compositor_rs_core::Id>, AnyView)>,
    /// The floating panels the session opens, one per sheet (`ContentView.swift`'s six controllers).
    levels_panel: FloatingPanelController,
    adjustment_panel: FloatingPanelController,
    color_range_panel: FloatingPanelController,
    effects_panel: FloatingPanelController,
    selection_amount_panel: FloatingPanelController,
    filter_panel: FloatingPanelController,
    /// The app's Color Picker, which the palette and the tool bars open (Swift
    /// `ColorPaletteControls` owns its controller; here the editor hosts the panel and the window
    /// host the dialog, see [`ColorPickerPanelController`]).
    color_picker_panel: ColorPickerPanelController,
    /// The welcome sheet an empty tab shows over the canvas (`welcome`).
    new_canvas_sheet: Entity<NewCanvasSheet>,
}

impl ContentView {
    pub fn new(session: Entity<EditorSession>, host: Arc<dyn SessionHost>, cx: &mut Context<Self>) -> Self {
        let tool_rail = cx.new(|cx| ToolRail::new(session.clone(), cx));
        let status_bar = cx.new(|cx| StatusBar::new(session.clone(), cx));
        let canvas = cx.new(|cx| CanvasView::new(session.clone(), host.clone(), cx));
        let layers_panel = cx.new(|cx| LayersPanel::new(session.clone()));
        // `welcome`: the empty tab's sheet creates a project of its own rather than a bare document.
        let creating_session = session.clone();
        let new_canvas_sheet = cx.new(|cx| {
            NewCanvasSheet::new(
                session.clone(),
                NewCanvasSheetCallbacks {
                    on_create: Some(Arc::new(move |width, height, cx| {
                        creating_session.update(cx, |session, _| session.create_new_project(width, height));
                    })),
                },
                cx,
            )
        });
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            host,
            layers_panel_width: settings::int_value(LAYERS_PANEL_WIDTH_KEY, LAYERS_PANEL_DEFAULT_WIDTH as i64) as f64,
            is_drop_targeted: false,
            tool_rail,
            status_bar,
            canvas,
            layers_panel,
            headers: Vec::new(),
            transform_inspector: None,
            // `ContentView.swift:11-16`: one controller per panel, named as the Swift named them.
            levels_panel: FloatingPanelController::new("levelsPanel"),
            adjustment_panel: FloatingPanelController::new("adjustmentPanel"),
            color_range_panel: FloatingPanelController::new("colorRangePanel"),
            effects_panel: FloatingPanelController::new("effectsPanel"),
            selection_amount_panel: FloatingPanelController::new("selectionAmountPanel"),
            filter_panel: FloatingPanelController::new("filterPanel"),
            color_picker_panel: ColorPickerPanelController::new(),
            new_canvas_sheet,
        }
    }

    /// The header a tool shows, built once and kept (`toolHeaders`).
    fn header(&mut self, tool: NavigationTool, cx: &mut Context<Self>) -> AnyView {
        if tool == NavigationTool::Move {
            let active = self.session.read(cx).active_layer_id;
            let stale = self
                .transform_inspector
                .as_ref()
                .is_none_or(|(id, _)| *id != active);
            if stale {
                let session = self.session.clone();
                let view = cx.new(|cx| TransformInspector::new(session, cx));
                self.transform_inspector = Some((active, view.into()));
            }
            return self
                .transform_inspector
                .as_ref()
                .map(|(_, view)| view.clone())
                .expect("the inspector was just built");
        }
        if let Some((_, view)) = self.headers.iter().find(|(candidate, _)| *candidate == tool) {
            return view.clone();
        }
        let session = self.session.clone();
        let view: AnyView = match tool {
            NavigationTool::Brush | NavigationTool::SpotHealing | NavigationTool::CloneStamp | NavigationTool::Blur => {
                cx.new(|cx| BrushControls::new(session, cx)).into()
            }
            NavigationTool::Marquee | NavigationTool::Lasso | NavigationTool::Wand => {
                cx.new(|cx| LassoControls::new(session, cx)).into()
            }
            NavigationTool::Gradient => cx.new(|cx| GradientControls::new(session, cx)).into(),
            NavigationTool::Type => cx.new(|cx| TypeControls::new(session, cx)).into(),
            NavigationTool::Shape => cx.new(|cx| ShapeControls::new(session, cx)).into(),
            NavigationTool::Hand | NavigationTool::Zoom => cx.new(|cx| NavigationToolHeader::new(session, cx)).into(),
            NavigationTool::Crop => cx.new(|cx| CropControls::new(session, cx)).into(),
            _ => unreachable!("the Move tool and the simple bars are handled above"),
        };
        self.headers.push((tool, view.clone()));
        view
    }

    /// Whether the tool shows a header at all.
    fn has_header(tool: NavigationTool) -> bool {
        matches!(
            tool,
            NavigationTool::Move
                | NavigationTool::Brush
                | NavigationTool::SpotHealing
                | NavigationTool::CloneStamp
                | NavigationTool::Blur
                | NavigationTool::Marquee
                | NavigationTool::Lasso
                | NavigationTool::Wand
                | NavigationTool::Gradient
                | NavigationTool::Type
                | NavigationTool::Shape
                | NavigationTool::Eyedropper
                | NavigationTool::Hand
                | NavigationTool::Zoom
                | NavigationTool::Crop
                | NavigationTool::Idle
        )
    }

    /// The sheet a floating panel is showing, if its session state is open (`onChange` handlers).
    fn open_sheets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let (levels, hue_saturation, color_range, filter_kind, effects_kind, amount) = {
            let session = session.read(cx);
            (
                session.levels.is_some(),
                session.hue_saturation.is_some(),
                session.color_range.is_some(),
                session.filter_edit.as_ref().map(|edit| edit.kind),
                session.effects_editing.as_ref().map(|selection| selection.kind),
                session.selection_amount_operation,
            )
        };

        if levels {
            if !self.levels_panel.is_visible() {
                let closing = session.clone();
                self.levels_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.cancel_levels());
                });
                let view = cx.new(|cx| LevelsSheet::new(session.clone(), cx));
                self.levels_panel
                    .show("Levels", view, FloatingPanelPlacement::Automatic, window, cx);
            }
        } else {
            self.levels_panel.close(window, cx);
        }

        if hue_saturation {
            if !self.adjustment_panel.is_visible() {
                let closing = session.clone();
                self.adjustment_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.cancel_hue_saturation());
                });
                let view = cx.new(|cx| HueSaturationSheet::new(session.clone(), cx));
                self.adjustment_panel.show(
                    "Hue/Saturation",
                    view,
                    FloatingPanelPlacement::Automatic,
                    window,
                    cx,
                );
            }
        } else {
            self.adjustment_panel.close(window, cx);
        }

        if color_range {
            if !self.color_range_panel.is_visible() {
                let closing = session.clone();
                self.color_range_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.cancel_color_range());
                });
                let view = cx.new(|cx| ColorRangeSheet::new(session.clone(), cx));
                self.color_range_panel.show(
                    "Color Range",
                    view,
                    FloatingPanelPlacement::Automatic,
                    window,
                    cx,
                );
            }
        } else {
            self.color_range_panel.close(window, cx);
        }

        // The effects panel's title is the selected effect's own name (`selection.kind.rawValue`).
        if let Some(kind) = effects_kind {
            if !self.effects_panel.is_visible() {
                let closing = session.clone();
                self.effects_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.finish_effects_editing(false));
                });
                let view = cx.new(|cx| EffectsSheet::new(session.clone(), kind, cx));
                self.effects_panel.show(
                    kind.raw_value(),
                    view,
                    FloatingPanelPlacement::Automatic,
                    window,
                    cx,
                );
            }
        } else {
            self.effects_panel.close(window, cx);
        }

        if let Some(operation) = amount {
            if !self.selection_amount_panel.is_visible() {
                let closing = session.clone();
                self.selection_amount_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.selection_amount_operation = None);
                });
                let view = cx.new(|cx| SelectionAmountSheet::new(session.clone(), operation, cx));
                self.selection_amount_panel.show(
                    format!("{} Selection", operation.raw_value()),
                    view,
                    FloatingPanelPlacement::Automatic,
                    window,
                    cx,
                );
            }
        } else {
            self.selection_amount_panel.close(window, cx);
        }

        // Camera Raw's panel is docked to the window's right edge; every other filter is centred.
        if let Some(kind) = filter_kind {
            if !self.filter_panel.is_visible() {
                let closing = session.clone();
                self.filter_panel.set_on_close(move |_, cx| {
                    closing.update(cx, |session, _| session.cancel_filter());
                });
                let placement = if kind == FilterKind::CameraRaw {
                    FloatingPanelPlacement::DockedToMainWindowRight
                } else {
                    FloatingPanelPlacement::Automatic
                };
                let view = cx.new(|cx| FilterSheet::new(session.clone(), cx));
                self.filter_panel
                    .show(kind.raw_value(), view, placement, window, cx);
            }
        } else {
            self.filter_panel.close(window, cx);
        }

        // The app's Color Picker: a color the canvas can be sampled for is picked in the editor's own
        // panel, while a picker opened from a dialog is hosted by the window's dialog layer instead
        // (`AppRoot::sync_color_picker`), because the editor's panels draw below it.
        let dialog_up = window.has_active_dialog(cx);
        self.color_picker_panel.sync(session, dialog_up, window, cx);
    }

    /// The panel each sheet is shown in.
    fn panels(
        &mut self,
        row_top: f32,
        row_height: f32,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let mut panels: Vec<AnyElement> = [
            &mut self.levels_panel,
            &mut self.adjustment_panel,
            &mut self.color_range_panel,
            &mut self.effects_panel,
            &mut self.selection_amount_panel,
            &mut self.filter_panel,
        ]
        .into_iter()
        .filter_map(|panel| panel.render(row_top, row_height, window, cx))
        .collect();
        panels.extend(self.color_picker_panel.render(row_top, row_height, window, cx));
        panels
    }

    /// `PanelResizeEdge`: the divider that resizes the panel to its right.
    fn resize_edge(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let start = std::rc::Rc::new(std::cell::Cell::new(f64::NAN));
        let width = self.layers_panel_width;
        let session = self.session.clone();
        div()
            .id("layers-panel-resize")
            .w(px(1.0))
            .h_full()
            .bg(hsla(0.0, 0.0, 1.0, 0.10))
            .cursor(CursorStyle::ResizeLeftRight)
            .tooltip(|window, cx| Tooltip::new("Drag to resize the panel").build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                {
                    let start = start.clone();
                    cx.listener(move |this, _: &MouseDownEvent, _, _| {
                        start.set(f64::NAN);
                        let _ = &session;
                        let _ = this;
                    })
                },
            )
            .on_mouse_move({
                let start = start.clone();
                cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    let x = f64::from(f32::from(event.position.x));
                    let anchor = start.get();
                    if anchor.is_nan() {
                        start.set(x);
                        return;
                    }
                    let translation = x - anchor;
                    let range = LayersPanel::widths();
                    let value = (width - translation).round().clamp(*range.start(), *range.end());
                    this.layers_panel_width = value;
                    settings::set_int(value as i64, LAYERS_PANEL_WIDTH_KEY);
                    cx.notify();
                })
            })
            .on_mouse_up(
                MouseButton::Left,
                {
                    let start = start.clone();
                    cx.listener(move |_, _: &MouseUpEvent, _, _| {
                        start.set(f64::NAN);
                    })
                },
            )
            // An 8-point grab area over the hairline (`Color.clear.frame(width: 8)`).
            .child(
                div()
                    .absolute()
                    .left(px(-3.5))
                    .w(px(8.0))
                    .h_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .on_mouse_move({
                        let start = start.clone();
                        let session = self.session.clone();
                        cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            if event.pressed_button != Some(MouseButton::Left) {
                                return;
                            }
                            let _ = &session;
                            let x = f64::from(f32::from(event.position.x));
                            let anchor = start.get();
                            if anchor.is_nan() {
                                start.set(x);
                                return;
                            }
                            let range = LayersPanel::widths();
                            let value = (this.layers_panel_width - (x - anchor)).round().clamp(*range.start(), *range.end());
                            this.layers_panel_width = value;
                            settings::set_int(value as i64, LAYERS_PANEL_WIDTH_KEY);
                            cx.notify();
                        })
                    }),
            )
    }

    /// The alert a session error shows (`alert(_:isPresented:)`), with its single OK button.
    fn alert(
        cx: &mut Context<Self>,
        title: &'static str,
        message: String,
        dismiss: impl Fn(&mut Self, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .child(
                div()
                    .w(px(320.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .p(px(20.0))
                    .rounded(px(10.0))
                    .bg(hsla(0.0, 0.0, 0.18, 1.0))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_size(px(12.0)).child(message))
                    .child(
                        div().flex().flex_row().justify_end().child(
                            Button::new("alert-ok").label("OK").on_click(
                                cx.listener(move |this, _, window, cx| dismiss(this, window, cx)),
                            ),
                        ),
                    ),
            )
    }
}

impl Render for ContentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.open_sheets(window, cx);
        let tool = self.session.read(cx).tool;
        // The canvas row's height is computed, not left to `flex_1`: in this gpui/taffy version a
        // flex item of a percentage-sized column is laid out against a zero flex basis, which
        // collapses the row's subtree — the canvas viewport and every panel in it — to nothing.
        // The row is the space between the tool header and the status bar.
        let editor_height = (f32::from(window.viewport_size().height)
            - f32::from(gpui_kit::component::TITLE_BAR_HEIGHT)
            - crate::workspace::TAB_STRIP_HEIGHT)
            .max(MIN_HEIGHT);
        let header_height = if Self::has_header(tool) { TOOL_HEADER_HEIGHT } else { 0.0 };
        let dividers = if header_height > 0.0 { 2.0 } else { 1.0 } * DIVIDER_HEIGHT;
        let row_height = (editor_height - header_height - status_bar::HEIGHT - dividers).max(0.0);
        // The panels' own elements are drawn last, so they sit over the editor (`FloatingPanelController`).
        let panels = self.panels(header_height, row_height, window, cx);
        let session = self.session.read(cx);
        let has_document = session.document.is_some();
        let shows_rulers = session.shows_rulers && has_document;
        let mask_alone = session.mask_alone_layer().map(|layer| layer.name.clone());
        let shows_sample_ring = session.shows_sample_ring;
        let import_error = session.import_error.clone();
        let brush_error = session.brush_error.clone();
        let crop_error = session.crop_error.clone();
        let document = session.document.as_ref().map(|document| (document.width, document.height));
        let project_name = session
            .project_url
            .as_ref()
            .and_then(|url| url.file_stem())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let _ = document;
        let is_drop_targeted = self.is_drop_targeted;
        let panel_width = self.layers_panel_width;

        // The window's title follows the project: `.navigationTitle(projectURL?… ?? "Untitled")`.
        window.set_window_title(&project_name);

        let header: Option<AnyElement> = if !Self::has_header(tool) {
            None
        } else if tool == NavigationTool::Move
            || tool.is_brush_tool()
            || tool.is_selection_tool()
            || matches!(
                tool,
                NavigationTool::Gradient | NavigationTool::Type | NavigationTool::Shape | NavigationTool::Hand | NavigationTool::Zoom | NavigationTool::Crop
            )
        {
            let view = self.header(tool, cx);
            Some(div().child(view).child(divider_h()).into_any_element())
        } else if tool == NavigationTool::Eyedropper {
            // `HStack(spacing: 16) { title, Toggle("Sample Ring"), Spacer() }`: the checkbox sits
            // next to the title, and there is one trailing spacer — not the two a title bar's own
            // spacer plus another would give, which would centre it.
            let session = self.session.clone();
            Some(
                div()
                    .child(
                        tool_header_bar(16.0)
                            .child(tool_header_title("Eyedropper"))
                            .child(
                                Checkbox::new("sample-ring")
                                    .label("Sample Ring")
                                    .checked(shows_sample_ring)
                                    .on_click(move |value, _, cx| {
                                        let value = *value;
                                        session.update(cx, |session, _| session.shows_sample_ring = value);
                                    }),
                            )
                            .child(tool_header_spacer()),
                    )
                    .child(divider_h())
                    .into_any_element(),
            )
        } else {
            // No tool (A) keeps the header, so the canvas doesn't jump.
            Some(
                div()
                    .child(tool_header_title_bar("Select a tool"))
                    .child(divider_h())
                    .into_any_element(),
            )
        };

        let canvas_area = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .when(shows_rulers, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_row()
                        .child(CanvasRulerCorner::new().into_any_element())
                        .child(CanvasRulerView::new(self.session.clone(), CanvasGuideAxis::Horizontal).into_any_element()),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .when(shows_rulers, |this| {
                        this.child(CanvasRulerView::new(self.session.clone(), CanvasGuideAxis::Vertical).into_any_element())
                    })
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.canvas.clone())
                            .when(!has_document, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .right_0()
                                        .bottom_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(self.new_canvas_sheet.clone()),
                                )
                            })
                            .when_some(mask_alone, |this, name| {
                                this.child(
                                    div()
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .right_0()
                                        .flex()
                                        .justify_center()
                                        .pb(px(14.0))
                                        .child(MaskAloneBadge::new(self.session.clone(), name).into_any_element()),
                                )
                            }),
                    ),
            );

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(MIN_WIDTH))
            .min_h(px(MIN_HEIGHT))
            .bg(EDITOR_BACKDROP)
            .children(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(row_height))
                    .child(self.tool_rail.clone())
                    .child(divider())
                    .child(canvas_area)
                    .child(self.resize_edge(cx))
                    .child(div().w(px(panel_width as f32)).h_full().child(self.layers_panel.clone())),
            )
            .child(divider_h())
            .child(self.status_bar.clone())
            .children(panels)
            .when(is_drop_targeted, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(3.0))
                        .left(px(3.0))
                        .right(px(3.0))
                        .bottom(px(3.0))
                        .rounded(px(8.0))
                        .border_3()
                        .border_color(hsla(0.6, 1.0, 0.6, 1.0)),
                )
            })
            .when_some(import_error, |this, message| {
                this.child(Self::alert(cx, "Import couldn’t finish", message, |this, _, cx| {
                    this.session.update(cx, |session, _| session.import_error = None);
                }))
            })
            .when_some(brush_error, |this, message| {
                this.child(Self::alert(cx, "Couldn’t paint", message, |this, _, cx| {
                    this.session.update(cx, |session, _| session.brush_error = None);
                }))
            })
            .when_some(crop_error, |this, message| {
                this.child(Self::alert(cx, "Couldn’t crop", message, |this, _, cx| {
                    this.session.update(cx, |session, _| session.crop_error = None);
                }))
            })
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.is_drop_targeted = false;
                let point = this.canvas_point(window);
                let urls: Vec<std::path::PathBuf> = paths.paths().to_vec();
                let host = this.host.clone();
                this.session.update(cx, |session, _| {
                    let at = session
                        .document
                        .as_ref()
                        .map(|document| session.viewport.document_point(point, document.size()));
                    session.import_images(&urls, at, host.as_ref());
                });
            }))
            .can_drop(|any, _, _| any.is::<ExternalPaths>())
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(hsla(0.6, 1.0, 0.6, 1.0)))
            .on_file_drop_exit(cx.listener(|this, _, _, cx| {
                this.is_drop_targeted = false;
                cx.notify();
            }))
    }
}

impl ContentView {
    /// The window point a drop lands at, in the canvas's own coordinates.
    fn canvas_point(&self, window: &Window) -> Point {
        let _ = window;
        Point::ZERO
    }
}

/// A one-point divider, as the Swift `Divider()` draws.
fn divider() -> impl IntoElement {
    div().w(px(1.0)).h_full().bg(hsla(0.0, 0.0, 1.0, 0.10))
}

/// The horizontal divider between stacked sections.
fn divider_h() -> impl IntoElement {
    div().h(px(1.0)).w_full().bg(hsla(0.0, 0.0, 1.0, 0.10))
}
/// The size a canvas area is given, for the sheets that need the window's own geometry.
pub fn canvas_area_size(size: Size) -> Size {
    size
}

/// The rect a sheet is centered in, from the editor's own rect.
pub fn centered_rect(area: Rect, sheet: Size) -> Rect {
    Rect::new(
        area.origin.x + (area.size.width - sheet.width) / 2.0,
        area.origin.y + (area.size.height - sheet.height) / 2.0,
        sheet.width,
        sheet.height,
    )
}

/// The tool rail's width plus a divider, so the canvas can be measured from the window's width.
pub fn chrome_width() -> f32 {
    crate::toolbar::tool_rail::WIDTH + 1.0
}

/// The padding a sheet's host gives it, matching `TOOL_HEADER_PADDING`.
pub fn sheet_padding() -> f32 {
    TOOL_HEADER_PADDING
}

/// The title font the sheets share with the tool headers.
pub fn sheet_title_font() -> Font {
    ToolHeaderStyle::title_font()
}

/// The scroll container the tool rail uses, so its width stays the rail's own.
pub fn rail_scroll_width() -> f32 {
    crate::toolbar::tool_rail::WIDTH
}

/// The indicator-less scroll view the rail is drawn in.
pub fn rail_scroll(id: &'static str) -> impl IntoElement {
    IndicatorlessScrollView::rail(id)
}

/// The ruler's thickness, for the canvas corner and the edges.
pub fn ruler_thickness() -> f32 {
    CanvasRuler::THICKNESS as f32
}
