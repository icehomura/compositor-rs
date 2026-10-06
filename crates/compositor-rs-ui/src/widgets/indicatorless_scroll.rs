//! The scroll view without scrollers (port of `UI/IndicatorlessScrollView.swift`).
//!
//! The tool rail scrolls when it overflows without showing or reserving space for a macOS
//! scroller, even when the system setting always shows scroll bars. The Swift view was an
//! `NSScrollView` with both scrollers off and no horizontal elasticity; here the scroll area is
//! gpui's own `overflow_y_scroll`, which draws no scroller unless a `Scrollbar` child is added —
//! and none is.
//!
//! ```no_run
//! use compositor_rs_ui::widgets::indicatorless_scroll::IndicatorlessScrollView;
//! use gpui_kit::div;
//!
//! # let tools = div();
//! IndicatorlessScrollView::rail("tool-rail").child(tools);
//! ```

use gpui_kit::*;

/// The width of the tool rail's content (the Swift container's `NSSize(width: 56, …)`).
pub const RAIL_WIDTH: f32 = 56.0;

/// Whether content taller than the view may rubber-band: the Swift container allowed vertical
/// elasticity only once the content stood more than a point taller than the view
/// (`height > contentView.bounds.height + 1`).
pub fn allows_vertical_elasticity(content_height: f32, bounds_height: f32) -> bool {
    content_height > bounds_height + 1.0
}

/// The scroll view of the tool rail: it scrolls, but never shows a scroller.
pub struct IndicatorlessScrollView {
    id: ElementId,
    width: Option<f32>,
    scroll: Option<ScrollHandle>,
    children: Vec<AnyElement>,
}

impl IndicatorlessScrollView {
    /// A scroll view over the height it is given, taking its width from its container.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            width: None,
            scroll: None,
            children: Vec::new(),
        }
    }

    /// The tool rail: `RAIL_WIDTH` wide, the width the Swift container laid its content out at.
    pub fn rail(id: impl Into<ElementId>) -> Self {
        Self::new(id).width(RAIL_WIDTH)
    }

    /// A fixed width, as the rail's own frame had.
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// Keeps `handle` on the scroll position, for a caller that follows it.
    pub fn track_scroll(mut self, handle: &ScrollHandle) -> Self {
        self.scroll = Some(handle.clone());
        self
    }

    /// Adds content.
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Adds several children, as the Swift `@ViewBuilder` content could be any number of views.
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(IntoElement::into_any_element));
        self
    }
}

impl IntoElement for IndicatorlessScrollView {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl RenderOnce for IndicatorlessScrollView {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            id,
            width,
            scroll,
            children,
        } = self;

        let mut root = div()
            .id(id)
            .test_support()
            .flex()
            .flex_col()
            .h_full()
            // The whole point of the view: it scrolls, with no scroller drawn or reserved.
            .overflow_y_scroll()
            .children(children);

        if let Some(width) = width {
            root = root.w(px(width)).flex_none();
        }
        if let Some(scroll) = scroll {
            root = root.track_scroll(&scroll);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::{IndicatorlessScrollView, allows_vertical_elasticity};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, InteractiveElement as _,
        IntoElement, ParentElement as _, Render, ScrollDelta, ScrollHandle, Styled as _,
        TestAppContext, TestSupportExt as _, Window, WindowBounds, WindowOptions, div, point, px,
        size,
    };

    #[test]
    fn a_view_only_rubber_bands_once_its_content_is_taller_than_it() {
        let bounds = 400.0;
        assert!(!allows_vertical_elasticity(400.0, bounds));
        assert!(!allows_vertical_elasticity(401.0, bounds));
        assert!(allows_vertical_elasticity(401.5, bounds));
        assert!(allows_vertical_elasticity(800.0, bounds));
    }

    struct Rail {
        scroll: ScrollHandle,
    }

    impl Render for Rail {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                IndicatorlessScrollView::rail("tool-rail")
                    .track_scroll(&self.scroll)
                    .children((0..16usize).map(|index| {
                        div()
                            .id(("tool", index))
                            .test_support()
                            .flex_none()
                            .h(px(44.))
                            .child("tool")
                    })),
            )
        }
    }

    #[gpui_kit::test]
    fn the_tool_rail_scrolls_without_a_scroller(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let scroll = ScrollHandle::new();
        let (window, _view) = open_window(cx, |_, cx| {
            cx.new(|_| Rail {
                scroll: scroll.clone(),
            })
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // The rail is 56 wide, as the Swift container laid its content out.
            assert_eq!(window.find("tool-rail").bounds().size.width, px(56.));
            assert!(window.find(("tool", 0usize)).visible());
            assert!(!window.find(("tool", 15usize)).visible());

            window.scroll(
                "tool-rail",
                ScrollDelta::Pixels(point(px(0.), px(-400.))),
                cx,
            );
            assert!(scroll.offset().y < px(0.), "the rail scrolled");
            assert!(!window.find(("tool", 0usize)).visible());
            assert!(window.find(("tool", 15usize)).visible());
        })
        .unwrap();
    }

    /// Opens a window the way gpui-kit's own tests do: the production entry point with test bounds.
    fn open_window<V: Render>(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
    ) -> (AnyWindowHandle, Entity<V>) {
        cx.update(|cx| {
            let bounds = gpui_kit::Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(200.), px(300.)),
            };
            let (window, content) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                build,
            )
            .expect("open the test window");
            (window, content)
        })
    }
}
