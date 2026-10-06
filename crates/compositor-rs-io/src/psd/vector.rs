//! Rasterizes Photoshop vector masks (`vmsk`/`vsms`) and maps fill rectangles/ellipses
//! onto live shape layers, from Adobe’s 2019 Photoshop File Formats Specification
//! (additional layer information: `vmsk`, `vogk`, `SoCo`, `vstk`). Ported from
//! `IO/PSD/PSDVector.swift`.

use std::sync::Arc;

use compositor_rs_core::buffer::{Rgba8Image, SharedImage};
use compositor_rs_core::color::PaletteColor;
use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::layer_shape::{LayerShapeStyle, ShapeKind};
use compositor_rs_core::limits;
use compositor_rs_core::path::{FillRule, Path};
use compositor_rs_core::path_ops::{LineCap, LineJoin};
use compositor_rs_pixels::canvas::Canvas;
use rustc_hash::FxHashMap;

use crate::psd::types::PSDReadError;
use compositor_rs_core::imported_image::ImageImportError;

/// A vector mask drawn into pixels, with where those pixels sit on the document.
pub struct Raster {
    pub image: SharedImage,
    pub bounds: Rect,
}

/// A vector mask that maps onto a live shape layer.
pub struct Live {
    pub style: LayerShapeStyle,
    pub bounds: Rect,
    pub image: SharedImage,
    pub notes: Vec<String>,
}

pub fn live(
    extra: &FxHashMap<String, &[u8]>,
    canvas: Size,
    remaining_pixels: usize,
) -> Result<Option<Live>, PSDReadError> {
    let stroke = extra.get("vstk").copied();
    let fill_enabled = stroke.and_then(|data| bool_value(data, "fillEnabled")).unwrap_or(extra.contains_key("SoCo"));
    let stroke_enabled = stroke.and_then(|data| bool_value(data, "strokeEnabled")).unwrap_or(false);
    let Some(fill) = (if fill_enabled { extra.get("SoCo").and_then(|data| rgb(data)) } else { None }) else {
        return Ok(None);
    };
    let origin = origination(extra.get("vogk").copied())
        .or_else(|| sharp_rect(extra.get("vmsk").or_else(|| extra.get("vsms")).copied(), canvas));
    let Some(origin) = origin else {
        return Ok(None);
    };
    let mut box_ = origin.bounds.integral();
    if !box_.origin.x.is_finite() || !box_.origin.y.is_finite() {
        return Ok(None);
    }
    let Some(size) = pixel_size(box_.size, remaining_pixels)? else {
        return Ok(None);
    };
    box_.size = Size::new(size.0 as f64, size.1 as f64);
    let style = LayerShapeStyle {
        kind: origin.kind,
        red: fill.0,
        green: fill.1,
        blue: fill.2,
        corner_radius: origin.corner_radius,
        line_width: None,
        start: None,
        end: None,
    };
    let image = Arc::new(shape_image(style.kind, size, style.color(), style.corner_radius));
    let mut notes: Vec<String> = Vec::new();
    if stroke_enabled {
        notes.push("The Photoshop stroke isn’t supported on shape layers and was omitted.".to_string());
    }
    notes.extend(origin.notes.iter().cloned());
    Ok(Some(Live { style, bounds: box_, image, notes }))
}

pub fn raster(
    extra: &FxHashMap<String, &[u8]>,
    canvas: Size,
    remaining_pixels: usize,
) -> Result<Option<Raster>, PSDReadError> {
    let Some(mask) = extra.get("vmsk").or_else(|| extra.get("vsms")) else {
        return Ok(None);
    };
    let Some(path) = path_from(mask, canvas) else {
        return Ok(None);
    };
    let fill = extra.get("SoCo").and_then(|data| rgb(data));
    let stroke = extra.get("vstk").copied();
    let fill_enabled = stroke.and_then(|data| bool_value(data, "fillEnabled")).unwrap_or(fill.is_some());
    let stroke_enabled = stroke.and_then(|data| bool_value(data, "strokeEnabled")).unwrap_or(false);
    let stroke_color = stroke.and_then(|data| rgb(data));
    let stroke_width = stroke.and_then(|data| unit_value(data, "strokeStyleLineWidth", 0)).unwrap_or(1.0);
    if !(fill_enabled && fill.is_some() || stroke_enabled && stroke_color.is_some()) {
        return Ok(None);
    }
    if !stroke_width.is_finite() {
        return Ok(None);
    }
    if stroke_enabled && !(0.0..=limits::MAX_SIDE_EXTENT).contains(&stroke_width) {
        return Err(ImageImportError::TooLarge.into());
    }
    let mut box_ = path.bounding_box();
    if stroke_enabled {
        let inset = -((stroke_width / 2.0 + 1.0).ceil());
        box_ = box_.inset_by(inset, inset);
    }
    box_ = box_.integral();
    if !box_.origin.x.is_finite() || !box_.origin.y.is_finite() {
        return Ok(None);
    }
    let Some(size) = pixel_size(box_.size, remaining_pixels)? else {
        return Ok(None);
    };
    let (width, height) = size;
    let mut target = Canvas::new_rgba(width, height);
    target.translate(-box_.min_x(), -box_.min_y());
    target.set_should_antialias(true);
    if fill_enabled {
        if let Some(fill) = fill {
            target.set_fill_color(PaletteColor::new(fill.0, fill.1, fill.2));
            target.fill_path(&path, FillRule::Winding);
        }
    }
    if stroke_enabled {
        if let Some(stroke_color) = stroke_color {
            // `stroke_path` strokes with the current paint, so the stroke's color is set here.
            target.set_fill_color(PaletteColor::new(stroke_color.0, stroke_color.1, stroke_color.2));
            target.stroke_path(&path, FillRule::Winding, stroke_width, LineCap::Butt, LineJoin::Miter, 10.0);
        }
    }
    let image: SharedImage = Arc::new(target.snapshot());
    Ok(Some(Raster { image, bounds: Rect::new(box_.min_x(), box_.min_y(), width as f64, height as f64) }))
}

/// The shape filling its box, anti-aliased where it curves — `ShapeTool.shapeImage` for the two
/// kinds a Photoshop `vogk`/`vmsk` rectangle or ellipse maps onto.
fn shape_image(kind: ShapeKind, size: (usize, usize), color: compositor_rs_core::PaletteColor, corner_radius: f64) -> Rgba8Image {
    let mut canvas = Canvas::new_rgba(size.0, size.1);
    let bounds = Rect::new(0.0, 0.0, size.0 as f64, size.1 as f64);
    canvas.set_fill_color(color);
    canvas.fill_path(&kind.path(bounds, corner_radius), FillRule::Winding);
    canvas.snapshot()
}

/// Rejects sizes that would trap on the integer conversion or exceed the 30,000 px / remaining-pixel budget.
fn pixel_size(size: Size, remaining_pixels: usize) -> Result<Option<(usize, usize)>, PSDReadError> {
    if !size.width.is_finite() || !size.height.is_finite() {
        return Ok(None);
    }
    if size.width.abs() > limits::MAX_SIDE_EXTENT || size.height.abs() > limits::MAX_SIDE_EXTENT {
        return Err(ImageImportError::TooLarge.into());
    }
    let budget = limits::MAX_SURFACE_PIXELS.min(remaining_pixels);
    if size.width * size.height > budget as f64 {
        return Err(ImageImportError::TooLarge.into());
    }
    let width = 1.max(size.width as i64) as usize;
    let height = 1.max(size.height as i64) as usize;
    if width * height > budget {
        return Err(ImageImportError::TooLarge.into());
    }
    Ok(Some((width, height)))
}

struct Origination {
    kind: ShapeKind,
    bounds: Rect,
    corner_radius: f64,
    notes: Vec<String>,
}

/// Photoshop `vogk` origination: 1/2 = rectangle (2 is rounded), 5 = ellipse.
fn origination(data: Option<&[u8]>) -> Option<Origination> {
    let data = data?;
    let origin_type = int32_value(data, "keyOriginType")?;
    let kind = match origin_type {
        1 | 2 => ShapeKind::Rectangle,
        5 => ShapeKind::Ellipse,
        _ => return None,
    };
    let from = offset_of(data, "keyOriginShapeBBox", 0).unwrap_or(0);
    let left = unit_value(data, "Left", from)?;
    let top = unit_value(data, "Top ", from)?;
    let right = unit_value(data, "Rght", from)?;
    let bottom = unit_value(data, "Btom", from)?;
    let bounds = Rect::new(left, top, right - left, bottom - top);
    if bounds.width() < 1.0
        || bounds.height() < 1.0
        || !bounds.origin.x.is_finite()
        || !bounds.origin.y.is_finite()
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
    {
        return None;
    }
    let mut origin = Origination { kind, bounds, corner_radius: 0.0, notes: Vec::new() };
    if kind == ShapeKind::Rectangle {
        if let Some(radii_at) = offset_of(data, "keyOriginRRectRadii", 0) {
            let keys = ["topLeft", "topRight", "bottomRight", "bottomLeft"];
            let radii: Vec<f64> = keys.iter().filter_map(|key| unit_value(data, key, radii_at)).collect();
            if radii.len() == 4 {
                let lo = radii.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = radii.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                if hi - lo > 0.5 {
                    return None;
                }
                origin.corner_radius = hi;
            }
        }
    }
    Some(origin)
}

fn sharp_rect(data: Option<&[u8]>, canvas: Size) -> Option<Origination> {
    let data = data?;
    let path = path_from(data, canvas)?;
    let mut offset = 8usize;
    let mut remaining = 0i32;
    let mut anchors: Vec<Point> = Vec::new();
    let mut sharp = true;
    while offset + 26 <= data.len() {
        let kind = i16_at(data, offset) as i64;
        let body = &data[offset + 2..offset + 26];
        offset += 26;
        match kind {
            0 | 3 => {
                if !anchors.is_empty() {
                    return None;
                }
                remaining = i16_at(body, 0) as i32;
            }
            1 | 2 | 4 | 5 => {
                if remaining <= 0 {
                    continue;
                }
                remaining -= 1;
                let incoming = point(body, 0, canvas);
                let anchor = point(body, 8, canvas);
                let outgoing = point(body, 16, canvas);
                if (incoming.x - anchor.x).hypot(incoming.y - anchor.y) > 0.5
                    || (outgoing.x - anchor.x).hypot(outgoing.y - anchor.y) > 0.5
                {
                    sharp = false;
                }
                anchors.push(anchor);
            }
            _ => continue,
        }
    }
    if !sharp || anchors.len() != 4 {
        return None;
    }
    let box_ = path.bounding_box();
    if box_.width() < 1.0 || box_.height() < 1.0 {
        return None;
    }
    Some(Origination { kind: ShapeKind::Rectangle, bounds: box_, corner_radius: 0.0, notes: Vec::new() })
}

/// The cubic records of a `vmsk`/`vsms` block as a path in document pixels.
pub fn path_from(data: &[u8], canvas: Size) -> Option<Path> {
    if data.len() < 8 || canvas.width <= 0.0 || canvas.height <= 0.0 {
        return None;
    }
    let mut path = Path::empty();
    let mut offset = 8usize;
    let mut remaining = 0i32;
    let mut closed = true;
    let mut first = true;
    let mut previous_out = Point::ZERO;
    while offset + 26 <= data.len() {
        let kind = i16_at(data, offset) as i64;
        let body = &data[offset + 2..offset + 26];
        offset += 26;
        match kind {
            0 | 3 => {
                if !first && closed {
                    path.close_subpath();
                }
                remaining = i16_at(body, 0) as i32;
                closed = kind == 0;
                first = true;
            }
            1 | 2 | 4 | 5 => {
                if remaining <= 0 {
                    continue;
                }
                remaining -= 1;
                let incoming = point(body, 0, canvas);
                let anchor = point(body, 8, canvas);
                let outgoing = point(body, 16, canvas);
                if first {
                    path.move_to(anchor);
                    first = false;
                } else {
                    path.add_curve(previous_out, incoming, anchor);
                }
                previous_out = outgoing;
            }
            _ => continue,
        }
    }
    if !first && closed {
        path.close_subpath();
    }
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

pub fn rgb(data: &[u8]) -> Option<(f64, f64, f64)> {
    let r = double_value(data, "Rd  ")?;
    let g = double_value(data, "Grn ")?;
    let b = double_value(data, "Bl  ")?;
    fn channel(value: f64) -> f64 {
        if value > 1.0 {
            value.clamp(0.0, 255.0) / 255.0
        } else {
            value.clamp(0.0, 1.0)
        }
    }
    Some((channel(r), channel(g), channel(b)))
}

pub fn bool_value(data: &[u8], key: &str) -> Option<bool> {
    let start = offset_of(data, key, 0)?;
    let tag = start + key.len();
    if !tag_eq(data, tag, b"bool") {
        return None;
    }
    data.get(tag + 4).map(|value| *value != 0)
}

pub fn unit_value(data: &[u8], key: &str, from: usize) -> Option<f64> {
    let key_at = offset_of(data, key, from)?;
    let unit = offset_of(data, "UntF", key_at)?;
    double_at(data, unit + 8)
}

fn int32_value(data: &[u8], key: &str) -> Option<i32> {
    let start = offset_of(data, key, 0)?;
    let tag = start + key.len();
    if !tag_eq(data, tag, b"long") {
        return None;
    }
    // A short buffer is nil, not the zero `i32_at` falls back to.
    if tag + 4 > data.len() {
        return None;
    }
    Some(i32_at(data, tag + 4))
}

fn point(bytes: &[u8], at: usize, canvas: Size) -> Point {
    let y = i32_at(bytes, at) as f64 / 0x1000000 as f64;
    let x = i32_at(bytes, at + 4) as f64 / 0x1000000 as f64;
    Point::new(x * canvas.width, y * canvas.height)
}

fn i32_at(bytes: &[u8], at: usize) -> i32 {
    if at + 4 > bytes.len() {
        return 0;
    }
    i32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn i16_at(bytes: &[u8], at: usize) -> i16 {
    if at + 2 > bytes.len() {
        return 0;
    }
    i16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn double_value(data: &[u8], key: &str) -> Option<f64> {
    let start = offset_of(data, key, 0)?;
    let tag = start + key.len();
    if !tag_eq(data, tag, b"doub") {
        return None;
    }
    double_at(data, tag + 4)
}

fn double_at(data: &[u8], offset: usize) -> Option<f64> {
    let raw = data.get(offset..offset + 8)?;
    let mut bits = 0u64;
    for byte in raw {
        bits = bits << 8 | *byte as u64;
    }
    Some(f64::from_bits(bits))
}

fn offset_of(data: &[u8], key: &str, from: usize) -> Option<usize> {
    let needle = key.as_bytes();
    if from >= data.len() || needle.is_empty() || data.len() - from < needle.len() {
        return None;
    }
    data[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index + from)
}

fn tag_eq(data: &[u8], offset: usize, tag: &[u8; 4]) -> bool {
    data.len() >= offset + 4 && &data[offset..offset + 4] == tag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(value: f64) -> i32 {
        (value * 0x1000000 as f64).round() as i32
    }

    fn point_record(x: f64, y: f64, sharp: bool) -> Vec<u8> {
        let mut record = Vec::new();
        // `vectorMask` in PSDRoundTripTests writes every cubic point as a kind-1 record.
        record.extend_from_slice(&1i16.to_be_bytes()); // a cubic point
        let anchor = (fixed(x), fixed(y));
        let control = if sharp { anchor } else { (fixed(x + 0.25), fixed(y)) };
        for value in [control.1, control.0, anchor.1, anchor.0, control.1, control.0] {
            record.extend_from_slice(&value.to_be_bytes());
        }
        record
    }

    fn rectangle_mask() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_be_bytes()); // version
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes()); // path count
        data.extend_from_slice(&0i16.to_be_bytes()); // closed subpath
        data.extend_from_slice(&4i16.to_be_bytes()); // four corners
        data.extend_from_slice(&[0u8; 22]); // pad the length record to its 26 bytes, as `vectorMask` does
        for (x, y) in [(0.1, 0.1), (0.5, 0.1), (0.5, 0.5), (0.1, 0.5)] {
            data.extend_from_slice(&point_record(x, y, true));
        }
        data
    }

    #[test]
    fn vmsk_paths_land_on_the_canvas() {
        let mask = rectangle_mask();
        let path = path_from(&mask, Size::new(100.0, 50.0)).expect("path");
        let bounds = path.bounding_box();
        assert!((bounds.min_x() - 10.0).abs() < 0.001);
        assert!((bounds.min_y() - 5.0).abs() < 0.001);
        assert!((bounds.width() - 40.0).abs() < 0.001);
        assert!((bounds.height() - 20.0).abs() < 0.001);
    }

    #[test]
    fn sharp_rects_become_live_rectangles() {
        let origin = sharp_rect(Some(&rectangle_mask()), Size::new(100.0, 50.0)).expect("rectangle");
        assert_eq!(origin.kind, ShapeKind::Rectangle);
        assert!((origin.bounds.width() - 40.0).abs() < 0.001);
        // A rounded outline is not a sharp rectangle.
        let mut rounded = Vec::new();
        rounded.extend_from_slice(&0u16.to_be_bytes());
        rounded.extend_from_slice(&0u16.to_be_bytes());
        rounded.extend_from_slice(&1u32.to_be_bytes());
        rounded.extend_from_slice(&0i16.to_be_bytes());
        rounded.extend_from_slice(&4i16.to_be_bytes());
        rounded.extend_from_slice(&[0u8; 22]);
        for (x, y) in [(0.1, 0.1), (0.5, 0.1), (0.5, 0.5), (0.1, 0.5)] {
            rounded.extend_from_slice(&point_record(x, y, false));
        }
        assert!(sharp_rect(Some(&rounded), Size::new(100.0, 50.0)).is_none());
    }

    #[test]
    fn colors_scale_from_unit_or_bytes() {
        let mut data = Vec::new();
        data.extend_from_slice(b"Rd  doub");
        data.extend_from_slice(&0.5f64.to_bits().to_be_bytes());
        data.extend_from_slice(b"Grn doub");
        data.extend_from_slice(&1.0f64.to_bits().to_be_bytes());
        data.extend_from_slice(b"Bl  doub");
        data.extend_from_slice(&255.0f64.to_bits().to_be_bytes());
        assert_eq!(rgb(&data), Some((0.5, 1.0, 1.0)));
        assert_eq!(rgb(b"nope"), None);
    }

    #[test]
    fn booleans_and_units_read_their_tags() {
        let mut data = Vec::new();
        data.extend_from_slice(b"fillEnabledbool");
        data.push(1);
        assert_eq!(bool_value(&data, "fillEnabled"), Some(true));
        assert_eq!(bool_value(&data, "missing"), None);
        assert_eq!(bool_value(b"xy", "x"), None);

        let mut unit = Vec::new();
        unit.extend_from_slice(b"strokeStyleLineWidthUntF#Rlt");
        unit.extend_from_slice(&2.5f64.to_bits().to_be_bytes());
        assert_eq!(unit_value(&unit, "strokeStyleLineWidth", 0), Some(2.5));
    }

    fn fill_style() -> Vec<u8> {
        let mut fill = Vec::new();
        fill.extend_from_slice(b"Rd  doub");
        fill.extend_from_slice(&1.0f64.to_bits().to_be_bytes());
        fill.extend_from_slice(b"Grn doub");
        fill.extend_from_slice(&0.0f64.to_bits().to_be_bytes());
        fill.extend_from_slice(b"Bl  doub");
        fill.extend_from_slice(&0.0f64.to_bits().to_be_bytes());
        fill
    }

    /// A triangle: sharp corners but only three of them, so it rasterizes and is not a live shape.
    fn triangle_mask() -> Vec<u8> {
        let mut mask = Vec::new();
        mask.extend_from_slice(&0u16.to_be_bytes());
        mask.extend_from_slice(&0u16.to_be_bytes());
        mask.extend_from_slice(&1u32.to_be_bytes());
        mask.extend_from_slice(&0i16.to_be_bytes());
        mask.extend_from_slice(&7i16.to_be_bytes());
        mask.extend_from_slice(&[0u8; 22]); // pad the length record to its 26 bytes, as `vectorMask` does
        for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
            mask.extend_from_slice(&point_record(x, y, true));
        }
        mask
    }

    #[test]
    fn a_filled_triangle_rasterizes_with_antialiased_edges() {
        let mask = triangle_mask();
        let fill = fill_style();
        let mut extra = FxHashMap::default();
        extra.insert("vmsk".to_string(), mask.as_slice());
        extra.insert("SoCo".to_string(), fill.as_slice());

        let canvas = Size::new(40.0, 20.0);
        let raster = raster(&extra, canvas, 1_000_000).unwrap().expect("raster");
        assert_eq!((raster.image.width(), raster.image.height()), (40, 20));
        assert_eq!(raster.bounds, Rect::new(0.0, 0.0, 40.0, 20.0));
        // Well inside the triangle: the fill is opaque red.
        assert_eq!(raster.image.get(5, 5), [255, 0, 0, 255]);
        // Past the hypotenuse (x/40 + y/20 > 1): nothing was drawn.
        assert_eq!(raster.image.get(38, 18), [0, 0, 0, 0]);
        // Three anchors do not make a live rectangle.
        assert!(live(&extra, canvas, 1_000_000).unwrap().is_none());
    }

    #[test]
    fn a_sharp_rectangle_becomes_a_live_shape() {
        let mask = rectangle_mask();
        let fill = fill_style();
        let mut extra = FxHashMap::default();
        extra.insert("vmsk".to_string(), mask.as_slice());
        extra.insert("SoCo".to_string(), fill.as_slice());

        let canvas = Size::new(40.0, 20.0);
        let live = live(&extra, canvas, 1_000_000).unwrap().expect("live");
        assert_eq!(live.style.kind, ShapeKind::Rectangle);
        assert_eq!((live.style.red, live.style.green, live.style.blue), (1.0, 0.0, 0.0));
        assert!(live.notes.is_empty());
        // The shape fills its box in the layer's own pixels (16×8 here).
        assert_eq!((live.image.width(), live.image.height()), (16, 8));
        assert_eq!(live.image.get(8, 4), [255, 0, 0, 255]);

        let raster = raster(&extra, canvas, 1_000_000).unwrap().expect("raster");
        assert_eq!((raster.image.width(), raster.image.height()), (16, 8));
        assert_eq!(raster.image.get(8, 4), [255, 0, 0, 255]);
    }
}
