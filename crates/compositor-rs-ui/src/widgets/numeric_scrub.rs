//! The drag-scrub target for numeric labels and units (port of `UI/NumericScrub.swift`).
//!
//! A label or unit beside a number field becomes a drag target for that field's value: dragging
//! sideways moves the value by the pointer's travel times a sensitivity, snapped to the control's
//! step and held inside its range, and the pointer shows a left-right resize cursor over it.
//!
//! The Swift modifier kept a `startValue` and an `isHovering` flag per view and put the arrow
//! cursor back when the drag ended; gpui draws the cursor from the element's own style, so
//! [`ScrubTarget`] tracks only the drag itself.
//!
//! ```no_run
//! use compositor_rs_ui::widgets::numeric_scrub::{NumericScrub, Scrubbable};
//! use gpui_kit::prelude::*;
//! use gpui_kit::div;
//!
//! # let value = 40.0_f64;
//! # let on_change = |_value: f64, _window: &mut gpui_kit::Window, _cx: &mut gpui_kit::App| {};
//! div()
//!     .child("Size")
//!     .scrubbable(
//!         "brush-size",
//!         NumericScrub::new(value, 1.0, (1.0, 2000.0)).step(1.0).on_change(on_change),
//!     );
//! ```

use std::rc::Rc;

use gpui_kit::*;

use super::slider_snap::{clamped, snapped_to_step};

/// The multiple of the step one Shift-arrow press adds (`ArrowStepper.listen`).
const SHIFT_STEP_MULTIPLIER: f64 = 10.0;
/// What one fine press takes off the step — the Control/Alt adjustment the port's fields offer,
/// where macOS's own sliders ignored every modifier but Shift (`altIncrementValue` defaults to
/// −1 and the app never sets it).
const FINE_STEP_DIVISOR: f64 = 10.0;

/// The amount one arrow-key press adds, from `ArrowStepper.listen`: one step, ten with Shift.
/// Control and Alt take a tenth of a step, so a fine press and a coarse one held together cancel
/// out.
pub fn arrow_step_amount(step: f64, modifiers: Modifiers) -> f64 {
    let coarse = if modifiers.shift {
        SHIFT_STEP_MULTIPLIER
    } else {
        1.0
    };
    let fine = if modifiers.control || modifiers.alt {
        FINE_STEP_DIVISOR
    } else {
        1.0
    };
    step * coarse / fine
}

/// The value an Up-arrow press lands on (`ArrowStepper.listen`'s `value + amount`, or
/// `value - amount` for Down). Each field's own binding keeps the result in range, as the Swift
/// fields did.
pub fn arrow_stepped(value: f64, step: f64, up: bool, modifiers: Modifiers) -> f64 {
    let amount = arrow_step_amount(step, modifiers);
    if up { value + amount } else { value - amount }
}

/// What a scrub drag does to a value: its sensitivity, its range, the grid it snaps to, and who to
/// tell when it starts, changes and ends.
pub struct NumericScrub {
    value: f64,
    sensitivity: f64,
    range: (f64, f64),
    step: Option<f64>,
    rounds_to_whole: bool,
    on_change: Option<Box<dyn Fn(f64, &mut Window, &mut App)>>,
    on_start: Option<Box<dyn Fn(&mut Window, &mut App)>>,
    on_end: Option<Box<dyn Fn(&mut Window, &mut App)>>,
}

impl NumericScrub {
    /// A scrub for `value` that moves it by `sensitivity` per point of travel and holds it inside
    /// `range`.
    pub fn new(value: f64, sensitivity: f64, range: (f64, f64)) -> Self {
        Self {
            value,
            sensitivity,
            range,
            step: None,
            rounds_to_whole: false,
            on_change: None,
            on_start: None,
            on_end: None,
        }
    }

    /// Dragged values snap to multiples of this, one for whole numbers; typing can still give any
    /// value, so the field's own binding is left alone.
    pub fn step(mut self, step: f64) -> Self {
        self.step = Some(step);
        self
    }

    /// Turns the dragged value into a whole number, as the modifier's `Binding<Int>` overload did.
    pub fn whole_numbers(mut self) -> Self {
        self.rounds_to_whole = true;
        self
    }

    /// Called with the dragged value while the drag is live.
    pub fn on_change(mut self, listener: impl Fn(f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(listener));
        self
    }

    /// Called when the drag begins — the moment a Swift `onStart` edit is opened.
    pub fn on_start(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_start = Some(Box::new(listener));
        self
    }

    /// Called when the drag ends, the moment a Swift `onEnd` edit is closed.
    pub fn on_end(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_end = Some(Box::new(listener));
        self
    }

    /// The value a drag reports: the value it started from, moved by the drag's horizontal travel
    /// times the sensitivity, snapped to the step and clamped to the range.
    pub fn dragged(&self, start: f64, translation_x: f64) -> f64 {
        let mut proposed = start + translation_x * self.sensitivity;
        if let Some(step) = self.step.filter(|step| *step > 0.0) {
            proposed = snapped_to_step(proposed, step);
        }
        let proposed = clamped(proposed, self.range.0, self.range.1);
        if self.rounds_to_whole {
            proposed.round()
        } else {
            proposed
        }
    }
}

/// Attaches [`NumericScrub`] to an element, making it a drag target for a value.
pub trait Scrubbable: IntoElement + Sized {
    /// Makes this label or unit a drag target for `scrub`'s value. `id` names the label: two
    /// scrubbable elements rendered side by side must be given different ids.
    fn scrubbable(self, id: impl Into<ElementId>, scrub: NumericScrub) -> ScrubTarget<Self> {
        ScrubTarget {
            element: self,
            scrub,
            id: id.into(),
        }
    }
}

impl<E: IntoElement + Sized> Scrubbable for E {}

/// The element [`Scrubbable::scrubbable`] produces: the original element with drag listeners.
pub struct ScrubTarget<E> {
    element: E,
    scrub: NumericScrub,
    id: ElementId,
}

/// The value a scrub drag carries; the drag itself moves a number, not a payload.
#[derive(Clone)]
struct ScrubDrag;

/// Where the pointer was when the label was pressed and what the value was then: a drag measures
/// its travel from here, as `DragGesture`'s `translation` measures from the press.
struct ScrubPress {
    value: f64,
    origin_x: f64,
}

/// One label's live scrub (`NumericScrub`'s `startValue`).
#[derive(Default)]
struct ScrubState {
    press: Option<ScrubPress>,
    /// Whether the gesture became a drag: a plain click neither opens nor closes an edit.
    dragged: bool,
}

/// The empty view a scrub drag carries: scrubbing moves a number, not something on screen, so the
/// drag shows no ghost under the pointer.
struct ScrubDragView;

impl Render for ScrubDragView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl<E: IntoElement + 'static> IntoElement for ScrubTarget<E> {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl<E: IntoElement + 'static> RenderOnce for ScrubTarget<E> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ScrubTarget { element, scrub, id } = self;
        let state =
            window.use_keyed_state((id.clone(), "scrub-state"), cx, |_, _| ScrubState::default());
        let start_value = scrub.value;
        let scrub = Rc::new(scrub);

        let press = {
            let state = state.clone();
            move |event: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                let press = ScrubPress {
                    value: start_value,
                    origin_x: f64::from(event.position.x),
                };
                state.update(cx, |state, _| {
                    state.press = Some(press);
                    state.dragged = false;
                });
            }
        };

        let begin = {
            let state = state.clone();
            let scrub = scrub.clone();
            move |_: &ScrubDrag, _: Point<Pixels>, window: &mut Window, cx: &mut App| {
                state.update(cx, |state, _| state.dragged = true);
                if let Some(on_start) = &scrub.on_start {
                    on_start(window, cx);
                }
                cx.new(|_| ScrubDragView)
            }
        };

        let change = {
            let state = state.clone();
            let scrub = scrub.clone();
            move |event: &DragMoveEvent<ScrubDrag>, window: &mut Window, cx: &mut App| {
                let Some((start, origin_x)) = state
                    .read(cx)
                    .press
                    .as_ref()
                    .map(|press| (press.value, press.origin_x))
                else {
                    return;
                };
                let translation_x = f64::from(event.event.position.x) - origin_x;
                let value = scrub.dragged(start, translation_x);
                if let Some(on_change) = &scrub.on_change {
                    on_change(value, window, cx);
                }
            }
        };

        let release = |state: Entity<ScrubState>, scrub: Rc<NumericScrub>| {
            move |_: &MouseUpEvent, window: &mut Window, cx: &mut App| {
                let dragged = state.update(cx, |state, _| {
                    state.press = None;
                    std::mem::take(&mut state.dragged)
                });
                if dragged {
                    if let Some(on_end) = &scrub.on_end {
                        on_end(window, cx);
                    }
                }
            }
        };

        div()
            .id(id)
            .test_support()
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(MouseButton::Left, press)
            .on_drag(ScrubDrag, begin)
            .on_drag_move::<ScrubDrag>(change)
            .on_mouse_up(MouseButton::Left, release(state.clone(), scrub.clone()))
            .on_mouse_up_out(MouseButton::Left, release(state, scrub))
            .child(element)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::{NumericScrub, Scrubbable, arrow_step_amount, arrow_stepped};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, IntoElement, Modifiers,
        ParentElement as _, Render, Styled as _, TestAppContext, TestSupportExt as _, Window,
        WindowBounds, WindowOptions, div, point, px, size,
    };

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Default::default()
        }
    }

    fn fine() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    #[test]
    fn arrow_keys_step_once_and_ten_times_as_far_with_shift() {
        assert_eq!(arrow_step_amount(1.0, Modifiers::default()), 1.0);
        assert_eq!(arrow_step_amount(1.0, shift()), 10.0);
        assert_eq!(arrow_step_amount(2.0, shift()), 20.0);
        assert_eq!(arrow_stepped(40.0, 1.0, true, Modifiers::default()), 41.0);
        assert_eq!(arrow_stepped(40.0, 1.0, true, shift()), 50.0);
        assert_eq!(arrow_stepped(40.0, 1.0, false, shift()), 30.0);
    }

    #[test]
    fn control_and_alt_take_a_tenth_of_a_step() {
        assert_eq!(arrow_step_amount(1.0, fine()), 0.1);
        assert_eq!(
            arrow_step_amount(
                10.0,
                Modifiers {
                    alt: true,
                    ..Default::default()
                }
            ),
            1.0
        );
        // A fine press and a coarse one held together cancel out.
        assert_eq!(
            arrow_step_amount(
                1.0,
                Modifiers {
                    shift: true,
                    control: true,
                    ..Default::default()
                }
            ),
            1.0
        );
    }

    /// `NumericScrub`'s `onChanged`: `start + translation × sensitivity`, snapped and clamped.
    #[test]
    fn a_drag_moves_the_value_by_its_travel_times_the_sensitivity() {
        let brush = NumericScrub::new(40.0, 1.0, (1.0, 2000.0));
        assert_eq!(brush.dragged(40.0, 20.0), 60.0);
        assert_eq!(brush.dragged(40.0, -20.0), 20.0);

        let snapped = NumericScrub::new(40.0, 1.0, (1.0, 2000.0)).step(1.0);
        assert_eq!(snapped.dragged(40.0, 12.4), 52.0);
        assert_eq!(snapped.dragged(40.0, 12.6), 53.0);

        // `Text("Hardness").scrubbable(sensitivity: 0.01, value: …, range: 0...1)`.
        let hardness = NumericScrub::new(0.5, 0.01, (0.0, 1.0));
        assert_eq!(hardness.dragged(0.5, 10.0), 0.6);
        assert_eq!(hardness.dragged(0.5, 100.0), 1.0);
        assert_eq!(hardness.dragged(0.5, -100.0), 0.0);

        // The `Binding<Int>` overload rounds what it reports to whole numbers.
        let whole = NumericScrub::new(0.0, 0.5, (-100.0, 100.0)).whole_numbers();
        assert_eq!(whole.dragged(0.0, 1.0), 1.0);
        assert_eq!(whole.dragged(0.0, 3.0), 2.0);
    }

    struct Label {
        value: f64,
        changes: Rc<RefCell<Vec<f64>>>,
        started: Rc<Cell<usize>>,
        ended: Rc<Cell<usize>>,
    }

    impl Render for Label {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let changes = self.changes.clone();
            let started = self.started.clone();
            let ended = self.ended.clone();
            let label = cx.entity().downgrade();
            div().size_full().flex().items_center().child(
                div()
                    .child("Size")
                    .scrubbable(
                        "size-label",
                        NumericScrub::new(self.value, 1.0, (1.0, 2000.0))
                            .step(1.0)
                            .on_change(move |value, _, cx| {
                                changes.borrow_mut().push(value);
                                if let Some(label) = label.upgrade() {
                                    label.update(cx, |label, cx| {
                                        label.value = value;
                                        cx.notify();
                                    });
                                }
                            })
                            .on_start({
                                let started = started.clone();
                                move |_, _| started.set(started.get() + 1)
                            })
                            .on_end({
                                let ended = ended.clone();
                                move |_, _| ended.set(ended.get() + 1)
                            }),
                    ),
            )
        }
    }

    #[gpui_kit::test]
    fn dragging_the_label_scrubs_the_value(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let started = Rc::new(Cell::new(0));
        let ended = Rc::new(Cell::new(0));
        let (window, _label) = open_window(cx, {
            let changes = changes.clone();
            let started = started.clone();
            let ended = ended.clone();
            |_, cx| {
                cx.new(|_| Label {
                    value: 40.0,
                    changes,
                    started,
                    ended,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let from = window.find("size-label").bounds().center();
            let to = point(from.x + px(20.0), from.y);
            window.drag(from, to, cx);

            assert_eq!(started.get(), 1, "the drag opened exactly one edit");
            assert_eq!(ended.get(), 1, "and closed exactly one");
            let changes = changes.borrow();
            assert_eq!(
                changes.last(),
                Some(&60.0),
                "40 + 20 points of travel at a sensitivity of 1: {changes:?}"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_click_on_the_label_opens_and_closes_nothing(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_kit::init(cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let started = Rc::new(Cell::new(0));
        let ended = Rc::new(Cell::new(0));
        let (window, _label) = open_window(cx, {
            let changes = changes.clone();
            let started = started.clone();
            let ended = ended.clone();
            |_, cx| {
                cx.new(|_| Label {
                    value: 40.0,
                    changes,
                    started,
                    ended,
                })
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("size-label", cx);
            assert!(changes.borrow().is_empty());
            assert_eq!(started.get(), 0);
            assert_eq!(ended.get(), 0);
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
                size: size(px(200.), px(60.)),
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
