//! The Type tool's model: the text alignment, the style a text layer keeps so it can be edited again, the
//! per-letter color and face runs, the text layer itself and the draft being typed.
//!
//! Ported from `Document/TypeTool.swift` (the model types and the style mutators; the session commands, the
//! inline editor and the rasterization live elsewhere).

use crate::buffer::SharedImage;
use crate::color::PaletteColor;
use crate::geom::{CGFloat, Point, Size};
use crate::layer_transform::LayerTransform;
use crate::limits::{MAX_SIDE_EXTENT, MAX_SURFACE_EXTENT};
use crate::Id;
use serde::{Deserialize, Serialize};

/// An offset range into `content`, in UTF-16 units — the `NSRange` the type tool works in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextRange {
    pub location: isize,
    pub length: isize,
}

impl TextRange {
    /// `NSRange(location: 0, length: 0)`.
    pub const EMPTY: TextRange = TextRange { location: 0, length: 0 };

    pub const fn new(location: isize, length: isize) -> Self {
        TextRange { location, length }
    }
}

/// How a paragraph's lines are set within its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextAlignment {
    #[serde(rename = "Left")]
    Left,
    #[serde(rename = "Center")]
    Center,
    #[serde(rename = "Right")]
    Right,
}

impl TextAlignment {
    /// Every alignment, in the order the font panel lists them.
    pub const ALL: [TextAlignment; 3] = [TextAlignment::Left, TextAlignment::Center, TextAlignment::Right];

    /// The manifest's raw string.
    pub const fn raw_value(self) -> &'static str {
        match self {
            TextAlignment::Left => "Left",
            TextAlignment::Center => "Center",
            TextAlignment::Right => "Right",
        }
    }

    /// The alignment a manifest's raw string names.
    pub fn from_raw(value: &str) -> Option<TextAlignment> {
        match value {
            "Left" => Some(TextAlignment::Left),
            "Center" => Some(TextAlignment::Center),
            "Right" => Some(TextAlignment::Right),
            _ => None,
        }
    }
}

impl Default for TextAlignment {
    fn default() -> Self {
        TextAlignment::Left
    }
}

/// Everything a text layer is: what it says, how it is set and how it wraps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LayerTextStyle {
    pub content: String,
    pub font_name: String,
    pub font_size: CGFloat,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
    pub alignment: TextAlignment,
    pub tracking: CGFloat,
    /// Baseline to baseline, in layer pixels, as Photoshop's Leading is. 0 is Auto: 120% of the font size.
    pub leading: CGFloat,
    /// Fixed paragraph bounds in layer pixels. Nil supports older point-text layers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub box_size: Option<Size>,
    /// Letters painted in a color other than `red`/`green`/`blue`, in UTF-16 offsets into `content`, sorted and not
    /// overlapping. Nil when the whole text is one color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_runs: Option<Vec<LayerTextColorRun>>,
    /// Letters set in a face other than `font_name`, in the same offsets. Nil when the whole text is one face.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_runs: Option<Vec<LayerTextFontRun>>,
}

impl Default for LayerTextStyle {
    fn default() -> Self {
        LayerTextStyle {
            content: "Text".to_string(),
            font_name: "Helvetica".to_string(),
            font_size: 72.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alignment: TextAlignment::Left,
            tracking: 0.0,
            leading: 0.0,
            box_size: None,
            color_runs: None,
            font_runs: None,
        }
    }
}

impl LayerTextStyle {
    /// The gap between the text and its box, in layer pixels — the same for point text and a fixed box, so turning
    /// one into the other doesn't move the text, and wide enough to leave the box's edges easy to grab.
    pub const PADDING: CGFloat = 12.0;

    /// 120% of the font size — Photoshop's Auto leading.
    pub fn auto_leading(&self) -> CGFloat {
        self.font_size * 1.2
    }

    /// The line's whole height: `leading` when it was set, Auto otherwise.
    pub fn line_height(&self) -> CGFloat {
        if self.leading > 0.0 {
            self.leading
        } else {
            self.auto_leading()
        }
    }

    /// Whether the paragraph bounds are usable: finite, at least 16 pixels on a side, no more than a side or a
    /// surface may be.
    pub fn box_is_valid(&self) -> bool {
        match self.box_size {
            None => true,
            Some(box_size) => {
                box_size.width.is_finite()
                    && box_size.height.is_finite()
                    && (16.0..=MAX_SIDE_EXTENT).contains(&box_size.width)
                    && (16.0..=MAX_SIDE_EXTENT).contains(&box_size.height)
                    && box_size.width * box_size.height <= MAX_SURFACE_EXTENT
            }
        }
    }

    /// Whether this style can be rasterized and saved.
    pub fn is_valid(&self) -> bool {
        self.content_units() <= 100_000
            && self.box_is_valid()
            && self.font_size.is_finite()
            && (1.0..=2000.0).contains(&self.font_size)
            && [self.red, self.green, self.blue]
                .iter()
                .all(|component| component.is_finite() && (0.0..=1.0).contains(component))
            && self.tracking.is_finite()
            && (-100.0..=1000.0).contains(&self.tracking)
            && self.leading.is_finite()
            && (0.0..=5000.0).contains(&self.leading)
            && self.color_runs_are_valid()
            && self.font_runs_are_valid()
    }

    fn color_runs_are_valid(&self) -> bool {
        let Some(color_runs) = &self.color_runs else { return true };
        let mut end: isize = 0;
        for run in color_runs {
            if !(run.location >= end
                && run.length > 0
                && run.location <= isize::MAX - run.length
                && [run.red, run.green, run.blue]
                    .iter()
                    .all(|component| component.is_finite() && (0.0..=1.0).contains(component)))
            {
                return false;
            }
            end = run.location + run.length;
        }
        !color_runs.is_empty() && end <= self.content_units() as isize
    }

    fn font_runs_are_valid(&self) -> bool {
        let Some(font_runs) = &self.font_runs else { return true };
        let mut end: isize = 0;
        for run in font_runs {
            if !(run.location >= end
                && run.length > 0
                && run.location <= isize::MAX - run.length
                && !run.font_name.is_empty()
                && run.font_name.chars().count() <= 200
                && !run.font_name.contains(is_newline))
            {
                return false;
            }
            end = run.location + run.length;
        }
        !font_runs.is_empty() && end <= self.content_units() as isize
    }

    /// The color of the UTF-16 unit at `at`.
    pub fn color(&self, at: isize) -> PaletteColor {
        match self.run_covering(at) {
            Some(run) => PaletteColor::new(run.red, run.green, run.blue),
            None => self.base_color(),
        }
    }

    /// Paints `range` in `color`. An empty range, or one covering the whole text, recolors all of it.
    pub fn set_color(&mut self, color: PaletteColor, range: TextRange) {
        let (start, end) = self.clamped(range);
        if start == end || (start == 0 && end == self.content_units() as isize) {
            self.red = color.red;
            self.green = color.green;
            self.blue = color.blue;
            self.color_runs = None;
            return;
        }
        let mut colors = self.unit_colors();
        for index in start..end {
            colors[index as usize] = color;
        }
        self.set_unit_colors(colors);
    }

    /// The face of the UTF-16 unit at `at`.
    pub fn font_name(&self, at: isize) -> String {
        match self.font_runs.as_deref().unwrap_or(&[]).iter().find(|run| covers(run.location, run.length, at)) {
            Some(run) => run.font_name.clone(),
            None => self.font_name.clone(),
        }
    }

    /// The one face covering `range`, or nil when that range is empty or uses more than one.
    pub fn uniform_font_name(&self, range: TextRange) -> Option<String> {
        let (start, end) = self.clamped(range);
        if end <= start {
            return None;
        }
        let face = self.font_name(start);
        let mut index = start;
        for run in self.font_runs.as_deref().unwrap_or(&[]) {
            if !(run.location < end && run.location + run.length > index) {
                continue;
            }
            if run.location > index && self.font_name != face {
                return None;
            }
            if run.font_name != face {
                return None;
            }
            index = end.min(index.max(run.location + run.length));
        }
        if index < end && self.font_name != face {
            return None;
        }
        Some(face)
    }

    /// Sets the face of `range`. An empty range, or one covering the whole text, changes all of it.
    pub fn set_font(&mut self, name: &str, range: TextRange) {
        if name.is_empty() || name.chars().count() > 200 || name.contains(is_newline) {
            return;
        }
        let (start, end) = self.clamped(range);
        if start == end || (start == 0 && end == self.content_units() as isize) {
            self.font_name = name.to_string();
            self.font_runs = None;
            return;
        }
        let mut fonts = self.unit_fonts();
        for index in start..end {
            fonts[index as usize] = name.to_string();
        }
        self.set_unit_fonts(fonts);
    }

    /// Keeps each letter's color and face when `range` of `content` is replaced by `length` new UTF-16 units, which
    /// take them from the letter before, as typing does. Call before `content` changes.
    pub fn replace_characters(&mut self, range: TextRange, with_length: isize) {
        let (start, end) = self.clamped(range);
        if self.color_runs.is_some() {
            let mut colors = self.unit_colors();
            let inherited = if start > 0 {
                colors[(start - 1) as usize]
            } else if end > start {
                colors[start as usize]
            } else {
                colors.first().copied().unwrap_or_else(|| self.base_color())
            };
            colors.splice(
                start as usize..end as usize,
                std::iter::repeat(inherited).take(with_length.max(0) as usize),
            );
            self.set_unit_colors(colors);
        }
        if self.font_runs.is_some() {
            let mut fonts = self.unit_fonts();
            let inherited = if start > 0 {
                fonts[(start - 1) as usize].clone()
            } else if end > start {
                fonts[start as usize].clone()
            } else {
                fonts.first().cloned().unwrap_or_else(|| self.font_name.clone())
            };
            fonts.splice(
                start as usize..end as usize,
                std::iter::repeat(inherited).take(with_length.max(0) as usize),
            );
            self.set_unit_fonts(fonts);
        }
    }

    /// The style's own color, before any run overrides it.
    fn base_color(&self) -> PaletteColor {
        PaletteColor::new(self.red, self.green, self.blue)
    }

    /// The number of UTF-16 units in `content` — the offsets the runs are in.
    fn content_units(&self) -> usize {
        self.content.encode_utf16().count()
    }

    /// `range` clamped to `0..=content_units`, as `setColor` reads an `NSRange`.
    fn clamped(&self, range: TextRange) -> (isize, isize) {
        let count = self.content_units() as isize;
        let start = range.location.max(0).min(count);
        let end = (range.location + range.length).max(start).min(count);
        (start, end)
    }

    /// The run covering the UTF-16 unit at `at`, if any.
    fn run_covering(&self, at: isize) -> Option<&LayerTextColorRun> {
        self.color_runs.as_deref().unwrap_or(&[]).iter().find(|run| covers(run.location, run.length, at))
    }

    fn unit_colors(&self) -> Vec<PaletteColor> {
        let base = self.base_color();
        let count = self.content_units();
        let mut colors = vec![base; count];
        for run in self.color_runs.as_deref().unwrap_or(&[]) {
            let color = PaletteColor::new(run.red, run.green, run.blue);
            let mut index = run.location.max(0);
            let upper = (run.location + run.length).min(count as isize);
            while index < upper {
                colors[index as usize] = color;
                index += 1;
            }
        }
        colors
    }

    fn set_unit_colors(&mut self, colors: Vec<PaletteColor>) {
        let base = self.base_color();
        let mut runs: Vec<LayerTextColorRun> = Vec::new();
        for (index, color) in colors.iter().enumerate() {
            if *color == base {
                continue;
            }
            if let Some(last) = runs.last_mut() {
                if last.location + last.length == index as isize
                    && PaletteColor::new(last.red, last.green, last.blue) == *color
                {
                    last.length += 1;
                    continue;
                }
            }
            runs.push(LayerTextColorRun {
                location: index as isize,
                length: 1,
                red: color.red,
                green: color.green,
                blue: color.blue,
            });
        }
        self.color_runs = if runs.is_empty() { None } else { Some(runs) };
    }

    fn unit_fonts(&self) -> Vec<String> {
        let count = self.content_units();
        let mut fonts = vec![self.font_name.clone(); count];
        for run in self.font_runs.as_deref().unwrap_or(&[]) {
            let mut index = run.location.max(0);
            let upper = (run.location + run.length).min(count as isize);
            while index < upper {
                fonts[index as usize] = run.font_name.clone();
                index += 1;
            }
        }
        fonts
    }

    fn set_unit_fonts(&mut self, fonts: Vec<String>) {
        if let Some(first) = fonts.first() {
            if fonts.iter().all(|name| name == first) {
                self.font_name = first.clone();
                self.font_runs = None;
                return;
            }
        }
        let mut runs: Vec<LayerTextFontRun> = Vec::new();
        for (index, name) in fonts.iter().enumerate() {
            if *name == self.font_name {
                continue;
            }
            if let Some(last) = runs.last_mut() {
                if last.location + last.length == index as isize && last.font_name == *name {
                    last.length += 1;
                    continue;
                }
            }
            runs.push(LayerTextFontRun {
                location: index as isize,
                length: 1,
                font_name: name.clone(),
            });
        }
        self.font_runs = if runs.is_empty() { None } else { Some(runs) };
    }
}

/// Whether the UTF-16 unit at `at` is inside the run at `location` for `length` units.
fn covers(location: isize, length: isize, at: isize) -> bool {
    location <= at && at < location + length
}

/// `Character.isNewline`: the line separators a font name may not contain.
fn is_newline(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// Letters painted in a color other than the style's own, in UTF-16 offsets into `content`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerTextColorRun {
    pub location: isize,
    pub length: isize,
    pub red: CGFloat,
    pub green: CGFloat,
    pub blue: CGFloat,
}

/// Letters set in a face other than the style's own, in UTF-16 offsets into `content`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerTextFontRun {
    pub location: isize,
    pub length: isize,
    pub font_name: String,
}

/// The cached raster participates in the existing compositor. Pixel edits rasterize the layer;
/// transforms and masks keep the source text editable, just as shape layers keep their source.
#[derive(Clone, Debug)]
pub struct LayerText {
    pub style: LayerTextStyle,
    pub image: SharedImage,
}

impl PartialEq for LayerText {
    fn eq(&self, other: &Self) -> bool {
        self.style == other.style && std::sync::Arc::ptr_eq(&self.image, &other.image)
    }
}

impl LayerText {
    /// The text for a loaded project: a valid style and its pixels both have to be there.
    pub fn loaded(style: Option<LayerTextStyle>, image: Option<SharedImage>) -> Option<LayerText> {
        match (style, image) {
            (Some(style), Some(image)) if style.is_valid() => Some(LayerText { style, image }),
            _ => None,
        }
    }
}

impl crate::document::ImageLayer {
    /// The text this layer still is: nil once its pixels were edited some other way.
    pub fn live_text(&self) -> Option<LayerText> {
        let text = self.text.as_ref()?;
        let crate::imported_image::PixelImage::Rgba(image) = &self.asset.as_ref()?.image else {
            return None;
        };
        if std::sync::Arc::ptr_eq(&image, &text.image) {
            Some(text.clone())
        } else {
            None
        }
    }
}

/// Text being typed or edited on the canvas.
#[derive(Clone)]
pub struct TextDraft {
    pub id: Id,
    pub document_id: Id,
    pub layer_id: Option<Id>,
    pub origin: Point,
    pub transform: Option<LayerTransform>,
    pub style: LayerTextStyle,
    /// What is selected in the on-canvas editor, in UTF-16 offsets into `style.content`. Color and font apply to it.
    pub selection: TextRange,
}

impl TextDraft {
    /// A draft with its own identity and an empty selection, as `TextDraft(documentID:layerID:origin:style:)` hands
    /// back.
    pub fn new(
        document_id: Id,
        layer_id: Option<Id>,
        origin: Point,
        transform: Option<LayerTransform>,
        style: LayerTextStyle,
    ) -> Self {
        TextDraft {
            id: crate::new_id(),
            document_id,
            layer_id,
            origin,
            transform,
            style,
            selection: TextRange::EMPTY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Rgba8Image;
    use std::sync::Arc;

    #[test]
    fn alignment_raw_values_and_order_match_the_manifest() {
        assert_eq!(TextAlignment::ALL, [TextAlignment::Left, TextAlignment::Center, TextAlignment::Right]);
        for alignment in TextAlignment::ALL {
            assert_eq!(TextAlignment::from_raw(alignment.raw_value()), Some(alignment));
        }
        assert_eq!(TextAlignment::default(), TextAlignment::Left);
        assert_eq!(serde_json::to_string(&TextAlignment::Right).unwrap(), "\"Right\"");
        assert_eq!(serde_json::from_str::<TextAlignment>("\"Center\"").unwrap(), TextAlignment::Center);
    }

    #[test]
    fn defaults_match_the_type_tool() {
        let style = LayerTextStyle::default();
        assert_eq!(style.content, "Text");
        assert_eq!(style.font_name, "Helvetica");
        assert_eq!(style.font_size, 72.0);
        assert_eq!((style.red, style.green, style.blue), (0.0, 0.0, 0.0));
        assert_eq!(style.alignment, TextAlignment::Left);
        assert_eq!(style.tracking, 0.0);
        assert_eq!(style.leading, 0.0);
        assert_eq!(style.auto_leading(), 72.0 * 1.2);
        assert_eq!(style.line_height(), 72.0 * 1.2);
        assert_eq!(LayerTextStyle::PADDING, 12.0);
        assert_eq!(style.box_size, None);
        assert_eq!(style.color_runs, None);
        assert_eq!(style.font_runs, None);
        assert!(style.is_valid());

        let mut with_leading = style.clone();
        with_leading.leading = 20.0;
        assert_eq!(with_leading.line_height(), 20.0);
        // 0 is Auto, a negative leading falls back to Auto too.
        with_leading.leading = -5.0;
        assert_eq!(with_leading.line_height(), 72.0 * 1.2);
    }

    #[test]
    fn validity_keeps_the_tools_limits() {
        let mut style = LayerTextStyle::default();
        assert!(style.is_valid());

        style.font_size = f64::NAN;
        assert!(!style.is_valid());
        style.font_size = 72.0;

        style.box_size = Some(Size::new(0.0, 100.0));
        assert!(!style.is_valid());
        style.box_size = Some(Size::new(360.0, 160.0));
        assert!(style.is_valid());
        style.box_size = Some(Size::new(16.0, 16.0));
        assert!(style.is_valid());
        style.box_size = None;

        style.font_size = 2000.0;
        assert!(style.is_valid());
        style.font_size = 2001.0;
        assert!(!style.is_valid());
        style.font_size = 0.9;
        assert!(!style.is_valid());
        style.font_size = 72.0;

        style.tracking = 1000.0;
        assert!(style.is_valid());
        style.tracking = 1000.1;
        assert!(!style.is_valid());
        style.tracking = -100.0;
        assert!(style.is_valid());
        style.tracking = -100.1;
        assert!(!style.is_valid());
        style.tracking = 0.0;

        style.leading = 5000.0;
        assert!(style.is_valid());
        style.leading = 5000.5;
        assert!(!style.is_valid());
        style.leading = 0.0;

        style.red = 1.0001;
        assert!(!style.is_valid());
        style.red = 0.0;

        style.content = "a".repeat(100_001);
        assert!(!style.is_valid());
        style.content = "a".repeat(100_000);
        assert!(style.is_valid());
    }

    #[test]
    fn invalid_color_runs_are_rejected() {
        let mut style = LayerTextStyle::default();
        style.content = "Text".to_string();
        style.color_runs = Some(vec![LayerTextColorRun { location: 2, length: 3, red: 1.0, green: 0.0, blue: 0.0 }]);
        assert!(!style.is_valid());
        style.color_runs = Some(vec![
            LayerTextColorRun { location: 0, length: 2, red: 1.0, green: 0.0, blue: 0.0 },
            LayerTextColorRun { location: 1, length: 2, red: 0.0, green: 1.0, blue: 0.0 },
        ]);
        assert!(!style.is_valid());
        style.color_runs = Some(vec![LayerTextColorRun { location: 0, length: 1, red: 2.0, green: 0.0, blue: 0.0 }]);
        assert!(!style.is_valid());
        style.color_runs = Some(vec![]);
        assert!(!style.is_valid());
        style.color_runs = Some(vec![LayerTextColorRun { location: 0, length: 4, red: 1.0, green: 0.0, blue: 0.0 }]);
        assert!(style.is_valid());
    }

    #[test]
    fn invalid_font_runs_are_rejected() {
        let mut style = LayerTextStyle::default();
        style.content = "Text".to_string();
        style.font_runs = Some(vec![LayerTextFontRun { location: 0, length: 1, font_name: String::new() }]);
        assert!(!style.is_valid());
        style.font_runs = Some(vec![LayerTextFontRun { location: 0, length: 1, font_name: "A".repeat(201) }]);
        assert!(!style.is_valid());
        style.font_runs = Some(vec![LayerTextFontRun { location: 0, length: 1, font_name: "A\nB".to_string() }]);
        assert!(!style.is_valid());
        style.font_runs = Some(vec![LayerTextFontRun { location: 3, length: 2, font_name: "Courier".to_string() }]);
        assert!(!style.is_valid());
        style.font_runs = Some(vec![LayerTextFontRun { location: 0, length: 4, font_name: "Courier".to_string() }]);
        assert!(style.is_valid());
    }

    #[test]
    fn color_applies_to_selection_and_follows_edits() {
        let red = PaletteColor::new(1.0, 0.0, 0.0);
        let mut style = LayerTextStyle::default();
        style.content = "Hello world".to_string();
        style.set_color(red, TextRange::new(6, 5));
        assert_eq!(
            style.color_runs,
            Some(vec![LayerTextColorRun { location: 6, length: 5, red: 1.0, green: 0.0, blue: 0.0 }])
        );
        assert_eq!(style.color(5), PaletteColor::BLACK);
        assert_eq!(style.color(6), red);

        // Painting next to a run in the same color joins it.
        style.set_color(red, TextRange::new(5, 1));
        assert_eq!(style.color_runs.as_ref().unwrap().len(), 1);
        assert_eq!(style.color_runs.as_ref().unwrap()[0].location, 5);

        // Typed letters take the color of the one before them; deleted ones take their color away.
        style.replace_characters(TextRange::new(11, 0), 1);
        style.content.push('!');
        assert!(style.is_valid() && style.color(11) == red);
        style.replace_characters(TextRange::new(0, 2), 0);
        style.content.drain(..2);
        assert!(style.is_valid());
        let runs = style.color_runs.as_ref().unwrap();
        assert_eq!(runs[0].location, 3);
        assert_eq!(runs[0].length, 7);

        // No selection, or all of it, recolors the whole text.
        style.set_color(red, TextRange::new(4, 0));
        assert_eq!(style.color_runs, None);
        assert_eq!(style.red, 1.0);
    }

    #[test]
    fn font_applies_to_the_selection_only() {
        let mut style = LayerTextStyle::default();
        style.content = "Hello".to_string();
        style.font_name = "Helvetica".to_string();
        style.set_font("Courier", TextRange::new(0, 2));
        assert_eq!(style.font_name, "Helvetica");
        assert_eq!(style.font_runs, Some(vec![LayerTextFontRun { location: 0, length: 2, font_name: "Courier".to_string() }]));
        assert_eq!(style.font_name(0), "Courier");
        assert_eq!(style.font_name(2), "Helvetica");
        assert_eq!(style.uniform_font_name(TextRange::new(0, 2)), Some("Courier".to_string()));
        assert_eq!(style.uniform_font_name(TextRange::new(0, 5)), None);
        style.set_font("Courier", TextRange::new(0, 5));
        assert_eq!(style.font_runs, None);
        assert_eq!(style.font_name, "Courier");
        style.set_font("Helvetica", TextRange::new(0, 2));
        assert_eq!(style.font_runs, Some(vec![LayerTextFontRun { location: 0, length: 2, font_name: "Helvetica".to_string() }]));
        style.set_font("Courier", TextRange::new(0, 0));
        assert_eq!(style.font_runs, None);
        assert_eq!(style.font_name, "Courier");
        style.set_font("Helvetica", TextRange::new(1, 3));
        style.replace_characters(TextRange::new(5, 0), 1);
        style.content.push('!');
        assert!(style.is_valid() && style.font_name(5) == "Courier");
    }

    #[test]
    fn style_serializes_the_manifest_keys() {
        let mut style = LayerTextStyle::default();
        style.content = "Hello world".to_string();
        style.box_size = Some(Size::new(200.0, 120.0));
        style.color_runs = Some(vec![LayerTextColorRun { location: 6, length: 5, red: 1.0, green: 0.0, blue: 0.0 }]);
        style.font_runs = Some(vec![LayerTextFontRun { location: 0, length: 2, font_name: "Courier".to_string() }]);
        let value = serde_json::to_value(&style).unwrap();
        assert_eq!(value["content"], "Hello world");
        assert_eq!(value["fontName"], "Helvetica");
        assert_eq!(value["fontSize"], 72.0);
        assert_eq!(value["red"], 0.0);
        assert_eq!(value["green"], 0.0);
        assert_eq!(value["blue"], 0.0);
        assert_eq!(value["alignment"], "Left");
        assert_eq!(value["tracking"], 0.0);
        assert_eq!(value["leading"], 0.0);
        // Paragraph bounds write the CoreGraphics array form.
        assert_eq!(value["boxSize"], serde_json::json!([200.0, 120.0]));
        assert_eq!(
            value["colorRuns"],
            serde_json::json!([{ "location": 6, "length": 5, "red": 1.0, "green": 0.0, "blue": 0.0 }])
        );
        assert_eq!(value["fontRuns"], serde_json::json!([{ "location": 0, "length": 2, "fontName": "Courier" }]));
        assert_eq!(serde_json::from_value::<LayerTextStyle>(value).unwrap(), style);

        // A whole default style omits the optional keys entirely.
        let default = serde_json::to_value(LayerTextStyle::default()).unwrap();
        let object = default.as_object().unwrap();
        for key in ["boxSize", "colorRuns", "fontRuns"] {
            assert!(!object.contains_key(key), "{key} should be omitted");
        }
    }

    #[test]
    fn older_text_metadata_without_the_new_keys_still_decodes() {
        let minimal = serde_json::json!({
            "content": "T",
            "fontName": "Arial",
            "fontSize": 24.0,
            "red": 0.0,
            "green": 0.0,
            "blue": 0.0,
            "alignment": "Center",
            "tracking": 1.0,
            "leading": 30.0
        });
        let decoded: LayerTextStyle = serde_json::from_value(minimal).unwrap();
        assert_eq!(decoded.box_size, None);
        assert_eq!(decoded.color_runs, None);
        assert_eq!(decoded.font_runs, None);
        assert_eq!(decoded.alignment, TextAlignment::Center);
        assert_eq!(decoded.font_name, "Arial");
        assert_eq!(decoded.leading, 30.0);
    }

    #[test]
    fn a_loaded_text_needs_a_valid_style_and_its_pixels() {
        let image = Arc::new(Rgba8Image::new(2, 2));
        assert!(LayerText::loaded(Some(LayerTextStyle::default()), Some(image.clone())).is_some());
        let text = LayerText::loaded(Some(LayerTextStyle::default()), Some(image.clone())).unwrap();
        assert_eq!(text, LayerText { style: LayerTextStyle::default(), image: image.clone() });
        let mut invalid = LayerTextStyle::default();
        invalid.font_size = f64::NAN;
        assert!(LayerText::loaded(Some(invalid), Some(image.clone())).is_none());
        assert!(LayerText::loaded(None, Some(image)).is_none());
        assert!(LayerText::loaded(Some(LayerTextStyle::default()), None).is_none());
    }

    #[test]
    fn a_new_draft_starts_empty_at_the_click() {
        let id = crate::new_id();
        let draft = TextDraft::new(id, None, Point::new(30.0, 40.0), None, LayerTextStyle::default());
        assert_eq!(draft.document_id, id);
        assert_eq!(draft.layer_id, None);
        assert_eq!(draft.origin, Point::new(30.0, 40.0));
        assert_eq!(draft.selection, TextRange::EMPTY);
        assert_ne!(draft.id, crate::Id::nil());
    }
}
