//! Ports of `CompositorTests/PSBImportTests.swift`, `PSDAdjustmentTests.swift`,
//! `PSDRoundTripTests.swift` and their `PSDFixture` (a tiny PSD writer that exists only for the
//! reader tests; Compositor does not write PSD). Test names keep the Swift ones.

use std::sync::Arc;

use compositor_core::buffer::{Gray8Image, Rgba8Image, SharedImage};
use compositor_core::document::ImageLayer;
use compositor_core::geom::{Point, Rect, Size};
use compositor_core::imported_image::{ImageImportError, PixelImage};
use compositor_core::layer_adjustment::{AdjustmentKind, ColorRange, HueBand, LevelRange, RangeAdjustment};
use compositor_core::layer_transform::LayerTransform;
use compositor_core::limits;
use compositor_core::{LayerBlendMode, PaletteColor};
use compositor_io::psd::builder::PSDDocumentBuilder;
use compositor_io::psd::reader::{PSDAdjustments, PSDReader};
use compositor_io::psd::types::{PSDDocument, PSDError, PSDLayerKind, PSDReadError, PSDRecord};
use compositor_io::psd::{text as psd_text, vector as psd_vector};
use rustc_hash::FxHashMap;
use uuid::Uuid;

// MARK: - The fixture writer

/// Big-endian byte builder, the `PSDBuffer` of the Swift fixture.
#[derive(Default)]
struct PsdBuffer(Vec<u8>);

impl PsdBuffer {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i16(&mut self, value: i16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn f64(&mut self, value: f64) {
        self.0.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }
    fn string(&mut self, value: &str) {
        self.0.extend_from_slice(value.as_bytes());
    }
    /// A descriptor class/key id: 4 bytes inline when the name is four characters, else length-prefixed.
    fn id(&mut self, value: &str) {
        let bytes = value.as_bytes();
        if bytes.len() == 4 {
            self.u32(0);
            self.bytes(bytes);
        } else {
            self.u32(bytes.len() as u32);
            self.bytes(bytes);
        }
    }
    fn utf16(&mut self, value: &str) {
        let units: Vec<u16> = value.encode_utf16().collect();
        self.u32(units.len() as u32);
        for unit in units {
            self.u16(unit);
        }
    }
    fn descriptor(&mut self, class_id: &str, items: &[(&str, Vec<u8>)]) {
        self.u32(16);
        self.u32(0);
        self.id(class_id);
        self.u32(items.len() as u32);
        for (key, value) in items {
            self.id(key);
            self.bytes(value);
        }
    }
}

/// `PSDFixture.data(_:composite:largeDocument:additionalLayerInfo:extras:)`.
fn fixture_data(
    document: &PSDDocument,
    large_document: bool,
    additional_layer_info: Option<(&str, Vec<u8>)>,
    extras: &[(Uuid, Vec<(&str, Vec<u8>)>)],
) -> Vec<u8> {
    let (width, height) = (document.width, document.height);
    assert!((1..=30_000).contains(&width) && (1..=30_000).contains(&height));
    let mut file = PsdBuffer::default();
    file.string("8BPS");
    file.u16(if large_document { 2 } else { 1 });
    file.bytes(&[0u8; 6]);
    file.u16(4);
    file.u32(height as u32);
    file.u32(width as u32);
    file.u16(8);
    file.u16(3);
    file.u32(0); // color mode data
    let resources = resolution_resource(document.resolution);
    file.u32(resources.len() as u32);
    file.bytes(&resources);
    let layers = layer_section(document, large_document, additional_layer_info, extras);
    if large_document {
        file.u64(layers.len() as u64);
    } else {
        file.u32(layers.len() as u32);
    }
    file.bytes(&layers);
    insert_composite(&mut file, width, height);
    file.0
}

/// The resolution resource (id 1005); the reader reads the first u32 as pixels per inch.
fn resolution_resource(resolution: f64) -> Vec<u8> {
    let mut resource = PsdBuffer::default();
    resource.string("8BIM");
    resource.u16(1005);
    resource.u8(0);
    resource.u8(0);
    resource.u32(16);
    let fixed = (resolution.clamp(1.0, 9600.0) * 65536.0).round() as u32;
    resource.u32(fixed);
    resource.u16(1);
    resource.u16(1);
    resource.u32(fixed);
    resource.u16(1);
    resource.u16(1);
    resource.0
}

/// A merged image section (`compression 1`): the reader stops before it, but a Photoshop file has one.
///
/// The Swift fixture flattened the composite `CGImage`; the merged pixels are irrelevant to the reader,
/// so this writes the document's transparent black planes.
fn insert_composite(file: &mut PsdBuffer, width: usize, height: usize) {
    file.u16(1);
    let plane = vec![0u8; width * height];
    let encoded = encode_plane(&plane, width, height, false);
    let count_bytes = height * 2;
    for _ in 0..4 {
        file.bytes(&encoded[..count_bytes]);
    }
    for _ in 0..4 {
        file.bytes(&encoded[count_bytes..]);
    }
}

struct Prepared {
    record: PSDRecord,
    is_divider: bool,
    channels: Vec<(i16, Vec<u8>)>,
    top: i32,
    left: i32,
    bottom: i32,
    right: i32,
    mask_top: i32,
    mask_left: i32,
    mask_bottom: i32,
    mask_right: i32,
    extras: Vec<(String, Vec<u8>)>,
}

/// Photoshop order: a type-3 divider, the children, then the folder (type 1).
fn layer_section(
    document: &PSDDocument,
    large_document: bool,
    additional_layer_info: Option<(&str, Vec<u8>)>,
    extras: &[(Uuid, Vec<(&str, Vec<u8>)>)],
) -> Vec<u8> {
    fn emit(
        document: &PSDDocument,
        parent: Option<Uuid>,
        large_document: bool,
        extras: &[(Uuid, Vec<(&str, Vec<u8>)>)],
        prepared: &mut Vec<Prepared>,
    ) {
        for record in document.layers.iter().filter(|record| record.parent_id == parent) {
            if record.is_group {
                prepared.push(empty_layer("</Layer group>", "norm", 3, true, 1.0, parent, None, None, true, large_document));
                emit(document, Some(record.id), large_document, extras, prepared);
                let blend_key = if record.blend_key == "pass" { "pass" } else { record.blend_key.as_str() };
                prepared.push(empty_layer(
                    &record.name,
                    blend_key,
                    1,
                    record.is_visible,
                    record.opacity,
                    parent,
                    Some(record.id),
                    record.mask.as_deref(),
                    record.mask_enabled,
                    large_document,
                ));
            } else {
                let extra: Vec<(String, Vec<u8>)> = extras
                    .iter()
                    .find(|(id, _)| *id == record.id)
                    .map(|(_, blocks)| blocks.iter().map(|(key, value)| ((*key).to_string(), value.clone())).collect())
                    .unwrap_or_default();
                prepared.push(prepare_layer(record, large_document, extra));
            }
        }
    }

    let mut prepared: Vec<Prepared> = Vec::new();
    emit(document, None, large_document, extras, &mut prepared);
    assert!(prepared.len() <= i16::MAX as usize);

    let mut records = PsdBuffer::default();
    records.i16(prepared.len() as i16);
    let mut payloads = PsdBuffer::default();
    for item in &prepared {
        write_record(&mut records, item, large_document, additional_layer_info.clone());
        for (_, payload) in &item.channels {
            payloads.bytes(payload);
        }
    }
    let mut info = PsdBuffer::default();
    if large_document {
        info.u64(0);
    } else {
        info.u32(0);
    }
    info.bytes(&records.0);
    info.bytes(&payloads.0);
    if info.0.len() % 2 == 1 {
        info.u8(0);
    }
    let length_field_bytes = if large_document { 8 } else { 4 };
    let layer_bytes = info.0.len() - length_field_bytes;
    let mut length = PsdBuffer::default();
    if large_document {
        length.u64(layer_bytes as u64);
    } else {
        length.u32(layer_bytes as u32);
    }
    info.0.splice(0..length_field_bytes, length.0);
    let mut section = PsdBuffer::default();
    section.bytes(&info.0);
    section.u32(0); // global layer mask info
    section.0
}

fn prepare_layer(record: &PSDRecord, large_document: bool, extras: Vec<(String, Vec<u8>)>) -> Prepared {
    let width = record.image.as_ref().map(|image| image.width()).unwrap_or(0);
    let height = record.image.as_ref().map(|image| image.height()).unwrap_or(0);
    let left = record.bounds.min_x().round() as i32;
    let top = record.bounds.min_y().round() as i32;
    let mut channels: Vec<(i16, Vec<u8>)> = Vec::new();
    if width > 0 && height > 0 {
        let image = record.image.as_ref().expect("image");
        let planes = planes_from(image);
        for (id, plane) in [(-1i16, &planes.3), (0, &planes.0), (1, &planes.1), (2, &planes.2)] {
            channels.push((id, channel_payload(plane, width, height, large_document)));
        }
    } else {
        channels = empty_channels();
    }
    let mut mask_bottom = top;
    let mut mask_right = left;
    if let Some(mask) = &record.mask {
        let plane = gray_plane(mask);
        channels.push((-2, channel_payload(&plane, mask.width(), mask.height(), large_document)));
        mask_bottom = top + mask.height() as i32;
        mask_right = left + mask.width() as i32;
    }
    Prepared {
        record: record.clone(),
        is_divider: false,
        channels,
        top,
        left,
        bottom: top + height as i32,
        right: left + width as i32,
        mask_top: top,
        mask_left: left,
        mask_bottom,
        mask_right,
        extras,
    }
}

#[allow(clippy::too_many_arguments)]
fn empty_layer(
    name: &str,
    blend_key: &str,
    section: i32,
    visible: bool,
    opacity: f64,
    parent: Option<Uuid>,
    id: Option<Uuid>,
    mask: Option<&Gray8Image>,
    mask_enabled: bool,
    large_document: bool,
) -> Prepared {
    let mut record = PSDRecord::new(id.unwrap_or_else(Uuid::new_v4), name);
    record.parent_id = parent;
    record.is_group = section != 3;
    record.is_visible = visible;
    record.opacity = opacity;
    record.blend_key = blend_key.to_string();
    record.kind = PSDLayerKind::Group;
    let mut channels = empty_channels();
    let mut mask_bottom = 0;
    let mut mask_right = 0;
    if let Some(mask) = mask {
        let plane = gray_plane(mask);
        channels.push((-2, channel_payload(&plane, mask.width(), mask.height(), large_document)));
        mask_bottom = mask.height() as i32;
        mask_right = mask.width() as i32;
        record.mask = Some(Arc::new(mask.clone()));
        record.mask_enabled = mask_enabled;
        record.mask_bounds = Rect::new(0.0, 0.0, mask_right as f64, mask_bottom as f64);
    }
    Prepared {
        record,
        is_divider: section == 3,
        channels,
        top: 0,
        left: 0,
        bottom: 0,
        right: 0,
        mask_top: 0,
        mask_left: 0,
        mask_bottom,
        mask_right,
        extras: Vec::new(),
    }
}

fn empty_channels() -> Vec<(i16, Vec<u8>)> {
    [(-1i16), 0, 1, 2].iter().map(|id| (*id, vec![0, 0])).collect()
}

fn channel_payload(plane: &[u8], width: usize, height: usize, large_document: bool) -> Vec<u8> {
    let encoded = encode_plane(plane, width, height, large_document);
    let mut data = vec![(1u16 >> 8) as u8, (1u16 & 0xff) as u8];
    data.extend_from_slice(&encoded);
    data
}

fn write_record(buffer: &mut PsdBuffer, item: &Prepared, large_document: bool, additional_layer_info: Option<(&str, Vec<u8>)>) {
    let record = &item.record;
    buffer.i32(item.top);
    buffer.i32(item.left);
    buffer.i32(item.bottom);
    buffer.i32(item.right);
    buffer.u16(item.channels.len() as u16);
    for (id, payload) in &item.channels {
        buffer.i16(*id);
        if large_document {
            buffer.u64(payload.len() as u64);
        } else {
            buffer.u32(payload.len() as u32);
        }
    }
    buffer.string("8BIM");
    let blend = format!("{:<4}", &record.blend_key);
    buffer.string(&blend[..4]);
    buffer.u8((record.opacity * 255.0).round() as u8);
    buffer.u8(if record.clipping { 1 } else { 0 });
    buffer.u8(if record.is_visible { 0 } else { 2 });
    buffer.u8(0);
    let extra = extra_data(item, large_document, additional_layer_info);
    buffer.u32(extra.len() as u32);
    buffer.bytes(&extra);
}

fn extra_data(item: &Prepared, large_document: bool, additional_layer_info: Option<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut extra = PsdBuffer::default();
    if item.record.mask.is_some() && item.mask_right > item.mask_left && item.mask_bottom > item.mask_top {
        extra.u32(20);
        extra.i32(item.mask_top);
        extra.i32(item.mask_left);
        extra.i32(item.mask_bottom);
        extra.i32(item.mask_right);
        extra.u8(255);
        let mut flags: u8 = if item.record.mask_linked { 0 } else { 1 };
        if !item.record.mask_enabled {
            flags |= 2;
        }
        extra.u8(flags);
        extra.u16(0);
    } else {
        extra.u32(0);
    }
    extra.u32(0); // blending ranges
    let pascal: Vec<u8> = item.record.name.as_bytes().iter().take(255).copied().collect();
    extra.u8(pascal.len() as u8);
    extra.bytes(&pascal);
    let name_bytes = 1 + pascal.len();
    let pad = (4 - (name_bytes % 4)) % 4;
    extra.bytes(&vec![0u8; pad]);
    if let Some((key, payload)) = &additional_layer_info {
        write_additional(&mut extra, key, payload, large_document);
    }
    write_additional(&mut extra, "luni", &luni(&item.record.name), large_document);
    if item.record.is_group || item.is_divider {
        let section: u32 = if item.is_divider { 3 } else { 1 };
        let blend_base = if item.is_divider {
            "norm".to_string()
        } else if item.record.blend_key == "pass" {
            "pass".to_string()
        } else {
            item.record.blend_key.clone()
        };
        let blend = format!("{:<4}", blend_base);
        let mut payload = vec![0, 0, 0, section as u8];
        payload.extend_from_slice(b"8BIM");
        payload.extend_from_slice(&blend.as_bytes()[..4]);
        write_additional(&mut extra, "lsct", &payload, large_document);
    }
    let mut sorted: Vec<&(String, Vec<u8>)> = item.extras.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, payload) in sorted {
        write_additional(&mut extra, key, payload, large_document);
    }
    extra.0
}

fn write_additional(buffer: &mut PsdBuffer, key: &str, payload: &[u8], large_document: bool) {
    buffer.string("8BIM");
    buffer.string(key);
    if large_document && PSB_LARGE_ADDITIONAL_INFO_KEYS.contains(&key) {
        buffer.u64(payload.len() as u64);
    } else {
        buffer.u32(payload.len() as u32);
    }
    buffer.bytes(payload);
    if payload.len() % 2 == 1 {
        buffer.u8(0);
    }
}

const PSB_LARGE_ADDITIONAL_INFO_KEYS: [&str; 13] = [
    "LMsk", "Lr16", "Lr32", "Layr", "Mt16", "Mt32", "Mtrn", "Alph", "FMsk", "lnk2", "FEid", "FXid", "PxSD",
];

fn luni(name: &str) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    data.utf16(name);
    data.0
}

/// PackBits, compression 1, with the row counts the reader expects.
fn encode_plane(plane: &[u8], width: usize, height: usize, large_document: bool) -> Vec<u8> {
    if width == 0 || height == 0 || plane.len() < width * height {
        return Vec::new();
    }
    let mut counts = PsdBuffer::default();
    let mut packed = PsdBuffer::default();
    for row in 0..height {
        let encoded = pack_bits(&plane[row * width..(row + 1) * width]);
        if large_document {
            counts.u32(encoded.len() as u32);
        } else {
            counts.u16(encoded.len() as u16);
        }
        packed.bytes(&encoded);
    }
    let mut data = counts.0;
    data.extend_from_slice(&packed.0);
    data
}

fn pack_bits(row: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < row.len() {
        if index + 1 < row.len() && row[index] == row[index + 1] {
            let mut run = 2;
            while index + run < row.len() && row[index + run] == row[index] && run < 128 {
                run += 1;
            }
            output.push((1 - run as i32) as u8);
            output.push(row[index]);
            index += run;
        } else {
            let start = index;
            index += 1;
            while index < row.len() && index - start < 128 {
                if index + 1 < row.len() && row[index] == row[index + 1] {
                    break;
                }
                index += 1;
            }
            output.push((index - start - 1) as u8);
            output.extend_from_slice(&row[start..index]);
        }
    }
    output
}

/// Premultiplied RGBA8 planes as the straight channel data Photoshop stores.
fn planes_from(image: &Rgba8Image) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let count = image.width() * image.height();
    let mut red = vec![0u8; count];
    let mut green = vec![0u8; count];
    let mut blue = vec![0u8; count];
    let mut alpha = vec![0u8; count];
    for (index, pixel) in image.data().chunks_exact(4).enumerate() {
        let a = pixel[3];
        alpha[index] = a;
        if a == 0 {
            continue;
        }
        red[index] = ((pixel[0] as u32 * 255 + a as u32 / 2) / a as u32) as u8;
        green[index] = ((pixel[1] as u32 * 255 + a as u32 / 2) / a as u32) as u8;
        blue[index] = ((pixel[2] as u32 * 255 + a as u32 / 2) / a as u32) as u8;
    }
    (red, green, blue, alpha)
}

fn gray_plane(image: &Gray8Image) -> Vec<u8> {
    image.data().to_vec()
}

// MARK: - The fixture's Photoshop extra blocks

/// `PSDFixture.tySh`: a Photoshop 6 type-tool block, laid out as Adobe's Type Tool Object Setting.
#[derive(Clone)]
struct TySh {
    text: String,
    font: String,
    font_size: f64,
    red: f64,
    green: f64,
    blue: f64,
    justification: i32,
    tracking: f64,
    leading: Option<f64>,
    faux_bold: bool,
    faux_italic: bool,
    vertical: bool,
    warp: bool,
    second_size: Option<f64>,
    second_leading: Option<f64>,
    second_horizontal_scale: Option<f64>,
    second_vertical_scale: Option<f64>,
    tx: f64,
    ty: f64,
    xx: f64,
    xy: f64,
    yx: f64,
    yy: f64,
    bounds: Option<(f64, f64, f64, f64)>,
    glyph_bounds: Option<(f64, f64, f64, f64)>,
}

impl Default for TySh {
    fn default() -> Self {
        TySh {
            text: "Hello".to_string(),
            font: "Helvetica".to_string(),
            font_size: 24.0,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            justification: 0,
            tracking: 0.0,
            leading: None,
            faux_bold: false,
            faux_italic: false,
            vertical: false,
            warp: false,
            second_size: None,
            second_leading: None,
            second_horizontal_scale: None,
            second_vertical_scale: None,
            tx: 40.0,
            ty: 50.0,
            xx: 1.0,
            xy: 0.0,
            yx: 0.0,
            yy: 1.0,
            bounds: None,
            glyph_bounds: None,
        }
    }
}

impl TySh {
    fn text(mut self, text: &str) -> Self {
        self.text = text.to_string();
        self
    }
    fn font(mut self, font: &str) -> Self {
        self.font = font.to_string();
        self
    }
    fn font_size(mut self, size: f64) -> Self {
        self.font_size = size;
        self
    }
    /// `PSDFixture.tySh`'s `red:green:blue:` arguments, the engine's FillColor values.
    fn color(mut self, red: f64, green: f64, blue: f64) -> Self {
        self.red = red;
        self.green = green;
        self.blue = blue;
        self
    }
    fn justification(mut self, value: i32) -> Self {
        self.justification = value;
        self
    }
    fn tracking(mut self, value: f64) -> Self {
        self.tracking = value;
        self
    }
    fn leading(mut self, value: f64) -> Self {
        self.leading = Some(value);
        self
    }
    fn faux(mut self, bold: bool, italic: bool) -> Self {
        self.faux_bold = bold;
        self.faux_italic = italic;
        self
    }
    fn vertical(mut self) -> Self {
        self.vertical = true;
        self
    }
    fn warp(mut self) -> Self {
        self.warp = true;
        self
    }
    fn second_size(mut self, size: f64) -> Self {
        self.second_size = Some(size);
        self
    }
    fn second_leading(mut self, value: f64) -> Self {
        self.second_leading = Some(value);
        self
    }
    fn second_horizontal_scale(mut self, value: f64) -> Self {
        self.second_horizontal_scale = Some(value);
        self
    }
    fn bounds(mut self, bounds: (f64, f64, f64, f64), glyph_bounds: (f64, f64, f64, f64)) -> Self {
        self.bounds = Some(bounds);
        self.glyph_bounds = Some(glyph_bounds);
        self
    }
    fn translate_to(mut self, tx: f64, ty: f64) -> Self {
        self.tx = tx;
        self.ty = ty;
        self
    }
    fn matrix(mut self, xx: f64, xy: f64, yx: f64, yy: f64) -> Self {
        self.xx = xx;
        self.xy = xy;
        self.yx = yx;
        self.yy = yy;
        self
    }

    fn data(&self) -> Vec<u8> {
        let mut block = PsdBuffer::default();
        block.u16(1);
        for value in [self.xx, self.xy, self.yx, self.yy, self.tx, self.ty] {
            block.f64(value);
        }
        block.u16(50);
        let mut items: Vec<(&str, Vec<u8>)> = vec![
            ("Txt ", text_item(&self.text)),
            ("Ornt", enum_item("Ornt", if self.vertical { "Vrtc" } else { "Hrzn" })),
        ];
        if let Some(bounds) = self.bounds {
            items.push(("bounds", rect_item(bounds)));
        }
        if let Some(bounds) = self.glyph_bounds {
            items.push(("boundingBox", rect_item(bounds)));
        }
        items.push(("EngineData", raw_item(self.engine().as_bytes())));
        block.descriptor("TxLr", &items);
        block.u16(1);
        block.descriptor("warp", &[("warpStyle", enum_item("warpStyle", if self.warp { "warpArc" } else { "warpNone" }))]);
        block.0
    }

    fn engine(&self) -> String {
        fn run(style: &TySh, size: f64, leading: Option<f64>, horizontal: f64, vertical: f64) -> String {
            format!(
                "<<\n/StyleSheet\n<<\n/StyleSheetData\n<<\n/Font 0\n/FontSize {size}\n/FauxBold {bold}\n/FauxItalic {italic}\n/AutoLeading {auto}\n/Leading {leading}\n/Tracking {tracking}\n/HorizontalScale {horizontal}\n/VerticalScale {vertical}\n/FillColor\n<<\n/Type 1\n/Values [ 1.0 {red} {green} {blue} ]\n>>\n>>\n>>\n>>",
                size = size,
                bold = style.faux_bold,
                italic = style.faux_italic,
                auto = leading.is_none(),
                leading = leading.unwrap_or(size * 1.2),
                tracking = style.tracking,
                horizontal = horizontal,
                vertical = vertical,
                red = style.red,
                green = style.green,
                blue = style.blue,
            )
        }
        let has_second = self.second_size.is_some()
            || self.second_leading.is_some()
            || self.second_horizontal_scale.is_some()
            || self.second_vertical_scale.is_some();
        let first = run(self, self.font_size, self.leading, 1.0, 1.0);
        let runs = if has_second {
            format!(
                "{}\n{}",
                first,
                run(
                    self,
                    self.second_size.unwrap_or(self.font_size),
                    self.second_leading.or(self.leading),
                    self.second_horizontal_scale.unwrap_or(1.0),
                    self.second_vertical_scale.unwrap_or(1.0),
                )
            )
        } else {
            first
        };
        format!(
            "\n<<\n/EngineDict\n<<\n/Editor\n<<\n/Text {text}\n>>\n/ParagraphRun\n<<\n/RunArray\n[\n<<\n/ParagraphSheet\n<<\n/Properties\n<<\n/Justification {justification}\n>>\n>>\n>>\n]\n>>\n/StyleRun\n<<\n/RunArray\n[\n{runs}\n]\n>>\n>>\n/ResourceDict\n<<\n/FontSet\n[\n<<\n/Name {font}\n>>\n]\n>>\n>>",
            text = parenthesized(&self.text),
            justification = self.justification,
            runs = runs,
            font = parenthesized(&self.font),
        )
    }
}

/// A PostScript string literal: backslashes and parentheses escaped.
fn parenthesized(text: &str) -> String {
    let mut encoded = String::from("(");
    for byte in text.bytes() {
        if byte == b'\\' || byte == b'(' || byte == b')' {
            encoded.push('\\');
        }
        encoded.push(byte as char);
    }
    encoded.push(')');
    encoded
}

fn text_item(text: &str) -> Vec<u8> {
    let mut item = PsdBuffer::default();
    item.string("TEXT");
    item.utf16(text);
    item.0
}

fn enum_item(item_type: &str, value: &str) -> Vec<u8> {
    let mut item = PsdBuffer::default();
    item.string("enum");
    item.id(item_type);
    item.id(value);
    item.0
}

fn raw_item(payload: &[u8]) -> Vec<u8> {
    let mut item = PsdBuffer::default();
    item.string("tdta");
    item.u32(payload.len() as u32);
    item.bytes(payload);
    item.0
}

fn rect_item(bounds: (f64, f64, f64, f64)) -> Vec<u8> {
    let mut item = PsdBuffer::default();
    item.string("Objc");
    item.u32(0);
    item.id("Rctn");
    item.u32(4);
    for (key, value) in [("Left", bounds.0), ("Top ", bounds.1), ("Rght", bounds.2), ("Btom", bounds.3)] {
        item.id(key);
        item.string("UntF");
        item.string("#Pnt");
        item.f64(value);
    }
    item.0
}

/// `PSDVectorFixtures.circle()`: a Photoshop-exported `vmsk`/`SoCo`/`vstk` set.
fn circle_extra() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "vmsk",
            base64(
                "AAAAAwAAAAAABgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAABAAEAAAAAAAAAAAAAAAAAAAAAAAAAAQDHWSAAXn4FAMdZIABSZmYAx1kgAEZOxwABALX4sAA8iIgAoHlcADyIiACK+gkAPIiIAAEAeZmZAEZOxwB5mZkAUmZmAHmZmQBefgUAAQCK+gkAaEREAKB5XABoREQAtfiwAGhERAAA",
            ),
        ),
        (
            "SoCo",
            base64(
                "AAAAEAAAAAEAAAAAAABudWxsAAAAAQAAAABDbHIgT2JqYwAAAAEAAAAAAABSR0JDAAAAAwAAAABSZCAgZG91YgAAAAAAAAAAAAAAAEdybiBkb3ViQFuAAAAAAAAAAAAAQmwgIGRvdWJAb+AAAAAAAA==",
            ),
        ),
        ("vstk", base64("AAAAEAAAAAEAAAAAAAtzdHJva2VTdHlsZQAAABAAAAASc3Ryb2tlU3R5bGVWZXJzaW9ubG9uZwAAAAIAAAANc3Ryb2tlRW5hYmxlZGJvb2wAAAAAC2ZpbGxFbmFibGVkYm9vbAEAAAAUc3Ryb2tlU3R5bGVMaW5lV2lkdGhVbnRGI1B4bEAjvQfpGNNeAAAAGXN0cm9rZVN0eWxlTGluZURhc2hPZmZzZXRVbnRGI1BudAAAAAAAAAAAAAAAFXN0cm9rZVN0eWxlTWl0ZXJMaW1pdGRvdWJAWQAAAAAAAAAAABZzdHJva2VTdHlsZUxpbmVDYXBUeXBlZW51bQAAABZzdHJva2VTdHlsZUxpbmVDYXBUeXBlAAAAEnN0cm9rZVN0eWxlQnV0dENhcAAAABdzdHJva2VTdHlsZUxpbmVKb2luVHlwZWVudW0AAAAXc3Ryb2tlU3R5bGVMaW5lSm9pblR5cGUAAAAUc3Ryb2tlU3R5bGVNaXRlckpvaW4AAAAYc3Ryb2tlU3R5bGVMaW5lQWxpZ25tZW50ZW51bQAAABhzdHJva2VTdHlsZUxpbmVBbGlnbm1lbnQAAAAWc3Ryb2tlU3R5bGVBbGlnbkNlbnRlcgAAABRzdHJva2VTdHlsZVNjYWxlTG9ja2Jvb2wAAAAAF3N0cm9rZVN0eWxlU3Ryb2tlQWRqdXN0Ym9vbAAAAAAWc3Ryb2tlU3R5bGVMaW5lRGFzaFNldFZsTHMAAAAAAAAAFHN0cm9rZVN0eWxlQmxlbmRNb2RlZW51bQAAAABCbG5NAAAAAE5ybWwAAAASc3Ryb2tlU3R5bGVPcGFjaXR5VW50RiNQcmNAWQAAAAAAAAAAABJzdHJva2VTdHlsZUNvbnRlbnRPYmpjAAAAAQAAAAAAD3NvbGlkQ29sb3JMYXllcgAAAAEAAAAAQ2xyIE9iamMAAAABAAAAAAAAUkdCQwAAAAMAAAAAUmQgIGRvdWJAb+AAAAAAAAAAAABHcm4gZG91YkBv4AAAAAAAAAAAAEJsICBkb3ViAAAAAAAAAAAAAAAVc3Ryb2tlU3R5bGVSZXNvbHV0aW9uZG91YkBSAAAAAAAA")),
    ]
}

/// `PSDVectorFixtures.rectangle()`: a stroked, dark-blue rectangle export.
fn rectangle_extra() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "vmsk",
            base64(
                "AAAAAwAAAAAABgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAABAAEAAAAAAAAAAAAAAAAAAAAAAAAAAQAkREQA1CIiACRERADUIiIAJEREANQiIgABAE9oSwDUIiIAT2hLANQiIgBPaEsA1CIiAAEAT2hLAH4AAABPaEsAfgAAAE9oSwB+AAAAAQAkREQAfgAAACRERAB+AAAAJEREAH4AAAAA",
            ),
        ),
        (
            "SoCo",
            base64("AAAAEAAAAAEAAAAAAABudWxsAAAAAQAAAABDbHIgT2JqYwAAAAEAAAAAAABSR0JDAAAAAwAAAABSZCAgZG91YgAAAAAAAAAAAAAAAEdybiBkb3ViAAAAAAAAAAAAAAAAQmwgIGRvdWJAAAAAAAAAAA=="),
        ),
        ("vstk", base64("AAAAEAAAAAEAAAAAAAtzdHJva2VTdHlsZQAAABAAAAASc3Ryb2tlU3R5bGVWZXJzaW9ubG9uZwAAAAIAAAANc3Ryb2tlRW5hYmxlZGJvb2wBAAAAC2ZpbGxFbmFibGVkYm9vbAEAAAAUc3Ryb2tlU3R5bGVMaW5lV2lkdGhVbnRGI1B4bEAjvQfpGNNeAAAAGXN0cm9rZVN0eWxlTGluZURhc2hPZmZzZXRVbnRGI1BudAAAAAAAAAAAAAAAFXN0cm9rZVN0eWxlTWl0ZXJMaW1pdGRvdWJAWQAAAAAAAAAAABZzdHJva2VTdHlsZUxpbmVDYXBUeXBlZW51bQAAABZzdHJva2VTdHlsZUxpbmVDYXBUeXBlAAAAEnN0cm9rZVN0eWxlQnV0dENhcAAAABdzdHJva2VTdHlsZUxpbmVKb2luVHlwZWVudW0AAAAXc3Ryb2tlU3R5bGVMaW5lSm9pblR5cGUAAAAUc3Ryb2tlU3R5bGVNaXRlckpvaW4AAAAYc3Ryb2tlU3R5bGVMaW5lQWxpZ25tZW50ZW51bQAAABhzdHJva2VTdHlsZUxpbmVBbGlnbm1lbnQAAAAWc3Ryb2tlU3R5bGVBbGlnbkNlbnRlcgAAABRzdHJva2VTdHlsZVNjYWxlTG9ja2Jvb2wAAAAAF3N0cm9rZVN0eWxlU3Ryb2tlQWRqdXN0Ym9vbAAAAAAWc3Ryb2tlU3R5bGVMaW5lRGFzaFNldFZsTHMAAAAAAAAAFHN0cm9rZVN0eWxlQmxlbmRNb2RlZW51bQAAAABCbG5NAAAAAE5ybWwAAAASc3Ryb2tlU3R5bGVPcGFjaXR5VW50RiNQcmNAWQAAAAAAAAAAABJzdHJva2VTdHlsZUNvbnRlbnRPYmpjAAAAAQAAAAAAD3NvbGlkQ29sb3JMYXllcgAAAAEAAAAAQ2xyIE9iamMAAAABAAAAAAAAUkdCQwAAAAMAAAAAUmQgIGRvdWJAb+AAAAAAAAAAAABHcm4gZG91YkBv4AAAAAAAAAAAAEJsICBkb3ViAAAAAAAAAAAAAAAVc3Ryb2tlU3R5bGVSZXNvbHV0aW9uZG91YkBSAAAAAAAA")),
    ]
}

/// The base64 alphabet the fixtures' blobs use, decoded by hand so the port needs no new dependency.
fn base64(text: &str) -> Vec<u8> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut output = Vec::new();
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        let Some(four) = value(byte) else { continue };
        accumulator = accumulator << 6 | four;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
        }
    }
    output
}

/// `originationData(type:rect:radii:)`: a `vogk` block.
fn origination_data(origin_type: u32, rect: (f64, f64, f64, f64), radii: &[f64]) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    data.string("keyOriginType");
    data.string("long");
    data.u32(origin_type);
    data.string("keyOriginShapeBBox");
    for (key, value) in [("Left", rect.0), ("Top ", rect.1), ("Rght", rect.2), ("Btom", rect.3)] {
        data.string(key);
        data.string("UntF");
        data.string("#Pxl");
        data.f64(value);
    }
    if radii.len() == 4 {
        data.string("keyOriginRRectRadii");
        for (key, value) in ["topLeft", "topRight", "bottomRight", "bottomLeft"].iter().zip(radii) {
            data.string(key);
            data.string("UntF");
            data.string("#Pxl");
            data.f64(*value);
        }
    }
    data.0
}

/// A `vmsk` with `corners` as sharp, corner-to-corner points on `canvas`.
fn vector_mask(canvas: (f64, f64), corners: &[(f64, f64)]) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    data.u32(3);
    data.u32(0);
    data.i16(6);
    data.bytes(&[0u8; 24]);
    data.i16(8);
    data.bytes(&[0u8; 24]);
    data.i16(0);
    data.i16(corners.len() as i16);
    data.bytes(&[0u8; 22]);
    for (x, y) in corners {
        data.i16(1);
        let point = |x: f64, y: f64| {
            let y = (y / canvas.1) * 0x1000000 as f64;
            let x = (x / canvas.0) * 0x1000000 as f64;
            (x as i32, y as i32)
        };
        for (px, py) in [point(*x, *y), point(*x, *y), point(*x, *y)] {
            data.i32(py);
            data.i32(px);
        }
    }
    data.0
}

fn color_descriptor(red: f64, green: f64, blue: f64) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    data.string("RGBC");
    for (key, value) in [("Rd  ", red), ("Grn ", green), ("Bl  ", blue)] {
        data.string(key);
        data.string("doub");
        data.f64(value);
    }
    data.0
}

fn solid_color(red: f64, green: f64, blue: f64) -> Vec<u8> {
    color_descriptor(red, green, blue)
}

fn stroke_style(fill: bool, stroke: bool, width: f64, red: f64, green: f64, blue: f64) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    for (key, value) in [("strokeEnabled", stroke), ("fillEnabled", fill)] {
        data.string(key);
        data.string("bool");
        data.u8(if value { 1 } else { 0 });
    }
    data.string("strokeStyleLineWidth");
    data.string("UntF");
    data.string("#Pxl");
    data.f64(width);
    data.bytes(&color_descriptor(red, green, blue));
    data.0
}

// MARK: - Small hand-built files

fn header(version: u16, width: u32, height: u32, depth: u16, mode: u16) -> Vec<u8> {
    let mut data = PsdBuffer::default();
    data.string("8BPS");
    data.u16(version);
    data.bytes(&[0u8; 6]);
    data.u16(3);
    data.u32(height);
    data.u32(width);
    data.u16(depth);
    data.u16(mode);
    data.0
}

fn raw_channel(plane: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8, 0];
    data.extend_from_slice(plane);
    data
}

fn oversized_layer_file(width: u32, height: u32, layer_width: i32, layer_height: i32) -> Vec<u8> {
    layer_file(
        width,
        height,
        layer_width,
        layer_height,
        &[(-1i16, vec![0, 0]), (0, vec![0, 0]), (1, vec![0, 0]), (2, vec![0, 0])],
    )
}

fn layer_file(canvas_width: u32, canvas_height: u32, layer_width: i32, layer_height: i32, channels: &[(i16, Vec<u8>)]) -> Vec<u8> {
    let mut data = header(1, canvas_width, canvas_height, 8, 3);
    data.extend_from_slice(&[0, 0, 0, 0]); // color mode data
    data.extend_from_slice(&[0, 0, 0, 0]); // image resources
    let mut records = PsdBuffer::default();
    records.i16(1);
    records.i32(0);
    records.i32(0);
    records.i32(layer_height);
    records.i32(layer_width);
    records.u16(channels.len() as u16);
    let mut payloads = PsdBuffer::default();
    for (id, payload) in channels {
        records.i16(*id);
        records.u32(payload.len() as u32);
        payloads.bytes(payload);
    }
    records.bytes(b"8BIMnorm");
    records.bytes(&[255, 0, 0, 0]);
    records.u32(12);
    records.u32(0);
    records.u32(0);
    records.u8(3);
    records.bytes(b"Big");
    let mut info = PsdBuffer::default();
    info.u32((records.0.len() + payloads.0.len()) as u32);
    info.bytes(&records.0);
    info.bytes(&payloads.0);
    info.u32(0);
    data.extend_from_slice(&(info.0.len() as u32).to_be_bytes());
    data.extend_from_slice(&info.0);
    data
}

fn lsct_types(data: &[u8]) -> Vec<u32> {
    let mut types = Vec::new();
    let mut search = 0usize;
    while let Some(position) = data[search..].windows(4).position(|window| window == b"lsct") {
        let at = search + position + 4;
        if at + 8 > data.len() {
            break;
        }
        types.push(u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]));
        search = at;
    }
    types
}

// MARK: - Test helpers

fn read(data: &[u8]) -> PSDDocument {
    PSDReader::read_data(data, limits::document_pixel_budget()).expect("document")
}

fn read_with(data: &[u8], remaining: usize) -> Result<PSDDocument, PSDReadError> {
    PSDReader::read_data(data, remaining)
}

fn import(document: &PSDDocument) -> compositor_io::psd::builder::PSDImport {
    PSDDocumentBuilder::make_import(document, &FxHashMap::default())
}

fn color_image(width: usize, height: usize, red: u8, green: u8, blue: u8, alpha: u8) -> SharedImage {
    let mut bytes = vec![0u8; width * height * 4];
    for pixel in bytes.chunks_exact_mut(4) {
        pixel[0] = ((red as u16 * alpha as u16 + 127) / 255) as u8;
        pixel[1] = ((green as u16 * alpha as u16 + 127) / 255) as u8;
        pixel[2] = ((blue as u16 * alpha as u16 + 127) / 255) as u8;
        pixel[3] = alpha;
    }
    Arc::new(Rgba8Image::from_data(width, height, bytes))
}

fn gray_image(width: usize, height: usize, value: u8) -> Gray8Image {
    Gray8Image::uniform(width, height, value)
}

// MARK: - Ported tests

/// `[String: Data]` as the reader's extra-block map.
fn extras<'a>(blocks: &'a [(&str, Vec<u8>)]) -> FxHashMap<String, &'a [u8]> {
    blocks.iter().map(|(key, value)| ((*key).to_string(), value.as_slice())).collect()
}

fn shorts(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|value| (*value as i16).to_be_bytes()).collect()
}

// MARK: PSBImportTests

#[test]
fn psb_round_trip_matches_psd_layer_content() {
    let mask = gray_image(2, 2, 128);
    let mut bottom = PSDRecord::new(Uuid::new_v4(), "Red");
    bottom.bounds = Rect::new(0.0, 0.0, 3.0, 2.0);
    bottom.image = Some(color_image(3, 2, 255, 0, 0, 255));
    let mut middle = PSDRecord::new(Uuid::new_v4(), "Green");
    middle.bounds = Rect::new(1.0, 1.0, 2.0, 3.0);
    middle.image = Some(color_image(2, 3, 0, 255, 0, 255));
    let mut top = PSDRecord::new(Uuid::new_v4(), "Blue mask");
    top.bounds = Rect::new(2.0, 2.0, 2.0, 2.0);
    top.image = Some(color_image(2, 2, 0, 0, 255, 255));
    top.mask = Some(Arc::new(mask));
    let source = PSDDocument { width: 5, height: 5, resolution: 144.0, layers: vec![bottom, middle, top] };

    let psd = read(&fixture_data(&source, false, None, &[]));
    let psb = read(&fixture_data(&source, true, None, &[]));

    assert_eq!(psb.layers.len(), psd.layers.len());
    assert_eq!(
        psb.layers.iter().map(|layer| layer.name.clone()).collect::<Vec<_>>(),
        psd.layers.iter().map(|layer| layer.name.clone()).collect::<Vec<_>>()
    );
    assert_eq!(
        psb.layers.iter().map(|layer| layer.bounds).collect::<Vec<_>>(),
        psd.layers.iter().map(|layer| layer.bounds).collect::<Vec<_>>()
    );
    for index in 0..psd.layers.len() {
        assert_eq!(
            psb.layers[index].image.as_ref().expect("psb pixels").data(),
            psd.layers[index].image.as_ref().expect("psd pixels").data()
        );
    }
    assert_eq!(
        psb.layers[2].mask.as_ref().expect("psb mask").data(),
        psd.layers[2].mask.as_ref().expect("psd mask").data()
    );
}

#[test]
fn psb_exceeding_canvas_limit_is_rejected() {
    let image = color_image(2, 2, 255, 0, 0, 255);
    let mut layer = PSDRecord::new(Uuid::new_v4(), "Large");
    layer.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    layer.image = Some(image);
    let mut data = fixture_data(
        &PSDDocument { width: 2, height: 2, resolution: 72.0, layers: vec![layer] },
        true,
        None,
        &[],
    );
    data[18..22].copy_from_slice(&[0, 0, 0x75, 0x31]);
    assert!(matches!(read_with(&data, limits::document_pixel_budget()), Err(PSDReadError::Import(ImageImportError::TooLarge))));
}

#[test]
fn psb_large_additional_info_block_does_not_hide_unicode_name() {
    let mut layer = PSDRecord::new(Uuid::new_v4(), "Café layer");
    layer.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    layer.image = Some(color_image(2, 2, 255, 0, 0, 255));
    let data = fixture_data(
        &PSDDocument { width: 2, height: 2, resolution: 72.0, layers: vec![layer] },
        true,
        Some(("LMsk", vec![1, 2, 3])),
        &[],
    );
    assert_eq!(read(&data).layers.iter().map(|layer| layer.name.clone()).collect::<Vec<_>>(), vec!["Café layer"]);
}

// MARK: PSDAdjustmentTests

#[test]
fn levels_gamma_is_in_hundredths() {
    // RGB: input 2–254, gamma 1.00 (stored as 100); red, green and blue untouched; padded to Photoshop's 292 bytes.
    let mut data = shorts(&[2, 2, 254, 0, 255, 100]);
    for _ in 0..3 {
        data.extend_from_slice(&shorts(&[0, 255, 0, 255, 100]));
    }
    data.resize(292, 0);
    let adjustment = PSDAdjustments::levels(&data).expect("levels");
    assert_eq!(
        adjustment.levels.ranges[0],
        LevelRange { black: 2.0, gamma: 1.0, white: 254.0, output_black: 0.0, output_white: 255.0 }
    );
    assert!(adjustment.levels.ranges[1..4].iter().all(|range| range.gamma == 1.0));
}

#[test]
fn hue_saturation_reads_master_and_each_range() {
    // Version 2, Colorize off; Colorize values (ignored), Master +5/+4/0; Reds' band and −30 saturation, +10 light.
    let mut data = shorts(&[2]);
    data.extend_from_slice(&[0, 0]);
    data.extend_from_slice(&shorts(&[23, 25, 0, 5, 4, 0, 315, 345, 15, 45, 0, -30, 10]));
    data.extend_from_slice(&shorts(&vec![0; 7 * 5]));
    let adjustment = PSDAdjustments::hue(&data).expect("hue");
    let settings = adjustment.hsv_settings.as_ref().expect("hsv settings");
    assert!(!settings.colorize);
    let master = settings
        .adjustments
        .iter()
        .find(|(range, _)| **range == ColorRange::Master)
        .map(|(_, value)| *value)
        .expect("master");
    assert_eq!(master, RangeAdjustment { hue: 5.0, saturation: 4.0, lightness: 0.0 });
    let reds = settings
        .adjustments
        .iter()
        .find(|(range, _)| **range == ColorRange::Reds)
        .map(|(_, value)| *value)
        .expect("reds");
    assert_eq!(reds, RangeAdjustment { hue: 0.0, saturation: -30.0, lightness: 10.0 });
    let band = settings.bands.iter().find(|(range, _)| **range == ColorRange::Reds).map(|(_, band)| *band).expect("reds band");
    assert_eq!(band, HueBand { falloff_start: 315.0, range_start: 345.0, range_end: 15.0, falloff_end: 45.0 });

    data[2] = 1; // Colorize on: its own values apply.
    let colorized = PSDAdjustments::hue(&data).expect("colorized");
    let settings = colorized.hsv_settings.as_ref().expect("hsv settings");
    assert!(settings.colorize);
    let master = settings
        .adjustments
        .iter()
        .find(|(range, _)| **range == ColorRange::Master)
        .map(|(_, value)| *value)
        .expect("master");
    assert_eq!(master, RangeAdjustment { hue: 23.0, saturation: 25.0, lightness: 0.0 });
}

#[test]
fn mask_patch_sits_where_it_is_on_the_canvas() {
    // A 2 × 2 white patch at (3, 1) on a 6 × 4 canvas, black everywhere else, on an adjustment layer (no pixels of its
    // own, so it covers the canvas): the patch must land at (3, 1), not stretch over the whole layer.
    let patch = gray_image(2, 2, 255);
    let mut record = PSDRecord::new(Uuid::new_v4(), "Levels");
    record.mask_bounds = Rect::new(3.0, 1.0, 2.0, 2.0);
    record.mask_default = 0;
    let canvas = Size::new(6.0, 4.0);
    let layer = ImageLayer::with_id(
        Uuid::new_v4(),
        None,
        "Levels".to_string(),
        true,
        LayerTransform { origin: Point::ZERO, size: canvas, ..Default::default() },
        None,
        false,
        1.0,
        LayerBlendMode::Normal,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let mask = PSDDocumentBuilder::mask_on_layer_grid(&patch, &record, &layer, canvas).expect("mask");
    assert_eq!((mask.width(), mask.height()), (6, 4));
    let rows: Vec<String> = (0..4)
        .map(|y| (0..6).map(|x| if mask.get(x, y) > 127 { '#' } else { '.' }).collect())
        .collect();
    assert_eq!(rows, vec!["......", "...##.", "...##.", "......"]);
}

// MARK: PSDRoundTripTests

#[test]
fn round_trip_layers_order_visibility_opacity_and_blend() {
    let mut bottom = PSDRecord::new(Uuid::new_v4(), "Red");
    bottom.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    bottom.image = Some(color_image(2, 2, 255, 0, 0, 255));
    bottom.opacity = 0.5;
    bottom.blend_key = "mul ".to_string();
    let mut top = PSDRecord::new(Uuid::new_v4(), "Blue");
    top.bounds = Rect::new(2.0, 0.0, 2.0, 2.0);
    top.image = Some(color_image(2, 2, 0, 0, 255, 255));
    top.is_visible = false;
    let data = fixture_data(
        &PSDDocument { width: 4, height: 4, resolution: 144.0, layers: vec![bottom, top] },
        false,
        None,
        &[],
    );
    assert_eq!(&data[..4], b"8BPS");
    let document = read(&data);
    assert_eq!((document.width, document.height), (4, 4));
    assert_eq!(document.resolution, 144.0);
    assert_eq!(document.layers.iter().map(|layer| layer.name.clone()).collect::<Vec<_>>(), vec!["Red", "Blue"]);
    assert!(document.layers[0].is_visible);
    assert!(!document.layers[1].is_visible);
    assert!((document.layers[0].opacity - 0.5).abs() < 0.01);
    assert_eq!(document.layers[0].blend_key, "mul ");
    assert_eq!(document.layers[0].image.as_ref().expect("pixels").width(), 2);
    let imported = import(&document);
    assert!(imported.conversions.is_empty());
    assert_eq!(imported.layers.iter().map(|layer| layer.name.clone()).collect::<Vec<_>>(), vec!["Red", "Blue"]);
    assert_eq!(imported.layers[0].blend_mode, LayerBlendMode::Multiply);
    assert!(!imported.layers[1].is_visible);
}

#[test]
fn round_trip_groups_masks_and_clipping() {
    let group_id = Uuid::new_v4();
    let mut group = PSDRecord::new(group_id, "Stack");
    group.is_group = true;
    group.blend_key = "pass".to_string();
    let mut base = PSDRecord::new(Uuid::new_v4(), "Base");
    base.parent_id = Some(group_id);
    base.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    base.image = Some(color_image(2, 2, 0, 255, 0, 255));
    base.mask = Some(Arc::new(gray_image(2, 2, 255)));
    let mut child = PSDRecord::new(Uuid::new_v4(), "Clipped");
    child.parent_id = Some(group_id);
    child.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    child.image = Some(color_image(2, 2, 255, 255, 0, 255));
    child.clipping = true;
    let data = fixture_data(
        &PSDDocument { width: 4, height: 4, resolution: 72.0, layers: vec![group, base, child] },
        false,
        None,
        &[],
    );
    let imported = import(&read(&data));
    assert!(imported.conversions.is_empty());
    let folder = imported.layers.iter().find(|layer| layer.is_group).expect("folder");
    let imported_base = imported.layers.iter().find(|layer| layer.name == "Base").expect("base");
    let imported_child = imported.layers.iter().find(|layer| layer.name == "Clipped").expect("clipped");
    assert_eq!(imported_base.parent_id, Some(folder.id));
    assert_eq!(imported_child.parent_id, Some(folder.id));
    assert!(imported_base.mask.is_some());
    assert_eq!(imported_child.mask_source_id, Some(imported_base.id));
}

#[test]
fn imported_groups_follow_photoshop_lsct_order() {
    let group_id = Uuid::new_v4();
    let mut group = PSDRecord::new(group_id, "Stack");
    group.is_group = true;
    let mut child = PSDRecord::new(Uuid::new_v4(), "Base");
    child.parent_id = Some(group_id);
    child.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    child.image = Some(color_image(2, 2, 0, 255, 0, 255));
    let data = fixture_data(
        &PSDDocument { width: 4, height: 4, resolution: 72.0, layers: vec![group, child] },
        false,
        None,
        &[],
    );
    assert_eq!(lsct_types(&data), vec![3, 1]);
    let imported = import(&read(&data));
    let folder = imported.layers.iter().find(|layer| layer.is_group).expect("folder");
    assert_eq!(folder.name, "Stack");
    assert_eq!(imported.layers.iter().find(|layer| layer.name == "Base").expect("base").parent_id, Some(folder.id));
}

#[test]
fn oversized_layer_bounds_are_rejected() {
    assert!(matches!(
        read_with(&oversized_layer_file(8, 8, 30_000, 30_000), 50),
        Err(PSDReadError::Import(ImageImportError::TooLarge))
    ));
    let mut layer = PSDRecord::new(Uuid::new_v4(), "Huge");
    layer.bounds = Rect::new(0.0, 0.0, 20.0, 20.0);
    layer.image = Some(color_image(20, 20, 255, 0, 0, 255));
    let data = fixture_data(
        &PSDDocument { width: 20, height: 20, resolution: 72.0, layers: vec![layer] },
        false,
        None,
        &[],
    );
    assert!(matches!(read_with(&data, 50), Err(PSDReadError::Import(ImageImportError::TooLarge))));
}

#[test]
fn unused_spot_channels_are_skipped_before_decode() {
    let pixels = vec![255u8; 4];
    let mut channels: Vec<(i16, Vec<u8>)> = Vec::new();
    for id in [-1i16, 0, 1, 2] {
        channels.push((id, raw_channel(&pixels)));
    }
    // Compression 99 would throw if these planes were unpacked. 52 extras fill the 56-channel cap.
    for id in 3i16..=54 {
        channels.push((id, vec![0, 99, 0, 0]));
    }
    let document = read(&layer_file(8, 8, 2, 2, &channels));
    assert_eq!(document.layers.len(), 1);
    let image = document.layers[0].image.as_ref().expect("pixels");
    assert_eq!((image.width(), image.height()), (2, 2));
}

#[test]
fn unsupported_compression_on_color_channels_is_still_rejected() {
    let pixels = vec![255u8; 4];
    let channels: Vec<(i16, Vec<u8>)> =
        vec![(-1, raw_channel(&pixels)), (0, vec![0, 99, 0, 0]), (1, raw_channel(&pixels)), (2, raw_channel(&pixels))];
    assert!(matches!(
        read_with(&layer_file(8, 8, 2, 2, &channels), limits::document_pixel_budget()),
        Err(PSDReadError::PSD(PSDError::UnsupportedCompression))
    ));
}

#[test]
fn matches_requires_photoshop_magic() {
    let directory = std::env::temp_dir();
    let jpeg = directory.join(format!("{}.psd", Uuid::new_v4()));
    std::fs::write(&jpeg, [0xFFu8, 0xD8, 0xFF, 0xE0]).expect("write");
    assert!(!PSDReader::matches(&jpeg));
    std::fs::remove_file(&jpeg).ok();

    let psd = directory.join(format!("{}.bin", Uuid::new_v4()));
    std::fs::write(&psd, b"8BPS").expect("write");
    assert!(PSDReader::matches(&psd));
    std::fs::remove_file(&psd).ok();
}

#[test]
fn unsupported_blend_produces_conversion_report() {
    let mut layer = PSDRecord::new(Uuid::new_v4(), "Dissolved");
    layer.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    layer.image = Some(color_image(2, 2, 255, 0, 0, 255));
    layer.blend_key = "diss".to_string();
    let data = fixture_data(
        &PSDDocument { width: 2, height: 2, resolution: 72.0, layers: vec![layer] },
        false,
        None,
        &[],
    );
    let imported = import(&read(&data));
    assert!(!imported.conversions.is_empty());
    assert!(imported.conversions.iter().any(|conversion| conversion.layer_name == "Dissolved" && conversion.message.contains("diss")));
    assert_eq!(imported.layers[0].blend_mode, LayerBlendMode::Normal);
}

#[test]
fn soft_light_imports_without_conversion() {
    let mut layer = PSDRecord::new(Uuid::new_v4(), "Soft");
    layer.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    layer.image = Some(color_image(2, 2, 255, 0, 0, 255));
    layer.blend_key = "sLit".to_string();
    let data = fixture_data(
        &PSDDocument { width: 2, height: 2, resolution: 72.0, layers: vec![layer] },
        false,
        None,
        &[],
    );
    let imported = import(&read(&data));
    assert!(imported.conversions.is_empty());
    assert_eq!(imported.layers[0].blend_mode, LayerBlendMode::SoftLight);
}

#[test]
fn folder_opacity_imports_onto_the_folder() {
    let group_id = Uuid::new_v4();
    let mut group = PSDRecord::new(group_id, "Stack");
    group.is_group = true;
    group.opacity = 0.5;
    let mut child = PSDRecord::new(Uuid::new_v4(), "Base");
    child.parent_id = Some(group_id);
    child.bounds = Rect::new(0.0, 0.0, 2.0, 2.0);
    child.image = Some(color_image(2, 2, 0, 255, 0, 255));
    let data = fixture_data(
        &PSDDocument { width: 4, height: 4, resolution: 72.0, layers: vec![group, child] },
        false,
        None,
        &[],
    );
    let imported = import(&read(&data));
    let folder = imported.layers.iter().find(|layer| layer.is_group).expect("folder");
    // Photoshop stores opacity in one byte, so a half-opaque group comes back as 128/255.
    assert!((folder.opacity - 0.5).abs() < 0.01);
    assert!(!imported.conversions.iter().any(|conversion| conversion.layer_name == "Stack" && conversion.message.contains("opacity")));
}

#[test]
fn unsupported_headers_are_rejected() {
    let limit = limits::document_pixel_budget();
    assert!(matches!(read_with(&header(3, 8, 8, 8, 3), limit), Err(PSDReadError::PSD(PSDError::UnsupportedVersion))));
    assert!(matches!(read_with(&header(1, 8, 8, 8, 4), limit), Err(PSDReadError::PSD(PSDError::UnsupportedColorMode))));
    assert!(matches!(read_with(&header(1, 8, 8, 16, 3), limit), Err(PSDReadError::PSD(PSDError::UnsupportedDepth))));
    assert!(matches!(read_with(&header(1, 30_001, 10, 8, 3), limit), Err(PSDReadError::Import(ImageImportError::TooLarge))));
}

// MARK: Vector masks and live shapes

#[test]
fn vector_mask_is_rasterized_with_fill_and_stroke() {
    let canvas = Size::new(200.0, 200.0);
    let mut blocks = vec![
        (
            "vmsk",
            vector_mask((200.0, 200.0), &[(120.0, 30.0), (120.0, 80.0), (20.0, 80.0), (20.0, 30.0)]),
        ),
        ("SoCo", solid_color(0.0, 110.0, 255.0)),
        ("vstk", stroke_style(true, false, 1.0, 255.0, 255.0, 0.0)),
    ];
    let raster = psd_vector::raster(&extras(&blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("raster");
    assert!(raster.bounds.width() >= 99.0 && raster.bounds.height() >= 49.0);
    assert!(raster.image.width() >= 99 && raster.image.height() >= 49);

    blocks[1].1 = solid_color(0.0, 0.0, 0.0);
    blocks[2].1 = stroke_style(true, true, 10.0, 255.0, 255.0, 0.0);
    let stroked = psd_vector::raster(&extras(&blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("raster");
    assert!(stroked.bounds.width() > raster.bounds.width());
    assert!(stroked.bounds.height() > raster.bounds.height());
}

#[test]
fn photoshop_shape_extras_rasterize_in_place() {
    let canvas = Size::new(1920.0, 1080.0);
    let circle_blocks = circle_extra();
    let circle = psd_vector::raster(&extras(&circle_blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("circle");
    assert!((circle.bounds.mid_x() - 618.0).abs() < 8.0);
    assert!((circle.bounds.mid_y() - 677.0).abs() < 8.0);
    assert!((circle.bounds.width() - 328.0).abs() < 12.0);
    assert!((circle.bounds.height() - 328.0).abs() < 12.0);

    let rectangle_blocks = rectangle_extra();
    let rectangle = psd_vector::raster(&extras(&rectangle_blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("rectangle");
    assert!(rectangle.bounds.width() > 640.0);
    assert!(rectangle.bounds.height() > 170.0);
    assert!((rectangle.bounds.mid_x() - 1268.0).abs() < 20.0);
    assert!((rectangle.bounds.mid_y() - 244.0).abs() < 20.0);
}

#[test]
fn fill_ellipse_imports_as_a_live_shape() {
    let canvas = Size::new(1920.0, 1080.0);
    let mut blocks = circle_extra();
    blocks.push(("vogk", origination_data(5, (454.0, 513.0, 782.0, 841.0), &[])));
    let live = psd_vector::live(&extras(&blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("live");
    assert_eq!(live.style.kind, compositor_core::layer_shape::ShapeKind::Ellipse);
    assert!((live.style.green - 110.0 / 255.0).abs() < 0.01);
    assert!((live.style.blue - 1.0).abs() < 0.01);
    assert!(live.notes.is_empty());
    assert!((live.bounds.min_x() - 454.0).abs() < 1.0 && (live.bounds.width() - 328.0).abs() < 1.0);

    let mut record = PSDRecord::new(Uuid::new_v4(), "cercle-bleu");
    record.kind = PSDLayerKind::Vector;
    record.image = Some(live.image.clone());
    record.bounds = live.bounds;
    record.shape = Some(live.style.clone());
    let imported = import(&PSDDocument { width: 1920, height: 1080, resolution: 72.0, layers: vec![record] });
    let layer = &imported.layers[0];
    let shape = layer.live_shape().expect("live shape");
    assert_eq!(shape.style.kind, compositor_core::layer_shape::ShapeKind::Ellipse);
    assert!(imported.conversions.is_empty());
    let PixelImage::Rgba(asset_image) = &layer.asset.as_ref().expect("asset").image else {
        panic!("the imported shape carries rgba pixels");
    };
    assert!(Arc::ptr_eq(asset_image, &shape.image));
}

#[test]
fn stroked_rectangle_imports_as_a_live_shape_and_reports_the_stroke() {
    let canvas = Size::new(1920.0, 1080.0);
    let mut blocks = rectangle_extra();
    blocks.push(("vogk", origination_data(2, (945.0, 153.0, 1591.0, 335.0), &[0.0, 0.0, 0.0, 0.0])));
    let live = psd_vector::live(&extras(&blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("live");
    assert_eq!(live.style.kind, compositor_core::layer_shape::ShapeKind::Rectangle);
    assert_eq!(live.style.corner_radius, 0.0);
    assert!(live.notes.iter().any(|note| note.contains("stroke")));

    let mut record = PSDRecord::new(Uuid::new_v4(), "rectangle-contour-jaune");
    record.kind = PSDLayerKind::Vector;
    record.image = Some(live.image.clone());
    record.bounds = live.bounds;
    record.shape = Some(live.style.clone());
    record.shape_notes = live.notes.clone();
    let imported = import(&PSDDocument { width: 1920, height: 1080, resolution: 72.0, layers: vec![record] });
    assert_eq!(
        imported.layers[0].live_shape().expect("live shape").style.kind,
        compositor_core::layer_shape::ShapeKind::Rectangle
    );
    assert!(imported
        .conversions
        .iter()
        .any(|conversion| conversion.layer_name == "rectangle-contour-jaune" && conversion.message.contains("stroke")));
    assert!(!imported.conversions.iter().any(|conversion| conversion.message.contains("rasterized")));
}

#[test]
fn four_sharp_corners_infer_a_rectangle_without_origination() {
    let canvas = Size::new(200.0, 200.0);
    let blocks = vec![
        (
            "vmsk",
            vector_mask((200.0, 200.0), &[(120.0, 30.0), (120.0, 80.0), (20.0, 80.0), (20.0, 30.0)]),
        ),
        ("SoCo", solid_color(0.0, 110.0, 255.0)),
        ("vstk", stroke_style(true, false, 1.0, 255.0, 255.0, 0.0)),
    ];
    let live = psd_vector::live(&extras(&blocks), canvas, limits::document_pixel_budget())
        .expect("decode")
        .expect("live");
    assert_eq!(live.style.kind, compositor_core::layer_shape::ShapeKind::Rectangle);
    assert!(live.notes.is_empty());
    assert!(live.bounds.width() >= 99.0 && live.bounds.height() >= 49.0);
}

#[test]
fn huge_origination_size_is_rejected_without_trapping() {
    let blocks = vec![
        ("vogk", origination_data(5, (0.0, 0.0, 1e20, 1e20), &[])),
        ("SoCo", solid_color(0.0, 110.0, 255.0)),
        ("vstk", stroke_style(true, false, 1.0, 255.0, 255.0, 0.0)),
    ];
    assert!(matches!(
        psd_vector::live(&extras(&blocks), Size::new(1920.0, 1080.0), limits::document_pixel_budget()),
        Err(PSDReadError::Import(ImageImportError::TooLarge))
    ));
}

#[test]
fn non_finite_origination_size_is_ignored() {
    let blocks = vec![
        ("vogk", origination_data(5, (10.0, 10.0, f64::INFINITY, 100.0), &[])),
        ("SoCo", solid_color(0.0, 110.0, 255.0)),
        ("vstk", stroke_style(true, false, 1.0, 255.0, 255.0, 0.0)),
    ];
    assert!(psd_vector::live(&extras(&blocks), Size::new(1920.0, 1080.0), limits::document_pixel_budget())
        .expect("decode")
        .is_none());
}

#[test]
fn huge_stroke_width_is_rejected_without_trapping() {
    let blocks = vec![
        (
            "vmsk",
            vector_mask((200.0, 200.0), &[(120.0, 30.0), (120.0, 80.0), (20.0, 80.0), (20.0, 30.0)]),
        ),
        ("SoCo", solid_color(0.0, 0.0, 0.0)),
        ("vstk", stroke_style(true, true, 1e20, 255.0, 255.0, 0.0)),
    ];
    assert!(matches!(
        psd_vector::raster(&extras(&blocks), Size::new(200.0, 200.0), limits::document_pixel_budget()),
        Err(PSDReadError::Import(ImageImportError::TooLarge))
    ));
}

// MARK: Text

#[test]
fn photoshop_point_text_imports_as_editable_text() {
    let ty_sh = TySh::default().text("Hello").data();
    let parsed = psd_text::parse(&extras(&[("TySh", ty_sh.clone())])).expect("text");
    assert_eq!(parsed.style.content, "Hello");
    assert_eq!(parsed.style.font_name, "Helvetica");
    assert_eq!(parsed.style.font_size, 24.0);
    assert_eq!((parsed.style.red, parsed.style.green, parsed.style.blue), (0.0, 0.0, 0.0));
    assert_eq!(parsed.style.alignment, compositor_core::layer_text::TextAlignment::Left);
    assert!(parsed.notes.is_empty());

    let record_id = Uuid::new_v4();
    let mut record = PSDRecord::new(record_id, "Greeting");
    record.image = Some(color_image(8, 8, 0, 0, 0, 255));
    record.bounds = Rect::new(1.0, 2.0, 8.0, 8.0);
    let data = fixture_data(
        &PSDDocument { width: 32, height: 32, resolution: 72.0, layers: vec![record] },
        false,
        None,
        &[(record_id, vec![("TySh", ty_sh)])],
    );
    let document = read(&data);
    let read_layer = &document.layers[0];
    assert_eq!(read_layer.kind, PSDLayerKind::Text);
    assert_eq!(read_layer.text.as_ref().expect("text").style.content, "Hello");
    let imported = import(&document);
    let layer = &imported.layers[0];
    let live = layer.live_text().expect("live text");
    assert_eq!(live.style.content, "Hello");
    assert_eq!(live.style.font_name, "Helvetica");
    assert_eq!(live.style.font_size, 24.0);
    let PixelImage::Rgba(asset_image) = &layer.asset.as_ref().expect("asset").image else {
        panic!("the imported text carries rgba pixels");
    };
    assert!(Arc::ptr_eq(asset_image, &live.image));
    assert!(!imported.conversions.iter().any(|conversion| conversion.message == psd_text::RASTERIZED_NOTE));
    assert!((layer.transform.origin.x - 40.0).abs() < 80.0);
    assert!((layer.transform.origin.y - 50.0).abs() < 80.0);
}

#[test]
fn photoshop_text_size_uses_matrix_scale_not_document_resolution() {
    // Identity scale keeps the engine size whatever the document PPI is.
    assert_eq!(psd_text::parse(&extras(&[("TySh", TySh::default().data())])).expect("text").style.font_size, 24.0);
    let scaled = TySh::default().matrix(2.0, 0.0, 0.0, 2.0).data();
    assert_eq!(psd_text::parse(&extras(&[("TySh", scaled)])).expect("text").style.font_size, 48.0);
    let scaled_25 = TySh::default().font_size(25.0).matrix(2.0, 0.0, 0.0, 2.0).data();
    assert_eq!(psd_text::parse(&extras(&[("TySh", scaled_25)])).expect("text").style.font_size, 50.0);
}

#[test]
fn photoshop_text_keeps_the_first_style_and_reports_the_rest() {
    // The Swift original passes `red: 1, green: 0, blue: 0` to `PSDFixture.tySh`.
    let blocks = TySh::default().text("Hello").color(1.0, 0.0, 0.0).justification(2).tracking(1000.0).leading(30.0).second_size(48.0).data();
    let parsed = psd_text::parse(&extras(&[("TySh", blocks)])).expect("text");
    assert_eq!(parsed.style.content, "Hello");
    assert_eq!(parsed.style.alignment, compositor_core::layer_text::TextAlignment::Center);
    assert_eq!(parsed.style.tracking, 24.0);
    assert_eq!(parsed.style.leading, 30.0);
    assert_eq!(parsed.style.red, 1.0);
    assert_eq!(parsed.style.font_size, 24.0);
    assert!(parsed.notes.contains(&psd_text::FIRST_STYLE_NOTE.to_string()));
}

#[test]
fn photoshop_text_reports_leading_only_style_differences() {
    let by_leading = TySh::default().text("Hello").leading(30.0).second_leading(48.0).data();
    let parsed = psd_text::parse(&extras(&[("TySh", by_leading)])).expect("text");
    assert_eq!(parsed.style.leading, 30.0);
    assert!(parsed.notes.contains(&psd_text::FIRST_STYLE_NOTE.to_string()));
    let by_scale = TySh::default().text("Hello").second_horizontal_scale(1.2).data();
    let parsed = psd_text::parse(&extras(&[("TySh", by_scale)])).expect("text");
    assert!(parsed.notes.contains(&psd_text::FIRST_STYLE_NOTE.to_string()));
}

#[test]
fn photoshop_paragraph_text_keeps_its_box() {
    let blocks = TySh::default()
        .text("Hello")
        .translate_to(10.0, 30.0)
        .bounds((0.0, 0.0, 200.0, 80.0), (0.0, -10.0, 40.0, 10.0))
        .data();
    let parsed = psd_text::parse(&extras(&[("TySh", blocks)])).expect("text");
    assert!(parsed.anchor_is_frame);
    assert_eq!(parsed.style.box_size, Some(Size::new(224.0, 104.0)));
    assert_eq!(parsed.document_anchor, Point::new(10.0, 30.0));
}

#[test]
fn oversized_photoshop_paragraph_frame_stays_pixels() {
    let blocks = TySh::default().text("Hello").bounds((0.0, 0.0, 40_000.0, 100.0), (0.0, 0.0, 40.0, 10.0)).data();
    assert!(psd_text::parse(&extras(&[("TySh", blocks.clone())])).is_none());

    let record_id = Uuid::new_v4();
    let mut record = PSDRecord::new(record_id, "Billboard");
    record.image = Some(color_image(4, 4, 0, 255, 0, 255));
    record.bounds = Rect::new(0.0, 0.0, 4.0, 4.0);
    let data = fixture_data(
        &PSDDocument { width: 16, height: 16, resolution: 72.0, layers: vec![record] },
        false,
        None,
        &[(record_id, vec![("TySh", blocks)])],
    );
    let imported = import(&read(&data));
    let layer = &imported.layers[0];
    assert!(layer.live_text().is_none());
    assert_eq!(layer.asset.as_ref().expect("asset").image.width(), 4);
    assert!(imported.conversions.iter().any(|conversion| conversion.message == psd_text::RASTERIZED_NOTE));
}

#[test]
fn warped_photoshop_text_stays_editable_and_says_so() {
    let blocks = TySh::default().text("Hello").faux(true, false).warp().data();
    let parsed = psd_text::parse(&extras(&[("TySh", blocks)])).expect("text");
    assert_eq!(parsed.style.content, "Hello");
    assert!(parsed.notes.contains(&psd_text::WARP_NOTE.to_string()));
    assert!(parsed.notes.contains(&psd_text::FAUX_NOTE.to_string()));
}

#[test]
fn vertical_or_broken_photoshop_text_stays_pixels() {
    let vertical = TySh::default().text("Hello").vertical().data();
    assert!(psd_text::parse(&extras(&[("TySh", vertical.clone())])).is_none());
    assert!(psd_text::parse(&extras(&[("TySh", vec![0, 1])])).is_none());
    assert!(psd_text::parse(&extras(&[("TySh", TySh::default().text("Hello").matrix(2.0, 0.0, 0.0, 1.0).data())])).is_none());

    let record_id = Uuid::new_v4();
    let mut record = PSDRecord::new(record_id, "Sideways");
    record.image = Some(color_image(4, 4, 0, 0, 255, 255));
    record.bounds = Rect::new(0.0, 0.0, 4.0, 4.0);
    let data = fixture_data(
        &PSDDocument { width: 16, height: 16, resolution: 72.0, layers: vec![record] },
        false,
        None,
        &[(record_id, vec![("TySh", vertical)])],
    );
    let imported = import(&read(&data));
    let layer = &imported.layers[0];
    assert!(layer.live_text().is_none());
    assert_eq!(layer.asset.as_ref().expect("asset").image.width(), 4);
    assert!(imported.conversions.iter().any(|conversion| conversion.message == psd_text::RASTERIZED_NOTE));
}

#[test]
fn missing_photoshop_font_is_reported_but_stays_editable() {
    let ty_sh = TySh::default().text("Hello").font("DefinitelyMissingFontXYZ").data();
    let parsed = psd_text::parse(&extras(&[("TySh", ty_sh)])).expect("text");
    let mut record = PSDRecord::new(Uuid::new_v4(), "Missing");
    record.kind = PSDLayerKind::Text;
    record.text = Some(parsed);
    record.image = Some(color_image(2, 2, 0, 0, 0, 255));
    let imported = import(&PSDDocument { width: 64, height: 64, resolution: 72.0, layers: vec![record] });
    let layer = &imported.layers[0];
    assert_eq!(layer.live_text().expect("live text").style.font_name, "DefinitelyMissingFontXYZ");
    assert!(imported
        .conversions
        .iter()
        .any(|conversion| Some(conversion.message.clone()) == psd_text::missing_font_note("DefinitelyMissingFontXYZ")));
}

#[test]
fn rotated_photoshop_text_keeps_its_angle() {
    let blocks = TySh::default().text("Hello").matrix(0.0, -1.0, 1.0, 0.0).data();
    let parsed = psd_text::parse(&extras(&[("TySh", blocks)])).expect("text");
    assert!((parsed.rotation - 90.0).abs() < 0.01);
    assert!(!parsed.flip_y);
}

// The two remaining PSDRoundTripTests cases (`importCreatesDocumentAndExistingCanvasGetsAGroup`,
// `cancelledConversionLeavesTheDocumentUnchanged`) drive `EditorSession` and the pending-conversion
// UI, which live in `compositor-session`/`compositor-ui`, not in this crate.

