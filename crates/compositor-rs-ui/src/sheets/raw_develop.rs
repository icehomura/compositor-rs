//! Raw develop: the sheet that spends a RAW file's latitude before it becomes a layer (port of
//! `UI/RawDevelopSheet.swift`).
//!
//! A RAW file holds more range than a layer can, so the choice of what to keep is made here rather
//! than assumed. The preview develops at screen size while the sliders move; the import then
//! develops the full frame once.
//!
//! `.task(id: revision)` is the sheet's debounce: a settings change bumps the revision and starts a
//! task, which sleeps 60 ms before developing — just enough to coalesce a burst of slider changes,
//! since the render itself is nearly free once the file's filter is warm (see `RawImporter.Queue`).
//! A superseded task publishes nothing, the port's stand-in for SwiftUI cancelling the previous
//! task. The develop runs inline on the sheet's own executor: `SessionHost` is not `Send`. The app
//! pushes the rendered card as an absolute child (the Swift's `.sheet` has no equivalent here), and
//! it owns the buttons' `.keyboardShortcut` roles — Escape for Cancel, Return for Import — since the
//! card is not a window and takes no focus of its own. `.fixedSize()` is the card's own size: it is
//! laid out from its content rather than stretched; `.monospacedDigit()` on a readout is the
//! theme's monospace family, as `crop.rs`'s pixel readout does it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use compositor_rs_core::imported_image::{ImportedImage, PixelImage};
use compositor_rs_core::SharedImage;
use compositor_rs_io::image_exporter::{ExportRaster, ImageExporter};
use compositor_rs_io::raw_importer::RawDevelopSettings;
use compositor_rs_session::projects::SessionHost;
use compositor_rs_session::EditorSession;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::{h_flex, v_flex, Disableable as _};
use gpui_kit::*;

use crate::toolbar::status_bar::SECONDARY;

/// The card's padding, `.padding(24)`.
const PADDING: f32 = 24.0;
/// The card's corner radius and fill: the editor's dark card, as the alert card in
/// `content_view.rs` uses.
const RADIUS: f32 = 10.0;
const CARD_BACKGROUND: Hsla = hsla(0.0, 0.0, 0.18, 1.0);
/// The body text's size, SwiftUI's body at 13 points.
const TEXT_SIZE: f32 = 13.0;
/// The title's size, `.title2.bold()`.
const TITLE_SIZE: f32 = 17.0;
/// `VStack(alignment: .leading, spacing: 16)`.
const STACK_SPACING: f32 = 16.0;
/// The preview's size, `.frame(width: 560, height: 340)`, and its corner radius,
/// `RoundedRectangle(cornerRadius: 6)`.
const PREVIEW_WIDTH: f32 = 560.0;
const PREVIEW_HEIGHT: f32 = 340.0;
const PREVIEW_RADIUS: f32 = 6.0;
/// The preview's fill, `Color.black.opacity(0.35)`.
const PREVIEW_BACKGROUND: Hsla = hsla(0.0, 0.0, 0.0, 0.35);
/// `HStack(spacing: 10)` around a slider's label, its track and its readout.
const ROW_SPACING: f32 = 10.0;
/// `Text(title).frame(width: 90, alignment: .leading)`.
const LABEL_WIDTH: f32 = 90.0;
/// `Slider(...).frame(width: 300)`.
const SLIDER_WIDTH: f32 = 300.0;
/// The readout's column, `.frame(width: 80, alignment: .trailing)`.
const VALUE_WIDTH: f32 = 80.0;
/// The button row's `HStack` spacing.
const BUTTON_SPACING: f32 = 8.0;
/// The preview develop's pixel limit, `develop(file, settings: current, limit: 800)`.
const PREVIEW_LIMIT: usize = 800;
/// The coalescing sleep, `Task.sleep(for: .milliseconds(60))`.
const DEBOUNCE: Duration = Duration::from_millis(60);

/// One of the sheet's four sliders, in the order `slider(_:value:range:unit:precision:)` is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevelopSlider {
    Exposure,
    Temperature,
    Tint,
    Boost,
}

/// Every slider the sheet shows.
const DEVELOP_SLIDERS: [DevelopSlider; 4] = [
    DevelopSlider::Exposure,
    DevelopSlider::Temperature,
    DevelopSlider::Tint,
    DevelopSlider::Boost,
];

impl DevelopSlider {
    /// The row's label.
    fn label(self) -> &'static str {
        match self {
            Self::Exposure => "Exposure",
            Self::Temperature => "Temperature",
            Self::Tint => "Tint",
            Self::Boost => "Boost",
        }
    }

    /// The range its track spans.
    fn range(self) -> (f32, f32) {
        match self {
            Self::Exposure => (-3.0, 3.0),
            Self::Temperature => (2000.0, 12000.0),
            Self::Tint => (-150.0, 150.0),
            Self::Boost => (0.0, 1.0),
        }
    }

    /// The readout's suffix, `unit: " EV"`, `unit: " K"` or `unit: ""`.
    fn unit(self) -> &'static str {
        match self {
            Self::Exposure => " EV",
            Self::Temperature => " K",
            Self::Tint | Self::Boost => "",
        }
    }

    /// The readout's decimals, `precision:`.
    fn precision(self) -> usize {
        match self {
            Self::Exposure | Self::Boost => 2,
            Self::Temperature | Self::Tint => 0,
        }
    }

    /// The step the component library's slider has to be told: SwiftUI's slider was continuous, so
    /// the step is the readout's own precision.
    fn step(self) -> f32 {
        10.0f32.powi(-(self.precision() as i32))
    }

    /// The settings field the slider edits (`$settings.exposure` and the like).
    fn value(self, settings: &RawDevelopSettings) -> f32 {
        match self {
            Self::Exposure => settings.exposure,
            Self::Temperature => settings.temperature,
            Self::Tint => settings.tint,
            Self::Boost => settings.boost,
        }
    }

    /// Writes the dragged value back into the settings.
    fn set(self, settings: &mut RawDevelopSettings, value: f32) {
        match self {
            Self::Exposure => settings.exposure = value,
            Self::Temperature => settings.temperature = value,
            Self::Tint => settings.tint = value,
            Self::Boost => settings.boost = value,
        }
    }

    /// `String(format: "%.\(precision)f%@", value, unit)`.
    fn text(self, value: f32) -> String {
        format!("{:.*}{}", self.precision(), value, self.unit())
    }
}

/// The sheet's state: the file being developed, the settings the sliders edit, and the preview the
/// debounced task refreshes.
pub struct RawDevelopSheet {
    /// The session the import belongs to (`RawDevelopSheet.session`). The sheet reads nothing from
    /// it — the `finish` closure is the `finishRawDevelop` call site — but the app keeps the entity
    /// with the session whose import it answers.
    pub session: Entity<EditorSession>,
    /// The platform services the develop goes through (`RawImporter.Queue.shared`).
    host: Arc<dyn SessionHost>,
    /// The RAW file.
    url: PathBuf,
    /// `settings`: what the sliders edit.
    settings: RawDevelopSettings,
    /// The four sliders' own state, in [`DEVELOP_SLIDERS`] order.
    sliders: [Entity<SliderState>; 4],
    /// `preview`: the screen-sized development the task last published.
    preview: Option<SharedImage>,
    /// [`Self::preview`] as the toolkit's image (`Image(decorative:scale:)`), encoded once per
    /// developed raster.
    preview_image: Option<Arc<RenderImage>>,
    /// `working`: a develop is under way, so the small spinner shows over the preview.
    working: bool,
    /// `revision`: bumped by every settings change. The task a change starts develops the revision it
    /// began with, and a superseded one publishes nothing.
    revision: u64,
    /// `finish`: `session.finishRawDevelop(_:)`, given the settings or `None` on Cancel.
    finish: Arc<dyn Fn(Option<RawDevelopSettings>, &mut Window, &mut App)>,
}

impl RawDevelopSheet {
    /// The sheet for the RAW file at `url`, whose settings start as the session recorded them.
    pub fn new(
        session: Entity<EditorSession>,
        host: Arc<dyn SessionHost>,
        url: PathBuf,
        settings: RawDevelopSettings,
        finish: Arc<dyn Fn(Option<RawDevelopSettings>, &mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sliders = DEVELOP_SLIDERS.map(|slider| {
            let (lower, upper) = slider.range();
            cx.new(|_| {
                SliderState::new()
                    .min(lower)
                    .max(upper)
                    .step(slider.step())
                    .default_value(slider.value(&settings))
            })
        });
        Self::subscribe_sliders(&sliders, cx);
        // `.task(id: revision)` on appear: the first develop runs at once, with no debounce
        // (`revision == 0`).
        Self::develop(0, host.clone(), url.clone(), settings, cx);
        Self {
            session,
            host,
            url,
            settings,
            sliders,
            preview: None,
            preview_image: None,
            working: true,
            revision: 0,
            finish,
        }
    }

    /// `Slider(value:in:)` for each row: a change writes the setting, bumps the revision and starts
    /// the debounced develop, as the Swift `onChange(of: settings) { revision += 1 }` did.
    fn subscribe_sliders(sliders: &[Entity<SliderState>], cx: &mut Context<Self>) {
        for (index, slider) in sliders.iter().enumerate() {
            cx.subscribe(slider, move |this, _, event: &SliderEvent, cx| {
                let value = match event {
                    SliderEvent::Change(SliderValue::Single(value))
                    | SliderEvent::Release(SliderValue::Single(value)) => *value,
                    _ => return,
                };
                this.set_setting(DEVELOP_SLIDERS[index], value, cx);
                cx.notify();
            })
            .detach();
        }
    }

    /// One slider's value arrived from the track.
    fn set_setting(&mut self, slider: DevelopSlider, value: f32, cx: &mut Context<Self>) {
        if slider.value(&self.settings) == value {
            return;
        }
        slider.set(&mut self.settings, value);
        self.revision += 1;
        Self::develop(self.revision, self.host.clone(), self.url.clone(), self.settings, cx);
    }

    /// `refreshPreview()`: develops a screen-sized copy and publishes it, unless a newer change has
    /// superseded this task.
    fn develop(
        revision: u64,
        host: Arc<dyn SessionHost>,
        url: PathBuf,
        settings: RawDevelopSettings,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if revision > 0 {
                cx.background_executor().timer(DEBOUNCE).await;
            }
            // `guard !Task.isCancelled`: a task a later change superseded stops before it works.
            let current = this.update(cx, |sheet, _| sheet.revision == revision).unwrap_or(false);
            if !current {
                return;
            }
            let _ = this.update(cx, |sheet, cx| {
                sheet.working = true;
                let developed = host.develop_raw(&url, &settings, Some(PREVIEW_LIMIT)).ok();
                // The develop is synchronous here, so a superseded task still ends its own spinner
                // (`defer { working = false }`) but publishes nothing.
                if sheet.revision == revision {
                    if let Some(preview) = developed.and_then(developed_raster) {
                        sheet.preview = Some(preview);
                        sheet.preview_image = None;
                    }
                }
                sheet.working = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// `Button("Reset") { settings.reset() }`: back to the camera's own reading, and the sliders with
    /// it.
    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.reset();
        for (index, slider) in DEVELOP_SLIDERS.iter().enumerate() {
            let value = slider.value(&self.settings);
            self.sliders[index].update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.revision += 1;
        Self::develop(self.revision, self.host.clone(), self.url.clone(), self.settings, cx);
    }

    /// `Image(decorative:scale:)`: the preview's pixels as the toolkit's image, encoded once per
    /// developed raster, as `canvas_view.rs`'s `raster` does.
    fn developed_image(&mut self, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
        if self.preview_image.is_none() {
            let raster = ExportRaster::new(self.preview.clone()?);
            let bytes = ImageExporter::png_data(&raster).ok()?;
            let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
            self.preview_image = image.use_render_image(window, cx);
        }
        self.preview_image.clone()
    }

    /// One `slider(_:value:range:unit:precision:)` row: the label column, the track and the
    /// monospaced-digit readout.
    fn slider_row(&self, slider: DevelopSlider, index: usize, mono: &SharedString) -> Div {
        h_flex()
            .gap(px(ROW_SPACING))
            .items_center()
            .child(div().w(px(LABEL_WIDTH)).child(slider.label()))
            .child(div().w(px(SLIDER_WIDTH)).child(Slider::new(&self.sliders[index])))
            .child(
                div()
                    .w(px(VALUE_WIDTH))
                    .text_right()
                    .text_color(SECONDARY)
                    .font_family(mono.clone())
                    .child(slider.text(slider.value(&self.settings))),
            )
    }
}

impl Render for RawDevelopSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_image = self.developed_image(window, cx);
        let mono = cx.theme().mono_font_family.clone();
        let as_shot = self.settings.is_as_shot();
        let file_name = self.url.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let reset = {
            let entity = cx.entity();
            Button::new("raw-develop-reset")
                .label("Reset")
                .disabled(as_shot)
                .on_click(move |_, window, cx| entity.update(cx, |sheet, cx| sheet.reset(window, cx)))
        };
        let cancel = {
            let finish = self.finish.clone();
            Button::new("raw-develop-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| finish(None, window, cx))
        };
        let import = {
            let finish = self.finish.clone();
            let settings = self.settings;
            Button::new("raw-develop-import")
                .label("Import")
                // `.keyboardShortcut(.defaultAction)`: the default button is the prominent one.
                .primary()
                .on_click(move |_, window, cx| finish(Some(settings), window, cx))
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
                    .rounded(px(RADIUS))
                    .bg(CARD_BACKGROUND)
                    .text_size(px(TEXT_SIZE))
                    // `.fixedSize()`: the card takes its content's size, centered in the window.
                    .flex_none()
                    .child(
                        div()
                            .text_size(px(TITLE_SIZE))
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Develop “{file_name}”")),
                    )
                    .child(
                        div()
                            .relative()
                            .w(px(PREVIEW_WIDTH))
                            .h(px(PREVIEW_HEIGHT))
                            .rounded(px(PREVIEW_RADIUS))
                            .bg(PREVIEW_BACKGROUND)
                            .overflow_hidden()
                            .children(render_image.map(|image| img(image).size_full().object_fit(ObjectFit::Contain)))
                            .children(self.working.then(|| {
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(progress_indicator())
                            })),
                    )
                    .children(
                        DEVELOP_SLIDERS
                            .iter()
                            .enumerate()
                            .map(|(index, slider)| self.slider_row(*slider, index, &mono)),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(BUTTON_SPACING))
                            .child(reset)
                            .child(div().flex_1())
                            .child(cancel)
                            .child(import),
                    ),
            )
    }
}

/// `asset.image` when the develop produced color pixels; a mask's gray raster is never a develop
/// result.
fn developed_raster(asset: ImportedImage) -> Option<SharedImage> {
    match asset.image {
        PixelImage::Rgba(image) => Some(image),
        PixelImage::Gray(_) => None,
    }
}

/// `ProgressView().controlSize(.small)`: the component library's small spinner.
fn progress_indicator() -> impl IntoElement {
    use gpui_kit::component::Sizable as _;
    gpui_kit::component::spinner::Spinner::new().small()
}
