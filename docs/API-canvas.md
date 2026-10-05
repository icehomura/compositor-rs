# Frozen API: `compositor_pixels::canvas` and `compositor_pixels::raster`

The `CGContext`/`BrushRaster` replacement. Porting slices code against these signatures; do not change them
without updating every caller. Implementations live in `crates/compositor-pixels/src/canvas.rs` and
`raster.rs`.

## `canvas`

```rust
pub enum InterpolationQuality { None, Low, High }          // CGInterpolationQuality's three used values

pub struct GradientPaint {
    pub kind: GradientKind,          // Linear { from: Point, to: Point } | Radial { center: Point, radius: f64 }
    pub stops: Vec<(f64, [f64; 4])>, // location, straight non-premultiplied sRGB + alpha
    pub extend: GradientExtend,      // None (transparent outside) | Pad
}

pub struct Canvas { /* private */ }

impl Canvas {
    /// A blank target. RGBA targets start fully transparent; gray (mask) targets start black,
    /// which the callers overwrite explicitly when they need white.
    pub fn new_rgba(width: usize, height: usize) -> Self;
    pub fn new_gray(width: usize, height: usize) -> Self;
    /// Wraps an existing image; drawing composites into it.
    pub fn from_rgba(image: Rgba8Image) -> Self;
    pub fn from_gray(image: Gray8Image) -> Self;

    pub fn is_mask(&self) -> bool;
    pub fn width(&self) -> usize;
    pub fn height(&self) -> usize;
    pub fn pixel_size(&self) -> Size;

    /// Graphics state, exactly like `CGContext.saveGState`/`restoreGState`.
    pub fn save(&mut self);
    pub fn restore(&mut self);

    /// Concatenates onto the CTM (`CGContext.translateBy`/`rotate(by:)`/`scaleBy`/`concatenate`).
    pub fn translate(&mut self, dx: f64, dy: f64);
    pub fn scale(&mut self, sx: f64, sy: f64);
    /// Counter-clockwise in the math sense; document space is y-down so callers pass the angle the
    /// Swift code passed to `CGContext.rotate(by:)`.
    pub fn rotate(&mut self, radians: f64);
    pub fn concatenate(&mut self, transform: AffineTransform);
    pub fn ctm(&self) -> AffineTransform;
    /// Device pixels per user-space unit along x (`deviceScale(of:)`).
    pub fn user_space_to_device(&self) -> AffineTransform;
    pub fn device_scale(&self) -> f64;

    /// Clips. `clip_path` intersects with the existing clip; paths are in current user space.
    pub fn clip_path(&mut self, path: &Path, rule: FillRule);
    pub fn clip_rect(&mut self, rect: Rect);
    /// `CGContext.clip(to:mask:)`: `image` is drawn into `rect` in user space and its coverage
    /// multiplies the clip. Values are the image's own bytes, 0…255 → 0…1.
    pub fn clip_to_image(&mut self, image: &Gray8Image, rect: Rect);
    /// `CGContext.clip(to: CGPath)` convenience used by the overlays.
    pub fn clip_to_zero(&mut self) -> ();

    pub fn set_fill_color(&mut self, color: PaletteColor);
    pub fn set_fill_gray(&mut self, value: f64);
    pub fn set_alpha(&mut self, alpha: f64);
    /// `CGContext.boundingBoxOfClipPath`: the device-space bounding box of the current clip.
    pub fn clip_bounds(&self) -> Rect;
    pub fn set_blend_mode(&mut self, mode: LayerBlendMode);
    pub fn set_should_antialias(&mut self, value: bool);
    pub fn set_interpolation_quality(&mut self, quality: InterpolationQuality);

    pub fn fill_path(&mut self, path: &Path, rule: FillRule);
    pub fn fill_rect(&mut self, rect: Rect);
    pub fn fill_rects(&mut self, rects: &[Rect]);
    pub fn fill_gradient(&mut self, rect: Rect, gradient: &GradientPaint);

    /// Draws an image into `rect` in user space, scaled, with the current interpolation quality and
    /// alpha and the destination rect clipped. Sampling ignores the transform's rotation for images
    /// (callers `concatenate` first).
    pub fn draw_image(&mut self, image: &Rgba8Image, rect: Rect);
    /// The greedy/AA coverage path: a gray image drawn as an alpha mask (used for text and for
    /// coverage-to-paint fills).
    pub fn draw_gray(&mut self, image: &Gray8Image, rect: Rect);
    /// Renders `image`'s pixels as already-multiplied coverage, so a mask stays grayscale without
    /// going through a color space (`LayerRenderer.drawCoverage`).
    pub fn draw_coverage(&mut self, image: &Gray8Image, rect: Rect);

    /// Reads the result back. Panics if the target kind does not match — callers know which they made.
    pub fn into_rgba(self) -> Rgba8Image;
    pub fn into_gray(self) -> Gray8Image;
    pub fn rgba(&self) -> &Rgba8Image;
    pub fn gray(&self) -> &Gray8Image;

    /// `CGContext.makeImage()`: a snapshot copy of the current target.
    pub fn snapshot(&self) -> Rgba8Image;
    pub fn snapshot_gray(&self) -> Gray8Image;
}
```

Antialiasing must match Core Graphics' default analytic coverage closely enough that mask edges and
selection coverage are stable; a scanline fill with 1/16-pixel row sampling or equivalent is acceptable —
document the choice. Every fill/draw respects the current clip, alpha, blend mode and CTM.

## `raster` (`enum BrushRaster`)

```rust
pub struct Raster;

impl Raster {
    /// The raster layouts a paint target can have.
    pub fn context_is_mask(width: usize, height: usize, mask: bool) -> bool;   // helper, if needed

    /// The affine map placing a `width`×`height` image with `transform` into document coordinates
    /// (`BrushRaster.pixelToDocument`). The uniform grid alignment follows the transform's origin
    /// components, exactly as the Swift does.
    pub fn pixel_to_document(transform: &LayerTransform, width: f64, height: f64) -> AffineTransform;

    /// Draws `image` into `rect` (document space) of a mask or RGBA canvas.
    pub fn draw(image: &Rgba8Image, rect: Rect, mask: bool, canvas: &mut Canvas);

    /// Fills `rect` with `coverage` (document alpha) times `alpha`.
    pub fn fill(rect: Rect, coverage: f64, alpha: f64, canvas: &mut Canvas);

    /// The softness curve of a brush tip: `falloff(_:)`.
    pub fn falloff(coverage: f64) -> f64;

    /// Runs `body` for each band of `count` row-bands, so a painting caller can commit in slices.
    pub fn in_bands<R>(count: usize, body: impl FnMut(usize, usize) -> R);
}
```

`BrushRaster.pixelToDocument` is the map `LayerTransform.unitToDocument` also uses
(`pixel_to_document(t, 1.0, 1.0)`), so it must be a single implementation.
