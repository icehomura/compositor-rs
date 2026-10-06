//! The tool headers' shared chrome: the option bar each tool shows above the canvas.
//!
//! Ported from `UI/ToolHeaderStyle.swift`: a 42-point bar, 18 points of horizontal padding, a 13-point
//! semibold title and 12-point controls.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The bar's height (`ToolHeaderStyle.barHeight`).
pub const TOOL_HEADER_HEIGHT: f32 = 42.0;

/// The bar's horizontal padding (`padding(.horizontal, 18)`).
pub const TOOL_HEADER_PADDING: f32 = 18.0;

/// The title's size and weight, and the controls'.
pub const TITLE_SIZE: f32 = 13.0;
pub const CONTROL_SIZE: f32 = 12.0;

/// `ToolHeaderStyle`.
pub struct ToolHeaderStyle;

impl ToolHeaderStyle {
    /// `ToolHeaderStyle.titleFont`: 13 points, semibold.
    pub fn title_font() -> Font {
        Font {
            weight: FontWeight::SEMIBOLD,
            ..Font::default()
        }
    }

    /// `ToolHeaderStyle.controlFont`: 12 points, regular.
    pub fn control_font() -> Font {
        Font::default()
    }
}

/// `toolHeaderBar()`: the row a tool's options sit in — the given spacing between them, 18 points of
/// horizontal padding, the control font, 42 points tall, and never compressed vertically.
pub fn tool_header_bar(spacing: f32) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(spacing))
        .px(px(TOOL_HEADER_PADDING))
        .h(px(TOOL_HEADER_HEIGHT))
        .flex_none()
        .text_size(px(CONTROL_SIZE))
}

/// The title a bar starts with (`Text("…").font(ToolHeaderStyle.titleFont)`).
pub fn tool_header_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(TITLE_SIZE))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// The trailing `Spacer()` of a bar.
pub fn tool_header_spacer() -> Div {
    div().flex_1()
}

/// `HStack(spacing: 16)` around a title and one control, the Eyedropper's and the idle tool's shape.
pub fn tool_header_title_bar(title: impl Into<SharedString>) -> Div {
    tool_header_bar(16.0)
        .child(tool_header_title(title))
        .child(tool_header_spacer())
}
