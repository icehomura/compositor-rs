//! The canonical raster formats: premultiplied sRGB RGBA8 (4 bytes/px, top-down, no padding) and
//! 8-bit gray (1 byte/px) for masks and selections — the layouts the original C kernels and `CGImage`
//! contexts use.

use crate::geom::{Cell, Rect};
use std::sync::Arc;

pub const RGBA_PIXEL: usize = 4;
pub const GRAY_PIXEL: usize = 1;
/// The tile the sparse raster splits on (`RasterSnapshot`'s spatial index).
pub const TILE: f64 = 256.0;

/// Premultiplied sRGB RGBA8. Rows run top-down; there is no row padding, so `stride == width * 4`.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba8Image {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl std::fmt::Debug for Rgba8Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rgba8Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl Rgba8Image {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height * RGBA_PIXEL],
        }
    }

    /// Fully transparent… no: fully **opaque white**, the blank-layer fill the editor uses.
    pub fn opaque(width: usize, height: usize, color: [u8; 4]) -> Self {
        let mut image = Self::new(width, height);
        for pixel in image.data.chunks_exact_mut(RGBA_PIXEL) {
            pixel.copy_from_slice(&color);
        }
        image
    }

    pub fn from_data(width: usize, height: usize, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width * height * RGBA_PIXEL, "RGBA buffer size mismatch");
        Self { width, height, data }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.width * RGBA_PIXEL
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn pixel_count(&self) -> usize {
        self.width * self.height
    }

    pub fn size_bytes(&self) -> usize {
        self.data.len()
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f64, self.height as f64)
    }

    pub fn contains(&self, x: i64, y: i64) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height
    }

    pub fn get(&self, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * self.width + x) * RGBA_PIXEL;
        [
            self.data[offset],
            self.data[offset + 1],
            self.data[offset + 2],
            self.data[offset + 3],
        ]
    }

    pub fn set(&mut self, x: usize, y: usize, pixel: [u8; 4]) {
        let offset = (y * self.width + x) * RGBA_PIXEL;
        self.data[offset..offset + RGBA_PIXEL].copy_from_slice(&pixel);
    }

    pub fn pixels(&self) -> impl Iterator<Item = [u8; 4]> + '_ {
        self.data.chunks_exact(RGBA_PIXEL).map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
    }

    /// A sub-image, `None` for a rectangle that is outside or empty.
    pub fn cropped(&self, rect: Rect) -> Option<Self> {
        let integral = rect.integral();
        if integral.is_empty() {
            return None;
        }
        let x = integral.min_x() as i64;
        let y = integral.min_y() as i64;
        let width = integral.width() as usize;
        let height = integral.height() as usize;
        if x < 0 || y < 0 || width == 0 || height == 0 || x as usize + width > self.width || y as usize + height > self.height {
            return None;
        }
        let mut result = Self::new(width, height);
        for row in 0..height {
            let source = ((y as usize + row) * self.width + x as usize) * RGBA_PIXEL;
            let destination = row * width * RGBA_PIXEL;
            result.data[destination..destination + width * RGBA_PIXEL]
                .copy_from_slice(&self.data[source..source + width * RGBA_PIXEL]);
        }
        Some(result)
    }

    /// A copy whose outside is transparent, growing to `rect` and placing the receiver at `origin`.
    pub fn padded(&self, origin: Cell, rect: Rect) -> Self {
        let mut result = Self::new(rect.width() as usize, rect.height() as usize);
        result.draw_over(self, origin);
        result
    }

    /// Copies `source` into this image at `origin`, clipped to the receiver.
    pub fn draw_over(&mut self, source: &Rgba8Image, origin: Cell) {
        let (ox, oy) = (origin[0] as i64, origin[1] as i64);
        for y in 0..source.height as i64 {
            let dy = oy + y;
            if dy < 0 || dy >= self.height as i64 {
                continue;
            }
            for x in 0..source.width as i64 {
                let dx = ox + x;
                if dx < 0 || dx >= self.width as i64 {
                    continue;
                }
                let pixel = source.get(x as usize, y as usize);
                self.set(dx as usize, dy as usize, pixel);
            }
        }
    }

    /// The half-open bounds of nonzero alpha, in pixels; `None` when fully transparent.
    pub fn alpha_bounds(&self) -> Option<Rect> {
        let mut min_x = usize::MAX;
        let mut min_y = usize::MAX;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut found = false;
        for y in 0..self.height {
            let row = y * self.stride();
            for x in 0..self.width {
                if self.data[row + x * RGBA_PIXEL + 3] != 0 {
                    found = true;
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        if !found {
            return None;
        }
        Some(Rect::new(
            min_x as f64,
            min_y as f64,
            (max_x - min_x + 1) as f64,
            (max_y - min_y + 1) as f64,
        ))
    }
}

/// 8-bit gray, one byte a pixel: layer masks, selections, and the wand's output.
#[derive(Clone, PartialEq, Eq)]
pub struct Gray8Image {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl std::fmt::Debug for Gray8Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gray8Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl Gray8Image {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height],
        }
    }

    /// A uniform raster — a 1×1 mask is valid and avoids allocating full-resolution pixels.
    pub fn uniform(width: usize, height: usize, value: u8) -> Self {
        Self {
            width,
            height,
            data: vec![value; width * height],
        }
    }

    pub fn from_data(width: usize, height: usize, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width * height, "gray buffer size mismatch");
        Self { width, height, data }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.width
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn pixel_count(&self) -> usize {
        self.width * self.height
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f64, self.height as f64)
    }

    pub fn get(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.width + x]
    }

    pub fn set(&mut self, x: usize, y: usize, value: u8) {
        self.data[y * self.width + x] = value;
    }

    /// A sub-image, `None` for a rectangle that is outside or empty.
    pub fn cropped(&self, rect: Rect) -> Option<Self> {
        let integral = rect.integral();
        if integral.is_empty() {
            return None;
        }
        let x = integral.min_x() as i64;
        let y = integral.min_y() as i64;
        let width = integral.width() as usize;
        let height = integral.height() as usize;
        if x < 0 || y < 0 || width == 0 || height == 0 || x as usize + width > self.width || y as usize + height > self.height {
            return None;
        }
        let mut result = Self::new(width, height);
        for row in 0..height {
            let source = (y as usize + row) * self.width + x as usize;
            result.data[row * width..row * width + width].copy_from_slice(&self.data[source..source + width]);
        }
        Some(result)
    }
}

/// An immutable raster shared by the document, history and the renderer.
pub type SharedImage = Arc<Rgba8Image>;

/// A gray raster shared the same way: masks and selections.
pub type SharedGray = Arc<Gray8Image>;

/// A premultiplied color as components 0…1.
pub fn unpack(pixel: [u8; 4]) -> [f64; 4] {
    [
        pixel[0] as f64 / 255.0,
        pixel[1] as f64 / 255.0,
        pixel[2] as f64 / 255.0,
        pixel[3] as f64 / 255.0,
    ]
}

pub fn pack(components: [f64; 4]) -> [u8; 4] {
    [
        (components[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (components[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (components[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (components[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_bounds_is_half_open() {
        let mut image = Rgba8Image::new(4, 4);
        image.set(1, 2, [10, 20, 30, 255]);
        assert_eq!(image.alpha_bounds(), Some(Rect::new(1.0, 2.0, 1.0, 1.0)));
        assert_eq!(Rgba8Image::new(4, 4).alpha_bounds(), None);
    }

    #[test]
    fn crop_rejects_outside_rects() {
        let image = Rgba8Image::new(4, 4);
        assert!(image.cropped(Rect::new(2.0, 2.0, 2.0, 2.0)).is_some());
        assert!(image.cropped(Rect::new(3.0, 3.0, 2.0, 2.0)).is_none());
        assert!(image.cropped(Rect::new(0.0, 0.0, 0.0, 2.0)).is_none());
    }
}
