//! The New Canvas welcome sheet: the size fields, the preset menu and the actions that start an
//! empty editor (port of `UI/NewCanvasSheet.swift`).
//!
//! It is hosted over the canvas while the session has no document (`ContentView`'s `welcome`), not
//! as a modal. "Open project" dispatches `compositor_rs::OpenProject` — the app answers it with its
//! open panel — and "Create canvas" calls the caller's `onCreate` (`ProjectController.newCanvas`
//! in the Swift), or `EditorSession::create_document` when no callback was given.

use std::sync::Arc;

use compositor_rs_core::limits::MAX_SIDE;
use compositor_rs_session::EditorSession;

use crate::actions::OpenProject;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{h_flex, v_flex, Disableable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `.frame(maxWidth: 500)`. The sheet is hosted over the canvas, so SwiftUI's flexible frame
/// resolves to the maximum there; a fixed width is the same result.
const MAX_WIDTH: f32 = 500.0;
/// `.font(.callout)`: 12 points, as macOS sizes it (the port's other `.callout`s agree).
const CALLOUT_SIZE: f32 = 12.0;
/// `.padding(28)`.
const PADDING: f32 = 28.0;

/// The error orange the Swift's `.orange` is (SwiftUI orange, #FF9500).
const ORANGE: Hsla = hsla(0.075, 1.0, 0.5, 1.0);

/// The welcome sheet's `onCreate` (`onOpen` is not a callback in the port: the sheet dispatches the
/// `compositor_rs::OpenProject` action, which the app answers).
#[derive(Clone, Default)]
pub struct NewCanvasSheetCallbacks {
    pub on_create: Option<Arc<dyn Fn(usize, usize, &mut App)>>,
}

/// New Canvas sizes: common screens and resolutions, in pixels, upright as the device is usually held.
pub struct CanvasPreset {
    pub title: &'static str,
    pub width: usize,
    pub height: usize,
}

impl CanvasPreset {
    pub const fn new(title: &'static str, width: usize, height: usize) -> Self {
        Self { title, width, height }
    }

    /// `Identifiable`'s `id`.
    pub fn id(&self) -> &'static str {
        self.title
    }

    /// Resolutions, Apple screens, then social formats; the menu divides them.
    pub const GROUPS: [&'static [CanvasPreset]; 3] = [
        &[
            CanvasPreset::new("4K", 3840, 2160),
            CanvasPreset::new("1440p", 2560, 1440),
            CanvasPreset::new("1080p", 1920, 1080),
        ],
        &[
            CanvasPreset::new("iPhone 18 Pro", 1206, 2622),
            CanvasPreset::new("iPhone 18 Pro Max", 1320, 2868),
            CanvasPreset::new("MacBook Pro 14\"", 3024, 1964),
            CanvasPreset::new("MacBook Pro 16\"", 3456, 2234),
            CanvasPreset::new("Studio Display", 5120, 2880),
        ],
        &[
            CanvasPreset::new("Instagram Square", 1080, 1080),
            CanvasPreset::new("Instagram Portrait", 1080, 1350),
            CanvasPreset::new("Instagram Story", 1080, 1920),
            CanvasPreset::new("YouTube Thumb", 1080, 608),
        ],
    ];

    /// `CanvasPreset.all`: every group flattened.
    pub fn all() -> impl Iterator<Item = &'static CanvasPreset> {
        Self::GROUPS.iter().flat_map(|group| group.iter())
    }

    /// Whether the fields spell this preset out (`String($0.width) == width && …`).
    pub fn matches(&self, width: &str, height: &str) -> bool {
        self.width.to_string() == width && self.height.to_string() == height
    }
}

/// The welcome sheet.
pub struct NewCanvasSheet {
    session: Entity<EditorSession>,
    callbacks: NewCanvasSheetCallbacks,
    /// The two text fields, built on the first render — the first time a window is available — as
    /// the Swift's `@State` fields are their own storage.
    width_input: Option<Entity<InputState>>,
    height_input: Option<Entity<InputState>>,
    /// Whether the clipboard has been consulted (`suggestedClipboardSize`).
    suggested_clipboard_size: bool,
    /// The preset menu's open state, so choosing a size can close it.
    menu_open: bool,
    /// The session and the two fields, observed so the sheet follows them.
    _subscriptions: Vec<Subscription>,
}

impl NewCanvasSheet {
    pub fn new(
        session: Entity<EditorSession>,
        callbacks: NewCanvasSheetCallbacks,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&session, |_, _, cx| cx.notify());
        Self {
            session,
            callbacks,
            width_input: None,
            height_input: None,
            suggested_clipboard_size: false,
            menu_open: false,
            _subscriptions: vec![observation],
        }
    }

    /// The width field's text.
    pub fn width_text(&self, cx: &App) -> String {
        self.width_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// The height field's text.
    pub fn height_text(&self, cx: &App) -> String {
        self.height_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Whether both fields hold a whole number in `1...MAX_SIDE` (`valid`).
    pub fn valid(&self, cx: &App) -> bool {
        valid_dimension(&self.width_text(cx)).is_some() && valid_dimension(&self.height_text(cx)).is_some()
    }

    /// The preset the fields match, or `None` (Custom).
    pub fn preset(&self, cx: &App) -> Option<&'static CanvasPreset> {
        let (width, height) = (self.width_text(cx), self.height_text(cx));
        CanvasPreset::all().find(|preset| preset.matches(&width, &height))
    }

    /// The fields and the clipboard suggestion, once the window exists (`.onAppear`).
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.width_input.is_some() {
            return;
        }
        let width = cx.new(|cx| InputState::new(window, cx).placeholder("Width"));
        let height = cx.new(|cx| InputState::new(window, cx).placeholder("Height"));
        self._subscriptions.push(cx.subscribe_in(
            &width,
            window,
            |_: &mut Self, _, _: &InputEvent, _, cx| cx.notify(),
        ));
        self._subscriptions.push(cx.subscribe_in(
            &height,
            window,
            |_: &mut Self, _, _: &InputEvent, _, cx| cx.notify(),
        ));

        let mut width_text = "1920".to_string();
        let mut height_text = "1080".to_string();
        if !self.suggested_clipboard_size {
            self.suggested_clipboard_size = true;
            let skipped = self.session.update(cx, |session, _| {
                let skipped = session.skips_initial_clipboard_canvas_size;
                session.skips_initial_clipboard_canvas_size = false;
                skipped
            });
            if !skipped {
                let suggested = self.session.update(cx, |session, _| clipboard_dimensions(session));
                if let Some((width, height)) = suggested {
                    width_text = width.to_string();
                    height_text = height.to_string();
                }
            }
        }
        width.update(cx, |state, cx| state.set_value(width_text, window, cx));
        height.update(cx, |state, cx| state.set_value(height_text, window, cx));

        // `.focusedField = .width`.
        let focus = width.read(cx).focus_handle(cx);
        window.focus(&focus, cx);

        self.width_input = Some(width);
        self.height_input = Some(height);
    }

    /// "Create canvas": the caller's `onCreate`, or a document with one empty layer.
    fn create_canvas(&mut self, cx: &mut App) {
        let (Some(width), Some(height)) = (
            valid_dimension(&self.width_text(cx)),
            valid_dimension(&self.height_text(cx)),
        ) else {
            return;
        };
        if let Some(on_create) = self.callbacks.on_create.clone() {
            on_create(width, height, cx);
            return;
        }
        self.session.update(cx, |session, _| session.create_document(width, height, true));
    }

    /// One size field: its title, the field itself and the "px" unit.
    fn dimension(&self, title: &'static str, width_axis: bool, disabled: bool) -> Div {
        let input = if width_axis {
            self.width_input.clone()
        } else {
            self.height_input.clone()
        }
        .expect("the fields are built on the first render");
        v_flex()
            .flex_1()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(CALLOUT_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .p(px(12.0))
                    .rounded(px(7.0))
                    .bg(hsla(0.0, 0.0, 1.0, 0.10))
                    .child(Input::new(&input).flex_1().appearance(false).disabled(disabled))
                    .child(div().text_color(hsla(0.0, 0.0, 1.0, 0.6)).child("px")),
            )
    }

    /// The three-dot preset menu (`Menu { Picker(…) }`): Custom, then the groups divided.
    fn preset_menu(&self, preset: Option<&'static CanvasPreset>, disabled: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.width_input.clone().expect("the fields are built on the first render");
        let height = self.height_input.clone().expect("the fields are built on the first render");
        let sheet = cx.entity().downgrade();
        Popover::new("new-canvas-presets")
            .open(self.menu_open)
            .on_open_change({
                let sheet = sheet.clone();
                move |open, _, cx| {
                    if let Some(sheet) = sheet.upgrade() {
                        sheet.update(cx, |sheet, cx| {
                            sheet.menu_open = *open;
                            cx.notify();
                        });
                    }
                }
            })
            .trigger(
                Button::new("new-canvas-presets-trigger")
                    .ghost()
                    .disabled(disabled)
                    .tooltip("Preset sizes for screens and common formats")
                    .accessibility_label("Preset sizes")
                    // Three dots drawn exactly (a rotated symbol keeps its sideways width), flush
                    // with the fields' right edge; the frame keeps it easy to click.
                    .child(
                        v_flex()
                            .items_end()
                            .justify_center()
                            .gap(px(2.5))
                            .w(px(28.0))
                            .h(px(28.0))
                            .children((0..3).map(|_| {
                                div()
                                    .size(px(2.5))
                                    .rounded_full()
                                    .bg(cx.theme().foreground)
                            })),
                    )
                    // `.fixedSize()`: no button padding either, so the dots stay flush with the
                    // fields' right edge.
                    .px(px(0.0))
                    .h(px(28.0)),
            )
            .content(move |_, _, _| {
                let mut menu = v_flex().w(px(240.0)).gap(px(2.0)).child(preset_row(
                    "Custom",
                    preset.is_none(),
                    None,
                    &width,
                    &height,
                    &sheet,
                ));
                for group in CanvasPreset::GROUPS {
                    menu = menu.child(Separator::horizontal());
                    for candidate in group {
                        menu = menu.child(preset_row(
                            candidate.title,
                            preset.map(|preset| preset.id()) == Some(candidate.id()),
                            Some(candidate),
                            &width,
                            &height,
                            &sheet,
                        ));
                    }
                }
                menu
            })
    }
}

/// `Image(systemName: "multiply").foregroundStyle(.tertiary)`: a small stroked × between the two
/// fields. The text glyph (U+2715) is a much larger, heavier cross than the symbol, so it is drawn.
fn multiply_mark() -> impl IntoElement {
    /// `.tertiary`, fainter than the `.secondary` text either side of it.
    const TERTIARY: Hsla = hsla(0.0, 0.0, 1.0, 0.3);

    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            // A 9.5×9.5 cross, centred in the symbol's 13-point line box.
            let x = bounds.origin.x;
            let y = bounds.origin.y;
            let (left, right) = (x + px(0.25), x + px(9.75));
            let (top, bottom) = (y + px(1.75), y + px(11.25));
            for (from, to) in [
                (point(left, top), point(right, bottom)),
                (point(right, top), point(left, bottom)),
            ] {
                let mut path = PathBuilder::stroke(px(1.0));
                path.move_to(from);
                path.line_to(to);
                if let Ok(path) = path.build() {
                    window.paint_path(path, TERTIARY);
                }
            }
        },
    )
    .w(px(10.0))
    .h(px(13.0))
}

/// One row of the preset menu: choosing a preset fills both fields in; Custom leaves them alone.
fn preset_row(
    title: &'static str,
    selected: bool,
    preset: Option<&'static CanvasPreset>,
    width: &Entity<InputState>,
    height: &Entity<InputState>,
    sheet: &WeakEntity<NewCanvasSheet>,
) -> impl IntoElement {
    let row = div()
        .id(ElementId::Name(format!("new-canvas-preset-{title}").into()))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .child(title);
    let mut row = row;
    if let Some(preset) = preset {
        let (width, height, sheet) = (width.clone(), height.clone(), sheet.clone());
        row = row.cursor(CursorStyle::PointingHand).hover(|style| style.bg(hsla(0.0, 0.0, 1.0, 0.08))).on_click(
            move |_, window, cx| {
                width.update(cx, |state, cx| state.set_value(preset.width.to_string(), window, cx));
                height.update(cx, |state, cx| state.set_value(preset.height.to_string(), window, cx));
                if let Some(sheet) = sheet.upgrade() {
                    sheet.update(cx, |sheet, cx| {
                        sheet.menu_open = false;
                        cx.notify();
                    });
                }
            },
        );
    }
    row.when(selected, |row| row.child(Icon::new(IconName::Check).size(px(11.0))))
}

impl Render for NewCanvasSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_fields(window, cx);
        let disabled = {
            let session = self.session.read(cx);
            session.is_importing || session.shows_busy
        };
        let valid = self.valid(cx);
        let preset = self.preset(cx);
        let session = self.session.clone();

        v_flex()
            .w(px(MAX_WIDTH))
            .p(px(PADDING))
            .gap(px(24.0))
            .child(
                h_flex()
                    .items_center()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("New canvas"),
                    )
                    .child(div().flex_1())
                    .child(self.preset_menu(preset, disabled, cx)),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap(px(16.0))
                    .child(self.dimension("Width", true, disabled))
                    .child(div().pt(px(20.0)).child(multiply_mark()))
                    .child(self.dimension("Height", false, disabled)),
            )
            .child(
                div()
                    .text_size(px(CALLOUT_SIZE))
                    .text_color(if valid { hsla(0.0, 0.0, 1.0, 0.6) } else { ORANGE })
                    .child(if valid {
                        "Transparent canvas · sRGB".to_string()
                    } else {
                        format!("Enter whole numbers from 1 to {} pixels.", grouped(MAX_SIDE))
                    }),
            )
            .child(
                h_flex()
                    .gap(px(10.0))
                    .child(
                        Button::new("new-canvas-open")
                            .label("Open project")
                            .outline()
                            .disabled(disabled)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenProject), cx)
                            }),
                    )
                    .child(
                        Button::new("new-canvas-import")
                            .label("Import image")
                            .outline()
                            .disabled(disabled)
                            .on_click({
                                let session = session.clone();
                                move |_, _, cx| {
                                    session.update(cx, |session, _| session.shows_importer = true)
                                }
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("new-canvas-create")
                            .label("Create canvas")
                            .primary()
                            .accessibility_id("createCanvas")
                            .disabled(!(valid && !disabled))
                            .on_click({
                                let sheet = cx.entity().downgrade();
                                move |_, _, cx| {
                                    if let Some(sheet) = sheet.upgrade() {
                                        sheet.update(cx, |sheet, cx| sheet.create_canvas(cx));
                                    }
                                }
                            }),
                    ),
            )
    }
}

/// `CanvasDocument.validDimension`: whole numbers from 1 to `MAX_SIDE`.
pub fn valid_dimension(text: &str) -> Option<usize> {
    let value = text.trim().parse::<i64>().ok()?;
    (1..=MAX_SIDE as i64).contains(&value).then_some(value as usize)
}

/// `Int.formatted()` for the message's limit: grouped digits (`30,000`).
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() * 4 / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

/// `NewCanvasSheet.clipboardDimensions()`: the pasteboard image's pixel size, when it is a usable
/// document size. The platform's [`compositor_rs_session::clipboard::Clipboard`] decodes the image.
fn clipboard_dimensions(session: &mut EditorSession) -> Option<(usize, usize)> {
    let image = session.clipboard.as_mut()?.read_image()?;
    EditorSession::clipboard_canvas_size(image.width() as i64, image.height() as i64, None)
}

#[cfg(test)]
mod tests {
    // Imported explicitly: `use super::*` would re-import gpui-kit's `test` macro and shadow the
    // built-in `#[test]` attribute (the dev-dependency enables `test-support`).
    use super::{grouped, valid_dimension, CanvasPreset};
    use compositor_rs_core::limits::MAX_SIDE;

    #[test]
    fn presets_keep_their_order_and_sizes() {
        let titles: Vec<&str> = CanvasPreset::all().map(|preset| preset.title).collect();
        assert_eq!(
            titles,
            [
                "4K", "1440p", "1080p", "iPhone 18 Pro", "iPhone 18 Pro Max", "MacBook Pro 14\"",
                "MacBook Pro 16\"", "Studio Display", "Instagram Square", "Instagram Portrait",
                "Instagram Story", "YouTube Thumb"
            ]
        );
        let iphone = CanvasPreset::all().find(|preset| preset.title == "iPhone 18 Pro").unwrap();
        assert_eq!((iphone.width, iphone.height), (1206, 2622));
    }

    #[test]
    fn dimensions_run_from_one_to_the_document_limit() {
        assert_eq!(valid_dimension("1920"), Some(1920));
        assert_eq!(valid_dimension(" 42 "), Some(42));
        assert_eq!(valid_dimension("0"), None);
        assert_eq!(valid_dimension("30001"), None);
        assert_eq!(valid_dimension("19.2"), None);
        assert_eq!(valid_dimension(""), None);
        assert_eq!(valid_dimension("abc"), None);
        assert_eq!(valid_dimension(&MAX_SIDE.to_string()), Some(MAX_SIDE));
    }

    #[test]
    fn the_limit_message_groups_its_digits() {
        assert_eq!(grouped(30_000), "30,000");
        assert_eq!(grouped(1_206), "1,206");
        assert_eq!(grouped(999), "999");
    }

    #[test]
    fn a_preset_matches_only_the_exact_fields() {
        let preset = CanvasPreset::new("1080p", 1920, 1080);
        assert!(preset.matches("1920", "1080"));
        assert!(!preset.matches("1920", "1081"));
    }
}
