//! The Crop tool's header (port of `UI/CropControls.swift`).
//!
//! A view of its own because dragging the crop frame changes `cropRect` on every mouse move: read
//! here, only this bar re-renders, not the whole editor and its Layers panel.

use compositor_session::EditorSession;

use crate::tool_controls::menu_picker;
use crate::tool_header::{tool_header_bar, tool_header_spacer, tool_header_title};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The ratios the bar offers (`Picker("Ratio")`'s literal list, in its order).
pub const RATIO_CHOICES: [&str; 7] = ["Free", "Original", "1:1", "4:3", "3:4", "16:9", "9:16"];

/// The Crop bar: its title, the ratio menu, the crop's size in pixels, and Cancel / Apply Crop.
pub struct CropControls {
    session: Entity<EditorSession>,
}

impl CropControls {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self { session }
    }
}

impl Render for CropControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let ratio = session.crop_ratio_choice.clone();
        let crop_rect = session.crop_rect;
        // `.disabled(session.showsBusy || session.document == nil)`.
        let enabled = !session.shows_busy && session.document.is_some();
        let has_rect = crop_rect.is_some();

        let ratio_picker = {
            let session = self.session.clone();
            menu_picker(
                "crop-ratio",
                RATIO_CHOICES.iter().map(|choice| (choice.to_string(), *choice)),
                ratio,
                move |choice, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| {
                        session.crop_ratio_choice = choice.clone();
                        // `onChange(of: session.cropRatioChoice) { session.changeCropRatio() }`.
                        session.change_crop_ratio();
                    });
                },
            )
            // `.frame(width: 170)`.
            .w(px(170.0))
            .disabled(!enabled)
        };

        let size = crop_rect.map(|rect| {
            format!(
                "{} × {} px",
                rect.size.width as i32, rect.size.height as i32
            )
        });

        let cancel = {
            let session = self.session.clone();
            Button::new("crop-cancel")
                .label("Cancel")
                .disabled(!enabled || !has_rect)
                .on_click(move |_, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.cancel_crop());
                })
        };

        let apply = {
            let session = self.session.clone();
            Button::new("crop-apply")
                .label("Apply Crop")
                .disabled(!enabled || !has_rect)
                .on_click(move |_, _, cx| {
                    let session = session.clone();
                    session.update(cx, |session, _| session.commit_crop());
                })
        };

        tool_header_bar(14.0)
            .child(tool_header_title("Crop"))
            .child(ratio_picker)
            .when_some(size, |this, size| {
                this.child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(size),
                )
            })
            .child(tool_header_spacer())
            .child(cancel)
            .child(apply)
    }
}
