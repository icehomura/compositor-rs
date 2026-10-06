//! File > Export JPEG…: the quality, the color the transparency is flattened onto, and the encoded
//! preview (port of `UI/JPEGExportSheet.swift`).
//!
//! The sheet is the app's centred overlay card, the port's stand-in for the Swift `.sheet`. The
//! preview is encoded a moment after the last change, as the Swift's `.task(id: options)` did: the
//! encoding runs on the background executor and a superseded one is dropped with its task, so a
//! newer setting always wins.
//!
//! Substitutions: `JPEGPreview`'s `CGImage` reaches gpui as a `RenderImage` by way of PNG, the one
//! image format the toolkit takes without an image-crate dependency (see `canvas_view.rs`);
//! `ByteCountFormatter` with `countStyle: .file` counts decimal units with three significant
//! digits; `.monospacedDigit()` is the theme's monospace family; the Swift `ScrollView` is the
//! preview's own offset, dragged and scrolled in points; `.interpolation(.none)` above 100% has no
//! gpui equivalent, so the preview always samples smoothly.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use compositor_core::settings;
use compositor_core::PaletteColor;
use compositor_io::image_exporter::{shared, ExportRaster, ImageExporter, JPEGOptions, JPEGResult};
use compositor_session::view::PreviewZoomCommand;
use compositor_session::EditorSession;

use crate::panels::color_picker::DialogColorSwatch;
use crate::toolbar::status_bar::SECONDARY;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Icon;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::*;

/// `.padding(24)`.
const PADDING: f32 = 24.0;
/// The card's width: the preview's own frame plus the padding on each side.
const WIDTH: f32 = JPEGPreview::FRAME_WIDTH + PADDING * 2.0;
/// The card's corner radius, as the app's other overlay cards use.
const RADIUS: f32 = 10.0;
/// The card's fill: the editor's dark card (`content_view.rs`'s alert card).
const CARD_BACKGROUND: Hsla = hsla(0.0, 0.0, 0.18, 1.0);
/// `Color(white: 0.12)`: the preview's own backdrop.
const PREVIEW_BACKGROUND: Hsla = hsla(0.0, 0.0, 0.12, 1.0);
/// `.regularMaterial` behind the spinner: the port's translucent card.
const MATERIAL: Hsla = hsla(0.0, 0.0, 0.25, 0.7);
/// The `VStack(alignment: .leading, spacing: 16)`.
const SPACING: f32 = 16.0;
/// The header row's `HStack(spacing: 8)`.
const HEADER_SPACING: f32 = 8.0;
/// `.padding(.bottom, -8)`: closer to the title row than the rest of the dialog's spacing.
const HEADER_PULL_UP: f32 = -8.0;
/// The quality rows' `HStack(spacing: 8)`.
const ROW_SPACING: f32 = 8.0;
/// The footer row's `HStack(spacing: 12)`.
const FOOTER_SPACING: f32 = 12.0;
/// The title's size, `.title2.bold()`.
const TITLE_SIZE: f32 = 17.0;
/// The body text's size, SwiftUI's body at 13 points.
const TEXT_SIZE: f32 = 13.0;
/// The quality readout's `.frame(width: 45, alignment: .trailing)`.
const QUALITY_WIDTH: f32 = 45.0;
/// The spinner badge's `.padding()` and its `RoundedRectangle(cornerRadius: 8)`.
const BADGE_PADDING: f32 = 8.0;
const BADGE_RADIUS: f32 = 8.0;
/// `Color.red`, the error text's `.foregroundStyle(.red)`.
const ERROR_RED: Hsla = hsla(0.0, 1.0, 0.5, 1.0);
/// A wheel line's height in points (the canvas counts one the same way).
const SCROLL_LINE_HEIGHT: f64 = 12.0;
/// `.task(id: options)` waits before encoding, so a drag across the slider encodes once.
const ENCODE_DELAY: Duration = Duration::from_millis(200);
/// `qualityKey`: the quality the next export starts from.
const QUALITY_KEY: &str = "jpegExportQuality";

/// `ByteCountFormatter.string(fromByteCount:countStyle: .file)`: decimal units with three
/// significant digits ("999 bytes", "1 KB", "1.5 KB", "12 KB", "1.23 MB").
fn byte_count_file(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "Zero bytes".to_string();
    }
    if bytes == 1 {
        return "1 byte".to_string();
    }
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    // Bytes below a kilobyte already returned; start in KB so `unit` and `value` agree —
    // the loop advances them together, and the old byte-based start made 1000 read "1 MB".
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    loop {
        // Three significant digits, rounded as the formatter rounds: 999,999 bytes reads "1 MB".
        let decimals = if value < 10.0 {
            2
        } else if value < 100.0 {
            1
        } else {
            0
        };
        if format!("{value:.decimals$}").parse::<f64>().unwrap_or(value) < 1000.0
            || unit + 1 >= UNITS.len()
        {
            break;
        }
        value /= 1000.0;
        unit += 1;
    }
    let decimals = if value < 10.0 {
        2
    } else if value < 100.0 {
        1
    } else {
        0
    };
    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    format!("{text} {}", UNITS[unit])
}

/// `Int.formatted()`: the number with the en-US grouping the size row spells out ("1,024").
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

/// The preview's pan in progress (`JPEGPreview`'s `dragStart`): where the pointer was pressed and
/// the offset it was showing then.
#[derive(Clone, Copy)]
pub struct PreviewDrag {
    pub start: Point<Pixels>,
    pub offset: Point<f64>,
}

/// The encoded JPEG, fitted or zoomed (1 is 100%: one image pixel per screen pixel, as the canvas
/// counts it), where it can be dragged or scrolled around. Double-click switches between Fit and
/// 100%.
#[derive(IntoElement)]
pub struct JPEGPreview {
    id: ElementId,
    image: Arc<RenderImage>,
    /// The exported image's size, which the preview may have been decoded smaller than.
    pixel_width: usize,
    pixel_height: usize,
    /// The zoom, 1 being 100%; `None` fits the whole image.
    zoom: Option<f64>,
    /// Where the view is scrolled to, in points.
    offset: Point<f64>,
    /// The pan in progress.
    drag: Option<PreviewDrag>,
    /// `@Environment(\.displayScale)`.
    display_scale: f64,
    on_offset: Rc<dyn Fn(Point<f64>, &mut Window, &mut App)>,
    on_drag: Rc<dyn Fn(Option<PreviewDrag>, &mut Window, &mut App)>,
    on_zoom: Rc<dyn Fn(Option<f64>, &mut Window, &mut App)>,
}

impl JPEGPreview {
    /// `static let frame = CGSize(width: 560, height: 330)`.
    pub const FRAME_WIDTH: f32 = 560.0;
    pub const FRAME_HEIGHT: f32 = 330.0;
    /// `static let steps: [Double] = [0.25, 0.5, 1, 2, 4, 8]`.
    pub const STEPS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

    /// `frame` as the math below carries it.
    pub const FRAME: Size<f64> = size(Self::FRAME_WIDTH as f64, Self::FRAME_HEIGHT as f64);

    /// A preview of `image` showing `offset`, told where it lands as it is dragged or scrolled.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<ElementId>,
        image: Arc<RenderImage>,
        pixel_width: usize,
        pixel_height: usize,
        zoom: Option<f64>,
        offset: Point<f64>,
        drag: Option<PreviewDrag>,
        display_scale: f64,
        on_offset: impl Fn(Point<f64>, &mut Window, &mut App) + 'static,
        on_drag: impl Fn(Option<PreviewDrag>, &mut Window, &mut App) + 'static,
        on_zoom: impl Fn(Option<f64>, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            image,
            pixel_width,
            pixel_height,
            zoom,
            offset,
            drag,
            display_scale,
            on_offset: Rc::new(on_offset),
            on_drag: Rc::new(on_drag),
            on_zoom: Rc::new(on_zoom),
        }
    }

    /// `fitZoom(width:height:in:displayScale:)`: the zoom at which the whole image fits `frame`.
    pub fn fit_zoom(width: usize, height: usize, frame: Size<f64>, display_scale: f64) -> f64 {
        let scale = display_scale.max(1.0);
        let points = size(width as f64 / scale, height as f64 / scale);
        (frame.width / points.width).min(frame.height / points.height)
    }

    /// `step(from:in:)`: the next zoom step past `zoom` in `direction` (1 in, −1 out), or `None`
    /// at the end.
    pub fn step(from: f64, direction: i32) -> Option<f64> {
        if direction > 0 {
            Self::STEPS.iter().copied().find(|step| *step > from * 1.001)
        } else {
            Self::STEPS
                .iter()
                .copied()
                .rev()
                .find(|step| *step < from * 0.999)
        }
    }

    /// `shownSize(_:)`: the image's size on screen at `zoom`, in points.
    pub fn shown_size(
        pixel_width: usize,
        pixel_height: usize,
        zoom: f64,
        display_scale: f64,
    ) -> Size<f64> {
        let scale = display_scale.max(1.0);
        size(
            pixel_width as f64 / scale * zoom,
            pixel_height as f64 / scale * zoom,
        )
    }

    /// The offset a scroll or a drag may reach: the `ScrollView`'s own clamping, keeping the content
    /// covering the view.
    pub fn clamped_offset(offset: Point<f64>, content: Size<f64>, view: Size<f64>) -> Point<f64> {
        point(
            offset.x.clamp(0.0, (content.width - view.width).max(0.0)),
            offset.y.clamp(0.0, (content.height - view.height).max(0.0)),
        )
    }

    /// `keepCentered(from:to:in:)`: zooming keeps the middle of the view on the same part of the
    /// image; coming from Fit, it starts at the center.
    pub fn keep_centered(
        pixel_width: usize,
        pixel_height: usize,
        display_scale: f64,
        from: Option<f64>,
        to: Option<f64>,
        offset: Point<f64>,
        view: Size<f64>,
    ) -> Point<f64> {
        let Some(to) = to else {
            return offset;
        };
        let size = Self::shown_size(pixel_width, pixel_height, to, display_scale);
        let mut middle = point(size.width / 2.0, size.height / 2.0);
        if let Some(from) = from {
            let before = Self::shown_size(pixel_width, pixel_height, from, display_scale);
            let fx = if before.width > 0.0 {
                (offset.x + view.width.min(before.width) / 2.0) / before.width
            } else {
                0.5
            };
            let fy = if before.height > 0.0 {
                (offset.y + view.height.min(before.height) / 2.0) / before.height
            } else {
                0.5
            };
            middle = point(fx * size.width, fy * size.height);
        }
        Self::clamped_offset(
            point(
                middle.x - view.width / 2.0,
                middle.y - view.height / 2.0,
            ),
            size,
            view,
        )
    }
}

impl RenderOnce for JPEGPreview {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let view = Self::FRAME;
        let image = self.image;
        let on_offset = self.on_offset;
        let on_drag = self.on_drag;
        let on_zoom = self.on_zoom;
        match self.zoom {
            Some(zoom) => {
                let content = Self::shown_size(
                    self.pixel_width,
                    self.pixel_height,
                    zoom,
                    self.display_scale,
                );
                let offset = self.offset;
                let drag = self.drag;
                div()
                    .id(self.id)
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .cursor(if drag.is_none() {
                        CursorStyle::OpenHand
                    } else {
                        CursorStyle::ClosedHand
                    })
                    .child(
                        img(image)
                            .absolute()
                            .left(px(-offset.x as f32))
                            .top(px(-offset.y as f32))
                            .w(px(content.width as f32))
                            .h(px(content.height as f32)),
                    )
                    .on_scroll_wheel({
                        let on_offset = on_offset.clone();
                        move |event: &ScrollWheelEvent, window, cx| {
                            let delta = match event.delta {
                                ScrollDelta::Pixels(delta) => point(
                                    f64::from(f32::from(delta.x)),
                                    f64::from(f32::from(delta.y)),
                                ),
                                ScrollDelta::Lines(delta) => point(
                                    f64::from(delta.x) * SCROLL_LINE_HEIGHT,
                                    f64::from(delta.y) * SCROLL_LINE_HEIGHT,
                                ),
                            };
                            // A scroll view moves its content against the wheel.
                            let next = JPEGPreview::clamped_offset(
                                point(offset.x - delta.x, offset.y - delta.y),
                                content,
                                view,
                            );
                            on_offset(next, window, cx);
                        }
                    })
                    .on_mouse_down(MouseButton::Left, {
                        let on_drag = on_drag.clone();
                        move |event: &MouseDownEvent, window, cx| {
                            on_drag(
                                Some(PreviewDrag {
                                    start: event.position,
                                    offset,
                                }),
                                window,
                                cx,
                            )
                        }
                    })
                    .on_mouse_move({
                        let on_offset = on_offset.clone();
                        move |event: &MouseMoveEvent, window, cx| {
                            let Some(drag) = drag else { return };
                            if event.pressed_button != Some(MouseButton::Left) {
                                return;
                            }
                            let translation = event.position - drag.start;
                            let next = JPEGPreview::clamped_offset(
                                point(
                                    drag.offset.x - f64::from(f32::from(translation.x)),
                                    drag.offset.y - f64::from(f32::from(translation.y)),
                                ),
                                content,
                                view,
                            );
                            on_offset(next, window, cx);
                        }
                    })
                    .on_mouse_up(MouseButton::Left, {
                        let on_drag = on_drag.clone();
                        move |_: &MouseUpEvent, window, cx| on_drag(None, window, cx)
                    })
                    .on_click({
                        let on_zoom = on_zoom.clone();
                        move |event: &ClickEvent, window, cx| {
                            if let ClickEvent::Mouse(event) = event {
                                if event.up.click_count == 2 {
                                    on_zoom(None, window, cx);
                                }
                            }
                        }
                    })
            }
            None => div()
                .id(self.id)
                .size_full()
                .child(
                    img(image)
                        .object_fit(ObjectFit::Contain)
                        .size_full(),
                )
                .on_click({
                    let on_zoom = on_zoom.clone();
                    move |event: &ClickEvent, window, cx| {
                        if let ClickEvent::Mouse(event) = event {
                            if event.up.click_count == 2 {
                                on_zoom(Some(1.0), window, cx);
                            }
                        }
                    }
                }),
        }
    }
}

/// The Export JPEG sheet's state: the options, the encoded preview they produced, and the view's
/// own zoom and scroll.
pub struct JpegExportSheet {
    raster: ExportRaster,
    session: Entity<EditorSession>,
    /// `finish(result?.data)`: the encoded bytes, or `None` on Cancel.
    finish: Arc<dyn Fn(Option<Vec<u8>>, &mut Window, &mut App)>,
    /// `@State private var options`.
    options: JPEGOptions,
    /// `@State private var result`.
    result: Option<JPEGResult>,
    /// `readyOptions`: the options the preview on screen was encoded from.
    ready_options: Option<JPEGOptions>,
    /// `@State private var error`.
    error: Option<String>,
    /// `@State private var zoom`, 1 being 100%; `None` fits the whole image.
    zoom: Option<f64>,
    /// The view's `.scrollPosition` as an offset, in points.
    offset: Point<f64>,
    /// `@State private var dragStart`, with the offset the pan began from.
    drag: Option<PreviewDrag>,
    /// `@Environment(\.displayScale)`, as of the last frame.
    display_scale: f64,
    /// `result.preview` as gpui paints it.
    preview_image: Option<Arc<RenderImage>>,
    /// A new result arrived and its image has not been built yet.
    image_pending: bool,
    /// The encoding in flight; dropping it cancels a superseded preview.
    encode: Option<Task<()>>,
    /// The quality slider, made with the sheet (the dialog state owns it from the start).
    quality: Entity<SliderState>,
    /// The card's keyboard focus: Escape and Return are the Swift buttons' shortcuts.
    focus: FocusHandle,
    /// The color the picker last reported. The swatch's callback carries no `cx` of its own, so the
    /// color lands here and is drained on the sheet's next frame.
    picked: Rc<RefCell<Option<PaletteColor>>>,
    /// The zoom command the View menu sent (`session.previewZoom`), drained the same way.
    zoom_command: Rc<Cell<Option<PreviewZoomCommand>>>,
    /// The first frame has shown: the preview is encoded once it is on screen.
    started: bool,
    /// `finish` has been called.
    finished: bool,
}

impl JpegExportSheet {
    /// The sheet for `raster`, starting from the quality of the last export (`qualityKey`).
    pub fn new(
        raster: ExportRaster,
        session: Entity<EditorSession>,
        finish: Arc<dyn Fn(Option<Vec<u8>>, &mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        // `var start = JPEGOptions()`; `if let saved …, saved.isFinite { start.quality = min(1, max(0, saved)) }`.
        let mut options = JPEGOptions::default();
        if let Ok(saved) = settings::string_value(QUALITY_KEY, "").parse::<f64>() {
            if saved.is_finite() {
                options.quality = saved.clamp(0.0, 1.0);
            }
        }
        let start_quality = options.quality as f32;
        let quality = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.01)
                .default_value(start_quality)
        });
        cx.subscribe(&quality, |sheet, _, event: &SliderEvent, cx| {
            let value = match event {
                SliderEvent::Change(SliderValue::Single(value))
                | SliderEvent::Release(SliderValue::Single(value)) => *value,
                _ => return,
            };
            sheet.set_quality(f64::from(value), cx);
        })
        .detach();
        cx.observe(&session, |sheet, _, cx| {
            // The picker's callback and the View menu's zoom command both arrive without a `cx`:
            // drain what they left and redraw.
            sheet.drain(cx);
            cx.notify();
        })
        .detach();
        let focus = cx.focus_handle();
        Self {
            raster,
            session,
            finish,
            options,
            result: None,
            ready_options: None,
            error: None,
            zoom: None,
            offset: point(0.0, 0.0),
            drag: None,
            display_scale: 1.0,
            preview_image: None,
            image_pending: false,
            encode: None,
            quality,
            focus,
            picked: Rc::new(RefCell::new(None)),
            zoom_command: Rc::new(Cell::new(None)),
            started: false,
            finished: false,
        }
    }

    /// The exported image's size in pixels.
    fn pixel_size(&self) -> (usize, usize) {
        (self.raster.image.width(), self.raster.image.height())
    }

    /// `shownZoom`: the zoom shown now, Fit's included.
    fn shown_zoom(&self) -> f64 {
        match self.zoom {
            Some(zoom) => zoom,
            None => {
                let (width, height) = self.pixel_size();
                JPEGPreview::fit_zoom(width, height, JPEGPreview::FRAME, self.display_scale)
            }
        }
    }

    /// `percent`: the zoom as the tooltips spell it.
    fn percent(&self) -> String {
        format!("{}%", (self.shown_zoom() * 100.0).round() as i64)
    }

    /// `zoomBy(_:)`: the next step past the zoom shown now, or nothing at the end.
    fn zoom_by(&mut self, direction: i32, cx: &mut Context<Self>) {
        if let Some(next) = JPEGPreview::step(self.shown_zoom(), direction) {
            self.set_zoom(Some(next), cx);
        }
    }

    /// `self.zoom = …`, with `keepCentered(from:to:in:)` around it (`onChange(of: zoom)`).
    fn set_zoom(&mut self, zoom: Option<f64>, cx: &mut Context<Self>) {
        if zoom == self.zoom {
            return;
        }
        let (width, height) = self.pixel_size();
        self.offset = JPEGPreview::keep_centered(
            width,
            height,
            self.display_scale,
            self.zoom,
            zoom,
            self.offset,
            JPEGPreview::FRAME,
        );
        self.zoom = zoom;
        cx.notify();
    }

    /// The preview's offset after a drag or a scroll.
    fn set_offset(&mut self, offset: Point<f64>, cx: &mut Context<Self>) {
        self.offset = offset;
        cx.notify();
    }

    /// A pan began or ended (`dragStart`).
    fn set_drag(&mut self, drag: Option<PreviewDrag>, cx: &mut Context<Self>) {
        self.drag = drag;
        cx.notify();
    }

    /// `Slider(value: $options.quality, in: 0...1, step: 0.01)`.
    fn set_quality(&mut self, value: f64, cx: &mut Context<Self>) {
        let quality = value.clamp(0.0, 1.0);
        if quality == self.options.quality {
            return;
        }
        self.options.quality = quality;
        self.options_changed(cx);
    }

    /// The `matte` binding's setter: the picker's color fills the transparent areas.
    fn set_matte(&mut self, color: PaletteColor, cx: &mut Context<Self>) {
        if color.red == self.options.red
            && color.green == self.options.green
            && color.blue == self.options.blue
        {
            return;
        }
        self.options.red = color.red;
        self.options.green = color.green;
        self.options.blue = color.blue;
        self.options_changed(cx);
    }

    /// `.task(id: options)`: the preview follows every change to the quality or the background.
    fn options_changed(&mut self, cx: &mut Context<Self>) {
        self.start_encoding(cx);
        cx.notify();
    }

    /// `.task(id: options)`: the 200 ms wait, the encode, and the result — a newer request drops the
    /// task it replaces, which is the Swift's `Task.checkCancellation`.
    fn start_encoding(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let requested = self.options;
        let raster = self.raster.clone();
        self.encode = Some(cx.spawn(async move |sheet, cx| {
            cx.background_executor().timer(ENCODE_DELAY).await;
            let encoded = cx
                .background_spawn(async move { ImageExporter::jpeg(&raster, &requested) })
                .await;
            sheet
                .update(cx, |sheet, cx| {
                    match encoded {
                        Ok(result) => {
                            sheet.result = Some(result);
                            sheet.ready_options = Some(requested);
                            sheet.image_pending = true;
                        }
                        Err(error) => sheet.error = Some(error.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        }));
    }

    /// `.onAppear { session.previewZoom = { command in … } }`: the View menu's zoom commands come to
    /// the preview while it is up.
    fn start_zoom_commands(&mut self, cx: &mut Context<Self>) {
        let commands = self.zoom_command.clone();
        self.session.update(cx, |session, _| {
            session.preview_zoom = Some(Box::new(move |command| commands.set(Some(command))));
        });
    }

    /// Takes what the picker's callback and the View menu left behind (neither carries a `cx`).
    fn drain(&mut self, cx: &mut Context<Self>) {
        let picked = self.picked.borrow_mut().take();
        if let Some(color) = picked {
            self.set_matte(color, cx);
        }
        if let Some(command) = self.zoom_command.take() {
            match command {
                PreviewZoomCommand::ZoomIn => self.zoom_by(1, cx),
                PreviewZoomCommand::ZoomOut => self.zoom_by(-1, cx),
                PreviewZoomCommand::Fit => self.set_zoom(None, cx),
                PreviewZoomCommand::Actual => self.set_zoom(Some(1.0), cx),
            }
        }
    }

    /// `result.preview` as gpui paints it: the pixels travel as PNG, the one image format the
    /// toolkit takes without an image-crate dependency, built once per encoding.
    fn build_preview_image(&mut self, window: &mut Window, cx: &mut App) {
        if !self.image_pending {
            return;
        }
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let raster = ExportRaster::new(shared(result.preview.clone()));
        if let Ok(bytes) = ImageExporter::png_data(&raster) {
            let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
            self.preview_image = image.use_render_image(window, cx);
        }
        self.image_pending = false;
    }

    /// Whether Export… is live (`result == nil || readyOptions != options || error != nil`).
    fn can_export(&self) -> bool {
        self.result.is_some() && self.ready_options == Some(self.options) && self.error.is_none()
    }

    /// `finish(_:)`, once: the picker and the View menu's preview hook are let go first.
    fn close(&mut self, data: Option<Vec<u8>>, window: &mut Window, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.session.update(cx, |session, _| session.preview_zoom = None);
        DialogColorSwatch::close_picker(&self.session, cx);
        (self.finish)(data, window, cx);
    }

    /// `Button("Export…")`: the quality is remembered and the encoded bytes go back.
    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_export() {
            return;
        }
        settings::set_string(&self.options.quality.to_string(), QUALITY_KEY);
        let data = self.result.as_ref().map(|result| result.data.clone());
        self.close(data, window, cx);
    }

    /// The header row: the title and the preview's own zoom controls.
    fn header(&self, entity: &Entity<Self>) -> Div {
        let percent = self.percent();
        let fit = {
            let entity = entity.clone();
            Button::new("jpeg-fit")
                .label("Fit")
                .disabled(self.zoom.is_none())
                .tooltip("Show the whole image (⌘0)")
                .on_click(move |_, _, cx| entity.update(cx, |sheet, cx| sheet.set_zoom(None, cx)))
        };
        let zoom_in = {
            let entity = entity.clone();
            Button::new("jpeg-zoom-in")
                .icon(Icon::new(IconName::ZoomIn))
                .disabled(JPEGPreview::step(self.shown_zoom(), 1).is_none())
                .tooltip(format!(
                    "Zoom in (⌘+), now {percent}. At 100% each pixel of the JPEG is one pixel of the screen, as on the canvas"
                ))
                .on_click(move |_, _, cx| entity.update(cx, |sheet, cx| sheet.zoom_by(1, cx)))
        };
        let zoom_out = {
            let entity = entity.clone();
            Button::new("jpeg-zoom-out")
                .icon(Icon::new(IconName::ZoomOut))
                .disabled(JPEGPreview::step(self.shown_zoom(), -1).is_none())
                .tooltip(format!("Zoom out (⌘−), now {percent}"))
                .on_click(move |_, _, cx| entity.update(cx, |sheet, cx| sheet.zoom_by(-1, cx)))
        };
        h_flex()
            .gap(px(HEADER_SPACING))
            .child(
                div()
                    .text_size(px(TITLE_SIZE))
                    .font_weight(FontWeight::BOLD)
                    .child("Export JPEG"),
            )
            // `Spacer()`.
            .child(div().flex_1())
            .child(fit)
            .child(zoom_in)
            .child(zoom_out)
            .mb(px(HEADER_PULL_UP))
    }

    /// The preview and the spinner over it while the options on screen have not been encoded yet.
    fn preview(&self, entity: &Entity<Self>) -> Stateful<Div> {
        let mut stack = div()
            .id("jpeg-preview-frame")
            .relative()
            .w(px(JPEGPreview::FRAME_WIDTH))
            .h(px(JPEGPreview::FRAME_HEIGHT))
            .overflow_hidden()
            .bg(PREVIEW_BACKGROUND)
            .tooltip(|window, cx| {
                Tooltip::new("Drag or scroll to move around; double-click switches between Fit and 100%")
                    .build(window, cx)
            });
        if let Some(image) = self.preview_image.clone() {
            let (pixel_width, pixel_height) = self.pixel_size();
            stack = stack.child(JPEGPreview::new(
                "jpeg-preview",
                image,
                pixel_width,
                pixel_height,
                self.zoom,
                self.offset,
                self.drag,
                self.display_scale,
                {
                    let entity = entity.clone();
                    move |offset, _, cx| entity.update(cx, |sheet, cx| sheet.set_offset(offset, cx))
                },
                {
                    let entity = entity.clone();
                    move |drag, _, cx| entity.update(cx, |sheet, cx| sheet.set_drag(drag, cx))
                },
                {
                    let entity = entity.clone();
                    move |zoom, _, cx| entity.update(cx, |sheet, cx| sheet.set_zoom(zoom, cx))
                },
            ));
        }
        if self.ready_options != Some(self.options) && self.error.is_none() {
            stack = stack.child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    div()
                        .p(px(BADGE_PADDING))
                        .rounded(px(BADGE_RADIUS))
                        .bg(MATERIAL)
                        .child(Spinner::new().small()),
                ),
            );
        }
        stack
    }

    /// The Quality row: the slider and its percentage.
    fn quality_row(&self, mono: &SharedString) -> Div {
        h_flex()
            .gap(px(ROW_SPACING))
            .child("Quality")
            .child(div().flex_1().min_w(px(0.0)).child(Slider::new(&self.quality)))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .justify_end()
                    .w(px(QUALITY_WIDTH))
                    .font_family(mono.clone())
                    .child(format!("{}%", (self.options.quality * 100.0).round() as i64)),
            )
    }

    /// The "Background for transparency" row: the color the transparency is flattened onto.
    fn background_row(&self) -> Div {
        let picked = self.picked.clone();
        h_flex()
            .gap(px(ROW_SPACING))
            .child("Background for transparency")
            .child(
                div()
                    .id("jpeg-matte-help")
                    .flex_shrink_0()
                    .tooltip(|window, cx| {
                        Tooltip::new("Color that fills transparent areas").build(window, cx)
                    })
                    .child(DialogColorSwatch::new(
                        self.session.clone(),
                        "JPEG Background",
                        PaletteColor::new(self.options.red, self.options.green, self.options.blue),
                        move |color| {
                            *picked.borrow_mut() = Some(color);
                        },
                    )),
            )
    }

    /// The footer row: the size, the state, and the buttons.
    fn footer(&self, entity: &Entity<Self>, mono: &SharedString) -> Div {
        let (width, height) = self.pixel_size();
        let status = if let Some(error) = self.error.as_ref() {
            div().text_color(ERROR_RED).child(error.clone())
        } else if self.ready_options == Some(self.options) {
            match self.result.as_ref() {
                Some(result) => div()
                    .font_family(mono.clone())
                    .child(byte_count_file(result.data.len())),
                None => div().text_color(SECONDARY).child("Updating…"),
            }
        } else {
            div().text_color(SECONDARY).child("Updating…")
        };
        let cancel = {
            let entity = entity.clone();
            Button::new("jpeg-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| {
                    entity.update(cx, |sheet, cx| sheet.close(None, window, cx))
                })
        };
        let export = {
            let entity = entity.clone();
            Button::new("jpeg-export")
                .label("Export…")
                .primary()
                .disabled(!self.can_export())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |sheet, cx| sheet.export(window, cx))
                })
        };
        h_flex()
            .gap(px(FOOTER_SPACING))
            .child(
                div()
                    .text_color(SECONDARY)
                    .child(format!("{} × {} px · sRGB", grouped(width), grouped(height))),
            )
            // `Spacer()`.
            .child(div().flex_1())
            .child(status)
            .child(cancel)
            .child(export)
    }
}

impl Render for JpegExportSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.display_scale = f64::from(window.scale_factor());
        self.drain(cx);
        if !self.started {
            self.started = true;
            // A Swift sheet holds the keyboard while it is up.
            window.focus(&self.focus, cx);
            self.start_encoding(cx);
            self.start_zoom_commands(cx);
        }
        self.build_preview_image(window, cx);
        let entity = cx.entity();
        let mono = cx.theme().mono_font_family.clone();
        let header = self.header(&entity);
        let preview = self.preview(&entity);
        let quality_row = self.quality_row(&mono);
        let background_row = self.background_row();
        let footer = self.footer(&entity, &mono);
        let keyboard = entity.clone();
        let focus = self.focus.clone();
        div()
            .track_focus(&focus)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(WIDTH))
                    .p(px(PADDING))
                    .gap(px(SPACING))
                    .rounded(px(RADIUS))
                    .bg(CARD_BACKGROUND)
                    .shadow_lg()
                    .text_size(px(TEXT_SIZE))
                    .child(header)
                    .child(preview)
                    .child(quality_row)
                    .child(background_row)
                    .child(footer),
            )
            // `configuredNativeShortcut(.escape)` / `.return`: the two buttons' keys. The card holds
            // the keyboard while it is up, as a Swift sheet does.
            .on_key_down(move |event: &KeyDownEvent, window, cx| match event.keystroke.key.as_str() {
                "escape" => {
                    keyboard.update(cx, |sheet, cx| sheet.close(None, window, cx));
                    cx.stop_propagation();
                }
                "enter" | "return" => {
                    keyboard.update(cx, |sheet, cx| sheet.export(window, cx));
                    cx.stop_propagation();
                }
                _ => {}
            })
    }
}

#[cfg(test)]
mod tests {
    // The builtin `#[test]`: the file's `use gpui_kit::*;` glob would otherwise shadow it with the
    // toolkit's `test` attribute macro (enabled by the test-support dev-dependency).
    use ::core::prelude::v1::test;
    use super::*;

    #[test]
    fn the_fit_zoom_divides_the_image_into_the_frame() {
        // An image larger than the frame in both directions fits by its narrower ratio.
        assert_eq!(JPEGPreview::fit_zoom(1120, 660, JPEGPreview::FRAME, 2.0), 1.0);
        assert_eq!(JPEGPreview::fit_zoom(1120, 660, JPEGPreview::FRAME, 1.0), 0.5);
        assert_eq!(JPEGPreview::fit_zoom(2240, 330, JPEGPreview::FRAME, 1.0), 0.25);
        // A scale of less than 1 counts as 1, as `max(1, displayScale)` did.
        assert_eq!(JPEGPreview::fit_zoom(560, 330, JPEGPreview::FRAME, 0.5), 1.0);
    }

    #[test]
    fn the_zoom_steps_run_through_the_swifts_stops() {
        assert_eq!(JPEGPreview::STEPS, [0.25, 0.5, 1.0, 2.0, 4.0, 8.0]);
        assert_eq!(JPEGPreview::step(1.0, 1), Some(2.0));
        assert_eq!(JPEGPreview::step(0.5, 1), Some(1.0));
        assert_eq!(JPEGPreview::step(8.0, 1), None);
        assert_eq!(JPEGPreview::step(1.0, -1), Some(0.5));
        assert_eq!(JPEGPreview::step(0.25, -1), None);
        // The 0.1% deadband keeps a step from landing on itself.
        assert_eq!(JPEGPreview::step(1.0005, 1), Some(2.0));
        assert_eq!(JPEGPreview::step(0.9996, -1), Some(0.5));
    }

    #[test]
    fn a_zoom_keeps_the_middle_of_the_view_on_the_same_spot() {
        let view = JPEGPreview::FRAME;
        // Fit starts at the center of the image.
        let centered = JPEGPreview::keep_centered(1120, 660, 1.0, None, Some(1.0), point(0.0, 0.0), view);
        assert_eq!(centered, point((1120.0 - 560.0) / 2.0, (660.0 - 330.0) / 2.0));
        // An image smaller than the view cannot scroll.
        assert_eq!(
            JPEGPreview::keep_centered(100, 100, 1.0, None, Some(1.0), point(0.0, 0.0), view),
            point(0.0, 0.0)
        );
        // Fit does nothing (`guard let new else { return }`).
        let kept = JPEGPreview::keep_centered(1120, 660, 1.0, Some(1.0), None, point(280.0, 165.0), view);
        assert_eq!(kept, point(280.0, 165.0));
    }

    #[test]
    fn the_size_reads_as_the_byte_formatter_writes_it() {
        assert_eq!(byte_count_file(0), "Zero bytes");
        assert_eq!(byte_count_file(1), "1 byte");
        assert_eq!(byte_count_file(999), "999 bytes");
        assert_eq!(byte_count_file(1000), "1 KB");
        assert_eq!(byte_count_file(1500), "1.5 KB");
        assert_eq!(byte_count_file(12_345), "12.3 KB");
        assert_eq!(byte_count_file(123_456), "123 KB");
        assert_eq!(byte_count_file(1_234_567), "1.23 MB");
        assert_eq!(byte_count_file(999_999), "1 MB");
        assert_eq!(grouped(1024), "1,024");
    }
}
