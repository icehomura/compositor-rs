//! The channel decoders: Photoshop layer channels, raw (compression 0), RLE/PackBits (1), ZIP (2) and
//! ZIP with prediction (3), ported from `IO/PSD/PSDChannelCoder.swift`.

use compositor_core::buffer::{Gray8Image, Rgba8Image};

use crate::psd::types::PSDError;

/// Where a channel sits inside its layer, in source pixels: the reader crops oversized layers to the
/// canvas before decoding and keeps the offset here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PSDCrop {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// `width * height` as a `usize`, `Truncated` when the two overflow.
fn pixel_count(width: usize, height: usize) -> Result<usize, PSDError> {
    width.checked_mul(height).ok_or(PSDError::Truncated)
}

/// Decodes one channel plane. `crop` limits the decoded pixels to the part of the layer that was kept
/// when an oversized layer was cropped to the canvas.
pub fn decode(
    compression: u16,
    width: usize,
    height: usize,
    data: &[u8],
    large_document: bool,
    crop: Option<PSDCrop>,
) -> Result<Vec<u8>, PSDError> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let Some(crop) = crop else {
        return decode_full(compression, width, height, data, large_document);
    };
    if crop.x < 0
        || crop.y < 0
        || crop.width < 0
        || crop.height < 0
        || crop.x + crop.width > width as i64
        || crop.y + crop.height > height as i64
    {
        return Err(PSDError::Truncated);
    }
    if crop.width == 0 || crop.height == 0 {
        return Ok(Vec::new());
    }
    match compression {
        0 => crop_raw(width, height, data, crop),
        1 => unpack_rle_cropped(width, height, data, large_document, crop),
        2 | 3 => {
            let plane = decode_full(compression, width, height, data, large_document)?;
            Ok(crop_plane(width, &plane, crop))
        }
        _ => Err(PSDError::UnsupportedCompression),
    }
}

fn decode_full(compression: u16, width: usize, height: usize, data: &[u8], large_document: bool) -> Result<Vec<u8>, PSDError> {
    let expected = pixel_count(width, height)?;
    match compression {
        0 => {
            if data.len() < expected {
                return Err(PSDError::Truncated);
            }
            Ok(data[..expected].to_vec())
        }
        1 => unpack_rle(width, height, data, large_document),
        2 => {
            let plane = inflate(data)?;
            if plane.len() < expected {
                return Err(PSDError::Truncated);
            }
            Ok(plane[..expected].to_vec())
        }
        3 => {
            let mut plane = inflate(data)?;
            if plane.len() < expected {
                return Err(PSDError::Truncated);
            }
            plane.truncate(expected);
            unzip_prediction(&mut plane, width, height);
            Ok(plane)
        }
        _ => Err(PSDError::UnsupportedCompression),
    }
}

/// `zlib` decompression, the shared step of the two ZIP compressions.
fn inflate(data: &[u8]) -> Result<Vec<u8>, PSDError> {
    use std::io::Read;
    let mut plane = Vec::new();
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    decoder.read_to_end(&mut plane).map_err(|_| PSDError::Truncated)?;
    Ok(plane)
}

/// ZIP with prediction: every row is stored as the difference of each byte from its left neighbor
/// (the first byte of a row is absolute). Undo that difference so the channel compares equal to the
/// raw one.
fn unzip_prediction(plane: &mut [u8], width: usize, height: usize) {
    for row in 0..height {
        let start = row * width;
        for column in 1..width {
            plane[start + column] = plane[start + column].wrapping_add(plane[start + column - 1]);
        }
    }
}

fn crop_raw(width: usize, height: usize, data: &[u8], crop: PSDCrop) -> Result<Vec<u8>, PSDError> {
    let expected = pixel_count(width, height)?;
    if data.len() < expected {
        return Err(PSDError::Truncated);
    }
    let crop = crop_bounds(crop);
    let mut plane = vec![0u8; crop.width * crop.height];
    for row in 0..crop.height {
        let source_start = (crop.y + row) * width + crop.x;
        let target_start = row * crop.width;
        plane[target_start..target_start + crop.width].copy_from_slice(&data[source_start..source_start + crop.width]);
    }
    Ok(plane)
}

/// The kept part of an already-decoded plane.
fn crop_plane(width: usize, plane: &[u8], crop: PSDCrop) -> Vec<u8> {
    let crop = crop_bounds(crop);
    let mut result = vec![0u8; crop.width * crop.height];
    for row in 0..crop.height {
        let source_start = (crop.y + row) * width + crop.x;
        let target_start = row * crop.width;
        result[target_start..target_start + crop.width].copy_from_slice(&plane[source_start..source_start + crop.width]);
    }
    result
}

/// The crop's four fields as in-bounds `usize`s; `decode` has already checked them.
fn crop_bounds(crop: PSDCrop) -> CropBounds {
    CropBounds { x: crop.x as usize, y: crop.y as usize, width: crop.width as usize, height: crop.height as usize }
}

struct CropBounds {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// The straight sRGB channels and alpha as the canonical premultiplied RGBA8 image.
pub fn rgba_image(width: usize, height: usize, red: &[u8], green: &[u8], blue: &[u8], alpha: &[u8]) -> Rgba8Image {
    let count = width * height;
    let mut pixels = vec![0u8; count * 4];
    for i in 0..count {
        let a = alpha[i] as u16;
        pixels[i * 4] = ((red[i] as u16 * a + 127) / 255) as u8;
        pixels[i * 4 + 1] = ((green[i] as u16 * a + 127) / 255) as u8;
        pixels[i * 4 + 2] = ((blue[i] as u16 * a + 127) / 255) as u8;
        pixels[i * 4 + 3] = a as u8;
    }
    Rgba8Image::from_data(width, height, pixels)
}

/// An 8-bit gray mask image from a decoded channel plane.
pub fn mask_image(width: usize, height: usize, gray: Vec<u8>) -> Gray8Image {
    Gray8Image::from_data(width, height, gray)
}

/// The row offsets of an RLE channel: 2 bytes a row, 4 in a PSB.
fn rle_row_counts(data: &[u8], height: usize, large_document: bool) -> Result<(usize, Vec<usize>), PSDError> {
    let mut offset = 0usize;
    let mut next = |offset: &mut usize| -> Result<u8, PSDError> {
        if *offset < data.len() {
            let value = data[*offset];
            *offset += 1;
            Ok(value)
        } else {
            Err(PSDError::Truncated)
        }
    };
    let mut counts = vec![0usize; height];
    for count in counts.iter_mut() {
        if large_document {
            let a = next(&mut offset)?;
            let b = next(&mut offset)?;
            let c = next(&mut offset)?;
            let d = next(&mut offset)?;
            *count = ((a as usize) << 24 | (b as usize) << 16 | (c as usize) << 8 | d as usize) as usize;
        } else {
            let hi = next(&mut offset)?;
            let lo = next(&mut offset)?;
            *count = (hi as usize) << 8 | lo as usize;
        }
    }
    Ok((offset, counts))
}

/// Unpacks one row of PackBits data into `row_buffer`, returning the offset after the row.
fn unpack_row(data: &[u8], end: usize, row_buffer: &mut [u8], width: usize, mut offset: usize) -> Result<usize, PSDError> {
    let mut written = 0usize;
    while written < width {
        if offset >= end {
            return Err(PSDError::Truncated);
        }
        let n = data[offset] as i8;
        offset += 1;
        if n >= 0 {
            let count = n as usize + 1;
            if written + count > width || offset + count > end {
                return Err(PSDError::Truncated);
            }
            row_buffer[written..written + count].copy_from_slice(&data[offset..offset + count]);
            offset += count;
            written += count;
        } else if n != -128 {
            let count = (1 - n as isize) as usize;
            if written + count > width || offset >= end {
                return Err(PSDError::Truncated);
            }
            let value = data[offset];
            offset += 1;
            for index in 0..count {
                row_buffer[written + index] = value;
            }
            written += count;
        }
    }
    Ok(offset)
}

fn unpack_rle(width: usize, height: usize, data: &[u8], large_document: bool) -> Result<Vec<u8>, PSDError> {
    let (mut offset, counts) = rle_row_counts(data, height, large_document)?;
    let mut plane = vec![0u8; pixel_count(width, height)?];
    let mut row_buffer = vec![0u8; width];
    for row in 0..height {
        let end = offset + counts[row];
        if end > data.len() {
            return Err(PSDError::Truncated);
        }
        unpack_row(data, end, &mut row_buffer, width, offset)?;
        plane[row * width..(row + 1) * width].copy_from_slice(&row_buffer);
        offset = end;
    }
    Ok(plane)
}

fn unpack_rle_cropped(width: usize, height: usize, data: &[u8], large_document: bool, crop: PSDCrop) -> Result<Vec<u8>, PSDError> {
    let (mut offset, counts) = rle_row_counts(data, height, large_document)?;
    let crop = crop_bounds(crop);
    let mut plane = vec![0u8; crop.width * crop.height];
    let mut row_buffer = vec![0u8; width];
    for row in 0..height {
        let end = offset + counts[row];
        if end > data.len() {
            return Err(PSDError::Truncated);
        }
        if row < crop.y || row >= crop.y + crop.height {
            offset = end;
            continue;
        }
        unpack_row(data, end, &mut row_buffer, width, offset)?;
        let target_start = (row - crop.y) * crop.width;
        plane[target_start..target_start + crop.width].copy_from_slice(&row_buffer[crop.x..crop.x + crop.width]);
        offset = end;
    }
    Ok(plane)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_bits_row(values: &[u8]) -> Vec<u8> {
        // A literal run: n = count - 1 followed by the bytes.
        let mut row = Vec::new();
        for chunk in values.chunks(128) {
            row.push((chunk.len() - 1) as u8);
            row.extend_from_slice(chunk);
        }
        row
    }

    fn rle_channel(rows: &[Vec<u8>], large_document: bool) -> Vec<u8> {
        let mut data = Vec::new();
        let mut packed = Vec::new();
        for values in rows {
            let row = pack_bits_row(values);
            if large_document {
                data.extend_from_slice(&(row.len() as u32).to_be_bytes());
            } else {
                data.extend_from_slice(&(row.len() as u16).to_be_bytes());
            }
            packed.extend_from_slice(&row);
        }
        data.extend_from_slice(&packed);
        data
    }

    #[test]
    fn raw_channels_decode_and_truncate() {
        let plane: Vec<u8> = (0..24).collect();
        assert_eq!(decode(0, 4, 6, &plane, false, None).unwrap(), plane);
        assert!(matches!(decode(0, 4, 6, &plane[..10], false, None), Err(PSDError::Truncated)));
        assert!(matches!(decode(9, 4, 6, &plane, false, None), Err(PSDError::UnsupportedCompression)));
        assert_eq!(decode(0, 0, 6, &[], false, None).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn pack_bits_decodes_literals_repeats_and_psb_counts() {
        // Row 1: a literal run, a no-op, then a repeat run. Row 2: a repeat run then a literal.
        let mut data = Vec::new();
        data.extend_from_slice(&[0x02u8, 1, 2, 3, 0x80, 0xFE, 9].as_ref());
        data.extend_from_slice(&[0xFDu8, 7, 0x01, 8, 9].as_ref());
        let mut channel: Vec<u8> = Vec::new();
        channel.extend_from_slice(&7u16.to_be_bytes());
        channel.extend_from_slice(&5u16.to_be_bytes());
        channel.extend_from_slice(&data);
        let plane = decode(1, 6, 2, &channel, false, None).unwrap();
        assert_eq!(plane, vec![1, 2, 3, 9, 9, 9, 7, 7, 7, 7, 8, 9]);

        // The same rows with PSB's 4-byte row counts.
        let mut psb: Vec<u8> = Vec::new();
        psb.extend_from_slice(&7u32.to_be_bytes());
        psb.extend_from_slice(&5u32.to_be_bytes());
        psb.extend_from_slice(&data);
        assert_eq!(decode(1, 6, 2, &psb, true, None).unwrap(), plane);

        // A row that does not fill its width is damaged, not padded.
        let mut short: Vec<u8> = Vec::new();
        short.extend_from_slice(&2u16.to_be_bytes());
        short.extend_from_slice(&[0xFDu8, 7].as_ref());
        assert!(matches!(decode(1, 6, 1, &short, false, None), Err(PSDError::Truncated)));
    }

    #[test]
    fn pack_bits_rejects_rows_that_overrun_their_count() {
        let mut channel: Vec<u8> = Vec::new();
        channel.extend_from_slice(&3u16.to_be_bytes());
        channel.extend_from_slice(&[0x05u8, 1, 2, 3, 4, 5, 6].as_ref()); // claims 6 bytes in 3
        assert!(matches!(decode(1, 6, 1, &channel, false, None), Err(PSDError::Truncated)));
    }

    #[test]
    fn pack_bits_crops_rows_and_columns() {
        let rows = vec![vec![10, 11, 12, 13, 14, 15], vec![20, 21, 22, 23, 24, 25]];
        let channel = rle_channel(&rows, false);
        let crop = PSDCrop { x: 2, y: 1, width: 3, height: 1 };
        assert_eq!(decode(1, 6, 2, &channel, false, Some(crop)).unwrap(), vec![22, 23, 24]);
        // A crop of nothing decodes to nothing without touching the data.
        let empty = PSDCrop { x: 0, y: 0, width: 0, height: 0 };
        assert_eq!(decode(1, 6, 2, &[], false, Some(empty)).unwrap(), Vec::<u8>::new());
        let outside = PSDCrop { x: 4, y: 0, width: 3, height: 1 };
        assert!(matches!(decode(1, 6, 2, &channel, false, Some(outside)), Err(PSDError::Truncated)));
    }

    #[test]
    fn raw_channels_crop_from_the_source_plane() {
        let plane: Vec<u8> = (0..24).collect();
        let crop = PSDCrop { x: 1, y: 2, width: 2, height: 3 };
        assert_eq!(decode(0, 4, 6, &plane, false, Some(crop)).unwrap(), vec![9, 10, 13, 14, 17, 18]);
    }

    fn zlib_encode(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn zip_channels_inflate() {
        let plane: Vec<u8> = (0..48).collect();
        let channel = zlib_encode(&plane);
        assert_eq!(decode(2, 6, 8, &channel, false, None).unwrap(), plane);
        // A stream that ends early is a damaged file, not a partial plane.
        assert!(matches!(decode(2, 6, 8, &channel[..4], false, None), Err(PSDError::Truncated)));
        assert!(matches!(decode(2, 100, 8, &channel, false, None), Err(PSDError::Truncated)));
    }

    #[test]
    fn zip_prediction_undoes_the_row_deltas() {
        let raw: Vec<u8> = (0..48).map(|value| (value * 7 + 3) as u8).collect();
        let width = 6usize;
        let height = 8usize;
        let mut predicted = raw.clone();
        for row in 0..height {
            let start = row * width;
            // Zig-zag encode: store each byte's difference from its left neighbor.
            for column in (1..width).rev() {
                predicted[start + column] = predicted[start + column].wrapping_sub(predicted[start + column - 1]);
            }
        }
        let channel = zlib_encode(&predicted);
        assert_eq!(decode(3, width, height, &channel, false, None).unwrap(), raw);
        // The same bytes without the prediction pass are the deltas, not the channel.
        assert_eq!(decode(2, width, height, &channel, false, None).unwrap(), predicted);
        assert_ne!(predicted, raw);
        // Cropping a predicted channel keeps the pixels the crop covers.
        let crop = PSDCrop { x: 1, y: 2, width: 3, height: 2 };
        assert_eq!(decode(3, width, height, &channel, false, Some(crop)).unwrap(), vec![raw[13], raw[14], raw[15], raw[19], raw[20], raw[21]]);
    }

    #[test]
    fn rgba_image_premultiplies() {
        let image = rgba_image(2, 1, &[255, 0], &[255, 128], &[255, 255], &[255, 0]);
        assert_eq!(image.get(0, 0), [255, 255, 255, 255]);
        assert_eq!(image.get(1, 0), [0, 0, 0, 0]);
        let half = rgba_image(1, 1, &[100], &[50], &[255], &[128]);
        assert_eq!(half.get(0, 0), [((100 * 128 + 127) / 255) as u8, ((50 * 128 + 127) / 255) as u8, ((255 * 128 + 127) / 255) as u8, 128]);
    }
}
