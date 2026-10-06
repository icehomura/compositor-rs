//! Reads a Photoshop 6 type layer (`TySh`) into the editor's text model, ported from
//! `IO/PSD/PSDText.swift`.
//!
//! Adobe’s *Photoshop File Formats Specification* (2019), Type Tool Object Setting:
//! version, a 2×3 transform, a text descriptor, and a warp descriptor.
//! The engine dictionary inside `EngineData` supplies the font, size, color,
//! tracking, leading and alignment. Anything this model cannot represent
//! (vertical text, shear, uneven scale) stays a raster.

use std::sync::Arc;

use compositor_core::buffer::SharedImage;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::layer_text::{LayerTextStyle, TextAlignment};
use compositor_core::layer_transform::{LayerTransform, LayerSampling};
use compositor_pixels::text as pixel_text;
use rustc_hash::FxHashMap;

/// `PSDText.Source`: the parsed text and how its raster is placed on the document.
#[derive(Clone, Debug)]
pub struct Source {
    pub style: LayerTextStyle,
    pub notes: Vec<String>,
    /// Document point that `imageAnchor` should land on.
    pub document_anchor: Point,
    pub rotation: f64,
    pub flip_y: bool,
    /// The anchor is the paragraph frame's top-left. Otherwise it is the point-text baseline.
    pub anchor_is_frame: bool,
}

pub const RASTERIZED_NOTE: &str = "Editable Photoshop text becomes pixels and can’t be retyped.";
pub const FIRST_STYLE_NOTE: &str = "Only the first text style was kept.";
pub const WARP_NOTE: &str = "The Photoshop text warp was omitted.";
pub const FAUX_NOTE: &str = "Faux bold or faux italic was omitted.";
pub const JUSTIFY_NOTE: &str = "Full justification was imported as left alignment.";

/// The note a font the machine does not have produces — `NSFont(name:size:) == nil` in the Swift.
pub fn missing_font_note(name: &str) -> Option<String> {
    if pixel_text::font_is_installed(name) {
        return None;
    }
    Some(format!(
        "The font “{}” isn’t installed, so the text was drawn with the system font.",
        name
    ))
}

pub fn parse(extra: &FxHashMap<String, &[u8]>) -> Option<Source> {
    let data = extra.get("TySh").or_else(|| extra.get("tySh"))?;
    if data.len() > 8_000_000 {
        return None;
    }
    let mut reader = Reader { data, offset: 0 };
    if reader.u16()? != 1 {
        return None;
    }
    let xx = reader.f64()?;
    let xy = reader.f64()?;
    let yx = reader.f64()?;
    let yy = reader.f64()?;
    let tx = reader.f64()?;
    let ty = reader.f64()?;
    if ![xx, xy, yx, yy, tx, ty].iter().all(|value| value.is_finite()) {
        return None;
    }
    if reader.u16()? != 50 {
        return None;
    }
    let text = reader.descriptor(true)?;
    if text_enumeration(&text, "Ornt").as_deref() == Some("Vrtc") {
        return None;
    }
    let placed = placement(xx, xy, yx, yy, tx, ty)?;

    let mut notes: Vec<String> = Vec::new();
    if reader.remaining() >= 2 && reader.u16()? == 1 {
        if let Some(warp) = reader.descriptor(true) {
            if let Some(style) = text_enumeration(&warp, "warpStyle") {
                if style != "warpNone" && style != "none" {
                    notes.push(WARP_NOTE.to_string());
                }
            }
        }
    }

    let engine = text_data(&text, "EngineData").and_then(engine_value);
    let content = cleaned(text_text(&text, "Txt ").or_else(|| text_text(&text, "Txt")).as_deref())
        .or_else(|| engine.as_ref().and_then(|engine| cleaned(engine_string(walk(engine, &["EngineDict", "Editor", "Text"])).as_deref())));
    let content = match content {
        Some(content) if !content.is_empty() && content.encode_utf16().count() <= 100_000 => content,
        _ => return None,
    };

    let mut style = LayerTextStyle::default();
    style.content = content;
    if let Some(engine) = &engine {
        apply_style(&mut style, engine, placed.pixel_scale, &mut notes);
    } else {
        style.font_size = (12.0 * placed.pixel_scale).clamp(1.0, 2000.0);
    }
    if !style.font_size.is_finite() || style.font_size <= 0.0 {
        return None;
    }

    let mut anchor = Point::new(placed.tx, placed.ty);
    let mut anchor_is_frame = false;
    if let (Some(bounds), Some(glyphs)) = (descriptor_rect(&text, "bounds"), descriptor_rect(&text, "boundingBox")) {
        if bounds.width() > glyphs.width() + 4.0
            && bounds.height() > glyphs.height() + 4.0
            && bounds.width() > 1.0
            && bounds.height() > 1.0
        {
            let frame = Size::new(bounds.width() * placed.pixel_scale, bounds.height() * placed.pixel_scale);
            let pad = LayerTextStyle::PADDING;
            let mut boxed = style.clone();
            boxed.box_size = Some(Size::new(frame.width + pad * 2.0, frame.height + pad * 2.0));
            // A paragraph frame the model cannot store is dropped entirely: importing as point
            // text would lose the wrap without saying so. Vertical text already falls back the same way.
            if !boxed.box_is_valid() {
                return None;
            }
            style = boxed;
            anchor = placed.map(Point::new(bounds.min_x(), bounds.min_y()));
            anchor_is_frame = true;
        }
    }
    if !style.is_valid() {
        return None;
    }
    Some(Source {
        style,
        notes,
        document_anchor: anchor,
        rotation: placed.rotation,
        flip_y: placed.flip_y,
        anchor_is_frame,
    })
}

/// Rasterizes the parsed text and places it: `PSDText.render(_:)`.
pub fn render(source: &Source) -> Result<(SharedImage, LayerTransform), pixel_text::TextError> {
    let image = pixel_text::text_image(&source.style)?;
    let size = Size::new(image.width() as f64, image.height() as f64);
    let anchor = if source.anchor_is_frame {
        Point::new(LayerTextStyle::PADDING, LayerTextStyle::PADDING)
    } else {
        Point::new(horizontal_anchor(&source.style, size.width), pixel_text::first_baseline(&source.style))
    };
    let transform = layer_transform(size, anchor, source.document_anchor, source.rotation, source.flip_y);
    if !transform.is_valid() {
        return Err(pixel_text::TextError::TooLarge);
    }
    Ok((Arc::new(image), transform))
}

struct Placement {
    pixel_scale: f64,
    rotation: f64,
    flip_y: bool,
    tx: f64,
    ty: f64,
    exx: f64,
    eyx: f64,
    exy: f64,
    eyy: f64,
}

impl Placement {
    fn map(&self, point: Point) -> Point {
        Point::new(
            self.exx * point.x + self.exy * point.y + self.tx,
            self.eyx * point.x + self.eyy * point.y + self.ty,
        )
    }
}

/// Uniform scale, rotation and an optional vertical flip. Shear and uneven scale return nil.
/// Engine sizes are already in text-space units that the matrix maps into document pixels;
/// document PPI is print metadata and must not multiply that product again.
fn placement(xx: f64, xy: f64, yx: f64, yy: f64, tx: f64, ty: f64) -> Option<Placement> {
    let scale_x = xx.hypot(yx);
    if scale_x <= 1e-6 {
        return None;
    }
    let cos_r = xx / scale_x;
    let sin_r = yx / scale_x;
    let local_x = cos_r * xy + sin_r * yy;
    let local_y = -sin_r * xy + cos_r * yy;
    let scale_y = local_y.abs();
    if scale_y <= 1e-6 {
        return None;
    }
    let largest = scale_x.max(scale_y);
    if local_x.abs() > 0.02 * largest || (scale_x - scale_y).abs() > 0.02 * largest {
        return None;
    }
    let pixel_scale = scale_x;
    if !pixel_scale.is_finite() || pixel_scale <= 0.0 {
        return None;
    }
    let y_sign = if local_y < 0.0 { -1.0 } else { 1.0 };
    let exx = cos_r * pixel_scale;
    let eyx = sin_r * pixel_scale;
    let exy = -sin_r * pixel_scale * y_sign;
    let eyy = cos_r * pixel_scale * y_sign;
    Some(Placement {
        pixel_scale,
        rotation: sin_r.atan2(cos_r) * 180.0 / std::f64::consts::PI,
        flip_y: local_y < 0.0,
        tx,
        ty,
        exx,
        eyx,
        exy,
        eyy,
    })
}

fn apply_style(style: &mut LayerTextStyle, engine: &Engine, pixel_scale: f64, notes: &mut Vec<String>) {
    let runs = engine_array(walk(engine, &["EngineDict", "StyleRun", "RunArray"]));
    let first = runs.first().unwrap_or(engine);
    let sheet = walk(first, &["StyleSheet", "StyleSheetData"])
        .or_else(|| walk(engine, &["EngineDict", "StyleRun", "RunArray"]));
    let data = sheet.unwrap_or(first);
    let points = engine_number(walk(data, &["FontSize"])).unwrap_or(12.0);
    if !points.is_finite() || points <= 0.0 {
        return;
    }
    style.font_size = (points * pixel_scale).clamp(1.0, 2000.0);
    let fonts = engine_array(walk(engine, &["ResourceDict", "FontSet"]));
    let index = engine_number(walk(data, &["Font"])).unwrap_or(0.0).round();
    if index >= 0.0 {
        if let Some(font) = fonts.get(index as usize) {
            if let Some(name) = engine_string(walk(font, &["Name"])) {
                if !name.is_empty() {
                    style.font_name = name;
                }
            }
        }
    }
    let values = engine_array(walk(data, &["FillColor", "Values"]));
    if !values.is_empty() {
        let channels: Vec<f64> = values.iter().filter_map(engine_number_ref).collect();
        let rgb = color(&channels);
        style.red = rgb.0;
        style.green = rgb.1;
        style.blue = rgb.2;
    }
    let tracking = engine_number(walk(data, &["Tracking"])).unwrap_or(0.0);
    if tracking.is_finite() {
        style.tracking = (tracking * style.font_size / 1000.0).clamp(-100.0, 1000.0);
    }
    let auto = engine_bool(walk(data, &["AutoLeading"])).unwrap_or(true);
    if !auto {
        if let Some(leading) = engine_number(walk(data, &["Leading"])) {
            if leading.is_finite() && leading > 0.0 {
                style.leading = (leading * pixel_scale).clamp(0.0, 5000.0);
            }
        }
    }
    if engine_bool(walk(data, &["FauxBold"])) == Some(true) || engine_bool(walk(data, &["FauxItalic"])) == Some(true) {
        notes.push(FAUX_NOTE.to_string());
    }
    if runs.len() > 1 && runs[1..].iter().any(|run| signature(run) != signature(first)) {
        notes.push(FIRST_STYLE_NOTE.to_string());
    }
    let paragraphs = engine_array(walk(engine, &["EngineDict", "ParagraphRun", "RunArray"]));
    let justification = engine_number(walk(
        paragraphs.first().unwrap_or(engine),
        &["ParagraphSheet", "Properties", "Justification"],
    ));
    match (justification.unwrap_or(0.0)).round() as i64 {
        1 => style.alignment = TextAlignment::Right,
        2 => style.alignment = TextAlignment::Center,
        0 => style.alignment = TextAlignment::Left,
        _ => {
            style.alignment = TextAlignment::Left;
            notes.push(JUSTIFY_NOTE.to_string());
        }
    }
}

#[derive(PartialEq)]
struct Signature {
    font: f64,
    size: f64,
    tracking: f64,
    leading: f64,
    auto_leading: bool,
    horizontal_scale: f64,
    vertical_scale: f64,
    bold: bool,
    italic: bool,
    red: f64,
    green: f64,
    blue: f64,
}

fn signature(run: &Engine) -> Signature {
    let data = walk(run, &["StyleSheet", "StyleSheetData"]).unwrap_or(run);
    let mut sign = Signature {
        font: engine_number(walk(data, &["Font"])).unwrap_or(0.0),
        size: engine_number(walk(data, &["FontSize"])).unwrap_or(0.0),
        tracking: engine_number(walk(data, &["Tracking"])).unwrap_or(0.0),
        leading: engine_number(walk(data, &["Leading"])).unwrap_or(0.0),
        auto_leading: engine_bool(walk(data, &["AutoLeading"])).unwrap_or(true),
        horizontal_scale: engine_number(walk(data, &["HorizontalScale"])).unwrap_or(1.0),
        vertical_scale: engine_number(walk(data, &["VerticalScale"])).unwrap_or(1.0),
        bold: engine_bool(walk(data, &["FauxBold"])).unwrap_or(false),
        italic: engine_bool(walk(data, &["FauxItalic"])).unwrap_or(false),
        red: 0.0,
        green: 0.0,
        blue: 0.0,
    };
    let channels: Vec<f64> = engine_array(walk(data, &["FillColor", "Values"]))
        .iter()
        .filter_map(engine_number_ref)
        .collect();
    let rgb = color(&channels);
    sign.red = rgb.0;
    sign.green = rgb.1;
    sign.blue = rgb.2;
    sign
}

fn color(values: &[f64]) -> (f64, f64, f64) {
    fn unit(value: f64) -> f64 {
        if value > 1.0 {
            value.clamp(0.0, 255.0) / 255.0
        } else {
            value.clamp(0.0, 1.0)
        }
    }
    if values.len() >= 4 {
        return (unit(values[1]), unit(values[2]), unit(values[3]));
    }
    if values.len() == 3 {
        return (unit(values[0]), unit(values[1]), unit(values[2]));
    }
    if let Some(gray) = values.first() {
        let gray = unit(*gray);
        return (gray, gray, gray);
    }
    (0.0, 0.0, 0.0)
}

fn horizontal_anchor(style: &LayerTextStyle, width: f64) -> f64 {
    match style.alignment {
        TextAlignment::Left => LayerTextStyle::PADDING,
        TextAlignment::Center => width / 2.0,
        TextAlignment::Right => width - LayerTextStyle::PADDING,
    }
}

/// Matches `BrushRaster.pixelToDocument`: flip, then clockwise rotation about the center.
fn layer_transform(size: Size, image_anchor: Point, document_anchor: Point, rotation: f64, flip_y: bool) -> LayerTransform {
    let mut local = Point::new(image_anchor.x - size.width / 2.0, image_anchor.y - size.height / 2.0);
    if flip_y {
        local.y = -local.y;
    }
    let radians = rotation * std::f64::consts::PI / 180.0;
    let rotated = Point::new(
        local.x * radians.cos() - local.y * radians.sin(),
        local.x * radians.sin() + local.y * radians.cos(),
    );
    let center = Point::new(document_anchor.x - rotated.x, document_anchor.y - rotated.y);
    LayerTransform {
        origin: Point::new(center.x - size.width / 2.0, center.y - size.height / 2.0),
        size,
        rotation,
        flip_x: false,
        flip_y,
        sampling: LayerSampling::default(),
    }
}

fn cleaned(text: Option<&str>) -> Option<String> {
    let mut text = text?.to_string();
    while text.starts_with('\u{feff}') || text.starts_with('\0') {
        text.remove(0);
    }
    while text.ends_with('\0') {
        text.pop();
    }
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Photoshop's text-engine dictionary: a small PostScript-like subset (`<< >>`, arrays, names, numbers, strings).
#[derive(Clone, Debug)]
enum Engine {
    Number(f64),
    Bool(bool),
    String(String),
    Dict(FxHashMap<String, Engine>),
    Array(Vec<Engine>),
}

fn walk<'a>(value: &'a Engine, keys: &[&str]) -> Option<&'a Engine> {
    let mut current = value;
    for key in keys {
        match current {
            Engine::Dict(items) => current = items.get(*key)?,
            _ => return None,
        }
    }
    Some(current)
}

fn engine_number(value: Option<&Engine>) -> Option<f64> {
    match value {
        Some(Engine::Number(number)) => Some(*number),
        _ => None,
    }
}

fn engine_number_ref(value: &Engine) -> Option<f64> {
    match value {
        Engine::Number(number) => Some(*number),
        _ => None,
    }
}

fn engine_bool(value: Option<&Engine>) -> Option<bool> {
    match value {
        Some(Engine::Bool(flag)) => Some(*flag),
        _ => None,
    }
}

fn engine_string(value: Option<&Engine>) -> Option<String> {
    match value {
        Some(Engine::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn engine_array(value: Option<&Engine>) -> Vec<Engine> {
    match value {
        Some(Engine::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

fn engine_value(data: &[u8]) -> Option<Engine> {
    if let Some(dict) = dictionary(data, 0) {
        return Some(dict);
    }
    let start = data.windows(2).position(|pair| pair == b"<<")?;
    if start == 0 {
        return None;
    }
    dictionary(data, start)
}

fn dictionary(data: &[u8], at: usize) -> Option<Engine> {
    let mut cursor = EngineCursor { bytes: data, index: at };
    match cursor.parse_value()? {
        Engine::Dict(items) => Some(Engine::Dict(items)),
        _ => None,
    }
}

struct EngineCursor<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl EngineCursor<'_> {
    fn parse_value(&mut self) -> Option<Engine> {
        self.skip_whitespace();
        let byte = self.peek()?;
        if byte == b'<' {
            if self.peek_ahead(1) == Some(b'<') {
                return self.parse_dictionary();
            }
            return self.parse_hex();
        }
        if byte == b'[' {
            return self.parse_array();
        }
        if byte == b'(' {
            return self.parse_string();
        }
        if byte == b'/' {
            self.index += 1;
            return Some(Engine::String(self.read_token()));
        }
        if byte == b'-' || byte == b'+' || byte == b'.' || byte.is_ascii_digit() {
            return self.parse_number().map(Engine::Number);
        }
        if self.take_word("true") {
            return Some(Engine::Bool(true));
        }
        if self.take_word("false") {
            return Some(Engine::Bool(false));
        }
        if self.take_word("null") {
            return Some(Engine::String(String::new()));
        }
        None
    }

    fn parse_dictionary(&mut self) -> Option<Engine> {
        if !self.take("<<") {
            return None;
        }
        let mut items: FxHashMap<String, Engine> = FxHashMap::default();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some(b'>') => break,
                Some(b'/') => {}
                _ => return None,
            }
            self.index += 1;
            let key = self.read_token();
            let value = self.parse_value()?;
            items.insert(key, value);
        }
        if !self.take(">>") {
            return None;
        }
        Some(Engine::Dict(items))
    }

    fn parse_array(&mut self) -> Option<Engine> {
        if !self.take("[") {
            return None;
        }
        let mut items: Vec<Engine> = Vec::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some(b']') => break,
                _ => {}
            }
            items.push(self.parse_value()?);
        }
        if !self.take("]") {
            return None;
        }
        Some(Engine::Array(items))
    }

    fn parse_number(&mut self) -> Option<f64> {
        let start = self.index;
        if self.peek() == Some(b'+') || self.peek() == Some(b'-') {
            self.index += 1;
        }
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.index += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            while let Some(byte) = self.peek() {
                if byte.is_ascii_digit() {
                    self.index += 1;
                } else {
                    break;
                }
            }
        }
        if self.peek() == Some(b'e') || self.peek() == Some(b'E') {
            self.index += 1;
            if self.peek() == Some(b'+') || self.peek() == Some(b'-') {
                self.index += 1;
            }
            while let Some(byte) = self.peek() {
                if byte.is_ascii_digit() {
                    self.index += 1;
                } else {
                    break;
                }
            }
        }
        if self.index <= start {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.index]).ok()?.parse().ok()
    }

    fn parse_string(&mut self) -> Option<Engine> {
        if !self.take("(") {
            return None;
        }
        let mut raw: Vec<u8> = Vec::new();
        while let Some(byte) = self.peek() {
            self.index += 1;
            if byte == b')' {
                break;
            }
            if byte == b'\\' {
                let escaped = self.peek()?;
                self.index += 1;
                if escaped == b'n' {
                    raw.push(0x0A);
                } else if escaped == b'r' {
                    raw.push(0x0D);
                } else if escaped == b't' {
                    raw.push(0x09);
                } else if (b'0'..=b'7').contains(&escaped) {
                    let mut value = (escaped - b'0') as u32;
                    for _ in 0..2 {
                        match self.peek() {
                            Some(digit) if (b'0'..=b'7').contains(&digit) => {
                                self.index += 1;
                                value = value * 8 + (digit - b'0') as u32;
                            }
                            _ => break,
                        }
                    }
                    raw.push((value & 0xFF) as u8);
                } else if escaped != b'\n' && escaped != b'\r' {
                    raw.push(escaped);
                }
            } else {
                raw.push(byte);
            }
        }
        Some(Engine::String(decode_engine(&raw)))
    }

    fn parse_hex(&mut self) -> Option<Engine> {
        if !self.take("<") {
            return None;
        }
        let mut nibbles: Vec<u8> = Vec::new();
        while let Some(byte) = self.peek() {
            if byte == b'>' {
                break;
            }
            self.index += 1;
            if let Some(nibble) = hex(byte) {
                nibbles.push(nibble);
            }
        }
        if !self.take(">") {
            return None;
        }
        let mut raw: Vec<u8> = Vec::new();
        let mut index = 0;
        while index + 1 < nibbles.len() {
            raw.push(nibbles[index] << 4 | nibbles[index + 1]);
            index += 2;
        }
        Some(Engine::String(decode_engine(&raw)))
    }

    fn read_token(&mut self) -> String {
        let start = self.index;
        while let Some(byte) = self.peek() {
            if is_delimiter(byte) {
                break;
            }
            self.index += 1;
        }
        ascii(&self.bytes[start..self.index])
    }

    fn take_word(&mut self, word: &str) -> bool {
        let encoded = word.as_bytes();
        if self.index + encoded.len() > self.bytes.len() || &self.bytes[self.index..self.index + encoded.len()] != encoded {
            return false;
        }
        let after = self.index + encoded.len();
        if after < self.bytes.len() && !is_delimiter(self.bytes[after]) {
            return false;
        }
        self.index = after;
        true
    }

    fn take(&mut self, token: &str) -> bool {
        let encoded = token.as_bytes();
        if self.index + encoded.len() > self.bytes.len() || &self.bytes[self.index..self.index + encoded.len()] != encoded {
            return false;
        }
        self.index += encoded.len();
        true
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if byte == b'%' {
                while let Some(next) = self.peek() {
                    if next == b'\n' || next == b'\r' {
                        break;
                    }
                    self.index += 1;
                }
            } else if byte <= 0x20 {
                self.index += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn peek_ahead(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.index + ahead).copied()
    }
}

fn is_delimiter(byte: u8) -> bool {
    byte <= 0x20
        || byte == b'/'
        || byte == b'<'
        || byte == b'>'
        || byte == b'['
        || byte == b']'
        || byte == b'('
        || byte == b')'
}

fn hex(byte: u8) -> Option<u8> {
    if byte.is_ascii_digit() {
        Some(byte - b'0')
    } else if (b'a'..=b'f').contains(&byte) {
        Some(byte - b'a' + 10)
    } else if (b'A'..=b'F').contains(&byte) {
        Some(byte - b'A' + 10)
    } else {
        None
    }
}

/// The ASCII decoding the Swift `String(bytes:encoding:.ascii) ?? ""` did.
fn ascii(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::new()
    }
}

fn decode_engine(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let payload = &raw[2..];
        if payload.len() % 2 != 0 {
            return String::new();
        }
        let units: Vec<u16> = payload.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
        return String::from_utf16(&units).unwrap_or_default();
    }
    raw.iter().map(|byte| *byte as char).collect()
}

/// The descriptor values a `TySh` block holds.
#[derive(Clone, Debug)]
pub enum DescriptorValue {
    Text(String),
    Number(f64),
    Enumeration(String),
    Data(Vec<u8>),
    Descriptor(Descriptor),
    List(Vec<DescriptorValue>),
}

pub type Descriptor = FxHashMap<String, DescriptorValue>;

fn text_text(descriptor: &Descriptor, key: &str) -> Option<String> {
    match descriptor.get(key) {
        Some(DescriptorValue::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn text_enumeration(descriptor: &Descriptor, key: &str) -> Option<String> {
    match descriptor.get(key) {
        Some(DescriptorValue::Enumeration(value)) => Some(value.clone()),
        _ => None,
    }
}

fn text_data<'a>(descriptor: &'a Descriptor, key: &str) -> Option<&'a [u8]> {
    match descriptor.get(key) {
        Some(DescriptorValue::Data(data)) => Some(data),
        _ => None,
    }
}

fn descriptor_rect(descriptor: &Descriptor, key: &str) -> Option<Rect> {
    let DescriptorValue::Descriptor(items) = descriptor.get(key)? else {
        return None;
    };
    fn side(items: &Descriptor, name: &str) -> Option<f64> {
        let value = items.get(name).or_else(|| items.get(name.trim()));
        match value {
            Some(DescriptorValue::Number(number)) => Some(*number),
            _ => None,
        }
    }
    let left = side(items, "Left")?;
    let top = side(items, "Top ")?;
    let right = side(items, "Rght")?;
    let bottom = side(items, "Btom")?;
    if ![left, top, right, bottom].iter().all(|value| value.is_finite()) {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

/// Descriptor walker from the same specification (class and keys are length-prefixed, or 4 bytes when the length is 0).
struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    fn descriptor(&mut self, versioned: bool) -> Option<Descriptor> {
        if versioned && self.u32()? != 16 {
            return None;
        }
        self.unicode()?;
        self.identifier()?;
        let count = self.u32()?;
        if count > 10_000 {
            return None;
        }
        let mut items: Descriptor = FxHashMap::default();
        for _ in 0..count as usize {
            let key = self.identifier()?;
            let value_type = self.four_cc()?;
            let value = self.value(&value_type)?;
            items.insert(key, value);
        }
        Some(items)
    }

    fn value(&mut self, value_type: &str) -> Option<DescriptorValue> {
        match value_type {
            "doub" => Some(DescriptorValue::Number(self.f64()?)),
            "UntF" => {
                self.four_cc()?;
                Some(DescriptorValue::Number(self.f64()?))
            }
            "long" => Some(DescriptorValue::Number(self.i32()? as f64)),
            "comp" => {
                let raw = self.bytes(8)?;
                let bits = raw.iter().fold(0u64, |value, byte| (value << 8) | *byte as u64);
                Some(DescriptorValue::Number(i64::from_be_bytes(bits.to_be_bytes()) as f64))
            }
            "bool" => {
                self.u8()?;
                Some(DescriptorValue::Number(0.0))
            }
            "TEXT" => Some(DescriptorValue::Text(self.unicode()?)),
            "enum" => {
                self.identifier()?;
                Some(DescriptorValue::Enumeration(self.identifier()?))
            }
            "tdta" => {
                let length = self.u32()?;
                if length > 8_000_000 {
                    return None;
                }
                Some(DescriptorValue::Data(self.bytes(length as usize)?.to_vec()))
            }
            "Objc" | "GlbO" => Some(DescriptorValue::Descriptor(self.descriptor(false)?)),
            "VlLs" => {
                let count = self.u32()?;
                if count > 10_000 {
                    return None;
                }
                let mut items: Vec<DescriptorValue> = Vec::with_capacity(count as usize);
                for _ in 0..count as usize {
                    let item_type = self.four_cc()?;
                    items.push(self.value(&item_type)?);
                }
                Some(DescriptorValue::List(items))
            }
            "alis" => {
                let length = self.u32()?;
                if length > 8_000_000 {
                    return None;
                }
                self.bytes(length as usize)?;
                Some(DescriptorValue::Number(0.0))
            }
            "obj " => {
                if self.reference() {
                    Some(DescriptorValue::Number(0.0))
                } else {
                    None
                }
            }
            "type" | "GlbC" => {
                self.unicode()?;
                self.identifier()?;
                Some(DescriptorValue::Number(0.0))
            }
            _ => None,
        }
    }

    /// Skips a descriptor reference so a later `EngineData` item can still be read.
    fn reference(&mut self) -> bool {
        let Some(count) = self.u32() else { return false };
        if count > 10_000 {
            return false;
        }
        for _ in 0..count as usize {
            let Some(form) = self.four_cc() else { return false };
            let ok = match form.as_str() {
                "prop" => self.unicode().is_some() && self.identifier().is_some() && self.identifier().is_some(),
                "Clss" => self.unicode().is_some() && self.identifier().is_some(),
                "Enmr" => {
                    self.unicode().is_some()
                        && self.identifier().is_some()
                        && self.identifier().is_some()
                        && self.identifier().is_some()
                }
                "rele" => self.unicode().is_some() && self.identifier().is_some() && self.i32().is_some(),
                "Idnt" | "indx" => self.i32().is_some(),
                "name" => self.unicode().is_some(),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        true
    }

    fn unicode(&mut self) -> Option<String> {
        let count = self.u32()?;
        if count > 1_000_000 {
            return None;
        }
        let raw = self.bytes(count as usize * 2)?;
        if raw.is_empty() {
            return Some(String::new());
        }
        let units: Vec<u16> = raw.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
        String::from_utf16(&units).ok()
    }

    fn identifier(&mut self) -> Option<String> {
        let length = self.u32()?;
        if length == 0 {
            return self.four_cc();
        }
        if length > 10_000 {
            return None;
        }
        let raw = self.bytes(length as usize)?;
        if raw.is_ascii() {
            String::from_utf8(raw.to_vec()).ok()
        } else {
            None
        }
    }

    fn four_cc(&mut self) -> Option<String> {
        let raw = self.bytes(4)?;
        if raw.is_ascii() {
            String::from_utf8(raw.to_vec()).ok()
        } else {
            None
        }
    }

    fn bytes(&mut self, count: usize) -> Option<&[u8]> {
        if count > self.remaining() {
            return None;
        }
        let slice = &self.data[self.offset..self.offset + count];
        self.offset += count;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        let value = *self.data.get(self.offset)?;
        self.offset += 1;
        Some(value)
    }

    fn u16(&mut self) -> Option<u16> {
        let raw = self.bytes(2)?;
        Some(u16::from_be_bytes([raw[0], raw[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let raw = self.bytes(4)?;
        Some(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|value| i32::from_be_bytes(value.to_be_bytes()))
    }

    fn f64(&mut self) -> Option<f64> {
        let raw = self.bytes(8)?;
        let bits = raw.iter().fold(0u64, |value, byte| (value << 8) | *byte as u64);
        Some(f64::from_bits(bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(text: &str) -> Engine {
        engine_value(text.as_bytes()).expect("engine dictionary")
    }

    #[test]
    fn engine_parses_dictionaries_arrays_strings_and_numbers() {
        let value = engine("<< /A 1 /B [1 2 3] /C (hi\\n) /D /Name /E true /F false /G << /H -2.5e1 >> >>");
        assert_eq!(engine_number(walk(&value, &["A"])), Some(1.0));
        assert_eq!(engine_number(walk(&value, &["G", "H"])), Some(-25.0));
        assert_eq!(engine_string(walk(&value, &["C"])).as_deref(), Some("hi\n"));
        assert_eq!(engine_string(walk(&value, &["D"])).as_deref(), Some("Name"));
        assert_eq!(engine_bool(walk(&value, &["E"])), Some(true));
        assert_eq!(engine_bool(walk(&value, &["F"])), Some(false));
        assert_eq!(engine_array(walk(&value, &["B"])).len(), 3);
    }

    #[test]
    fn engine_decodes_hex_utf16_and_latin1() {
        let value = engine("<< /A <FEFF0048 0069> /B (caf\\351) >>");
        assert_eq!(engine_string(walk(&value, &["A"])).as_deref(), Some("Hi"));
        assert_eq!(engine_string(walk(&value, &["B"])).as_deref(), Some("café"));
    }

    #[test]
    fn engine_skips_comments_and_leading_junk() {
        let value = engine("garbage % comment\n<<  /A 7 >>");
        assert_eq!(engine_number(walk(&value, &["A"])), Some(7.0));
    }

    #[test]
    fn colors_handle_rgb_rgba_gray_and_hundreds() {
        assert_eq!(color(&[]), (0.0, 0.0, 0.0));
        assert_eq!(color(&[0.5]), (0.5, 0.5, 0.5));
        assert_eq!(color(&[1.0, 0.0, 0.0]), (1.0, 0.0, 0.0));
        assert_eq!(color(&[1.0, 1.0, 0.0, 0.0]), (1.0, 0.0, 0.0));
        assert_eq!(color(&[255.0, 0.0, 0.0]), (1.0, 0.0, 0.0));
        assert_eq!(color(&[300.0]), (1.0, 1.0, 1.0));
    }

    #[test]
    fn cleaned_strips_bom_and_normalizes_newlines() {
        assert_eq!(cleaned(Some("\u{feff}\0a\r\nb\rc\0")).as_deref(), Some("a\nb\nc"));
        assert_eq!(cleaned(None), None);
    }

    #[test]
    fn placement_accepts_uniform_scale_and_rejects_shear() {
        let placed = placement(2.0, 0.0, 0.0, 2.0, 10.0, 20.0).expect("uniform");
        assert_eq!(placed.pixel_scale, 2.0);
        assert_eq!(placed.rotation, 0.0);
        assert!(!placed.flip_y);
        assert_eq!(placed.map(Point::new(1.0, 2.0)), Point::new(12.0, 24.0));

        let rotated = placement(0.0, -2.0, 2.0, 0.0, 0.0, 0.0).expect("rotation");
        assert_eq!(rotated.rotation, 90.0);
        assert!(placement(2.0, 1.0, 0.0, 2.0, 0.0, 0.0).is_none(), "shear");
        assert!(placement(2.0, 0.0, 0.0, 1.0, 0.0, 0.0).is_none(), "uneven scale");
        assert!(placement(0.0, 0.0, 0.0, 0.0, 0.0, 0.0).is_none(), "degenerate");
    }

    #[test]
    fn flip_is_vertical_only() {
        let placed = placement(1.0, 0.0, 0.0, -1.0, 0.0, 0.0).unwrap();
        assert!(placed.flip_y);
        assert_eq!(placed.map(Point::new(0.0, 1.0)), Point::new(0.0, -1.0));
    }

    #[test]
    fn descriptor_rect_reads_the_coregraphics_box_keys() {
        let mut items: Descriptor = FxHashMap::default();
        items.insert("Left".to_string(), DescriptorValue::Number(1.0));
        items.insert("Top ".to_string(), DescriptorValue::Number(2.0));
        items.insert("Rght".to_string(), DescriptorValue::Number(11.0));
        items.insert("Btom".to_string(), DescriptorValue::Number(8.0));
        let mut descriptor: Descriptor = FxHashMap::default();
        descriptor.insert("bounds".to_string(), DescriptorValue::Descriptor(items));
        assert_eq!(descriptor_rect(&descriptor, "bounds"), Some(Rect::new(1.0, 2.0, 10.0, 6.0)));
        assert_eq!(descriptor_rect(&descriptor, "missing"), None);
    }

    #[test]
    fn reader_reads_versioned_descriptors() {
        // A descriptor, laid out the way `PSDBuffer.descriptor` writes one: version, unicode
        // descriptor name, class id (length 0 → the four-char id follows), item count, then the
        // key as a length-0 four-char id, the TEXT type, and its UTF-16BE value.
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(&16u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes()); // name: empty unicode string
        data.extend_from_slice(&0u32.to_be_bytes()); // class id: length 0 → four-char "null" follows
        data.extend_from_slice(b"null");
        data.extend_from_slice(&1u32.to_be_bytes()); // key count
        data.extend_from_slice(&0u32.to_be_bytes()); // key id: length 0 → four-char "Txt " follows
        data.extend_from_slice(b"Txt ");
        data.extend_from_slice(b"TEXT");
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&[0x00, b'H', 0x00, b'i']);
        let mut reader = Reader { data: &data, offset: 0 };
        let descriptor = reader.descriptor(true).expect("descriptor");
        assert_eq!(text_text(&descriptor, "Txt ").as_deref(), Some("Hi"));
    }
}
