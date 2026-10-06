//! A movable, non-modal panel for tool dialogs: it never dims the editor, opens centered on the
//! canvas the first time, and reopens wherever it was last left (for the app session).
//!
//! Ported from `UI/FloatingPanel.swift`. The AppKit `NSPanel` becomes an in-window overlay
//! (`docs/PORTING.md` § 4: "floating panels become child windows/overlays"): the controller keeps
//! the frame — centred on the canvas the first time, remembered afterwards, docked to the right
//! edge when asked — and the host draws [`FloatingPanelController::render`] at its window's root.
//! The close button reports through `on_close`, so callers can treat it as Cancel.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::toolbar::tool_rail;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

/// Where a panel opens the first time (or each time, for docked styles).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatingPanelPlacement {
    /// Centered on the canvas, or the last position the user left the panel.
    Automatic,
    /// Full height of the main document window, flush against its right edge.
    DockedToMainWindowRight,
}

/// Top-left corners (window coordinates) per panel, so a panel reopens where it was left.
static POSITIONS: LazyLock<Mutex<HashMap<SharedString, Point<Pixels>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The focus handle of each open panel, so [`FloatingPanelController::refocus`] can find one.
static FOCUS_HANDLES: LazyLock<Mutex<HashMap<SharedString, FocusHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What a panel remembers for itself between frames: where it sits and its own focus.
struct PanelState {
    /// The panel's top-left corner in window coordinates.
    position: Point<Pixels>,
    /// Whether a position has been chosen yet (the first frame centres the panel).
    placed: bool,
    focus: FocusHandle,
}

/// The empty view a panel's title-bar drag carries: moving a panel is not a payload.
struct PanelDrag;

struct PanelDragView;

impl Render for PanelDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A movable, non-modal panel for tool dialogs.
pub struct FloatingPanelController {
    /// The panel's own name (`init(name:)`), which is also its identifier.
    name: SharedString,
    title: SharedString,
    placement: FloatingPanelPlacement,
    content: Option<AnyView>,
    visible: bool,
    /// Hides the panel without reporting a close (`dismissing`).
    dismissing: bool,
    on_close: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    /// Where the title-bar drag picked the panel up, relative to its own corner.
    grab: Rc<std::cell::Cell<Option<Point<Pixels>>>>,
}

impl FloatingPanelController {
    /// `init(name:)`.
    pub fn new(name: impl Into<SharedString>) -> Self {
        let name = name.into();
        Self {
            name,
            title: SharedString::default(),
            placement: FloatingPanelPlacement::Automatic,
            content: None,
            visible: false,
            dismissing: false,
            on_close: None,
            grab: Rc::new(std::cell::Cell::new(None)),
        }
    }

    /// The identifier a panel is known by (`identifier`), which is its name.
    pub fn identifier(&self) -> &SharedString {
        &self.name
    }

    /// The close button's report (`onClose`).
    pub fn set_on_close(&mut self, on_close: impl Fn(&mut Window, &mut App) + 'static) {
        self.on_close = Some(Rc::new(on_close));
    }

    /// `show(title:content:placement:)`: the panel is shown with the given content, at the last
    /// place it was left (or centred on the canvas the first time).
    pub fn show(
        &mut self,
        title: impl Into<SharedString>,
        content: impl Into<AnyView>,
        placement: FloatingPanelPlacement,
        window: &mut Window,
        cx: &mut App,
    ) {
        let was_visible = self.visible;
        self.title = title.into();
        self.placement = placement;
        self.content = Some(content.into());
        self.visible = true;
        self.dismissing = false;
        let _ = was_visible;
    }

    /// `close()`: hides the panel without reporting a close.
    pub fn close(&mut self, window: &mut Window, _cx: &mut App) {
        if !self.visible {
            return;
        }
        self.dismissing = true;
        self.visible = false;
        self.dismissing = false;
        window.refresh();
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Camera Raw's three grading wheels are the widest thing a docked panel holds; their own
    /// sliders are narrowed to fit this rather than the panel being widened to fit them.
    pub const DOCKED_WIDTH: f32 = 440.0;

    /// The window point a panel opens centered on: the middle of the editor's canvas area.
    ///
    /// The AppKit walked the window's view tree for the canvas view; an overlay has no such lookup,
    /// so the port uses the chrome widths the editor knows — the rail and divider on the left, the
    /// Layers panel's remembered width on the right.
    pub fn canvas_center(window: &Window, cx: &App) -> Point<Pixels> {
        let viewport = window.viewport_size();
        let left = tool_rail::WIDTH + 1.0;
        let panel = compositor_rs_core::settings::int_value(
            crate::content_view::LAYERS_PANEL_WIDTH_KEY,
            crate::content_view::LAYERS_PANEL_DEFAULT_WIDTH as i64,
        ) as f32;
        let right = (f32::from(viewport.width) - panel).max(left + 1.0);
        let _ = cx;
        Point::new(px((left + right) / 2.0), viewport.height / 2.0)
    }

    /// `remember(_:)`: the panel's top-left corner is kept, so it reopens where it was left.
    /// A docked frame is copied from the document window; remembering it would put the next filter
    /// that shares this panel on that right edge.
    fn remember(&self, position: Point<Pixels>) {
        if self.placement == FloatingPanelPlacement::DockedToMainWindowRight {
            return;
        }
        POSITIONS.lock().insert(self.name.clone(), position);
    }

    fn element_id(&self) -> ElementId {
        ElementId::Name(format!("floating-panel-position:{}", self.name).into())
    }

    /// The focused panel's keyboard, e.g. after a click on the canvas (`refocus(_:)`).
    pub fn refocus(identifier: &str, window: &mut Window, cx: &mut App) {
        let handle = FOCUS_HANDLES.lock().get(identifier).cloned();
        if let Some(handle) = handle {
            window.focus(&handle, cx);
            window.refresh();
        }
    }

    /// The panel's own element, for the host to draw at the window's root. `None` while hidden.
    pub fn render(&mut self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let content = self.content.clone()?;
        if !self.visible {
            return None;
        }
        let viewport = window.viewport_size();
        let docked = self.placement == FloatingPanelPlacement::DockedToMainWindowRight;
        let state = window.use_keyed_state(self.element_id(), cx, |_, cx| PanelState {
            position: Point::default(),
            placed: false,
            focus: cx.focus_handle(),
        });
        // A position left from the last time this panel was open.
        let saved = POSITIONS.lock().get(&self.name).copied();
        let position = state.update(cx, |state, cx| {
            if !state.placed {
                state.position = saved.unwrap_or_else(|| Self::canvas_center(window, cx));
                state.placed = true;
            }
            state.position
        });
        let docked_position = Point::new(px(f32::from(viewport.width) - Self::DOCKED_WIDTH), Pixels::ZERO);
        let position = if docked { docked_position } else { position };
        FOCUS_HANDLES
            .lock()
            .insert(self.name.clone(), state.read(cx).focus.clone());
        // `windowDidMove(_:)`.
        self.remember(position);

        let focus = state.read(cx).focus.clone();
        let on_close = self.on_close.clone();
        let this_state = state.clone();
        let grab = self.grab.clone();
        let drag_state = state.clone();

        let title = self.title.clone();

        let header = h_flex()
            .id(ElementId::Name(format!("floating-panel-header-{}", self.name).into()))
            .items_center()
            .h(px(28.0))
            .pl(px(10.0))
            .pr(px(6.0))
            .gap(px(6.0))
            .border_b_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.10))
            .cursor(CursorStyle::OpenHand)
            .on_mouse_down(MouseButton::Left, {
                let state = this_state.clone();
                let grab = grab.clone();
                move |event: &MouseDownEvent, _, cx| {
                    let panel = state.read(cx).position;
                    grab.set(Some(event.position - panel));
                }
            })
            .on_drag(PanelDrag, |_, _, _, cx| cx.new(|_| PanelDragView))
            .on_drag_move::<PanelDrag>({
                let grab = grab.clone();
                move |event: &DragMoveEvent<PanelDrag>, _, cx| {
                    let Some(grab) = grab.get() else { return };
                    let position = event.event.position - grab;
                    drag_state.update(cx, |state, cx| {
                        state.position = position;
                        cx.notify();
                    });
                }
            })
            .child(div().flex_1().text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                Button::new(ElementId::Name(format!("floating-panel-close-{}", self.name).into()))
                    .icon(Icon::new(IconName::X).size(px(11.0)))
                    .ghost()
                    .w(px(20.0))
                    .h(px(20.0))
                    .on_click(move |event: &ClickEvent, window, cx| {
                        let _ = event;
                        if let Some(on_close) = &on_close {
                            on_close(window, cx);
                        }
                    }),
            );

        let card = v_flex()
            .id(ElementId::Name(format!("floating-panel-{}", self.name).into()))
            .track_focus(&focus)
            .w(px(Self::DOCKED_WIDTH))
            .max_h(px(f32::from(viewport.height)))
            .rounded(px(8.0))
            .border_1()
            .border_color(hsla(0.0, 0.0, 1.0, 0.14))
            .bg(hsla(0.0, 0.0, 0.14, 0.98))
            .shadow_lg()
            .overflow_hidden()
            .when(docked, |this| this.h(px(f32::from(viewport.height))))
            .on_mouse_down(MouseButton::Left, {
                let focus = focus.clone();
                move |_, window, cx| window.focus(&focus, cx)
            })
            .child(header)
            .child(div().flex_1().min_h(px(0.0)).child(content));

        Some(
            deferred(
                div()
                    .absolute()
                    .left(position.x)
                    .top(position.y)
                    .child(card),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}
