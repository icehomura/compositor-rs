//! PSD conversion: the sheet that says what a Photoshop file needs converted before Compositor
//! takes it in (port of `UI/PSDConversionSheet.swift`).
//!
//! The sheet only reports and asks: its two buttons call the `finish` closure the app hands in — the
//! port's `EditorSession::finish_conversion(confirmed:host:)` call site — so the sheet itself stays
//! ignorant of session internals, and the app pushes the rendered card as an absolute child (the
//! Swift's `.sheet` modifier has no equivalent inside this view).
//!
//! Substitutions: `.roundedControls()` (the capsule button shape SwiftUI put on this sheet) is the
//! component library's button shape, and `List`'s own chrome (its row insets, separators and scroll
//! view) is the port's plain column of conversion rows. The Swift buttons' `.keyboardShortcut`
//! roles — Escape for Cancel, Return for the confirm button — are the app's key handling: the card
//! is not a window, so it takes no focus of its own.

use std::sync::Arc;

use compositor_session::projects::{PSDConversion, PSDConversionRequest};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, v_flex, Disableable as _};
use gpui_kit::*;

use crate::toolbar::status_bar::SECONDARY;

/// The card's padding, `.padding(24)`.
const PADDING: f32 = 24.0;
/// The card's minimum size, `.frame(minWidth: 520, minHeight: 360)`.
const MIN_WIDTH: f32 = 520.0;
const MIN_HEIGHT: f32 = 360.0;
/// The body's minimum height — `.frame(maxWidth: .infinity, minHeight: 180)` while reading, and
/// `.frame(minHeight: 180)` for the conversion list.
const BODY_MIN_HEIGHT: f32 = 180.0;
/// `VStack(alignment: .leading, spacing: 16)`.
const STACK_SPACING: f32 = 16.0;
/// `HStack(spacing: 10)` around the reading row's spinner and its caption.
const READING_SPACING: f32 = 10.0;
/// `VStack(alignment: .leading, spacing: 4)` inside a conversion row.
const ROW_SPACING: f32 = 4.0;
/// `.padding(.vertical, 4)` on a conversion row.
const ROW_PADDING: f32 = 4.0;
/// The button row's `HStack` spacing.
const BUTTON_SPACING: f32 = 8.0;
/// The card's corner radius and fill: the editor's dark card, as the alert card in
/// `content_view.rs` uses.
const RADIUS: f32 = 10.0;
const CARD_BACKGROUND: Hsla = hsla(0.0, 0.0, 0.18, 1.0);
/// The body text's size, SwiftUI's body at 13 points.
const TEXT_SIZE: f32 = 13.0;
/// The title's size, `.title2.bold()`.
const TITLE_SIZE: f32 = 17.0;

/// The PSD conversion sheet: what a Photoshop file needs converted, and the choice to go on.
pub struct PSDConversionSheet {
    /// `request`: the title, the confirm button's label and the conversions — or, while the file is
    /// still being read, the reading state.
    request: PSDConversionRequest,
    /// `finish`: `session.finishConversion(_:)`, told whether the import goes ahead.
    finish: Arc<dyn Fn(bool, &mut Window, &mut App)>,
}

impl PSDConversionSheet {
    /// The sheet for `request`; `finish` is the app's mapping onto
    /// `EditorSession::finish_conversion(confirmed, host)`: the sheet stays ignorant of session
    /// internals.
    pub fn new(request: PSDConversionRequest, finish: Arc<dyn Fn(bool, &mut Window, &mut App)>) -> Self {
        Self { request, finish }
    }

    /// `finishPSDReading(_:)` filling the sheet in: the same card shows the reading row first and
    /// the conversions once they are known.
    pub fn set_request(&mut self, request: PSDConversionRequest, cx: &mut Context<Self>) {
        self.request = request;
        cx.notify();
    }
}

impl Render for PSDConversionSheet {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let reading = self.request.is_reading;
        let cancel = {
            let finish = self.finish.clone();
            Button::new("psd-conversion-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| finish(false, window, cx))
        };
        let confirm = {
            let finish = self.finish.clone();
            Button::new("psd-conversion-confirm")
                .label(self.request.confirm_title.clone())
                // `.keyboardShortcut(.defaultAction)`: the default button is the prominent one.
                .primary()
                .disabled(reading)
                .on_click(move |_, window, cx| finish(true, window, cx))
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .gap(px(STACK_SPACING))
                    .p(px(PADDING))
                    .min_w(px(MIN_WIDTH))
                    .min_h(px(MIN_HEIGHT))
                    .rounded(px(RADIUS))
                    .bg(CARD_BACKGROUND)
                    .text_size(px(TEXT_SIZE))
                    .child(
                        div()
                            .text_size(px(TITLE_SIZE))
                            .font_weight(FontWeight::BOLD)
                            .child(self.request.title.clone()),
                    )
                    .child(
                        div().text_color(SECONDARY).child(if reading {
                            "Reading the file to see what needs converting."
                        } else {
                            "Compositor will convert these Photoshop features. Nothing is applied until you continue."
                        }),
                    )
                    .child(if reading {
                        reading_row()
                    } else {
                        conversion_list(&self.request.conversions)
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap(px(BUTTON_SPACING))
                            .child(cancel)
                            .child(confirm),
                    ),
            )
    }
}

/// The `if request.isReading` body: a small spinner and the reading caption, centered in the body's
/// minimum height (`.frame(maxWidth: .infinity, minHeight: 180)`).
fn reading_row() -> Div {
    h_flex()
        .w_full()
        .min_h(px(BODY_MIN_HEIGHT))
        .items_center()
        .justify_center()
        .gap(px(READING_SPACING))
        .child(progress_indicator())
        .child(div().text_color(SECONDARY).child("Reading the Photoshop file…"))
}

/// The `List(request.conversions)` body: one leading stack per conversion, four points of vertical
/// padding on each.
fn conversion_list(conversions: &[PSDConversion]) -> Div {
    v_flex()
        .w_full()
        .min_h(px(BODY_MIN_HEIGHT))
        .children(conversions.iter().map(|item| {
            v_flex()
                .gap(px(ROW_SPACING))
                .py(px(ROW_PADDING))
                // `.font(.headline)`.
                .child(
                    div()
                        .text_size(px(TEXT_SIZE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(item.layer_name.clone()),
                )
                .child(item.message.clone())
        }))
}

/// `ProgressView().controlSize(.small)`: the component library's small spinner.
fn progress_indicator() -> impl IntoElement {
    use gpui_kit::component::Sizable as _;
    gpui_kit::component::spinner::Spinner::new().small()
}
