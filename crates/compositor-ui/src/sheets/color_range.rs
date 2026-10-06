//! Select > Color Range's panel: the eyedroppers, Fuzziness and Invert, with the selection updating
//! on the canvas (port of `UI/ColorRangeSheet.swift`).
//!
//! The Swift view was the content of a floating `NSPanel`; the port's is hosted the same way — the
//! panel draws its chrome, and this view keeps the Swift's own
//! `.padding(24).frame(width: 340).fixedSize()` content in the type the port's panels share (12-point
//! controls, as [`crate::tool_header`]). The Fuzziness `Slider` is the port's plain-track slider,
//! [`CameraRawSlider`], the one the tool bars use for the macOS sliders. Escape and Return are the
//! buttons' `configuredNativeShortcut`s, which the floating panel's host does not route, so the sheet's
//! own root captures them.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use compositor_core::{Gray8Image, Rgba8Image};
use compositor_io::image_exporter::{shared, ExportRaster, ImageExporter};
use compositor_session::selection::{ColorRangeEdit, HueSampleMode};
use compositor_session::EditorSession;

use crate::tool_controls::{FieldSpec, Fields};
use crate::widgets::gradient_slider::CameraRawSlider;
use crate::widgets::numeric_scrub::{NumericScrub, Scrubbable as _};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::Icon;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `.frame(width: 340)` — the panel's width, padding included.
const WIDTH: f32 = 340.0;
/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// The eyedropper row's `HStack(spacing: 6)`.
const EYEDROPPER_SPACING: f32 = 6.0;
/// The Fuzziness row's `HStack(spacing: 10)`.
const ROW_SPACING: f32 = 10.0;
/// The eyedropper's `frame(width: 24, height: 20)`.
const EYEDROPPER_WIDTH: f32 = 24.0;
const EYEDROPPER_HEIGHT: f32 = 20.0;
/// `RoundedRectangle(cornerRadius: 4)` around the armed eyedropper.
const EYEDROPPER_RADIUS: f32 = 4.0;
/// `Color.accentColor.opacity(0.25)`: the system accent, the theme's blue here.
const ACCENT_OPACITY: f32 = 0.25;
/// The eyedropper symbol's size and the badge's `.system(size: 8)`.
const SYMBOL_SIZE: f32 = crate::tool_header::CONTROL_SIZE;
const BADGE_SIZE: f32 = 8.0;
/// The badge's `.offset(x: 3, y: 1)` from the eyedropper's bottom-trailing corner.
const BADGE_OFFSET_X: f32 = 3.0;
const BADGE_OFFSET_Y: f32 = 1.0;
/// `.frame(width: 48)` — the Fuzziness field.
const FIELD_WIDTH: f32 = 48.0;
/// The Fuzziness the panel shows before an edit exists (`edit?.fuzziness ?? 40`).
const DEFAULT_FUZZINESS: f64 = 40.0;
/// `Color.orange` (#FF9500), the error text's `.foregroundStyle(.orange)`.
const ERROR_ORANGE: Hsla = hsla(35.0 / 360.0, 1.0, 0.5, 1.0);

/// `ColorRangeEdit.fuzzinessRange` as the tuple the port's sliders and fields take.
fn fuzziness_range() -> (f64, f64) {
    (
        *ColorRangeEdit::FUZZINESS_RANGE.start(),
        *ColorRangeEdit::FUZZINESS_RANGE.end(),
    )
}

/// Select > Color Range's panel.
pub struct ColorRangeSheet {
    session: Entity<EditorSession>,
    /// The Fuzziness field, made on its first frame.
    fields: Fields,
    /// The preview picture, kept until the edit's `generation` moves on.
    preview: Rc<RefCell<PreviewCache>>,
}

/// The gpui picture the panel last built, and the edit it was built from.
#[derive(Default)]
struct PreviewCache {
    /// The `generation`, width and height the picture belongs to.
    key: Option<(i64, usize, usize)>,
    image: Option<Arc<RenderImage>>,
}

impl PreviewCache {
    /// The picture held for `key`, `None` when the cache was built for a different edit.
    fn image_for(&self, key: (i64, usize, usize)) -> Option<Option<Arc<RenderImage>>> {
        (self.key == Some(key)).then(|| self.image.clone())
    }
}

impl ColorRangeSheet {
    pub fn new(session: Entity<EditorSession>, cx: &mut Context<Self>) -> Self {
        // The panel reads the edit the canvas is updating, so every change repaints it.
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        Self {
            session,
            fields: Fields::default(),
            preview: Rc::new(RefCell::new(PreviewCache::default())),
        }
    }

    /// The Swift `help(_:)`: what each eyedropper's tooltip says.
    fn help(mode: HueSampleMode) -> &'static str {
        match mode {
            HueSampleMode::Replace => "Click the image to select that color",
            HueSampleMode::Add => "Click the image to add that color to the selection",
            HueSampleMode::Remove => "Click the image to take that color out of the selection",
        }
    }

    /// `mode.badge`, as the icon the port draws for it: the plus and minus circles.
    fn badge_icon(mode: HueSampleMode) -> Option<IconName> {
        match mode.badge()? {
            "plus.circle.fill" => Some(IconName::CirclePlus),
            "minus.circle.fill" => Some(IconName::CircleMinus),
            _ => None,
        }
    }

    /// The Swift `fuzziness` binding's setter: the whole number held inside the range, then the
    /// selection recomputed — neither step runs when the value did not move.
    fn set_fuzziness(session: &Entity<EditorSession>, value: f64, cx: &mut App) {
        let (lower, upper) = fuzziness_range();
        let clamped = value.round().clamp(lower, upper);
        let session = session.clone();
        session.update(cx, |session, _| {
            let changed = match session.color_range.as_mut() {
                Some(edit) if edit.fuzziness != clamped => {
                    edit.fuzziness = clamped;
                    true
                }
                _ => false,
            };
            if changed {
                session.update_color_range();
            }
        });
    }

    /// The Swift `preview`'s geometry: the picture scaled to fit `ColorRangeEdit.previewSize`, or
    /// that size itself before an edit exists.
    fn preview_frame(edit: Option<&ColorRangeEdit>) -> (f32, f32) {
        let limit = ColorRangeEdit::PREVIEW_SIZE;
        let (width, height) = edit
            .map(|edit| (edit.image.width() as f64, edit.image.height() as f64))
            .filter(|(width, height)| *width > 0.0 && *height > 0.0)
            .unwrap_or((limit.width, limit.height));
        let scale = (limit.width / width).min(limit.height / height);
        ((width * scale) as f32, (height * scale) as f32)
    }

    /// `Image(decorative:scale: 2)`: the mask preview as a gpui picture, built once per change.
    fn preview_raster(&self, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
        let session = self.session.clone();
        let (key, preview) = {
            let session = session.read(cx);
            let edit = session.color_range.as_ref()?;
            let preview = edit.preview.as_ref()?;
            let key = (edit.generation, preview.width(), preview.height());
            if let Some(image) = self.preview.borrow().image_for(key) {
                return image;
            }
            // Only a picture the cache does not hold is copied out from under the session borrow.
            (key, preview.clone())
        };
        let image = Self::raster(&preview, window, cx);
        let mut cache = self.preview.borrow_mut();
        cache.key = Some(key);
        cache.image = image.clone();
        image
    }

    /// A gray picture as the opaque sRGB one `CGImage` drew from its device-gray, alpha-less bitmap.
    fn raster(preview: &Gray8Image, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
        let (width, height) = (preview.width(), preview.height());
        if width == 0 || height == 0 {
            return None;
        }
        let mut data = vec![0u8; width * height * 4];
        for (pixel, &value) in data.chunks_exact_mut(4).zip(preview.data()) {
            pixel.copy_from_slice(&[value, value, value, 255]);
        }
        let raster = ExportRaster::new(shared(Rgba8Image::from_data(width, height, data)));
        let bytes = ImageExporter::png_data(&raster).ok()?;
        let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
        image.use_render_image(window, cx)
    }

    /// The Swift `eyedropper(_:)`: the symbol, with the plus or minus badge Add and Remove carry.
    fn eyedropper(mode: HueSampleMode) -> impl IntoElement {
        div()
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(px(EYEDROPPER_WIDTH))
            .h(px(EYEDROPPER_HEIGHT))
            .child(Icon::new(IconName::Pipette).size(px(SYMBOL_SIZE)))
            .when_some(Self::badge_icon(mode), |this, badge| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(-BADGE_OFFSET_Y))
                        .right(px(-BADGE_OFFSET_X))
                        .child(Icon::new(badge).size(px(BADGE_SIZE))),
                )
            })
    }

    /// The `HStack(spacing: 6)` of eyedroppers, the armed one (`effectiveMode`) lit with the accent.
    fn eyedroppers(
        &self,
        effective_mode: Option<HueSampleMode>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let accent = cx.theme().colors.blue;
        h_flex()
            .gap(px(EYEDROPPER_SPACING))
            .children(HueSampleMode::ALL.into_iter().map(move |mode| {
                let session = self.session.clone();
                div()
                    .id(ElementId::Name(
                        format!("color-range-mode-{}", mode.raw_value()).into(),
                    ))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(px(EYEDROPPER_RADIUS))
                    .when(effective_mode == Some(mode), |this| {
                        this.bg(accent.opacity(ACCENT_OPACITY))
                    })
                    .cursor(CursorStyle::PointingHand)
                    .role(Role::Button)
                    .aria_label(format!("{} color", mode.raw_value()))
                    .tooltip(move |window, cx| Tooltip::new(Self::help(mode)).build(window, cx))
                    .on_click(move |_, _, cx| {
                        session.update(cx, |session, _| {
                            if let Some(edit) = session.color_range.as_mut() {
                                edit.sample_mode = mode;
                            }
                        });
                    })
                    .child(Self::eyedropper(mode))
            }))
    }

    /// The Swift `preview`: black until a color is picked, then the selection in black and white —
    /// white where selected, shaped like the canvas and centered (`.frame(maxWidth: .infinity)`).
    fn preview(
        &self,
        frame: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let image = self.preview_raster(window, cx);
        let picture = canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _state, window, _cx| {
                if let Some(image) = image {
                    let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
                }
            },
        )
        .absolute()
        .size_full();
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(
                div()
                    .relative()
                    .w(px(frame.0))
                    .h(px(frame.1))
                    .bg(hsla(0.0, 0.0, 0.0, 1.0))
                    .border_1()
                    .border_color(hsla(0.0, 0.0, 1.0, 0.2))
                    .child(picture),
            )
    }

    /// The `HStack(spacing: 10)` of Fuzziness: the scrubbable label, the slider and the field, with
    /// the row's own help.
    fn fuzziness_row(&mut self, fuzziness: f64, cx: &mut Context<Self>) -> impl IntoElement {
        let field = self.fields.get("color-range-fuzziness", cx);
        let scrub = {
            let session = self.session.clone();
            NumericScrub::new(fuzziness, 1.0, fuzziness_range())
                .on_change(move |value, _, cx| Self::set_fuzziness(&session, value, cx))
        };
        let slider = {
            let session = self.session.clone();
            CameraRawSlider::plain(
                "color-range-fuzziness-slider",
                fuzziness,
                fuzziness_range(),
                "Fuzziness",
            )
            .on_change(move |value, _, cx| Self::set_fuzziness(&session, value, cx))
        };
        let write = {
            let session = self.session.clone();
            move |value: f64, _: &mut Window, cx: &mut App| Self::set_fuzziness(&session, value, cx)
        };
        h_flex()
            .id("color-range-fuzziness-row")
            .gap(px(ROW_SPACING))
            .tooltip(|window, cx| {
                Tooltip::new("How far a color may be from the picked ones and still be selected")
                    .build(window, cx)
            })
            .child(
                div()
                    .flex_none()
                    .child("Fuzziness")
                    .scrubbable("color-range-fuzziness-label", scrub),
            )
            .child(div().flex_1().min_w(px(0.0)).child(slider))
            .child(field.element(
                "color-range-fuzziness-field",
                fuzziness,
                // A typed value that is no number leaves the fuzziness where it was.
                FieldSpec::new(fuzziness_range(), 0).fallback(fuzziness),
                FIELD_WIDTH,
                write,
                cx,
            ))
    }

    /// The `Toggle("Invert", isOn:)`, whose setter recomputes the selection.
    fn invert_toggle(&self, invert: bool) -> Checkbox {
        let session = self.session.clone();
        Checkbox::new("color-range-invert")
            .label("Invert")
            .checked(invert)
            .tooltip("Select everything except those colors, such as all but a green screen")
            .on_click(move |checked, _, cx| {
                let checked = *checked;
                session.update(cx, |session, _| {
                    if let Some(edit) = session.color_range.as_mut() {
                        edit.invert = checked;
                    }
                    session.update_color_range();
                });
            })
    }

    /// The `HStack` of Cancel and the prominent OK.
    fn buttons(&self) -> impl IntoElement {
        let cancel = self.session.clone();
        let ok = self.session.clone();
        h_flex()
            .justify_between()
            .child(
                Button::new("color-range-cancel")
                    .label("Cancel")
                    .on_click(move |_, _, cx| {
                        let cancel = cancel.clone();
                        cancel.update(cx, |session, _| session.cancel_color_range());
                    }),
            )
            .child(
                Button::new("color-range-ok")
                    .label("OK")
                    .primary()
                    .on_click(move |_, _, cx| {
                        let ok = ok.clone();
                        ok.update(cx, |session, _| session.commit_color_range());
                    }),
            )
    }
}

impl Render for ColorRangeSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (fuzziness, invert, effective_mode, has_colors, error, frame) = {
            let session = self.session.read(cx);
            let edit = session.color_range.as_ref();
            (
                edit.map(|edit| edit.fuzziness).unwrap_or(DEFAULT_FUZZINESS),
                edit.map(|edit| edit.invert).unwrap_or(false),
                edit.map(ColorRangeEdit::effective_mode),
                edit.map(ColorRangeEdit::has_colors).unwrap_or(false),
                edit.and_then(|edit| edit.error.clone()),
                Self::preview_frame(edit),
            )
        };
        let help = if has_colors {
            "Shift-click adds a color, Option-click takes one away."
        } else {
            "Click the image to pick the color to select."
        };
        let secondary = cx.theme().tokens.muted_foreground;
        // `.configuredNativeShortcut(.escape)` / `.return` on the two buttons, which the floating
        // panel's host does not route. A Fuzziness field being typed into keeps both keys for itself.
        let keys = {
            let session = self.session.clone();
            let sheet = cx.entity();
            move |event: &KeyDownEvent, _: &mut Window, cx: &mut App| {
                if sheet.read(cx).fields.is_editing("color-range-fuzziness", cx) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        let session = session.clone();
                        session.update(cx, |session, _| session.cancel_color_range());
                        cx.stop_propagation();
                    }
                    "enter" | "return" => {
                        let session = session.clone();
                        session.update(cx, |session, _| session.commit_color_range());
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }
        };
        v_flex()
            .w(px(WIDTH))
            .p(px(PADDING))
            .gap(px(SPACING))
            .text_size(px(crate::tool_header::CONTROL_SIZE))
            .capture_key_down(keys)
            .child(self.eyedroppers(effective_mode, cx))
            .child(self.preview(frame, window, cx))
            .child(div().text_color(secondary).child(help))
            .child(self.fuzziness_row(fuzziness, cx))
            .child(self.invert_toggle(invert))
            .when_some(error, |this, error| {
                this.child(div().text_color(ERROR_ORANGE).child(error))
            })
            .child(Separator::horizontal())
            .child(self.buttons())
    }
}
