//! Reads Photoshop `.psd`/`.psb` files from Adobe’s *Photoshop File Formats Specification*
//! (2019 HTML edition: File Header, Color Mode Data, Image Resources, Layer and
//! Mask Information, Image Data). Original implementation of the 8BPS header,
//! layer records, PackBits, and additional layer info. Not copied, transcribed,
//! or adapted from GIMP, psd-tools, or any other GPL-licensed PSD reader.
//!
//! Ported from `IO/PSD/PSDReader.swift`.

use std::sync::Arc;

use compositor_rs_core::buffer::{SharedGray, SharedImage};
use compositor_rs_core::geom::{Point, Rect, Size};
use compositor_rs_core::imported_image::ImageImportError;
use compositor_rs_core::layer_adjustment::{
    AdjustmentKind, ColorRange, CurvePoint, HueBand, LayerAdjustment, LevelRange, RangeAdjustment,
};
use compositor_rs_core::limits;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::psd::channel_coder::{self, PSDCrop};
use crate::psd::text as psd_text;
use crate::psd::types::{PSDDocument, PSDError, PSDLayerKind, PSDReadError, PSDRecord};
use crate::psd::vector as psd_vector;

/// The PSD reader.
pub struct PSDReader;

impl PSDReader {
    pub fn matches(path: &std::path::Path) -> bool {
        use std::io::Read;
        let Ok(mut file) = std::fs::File::open(path) else { return false };
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).is_ok() && &magic == b"8BPS"
    }

    pub fn matches_data(data: &[u8]) -> bool {
        data.len() >= 4 && &data[..4] == b"8BPS"
    }

    pub fn read(path: &std::path::Path, remaining_pixels: usize) -> Result<PSDDocument, PSDReadError> {
        let data = std::fs::read(path).map_err(|_| PSDReadError::Import(ImageImportError::Unreadable))?;
        Self::read_data(&data, remaining_pixels)
    }

    pub fn read_data(data: &[u8], remaining_pixels: usize) -> Result<PSDDocument, PSDReadError> {
        let mut cursor = PSDCursor::new(data);
        if cursor.string(4)? != "8BPS" {
            return Err(ImageImportError::Unreadable.into());
        }
        let version = cursor.u16()?;
        if version != 1 && version != 2 {
            return Err(PSDError::UnsupportedVersion.into());
        }
        let is_psb = version == 2;
        cursor.skip(6)?;
        let _channels = cursor.u16()?;
        let canvas_height = cursor.u32()? as usize;
        let canvas_width = cursor.u32()? as usize;
        let depth = cursor.u16()?;
        let mode = cursor.u16()?;
        if !(1..=limits::MAX_SIDE).contains(&canvas_width)
            || !(1..=limits::MAX_SIDE).contains(&canvas_height)
            || canvas_width * canvas_height > limits::MAX_SURFACE_PIXELS
        {
            return Err(ImageImportError::TooLarge.into());
        }
        if depth != 8 {
            return Err(PSDError::UnsupportedDepth.into());
        }
        if mode != 3 {
            return Err(PSDError::UnsupportedColorMode.into());
        }
        let color_mode_length = cursor.u32()? as usize;
        cursor.skip(color_mode_length)?;
        let resources_length = cursor.u32()? as usize;
        let resources_end = cursor.offset().saturating_add(resources_length);
        let mut resolution = 72.0;
        while cursor.offset().saturating_add(12) <= resources_end {
            let signature = cursor.string(4)?;
            if signature != "8BIM" {
                break;
            }
            let id = cursor.u16()?;
            let name_length = cursor.u8()? as usize;
            cursor.skip(name_length)?;
            if (name_length + 1) % 2 == 1 {
                cursor.skip(1)?;
            }
            let length = cursor.u32()? as usize;
            let data_start = cursor.offset();
            if id == 1005 && length >= 4 {
                resolution = cursor.u32()? as f64 / 65536.0;
                if !resolution.is_finite() || resolution < 1.0 {
                    resolution = 72.0;
                }
                resolution = resolution.clamp(1.0, 9600.0);
            }
            cursor.set_offset(data_start.saturating_add(length));
            if length % 2 == 1 {
                cursor.skip(1)?;
            }
        }
        cursor.set_offset(resources_end);
        let layer_section = checked_length(if is_psb { cursor.u64()? } else { cursor.u32()? as u64 })?;
        let layer_section_end = cursor.offset().saturating_add(layer_section);
        if layer_section < 4 {
            return Ok(PSDDocument { width: canvas_width, height: canvas_height, resolution, layers: Vec::new() });
        }
        let _layer_info_length = checked_length(if is_psb { cursor.u64()? } else { cursor.u32()? as u64 })?;
        let raw_count = cursor.i16()?;
        let count = raw_count.unsigned_abs() as usize;
        if count > 10_000 {
            return Err(ImageImportError::TooLarge.into());
        }
        let mut raw: Vec<RawLayer> = Vec::with_capacity(count);
        for _ in 0..count {
            raw.push(read_record(&mut cursor, is_psb)?);
        }
        if !layers_fit_budget(&raw, remaining_pixels) {
            for layer in raw.iter_mut() {
                crop_to_canvas(layer, canvas_width, canvas_height);
            }
            if !layers_fit_budget(&raw, remaining_pixels) {
                return Err(ImageImportError::TooLarge.into());
            }
        }
        let mut used_pixels = 0usize;
        for layer in raw.iter_mut() {
            decode_channels(&mut cursor, layer, remaining_pixels.saturating_sub(used_pixels), is_psb)?;
            if let Some(image) = &layer.image {
                used_pixels += image.width() * image.height();
            }
        }
        cursor.set_offset(layer_section_end);
        let canvas = Size::new(canvas_width as f64, canvas_height as f64);
        let layers = assemble(&raw, canvas, remaining_pixels.saturating_sub(used_pixels))?;
        Ok(PSDDocument { width: canvas_width, height: canvas_height, resolution, layers })
    }
}

/// The raw layer record as read from the file, before `assemble` turns it into a `PSDRecord`.
struct RawLayer<'a> {
    name: String,
    top: i64,
    left: i64,
    bottom: i64,
    right: i64,
    source_top: i64,
    source_left: i64,
    source_bottom: i64,
    source_right: i64,
    opacity: u8,
    fill: u8,
    clipping: bool,
    hidden: bool,
    blend_key: String,
    channels: Vec<(i16, usize)>,
    extra: FxHashMap<String, &'a [u8]>,
    mask_top: i64,
    mask_left: i64,
    mask_bottom: i64,
    mask_right: i64,
    source_mask_top: i64,
    source_mask_left: i64,
    source_mask_bottom: i64,
    source_mask_right: i64,
    mask_default: u8,
    mask_disabled: bool,
    mask_linked: bool,
    mask_from_render: bool,
    has_mask: bool,
    section: Option<i32>,
    image: Option<SharedImage>,
    mask_image: Option<SharedGray>,
    image_crop: Option<PSDCrop>,
    mask_crop: Option<PSDCrop>,
    cropped: bool,
}

impl Default for RawLayer<'_> {
    fn default() -> Self {
        RawLayer {
            name: String::new(),
            top: 0,
            left: 0,
            bottom: 0,
            right: 0,
            source_top: 0,
            source_left: 0,
            source_bottom: 0,
            source_right: 0,
            opacity: 255,
            fill: 255,
            clipping: false,
            hidden: false,
            blend_key: "norm".to_string(),
            channels: Vec::new(),
            extra: FxHashMap::default(),
            mask_top: 0,
            mask_left: 0,
            mask_bottom: 0,
            mask_right: 0,
            source_mask_top: 0,
            source_mask_left: 0,
            source_mask_bottom: 0,
            source_mask_right: 0,
            mask_default: 255,
            mask_disabled: false,
            mask_linked: true,
            mask_from_render: false,
            has_mask: false,
            section: None,
            image: None,
            mask_image: None,
            image_crop: None,
            mask_crop: None,
            cropped: false,
        }
    }
}

/// The additional-info keys whose length is 8 bytes in a PSB.
const PSB_LARGE_ADDITIONAL_INFO_KEYS: [&str; 13] = [
    "LMsk", "Lr16", "Lr32", "Layr", "Mt16", "Mt32", "Mtrn", "Alph", "FMsk", "lnk2", "FEid", "FXid", "PxSD",
];

fn checked_length(value: u64) -> Result<usize, PSDReadError> {
    // Swift's `UInt64(Int.max)` bound: a length that cannot be an `Int` is an oversized file.
    if value > i64::MAX as u64 {
        return Err(ImageImportError::TooLarge.into());
    }
    Ok(value as usize)
}

fn read_record<'a>(cursor: &mut PSDCursor<'a>, is_psb: bool) -> Result<RawLayer<'a>, PSDReadError> {
    let mut layer = RawLayer::default();
    layer.top = cursor.i32()? as i64;
    layer.left = cursor.i32()? as i64;
    layer.bottom = cursor.i32()? as i64;
    layer.right = cursor.i32()? as i64;
    layer.source_top = layer.top;
    layer.source_left = layer.left;
    layer.source_bottom = layer.bottom;
    layer.source_right = layer.right;
    let channel_count = cursor.u16()? as usize;
    if channel_count > 56 {
        return Err(ImageImportError::TooLarge.into());
    }
    for _ in 0..channel_count {
        let id = cursor.i16()?;
        let length = checked_length(if is_psb { cursor.u64()? } else { cursor.u32()? as u64 })?;
        layer.channels.push((id, length));
    }
    if cursor.string(4)? != "8BIM" {
        return Err(PSDError::Truncated.into());
    }
    layer.blend_key = cursor.string(4)?;
    layer.opacity = cursor.u8()?;
    layer.clipping = cursor.u8()? != 0;
    let flags = cursor.u8()?;
    layer.hidden = (flags & 2) != 0;
    cursor.skip(1)?;
    let extra_length = cursor.u32()? as usize;
    let extra_end = cursor.offset().saturating_add(extra_length);
    let mask_length = cursor.u32()? as usize;
    let mask_end = cursor.offset().saturating_add(mask_length);
    if mask_length >= 20 {
        layer.has_mask = true;
        layer.mask_top = cursor.i32()? as i64;
        layer.mask_left = cursor.i32()? as i64;
        layer.mask_bottom = cursor.i32()? as i64;
        layer.mask_right = cursor.i32()? as i64;
        layer.source_mask_top = layer.mask_top;
        layer.source_mask_left = layer.mask_left;
        layer.source_mask_bottom = layer.mask_bottom;
        layer.source_mask_right = layer.mask_right;
        layer.mask_default = cursor.u8()?;
        let mask_flags = cursor.u8()?;
        layer.mask_disabled = (mask_flags & 2) != 0;
        layer.mask_linked = (mask_flags & 1) == 0;
        layer.mask_from_render = (mask_flags & 8) != 0;
    }
    cursor.set_offset(mask_end);
    let ranges = cursor.u32()? as usize;
    cursor.skip(ranges)?;
    let name_count = cursor.u8()? as usize;
    let name_bytes = cursor.bytes(name_count)?;
    // Mac OS Roman decodes every byte value, so the Swift's ISO Latin-1 fallback never ran.
    layer.name = mac_roman(name_bytes);
    let name_pad = (4 - ((name_count + 1) % 4)) % 4;
    cursor.skip(name_pad)?;
    while cursor.offset().saturating_add(12) <= extra_end {
        let signature = cursor.string(4)?;
        if signature != "8BIM" && signature != "8B64" {
            break;
        }
        let key = cursor.string(4)?;
        let length: usize;
        if signature == "8B64" || (is_psb && PSB_LARGE_ADDITIONAL_INFO_KEYS.contains(&key.as_str())) {
            if cursor.offset().saturating_add(8) > extra_end {
                break;
            }
            length = checked_length(cursor.u64()?)?;
        } else {
            length = cursor.u32()? as usize;
        }
        let payload = cursor.bytes(length)?;
        if length % 2 == 1 {
            cursor.skip(1)?;
        }
        layer.extra.insert(key.clone(), payload);
        if key == "luni" {
            if let Some(unicode) = unicode_name(payload) {
                layer.name = unicode;
            }
        }
        if key == "iOpa" {
            if let Some(fill) = payload.first() {
                layer.fill = *fill;
            }
        }
        if key == "lsct" || key == "lsdk" {
            if payload.len() >= 4 {
                layer.section = Some(u32_at(payload, 0) as i32);
            }
        }
    }
    cursor.set_offset(extra_end);
    Ok(layer)
}

fn unicode_name(data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }
    let count = u32_at(data, 0) as usize;
    if count == 0 || data.len() < 4 + count * 2 {
        return None;
    }
    let mut units = Vec::with_capacity(count);
    for index in 0..count {
        let hi = data[4 + index * 2];
        let lo = data[5 + index * 2];
        units.push(u16::from_be_bytes([hi, lo]));
    }
    Some(String::from_utf16_lossy(&units).trim_matches('\0').to_string())
}

/// Transparency, R, G, B, and the user mask. Spot and other extra IDs are skipped before decode.
const UNPACKED_CHANNEL_IDS: [i16; 5] = [-1, 0, 1, 2, -2];

fn layers_fit_budget(layers: &[RawLayer], remaining_pixels: usize) -> bool {
    let mut used_pixels = 0usize;
    for layer in layers {
        let width = (layer.right - layer.left).max(0);
        let height = (layer.bottom - layer.top).max(0);
        let mask_width = (layer.mask_right - layer.mask_left).max(0);
        let mask_height = (layer.mask_bottom - layer.mask_top).max(0);
        if !layer_fits_budget(
            width,
            height,
            mask_width,
            mask_height,
            layer.has_mask,
            remaining_pixels.saturating_sub(used_pixels),
        ) {
            return false;
        }
        if width > 0 && height > 0 {
            used_pixels += (width * height) as usize;
        }
    }
    true
}

fn layer_fits_budget(width: i64, height: i64, mask_width: i64, mask_height: i64, has_mask: bool, remaining_pixels: usize) -> bool {
    let budget = remaining_pixels as i64;
    if width > 0 && height > 0 {
        if !(width <= limits::MAX_SIDE as i64 && height <= limits::MAX_SIDE as i64 && width * height <= budget) {
            return false;
        }
    }
    if has_mask && mask_width > 0 && mask_height > 0 {
        if !(mask_width <= limits::MAX_SIDE as i64 && mask_height <= limits::MAX_SIDE as i64 && mask_width * mask_height <= budget) {
            return false;
        }
    }
    true
}

fn crop_to_canvas(layer: &mut RawLayer, width: usize, height: usize) {
    let image_crop = crop_box(layer.left, layer.top, layer.right, layer.bottom, width, height);
    if image_crop.x != 0
        || image_crop.y != 0
        || image_crop.width != layer.right - layer.left
        || image_crop.height != layer.bottom - layer.top
    {
        layer.left += image_crop.x;
        layer.top += image_crop.y;
        layer.right = layer.left + image_crop.width;
        layer.bottom = layer.top + image_crop.height;
        layer.image_crop = Some(image_crop);
        layer.cropped = true;
    }
    if !layer.has_mask {
        return;
    }
    let mask_crop = crop_box(layer.mask_left, layer.mask_top, layer.mask_right, layer.mask_bottom, width, height);
    if mask_crop.x != 0
        || mask_crop.y != 0
        || mask_crop.width != layer.mask_right - layer.mask_left
        || mask_crop.height != layer.mask_bottom - layer.mask_top
    {
        layer.mask_left += mask_crop.x;
        layer.mask_top += mask_crop.y;
        layer.mask_right = layer.mask_left + mask_crop.width;
        layer.mask_bottom = layer.mask_top + mask_crop.height;
        layer.mask_crop = Some(mask_crop);
        layer.cropped = true;
    }
}

fn crop_box(left: i64, top: i64, right: i64, bottom: i64, canvas_width: usize, canvas_height: usize) -> PSDCrop {
    let canvas_width = canvas_width as i64;
    let canvas_height = canvas_height as i64;
    let cropped_left = canvas_width.min(left.max(0));
    let cropped_top = canvas_height.min(top.max(0));
    let cropped_right = cropped_left.max(canvas_width.min(right));
    let cropped_bottom = cropped_top.max(canvas_height.min(bottom));
    PSDCrop {
        x: cropped_left - left,
        y: cropped_top - top,
        width: cropped_right - cropped_left,
        height: cropped_bottom - cropped_top,
    }
}

/// The layer's target and source sizes, and where the keeper crop sits, for one channel decode.
struct ChannelDims {
    width: i64,
    height: i64,
    mask_width: i64,
    mask_height: i64,
    source_width: i64,
    source_height: i64,
    source_mask_width: i64,
    source_mask_height: i64,
    image_crop: Option<PSDCrop>,
    mask_crop: Option<PSDCrop>,
}

/// One channel payload: the compression word, then the compressed plane, decoded to the target size
/// (or the crop of it).
fn decode_one_channel(
    cursor: &mut PSDCursor<'_>,
    id: i16,
    length: usize,
    dims: &ChannelDims,
    is_psb: bool,
) -> Result<Option<Vec<u8>>, PSDReadError> {
    let compression = cursor.u16()?;
    let payload = cursor.bytes(length - 2)?;
    let is_mask = id == -2;
    let source_w = if is_mask { dims.source_mask_width } else { dims.source_width };
    let source_h = if is_mask { dims.source_mask_height } else { dims.source_height };
    let target_w = if is_mask { dims.mask_width } else { dims.width };
    let target_h = if is_mask { dims.mask_height } else { dims.height };
    let crop = if is_mask { dims.mask_crop } else { dims.image_crop };
    if target_w <= 0 || target_h <= 0 {
        return Ok(None);
    }
    Ok(Some(channel_coder::decode(
        compression,
        source_w as usize,
        source_h as usize,
        payload,
        is_psb,
        crop,
    )?))
}

fn decode_channels(
    cursor: &mut PSDCursor<'_>,
    layer: &mut RawLayer<'_>,
    remaining_pixels: usize,
    is_psb: bool,
) -> Result<(), PSDReadError> {
    let mut planes: FxHashMap<i16, Vec<u8>> = FxHashMap::default();
    let width = (layer.right - layer.left).max(0);
    let height = (layer.bottom - layer.top).max(0);
    let mask_width = (layer.mask_right - layer.mask_left).max(0);
    let mask_height = (layer.mask_bottom - layer.mask_top).max(0);
    if !layer_fits_budget(width, height, mask_width, mask_height, layer.has_mask, remaining_pixels) {
        return Err(ImageImportError::TooLarge.into());
    }
    let source_width = (layer.source_right - layer.source_left).max(0);
    let source_height = (layer.source_bottom - layer.source_top).max(0);
    let source_mask_width = (layer.source_mask_right - layer.source_mask_left).max(0);
    let source_mask_height = (layer.source_mask_bottom - layer.source_mask_top).max(0);
    let dims = ChannelDims {
        width,
        height,
        mask_width,
        mask_height,
        source_width,
        source_height,
        source_mask_width,
        source_mask_height,
        image_crop: layer.image_crop,
        mask_crop: layer.mask_crop,
    };
    for &(id, length) in layer.channels.iter() {
        let start = cursor.offset();
        let step = start.saturating_add(length);
        if UNPACKED_CHANNEL_IDS.contains(&id) && length >= 2 {
            if let Some(plane) = decode_one_channel(cursor, id, length, &dims, is_psb)? {
                planes.insert(id, plane);
            }
        }
        cursor.set_offset(step);
    }
    if layer.has_mask && mask_width > 0 && mask_height > 0 {
        if let Some(gray) = planes.get(&-2) {
            let count = (mask_width * mask_height) as usize;
            if gray.len() >= count {
                layer.mask_image = Some(Arc::new(channel_coder::mask_image(
                    mask_width as usize,
                    mask_height as usize,
                    gray[..count].to_vec(),
                )));
            }
        }
    }
    if width <= 0 || height <= 0 {
        return Ok(());
    }
    let count = (width * height) as usize;
    let opaque = vec![255u8; count];
    let black = vec![0u8; count];
    let red = planes.get(&0).cloned().unwrap_or_else(|| black.clone());
    let green = planes.get(&1).cloned().unwrap_or_else(|| black.clone());
    let blue = planes.get(&2).cloned().unwrap_or_else(|| black.clone());
    let alpha = planes.get(&-1).cloned().unwrap_or_else(|| opaque.clone());
    if red.len() < count || green.len() < count || blue.len() < count || alpha.len() < count {
        return Err(PSDError::Truncated.into());
    }
    layer.image = Some(Arc::new(channel_coder::rgba_image(
        width as usize,
        height as usize,
        &red[..count],
        &green[..count],
        &blue[..count],
        &alpha[..count],
    )));
    Ok(())
}

fn assemble(raw: &[RawLayer], canvas: Size, remaining_pixels: usize) -> Result<Vec<PSDRecord>, PSDReadError> {
    let mut result: Vec<PSDRecord> = Vec::new();
    let mut groups: Vec<Uuid> = Vec::new();
    let mut remaining = remaining_pixels;
    for layer in raw {
        // Photoshop stores groups bottom-to-top: type 3 divider, then children, then the folder (type 1/2).
        if layer.section == Some(3) {
            groups.push(Uuid::new_v4());
            continue;
        }
        let is_group = layer.section == Some(1) || layer.section == Some(2);
        let id = if is_group { groups.pop().unwrap_or_else(Uuid::new_v4) } else { Uuid::new_v4() };
        let mut record = PSDRecord::new(id, if layer.name.is_empty() { "Layer" } else { layer.name.as_str() });
        record.parent_id = groups.last().copied();
        record.is_group = is_group;
        record.is_visible = !layer.hidden;
        record.blend_key =
            if is_group && (layer.blend_key == "pass" || layer.blend_key == "norm") { "pass".to_string() } else { layer.blend_key.clone() };
        record.clipping = layer.clipping;
        record.kind = layer_kind(layer, is_group);
        record.cropped_to_canvas = layer.cropped;
        let has_effects = record.kind == PSDLayerKind::Effects
            || layer.extra.keys().any(|key| ["lfx2", "lrFX", "lmfx"].contains(&key.as_str()));
        record.opacity = if has_effects && layer.fill != 255 {
            layer.opacity as f64 / 255.0
        } else {
            (layer.opacity as f64 / 255.0) * (layer.fill as f64 / 255.0)
        };
        record.bounds = if is_group {
            Rect::from_origin_size(Point::ZERO, canvas)
        } else {
            Rect::new(
                layer.left as f64,
                layer.top as f64,
                (layer.right - layer.left).max(0) as f64,
                (layer.bottom - layer.top).max(0) as f64,
            )
        };
        record.image = if is_group { None } else { layer.image.clone() };
        let mut handled = false;
        if record.kind == PSDLayerKind::Text {
            if let Some(text) = psd_text::parse(&layer.extra) {
                record.text = Some(text);
                handled = true;
            }
        }
        if !handled && !is_group {
            if let Some(live) = psd_vector::live(&layer.extra, canvas, remaining)? {
                record.image = Some(live.image.clone());
                record.bounds = live.bounds;
                record.shape = Some(live.style);
                record.shape_notes = live.notes;
                record.kind = PSDLayerKind::Vector;
                remaining = remaining.saturating_sub((live.image.width() * live.image.height()) as usize);
                handled = true;
            } else if record.image.is_none() {
                if let Some(raster) = psd_vector::raster(&layer.extra, canvas, remaining)? {
                    record.image = Some(raster.image.clone());
                    record.bounds = raster.bounds;
                    record.kind = PSDLayerKind::Vector;
                    remaining = remaining.saturating_sub((raster.image.width() * raster.image.height()) as usize);
                }
            }
        }
        record.mask = if layer.mask_from_render { None } else { layer.mask_image.clone() };
        record.mask_bounds = Rect::new(
            layer.mask_left as f64,
            layer.mask_top as f64,
            (layer.mask_right - layer.mask_left) as f64,
            (layer.mask_bottom - layer.mask_top) as f64,
        );
        record.mask_default = layer.mask_default;
        record.mask_enabled = !layer.mask_disabled;
        record.mask_linked = layer.mask_linked;
        if !is_group {
            record.adjustment = PSDAdjustments::parse(&layer.extra);
        }
        if record.adjustment.is_some() {
            record.kind = PSDLayerKind::Adjustment;
        }
        result.push(record);
    }
    if !groups.is_empty() {
        return Err(PSDError::Truncated.into());
    }
    Ok(result)
}

fn layer_kind(layer: &RawLayer, is_group: bool) -> PSDLayerKind {
    if is_group {
        return PSDLayerKind::Group;
    }
    if layer.extra.keys().any(|key| ["TySh", "tySh", "txt2"].contains(&key.as_str())) {
        return PSDLayerKind::Text;
    }
    if layer.extra.keys().any(|key| ["vmsk", "vsms", "vogk"].contains(&key.as_str())) {
        return PSDLayerKind::Vector;
    }
    if layer.extra.keys().any(|key| ["SoLd", "SoLE"].contains(&key.as_str())) {
        return PSDLayerKind::SmartObject;
    }
    if layer.extra.keys().any(|key| ["lfx2", "lrFX", "lmfx"].contains(&key.as_str())) {
        return PSDLayerKind::Effects;
    }
    if layer.extra.keys().any(|key| ADJUSTMENT_KEYS.contains(&key.as_str())) {
        return PSDLayerKind::Adjustment;
    }
    PSDLayerKind::Raster
}

const ADJUSTMENT_KEYS: [&str; 16] = [
    "levl", "curv", "hue2", "hue ", "expA", "grdm", "brit", "blnc", "nvrt", "thrs", "post", "mixr", "selc", "blwh", "phfl", "vibA",
];

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
}

/// The big-endian byte cursor the PSD structures are read with.
struct PSDCursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> PSDCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        PSDCursor { data, offset: 0 }
    }

    fn offset(&self) -> usize {
        self.offset
    }

    fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    fn need(&self, count: usize) -> Result<(), PSDReadError> {
        if self.offset.checked_add(count).is_some_and(|end| end <= self.data.len()) {
            Ok(())
        } else {
            Err(PSDError::Truncated.into())
        }
    }

    fn skip(&mut self, count: usize) -> Result<(), PSDReadError> {
        self.need(count)?;
        self.offset += count;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, PSDReadError> {
        self.need(1)?;
        let value = self.data[self.offset];
        self.offset += 1;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, PSDReadError> {
        self.need(2)?;
        let value = u16::from_be_bytes([self.data[self.offset], self.data[self.offset + 1]]);
        self.offset += 2;
        Ok(value)
    }

    fn i16(&mut self) -> Result<i16, PSDReadError> {
        self.u16().map(|value| i16::from_be_bytes(value.to_be_bytes()))
    }

    fn u32(&mut self) -> Result<u32, PSDReadError> {
        self.need(4)?;
        let value = u32::from_be_bytes([
            self.data[self.offset],
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(value)
    }

    fn i32(&mut self) -> Result<i32, PSDReadError> {
        self.u32().map(|value| i32::from_be_bytes(value.to_be_bytes()))
    }

    fn u64(&mut self) -> Result<u64, PSDReadError> {
        self.need(8)?;
        let mut value = 0u64;
        for index in 0..8 {
            value = value << 8 | self.data[self.offset + index] as u64;
        }
        self.offset += 8;
        Ok(value)
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8], PSDReadError> {
        self.need(count)?;
        let slice = &self.data[self.offset..self.offset + count];
        self.offset += count;
        Ok(slice)
    }

    fn string(&mut self, count: usize) -> Result<String, PSDReadError> {
        let bytes = self.bytes(count)?;
        Ok(as_ascii(bytes))
    }
}

/// `String(bytes:encoding:.ascii) ?? ""`.
fn as_ascii(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::new()
    }
}

/// Mac OS Roman, the encoding layer names use (`String(bytes:encoding:.macOSRoman)`).
fn mac_roman(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| mac_roman_char(*byte)).collect()
}

fn mac_roman_char(byte: u8) -> char {
    const HIGH: [char; 128] = [
        'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á', 'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è', 'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó', 'ò', 'ô',
        'ö', 'õ', 'ú', 'ù', 'û', 'ü', '†', '°', '¢', '£', '§', '•', '¶', 'ß', '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø', '∞', '±', '≤', '≥',
        '¥', 'µ', '∂', '∑', '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø', '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«', '»', '…', '\u{00a0}', 'À',
        'Ã', 'Õ', 'Œ', 'œ', '–', '—', '“', '”', '‘', '’', '÷', '◊', 'ÿ', 'Ÿ', '⁄', '€', '‹', '›', 'ﬁ', 'ﬂ', '‡', '·', '‚', '„', '‰', 'Â',
        'Ê', 'Á', 'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô', '\u{f8ff}', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜', '¯', '˘', '˙', '˚', '¸', '˝', '˛',
        'ˇ',
    ];
    if byte < 0x80 {
        byte as char
    } else {
        HIGH[(byte - 0x80) as usize]
    }
}

/// The layer adjustments Photoshop stores as additional layer information.
pub enum PSDAdjustments {}

impl PSDAdjustments {
    pub fn parse(extra: &FxHashMap<String, &[u8]>) -> Option<LayerAdjustment> {
        if let Some(data) = extra.get("levl") {
            return Self::levels(data);
        }
        if let Some(data) = extra.get("curv") {
            return Self::curves(data);
        }
        if let Some(data) = extra.get("hue2").or_else(|| extra.get("hue ")) {
            return Self::hue(data);
        }
        None
    }

    /// Photoshop's 'levl': a version, then records of input black, input white, output black, output white and gamma
    /// in hundredths (100 is 1.00), for RGB, then red, green and blue.
    pub fn levels(data: &[u8]) -> Option<LayerAdjustment> {
        if data.len() < 292 {
            return None;
        }
        let mut settings = LayerAdjustment::new(AdjustmentKind::Levels).levels;
        for channel in 0..4 {
            let base = 2 + channel * 10;
            let input_black = u16_at(data, base) as f64;
            let input_white = u16_at(data, base + 2) as f64;
            let output_black = u16_at(data, base + 4) as f64;
            let output_white = u16_at(data, base + 6) as f64;
            let gamma = u16_at(data, base + 8) as f64 / 100.0;
            settings.ranges[channel] = LevelRange {
                black: input_black,
                gamma,
                white: input_white,
                output_black,
                output_white,
            }
            .normalized();
        }
        let mut adjustment = LayerAdjustment::new(AdjustmentKind::Levels);
        adjustment.levels = settings;
        Some(adjustment)
    }

    fn curves(data: &[u8]) -> Option<LayerAdjustment> {
        if data.len() < 5 {
            return None;
        }
        let mut offset = 0usize;
        if data[offset] == 0 {
            offset += 1;
        }
        if offset + 2 > data.len() {
            return None;
        }
        let version = u16_at(data, offset);
        offset += 2;
        if version != 1 && version != 4 {
            return None;
        }
        if offset + 2 > data.len() {
            return None;
        }
        let count = u16_at(data, offset) as usize;
        offset += 2;
        let mut settings = LayerAdjustment::new(AdjustmentKind::Curves).curves;
        for channel in 0..count.min(4) {
            if offset + 2 > data.len() {
                return None;
            }
            let points = u16_at(data, offset) as usize;
            offset += 2;
            let mut curve: Vec<CurvePoint> = Vec::new();
            for _ in 0..points {
                if offset + 4 > data.len() {
                    return None;
                }
                let output = u16_at(data, offset) as f64;
                let input = u16_at(data, offset + 2) as f64;
                offset += 4;
                curve.push(CurvePoint { x: input.clamp(0.0, 255.0), y: output.clamp(0.0, 255.0) });
            }
            if curve.len() >= 2 {
                curve.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
                if curve.first().map(|point| point.x) != Some(0.0) {
                    let first_y = curve[0].y;
                    curve.insert(0, CurvePoint { x: 0.0, y: first_y });
                }
                if curve.last().map(|point| point.x) != Some(255.0) {
                    let last_y = curve[curve.len() - 1].y;
                    curve.push(CurvePoint { x: 255.0, y: last_y });
                }
                settings.channels[channel] = curve;
            }
        }
        if !settings.is_valid() {
            return None;
        }
        let mut adjustment = LayerAdjustment::new(AdjustmentKind::Curves);
        adjustment.curves = settings;
        Some(adjustment)
    }

    /// Photoshop's 'hue2': a version, the Colorize switch and a pad byte, the Colorize hue, saturation and lightness,
    /// the Master's, then for Reds through Magentas the band (where the range fades in, is full, and fades out, in
    /// degrees) and its hue, saturation and lightness.
    pub fn hue(data: &[u8]) -> Option<LayerAdjustment> {
        if data.len() < 16 {
            return None;
        }
        let colorize = data[2] != 0;
        let mut settings = LayerAdjustment::new(AdjustmentKind::Hsv).resolved_hsv();
        settings.colorize = colorize;
        // Colorize has values of its own; the Master applies otherwise.
        settings.adjustments.insert(ColorRange::Master, range_adjustment(data, if colorize { 4 } else { 10 }));
        if colorize {
            let mut adjustment = LayerAdjustment::new(AdjustmentKind::Hsv);
            adjustment.hsv_settings = Some(settings);
            return Some(adjustment);
        }
        let mut offset = 16usize;
        for range in COLOR_RANGES {
            if offset + 14 > data.len() {
                break;
            }
            fn degrees(data: &[u8], at: usize) -> f64 {
                let value = i16_at(data, at) as f64 % 360.0;
                if value < 0.0 {
                    value + 360.0
                } else {
                    value
                }
            }
            settings.bands.insert(
                range,
                HueBand {
                    falloff_start: degrees(data, offset),
                    range_start: degrees(data, offset + 2),
                    range_end: degrees(data, offset + 4),
                    falloff_end: degrees(data, offset + 6),
                },
            );
            settings.adjustments.insert(range, range_adjustment(data, offset + 8));
            offset += 14;
        }
        let mut adjustment = LayerAdjustment::new(AdjustmentKind::Hsv);
        adjustment.hsv_settings = Some(settings);
        Some(adjustment)
    }
}

/// Photoshop's color ranges in the order the 'hue2' block stores them: Reds through Magentas.
const COLOR_RANGES: [ColorRange; 6] = [
    ColorRange::Reds,
    ColorRange::Yellows,
    ColorRange::Greens,
    ColorRange::Cyans,
    ColorRange::Blues,
    ColorRange::Magentas,
];

fn range_adjustment(data: &[u8], offset: usize) -> RangeAdjustment {
    RangeAdjustment {
        hue: i16_at(data, offset) as f64,
        saturation: i16_at(data, offset + 2) as f64,
        lightness: i16_at(data, offset + 4) as f64,
    }
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

fn i16_at(data: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([data[offset], data[offset + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_bytes(value: u16) -> [u8; 2] {
        value.to_be_bytes()
    }

    fn u32_bytes(value: u32) -> [u8; 4] {
        value.to_be_bytes()
    }

    fn u64_bytes(value: u64) -> [u8; 8] {
        value.to_be_bytes()
    }

    /// A `.psd`/`.psb` holding one 4×2 layer with red, green, blue and alpha channels as raw data.
    fn synthetic_psd(layer_name: &str, psb: bool, resolution: Option<f64>) -> Vec<u8> {
        let (width, height) = (4u32, 2u32);
        // The first row decodes as red, magenta, black, …: (1, 0) reads magenta only when both the
        // red and the blue plane carry a sample there, so the assertion proves each channel lands
        // in its own slot.
        let red = [255u8, 255, 0, 255, 0, 255, 0, 255];
        let green = [0u8; 8];
        let blue = [0u8, 255, 0, 0, 0, 0, 0, 0];
        let alpha = [255u8; 8];
        let mut channel_data = Vec::new();
        for plane in [&alpha, &red, &green, &blue] {
            channel_data.extend_from_slice(&u16_bytes(0)); // compression 0, raw
            channel_data.extend_from_slice(plane);
        }

        let mut extra = Vec::new();
        extra.extend_from_slice(&u32_bytes(0)); // mask length
        extra.extend_from_slice(&u32_bytes(0)); // blending ranges length
        let name = layer_name.as_bytes();
        extra.push(name.len() as u8);
        extra.extend_from_slice(name);
        let pad = (4 - ((name.len() + 1) % 4)) % 4;
        extra.extend_from_slice(&vec![0u8; pad]);

        let mut record = Vec::new();
        record.extend_from_slice(&0i32.to_be_bytes()); // top
        record.extend_from_slice(&0i32.to_be_bytes()); // left
        record.extend_from_slice(&(height as i32).to_be_bytes()); // bottom
        record.extend_from_slice(&(width as i32).to_be_bytes()); // right
        record.extend_from_slice(&u16_bytes(4)); // channel count
        let channel_length = 2 + 8;
        for id in [-1i16, 0, 1, 2] {
            record.extend_from_slice(&id.to_be_bytes());
            if psb {
                record.extend_from_slice(&u64_bytes(channel_length as u64));
            } else {
                record.extend_from_slice(&u32_bytes(channel_length as u32));
            }
        }
        record.extend_from_slice(b"8BIM");
        record.extend_from_slice(b"norm");
        record.push(255); // opacity
        record.push(0); // clipping
        record.push(0); // flags
        record.push(0); // filler
        record.extend_from_slice(&u32_bytes(extra.len() as u32));
        record.extend_from_slice(&extra);

        let mut layer_info = Vec::new();
        layer_info.extend_from_slice(&u16_bytes(1)); // one layer record
        layer_info.extend_from_slice(&record);
        layer_info.extend_from_slice(&channel_data);
        let layer_info_length = layer_info.len() as u64;

        let mut section = Vec::new();
        if psb {
            section.extend_from_slice(&u64_bytes(layer_info_length));
        } else {
            section.extend_from_slice(&u32_bytes(layer_info_length as u32));
        }
        section.extend_from_slice(&layer_info);
        section.extend_from_slice(&u32_bytes(0)); // global layer mask info length

        let mut resources = Vec::new();
        if let Some(resolution) = resolution {
            resources.extend_from_slice(b"8BIM");
            resources.extend_from_slice(&u16_bytes(1005));
            resources.push(0); // empty name
            resources.push(0); // name padding
            resources.extend_from_slice(&u32_bytes(4));
            resources.extend_from_slice(&u32_bytes((resolution * 65536.0).round() as u32));
        }

        let mut file = Vec::new();
        file.extend_from_slice(b"8BPS");
        file.extend_from_slice(&u16_bytes(if psb { 2 } else { 1 }));
        file.extend_from_slice(&[0u8; 6]);
        file.extend_from_slice(&u16_bytes(4)); // channels
        file.extend_from_slice(&u32_bytes(height));
        file.extend_from_slice(&u32_bytes(width));
        file.extend_from_slice(&u16_bytes(8)); // depth
        file.extend_from_slice(&u16_bytes(3)); // RGB
        file.extend_from_slice(&u32_bytes(0)); // color mode data length
        file.extend_from_slice(&u32_bytes(resources.len() as u32));
        file.extend_from_slice(&resources);
        if psb {
            file.extend_from_slice(&u64_bytes(section.len() as u64));
        } else {
            file.extend_from_slice(&u32_bytes(section.len() as u32));
        }
        file.extend_from_slice(&section);
        file
    }

    #[test]
    fn a_synthetic_layer_record_parses_into_a_document() {
        let data = synthetic_psd("Layer", false, None);
        assert!(PSDReader::matches_data(&data));
        let document = PSDReader::read_data(&data, limits::document_pixel_budget()).expect("document");
        assert_eq!((document.width, document.height), (4, 2));
        assert_eq!(document.resolution, 72.0);
        assert_eq!(document.layers.len(), 1);
        let layer = &document.layers[0];
        assert_eq!(layer.name, "Layer");
        assert!(!layer.is_group);
        assert!(layer.is_visible);
        assert_eq!(layer.opacity, 1.0);
        assert_eq!(layer.blend_key, "norm");
        assert_eq!(layer.kind, PSDLayerKind::Raster);
        assert_eq!(layer.bounds, Rect::new(0.0, 0.0, 4.0, 2.0));
        assert_eq!(layer.blend_mode(), Some(compositor_rs_core::LayerBlendMode::Normal));
        let image = layer.image.as_ref().expect("pixels");
        assert_eq!((image.width(), image.height()), (4, 2));
        assert_eq!(image.get(0, 0), [255, 0, 0, 255]);
        assert_eq!(image.get(1, 0), [255, 0, 255, 255]);
        assert_eq!(image.get(2, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn psb_uses_eight_byte_lengths() {
        let data = synthetic_psd("Large", true, None);
        let document = PSDReader::read_data(&data, limits::document_pixel_budget()).expect("document");
        assert_eq!(document.layers.len(), 1);
        assert_eq!(document.layers[0].name, "Large");
        assert_eq!(document.layers[0].image.as_ref().expect("pixels").get(0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn the_resolution_resource_sets_the_document_dpi() {
        let data = synthetic_psd("Layer", false, Some(300.0));
        let document = PSDReader::read_data(&data, limits::document_pixel_budget()).expect("document");
        assert_eq!(document.resolution, 300.0);
        // A zero resolution resource falls back to 72.
        let zero = synthetic_psd("Layer", false, Some(0.0));
        let document = PSDReader::read_data(&zero, limits::document_pixel_budget()).expect("document");
        assert_eq!(document.resolution, 72.0);
    }

    #[test]
    fn a_file_without_layers_is_a_flat_canvas() {
        // A header whose layer section length is zero: the merged image is all there is.
        let mut file = Vec::new();
        file.extend_from_slice(b"8BPS");
        file.extend_from_slice(&u16_bytes(1));
        file.extend_from_slice(&[0u8; 6]);
        file.extend_from_slice(&u16_bytes(3));
        file.extend_from_slice(&u32_bytes(2));
        file.extend_from_slice(&u32_bytes(4));
        file.extend_from_slice(&u16_bytes(8));
        file.extend_from_slice(&u16_bytes(3));
        file.extend_from_slice(&u32_bytes(0));
        file.extend_from_slice(&u32_bytes(0));
        file.extend_from_slice(&u32_bytes(0));
        let document = PSDReader::read_data(&file, limits::document_pixel_budget()).expect("document");
        assert_eq!(document.layers.len(), 0);
        assert_eq!((document.width, document.height), (4, 2));
    }

    #[test]
    fn invalid_headers_are_rejected() {
        let mut data = synthetic_psd("Layer", false, None);
        let mut bad_magic = data.clone();
        bad_magic[0] = b'X';
        assert!(matches!(PSDReader::read_data(&bad_magic, 1_000_000), Err(PSDReadError::Import(ImageImportError::Unreadable))));

        let mut bad_version = data.clone();
        bad_version[4..6].copy_from_slice(&u16_bytes(3));
        assert!(matches!(PSDReader::read_data(&bad_version, 1_000_000), Err(PSDReadError::PSD(PSDError::UnsupportedVersion))));

        let mut bad_depth = data.clone();
        bad_depth[22..24].copy_from_slice(&u16_bytes(16));
        assert!(matches!(PSDReader::read_data(&bad_depth, 1_000_000), Err(PSDReadError::PSD(PSDError::UnsupportedDepth))));

        let mut bad_mode = data.clone();
        bad_mode[24..26].copy_from_slice(&u16_bytes(1));
        assert!(matches!(PSDReader::read_data(&bad_mode, 1_000_000), Err(PSDReadError::PSD(PSDError::UnsupportedColorMode))));

        // A file that stops inside the layer records is damaged, not empty.
        data.truncate(data.len() - 20);
        assert!(matches!(PSDReader::read_data(&data, 1_000_000), Err(PSDReadError::PSD(PSDError::Truncated))));

        // A budget of a single pixel cannot hold the 4×2 layer.
        let whole = synthetic_psd("Layer", false, None);
        assert!(matches!(PSDReader::read_data(&whole, 1), Err(PSDReadError::Import(ImageImportError::TooLarge))));
    }

    #[test]
    fn mac_roman_layer_names_decode() {
        assert_eq!(mac_roman(b"Layer"), "Layer");
        assert_eq!(mac_roman(&[0x8E, 0x87, 0x87]), "éáá");
        assert_eq!(mac_roman(&[0xD5]), "’");
    }

    #[test]
    fn adjustment_blocks_parse() {
        // 'levl': version, then four 10-byte channel records.
        let mut level = vec![0u8; 292];
        level[0..2].copy_from_slice(&u16_bytes(0));
        level[2..4].copy_from_slice(&u16_bytes(0)); // input black
        level[4..6].copy_from_slice(&u16_bytes(255)); // input white
        let adjustment = PSDAdjustments::levels(&level).expect("levels");
        assert_eq!(adjustment.kind, AdjustmentKind::Levels);
        assert_eq!(adjustment.levels.ranges[0].black, 0.0);
        assert_eq!(adjustment.levels.ranges[0].white, 255.0);
        assert!(PSDAdjustments::levels(&level[..100]).is_none());

        // 'curv': a pad byte, version 1, then one channel with a 3-point curve.
        let mut curves = Vec::new();
        curves.push(0); // the leading pad byte Photoshop writes
        curves.extend_from_slice(&u16_bytes(1));
        curves.extend_from_slice(&u16_bytes(1));
        curves.extend_from_slice(&u16_bytes(3));
        curves.extend_from_slice(&u16_bytes(0));
        curves.extend_from_slice(&u16_bytes(0));
        curves.extend_from_slice(&u16_bytes(128));
        curves.extend_from_slice(&u16_bytes(128));
        curves.extend_from_slice(&u16_bytes(255));
        curves.extend_from_slice(&u16_bytes(255));
        let adjustment = PSDAdjustments::curves(&curves).expect("curves");
        assert_eq!(adjustment.kind, AdjustmentKind::Curves);
        assert_eq!(adjustment.curves.channels[0].len(), 3);
        assert_eq!(adjustment.curves.channels[0][1].x, 128.0);

        // 'hue2': colorize off, Master at offset 10.
        let mut hue = vec![0u8; 16];
        hue[2] = 0;
        hue[10..12].copy_from_slice(&i16::to_be_bytes(-30));
        hue[12..14].copy_from_slice(&i16::to_be_bytes(20));
        hue[14..16].copy_from_slice(&i16::to_be_bytes(10));
        let adjustment = PSDAdjustments::hue(&hue).expect("hue");
        assert_eq!(adjustment.kind, AdjustmentKind::Hsv);
        let settings = adjustment.hsv_settings.as_ref().expect("hsv settings");
        let master = settings.adjustments.get(&ColorRange::Master).expect("master");
        assert_eq!((master.hue, master.saturation, master.lightness), (-30.0, 20.0, 10.0));

        // Colorize on reads its own values at offset 4.
        let mut colorized = vec![0u8; 16];
        colorized[2] = 1;
        colorized[4..6].copy_from_slice(&i16::to_be_bytes(180));
        let adjustment = PSDAdjustments::hue(&colorized).expect("colorized");
        assert_eq!(adjustment.hsv_settings.as_ref().unwrap().colorize, true);
        assert_eq!(adjustment.hsv_settings.as_ref().unwrap().adjustments.get(&ColorRange::Master).unwrap().hue, 180.0);
    }
}
