//! The selection tools' header (port of `UI/LassoControls.swift`): the Marquee, Lasso and Magic
//! Wand bars, their wand and object-selection controls, the selection modifiers, the icons the rail
//! draws for the polygonal lasso and the object selection, and Select > Modify's amount sheet.

use compositor_rs_core::document::NavigationTool;
use compositor_rs_core::selection::{LassoKind, SelectionMode, WandMode};
use compositor_rs_pixels::masks::WandSampleSize;
use compositor_rs_session::selection::SelectionAmountOperation;
use compositor_rs_session::EditorSession;

use crate::tool_controls::{
    segmented_picker, unit_suffix, EditEnd, FieldSpec, Fields, TextFields, CONTROL_SPACING,
};
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `LassoKind.marqueeChoices`.
pub const MARQUEE_CHOICES: [LassoKind; 2] = [LassoKind::Rectangle, LassoKind::Ellipse];
/// `LassoKind.lassoChoices`.
pub const LASSO_CHOICES: [LassoKind; 2] = [LassoKind::Freehand, LassoKind::Polygonal];

/// The bar the Marquee, Lasso and Magic Wand tools share.
pub struct LassoControls {
    session: Entity<EditorSession>,
    fields: Fields,
}

impl LassoControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
        }
    }
}

/// The bar's own state, read in one pass.
struct State {
    tool: NavigationTool,
    marquee_kind: LassoKind,
    lasso_kind: LassoKind,
    wand_mode: WandMode,
    selection_mode: SelectionMode,
    wand_tolerance: i32,
    wand_sample_size: WandSampleSize,
    wand_sample_all_layers: bool,
    wand_contiguous: bool,
    object_sample_all_layers: bool,
    object_edge_offset: i32,
    antialiased: bool,
    feather_amount: i32,
    expand_amount: i32,
    contract_amount: i32,
    can_modify_selection: bool,
    can_edit_selection: bool,
    /// The open selection's emptiness, `None` when nothing is selected (`if let selection = session.selection`).
    selection_is_empty: Option<bool>,
    enabled: bool,
}

impl Render for LassoControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = {
            let session = self.session.read(cx);
            State {
                tool: session.tool,
                marquee_kind: session.marquee_kind,
                lasso_kind: session.lasso_kind,
                wand_mode: session.wand_mode,
                selection_mode: session.displayed_selection_mode(),
                wand_tolerance: session.wand_settings.tolerance,
                wand_sample_size: session.wand_settings.sample_size,
                wand_sample_all_layers: session.wand_settings.sample_all_layers,
                wand_contiguous: session.wand_settings.contiguous,
                object_sample_all_layers: session.object_selection_settings.sample_all_layers,
                object_edge_offset: session.object_selection_settings.edge_offset,
                antialiased: session.selection_antialiased,
                feather_amount: session.selection_feather_amount,
                expand_amount: session.selection_expand_amount,
                contract_amount: session.selection_contract_amount,
                can_modify_selection: session.can_modify_selection(),
                can_edit_selection: session.can_edit_selection(),
                selection_is_empty: session.selection().map(|selection| selection.is_empty()),
                enabled: session.document.is_some() && !session.shows_busy,
            }
        };
        let tool = state.tool;

        let title = tool_header_title(match tool {
            NavigationTool::Marquee => "Marquee",
            NavigationTool::Wand => "Magic",
            _ => "Lasso",
        });

        // The Marquee's shape (`Press M`), the Wand's mode (`Tab`), the Lasso's kind (`L`).
        let marquee_kind = (tool == NavigationTool::Marquee).then(|| {
            let session = self.session.clone();
            div()
                .id("marquee-kind")
                .tooltip(|window, cx| {
                    Tooltip::new("Press M to switch between Rectangle and Ellipse").build(window, cx)
                })
                .child(
                    segmented_picker(
                        "marquee-kind-picker",
                        MARQUEE_CHOICES.map(|kind| (kind, kind.raw_value())),
                        state.marquee_kind,
                        move |kind, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| {
                                session.cancel_lasso();
                                session.marquee_kind = kind;
                            });
                        },
                    )
                    .disabled(!state.enabled),
                )
        });
        let wand_mode = (tool == NavigationTool::Wand).then(|| {
            let session = self.session.clone();
            div()
                .id("wand-mode")
                .tooltip(|window, cx| {
                    Tooltip::new("Press Tab to switch between Wand and Object").build(window, cx)
                })
                .child(
                    segmented_picker(
                        "wand-mode-picker",
                        WandMode::ALL.map(|mode| (mode, mode.raw_value())),
                        state.wand_mode,
                        move |mode, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| {
                                session.cancel_lasso();
                                session.wand_mode = mode;
                            });
                        },
                    )
                    .disabled(!state.enabled),
                )
        });
        let lasso_kind = (tool == NavigationTool::Lasso).then(|| {
            let session = self.session.clone();
            div()
                .id("lasso-kind")
                .tooltip(|window, cx| {
                    Tooltip::new("Press L to switch between Freehand and Polygonal").build(window, cx)
                })
                .child(
                    segmented_picker(
                        "lasso-kind-picker",
                        LASSO_CHOICES.map(|kind| (kind, kind.raw_value())),
                        state.lasso_kind,
                        move |kind, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| {
                                session.cancel_lasso();
                                session.lasso_kind = kind;
                            });
                        },
                    )
                    .disabled(!state.enabled),
                )
        });

        // Shows held Shift/Option (or an outline's mode) live; clicking sets the choice.
        let selection_mode = {
            let session = self.session.clone();
            div()
                .id("selection-mode")
                .tooltip(|window, cx| {
                    Tooltip::new("Hold Shift to add or Option to subtract for one outline")
                        .build(window, cx)
                })
                .child(
                    segmented_picker(
                        "selection-mode-picker",
                        SelectionMode::ALL.map(|mode| (mode, mode.raw_value())),
                        state.selection_mode,
                        move |mode, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| session.selection_mode_choice = mode);
                        },
                    )
                    .disabled(!state.enabled),
                )
        };

        let wand_controls = (tool == NavigationTool::Wand && state.wand_mode == WandMode::Wand)
            .then(|| self.wand_controls(&state, cx));
        let object_controls = (tool == NavigationTool::Wand && state.wand_mode == WandMode::Object)
            .then(|| self.object_selection_controls(&state, cx));

        // Rectangles snap to whole pixels, so smoothing doesn't apply (as in Photoshop); ellipses
        // curve.
        let antialias = (tool == NavigationTool::Lasso
            || tool == NavigationTool::Wand
            || (tool == NavigationTool::Marquee && state.marquee_kind == LassoKind::Ellipse))
        .then(|| {
            let session = self.session.clone();
            let object_mode = tool == NavigationTool::Wand && state.wand_mode == WandMode::Object;
            Checkbox::new("selection-antialias")
                .label("Anti-alias")
                .checked(state.antialiased)
                .disabled(!state.enabled)
                .tooltip(if object_mode {
                    "Smooth the detected object outline; turn off for the raw pixel mask"
                } else {
                    "Smooth selection edges; turn off for hard pixel edges"
                })
                .on_click(move |checked, _, cx| {
                    let checked = *checked;
                    let session = session.clone();
                    session.update(cx, |session, _| session.selection_antialiased = checked);
                })
        });

        let modifiers = div()
            .flex()
            .items_center()
            .gap(px(CONTROL_SPACING))
            .child(div().w(px(1.0)).h(px(18.0)).bg(hsla(0.0, 0.0, 1.0, 0.10)))
            .child(self.modify_control("Expand", state.expand_amount, state.can_modify_selection, cx))
            .child(self.modify_control("Contract", state.contract_amount, state.can_modify_selection, cx))
            .child(self.feather_control(&state, cx));

        let deselect = state.selection_is_empty.is_some().then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(CONTROL_SPACING))
                .when(state.selection_is_empty == Some(true), |this| {
                    this.child(
                        div()
                            .text_color(cx.theme().tokens.muted_foreground)
                            .child("Empty selection"),
                    )
                })
                .child({
                    let session = self.session.clone();
                    Button::new("deselect")
                        .label("Deselect")
                        .disabled(!state.can_edit_selection)
                        .on_click(move |_, _, cx| {
                            let session = session.clone();
                            session.update(cx, |session, _| session.deselect());
                        })
                })
        });

        tool_header_bar(CONTROL_SPACING)
            .child(title)
            .children(marquee_kind)
            .children(wand_mode)
            .children(lasso_kind)
            .child(selection_mode)
            .children(wand_controls)
            .children(object_controls)
            .children(antialias)
            .child(modifiers)
            .child(tool_header_spacer())
            .children(deselect)
    }
}

impl LassoControls {
    /// Tolerance, sample size, which pixels to read, and whether matches must connect.
    fn wand_controls(&mut self, state: &State, cx: &mut Context<Self>) -> impl IntoElement {
        let tolerance_field = self.fields.get("wand-tolerance", cx);
        let tolerance_scrub = {
            let session = self.session.clone();
            NumericScrub::new(state.wand_tolerance as f64, 1.0, (0.0, 255.0)).whole_numbers().on_change(
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.wand_settings.tolerance = value as i32);
                },
            )
        };
        let tolerance_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| session.wand_settings.tolerance = value as i32);
            }
        };
        let sample_size = {
            let session = self.session.clone();
            segmented_picker(
                "wand-sample-size",
                WandSampleSize::ALL.map(|size| (size, size.title())),
                state.wand_sample_size,
                move |size, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.wand_settings.sample_size = size);
                },
            )
            .disabled(!state.enabled)
        };
        let sample = {
            let session = self.session.clone();
            segmented_picker(
                "wand-sample",
                [(false, "This Layer"), (true, "All Layers")],
                state.wand_sample_all_layers,
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.wand_settings.sample_all_layers = value);
                },
            )
            .disabled(!state.enabled)
        };
        let contiguous = {
            let session = self.session.clone();
            Checkbox::new("wand-contiguous")
                .label("Contiguous")
                .checked(state.wand_contiguous)
                .disabled(!state.enabled)
                .tooltip("Select only similar pixels connected to the one you click; off selects them everywhere")
                .on_click(move |checked, _, cx| {
                    let checked = *checked;
                    let session = session.clone();
                    session.update(cx, |session, _| session.wand_settings.contiguous = checked);
                })
        };

        div()
            .flex()
            .items_center()
            .gap(px(CONTROL_SPACING))
            .child(
                div()
                    .id("wand-tolerance")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .tooltip(|window, cx| {
                        Tooltip::new("How far each color channel (0–255) can differ from the clicked color and still be selected")
                            .build(window, cx)
                    })
                    .child(div().child("Tolerance").scrubbable("wand-tolerance-label", tolerance_scrub))
                    .child(tolerance_field.element(
                        "wand-tolerance-field",
                        state.wand_tolerance as f64,
                        FieldSpec::new((0.0, 255.0), 0)
                            .fallback(0.0)
                            .disabled(!state.enabled),
                        44.0,
                        tolerance_write,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("wand-sample-size-label")
                    .tooltip(|window, cx| {
                        Tooltip::new("Match the clicked pixel, or the average of the pixels around it")
                            .build(window, cx)
                    })
                    .child(sample_size),
            )
            .child(
                div()
                    .id("wand-sample-label")
                    .tooltip(|window, cx| {
                        Tooltip::new("Read colors from the active layer only, or from every visible layer as shown")
                            .build(window, cx)
                    })
                    .child(sample),
            )
            .child(contiguous)
    }

    /// The Object Selection's own pair: which layers to analyze, and how far the edge moves.
    fn object_selection_controls(&mut self, state: &State, cx: &mut Context<Self>) -> impl IntoElement {
        let edge_field = self.fields.get("object-edge", cx);
        let edge_scrub = {
            let session = self.session.clone();
            NumericScrub::new(state.object_edge_offset as f64, 1.0, (-10.0, 10.0)).whole_numbers().on_change(
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.object_selection_settings.edge_offset = value as i32;
                    });
                },
            )
        };
        let edge_write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    session.object_selection_settings.edge_offset = value as i32;
                });
            }
        };
        let sample = {
            let session = self.session.clone();
            segmented_picker(
                "object-sample",
                [(false, "This Layer"), (true, "All Layers")],
                state.object_sample_all_layers,
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.object_selection_settings.sample_all_layers = value;
                    });
                },
            )
            .disabled(!state.enabled)
        };

        div()
            .flex()
            .items_center()
            .gap(px(CONTROL_SPACING))
            .child(
                div()
                    .id("object-sample-label")
                    .tooltip(|window, cx| {
                        Tooltip::new("Analyze the active layer only, or every visible layer as shown")
                            .build(window, cx)
                    })
                    .child(sample),
            )
            .child(
                div()
                    .id("object-edge")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    // The bar squeezes text before controls, so without this the label and unit collapse to
                    // nothing the moment a selection adds its own buttons, leaving an unlabelled number box.
                    .flex_none()
                    .tooltip(|window, cx| {
                        Tooltip::new("Positive values tighten the detected mask inward; negative values expand it outward")
                            .build(window, cx)
                    })
                    .child(div().child("Edge").scrubbable("object-edge-label", edge_scrub))
                    .child(unit_suffix(
                        edge_field.element(
                            "object-edge-field",
                            state.object_edge_offset as f64,
                            FieldSpec::new((-10.0, 10.0), 0)
                                .fallback(0.0)
                                .disabled(!state.enabled),
                            40.0,
                            edge_write,
                            cx,
                        ),
                        div().child("px"),
                    )),
            )
    }

    /// Feather's button, its amount field and its unit scrub.
    fn feather_control(&mut self, state: &State, cx: &mut Context<Self>) -> impl IntoElement {
        let field = self.fields.get("selection-feather", cx);
        let write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| session.selection_feather_amount = value as i32);
            }
        };
        let scrub = {
            let session = self.session.clone();
            NumericScrub::new(state.feather_amount as f64, 1.0, (1.0, 250.0)).whole_numbers().on_change(
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.selection_feather_amount = value as i32);
                },
            )
        };
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .child({
                let session = self.session.clone();
                Button::new("selection-feather-button")
                    .label("Feather")
                    .disabled(!state.can_modify_selection)
                    .tooltip("Fade the edge of the selection by this many pixels")
                    .on_click(move |_, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| {
                            session.feather_selection(session.selection_feather_amount)
                        });
                    })
            })
            .child(unit_suffix(
                field.element(
                    "selection-feather-field",
                    state.feather_amount as f64,
                    FieldSpec::new((1.0, 250.0), 0)
                        .fallback(2.0)
                        .disabled(!state.enabled),
                    48.0,
                    write,
                    cx,
                ),
                div().child("px").scrubbable("selection-feather-unit", scrub),
            ))
    }

    /// A button plus its pixel amount (1–500, default 1); both disabled without a selection.
    fn modify_control(
        &mut self,
        title: &'static str,
        amount: i32,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field = self.fields.get(
            match title {
                "Expand" => "selection-expand",
                _ => "selection-contract",
            },
            cx,
        );
        let expanded = title == "Expand";
        let write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| {
                let session = session.clone();
                session.update(cx, |session, _| {
                    if expanded {
                        session.selection_expand_amount = value as i32;
                    } else {
                        session.selection_contract_amount = value as i32;
                    }
                });
            }
        };
        let scrub = {
            let session = self.session.clone();
            NumericScrub::new(amount as f64, 1.0, (1.0, 500.0)).whole_numbers().on_change(
                move |value, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        if expanded {
                            session.selection_expand_amount = value as i32;
                        } else {
                            session.selection_contract_amount = value as i32;
                        }
                    });
                },
            )
        };
        div()
            .id("selection-modify-row")
            .flex()
            .items_center()
            .gap(px(5.0))
            .when(!enabled, |this| this.opacity(0.5))
            .tooltip({
                let help: SharedString = format!("{title} the selection by this many pixels").into();
                move |window, cx| Tooltip::new(help.clone()).build(window, cx)
            })
            .child({
                let session = self.session.clone();
                Button::new((ElementId::from("selection-modify"), title))
                    .label(title)
                    .disabled(!enabled)
                    .on_click(move |_, _, cx| {
                        let session = session.clone();
                        session.update(cx, |session, _| {
                            if expanded {
                                session.expand_selection(session.selection_expand_amount);
                            } else {
                                session.contract_selection(session.selection_contract_amount);
                            }
                        });
                    })
            })
            .child(unit_suffix(
                field.element(
                    match title {
                        "Expand" => "selection-expand-field",
                        _ => "selection-contract-field",
                    },
                    amount as f64,
                    FieldSpec::new((1.0, 500.0), 0).fallback(1.0).disabled(!enabled),
                    40.0,
                    write,
                    cx,
                ),
                div()
                    .child("px")
                    .scrubbable(
                        match title {
                            "Expand" => "selection-expand-unit",
                            _ => "selection-contract-unit",
                        },
                        scrub,
                    ),
            ))
    }
}

/// Tool-rail icon for the Polygonal Lasso: the lasso's loop and rope drawn as straight segments, in
/// the line weight of the SF Symbols beside it.
pub struct PolygonalLassoToolIcon;

impl PolygonalLassoToolIcon {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PolygonalLassoToolIcon {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for PolygonalLassoToolIcon {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for PolygonalLassoToolIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _, window, _| {
                // `let unit = size.width / 18`.
                let unit = f32::from(bounds.size.width) / 18.0;
                let at = |x: f32, y: f32| {
                    point(bounds.left() + px(x * unit), bounds.top() + px(y * unit))
                };
                let color = window.text_style().color;
                let mut stroke = PathBuilder::stroke(px(1.4 * unit));
                // Laid out like the SF Symbol lasso: a wide loop, a knot below its right side, a
                // short rope.
                let loop_points = [
                    (1.2, 7.0),
                    (4.0, 2.4),
                    (11.8, 1.8),
                    (16.8, 5.2),
                    (15.6, 10.4),
                    (7.0, 11.6),
                ];
                stroke.move_to(at(loop_points[0].0, loop_points[0].1));
                for (x, y) in &loop_points[1..] {
                    stroke.line_to(at(*x, *y));
                }
                stroke.close();
                let knot = [(8.9, 10.9), (13.3, 10.5), (11.6, 14.5)];
                stroke.move_to(at(knot[0].0, knot[0].1));
                for (x, y) in &knot[1..] {
                    stroke.line_to(at(*x, *y));
                }
                stroke.close();
                stroke.move_to(at(11.6, 14.5));
                stroke.line_to(at(12.9, 17.3));
                if let Ok(path) = stroke.build() {
                    window.paint_path(path, color);
                }
            },
        )
        .w(px(crate::tool_controls::clone_stamp::ICON_SIZE))
        .h(px(crate::tool_controls::clone_stamp::ICON_SIZE))
    }
}

/// The Object Selection's tool-rail icon: four corner brackets around a pointer.
pub struct ObjectSelectionToolIcon;

impl ObjectSelectionToolIcon {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ObjectSelectionToolIcon {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for ObjectSelectionToolIcon {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for ObjectSelectionToolIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _, window, _| {
                let unit = f32::from(bounds.size.width) / 18.0;
                let at = |x: f32, y: f32| {
                    point(bounds.left() + px(x * unit), bounds.top() + px(y * unit))
                };
                let color = window.text_style().color;
                let mut corners = PathBuilder::stroke(px(1.6 * unit));
                for bend in [
                    [(2.0, 6.0), (2.0, 2.0), (6.0, 2.0)],
                    [(12.0, 2.0), (16.0, 2.0), (16.0, 6.0)],
                    [(16.0, 12.0), (16.0, 16.0), (12.0, 16.0)],
                    [(6.0, 16.0), (2.0, 16.0), (2.0, 12.0)],
                ] {
                    corners.move_to(at(bend[0].0, bend[0].1));
                    corners.line_to(at(bend[1].0, bend[1].1));
                    corners.line_to(at(bend[2].0, bend[2].1));
                }
                if let Ok(path) = corners.build() {
                    window.paint_path(path, color);
                }
                let mut cursor = PathBuilder::fill();
                let arrow = [
                    (7.0, 5.0),
                    (7.0, 14.0),
                    (9.6, 11.7),
                    (11.3, 15.3),
                    (13.2, 14.4),
                    (11.5, 10.9),
                    (14.5, 10.9),
                ];
                cursor.move_to(at(arrow[0].0, arrow[0].1));
                for (x, y) in &arrow[1..] {
                    cursor.line_to(at(*x, *y));
                }
                cursor.close();
                if let Ok(path) = cursor.build() {
                    window.paint_path(path, color);
                }
            },
        )
        .w(px(crate::tool_controls::clone_stamp::ICON_SIZE))
        .h(px(crate::tool_controls::clone_stamp::ICON_SIZE))
    }
}

/// Select > Modify's amount sheet: a whole number from 1 to the operation's maximum, as a scrubber,
/// a slider and a field, with Cancel and OK.
pub struct SelectionAmountSheet {
    session: Entity<EditorSession>,
    operation: SelectionAmountOperation,
    /// What the field holds (`@State private var input`).
    input: String,
    fields: TextFields,
    /// Whether the field has taken the focus yet (`onAppear { focused = true }`).
    focused_on_appear: bool,
}

impl SelectionAmountSheet {
    pub fn new(
        session: Entity<EditorSession>,
        operation: SelectionAmountOperation,
        cx: &mut Context<Self>,
    ) -> Self {
        let amount = {
            let session = session.read(cx);
            match operation {
                SelectionAmountOperation::Expand => session.selection_expand_amount,
                SelectionAmountOperation::Contract => session.selection_contract_amount,
                SelectionAmountOperation::Feather => session.selection_feather_amount,
            }
        };
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            operation,
            input: amount.to_string(),
            fields: TextFields::default(),
            focused_on_appear: false,
        }
    }

    /// `maximum`: Feather reaches 250, the others 500.
    pub fn maximum(&self) -> i32 {
        if self.operation == SelectionAmountOperation::Feather {
            250
        } else {
            500
        }
    }

    /// The amount the field holds, or `None` while it is not a whole number in range.
    pub fn amount(&self) -> Option<i32> {
        self.input
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|value| (1..=self.maximum()).contains(value))
    }

    /// Cancel (`session.selectionAmountOperation = nil`).
    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, _| session.selection_amount_operation = None);
    }

    /// OK (`session.confirmSelectionAmount(amount)`).
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(amount) = self.amount() {
            self.session
                .update(cx, |session, _| session.confirm_selection_amount(amount));
        }
    }
}

impl Render for SelectionAmountSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let maximum = self.maximum();
        let amount = self.amount();
        let invalid = amount.is_none();
        let current = amount.unwrap_or(1);

        let field = self.fields.get("selection-amount", cx);
        if !self.focused_on_appear {
            self.focused_on_appear = true;
            field.focus(window, cx);
        }

        let input_field = {
            let on_change = {
                let entity = cx.entity();
                move |typed: String, _: &mut Window, cx: &mut App| {
                    entity.update(cx, |this, cx| {
                        this.input = typed;
                        cx.notify();
                    });
                }
            };
            let on_end = {
                let entity = cx.entity();
                move |end: EditEnd, _: &mut Window, cx: &mut App| {
                    entity.update(cx, |this, cx| match end {
                        EditEnd::Escape => this.cancel(cx),
                        EditEnd::Return => this.confirm(cx),
                        EditEnd::Blur => {}
                    });
                }
            };
            unit_suffix(
                field.element(
                    "selection-amount-field",
                    &self.input,
                    56.0,
                    on_change,
                    on_end,
                    cx,
                ),
                div().child("px"),
            )
        };

        let scrub = {
            let entity = cx.entity();
            NumericScrub::new(current as f64, 1.0, (1.0, maximum as f64))
                .whole_numbers()
                .on_change(move |value, _, cx| {
                    let typed = format!("{}", value as i32);
                    entity.update(cx, |this, cx| {
                        this.input = typed;
                        cx.notify();
                    });
                })
        };

        let slider = {
            let entity = cx.entity();
            CameraRawSlider::plain(
                "selection-amount-slider",
                current as f64,
                (1.0, maximum as f64),
                "Amount",
            )
            .step(1.0)
            .on_change(move |value, _, cx| {
                let typed = format!("{}", value.round() as i32);
                entity.update(cx, |this, cx| {
                    this.input = typed;
                    cx.notify();
                });
            })
        };

        let escape = {
            let entity = cx.entity();
            move |event: &KeyDownEvent, _: &mut Window, cx: &mut App| {
                let key = event.keystroke.key.as_str();
                if key != "escape" && key != "enter" && key != "return" {
                    return;
                }
                let confirm = key != "escape";
                entity.update(cx, |this, cx| {
                    if confirm {
                        this.confirm(cx);
                    } else {
                        this.cancel(cx);
                    }
                });
                cx.stop_propagation();
            }
        };

        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .w(px(380.0))
            .p(px(24.0))
            .on_key_down(escape)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .min_w(px(60.0))
                            .child(div().child("Amount").scrubbable("selection-amount-label", scrub)),
                    )
                    .child(div().w(px(180.0)).child(slider))
                    .child(input_field),
            )
            .child(
                div()
                    .text_color(cx.theme().tokens.muted_foreground)
                    .opacity(if invalid { 1.0 } else { 0.0 })
                    .child(format!("Enter a whole number from 1 to {maximum} px.")),
            )
            .child(div().h(px(1.0)).w_full().bg(hsla(0.0, 0.0, 1.0, 0.10)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child({
                        let entity = cx.entity();
                        Button::new("selection-amount-cancel")
                            .label("Cancel")
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |this, cx| this.cancel(cx));
                            })
                    })
                    .child(tool_header_spacer())
                    .child({
                        let entity = cx.entity();
                        Button::new("selection-amount-ok")
                            .label("OK")
                            .primary()
                            .disabled(invalid)
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |this, cx| this.confirm(cx));
                            })
                    }),
            )
    }
}
