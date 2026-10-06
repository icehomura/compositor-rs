//! Camera Raw's panel state, and the session commands the panel's controls and canvas run.
//!
//! Ports the `extension EditorSession` of `Document/CameraRaw.swift` (`sampleCameraRawWhiteBalance`,
//! `applyCameraRawAutoWhiteBalance`, `sampleCameraRawDefringe`, `updateCameraRawReadout`,
//! `beginCameraRawDrag`, `dragCameraRaw`, `sampleCameraRawPointColor`, the guided-upright lines, and
//! the `cameraRawSample`/`cameraRawNormalizedPoint` reads behind them) plus the Camera Raw half of
//! `FilterEdit` in `Document/Filters.swift` — the section eyes, the curve/mixer/grading editors' page
//! state, the armed eyedroppers, the Option-drag clipping view, the sharpen-mask preview, the scope
//! and the pointer readout — which the port gathers into [`CameraRawPanel`].
//!
//! The grade itself is `compositor_rs_pixels::camera_raw` (`CameraRawSettings`, `CameraRawScope`, the
//! sub-settings and their normalization) with the kernels in `compositor_rs_pixels::adjust_pixels`; this
//! module never touches pixels except to read one for an eyedropper or the readout.
//!
//! **Async → sync.** Swift scanned the original layer for Auto white balance on a detached task and
//! applied the result only if Auto was still selected; the port's scan runs inline, so no older one is
//! ever in flight. Everything else keeps the Swift order: the panel writes its state, then calls
//! `updateFilter(_:preview:)`, exactly as the UI did.

use compositor_rs_core::geom::{Point, Rect};
use compositor_rs_core::image_ops::FilterKind;
use compositor_rs_core::imported_image::PixelImage;
use compositor_rs_core::Rgba8Image;
use compositor_rs_pixels::camera_raw::{
    CameraRawClipping, CameraRawCurvePage, CameraRawGeometryGuide, CameraRawGradePage,
    CameraRawMixerPage, CameraRawMixerSettings, CameraRawMixerTab, CameraRawPointChannel,
    CameraRawPointColor, CameraRawScope, CameraRawScopeMode, CameraRawSettings,
    CameraRawUprightMode, CameraRawWhiteBalance,
};

use crate::filters::FilterEdit;
use crate::EditorSession;

/// Camera Raw's adjustment column, in the order the panel lists its sections
/// (`CameraRawControls.Section`, whose declaration order is `allCases`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CameraRawSection {
    Light,
    Color,
    ColorGrading,
    Effects,
    Curve,
    ColorMixer,
    Detail,
    Optics,
    Geometry,
    Calibration,
}

impl CameraRawSection {
    /// `Section.allCases`.
    pub const ALL: [CameraRawSection; 10] = [
        CameraRawSection::Light,
        CameraRawSection::Color,
        CameraRawSection::ColorGrading,
        CameraRawSection::Effects,
        CameraRawSection::Curve,
        CameraRawSection::ColorMixer,
        CameraRawSection::Detail,
        CameraRawSection::Optics,
        CameraRawSection::Geometry,
        CameraRawSection::Calibration,
    ];

    /// The section's heading (`Section.rawValue`).
    pub fn raw_value(self) -> &'static str {
        match self {
            CameraRawSection::Light => "Light",
            CameraRawSection::Color => "Color",
            CameraRawSection::ColorGrading => "Color Grading",
            CameraRawSection::Effects => "Effects",
            CameraRawSection::Curve => "Curve",
            CameraRawSection::ColorMixer => "Color Mixer",
            CameraRawSection::Detail => "Detail",
            CameraRawSection::Optics => "Optics",
            CameraRawSection::Geometry => "Geometry",
            CameraRawSection::Calibration => "Calibration",
        }
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|section| section.raw_value() == value)
    }
}

/// Camera Raw's panel state: the `FilterEdit` fields the Swift kept beside the grade (section eyes,
/// the curve/mixer/grading editors, the armed eyedroppers, the preview-only views and the scope).
/// Fields drop the redundant `cameraRaw` prefix the way `FilterEdit.panel`'s consumers already write
/// them (`panel.clipping`, `panel.scope`).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawPanel {
    /// Panel eyes (`showsCameraRawLight` … `showsCameraRawCalibration`). Off drops that group's
    /// amounts from the preview and from OK, without clearing the sliders.
    pub shows_light: bool,
    pub shows_color: bool,
    pub shows_effects: bool,
    pub shows_curve: bool,
    pub shows_mixer: bool,
    pub shows_grading: bool,
    pub shows_detail: bool,
    pub shows_optics: bool,
    pub shows_geometry: bool,
    pub shows_calibration: bool,
    /// `cameraRawCurvePage`: which half of the Curve section is showing.
    pub curve_page: CameraRawCurvePage,
    /// `cameraRawPointChannel`: the channel a point curve edits.
    pub point_channel: CameraRawPointChannel,
    /// `cameraRawMixerPage`.
    pub mixer_page: CameraRawMixerPage,
    /// `cameraRawMixerTab`: which shift the targeted adjustment writes.
    pub mixer_tab: CameraRawMixerTab,
    /// `cameraRawMixerSwatch`: the color family the HSL page is showing.
    pub mixer_swatch: usize,
    /// `cameraRawPointIndex`: the Point Color the eyedropper replaces (or where the next one goes).
    pub point_index: usize,
    /// `cameraRawGradePage`.
    pub grade_page: CameraRawGradePage,
    /// `targetsCameraRawCurve`: the curve's targeted adjustment is armed.
    pub targets_curve: bool,
    /// `targetsCameraRawMixer`: the mixer's targeted adjustment is armed.
    pub targets_mixer: bool,
    /// `samplesPointColor`: the Point Color eyedropper is armed.
    pub samples_point_color: bool,
    /// `samplesWhiteBalance`: the white-balance eyedropper is armed.
    pub samples_white_balance: bool,
    /// `samplesDefringe`: the defringe eyedropper is armed.
    pub samples_defringe: bool,
    /// `drawingCameraRawGeometryGuide`: guided-upright lines are being drawn on the preview.
    pub drawing_geometry_guide: bool,
    /// `cameraRawGuideDraft`: the line being dragged.
    pub guide_draft: Option<CameraRawGuideDraft>,
    /// `cameraRawClipping`: set while Option is held on Exposure, Highlights, Shadows, Whites or
    /// Blacks. Preview only; never written into the layer.
    pub clipping: Option<CameraRawClipping>,
    /// `cameraRawSharpenMask`: set while Option is held on Sharpening Masking.
    pub sharpen_mask: bool,
    /// `showsShadowClipping`: the histogram's clipping indicator, painted on the preview.
    pub shows_shadow_clipping: bool,
    /// `showsHighlightClipping`: the histogram's clipping indicator, painted on the preview.
    pub shows_highlight_clipping: bool,
    /// `cameraRawScopeMode`: the histogram, or the vectorscope shown in its place.
    pub scope_mode: CameraRawScopeMode,
    /// `cameraRawScope`: the newest scope, counted from the graded preview.
    pub scope: Option<CameraRawScope>,
    /// `cameraRawReadout`: RGB of the pixel under the pointer, in the adjusted preview.
    pub readout: Option<CameraRawReadout>,
}

impl Default for CameraRawPanel {
    fn default() -> Self {
        Self {
            shows_light: true,
            shows_color: true,
            shows_effects: true,
            shows_curve: true,
            shows_mixer: true,
            shows_grading: true,
            shows_detail: true,
            shows_optics: true,
            shows_geometry: true,
            shows_calibration: true,
            curve_page: CameraRawCurvePage::default(),
            point_channel: CameraRawPointChannel::default(),
            mixer_page: CameraRawMixerPage::default(),
            mixer_tab: CameraRawMixerTab::default(),
            mixer_swatch: 0,
            point_index: 0,
            grade_page: CameraRawGradePage::default(),
            targets_curve: false,
            targets_mixer: false,
            samples_point_color: false,
            samples_white_balance: false,
            samples_defringe: false,
            drawing_geometry_guide: false,
            guide_draft: None,
            clipping: None,
            sharpen_mask: false,
            shows_shadow_clipping: false,
            shows_highlight_clipping: false,
            scope_mode: CameraRawScopeMode::default(),
            scope: None,
            readout: None,
        }
    }
}

impl CameraRawPanel {
    /// `renderSettings()`: the sliders as they will be rendered — a hidden group contributes nothing.
    pub fn render_settings(&self, settings: &CameraRawSettings) -> CameraRawSettings {
        settings.applying(
            self.shows_light,
            self.shows_color,
            self.shows_effects,
            self.shows_curve,
            self.shows_mixer,
            self.shows_grading,
            self.shows_detail,
            self.shows_optics,
            self.shows_geometry,
            self.shows_calibration,
        )
    }

    /// `pointColorVisualizeIndex`: the picked color whose pixels are lit, or −1.
    pub fn point_color_visualize_index(&self, settings: &CameraRawSettings) -> i32 {
        match settings.mixer.points.get(self.point_index) {
            Some(point) if point.visualize => self.point_index as i32,
            _ => -1,
        }
    }

    /// `showsCameraRaw…`, by section.
    pub fn shows(&self, section: CameraRawSection) -> bool {
        match section {
            CameraRawSection::Light => self.shows_light,
            CameraRawSection::Color => self.shows_color,
            CameraRawSection::ColorGrading => self.shows_grading,
            CameraRawSection::Effects => self.shows_effects,
            CameraRawSection::Curve => self.shows_curve,
            CameraRawSection::ColorMixer => self.shows_mixer,
            CameraRawSection::Detail => self.shows_detail,
            CameraRawSection::Optics => self.shows_optics,
            CameraRawSection::Geometry => self.shows_geometry,
            CameraRawSection::Calibration => self.shows_calibration,
        }
    }

    pub fn set_shows(&mut self, section: CameraRawSection, shows: bool) {
        match section {
            CameraRawSection::Light => self.shows_light = shows,
            CameraRawSection::Color => self.shows_color = shows,
            CameraRawSection::ColorGrading => self.shows_grading = shows,
            CameraRawSection::Effects => self.shows_effects = shows,
            CameraRawSection::Curve => self.shows_curve = shows,
            CameraRawSection::ColorMixer => self.shows_mixer = shows,
            CameraRawSection::Detail => self.shows_detail = shows,
            CameraRawSection::Optics => self.shows_optics = shows,
            CameraRawSection::Geometry => self.shows_geometry = shows,
            CameraRawSection::Calibration => self.shows_calibration = shows,
        }
    }

    /// The section header's eye: `session.filterEdit?.showsCameraRaw….toggle()`.
    pub fn toggle_shows(&mut self, section: CameraRawSection) {
        self.set_shows(section, !self.shows(section));
    }
}

/// A targeted-adjustment drag in progress (`CameraRawDrag`): where the pointer started, the grade it
/// started from, and the tone and hue under the pointer when it began.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRawDrag {
    pub start_y: f64,
    pub settings: CameraRawSettings,
    pub tone: f64,
    pub hue: f64,
}

/// The guided-upright line being dragged (`cameraRawGuideDraft`), in normalized image coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRawGuideDraft {
    pub start: Point,
    pub end: Point,
}

/// The RGB of the pixel under the pointer in the adjusted preview (`cameraRawReadout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraRawReadout {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

/// `cameraRawSample(at:)`'s reading of the pixel under the pointer.
struct CameraRawSample {
    tone: f64,
    hue: f64,
    saturation: f64,
    luminance: f64,
}

/// One pixel of a premultiplied buffer, as straight 0…1 channels. `None` outside the image or over a
/// fully transparent pixel.
fn straight_pixel(image: &Rgba8Image, point: Point) -> Option<(f64, f64, f64)> {
    if !(point.x >= 0.0
        && point.y >= 0.0
        && point.x < image.width() as f64
        && point.y < image.height() as f64)
    {
        return None;
    }
    let [red, green, blue, alpha] = image.get(point.x.floor() as usize, point.y.floor() as usize);
    let alpha = f64::from(alpha);
    if alpha == 0.0 {
        return None;
    }
    Some((
        (f64::from(red) / alpha).min(1.0),
        (f64::from(green) / alpha).min(1.0),
        (f64::from(blue) / alpha).min(1.0),
    ))
}

/// The same for the layer's own pixels; an 8-bit gray asset reads as equal channels. Shared with the
/// other eyedroppers (`crate::filters`'s `sampleLevels`).
pub(crate) fn straight_image_pixel(image: &PixelImage, point: Point) -> Option<(f64, f64, f64)> {
    if !(point.x >= 0.0
        && point.y >= 0.0
        && point.x < image.width() as f64
        && point.y < image.height() as f64)
    {
        return None;
    }
    let x = point.x.floor() as usize;
    let y = point.y.floor() as usize;
    match image {
        PixelImage::Rgba(pixels) => straight_pixel(pixels, Point::new(x as f64, y as f64)),
        PixelImage::Gray(pixels) => {
            let value = f64::from(pixels.get(x, y)) / 255.0;
            Some((value, value, value))
        }
    }
}

/// `updateCameraRawReadout`'s straight 8-bit channels: `(value * 255 + alpha / 2) / alpha`, capped.
fn straight_readout(image: &Rgba8Image, point: Point) -> Option<CameraRawReadout> {
    if !(point.x >= 0.0
        && point.y >= 0.0
        && point.x < image.width() as f64
        && point.y < image.height() as f64)
    {
        return None;
    }
    let [red, green, blue, alpha] = image.get(point.x.floor() as usize, point.y.floor() as usize);
    let alpha = i32::from(alpha);
    if alpha == 0 {
        return None;
    }
    let straight = |value: u8| ((i32::from(value) * 255 + alpha / 2) / alpha).min(255) as u8;
    Some(CameraRawReadout {
        red: straight(red),
        green: straight(green),
        blue: straight(blue),
    })
}

/// `cameraRawSample(at:)`: the tone, hue, saturation and luminance under `at` of the preview the
/// panel shows (`preparedPreview ?? previewSource`). `None` outside the layer or over a transparent
/// pixel.
fn camera_raw_sample(edit: &FilterEdit, at: Point) -> Option<CameraRawSample> {
    let prepared = edit.prepared_preview();
    let image = prepared.as_ref().unwrap_or(&edit.preview_source);
    let pixel = edit.preview_mapping.inverted().applying(at);
    let (red, green, blue) = straight_pixel(image, pixel)?;
    let max_channel = red.max(green).max(blue);
    let min_channel = red.min(green).min(blue);
    let chroma = max_channel - min_channel;
    let mut hue = 0.0;
    if chroma > 1e-6 {
        hue = if max_channel == red {
            (green - blue) / chroma
        } else if max_channel == green {
            2.0 + (blue - red) / chroma
        } else {
            4.0 + (red - green) / chroma
        };
        hue /= 6.0;
        if hue < 0.0 {
            hue += 1.0;
        }
    }
    let tone = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    let saturation = if max_channel == 0.0 {
        0.0
    } else {
        chroma / max_channel
    };
    Some(CameraRawSample {
        tone,
        hue: hue * 360.0,
        saturation,
        luminance: (max_channel + min_channel) / 2.0,
    })
}

/// `cameraRawNormalizedPoint(at:)`: the point in the preview image's own 0…1 grid.
fn camera_raw_normalized_point(edit: &FilterEdit, at: Point) -> Option<Point> {
    let image = &edit.preview_source;
    let pixel = edit.preview_mapping.inverted().applying(at);
    if !(pixel.x >= 0.0
        && pixel.y >= 0.0
        && pixel.x < image.width() as f64
        && pixel.y < image.height() as f64)
    {
        return None;
    }
    Some(Point::new(
        pixel.x / image.width() as f64,
        pixel.y / image.height() as f64,
    ))
}

/// The Camera Raw panel's armed eyedroppers, read by the canvas to pick rather than paint
/// (`FilterEdit.samplesWhiteBalance`, `samplesPointColor`, `samplesDefringe`).
impl FilterEdit {
    /// `samplesWhiteBalance`: the white-balance eyedropper is armed.
    pub fn samples_white_balance(&self) -> bool {
        self.panel.samples_white_balance
    }

    /// `samplesPointColor`: the Point Color eyedropper is armed.
    pub fn samples_point_color(&self) -> bool {
        self.panel.samples_point_color
    }

    /// `samplesDefringe`: the defringe eyedropper is armed.
    pub fn samples_defringe(&self) -> bool {
        self.panel.samples_defringe
    }
}

/// The `extension EditorSession` of `Document/CameraRaw.swift`.
impl EditorSession {
    /// `sampleCameraRawWhiteBalance(at:)`: the clicked pixel of the original layer becomes neutral.
    pub fn sample_camera_raw_white_balance(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw
            || !edit.panel.samples_white_balance
            || edit.committing
        {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        if !Rect::from_origin_size(Point::ZERO, document.size()).contains(at) {
            return;
        }
        let pixel = edit.mapping.inverted().applying(at);
        let Some((red, green, blue)) = straight_image_pixel(&edit.original.image, pixel) else {
            return;
        };
        let Some((temperature, tint)) = CameraRawSettings::neutralize_straight(red, green, blue)
        else {
            return;
        };
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        edit.camera_raw.temperature = temperature;
        edit.camera_raw.tint = tint;
        edit.camera_raw.white_balance = CameraRawWhiteBalance::Custom;
        let settings = edit.settings.clone();
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    /// `applyCameraRawAutoWhiteBalance()`: White Balance › Auto. The mode is stored before the scan,
    /// so the picker moves on the click; the average of the original layer is applied only if Auto is
    /// still selected.
    ///
    /// The Swift took the average off the main thread and let a newer choice supersede one still
    /// running; the port's scan is synchronous, so no older one is ever in flight.
    pub fn apply_camera_raw_auto_white_balance(&mut self) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw || edit.committing {
            return;
        }
        let settings = edit.settings.clone();
        let preview = edit.preview;
        if let Some(edit) = self.filter_edit.as_mut() {
            edit.camera_raw.white_balance = CameraRawWhiteBalance::Auto;
        }
        self.update_filter(settings, preview);
        let solved = self
            .filter_edit
            .as_ref()
            .filter(|edit| {
                !edit.committing && edit.camera_raw.white_balance == CameraRawWhiteBalance::Auto
            })
            .and_then(|edit| edit.original.image.as_rgba())
            .and_then(CameraRawSettings::auto_balance);
        let Some((temperature, tint)) = solved else {
            return;
        };
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        let settings = edit.settings.clone();
        let preview = edit.preview;
        if let Some(edit) = self.filter_edit.as_mut() {
            edit.camera_raw.temperature = temperature;
            edit.camera_raw.tint = tint;
        }
        self.update_filter(settings, preview);
    }

    /// `sampleCameraRawDefringe(at:)`: centers the purple or green hue range on the clicked fringe
    /// color.
    pub fn sample_camera_raw_defringe(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw || !edit.panel.samples_defringe || edit.committing {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        if !Rect::from_origin_size(Point::ZERO, document.size()).contains(at) {
            return;
        }
        let pixel = edit.mapping.inverted().applying(at);
        let Some((red, green, blue)) = straight_image_pixel(&edit.original.image, pixel) else {
            return;
        };
        let hue = CameraRawSettings::hue_degrees(red, green, blue);
        let (purple_center, green_center, span) = (290.0, 90.0, 25.0);
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        if (hue - purple_center).abs() < (hue - green_center).abs() {
            edit.camera_raw.optics.purple_hue_low = hue - span;
            edit.camera_raw.optics.purple_hue_high = hue + span;
            if edit.camera_raw.optics.purple_amount == 0.0 {
                edit.camera_raw.optics.purple_amount = 50.0;
            }
        } else {
            edit.camera_raw.optics.green_hue_low = hue - span;
            edit.camera_raw.optics.green_hue_high = hue + span;
            if edit.camera_raw.optics.green_amount == 0.0 {
                edit.camera_raw.optics.green_amount = 50.0;
            }
        }
        edit.camera_raw.optics = edit.camera_raw.optics.normalized();
        let settings = edit.settings.clone();
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    /// `updateCameraRawReadout(at:)`: RGB under the pointer while Camera Raw is open. Outside the
    /// layer, or over a transparent pixel, clears the readout.
    pub fn update_camera_raw_readout(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw {
            return;
        }
        let prepared = edit.prepared_preview();
        let image = prepared.as_ref().unwrap_or(&edit.preview_source);
        let pixel = edit.preview_mapping.inverted().applying(at);
        let readout = straight_readout(image, pixel);
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        edit.panel.readout = readout;
    }

    /// `beginCameraRawDrag(at:)`: the targeted adjustment takes the grade and the tone/hue under the
    /// pointer as its starting point.
    pub fn begin_camera_raw_drag(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw {
            return;
        }
        let Some(sample) = camera_raw_sample(edit, at) else {
            return;
        };
        let drag = CameraRawDrag {
            start_y: at.y,
            settings: edit.camera_raw.clone(),
            tone: sample.tone,
            hue: sample.hue,
        };
        self.camera_raw_drag = Some(drag);
    }

    /// `dragCameraRaw(to:)`: vertical drags nudge the curve's region (or point) or the mixer's family
    /// shifts, always from the grade the drag started with.
    pub fn drag_camera_raw(&mut self, to: Point) {
        let Some(drag) = self.camera_raw_drag.clone() else {
            return;
        };
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        let delta = (drag.start_y - to.y) * 0.35;
        let mut camera_raw = drag.settings.clone();
        if edit.panel.targets_curve {
            if edit.panel.curve_page == CameraRawCurvePage::Parametric {
                let region = drag.settings.curve.region(drag.tone);
                let value = drag.settings.curve.amount(region) + delta;
                camera_raw.curve.set_amount(region, value);
            } else {
                camera_raw.curve =
                    drag.settings
                        .curve
                        .nudged(edit.panel.point_channel, drag.tone, delta / 100.0);
            }
        } else if edit.panel.targets_mixer {
            let weights = CameraRawMixerSettings::weights(drag.hue);
            for index in 0..8 {
                let weight = weights[index];
                if weight <= 0.0 {
                    continue;
                }
                match edit.panel.mixer_tab {
                    CameraRawMixerTab::Hue => {
                        camera_raw.mixer.hue[index] =
                            (drag.settings.mixer.hue[index] + delta * weight).clamp(-100.0, 100.0);
                    }
                    CameraRawMixerTab::Saturation => {
                        camera_raw.mixer.saturation[index] =
                            (drag.settings.mixer.saturation[index] + delta * weight)
                                .clamp(-100.0, 100.0);
                    }
                    CameraRawMixerTab::Luminance => {
                        camera_raw.mixer.luminance[index] = (drag.settings.mixer.luminance[index]
                            + delta * weight)
                            .clamp(-100.0, 100.0);
                    }
                }
            }
        }
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        edit.camera_raw = camera_raw;
        let settings = edit.settings.clone();
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    /// `sampleCameraRawPointColor(at:)`: picks the color under the pointer into the Point Color
    /// slot the panel points at, keeping that slot's shifts.
    pub fn sample_camera_raw_point_color(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if !edit.panel.samples_point_color {
            return;
        }
        let Some(sample) = camera_raw_sample(edit, at) else {
            return;
        };
        let index = edit.panel.point_index;
        let mut color = CameraRawPointColor {
            hue: sample.hue,
            saturation: sample.saturation,
            luminance: sample.luminance,
            ..CameraRawPointColor::default()
        };
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        let mut picked_new = false;
        if let Some(existing) = edit.camera_raw.mixer.points.get(index).copied() {
            color.hue_shift = existing.hue_shift;
            color.saturation_shift = existing.saturation_shift;
            color.luminance_shift = existing.luminance_shift;
            edit.camera_raw.mixer.points[index] = color;
        } else if edit.camera_raw.mixer.points.len() < 8 {
            edit.camera_raw.mixer.points.push(color);
            picked_new = true;
        }
        if picked_new {
            edit.panel.point_index = edit.camera_raw.mixer.points.len() - 1;
        }
        let settings = edit.settings.clone();
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }

    /// `beginCameraRawGeometryGuide(at:)`: guided upright starts a line at the pointer.
    pub fn begin_camera_raw_geometry_guide(&mut self, at: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if edit.kind != FilterKind::CameraRaw || !edit.panel.drawing_geometry_guide {
            return;
        }
        let Some(normalized) = camera_raw_normalized_point(edit, at) else {
            return;
        };
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        edit.panel.guide_draft = Some(CameraRawGuideDraft {
            start: normalized,
            end: normalized,
        });
    }

    /// `continueCameraRawGeometryGuide(to:)`: the line follows the pointer.
    pub fn continue_camera_raw_geometry_guide(&mut self, to: Point) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        if !edit.panel.drawing_geometry_guide {
            return;
        }
        let Some(normalized) = camera_raw_normalized_point(edit, to) else {
            return;
        };
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        let Some(draft) = edit.panel.guide_draft.as_mut() else {
            return;
        };
        draft.end = normalized;
    }

    /// `commitCameraRawGeometryGuide()`: the line is kept and Guided Upright is selected.
    pub fn commit_camera_raw_geometry_guide(&mut self) {
        let Some(edit) = self.filter_edit.as_ref() else {
            return;
        };
        let Some(draft) = edit.panel.guide_draft else {
            return;
        };
        let Some(edit) = self.filter_edit.as_mut() else {
            return;
        };
        edit.camera_raw
            .geometry
            .guides
            .push(CameraRawGeometryGuide::new(
                draft.start.x,
                draft.start.y,
                draft.end.x,
                draft.end.y,
            ));
        edit.camera_raw.geometry.upright = CameraRawUprightMode::Guided;
        edit.panel.guide_draft = None;
        let settings = edit.settings.clone();
        let preview = edit.preview;
        self.update_filter(settings, preview);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        straight_image_pixel, straight_pixel, CameraRawPanel, CameraRawReadout, CameraRawSection,
    };
    use compositor_rs_core::geom::Point;
    use compositor_rs_core::imported_image::PixelImage;
    use compositor_rs_core::Rgba8Image;
    use compositor_rs_pixels::camera_raw::{
        CameraRawPointColor, CameraRawScopeMode, CameraRawSettings, CameraRawWhiteBalance,
    };

    #[test]
    fn the_panel_starts_with_every_group_shown_and_the_first_page_of_each_editor() {
        let panel = CameraRawPanel::default();
        assert!(CameraRawSection::ALL
            .iter()
            .all(|section| panel.shows(*section)));
        assert_eq!(panel.scope_mode, CameraRawScopeMode::Histogram);
        assert!(panel.scope.is_none() && panel.readout.is_none());
        assert!(panel.clipping.is_none() && !panel.sharpen_mask);
        assert!(
            !panel.samples_white_balance && !panel.samples_point_color && !panel.samples_defringe
        );
        assert_eq!(panel.curve_page.raw_value(), "Parametric");
        assert_eq!(panel.point_channel.raw_value(), "RGB");
        assert_eq!(panel.mixer_page.raw_value(), "HSL");
        assert_eq!(panel.mixer_tab.raw_value(), "Hue");
        assert_eq!(panel.grade_page.raw_value(), "Three-Way");
        assert_eq!(panel.point_index, 0);
        assert_eq!(panel.mixer_swatch, 0);
    }

    #[test]
    fn hiding_a_group_drops_its_amounts_from_the_rendered_grade() {
        let settings = CameraRawSettings {
            white_balance: CameraRawWhiteBalance::Custom,
            exposure: 1.5,
            temperature: 20.0,
            grain_amount: 30.0,
            ..CameraRawSettings::default()
        };
        assert_eq!(
            CameraRawPanel::default().render_settings(&settings),
            settings
        );
        let mut panel = CameraRawPanel::default();
        panel.toggle_shows(CameraRawSection::Light);
        panel.toggle_shows(CameraRawSection::Color);
        let hidden = panel.render_settings(&settings);
        assert_eq!(hidden.exposure, 0.0);
        assert_eq!(hidden.temperature, 0.0);
        assert_eq!(hidden.grain_amount, 30.0, "the Effects eye stayed on");
    }

    #[test]
    fn the_visualized_point_color_is_only_reported_for_the_panel_s_point() {
        let mut settings = CameraRawSettings::default();
        settings.mixer.points = vec![CameraRawPointColor {
            visualize: true,
            ..CameraRawPointColor::default()
        }];
        let mut panel = CameraRawPanel::default();
        assert_eq!(panel.point_color_visualize_index(&settings), 0);
        panel.point_index = 1;
        assert_eq!(
            panel.point_color_visualize_index(&settings),
            -1,
            "no such point"
        );
        settings.mixer.points[0].visualize = false;
        panel.point_index = 0;
        assert_eq!(
            panel.point_color_visualize_index(&settings),
            -1,
            "not visualized"
        );
    }

    #[test]
    fn straight_reads_un_premultiply_and_a_gray_layer_reads_as_equal_channels() {
        let mut image = Rgba8Image::new(2, 1);
        image.set(0, 0, [128, 0, 64, 128]);
        image.set(1, 0, [0, 0, 0, 0]);
        let (red, green, blue) = straight_pixel(&image, Point::new(0.0, 0.0)).expect("opaque");
        assert_eq!((red, green, blue), (1.0, 0.0, 0.5));
        assert!(
            straight_pixel(&image, Point::new(1.0, 0.0)).is_none(),
            "transparent"
        );
        assert!(
            straight_pixel(&image, Point::new(2.0, 0.0)).is_none(),
            "outside"
        );

        let gray = PixelImage::Gray(std::sync::Arc::new(
            compositor_rs_core::buffer::Gray8Image::uniform(1, 1, 51),
        ));
        let (red, green, blue) = straight_image_pixel(&gray, Point::new(0.0, 0.0)).expect("gray");
        assert_eq!((red, green, blue), (0.2, 0.2, 0.2));
    }

    #[test]
    fn a_readout_rounds_premultiplied_bytes_back_to_straight_eights() {
        let mut image = Rgba8Image::new(1, 1);
        image.set(0, 0, [64, 128, 0, 128]);
        assert_eq!(
            super::straight_readout(&image, Point::new(0.0, 0.0)),
            Some(CameraRawReadout {
                red: 128,
                green: 255,
                blue: 0
            })
        );
        assert!(super::straight_readout(&image, Point::new(1.0, 0.0)).is_none());
    }
}
